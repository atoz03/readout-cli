//! Dashboard state.
//!
//! The app owns the raw event stream once, and every view is a rollup of it
//! under the current filter. Changing a filter recomputes the summary — which
//! is cheap next to the scan — rather than mutating per-view state, so the
//! pages can never disagree with each other.

use crate::agg::{Filter, SHARED_DEVICE_ID, Summary, summarize};
use crate::devices::DeviceRecord;
use crate::model::{Source, UsageEvent};
use crate::pricing::Pricing;
use crate::replay::{ReplayRequest, SessionReplay};
use crate::scan::{Progress, ScanStats};
use crate::settings::Settings;
use crate::tui::anim::{Eased, Pulse};
use crate::tui::hit::Registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Overview,
    Daily,
    Models,
    Projects,
    Sessions,
    Replay,
    Devices,
    Pricing,
    Settings,
}

impl Page {
    /// Sidebar order, grouped under headings.
    pub const GROUPS: [(&'static str, &'static [Page]); 3] = [
        ("Overview", &[Page::Overview, Page::Daily]),
        ("Breakdown", &[Page::Models, Page::Projects, Page::Sessions, Page::Devices]),
        ("Config", &[Page::Pricing, Page::Settings]),
    ];

    pub const ORDER: [Page; 8] = [
        Page::Overview,
        Page::Daily,
        Page::Models,
        Page::Projects,
        Page::Sessions,
        Page::Devices,
        Page::Pricing,
        Page::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Overview => "Readout",
            Page::Daily => "Daily",
            Page::Models => "Models",
            Page::Projects => "Projects",
            Page::Sessions => "Sessions",
            Page::Replay => "Session Replay",
            Page::Devices => "Devices",
            Page::Pricing => "Pricing",
            Page::Settings => "Settings",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Overview => "◈",
            Page::Daily => "▤",
            Page::Models => "◱",
            Page::Projects => "▣",
            Page::Sessions => "◷",
            Page::Replay => "▷",
            Page::Devices => "◫",
            Page::Pricing => "$",
            Page::Settings => "⚙",
        }
    }

    pub fn index(self) -> usize {
        // Replay 是 Sessions 的上下文子页，不单独占侧栏位置。
        let page = if self == Page::Replay { Page::Sessions } else { self };
        Page::ORDER.iter().position(|p| *p == page).unwrap_or(0)
    }
}

/// Time window chips in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Today,
    D7,
    D30,
    D90,
    All,
}

impl Range {
    pub const ORDER: [Range; 5] = [Range::Today, Range::D7, Range::D30, Range::D90, Range::All];

    pub fn label(self) -> &'static str {
        match self {
            Range::Today => "Today",
            Range::D7 => "7d",
            Range::D30 => "30d",
            Range::D90 => "90d",
            Range::All => "All",
        }
    }

    /// The label for a header with no room for the long one. Only `Today`
    /// differs; the rest are already as short as they get.
    pub fn label_short(self) -> &'static str {
        match self {
            Range::Today => "1d",
            other => other.label(),
        }
    }

    pub fn days(self) -> Option<i64> {
        match self {
            Range::Today => Some(1),
            Range::D7 => Some(7),
            Range::D30 => Some(30),
            Range::D90 => Some(90),
            Range::All => None,
        }
    }

    /// The chip a `--days N` on the command line lands on.
    ///
    /// Every window has to be one of the five, because the chips are the only
    /// thing on screen saying which one is active. Rounding up to the nearest
    /// chip that still contains the requested window is the honest direction:
    /// it shows the user at least what they asked for, never less.
    pub fn for_days(days: Option<i64>) -> Range {
        match days {
            None => Range::All,
            Some(d) if d <= 1 => Range::Today,
            Some(d) if d <= 7 => Range::D7,
            Some(d) if d <= 30 => Range::D30,
            Some(d) if d <= 90 => Range::D90,
            _ => Range::All,
        }
    }

    /// How many days a trend chart should span for this window. "All" is
    /// capped so a two-year history does not compress into invisible slivers.
    pub fn chart_days(self, first_ts: i64) -> usize {
        match self.days() {
            Some(d) => d as usize,
            None => {
                if first_ts == 0 {
                    30
                } else {
                    let days = (chrono::Local::now().timestamp() - first_ts) / 86_400 + 1;
                    days.clamp(30, 365) as usize
                }
            }
        }
    }
}

/// A drill-down applied on top of the range.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Drill {
    #[default]
    None,
    Model(String),
    Project(String),
    Device(String),
}

impl Drill {
    pub fn label(&self) -> Option<String> {
        match self {
            Drill::None => None,
            Drill::Model(m) => Some(format!("model: {m}")),
            Drill::Project(p) => Some(format!("project: {p}")),
            Drill::Device(device) => Some(format!("device: {device}")),
        }
    }
}

/// Progress of the background scan.
#[derive(Debug, Clone)]
pub enum Loading {
    Scanning(Option<Progress>),
    Done,
    Failed(String),
}

/// Why a scan is about to run.
///
/// A manual rescan says so on screen, because the user asked and deserves an
/// acknowledgement. A watch rescan must not: flashing a loading panel over
/// the dashboard every few seconds would make a tool for reading numbers
/// impossible to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rescan {
    Manual,
    Watch,
}

/// 设备后台任务。SSH config 只负责发现；首次连接成功后才持久化启用状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceRequest {
    SyncAll,
    SyncHost(String),
    ConnectHost(String),
    UpdateHost(String),
}

/// Replay 页面自己的短生命周期状态；正文不会进入 usage cache。
pub struct ReplayUi {
    pub request: Option<ReplayRequest>,
    pub data: Option<SessionReplay>,
    pub loading: bool,
    pub error: Option<String>,
    pub playing: bool,
    pub speed: u8,
    pub position_ms: f64,
    pub last_tick: std::time::Instant,
    pub return_session_index: usize,
}

impl Default for ReplayUi {
    fn default() -> Self {
        ReplayUi {
            request: None,
            data: None,
            loading: false,
            error: None,
            playing: false,
            speed: 1,
            position_ms: 0.0,
            last_tick: std::time::Instant::now(),
            return_session_index: 0,
        }
    }
}

/// How often watch mode looks for new transcripts.
///
/// A warm scan is a few hundred milliseconds at worst and reads only what was
/// appended, so this could be shorter. It is not, because the number being
/// watched moves on the timescale of a model finishing a response, and a
/// dashboard that redraws faster than the thing it measures is just motion.
pub const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

pub struct App {
    pub page: Page,
    pub range: Range,
    pub drill: Drill,
    pub sources: Vec<Source>,
    /// Sources the user has switched off in the UI. Kept separate from
    /// `sources` so a CLI `--source` restriction is never widened by a click.
    pub disabled: Vec<Source>,

    pub events: Vec<UsageEvent>,
    pub devices: Vec<DeviceRecord>,
    /// Devices 页始终基于全设备事件，避免关闭默认聚合后远端状态一起消失。
    pub device_summary: Summary,
    pub settings: Settings,
    pub settings_path: String,
    settings_file: Option<std::path::PathBuf>,
    /// Settings 内的本机名称编辑器。名称是用户可改的显示信息，稳定 device ID 不变。
    pub device_name_editor: bool,
    pub device_name_input: String,
    /// SSH config 中发现但尚未添加的 Host。默认 Devices 列表不展示它们；只有用户
    /// 进入 Add SSH device 后才作为可搜索候选出现。
    pub available_ssh_hosts: Vec<String>,
    pub device_picker: bool,
    pub device_query: String,
    /// SSH config 按需读取一次，不进入主页面首帧的热路径。
    pub ssh_hosts_loaded: bool,
    /// 打开 Devices 页时是否真的去读 `~/.ssh/config`。测试关掉它，断言才不会
    /// 取决于跑测试的机器上恰好配了哪些 Host——但两边走的是同一条代码路径。
    pub discover_ssh_hosts: bool,
    /// 已经按过一次 `u`、正在等待确认的远端 host。远端升级会下载并执行安装器，
    /// 一次误触不该就把另一台机器上的二进制换掉。
    pub update_armed: Option<String>,
    pub pricing: Pricing,
    pub summary: Summary,
    pub stats: ScanStats,
    pub loading: Loading,

    /// Selected row on the focused list, per page.
    pub selected: usize,
    pub scroll: usize,
    /// Rows the last frame had space for, written during render so keyboard
    /// navigation can scroll the window without knowing the layout.
    pub list_rows: std::cell::Cell<usize>,
    pub hover: Option<u64>,

    pub hits: Registry,
    pub pulse: Pulse,
    /// KPI counters, eased so they count up on load and on filter changes.
    pub kpi: [Eased; 4],
    /// Bar growth factor, 0..1, restarted whenever the data behind a chart
    /// changes so bars grow rather than snap.
    pub grow: Eased,
    pub should_quit: bool,
    pub needs_redraw: bool,
    /// Set when a rescan has been asked for but not yet started.
    pub rescan_requested: Option<Rescan>,
    /// A scan thread is running. Watch scans deliberately leave `loading` on
    /// `Done`, so this is the only thing that knows one is in flight — and
    /// without it the loop would spawn a fresh thread every tick.
    pub scan_pending: bool,
    /// When the last scan finished, successfully or not. Watch mode counts
    /// from here, so a failing scan retries on the next interval rather than
    /// wedging the dashboard on a stale number.
    pub last_scan: Option<std::time::Instant>,
    /// Re-scan on a timer, so the numbers keep up with the work.
    pub watch: bool,
    pub device_requested: Option<DeviceRequest>,
    pub device_pending: bool,
    /// The local date the current summary was built on. Every window is
    /// anchored to "now", so a dashboard left watching past midnight is
    /// showing yesterday's answer until it repaints.
    pub summary_date: chrono::NaiveDate,
    pub status: Option<String>,

    pub replay: ReplayUi,
    /// event loop 取走请求后在后台读取 transcript。
    pub replay_requested: Option<ReplayRequest>,
}

impl App {
    #[cfg(test)]
    pub fn new(sources: Vec<Source>, base: Filter, pricing: Pricing) -> Self {
        let mut app = Self::with_settings(sources, base, pricing, Settings::default());
        app.discover_ssh_hosts = false;
        // 单元测试不应因为一次设置交互写入开发者的真实配置；需要验证持久化的测试
        // 会显式提供自己的临时路径。
        app.settings_file = None;
        app
    }

    pub fn with_settings(
        sources: Vec<Source>,
        base: Filter,
        pricing: Pricing,
        settings: Settings,
    ) -> Self {
        // `--days 7` used to open on the 30-day chip: the window was right but
        // the header said otherwise. Land on the chip that matches.
        let today = chrono::Local::now().date_naive();
        let range = Range::for_days(base.since.map(|s| (today - s).num_days() + 1));
        let settings_file = crate::paths::settings_file().ok();
        let settings_path = settings_file
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unavailable".into());
        App {
            page: Page::Overview,
            range,
            drill: match (&base.model, &base.project) {
                (Some(m), _) => Drill::Model(m.clone()),
                (_, Some(p)) => Drill::Project(p.clone()),
                _ => Drill::None,
            },
            sources,
            disabled: Vec::new(),
            events: Vec::new(),
            devices: vec![DeviceRecord {
                id: settings.device.id.clone(),
                name: settings.device.name.clone(),
                host: None,
                exporter_version: Some(env!("CARGO_PKG_VERSION").into()),
                generated_at: 0,
                is_local: true,
                available: true,
                enabled: true,
                discovered: true,
                problem: None,
            }],
            device_summary: Summary::default(),
            settings,
            settings_path,
            settings_file,
            device_name_editor: false,
            device_name_input: String::new(),
            available_ssh_hosts: Vec::new(),
            device_picker: false,
            device_query: String::new(),
            ssh_hosts_loaded: false,
            discover_ssh_hosts: true,
            update_armed: None,
            pricing,
            summary: Summary::default(),
            stats: ScanStats::default(),
            loading: Loading::Scanning(None),
            selected: 0,
            scroll: 0,
            list_rows: std::cell::Cell::new(0),
            hover: None,
            hits: Registry::default(),
            pulse: Pulse::default(),
            kpi: [Eased::from_zero(0.0); 4],
            grow: Eased::from_zero(1.0),
            should_quit: false,
            needs_redraw: true,
            rescan_requested: None,
            scan_pending: false,
            last_scan: None,
            watch: false,
            device_requested: None,
            device_pending: false,
            summary_date: today,
            status: None,
            replay: ReplayUi::default(),
            replay_requested: None,
        }
    }

    /// Sources currently contributing, honouring both the CLI restriction and
    /// the in-UI toggles.
    pub fn active_sources(&self) -> Vec<Source> {
        let active: Vec<Source> =
            self.sources.iter().copied().filter(|s| !self.disabled.contains(s)).collect();
        // Turning off the last tool would blank the dashboard with no way to
        // tell an empty filter from empty data; keep at least one on.
        if active.is_empty() { self.sources.clone() } else { active }
    }

    pub fn filter(&self) -> Filter {
        let today = chrono::Local::now().date_naive();
        let mut f = Filter { sources: self.active_sources(), ..Default::default() };
        f.since = self.range.days().map(|d| today - chrono::Duration::days(d - 1));
        f.until = Some(today);
        match &self.drill {
            Drill::None => {}
            Drill::Model(m) => f.model = Some(m.clone()),
            Drill::Project(p) => f.project = Some(p.clone()),
            Drill::Device(device) => f.device = Some(device.clone()),
        }
        if f.device.is_none() && !self.settings.aggregate_devices {
            f.device = Some(self.settings.device.id.clone());
        }
        f
    }

    /// Recompute every rollup and restart the entrance animations.
    pub fn recompute(&mut self, animate: bool) {
        self.summary = summarize(&self.events, &self.filter(), &self.pricing);
        let mut device_filter = self.filter();
        device_filter.device = None;
        self.device_summary = summarize(&self.events, &device_filter, &self.pricing);
        self.summary_date = chrono::Local::now().date_naive();
        let targets = [
            self.summary.total.tokens.total() as f64,
            self.summary.total.priced.cost,
            self.summary.total.events as f64,
            self.summary.total.session_count() as f64,
        ];
        for (slot, target) in self.kpi.iter_mut().zip(targets) {
            if animate {
                *slot = Eased::from_zero(target);
            } else {
                slot.snap_to(target);
            }
        }
        if animate {
            self.grow = Eased::from_zero(1.0).with_rate(crate::tui::anim::RATE_FAST);
        } else {
            self.grow.snap_to(1.0);
        }
        self.clamp_selection();
        self.needs_redraw = true;
    }

    /// What the dashboard is currently showing, reduced to the things a new
    /// scan could change. Used to tell a rescan that found work from one that
    /// found nothing.
    fn signature(&self) -> (usize, u64, u64) {
        (self.stats.events, self.summary.total.tokens.total(), self.summary.total.events)
    }

    /// Fold a finished scan into the dashboard.
    ///
    /// A watch scan neither replays the entrance animation nor snaps: the
    /// KPIs keep the value they are showing and ease from there to the new
    /// one, so a number that went up looks like a number that went up. When
    /// nothing moved it issues no redraw at all, so a watched dashboard on an
    /// idle machine sends nothing to the terminal — which is what makes
    /// leaving one open over ssh reasonable.
    pub fn apply_scan(&mut self, events: Vec<UsageEvent>, stats: ScanStats, kind: Rescan) {
        let before: [f64; 4] = std::array::from_fn(|i| self.kpi[i].value());
        let was = self.signature();
        let day = self.summary_date;
        // A failure is on screen until something paints over it. Recovering
        // quietly would leave "scan failed" under a dashboard that is fine.
        let recovered = matches!(self.loading, Loading::Failed(_));

        self.events = events;
        self.stats = stats;
        self.loading = Loading::Done;
        self.last_scan = Some(std::time::Instant::now());

        match kind {
            Rescan::Manual => {
                self.status = None;
                self.recompute(true);
            }
            Rescan::Watch => {
                self.recompute(false);
                let moved = self.signature() != was;
                if moved {
                    for (slot, from) in self.kpi.iter_mut().zip(before) {
                        slot.ease_from(from);
                    }
                }
                // Midnight moves every window without moving a single token,
                // so the totals can match while the frame on screen is a day
                // out of date — "Today $189" on a day that has spent nothing.
                let rolled = self.summary_date != day;
                self.needs_redraw = moved || recovered || rolled;
            }
        }
    }

    /// Record a scan that ended without a result, so watch mode can try again.
    pub fn scan_failed(&mut self, err: String) {
        self.loading = Loading::Failed(err);
        self.last_scan = Some(std::time::Instant::now());
        self.needs_redraw = true;
    }

    /// Whether watch mode should start a scan now.
    ///
    /// Takes the clock as an argument so the schedule can be tested without
    /// waiting five seconds for it.
    pub fn watch_due(&self, now: std::time::Instant) -> bool {
        if !self.watch || self.scan_pending || self.rescan_requested.is_some() {
            return false;
        }
        match self.last_scan {
            None => false,
            Some(t) => now.duration_since(t) >= WATCH_INTERVAL,
        }
    }

    pub fn toggle_watch(&mut self) {
        self.watch = !self.watch;
        self.status = Some(if self.watch {
            "watching — rescanning every 5s".into()
        } else {
            "watch off".into()
        });
        self.needs_redraw = true;
    }

    /// Ask for a rescan, unless one is already under way.
    pub fn request_rescan(&mut self, kind: Rescan) {
        if self.scan_pending || self.device_pending || self.rescan_requested.is_some() {
            return;
        }
        self.rescan_requested = Some(kind);
    }

    /// 合并 SSH config 中发现的别名与已启用设备，不发起任何网络连接。
    pub fn refresh_ssh_hosts(&mut self) {
        match crate::devices::discover_ssh_hosts() {
            Ok(hosts) => self.apply_discovered_ssh_hosts(hosts),
            Err(error) => {
                self.status = Some(format!("could not read SSH config: {error}"));
            }
        }
        self.needs_redraw = true;
    }

    fn apply_discovered_ssh_hosts(&mut self, hosts: Vec<String>) {
        for device in self.devices.iter_mut().filter(|device| !device.is_local) {
            let host = device.host.as_deref();
            device.discovered = host.is_some_and(|host| hosts.iter().any(|item| item == host));
            device.enabled = host.is_some_and(|host| self.settings.ssh_host_enabled(host));
        }
        self.available_ssh_hosts =
            hosts.into_iter().filter(|host| !self.settings.ssh_host_enabled(host)).collect();
        self.devices.sort_by(|a, b| {
            b.is_local.cmp(&a.is_local).then_with(|| {
                a.host.as_deref().unwrap_or(&a.name).cmp(b.host.as_deref().unwrap_or(&b.name))
            })
        });
        self.ssh_hosts_loaded = true;
        self.clamp_selection();
    }

    /// Rows on the currently focused list, used to bound selection.
    pub fn row_count(&self) -> usize {
        match self.page {
            Page::Overview => self.summary.by_model.len(),
            Page::Daily => self.summary.daily.len(),
            Page::Models => self.summary.by_model.len(),
            Page::Projects => self.summary.by_project.len(),
            Page::Sessions => self.summary.by_session.len(),
            Page::Replay => self.replay.data.as_ref().map_or(0, |replay| replay.events.len()),
            Page::Devices => self.device_row_count(),
            Page::Pricing => self.pricing.known_models().len(),
            Page::Settings => usize::from(!self.device_name_editor) * 5,
        }
    }

    pub fn clamp_selection(&mut self) {
        let n = self.row_count();
        if n == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn set_page(&mut self, page: Page) {
        if self.page == Page::Devices && page != Page::Devices {
            self.cancel_device_picker();
        }
        if self.page == Page::Settings && page != Page::Settings {
            self.cancel_device_name_editor();
        }
        if self.page != page {
            self.page = page;
            self.selected = 0;
            self.scroll = 0;
            self.disarm_update();
            self.grow = Eased::from_zero(1.0).with_rate(crate::tui::anim::RATE_FAST);
            self.needs_redraw = true;
        }
        if page == Page::Devices && self.discover_ssh_hosts && !self.ssh_hosts_loaded {
            self.refresh_ssh_hosts();
        }
    }

    pub fn next_page(&mut self, delta: isize) {
        let n = Page::ORDER.len() as isize;
        let i = (self.page.index() as isize + delta).rem_euclid(n) as usize;
        self.set_page(Page::ORDER[i]);
    }

    pub fn set_range(&mut self, range: Range) {
        if self.range != range {
            self.range = range;
            self.recompute(true);
        }
    }

    pub fn toggle_source(&mut self, s: Source) {
        if !self.sources.contains(&s) {
            self.status = Some(format!("{} was excluded on the command line", s.label()));
            return;
        }
        if let Some(i) = self.disabled.iter().position(|d| *d == s) {
            self.disabled.remove(i);
        } else if self.active_sources().len() > 1 {
            self.disabled.push(s);
        } else {
            self.status = Some("at least one tool must stay on".into());
            return;
        }
        self.recompute(true);
    }

    pub fn set_drill(&mut self, drill: Drill) {
        if self.drill != drill {
            self.drill = drill;
            self.recompute(true);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let i = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        if i != self.selected {
            self.selected = i;
            self.disarm_update();
            if self.page == Page::Replay
                && let Some(event) =
                    self.replay.data.as_ref().and_then(|replay| replay.events.get(i))
            {
                self.replay.position_ms = event.offset_ms as f64;
                self.replay.playing = false;
            }
            self.needs_redraw = true;
        }
    }

    /// Scroll the visible window to keep the selection inside it.
    pub fn ensure_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let n = self.row_count();
        let max = n.saturating_sub(1);
        let s = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        if s != self.scroll {
            self.scroll = s;
            self.needs_redraw = true;
        }
    }

    /// 激活当前行：模型切换筛选，项目进入 Sessions，session 进入 Replay。
    pub fn activate_selected(&mut self) {
        match self.page {
            Page::Models | Page::Overview => {
                let next = self
                    .summary
                    .by_model
                    .get(self.selected)
                    .map(|bucket| Drill::Model(bucket.label.clone()));
                if let Some(next) = next {
                    self.set_drill(if self.drill == next { Drill::None } else { next });
                }
            }
            Page::Projects => self.open_project(self.selected),
            Page::Sessions => self.open_session(self.selected),
            Page::Replay => self.seek_replay(self.selected),
            Page::Devices => {
                if self.device_picker {
                    let host = self.selected_ssh_host();
                    if let Some(host) = host {
                        self.request_device(DeviceRequest::ConnectHost(host));
                    }
                    return;
                }
                let Some(record) = self.devices.get(self.selected).cloned() else {
                    let shared_index = self.devices.len();
                    let add_index = shared_index + usize::from(self.has_shared_device());
                    if self.selected == shared_index && self.has_shared_device() {
                        self.set_drill(Drill::Device(SHARED_DEVICE_ID.into()));
                        self.status = Some("showing copied history".into());
                        self.set_page(Page::Overview);
                    } else if self.selected == add_index {
                        self.begin_device_picker();
                    }
                    return;
                };
                let Some(host) = record.host.clone() else {
                    let label = record.name;
                    self.set_drill(Drill::Device(record.id));
                    self.status = Some(format!("showing device {label}"));
                    self.set_page(Page::Overview);
                    return;
                };
                if !record.enabled {
                    self.request_device(DeviceRequest::ConnectHost(host));
                // 快照坏掉的设备 available 为 false，所以 Enter 落在这里重新同步 ——
                // 重建那份快照正是唯一的修法。
                } else if !record.available {
                    self.request_device(DeviceRequest::SyncHost(host));
                } else {
                    let label = record.name;
                    self.set_drill(Drill::Device(record.id));
                    self.status = Some(format!("showing device {label}"));
                    self.set_page(Page::Overview);
                }
            }
            Page::Settings => self.activate_setting(self.selected),
            _ => {}
        }
    }

    /// 项目是 session 的父级：打开项目后直接落到过滤后的 Sessions 页。
    pub fn open_project(&mut self, index: usize) {
        let Some(project) = self.summary.by_project.get(index).map(|b| b.label.clone()) else {
            return;
        };
        self.set_drill(Drill::Project(project));
        self.set_page(Page::Sessions);
    }

    /// 选中 session 后切到上下文 replay 页，并把读取交给后台线程。
    pub fn open_session(&mut self, index: usize) {
        let Some(bucket) = self.summary.by_session.get(index) else { return };
        let session = bucket.label.clone();
        let bucket_source = bucket.sources.iter().next().copied();
        let project = bucket.top_project().unwrap_or("unknown").to_string();
        let model = bucket.top_model().unwrap_or("unknown").to_string();
        let filter = self.filter();
        let matching: Vec<_> = self
            .events
            .iter()
            .filter(|event| {
                event.session == session
                    && bucket_source.is_none_or(|source| event.source == source)
                    && filter.admits(event)
            })
            .collect();
        let source = matching.iter().max_by_key(|event| event.ts).map(|event| event.source);
        let Some(source) = source else {
            self.status = Some("could not locate the session source".into());
            return;
        };
        let request = ReplayRequest { source, session, project, model };
        let local = matching.iter().any(|event| {
            event.observed_on.is_empty()
                || event.observed_on.iter().any(|id| id == &self.settings.device.id)
        });
        if !local {
            let mut names: Vec<_> = matching
                .iter()
                .flat_map(|event| event.observed_on.iter())
                .map(|id| self.device_name(id).to_string())
                .collect();
            names.sort();
            names.dedup();
            self.replay = ReplayUi {
                request: Some(request),
                error: Some(format!(
                    "usage-only remote session; Replay remains on {}",
                    names.join(", ")
                )),
                return_session_index: index,
                ..ReplayUi::default()
            };
            self.page = Page::Replay;
            self.selected = 0;
            self.scroll = 0;
            self.needs_redraw = true;
            return;
        }
        self.replay = ReplayUi {
            request: Some(request.clone()),
            loading: true,
            return_session_index: index,
            ..ReplayUi::default()
        };
        self.replay_requested = Some(request);
        self.page = Page::Replay;
        self.selected = 0;
        self.scroll = 0;
        self.grow = Eased::from_zero(1.0).with_rate(crate::tui::anim::RATE_FAST);
        self.needs_redraw = true;
    }

    pub fn apply_replay(&mut self, replay: SessionReplay) {
        self.replay.loading = false;
        self.replay.error = None;
        self.replay.data = Some(replay);
        self.replay.position_ms = 0.0;
        self.replay.playing = false;
        self.selected = 0;
        self.scroll = 0;
        self.needs_redraw = true;
    }

    pub fn fail_replay(&mut self, error: String) {
        self.replay.loading = false;
        self.replay.error = Some(error);
        self.needs_redraw = true;
    }

    pub fn device_name<'a>(&'a self, id: &'a str) -> &'a str {
        if id == SHARED_DEVICE_ID {
            return "Shared";
        }
        self.devices.iter().find(|device| device.id == id).map_or(id, |device| device.name.as_str())
    }

    pub fn has_shared_device(&self) -> bool {
        self.device_summary.by_device.iter().any(|bucket| bucket.label == SHARED_DEVICE_ID)
    }

    pub fn device_row_count(&self) -> usize {
        if self.device_picker {
            self.device_candidates().len()
        } else {
            self.devices.len() + usize::from(self.has_shared_device()) + 1
        }
    }

    pub fn device_candidates(&self) -> Vec<String> {
        let query = self.device_query.to_ascii_lowercase();
        let mut candidates: Vec<_> = self
            .available_ssh_hosts
            .iter()
            .filter(|host| query.is_empty() || host.to_ascii_lowercase().contains(&query))
            .cloned()
            .collect();
        let manual = self.device_query.trim();
        if candidates.is_empty()
            && crate::settings::validate_ssh_alias(manual).is_ok()
            && !self.settings.ssh_host_enabled(manual)
        {
            candidates.push(manual.to_string());
        }
        candidates
    }

    pub fn begin_device_picker(&mut self) {
        self.device_picker = true;
        self.device_query.clear();
        self.selected = 0;
        self.scroll = 0;
        self.status =
            Some("type to filter SSH hosts · Enter connect · Ctrl+u install · Esc back".into());
        self.needs_redraw = true;
    }

    pub fn cancel_device_picker(&mut self) {
        if self.device_picker {
            self.device_picker = false;
            self.device_query.clear();
            self.selected = 0;
            self.scroll = 0;
            self.disarm_update();
            self.needs_redraw = true;
        }
    }

    pub fn push_device_query(&mut self, ch: char) {
        if !ch.is_control() && self.device_query.len() < 255 {
            self.device_query.push(ch);
            self.selected = 0;
            self.scroll = 0;
            self.disarm_update();
            self.needs_redraw = true;
        }
    }

    pub fn pop_device_query(&mut self) {
        if self.device_query.pop().is_some() {
            self.selected = 0;
            self.scroll = 0;
            self.disarm_update();
            self.needs_redraw = true;
        }
    }

    fn selected_ssh_host(&self) -> Option<String> {
        if self.device_picker {
            self.device_candidates().get(self.selected).cloned()
        } else {
            self.devices.get(self.selected).and_then(|device| device.host.clone())
        }
    }

    pub fn activate_setting(&mut self, index: usize) {
        let before = self.settings.clone();
        match index {
            0 => self.settings.aggregate_devices = !self.settings.aggregate_devices,
            1 => {
                self.begin_device_name_editor();
                return;
            }
            2 => {
                self.set_page(Page::Devices);
                self.status = Some("select Add SSH device… or an existing host".into());
                return;
            }
            3 => {
                self.status =
                    Some("manage aliases with `readout project-alias set|remove|list`".into());
                return;
            }
            4 => {
                self.status = Some(format!("settings file: {}", self.settings_path));
                return;
            }
            _ => return,
        }
        if let Err(error) = self.save_settings() {
            self.settings = before;
            self.status = Some(format!("could not save settings: {error}"));
            return;
        }
        self.status = Some("settings saved".into());
        self.recompute(true);
    }

    pub fn begin_device_name_editor(&mut self) {
        self.device_name_editor = true;
        self.device_name_input.clone_from(&self.settings.device.name);
        self.selected = 0;
        self.scroll = 0;
        self.status = Some("edit the local name · Ctrl+u clear · Enter save · Esc cancel".into());
        self.needs_redraw = true;
    }

    pub fn cancel_device_name_editor(&mut self) {
        if self.device_name_editor {
            self.device_name_editor = false;
            self.device_name_input.clear();
            self.selected = 1;
            self.scroll = 0;
            self.needs_redraw = true;
        }
    }

    pub fn clear_device_name_input(&mut self) {
        self.device_name_input.clear();
        self.needs_redraw = true;
    }

    pub fn push_device_name_input(&mut self, ch: char) {
        let next_len = self.device_name_input.len().saturating_add(ch.len_utf8());
        let text = ch.to_string();
        let safe = crate::fmt::terminal_text(&text) == text;
        if !ch.is_control() && safe && next_len <= 128 {
            self.device_name_input.push(ch);
            self.needs_redraw = true;
        }
    }

    pub fn pop_device_name_input(&mut self) {
        if self.device_name_input.pop().is_some() {
            self.needs_redraw = true;
        }
    }

    pub fn save_device_name_input(&mut self) {
        let name = self.device_name_input.trim().to_string();
        let before = self.settings.clone();
        if let Err(error) = self.settings.set_device_name(name.clone()) {
            self.status = Some(format!("invalid device name: {error}"));
            self.needs_redraw = true;
            return;
        }
        if let Err(error) = self.save_settings() {
            self.settings = before;
            self.status = Some(format!("could not save settings: {error}"));
            self.needs_redraw = true;
            return;
        }
        if let Some(local) = self.devices.iter_mut().find(|device| device.is_local) {
            local.name.clone_from(&name);
        }
        self.device_name_editor = false;
        self.device_name_input.clear();
        self.selected = 1;
        self.scroll = 0;
        self.status = Some(format!("local device renamed to {name}"));
        self.needs_redraw = true;
    }

    fn save_settings(&self) -> anyhow::Result<()> {
        let path = self
            .settings_file
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("settings path is unavailable"))?;
        self.settings.save_to(path)
    }

    pub fn request_sync(&mut self) {
        self.request_device(DeviceRequest::SyncAll);
    }

    fn request_device(&mut self, request: DeviceRequest) {
        if self.scan_pending {
            self.status = Some("wait for the current scan before contacting devices".into());
            self.needs_redraw = true;
            return;
        }
        if self.device_pending || self.device_requested.is_some() {
            return;
        }
        if matches!(request, DeviceRequest::SyncAll) && !self.settings.has_ssh_hosts() {
            self.status = Some("no SSH devices are enabled".into());
            self.needs_redraw = true;
            return;
        }
        self.status = Some(match &request {
            DeviceRequest::SyncAll => "syncing devices…".into(),
            DeviceRequest::SyncHost(host) => format!("syncing {host}…"),
            DeviceRequest::ConnectHost(host) => format!("validating {host}…"),
            DeviceRequest::UpdateHost(host) => format!("updating {host}…"),
        });
        self.device_requested = Some(request);
        self.needs_redraw = true;
    }

    /// 第一次按 `u` 只是把这台设备装上膛，第二次才真的执行。远端升级会下载并运行
    /// 官方安装器，替换掉另一台机器上的二进制——这类动作值得一次明确确认。
    pub fn update_selected_device(&mut self) {
        if !self.device_picker
            && self.devices.get(self.selected).is_some_and(|device| device.is_local)
        {
            self.status =
                Some("the local device uses `readout update` outside the dashboard".into());
            return;
        }
        let Some(host) = self.selected_ssh_host() else {
            self.status = Some("select an SSH device to update".into());
            return;
        };
        if self.update_armed.as_deref() != Some(host.as_str()) {
            self.update_armed = Some(host.clone());
            let key = if self.device_picker { "Ctrl+u" } else { "u" };
            self.status =
                Some(format!("press {key} again to run the installer on {host}, or Esc to cancel"));
            self.needs_redraw = true;
            return;
        }
        self.update_armed = None;
        self.request_device(DeviceRequest::UpdateHost(host));
    }

    /// 选中行一变，上膛就失效：确认必须落在用户看着的那台设备上。
    pub fn disarm_update(&mut self) {
        if self.update_armed.is_some() {
            self.update_armed = None;
            self.needs_redraw = true;
        }
    }

    pub fn disable_selected_device(&mut self) {
        if self.device_picker {
            self.status = Some("press Esc to leave the device picker".into());
            return;
        }
        let Some(host) = self.devices.get(self.selected).and_then(|device| device.host.clone())
        else {
            self.status = Some("select an enabled SSH device to disable".into());
            return;
        };
        if !self.settings.ssh_host_enabled(&host) {
            self.status = Some(format!("{host} is not enabled"));
            return;
        }
        let before = self.settings.clone();
        self.settings.disable_ssh_host(&host);
        if let Err(error) = self.settings.save() {
            self.settings = before;
            self.status = Some(format!("could not save settings: {error}"));
            return;
        }
        if let Some(device) =
            self.devices.iter_mut().find(|device| device.host.as_deref() == Some(&host))
        {
            device.enabled = false;
        }
        self.status = Some(format!("disabled {host}"));
        self.request_rescan(Rescan::Manual);
        self.needs_redraw = true;
    }

    pub fn sync_finished(&mut self, report: &crate::devices::SyncReport) {
        self.device_pending = false;
        self.status = Some(match (report.synced.is_empty(), report.failed.is_empty()) {
            (false, true) => format!("synced {}", report.synced.join(", ")),
            (false, false) => {
                format!("synced {} · {} failed", report.synced.join(", "), report.failed.len())
            }
            (true, false) => format!(
                "{} device(s) failed · {}",
                report.failed.len(),
                report.failed.first().map_or("unknown error", String::as_str)
            ),
            (true, true) => "nothing to sync".into(),
        });
        if !report.synced.is_empty() {
            self.request_rescan(Rescan::Watch);
        } else {
            self.needs_redraw = true;
        }
    }

    pub fn connect_finished(&mut self, host: &str, updated: bool) {
        self.device_pending = false;
        let before = self.settings.clone();
        if let Err(error) =
            self.settings.enable_ssh_host(host.to_string()).and_then(|_| self.settings.save())
        {
            self.settings = before;
            self.status =
                Some(format!("{host} is compatible, but settings could not be saved: {error}"));
            self.needs_redraw = true;
            return;
        }
        self.status = Some(if updated {
            format!("updated and connected {host}")
        } else {
            format!("connected {host}")
        });
        self.device_picker = false;
        self.device_query.clear();
        self.available_ssh_hosts.retain(|item| item != host);
        self.request_rescan(Rescan::Watch);
    }

    pub fn sync_failed(&mut self, error: String) {
        self.device_pending = false;
        self.status = Some(format!("device operation failed: {error}"));
        self.needs_redraw = true;
    }

    pub fn back_to_sessions(&mut self) {
        let index = self.replay.return_session_index;
        self.page = Page::Sessions;
        self.selected = index.min(self.summary.by_session.len().saturating_sub(1));
        self.scroll = self.selected;
        self.replay.playing = false;
        self.replay_requested = None;
        self.needs_redraw = true;
    }

    pub fn toggle_replay(&mut self) {
        let Some(replay) = self.replay.data.as_ref() else { return };
        if replay.events.is_empty() {
            return;
        }
        if self.replay.position_ms >= replay.duration_ms() as f64 {
            self.replay.position_ms = 0.0;
            self.selected = 0;
            self.scroll = 0;
        }
        self.replay.playing = !self.replay.playing;
        self.replay.last_tick = std::time::Instant::now();
        self.needs_redraw = true;
    }

    pub fn set_replay_speed(&mut self, speed: u8) {
        if matches!(speed, 1 | 2 | 4) {
            self.replay.speed = speed;
            self.replay.last_tick = std::time::Instant::now();
            self.needs_redraw = true;
        }
    }

    pub fn seek_replay(&mut self, index: usize) {
        let Some(replay) = self.replay.data.as_ref() else { return };
        let Some(event) = replay.events.get(index) else { return };
        self.selected = index;
        self.replay.position_ms = event.offset_ms as f64;
        self.replay.playing = false;
        self.ensure_visible(self.list_rows.get());
        self.needs_redraw = true;
    }

    fn tick_replay(&mut self) -> bool {
        if self.page != Page::Replay || !self.replay.playing {
            return false;
        }
        let Some(replay) = self.replay.data.as_ref() else { return false };
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.replay.last_tick).as_secs_f64() * 1_000.0;
        self.replay.last_tick = now;
        let duration = replay.duration_ms() as f64;
        self.replay.position_ms =
            (self.replay.position_ms + elapsed * f64::from(self.replay.speed)).min(duration);
        let index = replay
            .events
            .partition_point(|event| event.offset_ms as f64 <= self.replay.position_ms)
            .saturating_sub(1);
        self.selected = index;
        let rows = self.list_rows.get();
        if rows > 0 && self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        if self.replay.position_ms >= duration {
            self.replay.playing = false;
        }
        true
    }

    /// Advance animations; returns true if a redraw is warranted.
    pub fn tick(&mut self) -> bool {
        let mut moving = false;
        for k in self.kpi.iter_mut() {
            moving |= k.tick();
        }
        moving |= self.grow.tick();
        moving |= self.tick_replay();
        // A running scan animates its spinner regardless of eased values.
        if matches!(self.loading, Loading::Scanning(_)) || self.replay.loading {
            moving = true;
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tokens;
    use crate::replay::{ReplayEvent, ReplayKind};

    fn app() -> App {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.events = vec![
            UsageEvent {
                source: Source::Claude,
                ts: chrono::Local::now().timestamp(),
                model: "claude-opus-5".into(),
                session: "s1".into(),
                project: "alpha".into(),
                tokens: Tokens { input: 100, output: 100, ..Default::default() },
                observed_on: Vec::new(),
                dedup_key: None,
                dedup_rank: 0,
            },
            UsageEvent {
                source: Source::Codex,
                ts: chrono::Local::now().timestamp(),
                model: "gpt-5.2".into(),
                session: "s2".into(),
                project: "beta".into(),
                tokens: Tokens { input: 50, output: 50, ..Default::default() },
                observed_on: Vec::new(),
                dedup_key: None,
                dedup_rank: 0,
            },
        ];
        a.recompute(false);
        a
    }

    #[test]
    fn drilling_into_a_model_narrows_every_rollup() {
        let mut a = app();
        assert_eq!(a.summary.total.events, 2);
        a.set_drill(Drill::Model("gpt-5.2".into()));
        assert_eq!(a.summary.total.events, 1);
        assert_eq!(a.summary.by_project[0].label, "beta");
        a.set_drill(Drill::None);
        assert_eq!(a.summary.total.events, 2);
    }

    #[test]
    fn turning_off_the_last_tool_is_refused() {
        let mut a = app();
        a.toggle_source(Source::Claude);
        assert_eq!(a.active_sources(), vec![Source::Codex]);
        a.toggle_source(Source::Codex);
        assert_eq!(a.active_sources(), vec![Source::Codex], "one tool must stay on");
        assert!(a.status.is_some());
    }

    #[test]
    fn a_cli_excluded_tool_cannot_be_switched_on_from_the_ui() {
        let mut a = App::new(vec![Source::Claude], Filter::default(), Pricing::builtin());
        a.toggle_source(Source::Codex);
        assert_eq!(a.active_sources(), vec![Source::Claude]);
    }

    #[test]
    fn selection_stays_inside_the_list_when_it_shrinks() {
        let mut a = app();
        a.set_page(Page::Models);
        a.move_selection(10);
        assert_eq!(a.selected, a.row_count() - 1);
        a.set_drill(Drill::Model("gpt-5.2".into()));
        assert!(a.selected < a.row_count().max(1));
    }

    #[test]
    fn scrolling_follows_the_selection() {
        let mut a = app();
        a.set_page(Page::Models);
        a.selected = 1;
        a.ensure_visible(1);
        assert_eq!(a.scroll, 1);
        a.selected = 0;
        a.ensure_visible(1);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn opening_a_project_shows_only_that_projects_sessions() {
        let mut a = app();
        a.set_page(Page::Projects);
        let project = a.summary.by_project[0].label.clone();
        a.open_project(0);
        assert_eq!(a.page, Page::Sessions);
        assert_eq!(a.drill, Drill::Project(project.clone()));
        assert!(a.summary.by_session.iter().all(|bucket| bucket.top_project() == Some(&project)));
    }

    #[test]
    fn replay_playback_advances_the_selected_event_and_stops_at_the_end() {
        let mut a = app();
        a.page = Page::Replay;
        a.replay.data = Some(SessionReplay {
            events: vec![
                ReplayEvent {
                    ts_ms: 1,
                    offset_ms: 0,
                    kind: ReplayKind::User,
                    title: "user".into(),
                    detail: "start".into(),
                },
                ReplayEvent {
                    ts_ms: 1_001,
                    offset_ms: 1_000,
                    kind: ReplayKind::ToolCall,
                    title: "shell".into(),
                    detail: "pwd".into(),
                },
            ],
            first_ts_ms: 1,
            last_ts_ms: 1_001,
            truncated: false,
        });
        a.replay.playing = true;
        a.replay.speed = 2;
        a.replay.last_tick = std::time::Instant::now() - std::time::Duration::from_millis(600);
        assert!(a.tick());
        assert_eq!(a.selected, 1);
        assert!(!a.replay.playing);
        assert_eq!(a.replay.position_ms, 1_000.0);
    }

    #[test]
    fn kpis_animate_from_zero_on_a_filter_change() {
        let mut a = app();
        a.set_range(Range::D7);
        assert_eq!(a.kpi[0].value(), 0.0);
        for _ in 0..200 {
            a.tick();
        }
        assert_eq!(a.kpi[0].value(), a.summary.total.tokens.total() as f64);
    }

    #[test]
    fn a_days_flag_opens_on_the_chip_that_matches_it() {
        // The chips are the only thing on screen naming the active window, so
        // `--days 7` landing on the 30-day chip made the header contradict the
        // data underneath it.
        assert_eq!(Range::for_days(None), Range::All);
        assert_eq!(Range::for_days(Some(1)), Range::Today);
        assert_eq!(Range::for_days(Some(7)), Range::D7);
        assert_eq!(Range::for_days(Some(30)), Range::D30);
        assert_eq!(Range::for_days(Some(400)), Range::All);
        // Windows between chips round up to the one that still contains them.
        assert_eq!(Range::for_days(Some(3)), Range::D7);
        assert_eq!(Range::for_days(Some(45)), Range::D90);
    }

    #[test]
    fn the_today_chip_is_a_one_day_window() {
        assert_eq!(Range::Today.days(), Some(1));
        let mut a = app();
        a.set_range(Range::Today);
        assert_eq!(a.filter().since, Some(chrono::Local::now().date_naive()));
    }

    #[test]
    fn watch_waits_for_the_interval_and_for_the_scan_in_flight() {
        let mut a = app();
        let now = std::time::Instant::now();
        let ago = now - WATCH_INTERVAL - std::time::Duration::from_secs(1);

        assert!(!a.watch_due(now), "watch is off");
        a.watch = true;
        assert!(!a.watch_due(now), "nothing has scanned yet");

        a.last_scan = Some(now);
        assert!(!a.watch_due(now), "the interval has not passed");
        a.last_scan = Some(ago);
        assert!(a.watch_due(now));

        // Watch scans leave `loading` on Done, so without this guard the loop
        // would spawn a fresh thread on every 16ms tick.
        a.scan_pending = true;
        assert!(!a.watch_due(now), "a scan is already running");
        a.scan_pending = false;
        a.rescan_requested = Some(Rescan::Manual);
        assert!(!a.watch_due(now), "one is already queued");
    }

    #[test]
    fn a_failed_scan_does_not_wedge_watch_mode() {
        // A transient read error must cost one interval, not the feature.
        let mut a = app();
        a.watch = true;
        a.scan_pending = true;
        a.scan_failed("disk went away".into());
        assert!(!a.scan_pending || a.last_scan.is_some());
        a.scan_pending = false;
        a.last_scan = Some(std::time::Instant::now() - WATCH_INTERVAL);
        assert!(a.watch_due(std::time::Instant::now()), "it tries again next interval");
    }

    #[test]
    fn a_watch_scan_that_finds_nothing_new_does_not_redraw() {
        // The idle cost of a watched dashboard is the whole argument for
        // leaving one open over ssh.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(!a.needs_redraw, "nothing moved, so nothing to repaint");
    }

    #[test]
    fn crossing_midnight_repaints_even_if_nothing_moved() {
        // Left watching overnight, every window shifts a day while the totals
        // stay put. Without this the dashboard keeps yesterday's frame, today
        // figure and all, until something else happens to repaint it.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.summary_date = chrono::Local::now().date_naive() - chrono::Duration::days(1);
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(a.needs_redraw, "the date under every window changed");
    }

    #[test]
    fn recovering_from_a_failure_repaints_even_if_nothing_moved() {
        // Otherwise "scan failed" sits in the footer of a dashboard that has
        // been working again for minutes.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.scan_failed("disk went away".into());
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(matches!(a.loading, Loading::Done));
        assert!(a.needs_redraw, "the failure on screen is now wrong");
    }

    #[test]
    fn a_watch_scan_eases_from_the_number_on_screen() {
        // Replaying the count-up from zero would announce a fresh dashboard
        // when what happened is that one figure went up.
        let mut a = app();
        for _ in 0..400 {
            a.tick();
        }
        let shown = a.kpi[0].value();
        assert!(shown > 0.0);

        let mut events = a.events.clone();
        events.push(UsageEvent {
            source: Source::Claude,
            ts: chrono::Local::now().timestamp(),
            model: "claude-opus-5".into(),
            session: "s3".into(),
            project: "alpha".into(),
            tokens: Tokens { input: 1_000, output: 1_000, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 0,
        });
        a.needs_redraw = false;
        a.apply_scan(events, a.stats.clone(), Rescan::Watch);

        assert!(a.needs_redraw, "the number moved, so the frame is stale");
        assert_eq!(a.kpi[0].value(), shown, "it resumes from what was on screen");
        for _ in 0..400 {
            a.tick();
        }
        assert_eq!(a.kpi[0].value(), a.summary.total.tokens.total() as f64);
    }

    #[test]
    fn page_navigation_wraps_in_both_directions() {
        let mut a = app();
        a.set_page(Page::Settings);
        a.next_page(1);
        assert_eq!(a.page, Page::Overview);
        a.next_page(-1);
        assert_eq!(a.page, Page::Settings);
    }

    #[test]
    fn every_user_can_rename_the_local_device_in_settings_without_changing_its_id() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        let id = a.settings.device.id.clone();
        let path = std::env::temp_dir()
            .join(format!("readout-tui-device-name-{}.json", std::process::id()));
        a.settings_file = Some(path.clone());
        a.page = Page::Settings;
        a.selected = 1;
        a.activate_selected();
        assert!(a.device_name_editor);
        assert_eq!(a.device_name_input, a.settings.device.name);

        a.clear_device_name_input();
        a.save_device_name_input();
        assert!(a.device_name_editor, "an invalid empty name stays in the editor");
        assert!(a.status.as_deref().is_some_and(|status| status.contains("invalid")));

        for ch in "workstation".chars() {
            a.push_device_name_input(ch);
        }
        a.save_device_name_input();
        assert!(!a.device_name_editor);
        assert_eq!(a.settings.device.name, "workstation");
        assert_eq!(a.settings.device.id, id);
        assert_eq!(a.devices[0].name, "workstation");
        let persisted = Settings::load_from(&path).unwrap();
        assert_eq!(persisted.device.name, "workstation");
        assert_eq!(persisted.device.id, id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_ssh_settings_row_opens_the_shared_device_management_flow() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.page = Page::Settings;
        a.selected = 2;
        a.activate_selected();
        assert_eq!(a.page, Page::Devices);
        assert_eq!(a.status.as_deref(), Some("select Add SSH device… or an existing host"));
    }

    #[test]
    fn an_all_time_chart_window_is_bounded() {
        assert_eq!(Range::D7.chart_days(0), 7);
        assert_eq!(Range::All.chart_days(0), 30, "no data means a default window");
        let five_years_ago = chrono::Local::now().timestamp() - 5 * 365 * 86_400;
        assert_eq!(Range::All.chart_days(five_years_ago), 365, "capped so bars stay readable");
    }

    #[test]
    fn aggregate_setting_changes_default_totals_but_not_the_devices_page_source() {
        let mut settings = Settings::default();
        let local = settings.device.id.clone();
        settings.aggregate_devices = false;
        let mut a = App::with_settings(
            Source::ALL.to_vec(),
            Filter::default(),
            Pricing::builtin(),
            settings,
        );
        let local_event = UsageEvent {
            source: Source::Codex,
            ts: chrono::Local::now().timestamp(),
            model: "gpt-local".into(),
            session: "local-session".into(),
            project: "local-project".into(),
            tokens: Tokens { output: 10, ..Default::default() },
            observed_on: vec![local],
            dedup_key: Some("local-event".into()),
            dedup_rank: 0,
        };
        let mut remote_event = local_event.clone();
        remote_event.session = "remote-session".into();
        remote_event.tokens.output = 20;
        remote_event.observed_on = vec!["dev-remote".into()];
        remote_event.dedup_key = Some("remote-event".into());
        a.events = vec![local_event, remote_event];
        a.recompute(false);
        assert_eq!(a.summary.total.tokens.output, 10);
        assert_eq!(a.device_summary.total.tokens.output, 30);

        a.settings.aggregate_devices = true;
        a.recompute(false);
        assert_eq!(a.summary.total.tokens.output, 30);
    }

    #[test]
    fn discovered_hosts_are_not_enabled_until_the_compatibility_check_finishes() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.apply_discovered_ssh_hosts(vec!["workstation".into()]);
        a.page = Page::Devices;
        assert_eq!(a.devices.len(), 1, "unconfigured hosts stay out of the main list");
        a.selected = 1;
        a.activate_selected();
        assert!(a.device_picker);
        a.activate_selected();

        assert!(!a.settings.ssh_host_enabled("workstation"));
        assert_eq!(a.device_requested, Some(DeviceRequest::ConnectHost("workstation".into())));
        assert_eq!(a.status.as_deref(), Some("validating workstation…"));
    }

    #[test]
    fn configured_direct_hosts_stay_visible_and_can_sync_without_ssh_config() {
        let mut settings = Settings::default();
        settings.enable_ssh_host("old-host".into()).unwrap();
        let mut a = App::with_settings(
            Source::ALL.to_vec(),
            Filter::default(),
            Pricing::builtin(),
            settings,
        );
        a.devices.push(DeviceRecord {
            id: "dev-old".into(),
            name: "Old".into(),
            host: Some("old-host".into()),
            exporter_version: Some("0.2.3".into()),
            generated_at: 1,
            is_local: false,
            available: false,
            enabled: true,
            discovered: true,
            problem: None,
        });
        a.apply_discovered_ssh_hosts(Vec::new());
        a.page = Page::Devices;
        a.selected = 1;
        a.activate_selected();

        assert_eq!(a.device_requested, Some(DeviceRequest::SyncHost("old-host".into())));
        assert_eq!(a.status.as_deref(), Some("syncing old-host…"));
    }

    #[test]
    fn upgrading_a_remote_takes_two_presses_and_a_moved_selection_cancels_it() {
        // 远端升级会在另一台机器上下载并执行安装器。一次误触不该做到这件事，
        // 而确认必须落在用户当时选中的那一行上。
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.apply_discovered_ssh_hosts(vec!["gpu-01".into(), "workstation".into()]);
        a.page = Page::Devices;
        a.begin_device_picker();

        a.update_selected_device();
        assert!(a.device_requested.is_none(), "the first press only arms it");
        assert_eq!(a.update_armed.as_deref(), Some("gpu-01"));
        assert!(a.status.as_deref().is_some_and(|status| status.contains("press Ctrl+u again")));

        a.move_selection(1);
        a.update_selected_device();
        assert!(a.device_requested.is_none(), "a different row starts its own confirmation");
        assert_eq!(a.update_armed.as_deref(), Some("workstation"));

        a.update_selected_device();
        assert_eq!(a.device_requested, Some(DeviceRequest::UpdateHost("workstation".into())));
        assert!(a.update_armed.is_none());
    }

    #[test]
    fn the_device_picker_filters_large_host_lists_without_changing_settings() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.apply_discovered_ssh_hosts(vec![
            "gpu-01".into(),
            "gpu-02".into(),
            "laptop".into(),
            "workstation".into(),
        ]);
        a.page = Page::Devices;
        a.begin_device_picker();
        a.push_device_query('g');
        a.push_device_query('p');
        a.push_device_query('u');

        assert_eq!(a.device_candidates(), vec!["gpu-01", "gpu-02"]);
        assert!(a.settings.ssh_hosts.is_empty());
        a.pop_device_query();
        assert_eq!(a.device_candidates().len(), 2);
        a.cancel_device_picker();
        assert!(!a.device_picker);
        assert_eq!(a.device_row_count(), 2, "local row plus explicit add row");
    }

    #[test]
    fn the_device_picker_accepts_a_safe_hostname_not_present_in_ssh_config() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.page = Page::Devices;
        a.begin_device_picker();
        for ch in "server-42".chars() {
            a.push_device_query(ch);
        }
        assert_eq!(a.device_candidates(), vec!["server-42"]);
        a.activate_selected();
        assert_eq!(a.device_requested, Some(DeviceRequest::ConnectHost("server-42".into())));
    }

    #[test]
    fn opening_devices_reads_the_ssh_config_on_one_code_path() {
        // 发现动作由运行期开关控制而不是 cfg(test)：生产和测试跑的是同一段代码，
        // 只是测试不去碰跑测试的机器上的 ~/.ssh/config。
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        assert!(!a.discover_ssh_hosts);
        a.set_page(Page::Devices);
        assert!(!a.ssh_hosts_loaded, "discovery stays off for tests");

        a.discover_ssh_hosts = true;
        a.set_page(Page::Overview);
        a.set_page(Page::Devices);
        // 跑测试的机器上有没有 ~/.ssh/config 都不该影响结论：要么读到了，要么
        // 说清读不到的原因。断言的是这条路径真的跑了，不是它读到了什么。
        assert!(
            a.ssh_hosts_loaded
                || a.status.as_deref().is_some_and(|status| status.contains("SSH config")),
            "opening the page either loads the config or reports why it could not"
        );
    }

    #[test]
    fn remote_only_session_opens_an_explicit_usage_only_state() {
        let settings = Settings::default();
        let mut a = App::with_settings(
            Source::ALL.to_vec(),
            Filter::default(),
            Pricing::builtin(),
            settings,
        );
        a.devices.push(DeviceRecord {
            id: "dev-remote".into(),
            name: "workstation".into(),
            host: Some("workstation".into()),
            exporter_version: Some(env!("CARGO_PKG_VERSION").into()),
            generated_at: chrono::Utc::now().timestamp(),
            is_local: false,
            available: true,
            enabled: true,
            discovered: true,
            problem: None,
        });
        a.events = vec![UsageEvent {
            source: Source::Codex,
            ts: chrono::Local::now().timestamp(),
            model: "gpt-remote".into(),
            session: "remote-session".into(),
            project: "remote-project".into(),
            tokens: Tokens { output: 10, ..Default::default() },
            observed_on: vec!["dev-remote".into()],
            dedup_key: Some("remote-event".into()),
            dedup_rank: 0,
        }];
        a.recompute(false);
        a.open_session(0);
        assert_eq!(a.page, Page::Replay);
        assert!(a.replay_requested.is_none());
        assert!(a.replay.error.as_deref().is_some_and(|error| {
            error.contains("usage-only") && error.contains("workstation")
        }));
    }
}

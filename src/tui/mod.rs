//! The interactive dashboard.
//!
//! The scan runs on a background thread and streams progress into the event
//! loop, so the first frame paints immediately and the corpus fills in behind
//! it. The loop itself blocks on input with a timeout equal to the animation
//! tick, and only redraws when something actually changed — an idle dashboard
//! costs nothing, which matters over ssh.

pub mod anim;
pub mod app;
pub mod hit;
pub mod pages;
pub mod theme;
pub mod widgets;

use crate::agg::Filter;
use crate::devices::{self, LoadedUsage, SyncReport};
use crate::model::Source;
use crate::pricing::Pricing;
use crate::replay::{self, ReplayRequest, SessionReplay};
use crate::scan::Progress;
use crate::settings::Settings;
use anyhow::Result;
use app::{App, DeviceRequest, Drill, Loading, Page, Range, Rescan};
use crossterm::cursor;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hit::Action;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

/// What the scan thread sends back.
enum ScanMsg {
    Progress(Progress),
    Done(Box<LoadedUsage>),
    Failed(String),
}

enum DeviceMsg {
    Synced(SyncReport),
    Connected { host: String, updated: bool },
    Failed(String),
}

enum ReplayMsg {
    Done(SessionReplay),
    Failed(String),
}

enum SearchMsg {
    Done(Box<crate::search::Results>),
    Failed(String),
}

pub fn run(
    sources: Vec<Source>,
    base: Filter,
    use_cache: bool,
    watch: bool,
    settings: Settings,
) -> Result<()> {
    // Without a terminal there is nothing to put in raw mode, and crossterm
    // reports that as a bare "No such device or address (os error 6)". Say
    // what happened and name the two subcommands that work in a pipe.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        anyhow::bail!(
            "the dashboard needs a terminal; stdout is not one.\n\
             Try `readout summary` for text, or `readout snapshot` for one frame."
        );
    }
    let pricing = Pricing::load(crate::paths::pricing_override_file().ok().as_deref())?;
    let mut app = App::with_settings(sources.clone(), base, pricing, settings);
    app.watch = watch;

    let (mut terminal, mut guard) = setup()?;
    // Whatever happens below, the guard restores the terminal before an error
    // propagates, and during unwinding as well.
    let result = event_loop(&mut terminal, &mut app, sources, use_cache);
    guard.restore(&mut terminal)?;
    result
}

/// Render one settled frame to a string of ANSI escapes, without touching the
/// terminal state.
///
/// This exists so the layout can be inspected at an exact size — in a test, in
/// a bug report, or piped to a file — rather than only by eye at whatever size
/// the window happens to be.
pub struct SnapshotRequest {
    pub sources: Vec<Source>,
    pub filter: Filter,
    pub use_cache: bool,
    pub width: u16,
    pub height: u16,
    pub page: Page,
    /// Run this search before drawing. The Search page is a prompt until
    /// something has been searched, so without it a snapshot of that page
    /// could never show the layout it exists to show.
    pub query: Option<String>,
    pub settings: Settings,
}

pub fn snapshot(request: SnapshotRequest) -> Result<String> {
    let SnapshotRequest { sources, filter, use_cache, width, height, page, query, settings } =
        request;
    let pricing = Pricing::load(crate::paths::pricing_override_file().ok().as_deref())?;
    let mut app = App::with_settings(sources.clone(), filter, pricing, settings.clone());
    let result = devices::load_usage(&sources, use_cache, &settings, None)?;
    app.events = result.scan.events;
    app.stats = result.scan.stats;
    app.devices = result.devices;
    if let Some(first) = result.warnings.first() {
        app.status = Some(format!("skipped {first}"));
    }
    app.loading = Loading::Done;
    app.set_page(page);
    // Snapshots show the settled state; animating into a still image would
    // only ever capture a half-drawn frame.
    app.recompute(false);
    if let Some(query) = query {
        app.search.query = query;
        app.submit_search();
        match app.search_requested.take() {
            Some(request) => match crate::search::run(&request) {
                Ok(results) => app.apply_search(results),
                Err(error) => app.fail_search(crate::fmt::error_chain(&error)),
            },
            None => app.search.editing = true,
        }
    }

    let area = ratatui::layout::Rect { x: 0, y: 0, width, height };
    let mut buf = ratatui::buffer::Buffer::empty(area);
    pages::draw(&mut app, &mut buf, area);
    Ok(render_ansi(&buf))
}

fn render_ansi(buf: &ratatui::buffer::Buffer) -> String {
    use ratatui::style::Color;
    fn code(c: Color, fg: bool) -> String {
        let base = if fg { 38 } else { 48 };
        match c {
            Color::Rgb(r, g, b) => format!("\x1b[{base};2;{r};{g};{b}m"),
            Color::Reset => format!("\x1b[{}m", if fg { 39 } else { 49 }),
            _ => String::new(),
        }
    }
    let mut out = String::new();
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            let Some(cell) = buf.cell((x, y)) else { continue };
            out.push_str(&code(cell.fg, true));
            out.push_str(&code(cell.bg, false));
            if cell.modifier.contains(ratatui::style::Modifier::BOLD) {
                out.push_str("\x1b[1m");
            }
            out.push_str(cell.symbol());
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

type Term = Terminal<CrosstermBackend<Stdout>>;

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn restore(&mut self, terminal: &mut Term) -> Result<()> {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
    }
}

fn setup() -> Result<(Term, TerminalGuard)> {
    enable_raw_mode()?;
    let guard = TerminalGuard { active: true };
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok((terminal, guard))
}

fn spawn_scan(sources: Vec<Source>, use_cache: bool, settings: Settings) -> Receiver<ScanMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let tx2: Sender<ScanMsg> = tx.clone();
        let report = move |p: Progress| {
            // A closed channel means the UI is gone; dropping the send is the
            // correct response, not an error.
            let _ = tx2.send(ScanMsg::Progress(p));
        };
        match devices::load_usage(&sources, use_cache, &settings, Some(&report)) {
            Ok(r) => {
                let _ = tx.send(ScanMsg::Done(Box::new(r)));
            }
            Err(e) => {
                let _ = tx.send(ScanMsg::Failed(e.to_string()));
            }
        }
    });
    rx
}

fn spawn_device(settings: Settings, request: DeviceRequest) -> Receiver<DeviceMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let result = match request {
            DeviceRequest::SyncAll => devices::sync_all(&settings, None).map(DeviceMsg::Synced),
            DeviceRequest::SyncHost(host) => {
                devices::sync_all(&settings, Some(&host)).map(DeviceMsg::Synced)
            }
            DeviceRequest::ConnectHost(host) => devices::sync_host(&settings, &host)
                .map(|_| DeviceMsg::Connected { host, updated: false }),
            DeviceRequest::UpdateHost(host) => devices::update_remote(&host)
                .and_then(|_| devices::sync_host(&settings, &host))
                .map(|_| DeviceMsg::Connected { host, updated: true }),
        };
        let message = match result {
            Ok(message) => message,
            Err(error) => DeviceMsg::Failed(crate::fmt::error_chain(&error)),
        };
        let _ = tx.send(message);
    });
    rx
}

fn spawn_replay(request: ReplayRequest) -> Receiver<ReplayMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let message = match replay::load(request) {
            Ok(replay) => ReplayMsg::Done(replay),
            Err(error) => ReplayMsg::Failed(error.to_string()),
        };
        let _ = tx.send(message);
    });
    rx
}

/// Search reads the whole corpus, so it runs where the scan does: off the
/// event loop, leaving the dashboard responsive while it works.
fn spawn_search(request: crate::search::Request) -> Receiver<SearchMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let message = match crate::search::run(&request) {
            Ok(results) => SearchMsg::Done(Box::new(results)),
            Err(error) => SearchMsg::Failed(crate::fmt::error_chain(&error)),
        };
        let _ = tx.send(message);
    });
    rx
}

fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    sources: Vec<Source>,
    use_cache: bool,
) -> Result<()> {
    let mut rx = spawn_scan(sources.clone(), use_cache, app.settings.clone());
    let mut replay_rx: Option<Receiver<ReplayMsg>> = None;
    let mut search_rx: Option<Receiver<SearchMsg>> = None;
    let mut device_rx: Option<Receiver<DeviceMsg>> = None;
    app.scan_pending = true;
    let mut kind = Rescan::Manual;

    loop {
        if let Some(request) = app.replay_requested.take()
            && app.page == Page::Replay
        {
            replay_rx = Some(spawn_replay(request));
        }
        if let Some(receiver) = replay_rx.as_ref()
            && drain_replay(app, receiver)
        {
            replay_rx = None;
        }
        if let Some(request) = app.search_requested.take() {
            search_rx = Some(spawn_search(request));
        }
        if let Some(receiver) = search_rx.as_ref()
            && drain_search(app, receiver)
        {
            search_rx = None;
        }
        if let Some(receiver) = device_rx.as_ref()
            && drain_device(app, receiver)
        {
            device_rx = None;
        }

        if app.needs_redraw {
            terminal.draw(|f| {
                let area = f.area();
                pages::draw(app, f.buffer_mut(), area);
            })?;
            app.needs_redraw = false;
        }

        drain_scan(app, &rx, kind);

        if app.watch_due(std::time::Instant::now()) {
            app.request_rescan(Rescan::Watch);
        }
        if let Some(request) = app.device_requested.take() {
            app.device_pending = true;
            device_rx = Some(spawn_device(app.settings.clone(), request));
        }

        if let Some(next) = app.rescan_requested.take() {
            kind = next;
            // A watch scan leaves the dashboard exactly as it is until it has
            // something to say. Only the manual one announces itself.
            if kind == Rescan::Manual {
                app.loading = Loading::Scanning(None);
                app.status = Some("rescanning…".into());
                app.needs_redraw = true;
            }
            app.scan_pending = true;
            rx = spawn_scan(sources.clone(), use_cache, app.settings.clone());
        }

        if event::poll(anim::TICK)? {
            // Drain the queue in one go: a fast mouse or a held key can
            // outpace the frame rate, and redrawing per event would fall
            // behind. One redraw after the burst keeps input responsive.
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(k) => on_key(app, k),
                    Event::Mouse(m) => on_mouse(app, m),
                    Event::Resize(_, _) => app.needs_redraw = true,
                    _ => {}
                }
                if app.should_quit {
                    return Ok(());
                }
            }
        }

        if app.tick() {
            app.needs_redraw = true;
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn drain_device(app: &mut App, rx: &Receiver<DeviceMsg>) -> bool {
    match rx.try_recv() {
        Ok(DeviceMsg::Synced(report)) => {
            app.sync_finished(&report);
            true
        }
        Ok(DeviceMsg::Connected { host, updated }) => {
            app.connect_finished(&host, updated);
            true
        }
        Ok(DeviceMsg::Failed(error)) => {
            app.sync_failed(error);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            if app.device_pending {
                app.sync_failed("the device worker stopped without reporting".into());
            }
            true
        }
    }
}

/// 返回 true 表示后台搜索已经结束，可以丢弃 receiver。
fn drain_search(app: &mut App, rx: &Receiver<SearchMsg>) -> bool {
    match rx.try_recv() {
        Ok(SearchMsg::Done(results)) => {
            app.apply_search(*results);
            true
        }
        Ok(SearchMsg::Failed(error)) => {
            app.fail_search(error);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            if app.search.running {
                app.fail_search("the search stopped without reporting".into());
            }
            true
        }
    }
}

/// 返回 true 表示后台 replay 已经结束，可以丢弃 receiver。
fn drain_replay(app: &mut App, rx: &Receiver<ReplayMsg>) -> bool {
    match rx.try_recv() {
        Ok(ReplayMsg::Done(replay)) => {
            app.apply_replay(replay);
            true
        }
        Ok(ReplayMsg::Failed(error)) => {
            app.fail_replay(error);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            if app.replay.loading {
                app.fail_replay("the replay loader stopped without reporting".into());
            }
            true
        }
    }
}

fn drain_scan(app: &mut App, rx: &Receiver<ScanMsg>, kind: Rescan) {
    loop {
        match rx.try_recv() {
            // Progress from a watch scan is deliberately dropped: reporting it
            // would put the dashboard back into a loading state every few
            // seconds to say something the user did not ask about.
            Ok(ScanMsg::Progress(p)) => {
                if kind == Rescan::Manual {
                    app.loading = Loading::Scanning(Some(p));
                    app.needs_redraw = true;
                }
            }
            Ok(ScanMsg::Done(result)) => {
                app.scan_pending = false;
                app.devices = result.devices;
                if app.page == Page::Devices && app.ssh_hosts_loaded {
                    app.refresh_ssh_hosts();
                }
                app.apply_scan(result.scan.events, result.scan.stats, kind);
                // 本机数字照常显示，但坏掉的快照必须说出来——否则总量少了一台
                // 设备，界面上却看不出任何异常。
                if let Some(first) = result.warnings.first() {
                    app.status = Some(match result.warnings.len() {
                        1 => format!("skipped {first}"),
                        n => format!("skipped {first} · {} more device(s)", n - 1),
                    });
                }
            }
            Ok(ScanMsg::Failed(e)) => {
                app.scan_pending = false;
                app.scan_failed(e);
            }
            Err(TryRecvError::Empty) => return,
            // The thread ended without a verdict, which means it died. Say so
            // rather than leaving watch mode waiting forever on a scan that
            // will never report.
            Err(TryRecvError::Disconnected) => {
                if app.scan_pending {
                    app.scan_pending = false;
                    app.scan_failed("the scan stopped without reporting".into());
                }
                return;
            }
        }
    }
}

fn on_key(app: &mut App, k: KeyEvent) {
    // Windows sends both press and release; acting on both double-fires.
    if k.kind != KeyEventKind::Press {
        return;
    }
    app.status = None;
    app.needs_redraw = true;

    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') => app.should_quit = true,
            KeyCode::Char('u') if app.page == Page::Settings && app.device_name_editor => {
                app.clear_device_name_input();
            }
            KeyCode::Char('u') if app.page == Page::Devices && app.device_picker => {
                app.update_selected_device();
            }
            KeyCode::Char('u') if app.page == Page::Search && app.search.editing => {
                app.clear_search_query();
            }
            _ => {}
        }
        return;
    }

    if app.page == Page::Replay {
        match k.code {
            KeyCode::Esc => app.back_to_sessions(),
            KeyCode::Char(' ') => app.toggle_replay(),
            KeyCode::Char('1') => app.set_replay_speed(1),
            KeyCode::Char('2') => app.set_replay_speed(2),
            KeyCode::Char('4') => app.set_replay_speed(4),
            KeyCode::Char('[') => move_and_follow(app, -1),
            KeyCode::Char(']') => move_and_follow(app, 1),
            _ => {}
        }
        if matches!(k.code, KeyCode::Esc | KeyCode::Char(' ' | '1' | '2' | '4' | '[' | ']')) {
            return;
        }
    }

    if app.page == Page::Settings && app.device_name_editor {
        match k.code {
            KeyCode::Esc => {
                app.cancel_device_name_editor();
                app.status = Some("device name edit cancelled".into());
            }
            KeyCode::Backspace => app.pop_device_name_input(),
            KeyCode::Enter => app.save_device_name_input(),
            KeyCode::Char(ch)
                if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.push_device_name_input(ch);
            }
            _ => {}
        }
        return;
    }

    // The query editor owns the keyboard while it has focus, so `d`, `q` and
    // the rest reach the query rather than the dashboard's shortcuts.
    if app.page == Page::Search && app.search.editing {
        match k.code {
            KeyCode::Esc => app.cancel_search_edit(),
            KeyCode::Backspace => app.pop_search_query(),
            KeyCode::Enter => app.submit_search(),
            KeyCode::Char(ch)
                if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.push_search_query(ch);
            }
            _ => {}
        }
        return;
    }
    if app.page == Page::Search {
        match k.code {
            KeyCode::Char('/') | KeyCode::Char('i') => {
                app.begin_search_edit();
                return;
            }
            KeyCode::Enter => {
                app.open_search_session(app.selected);
                return;
            }
            _ => {}
        }
    }

    if app.page == Page::Devices && app.device_picker {
        match k.code {
            KeyCode::Esc => {
                app.cancel_device_picker();
                app.status = Some("device picker closed".into());
                return;
            }
            KeyCode::Backspace => {
                app.pop_device_query();
                return;
            }
            KeyCode::Enter => {
                app.activate_selected();
                return;
            }
            KeyCode::Char(ch)
                if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.push_device_query(ch);
                return;
            }
            _ => {}
        }
    } else if app.page == Page::Devices {
        match k.code {
            KeyCode::Char('r') => {
                app.request_sync();
                return;
            }
            KeyCode::Delete | KeyCode::Backspace => {
                app.disable_selected_device();
                return;
            }
            KeyCode::Char('u') => {
                app.update_selected_device();
                return;
            }
            // Esc 先撤销待确认的远端升级，之后才轮到它平时的清除筛选。
            KeyCode::Esc if app.update_armed.is_some() => {
                app.disarm_update();
                app.status = Some("update cancelled".into());
                return;
            }
            _ => {}
        }
    }

    match k.code {
        KeyCode::Char('q') | KeyCode::Esc if app.drill == Drill::None => {
            if k.code == KeyCode::Char('q') {
                app.should_quit = true;
            }
        }
        KeyCode::Esc => app.set_drill(Drill::None),
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('r') => app.request_rescan(Rescan::Manual),
        // `/` means search everywhere it is not already typing.
        KeyCode::Char('/') => {
            app.set_page(Page::Search);
            app.begin_search_edit();
        }
        KeyCode::Char('w') => app.toggle_watch(),
        KeyCode::Char('t') => app.set_range(Range::Today),
        KeyCode::Tab | KeyCode::Right => app.next_page(1),
        KeyCode::BackTab | KeyCode::Left => app.next_page(-1),
        KeyCode::Down | KeyCode::Char('j') => move_and_follow(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_and_follow(app, -1),
        KeyCode::PageDown => move_and_follow(app, 10),
        KeyCode::PageUp => move_and_follow(app, -10),
        KeyCode::Home => move_and_follow(app, -(app.row_count() as isize)),
        KeyCode::End => move_and_follow(app, app.row_count() as isize),
        KeyCode::Enter => app.activate_selected(),
        KeyCode::Char('1') => app.set_range(Range::D7),
        KeyCode::Char('2') => app.set_range(Range::D30),
        KeyCode::Char('3') => app.set_range(Range::D90),
        KeyCode::Char('4') => app.set_range(Range::All),
        KeyCode::Char('c') => app.toggle_source(Source::Claude),
        KeyCode::Char('x') => app.toggle_source(Source::Codex),
        KeyCode::Char('?') => {
            app.status = Some(
                "click the sidebar, chips, and rows · wheel scrolls · enter drills in · \
                 / searches your history · w keeps the numbers live"
                    .into(),
            )
        }
        _ => {
            app.needs_redraw = false;
        }
    }
}

/// Move the selection and scroll the window to keep it on screen.
///
/// The row count comes from the last frame, which is the only place that knows
/// how tall the list drew. A selection the user cannot see is the same as no
/// selection at all.
fn move_and_follow(app: &mut App, delta: isize) {
    app.move_selection(delta);
    app.ensure_visible(app.list_rows.get());
}

fn on_mouse(app: &mut App, m: MouseEvent) {
    match m.kind {
        MouseEventKind::Moved => {
            let hovered = app.hits.hover_at(m.column, m.row);
            if hovered != app.hover {
                app.hover = hovered;
                app.needs_redraw = true;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(action) = app.hits.hit(m.column, m.row).cloned() else { return };
            app.status = None;
            apply(app, action);
            app.needs_redraw = true;
        }
        MouseEventKind::ScrollDown => {
            app.scroll_by(3);
        }
        MouseEventKind::ScrollUp => {
            app.scroll_by(-3);
        }
        _ => {}
    }
}

fn apply(app: &mut App, action: Action) {
    match action {
        Action::Page(p) => app.set_page(p),
        Action::Range(r) => app.set_range(r),
        Action::ToggleSource(s) => app.toggle_source(s),
        Action::Row(i) => {
            // Single-click selects; a second click on the row already selected
            // opens it. Drilling on the first click would let one stray click
            // filter the whole dashboard before the user had picked anything.
            if app.selected == i {
                app.activate_selected();
            } else {
                app.selected = i;
                app.disarm_update();
            }
        }
        Action::ProjectRow(i) => app.open_project(i),
        Action::SessionRow(i) => app.open_session(i),
        Action::InsightSessionRow(i) => app.open_insight_session(i),
        Action::SearchRow(i) => app.open_search_session(i),
        Action::SearchSample(i) => app.open_search_sample(i),
        Action::SearchEdit => app.begin_search_edit(),
        Action::BackToSessions => app.back_to_sessions(),
        Action::ReplayToggle => app.toggle_replay(),
        Action::ReplaySpeed(speed) => app.set_replay_speed(speed),
        Action::ReplaySeek(i) => app.seek_replay(i),
        Action::Setting(i) => {
            app.selected = i;
            app.activate_setting(i);
        }
        Action::SyncDevices => app.request_sync(),
        Action::ClearFilter => app.set_drill(Drill::None),
        Action::Refresh => app.request_rescan(Rescan::Manual),
        Action::ToggleWatch => app.toggle_watch(),
        Action::Quit => app.should_quit = true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tokens;
    use crate::model::UsageEvent;
    use crate::replay::{ReplayEvent, ReplayKind, ReplayRequest, SessionReplay};

    fn app_with_data() -> App {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.events = (0..5)
            .map(|i| UsageEvent {
                source: if i % 2 == 0 { Source::Claude } else { Source::Codex },
                ts: chrono::Local::now().timestamp() - i * 3600,
                model: format!("model-{i}"),
                session: format!("s{i}"),
                project: format!("p{i}"),
                tokens: Tokens { input: 100 * (i as u64 + 1), output: 10, ..Default::default() },
                observed_on: Vec::new(),
                dedup_key: None,
                dedup_rank: 0,
            })
            .collect();
        a.recompute(false);
        a
    }

    fn press(app: &mut App, code: KeyCode) {
        on_key(
            app,
            KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: event::KeyEventState::NONE,
            },
        );
    }

    #[test]
    fn escape_clears_a_drill_down_before_it_can_quit() {
        let mut a = app_with_data();
        a.set_drill(Drill::Model("model-1".into()));
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.drill, Drill::None);
        assert!(!a.should_quit, "the first escape clears the filter rather than exiting");
        press(&mut a, KeyCode::Esc);
        assert!(!a.should_quit, "escape alone never quits");
        press(&mut a, KeyCode::Char('q'));
        assert!(a.should_quit);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut a = app_with_data();
        on_key(
            &mut a,
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Release,
                state: event::KeyEventState::NONE,
            },
        );
        assert!(!a.should_quit, "acting on release would double-fire every key");
    }

    #[test]
    fn ctrl_c_quits() {
        let mut a = app_with_data();
        on_key(
            &mut a,
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: event::KeyEventState::NONE,
            },
        );
        assert!(a.should_quit);
    }

    #[test]
    fn the_settings_name_editor_captures_keys_and_renders_the_current_value() {
        let mut a = app_with_data();
        a.set_page(Page::Settings);
        a.begin_device_name_editor();
        on_key(
            &mut a,
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: event::KeyEventState::NONE,
            },
        );
        for ch in "custom-node".chars() {
            press(&mut a, KeyCode::Char(ch));
        }
        let frame = buffer_text(&render(&mut a, 110, 24));
        assert!(frame.contains("Rename local device"));
        assert!(frame.contains("custom-node_"));
        assert!(!a.should_quit);

        press(&mut a, KeyCode::Esc);
        assert!(!a.device_name_editor);
        assert_eq!(a.page, Page::Settings);
    }

    #[test]
    fn number_keys_select_the_matching_range() {
        let mut a = app_with_data();
        press(&mut a, KeyCode::Char('1'));
        assert_eq!(a.range, Range::D7);
        press(&mut a, KeyCode::Char('4'));
        assert_eq!(a.range, Range::All);
    }

    #[test]
    fn clicking_a_model_row_toggles_the_drill_down() {
        // Rows go through the one Row action everywhere, so the sequence a
        // mouse produces is click-to-select, click-to-open, click-to-release.
        let mut a = app_with_data();
        a.set_page(Page::Models);
        let model = a.summary.by_model[1].label.clone();
        apply(&mut a, Action::Row(1));
        assert_eq!(a.drill, Drill::None, "the first click only selects");

        apply(&mut a, Action::Row(1));
        assert_eq!(a.drill, Drill::Model(model));
        // The filter leaves one model standing and the selection lands on it,
        // so the next click activates straight away — and activating the row
        // that *is* the current filter releases it.
        apply(&mut a, Action::Row(0));
        assert_eq!(a.drill, Drill::None, "activating the current filter releases it");
    }

    #[test]
    fn a_refresh_click_requests_a_rescan() {
        let mut a = app_with_data();
        apply(&mut a, Action::Refresh);
        assert_eq!(a.rescan_requested, Some(Rescan::Manual));
    }

    #[test]
    fn devices_refresh_is_manual_and_uses_the_background_sync_worker() {
        let mut a = app_with_data();
        a.settings.enable_ssh_host("workstation".into()).unwrap();
        a.page = Page::Devices;
        press(&mut a, KeyCode::Char('r'));
        assert_eq!(a.device_requested, Some(DeviceRequest::SyncAll));
        assert!(a.rescan_requested.is_none());
    }

    #[test]
    fn a_second_refresh_does_not_stack_a_second_scan() {
        // Every request spawns a thread. Clicking the scan summary twice, or
        // holding `r`, must not start a scan per click.
        let mut a = app_with_data();
        apply(&mut a, Action::Refresh);
        a.scan_pending = true;
        a.rescan_requested = None;
        apply(&mut a, Action::Refresh);
        assert_eq!(a.rescan_requested, None, "a scan is already running");
    }

    /// Run the event loop's scan-scheduling step for a stretch of frames.
    ///
    /// This is the loop body with the terminal, the input and the scan thread
    /// removed — what is left is the decision the loop makes ~60 times a
    /// second, which is the part that can go wrong at that rate.
    fn drive(app: &mut App, frames: u32, finish_scans: bool) -> usize {
        let t0 = std::time::Instant::now();
        app.last_scan = Some(t0);
        let mut spawned = 0;
        for f in 0..frames {
            let now = t0 + anim::TICK * f;
            if app.watch_due(now) {
                app.request_rescan(Rescan::Watch);
            }
            if app.rescan_requested.take().is_some() {
                app.scan_pending = true;
                spawned += 1;
            }
            if finish_scans && app.scan_pending {
                app.scan_pending = false;
                app.last_scan = Some(now);
            }
        }
        spawned
    }

    #[test]
    fn watching_starts_one_scan_per_interval_not_one_per_frame() {
        let mut a = app_with_data();
        a.watch = true;
        let per_interval = (app::WATCH_INTERVAL.as_millis() / anim::TICK.as_millis()) as u32;
        // Three and a half intervals. The interval is measured from when the
        // last scan *finished*, so each one lands a little after the tick
        // that was due — the half gives that drift somewhere to go without
        // admitting a fourth scan.
        let spawned = drive(&mut a, per_interval * 7 / 2, true);
        assert_eq!(spawned, 3, "one per interval, no more");
    }

    #[test]
    fn a_scan_that_never_finishes_is_never_joined_by_a_second() {
        // Watch scans leave `loading` on Done by design, so the elapsed time
        // keeps growing while one is in flight. Without the in-flight guard
        // the loop would spawn a thread on every frame from then on.
        let mut a = app_with_data();
        a.watch = true;
        let per_interval = (app::WATCH_INTERVAL.as_millis() / anim::TICK.as_millis()) as u32;
        assert_eq!(drive(&mut a, per_interval * 5, false), 1, "one scan, still running");
    }

    #[test]
    fn nothing_is_scanned_on_a_timer_unless_watching() {
        let mut a = app_with_data();
        let per_interval = (app::WATCH_INTERVAL.as_millis() / anim::TICK.as_millis()) as u32;
        assert_eq!(drive(&mut a, per_interval * 3, true), 0);
    }

    #[test]
    fn the_watch_chip_toggles_watching() {
        let mut a = app_with_data();
        assert!(!a.watch);
        apply(&mut a, Action::ToggleWatch);
        assert!(a.watch);
        apply(&mut a, Action::ToggleWatch);
        assert!(!a.watch);
    }

    #[test]
    fn a_row_click_selects_before_it_drills() {
        // The advertised mouse contract: one click picks a row, a second click
        // on that same row filters by it. A first click that drilled would
        // rewrite the whole dashboard from a passing click.
        let mut a = app_with_many_rows();
        a.set_page(Page::Models);
        let target = 3;
        apply(&mut a, Action::Row(target));
        assert_eq!(a.selected, target);
        assert_eq!(a.drill, Drill::None, "the first click must not filter");

        // Read the label first: drilling filters the list it came from.
        let label = a.summary.by_model[target].label.clone();
        apply(&mut a, Action::Row(target));
        assert_eq!(a.drill, Drill::Model(label), "the second click opens the row");
    }

    #[test]
    fn a_session_row_opens_its_replay_not_a_model_filter() {
        let mut a = app_with_data();
        let target = 1;
        let session = a.summary.by_session[target].label.clone();

        apply(&mut a, Action::SessionRow(target));
        assert_eq!(a.page, Page::Replay);
        assert!(a.replay.loading);
        assert_eq!(a.drill, Drill::None);
        assert_eq!(
            a.replay_requested.as_ref().map(|request| request.session.as_str()),
            Some(session.as_str())
        );
    }

    #[test]
    fn an_insight_row_opens_the_session_it_names_not_the_one_at_that_offset() {
        // Insights ranks by cost and Sessions by recency, so the two lists
        // disagree about what row 0 is. Resolving an insight row against
        // `by_session` would quietly replay a different session than the one
        // the user clicked.
        let mut a = app_with_data();
        a.set_page(Page::Insights);
        let ranked = a.insights.costly_sessions[0].session.clone();
        assert_ne!(
            ranked, a.summary.by_session[0].label,
            "the two orderings must differ for this test to mean anything"
        );

        apply(&mut a, Action::InsightSessionRow(0));
        assert_eq!(a.page, Page::Replay);
        assert_eq!(
            a.replay_requested.as_ref().map(|request| request.session.as_str()),
            Some(ranked.as_str())
        );
        // Esc goes back to the ranking it was opened from, not to a list the
        // user was never on.
        assert_eq!(a.replay.return_page, Page::Insights);
        a.back_to_sessions();
        assert_eq!(a.page, Page::Insights);
    }

    #[test]
    fn insights_are_recomputed_with_the_summary_rather_than_alongside_it() {
        // Two views of one window that are accumulated separately can drift
        // apart. These are both derived from the same recompute, so a drill
        // that narrows one must narrow the other by construction.
        let mut a = app_with_data();
        assert_eq!(a.insights.tokens, a.summary.total.tokens);
        a.set_drill(Drill::Model("model-1".into()));
        assert_eq!(a.insights.tokens, a.summary.total.tokens);
        assert_eq!(a.insights.session_total, a.summary.by_session.len());
        assert_eq!(a.row_count(), a.insights.costly_sessions.len().min(a.row_count()));
    }

    #[test]
    fn an_all_time_window_shows_no_period_comparison() {
        // There is no window before all of them, and inventing one to fill the
        // card would be a fabricated baseline.
        let mut a = app_with_data();
        a.set_range(Range::All);
        assert!(a.insights.previous.is_none());
        a.set_range(Range::D7);
        assert!(a.insights.previous.is_some());
    }

    #[test]
    fn hover_only_redraws_when_the_target_changes() {
        let mut a = app_with_data();
        a.hits.add_hoverable(
            ratatui::layout::Rect { x: 0, y: 0, width: 5, height: 1 },
            Action::Row(0),
            42,
        );
        a.needs_redraw = false;
        on_mouse(&mut a, mouse(MouseEventKind::Moved, 1, 0));
        assert_eq!(a.hover, Some(42));
        assert!(a.needs_redraw);

        a.needs_redraw = false;
        on_mouse(&mut a, mouse(MouseEventKind::Moved, 2, 0));
        assert!(!a.needs_redraw, "the same target must not force a frame");
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn the_wheel_scrolls_the_list() {
        let mut a = app_with_data();
        a.set_page(Page::Models);
        on_mouse(&mut a, mouse(MouseEventKind::ScrollDown, 30, 10));
        assert!(a.scroll > 0);
        on_mouse(&mut a, mouse(MouseEventKind::ScrollUp, 30, 10));
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn a_click_on_empty_space_does_nothing() {
        let mut a = app_with_data();
        let before = a.page;
        on_mouse(&mut a, mouse(MouseEventKind::Down(MouseButton::Left), 200, 200));
        assert_eq!(a.page, before);
        assert!(!a.should_quit);
    }

    #[test]
    fn the_dashboard_renders_at_a_range_of_sizes_without_panicking() {
        let mut a = app_with_data();
        for page in Page::ORDER {
            a.set_page(page);
            for (w, h) in [(200u16, 60u16), (120, 40), (80, 24), (40, 12), (20, 8), (8, 4), (1, 1)]
            {
                let area = ratatui::layout::Rect { x: 0, y: 0, width: w, height: h };
                let mut buf = ratatui::buffer::Buffer::empty(area);
                pages::draw(&mut a, &mut buf, area);
            }
        }
    }

    #[test]
    fn session_replay_renders_trace_timeline_and_compact_sizes() {
        let mut a = app_with_data();
        a.page = Page::Replay;
        a.replay.request = Some(ReplayRequest {
            source: Source::Codex,
            session: "s0".into(),
            project: "p0".into(),
            model: "model-0".into(),
        });
        a.replay.data = Some(SessionReplay {
            events: vec![
                ReplayEvent {
                    ts_ms: 1_787_000_000_000,
                    offset_ms: 0,
                    kind: ReplayKind::User,
                    title: "user".into(),
                    detail: "please inspect the repository".into(),
                },
                ReplayEvent {
                    ts_ms: 1_787_000_002_000,
                    offset_ms: 2_000,
                    kind: ReplayKind::ToolCall,
                    title: "shell".into(),
                    detail: "rg --files".into(),
                },
                ReplayEvent {
                    ts_ms: 1_787_000_003_000,
                    offset_ms: 3_000,
                    kind: ReplayKind::ToolResult,
                    title: "shell".into(),
                    detail: "Cargo.toml src/main.rs".into(),
                },
            ],
            first_ts_ms: 1_787_000_000_000,
            last_ts_ms: 1_787_000_003_000,
            truncated: false,
        });
        for (width, height) in [(160, 48), (100, 30), (60, 18), (20, 8), (1, 1)] {
            let buffer = render(&mut a, width, height);
            if width >= 100 && height >= 30 {
                let text = buffer_text(&buffer);
                assert!(text.contains("Event Trace"));
                assert!(text.contains("shell"));
                assert!(text.contains("1x"));
            }
        }
    }

    fn search_results(matches: usize) -> crate::search::Results {
        crate::search::Results {
            query: "deadlock".into(),
            sessions: vec![
                crate::search::SessionHits {
                    source: Source::Claude,
                    session: "sess-recent".into(),
                    project: "/w/alpha".into(),
                    last_ts_ms: 1_787_000_002_000,
                    matches,
                    samples: vec![
                        crate::search::Hit {
                            ts_ms: 1_787_000_001_000,
                            kind: ReplayKind::User,
                            title: "user".into(),
                            snippet: "why does the pool deadlock".into(),
                        },
                        crate::search::Hit {
                            ts_ms: 1_787_000_002_000,
                            kind: ReplayKind::ToolCall,
                            title: "Bash".into(),
                            snippet: "grep -rn deadlock src".into(),
                        },
                    ],
                },
                crate::search::SessionHits {
                    source: Source::Codex,
                    session: "sess-older".into(),
                    project: "/w/beta".into(),
                    last_ts_ms: 1_786_000_000_000,
                    matches: 1,
                    samples: vec![crate::search::Hit {
                        ts_ms: 1_786_000_000_000,
                        kind: ReplayKind::Assistant,
                        title: "assistant".into(),
                        snippet: "the deadlock was in the waiter".into(),
                    }],
                },
            ],
            sessions_matched: 9,
            total_matches: matches + 1,
            files_searched: 400,
            files_failed: 0,
            files_total: 400,
            bytes_read: 1_000,
            truncated: false,
            elapsed_ms: 1_500,
        }
    }

    #[test]
    fn search_results_name_their_sessions_and_quote_the_lines_that_matched() {
        let mut a = app_with_many_rows();
        a.set_page(Page::Search);
        a.search.query = "deadlock".into();
        a.apply_search(search_results(7));

        let text = buffer_text(&render(&mut a, 130, 30));
        assert!(text.contains("/w/alpha"), "a result is named by its project: {text}");
        assert!(text.contains("grep -rn deadlock src"), "the matching line is quoted");
        assert!(text.contains("Bash"), "a tool hit names its tool");
        // Nine sessions matched and two are shown; silence would read as two.
        assert!(text.contains("showing the 2 most recent of 9"), "{text}");
        // The samples say they are a sample.
        assert!(text.contains("2 of 7"), "{text}");
    }

    #[test]
    fn a_narrow_search_page_drops_the_sample_pane_rather_than_the_results() {
        let mut a = app_with_many_rows();
        a.set_page(Page::Search);
        a.apply_search(search_results(2));
        for (width, height) in [(160, 48), (130, 30), (100, 24), (60, 18), (20, 8), (1, 1)] {
            let text = buffer_text(&render(&mut a, width, height));
            if width >= 60 && height >= 18 {
                assert!(text.contains("/w/alpha"), "{width}x{height}: {text}");
            }
        }
    }

    #[test]
    fn opening_a_hit_seeks_the_replay_to_the_moment_it_matched() {
        let mut a = app_with_many_rows();
        a.set_page(Page::Search);
        a.apply_search(search_results(2));
        // The second sample, not the first: a hit names a moment.
        a.open_search_sample(1);
        assert_eq!(a.page, Page::Replay);
        assert_eq!(a.replay.pending_seek_ts_ms, Some(1_787_000_002_000));
        assert_eq!(
            a.replay_requested.as_ref().map(|r| r.session.as_str()),
            Some("sess-recent"),
            "the request goes straight to the matched session"
        );

        a.apply_replay(SessionReplay {
            events: vec![
                ReplayEvent {
                    ts_ms: 1_787_000_000_000,
                    offset_ms: 0,
                    kind: ReplayKind::User,
                    title: "user".into(),
                    detail: "start".into(),
                },
                ReplayEvent {
                    ts_ms: 1_787_000_002_000,
                    offset_ms: 2_000,
                    kind: ReplayKind::ToolCall,
                    title: "Bash".into(),
                    detail: "grep".into(),
                },
            ],
            first_ts_ms: 1_787_000_000_000,
            last_ts_ms: 1_787_000_002_000,
            truncated: false,
        });
        assert_eq!(a.selected, 1, "the replay lands on the matching event, not on the start");
        assert_eq!(a.replay.position_ms, 2_000.0);
        assert_eq!(a.replay.pending_seek_ts_ms, None, "the seek is consumed once");

        // Esc returns to the search results rather than to a list the reader
        // was never on.
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.page, Page::Search);
    }

    #[test]
    fn a_hit_in_a_session_with_no_billed_usage_still_opens_its_replay() {
        // `sess-recent` has no usage event, so resolving the source through
        // the filtered event stream would find nothing. Search already knows
        // it: it matched the file on this machine.
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.set_page(Page::Search);
        a.apply_search(search_results(1));
        a.open_search_session(0);
        assert_eq!(a.page, Page::Replay);
        assert!(a.replay.error.is_none(), "{:?}", a.replay.error);
        assert!(a.replay_requested.is_some());
    }

    #[test]
    fn the_query_editor_owns_the_keyboard_while_it_has_focus() {
        let mut a = app_with_many_rows();
        a.set_page(Page::Search);
        assert!(a.search.editing, "an unsearched page opens in the editor");
        // `q` quits the dashboard everywhere else; here it is a letter.
        for ch in "deadlock".chars() {
            press(&mut a, KeyCode::Char(ch));
        }
        assert_eq!(a.search.query, "deadlock");
        assert!(!a.should_quit);

        press(&mut a, KeyCode::Enter);
        assert!(a.search_requested.is_some(), "Enter runs the search");
        assert!(!a.search.editing, "and gives the keyboard back to the list");

        // Too short to be a search: say so rather than reading the corpus.
        a.search_requested = None;
        a.search.running = false;
        a.begin_search_edit();
        a.search.query = "d".into();
        press(&mut a, KeyCode::Enter);
        assert!(a.search_requested.is_none());
        assert!(a.search.error.is_some());
    }

    #[test]
    fn slash_opens_search_from_any_page() {
        let mut a = app_with_many_rows();
        a.set_page(Page::Models);
        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.page, Page::Search);
        assert!(a.search.editing);
    }

    /// Plain text of a rendered buffer, one line per row.
    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in buf.area.y..buf.area.bottom() {
            for x in buf.area.x..buf.area.right() {
                if let Some(c) = buf.cell((x, y)) {
                    out.push_str(c.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    /// An app with more rows than any list can show at once.
    fn app_with_many_rows() -> App {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        let now = chrono::Local::now().timestamp();
        a.events = (0..60)
            .map(|i| UsageEvent {
                source: Source::Claude,
                // One event per day, so the day list is 60 rows deep too.
                ts: now - i * 86_400,
                model: format!("model-{i:02}"),
                session: format!("sess-{i:02}"),
                project: format!("proj-{i:02}"),
                tokens: Tokens { input: 1_000 * (60 - i as u64), output: 10, ..Default::default() },
                observed_on: Vec::new(),
                dedup_key: None,
                dedup_rank: 0,
            })
            .collect();
        a.set_range(Range::All);
        a.recompute(false);
        a
    }

    fn render(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let area = ratatui::layout::Rect { x: 0, y: 0, width, height };
        let mut buf = ratatui::buffer::Buffer::empty(area);
        pages::draw(app, &mut buf, area);
        buf
    }

    #[test]
    fn the_selection_stays_on_screen_when_it_runs_past_the_window() {
        // Every page whose list the keyboard drives, end to end: render (which
        // is the only thing that knows how tall the list drew), press End, and
        // render again. A selection scrolled off-screen is the same as none.
        for (page, label) in [
            (Page::Models, "model-59"),
            (Page::Projects, "proj-59"),
            // A session row is identified by its project and model, never by
            // the raw id, so that is what the assertion looks for.
            (Page::Sessions, "proj-59  model-59"),
            (Page::Overview, "model-59"),
        ] {
            let mut a = app_with_many_rows();
            a.set_page(page);
            render(&mut a, 110, 24);
            press(&mut a, KeyCode::End);
            let buf = render(&mut a, 110, 24);

            let rows = a.list_rows.get();
            assert!(rows > 0, "{page:?}: the render must report a row count");
            assert_eq!(a.selected, a.row_count() - 1, "{page:?}: End selects the last row");
            assert!(
                a.selected >= a.scroll && a.selected < a.scroll + rows,
                "{page:?}: selection {} outside window {}..{}",
                a.selected,
                a.scroll,
                a.scroll + rows
            );
            assert!(
                buffer_text(&buf).contains(label),
                "{page:?}: the selected row was never drawn"
            );

            // Home walks it back to the top of the same list.
            press(&mut a, KeyCode::Home);
            render(&mut a, 110, 24);
            assert_eq!((a.selected, a.scroll), (0, 0), "{page:?}: Home returns to the first row");
        }
    }

    #[test]
    fn the_daily_list_scrolls_with_its_own_selection() {
        // The day list is ordered newest-first, so the last row is the oldest
        // day — and it is the row `End` must reveal.
        let mut a = app_with_many_rows();
        a.set_page(Page::Daily);
        render(&mut a, 110, 24);
        press(&mut a, KeyCode::End);
        let buf = render(&mut a, 110, 24);
        let rows = a.list_rows.get();
        assert!(rows > 0);
        assert!(a.selected >= a.scroll && a.selected < a.scroll + rows);
        let oldest = a.summary.daily.first().expect("a dense window has days").date;
        assert!(
            buffer_text(&buf).contains(&oldest.format("%b %-d").to_string()),
            "the oldest day must be on screen once the selection reaches it"
        );
    }

    #[test]
    fn a_frame_costs_far_less_than_the_frame_budget() {
        // The dashboard animates at `anim::TICK`, so a frame that took anywhere
        // near a tick to build would cap the frame rate no matter what the tick
        // is set to. Measured at ~250µs for the heaviest page on a big
        // terminal; the bound is loose enough for a shared CI runner and still
        // an order of magnitude under the budget.
        //
        // Timed only when optimized. An unoptimized frame is an order of
        // magnitude slower and says nothing about the shipped binary — a debug
        // number checked against a runtime budget just measures the profile.
        // The draws still run in debug, so the paths stay exercised.
        let mut a = app_with_many_rows();
        let area = ratatui::layout::Rect { x: 0, y: 0, width: 160, height: 48 };
        for page in Page::ORDER {
            a.set_page(page);
            let mut warm = ratatui::buffer::Buffer::empty(area);
            pages::draw(&mut a, &mut warm, area);

            let started = std::time::Instant::now();
            const FRAMES: u32 = 50;
            for _ in 0..FRAMES {
                let mut buf = ratatui::buffer::Buffer::empty(area);
                pages::draw(&mut a, &mut buf, area);
            }
            let per_frame = started.elapsed() / FRAMES;
            if cfg!(debug_assertions) {
                continue;
            }
            assert!(
                per_frame < Duration::from_millis(5),
                "{page:?} takes {per_frame:?} to draw, against a {:?} frame",
                anim::TICK
            );
        }
    }

    #[test]
    fn a_max_size_replay_stays_within_the_frame_budget() {
        let mut a = app_with_data();
        a.page = Page::Replay;
        a.replay.request = Some(ReplayRequest {
            source: Source::Codex,
            session: "dense-session".into(),
            project: "dense-project".into(),
            model: "dense-model".into(),
        });
        a.replay.data = Some(SessionReplay {
            events: (0..20_000)
                .map(|index| ReplayEvent {
                    ts_ms: 1_787_000_000_000 + index as i64,
                    offset_ms: index as u64,
                    kind: match index % 4 {
                        0 => ReplayKind::Assistant,
                        1 => ReplayKind::ToolCall,
                        2 => ReplayKind::ToolResult,
                        _ => ReplayKind::User,
                    },
                    title: "event".into(),
                    detail: "compact detail".into(),
                })
                .collect(),
            first_ts_ms: 1_787_000_000_000,
            last_ts_ms: 1_787_000_019_999,
            truncated: false,
        });
        a.selected = 10_000;
        a.scroll = 10_000;
        a.grow.snap_to(1.0);

        let area = ratatui::layout::Rect { x: 0, y: 0, width: 160, height: 48 };
        let mut warm = ratatui::buffer::Buffer::empty(area);
        pages::draw(&mut a, &mut warm, area);
        assert!(
            a.hits.len() <= usize::from(area.width) + 64,
            "时间线的点击区域必须受屏幕宽度限制，实际为 {}",
            a.hits.len()
        );

        let started = std::time::Instant::now();
        const FRAMES: u32 = 50;
        for _ in 0..FRAMES {
            let mut buf = ratatui::buffer::Buffer::empty(area);
            pages::draw(&mut a, &mut buf, area);
        }
        let per_frame = started.elapsed() / FRAMES;
        if !cfg!(debug_assertions) {
            assert!(
                per_frame < Duration::from_millis(5),
                "20,000-event Replay takes {per_frame:?} to draw, against a {:?} frame",
                anim::TICK
            );
        }
    }

    #[test]
    fn every_page_the_arrows_work_on_shows_where_the_selection_is() {
        // The bug this pins: Overview drew its model list inert while still
        // owning the arrow keys, so ↑↓ moved a selection with nothing on
        // screen to show for it and the keys read as broken. A page that
        // counts rows must also mark the row it is on.
        for page in Page::ORDER {
            let mut a = app_with_many_rows();
            a.set_page(page);
            if a.row_count() == 0 {
                continue;
            }
            render(&mut a, 110, 24);
            press(&mut a, KeyCode::Down);
            press(&mut a, KeyCode::Down);
            let buf = render(&mut a, 110, 24);
            assert!(
                buffer_text(&buf).contains(crate::tui::theme::SELECT_MARK),
                "{page:?}: the arrows move a selection that is never drawn"
            );
        }
    }

    #[test]
    fn the_rate_table_scrolls_to_its_last_row() {
        // Pricing counts rows like every other page, so ↑↓ and the wheel move a
        // selection there — the table has to be the list that answers.
        let mut a = app_with_many_rows();
        a.set_page(Page::Pricing);
        render(&mut a, 110, 24);
        press(&mut a, KeyCode::End);
        let buf = render(&mut a, 110, 24);
        let rows = a.list_rows.get();
        assert!(rows > 0, "the rate table must report a row count");
        assert!(a.scroll > 0, "the last rate is past the window, so it must have scrolled");
        assert!(a.selected >= a.scroll && a.selected < a.scroll + rows);
        let last = a.pricing.known_models().last().expect("built-in rates exist").0.clone();
        assert!(buffer_text(&buf).contains(&last), "the last rate was never drawn");
    }

    #[test]
    fn the_dashboard_renders_with_no_data_at_all() {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.recompute(false);
        let area = ratatui::layout::Rect { x: 0, y: 0, width: 100, height: 30 };
        let mut buf = ratatui::buffer::Buffer::empty(area);
        for page in Page::ORDER {
            a.set_page(page);
            pages::draw(&mut a, &mut buf, area);
        }
    }

    #[test]
    fn rendering_registers_clickable_regions() {
        let mut a = app_with_data();
        let area = ratatui::layout::Rect { x: 0, y: 0, width: 120, height: 40 };
        let mut buf = ratatui::buffer::Buffer::empty(area);
        pages::draw(&mut a, &mut buf, area);
        assert!(a.hits.len() > 5, "the sidebar, chips and rows must all be clickable");
        // Every sidebar entry resolves to its page.
        assert!(matches!(a.hits.hit(3, 3), Some(Action::Page(_))));
    }
}

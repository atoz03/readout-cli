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
use crate::model::Source;
use crate::pricing::Pricing;
use crate::scan::{self, Progress};
use anyhow::Result;
use app::{App, Drill, Loading, Page, Range};
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
    Done(Box<scan::ScanResult>),
    Failed(String),
}

pub fn run(sources: Vec<Source>, base: Filter, use_cache: bool) -> Result<()> {
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
    let mut app = App::new(sources.clone(), base, pricing);

    let mut terminal = setup()?;
    // Whatever happens below, the terminal must come back. Restoring before
    // propagating means a panic or error never leaves the user in raw mode
    // with a hidden cursor.
    let result = event_loop(&mut terminal, &mut app, sources, use_cache);
    restore(&mut terminal)?;
    result
}

/// Render one settled frame to a string of ANSI escapes, without touching the
/// terminal state.
///
/// This exists so the layout can be inspected at an exact size — in a test, in
/// a bug report, or piped to a file — rather than only by eye at whatever size
/// the window happens to be.
pub fn snapshot(
    sources: Vec<Source>,
    base: Filter,
    use_cache: bool,
    width: u16,
    height: u16,
    page: Page,
) -> Result<String> {
    let pricing = Pricing::load(crate::paths::pricing_override_file().ok().as_deref())?;
    let mut app = App::new(sources.clone(), base, pricing);
    let result = scan::scan_with_cache(&sources, use_cache, None)?;
    app.events = result.events;
    app.stats = result.stats;
    app.loading = Loading::Done;
    app.set_page(page);
    // Snapshots show the settled state; animating into a still image would
    // only ever capture a half-drawn frame.
    app.recompute(false);

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

fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn spawn_scan(sources: Vec<Source>, use_cache: bool) -> Receiver<ScanMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let tx2: Sender<ScanMsg> = tx.clone();
        let report = move |p: Progress| {
            // A closed channel means the UI is gone; dropping the send is the
            // correct response, not an error.
            let _ = tx2.send(ScanMsg::Progress(p));
        };
        match scan::scan_with_cache(&sources, use_cache, Some(&report)) {
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

fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    sources: Vec<Source>,
    use_cache: bool,
) -> Result<()> {
    let mut rx = spawn_scan(sources.clone(), use_cache);

    loop {
        if app.needs_redraw {
            terminal.draw(|f| {
                let area = f.area();
                pages::draw(app, f.buffer_mut(), area);
            })?;
            app.needs_redraw = false;
        }

        drain_scan(app, &rx);

        if app.rescan_requested {
            app.rescan_requested = false;
            app.loading = Loading::Scanning(None);
            app.status = Some("rescanning…".into());
            rx = spawn_scan(sources.clone(), use_cache);
            app.needs_redraw = true;
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

fn drain_scan(app: &mut App, rx: &Receiver<ScanMsg>) {
    loop {
        match rx.try_recv() {
            Ok(ScanMsg::Progress(p)) => {
                app.loading = Loading::Scanning(Some(p));
                app.needs_redraw = true;
            }
            Ok(ScanMsg::Done(result)) => {
                app.events = result.events;
                app.stats = result.stats;
                app.loading = Loading::Done;
                app.status = None;
                app.recompute(true);
            }
            Ok(ScanMsg::Failed(e)) => {
                app.loading = Loading::Failed(e);
                app.needs_redraw = true;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
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
            _ => {}
        }
        return;
    }

    match k.code {
        KeyCode::Char('q') | KeyCode::Esc if app.drill == Drill::None => {
            if k.code == KeyCode::Char('q') {
                app.should_quit = true;
            }
        }
        KeyCode::Esc => app.set_drill(Drill::None),
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('r') => app.rescan_requested = true,
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
            app.status =
                Some("click the sidebar, chips, and rows · wheel scrolls · enter drills in".into())
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
            }
        }
        Action::ClearFilter => app.set_drill(Drill::None),
        Action::Refresh => app.rescan_requested = true,
        Action::Quit => app.should_quit = true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tokens;
    use crate::model::UsageEvent;

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
        assert!(a.rescan_requested);
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
            assert!(
                per_frame < Duration::from_millis(5),
                "{page:?} takes {per_frame:?} to draw, against a {:?} frame",
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

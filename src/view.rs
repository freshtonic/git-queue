//! The ratatui view for `git queue tui` (ADR-0001).
//!
//! This is the deliberately-thin, essentially-untested shell in front of the
//! headless [`Engine`]: it owns terminal setup/teardown, the event loop, and
//! the three-pane layout, but no domain logic — every mutation goes through an
//! [`Operation`](crate::engine::Operation) on the engine. In this ticket the
//! view is **read-only**: it renders the queue, the selected commit's message
//! and its own diff, and lets the user navigate; it performs no mutations.
//!
//! Rendering decisions that are pure (diff-line classification, queue-row
//! ordering) live in [`render`](crate::render) and [`engine`](crate::engine)
//! so they can be unit-tested; the code here is the ratatui wiring around them.

use crate::engine::{Engine, Row};
use crate::git;
use crate::render::{classify_diff_line, DiffLine};
use anyhow::Result;
use ratatui::crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io;

/// Which pane has keyboard focus — scrolling and (later) pane-specific actions
/// target it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Queue,
    Message,
    Diff,
}

/// The view's mutable state over one engine session.
struct App {
    engine: Engine,
    /// Index of the selected commit within `engine.line().commits`.
    selected: usize,
    focus: Pane,
    msg_scroll: u16,
    diff_scroll: u16,
    show_help: bool,
    /// The selected commit's cached message and diff (recomputed on move).
    message: String,
    diff: String,
    /// Scroll offset of the queue list, kept so mouse clicks hit-test correctly.
    list_state: ListState,
    /// Pane rectangles from the last frame, for mouse hit-testing.
    queue_area: Rect,
    message_area: Rect,
    diff_area: Rect,
    /// Whether the user asked to quit.
    quit: bool,
}

impl App {
    fn new(engine: Engine) -> Result<App> {
        let mut app = App {
            engine,
            selected: 0,
            focus: Pane::Queue,
            msg_scroll: 0,
            diff_scroll: 0,
            show_help: false,
            message: String::new(),
            diff: String::new(),
            list_state: ListState::default(),
            queue_area: Rect::default(),
            message_area: Rect::default(),
            diff_area: Rect::default(),
            quit: false,
        };
        app.refresh_selection()?;
        Ok(app)
    }

    /// Recompute the message and diff for the currently selected commit, and
    /// reset the message/diff scroll.
    fn refresh_selection(&mut self) -> Result<()> {
        let sha = self.engine.line().commits[self.selected].sha.clone();
        self.message = git::commit_message(&sha)?;
        self.diff = git::commit_diff(&sha)?;
        self.msg_scroll = 0;
        self.diff_scroll = 0;
        Ok(())
    }

    fn commit_count(&self) -> usize {
        self.engine.line().commits.len()
    }

    fn select(&mut self, index: usize) -> Result<()> {
        let clamped = index.min(self.commit_count().saturating_sub(1));
        if clamped != self.selected {
            self.selected = clamped;
            self.refresh_selection()?;
        }
        Ok(())
    }

    /// The queue-pane display rows (branch headers interleaved with commits),
    /// and the display-row index of the selected commit.
    fn rows_and_selected_row(&self) -> (Vec<Row>, usize) {
        let rows = self.engine.line().rows();
        let selected_row = rows
            .iter()
            .position(|r| matches!(r, Row::Commit { index } if *index == self.selected))
            .unwrap_or(0);
        (rows, selected_row)
    }
}

/// Run the interactive editor over a loaded engine. Sets up the terminal,
/// drives the event loop, and restores the terminal on the way out (even on
/// panic, via the guard and the chained panic hook).
pub fn run(engine: Engine) -> Result<()> {
    let mut app = App::new(engine)?;
    let mut term = TerminalGuard::new()?;
    let res = event_loop(&mut app, &mut term.terminal);
    // The guard's Drop restores the terminal; surface the loop's error after.
    drop(term);
    res
}

type Backend = ratatui::backend::CrosstermBackend<io::Stdout>;

/// Owns the terminal's raw-mode / alt-screen / mouse-capture state and restores
/// it on drop, so a `?` or panic in the loop never leaves the user's terminal
/// wedged.
struct TerminalGuard {
    terminal: Terminal<Backend>,
}

impl TerminalGuard {
    fn new() -> Result<TerminalGuard> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = Backend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;

        // Chain a panic hook so a panic mid-render still restores the terminal
        // before the message prints.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            default_hook(info);
        }));

        Ok(TerminalGuard { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
        let _ = self.terminal.show_cursor();
    }
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

fn event_loop(app: &mut App, terminal: &mut Terminal<Backend>) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key)?,
            Event::Mouse(m) => handle_mouse(app, m)?,
            _ => {}
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    // The help overlay swallows input: any key dismisses it.
    if app.show_help {
        app.show_help = false;
        return Ok(());
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('j') | KeyCode::Down => app.select(app.selected + 1)?,
        KeyCode::Char('k') | KeyCode::Up => app.select(app.selected.saturating_sub(1))?,
        KeyCode::Char('g') => app.select(0)?,
        KeyCode::Char('G') => app.select(app.commit_count().saturating_sub(1))?,
        KeyCode::Tab => app.focus = next_pane(app.focus),
        KeyCode::Char('d') if ctrl => scroll_focused(app, 10),
        KeyCode::Char('u') if ctrl => scroll_focused(app, -10),
        KeyCode::PageDown => scroll_focused(app, 10),
        KeyCode::PageUp => scroll_focused(app, -10),
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut App, m: event::MouseEvent) -> Result<()> {
    if app.show_help {
        return Ok(());
    }
    let at = Rect::new(m.column, m.row, 1, 1);
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if intersects(app.queue_area, m.column, m.row) {
                app.focus = Pane::Queue;
                click_select_commit(app, m.row);
                app.refresh_selection()?;
            } else if intersects(app.message_area, m.column, m.row) {
                app.focus = Pane::Message;
            } else if intersects(app.diff_area, m.column, m.row) {
                app.focus = Pane::Diff;
            }
            let _ = at;
        }
        MouseEventKind::ScrollDown => scroll_pane_at(app, m.column, m.row, 3),
        MouseEventKind::ScrollUp => scroll_pane_at(app, m.column, m.row, -3),
        _ => {}
    }
    Ok(())
}

/// Select the commit whose queue-list row was clicked. Header clicks and
/// out-of-range clicks are ignored.
fn click_select_commit(app: &mut App, row: u16) {
    // Row within the list's inner (bordered) area, plus the scroll offset.
    let inner_top = app.queue_area.y + 1;
    if row < inner_top {
        return;
    }
    let visible_row = (row - inner_top) as usize;
    let display_index = app.list_state.offset() + visible_row;
    let (rows, _) = app.rows_and_selected_row();
    if let Some(Row::Commit { index }) = rows.get(display_index) {
        app.selected = *index;
    }
}

fn scroll_pane_at(app: &mut App, col: u16, row: u16, delta: i32) {
    if intersects(app.message_area, col, row) {
        app.msg_scroll = apply_scroll(app.msg_scroll, delta);
    } else if intersects(app.diff_area, col, row) {
        app.diff_scroll = apply_scroll(app.diff_scroll, delta);
    }
}

fn scroll_focused(app: &mut App, delta: i32) {
    match app.focus {
        Pane::Message => app.msg_scroll = apply_scroll(app.msg_scroll, delta),
        Pane::Diff => app.diff_scroll = apply_scroll(app.diff_scroll, delta),
        Pane::Queue => {} // selection, not scroll, drives the queue pane
    }
}

fn apply_scroll(current: u16, delta: i32) -> u16 {
    (current as i32 + delta).max(0) as u16
}

fn next_pane(p: Pane) -> Pane {
    match p {
        Pane::Queue => Pane::Message,
        Pane::Message => Pane::Diff,
        Pane::Diff => Pane::Queue,
    }
}

fn intersects(area: Rect, col: u16, row: u16) -> bool {
    col >= area.x && col < area.right() && row >= area.y && row < area.bottom()
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(f.area());
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);

    app.queue_area = chunks[0];
    app.message_area = right[0];
    app.diff_area = right[1];

    draw_queue(f, app, chunks[0]);
    draw_message(f, app, right[0]);
    draw_diff(f, app, right[1]);

    if app.show_help {
        draw_help(f, f.area());
    }
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

fn draw_queue(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let line = app.engine.line();
    let (rows, selected_row) = app.rows_and_selected_row();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| match r {
            Row::Branch { boundary } => ListItem::new(Line::from(Span::styled(
                format!("[{}]", line.boundaries[*boundary].name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))),
            Row::Commit { index } => {
                let c = &line.commits[*index];
                let id = match &c.id {
                    Some(id) => id.chars().take(10).collect::<String>(),
                    None => "(no id)".to_string(),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {id:<10} "), Style::default().fg(Color::Blue)),
                    Span::raw(c.subject.clone()),
                ]))
            }
        })
        .collect();

    app.list_state.select(Some(selected_row));
    let list = List::new(items)
        .block(pane_block("queue", app.focus == Pane::Queue))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_message(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let p = Paragraph::new(app.message.as_str())
        .block(pane_block("message", app.focus == Pane::Message))
        .scroll((app.msg_scroll, 0));
    f.render_widget(p, area);
}

fn draw_diff(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .diff
        .lines()
        .map(|l| {
            let style = match classify_diff_line(l) {
                DiffLine::Added => Style::default().fg(Color::Green),
                DiffLine::Removed => Style::default().fg(Color::Red),
                DiffLine::Hunk => Style::default().fg(Color::Cyan),
                DiffLine::FileHeader => Style::default().add_modifier(Modifier::BOLD),
                DiffLine::Binary => Style::default().fg(Color::Magenta),
                DiffLine::Context => Style::default(),
            };
            Line::from(Span::styled(l.to_string(), style))
        })
        .collect();
    let p = Paragraph::new(lines)
        .block(pane_block("diff", app.focus == Pane::Diff))
        .scroll((app.diff_scroll, 0));
    f.render_widget(p, area);
}

fn draw_help(f: &mut ratatui::Frame, area: Rect) {
    let bindings = [
        ("j / ↓", "next commit"),
        ("k / ↑", "previous commit"),
        ("g / G", "first / last commit"),
        ("Tab", "cycle pane focus"),
        ("Ctrl-d / Ctrl-u", "scroll focused pane"),
        ("PgDn / PgUp", "scroll focused pane"),
        ("mouse click", "select commit / focus pane"),
        ("mouse wheel", "scroll pane under cursor"),
        ("?", "toggle this help"),
        ("q / Esc", "quit"),
    ];
    let mut lines = vec![Line::from(Span::styled(
        "git queue tui — keybindings",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::from(""));
    for (k, desc) in bindings {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<16}"), Style::default().fg(Color::Cyan)),
            Span::raw(desc),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  (press any key to dismiss)",
        Style::default().fg(Color::DarkGray),
    )));

    let popup = centered_rect(60, 60, area);
    f.render_widget(Clear, popup);
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" help "),
    );
    f.render_widget(p, popup);
}

/// A rectangle `percent_x` × `percent_y` of `area`, centred.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;

    // The engine reads process cwd; serialise the tests that swap it.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A repo on an untracked `feature` branch with two commits — loads as one
    /// provisional section, so no queue metadata is needed.
    fn repo_with_feature_branch() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        for (name, body) in [("seed.txt", "seed\n"), ("first.txt", "hello\n")] {
            std::fs::write(dir.join(name), body).unwrap();
            git(dir, &["add", name]);
            git(dir, &["commit", "-q", "-m", &format!("add {name}")]);
        }
        git(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("feature.txt"), "the feature\n").unwrap();
        git(dir, &["add", "feature.txt"]);
        git(dir, &["commit", "-q", "-m", "add feature work"]);
        std::fs::write(dir.join("more.txt"), "more work\n").unwrap();
        git(dir, &["add", "more.txt"]);
        git(dir, &["commit", "-q", "-m", "more feature work"]);
        tmp
    }

    fn app_in(dir: &Path) -> App {
        let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_current_dir(dir).unwrap();
        App::new(Engine::load().expect("engine loads")).expect("app builds")
    }

    fn buffer_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_the_three_panes_with_queue_message_and_diff() {
        let tmp = repo_with_feature_branch();
        let mut app = app_in(tmp.path());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend());

        assert!(text.contains("queue"), "queue pane titled");
        assert!(text.contains("message"), "message pane titled");
        assert!(text.contains("diff"), "diff pane titled");
        assert!(text.contains("feature"), "branch header shown: {text:?}");
        assert!(
            text.contains("add feature work"),
            "commit subject shown in queue/message"
        );
        assert!(text.contains("the feature"), "diff content shown");
    }

    #[test]
    fn q_quits_and_help_toggles() {
        let tmp = repo_with_feature_branch();
        let mut app = app_in(tmp.path());

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)).unwrap();
        assert!(app.show_help);
        // Any key dismisses help rather than acting.
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)).unwrap();
        assert!(!app.show_help);
        assert!(!app.quit, "the key that dismisses help does not also quit");

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)).unwrap();
        assert!(app.quit);
    }

    #[test]
    fn j_and_k_move_the_selection() {
        let tmp = repo_with_feature_branch();
        let mut app = app_in(tmp.path());
        assert_eq!(app.selected, 0);
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)).unwrap();
        assert_eq!(app.selected, 1);
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)).unwrap();
        assert_eq!(app.selected, 0);
        // Can't move above the front.
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)).unwrap();
        assert_eq!(app.selected, 0);
    }
}

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

use crate::engine::{Applied, Engine, Operation, Row};
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

/// A pending single-line text prompt in the footer (branch name entry).
#[derive(Debug, Clone)]
enum Prompt {
    /// Rename the branch at this boundary to the entered name.
    Rename { boundary: usize },
    /// Add a boundary after this commit, naming the new branch.
    AddBoundary { index: usize },
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self {
            Prompt::Rename { .. } => "rename branch to",
            Prompt::AddBoundary { .. } => "new branch name",
        }
    }
}

struct Input {
    prompt: Prompt,
    buffer: String,
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
    /// An active footer text prompt (rename / add-boundary), if any.
    input: Option<Input>,
    /// Showing the "quit with pending operations?" guard.
    confirm_quit: bool,
    /// Showing the "conflict — resolve or undo?" prompt.
    conflict_prompt: bool,
    /// Set when the user chose to resolve a conflict in the shell: quit without
    /// the normal exit summary and print resolution instructions instead.
    suspend: bool,
    /// Transient status/error line shown in the footer.
    status: String,
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
            input: None,
            confirm_quit: false,
            conflict_prompt: false,
            suspend: false,
            status: String::new(),
            quit: false,
        };
        app.refresh_selection()?;
        Ok(app)
    }

    /// The boundary (branch) index owning the selected commit.
    fn selected_boundary(&self) -> usize {
        self.engine.line().boundary_of(self.selected)
    }

    /// Apply an operation, folding any error into the status line, raising the
    /// conflict prompt on a conflict, and keeping the selection valid.
    fn apply_op(&mut self, op: Operation) {
        match self.engine.apply(op) {
            Ok(Applied::Done) => self.after_change(),
            Ok(Applied::Conflict) => self.conflict_prompt = true,
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// Reorder the selected commit to `to`, following it with the selection.
    fn reorder_to(&mut self, to: usize) {
        let from = self.selected;
        match self.engine.apply(Operation::Reorder { from, to }) {
            Ok(Applied::Done) => {
                self.selected = to;
                self.after_change();
            }
            Ok(Applied::Conflict) => self.conflict_prompt = true,
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// Post-change housekeeping: clear the status, clamp the selection, and
    /// refresh the message/diff for the (possibly rewritten) selected commit.
    fn after_change(&mut self) {
        self.status.clear();
        let last = self.commit_count().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
        }
        if let Err(e) = self.refresh_selection() {
            self.status = format!("{e:#}");
        }
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
    // The guard's Drop restores the terminal; print only once the alternate
    // screen is gone.
    drop(term);
    res?;
    if app.suspend {
        print_suspend_instructions();
    } else {
        print_exit_summary(&app.engine);
    }
    Ok(())
}

/// Printed when the user chose to resolve a conflict in the shell: the repo is
/// left in the standard mid-rebase state, and re-running `git queue tui`
/// resumes once the rebase is finished.
fn print_suspend_instructions() {
    println!("The operation produced conflicts; a rebase is in progress.");
    println!("Resolve it with your normal git workflow, then re-open the editor:");
    println!("  git status                 # see the conflicted files");
    println!("  # edit the files to resolve, then:");
    println!("  git add <files>");
    println!("  git rebase --continue      # repeat until the rebase finishes");
    println!("  git queue tui              # resume editing the queue");
}

/// After a session that changed anything, print the new branch layout, the
/// `sync` reminder, and warnings for dissolved branches that still have a PR.
/// A pure read-only session prints nothing.
fn print_exit_summary(engine: &Engine) {
    if !engine.changed() {
        return;
    }
    let layout = engine.layout();
    println!(
        "Queue `{}` now has {} branch(es):",
        engine.queue_name(),
        layout.len()
    );
    for (parent, branch) in &layout {
        println!("  {parent} ← {branch}");
    }
    println!(
        "Now on `{}`. Run `git queue sync` to update the PRs.",
        engine.landing_branch()
    );
    for (branch, pr) in engine.dissolved_with_prs() {
        println!(
            "note: dissolved `{branch}` still has an open PR #{pr} — close it, or \
             repurpose it manually."
        );
    }
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
    // Modal states consume input first, in priority order.
    if app.show_help {
        app.show_help = false; // any key dismisses help
        return Ok(());
    }
    if app.conflict_prompt {
        return handle_conflict_key(app, key);
    }
    if app.input.is_some() {
        return handle_input_key(app, key);
    }
    if app.confirm_quit {
        return handle_quit_confirm_key(app, key);
    }
    handle_normal_key(app, key)
}

fn handle_normal_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Quitting with operations on the stack asks first.
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.engine.has_pending() {
                app.confirm_quit = true;
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('j') | KeyCode::Down => app.select(app.selected + 1)?,
        KeyCode::Char('k') | KeyCode::Up => app.select(app.selected.saturating_sub(1))?,
        KeyCode::Char('g') => app.select(0)?,
        KeyCode::Char('G') => app.select(app.commit_count().saturating_sub(1))?,
        KeyCode::Tab => app.focus = next_pane(app.focus),
        // Ctrl-modified bindings must precede their bare-key namesakes so the
        // guard wins (Ctrl-d/u scroll; Ctrl-r redoes; bare r/u are ops).
        KeyCode::Char('d') if ctrl => scroll_focused(app, 10),
        KeyCode::Char('u') if ctrl => scroll_focused(app, -10),
        KeyCode::Char('r') if ctrl => app.apply_op(Operation::Redo),
        KeyCode::PageDown => scroll_focused(app, 10),
        KeyCode::PageUp => scroll_focused(app, -10),

        // ---- boundary operations (ref-only) ----
        KeyCode::Char('r') => {
            app.input = Some(Input {
                prompt: Prompt::Rename {
                    boundary: app.selected_boundary(),
                },
                buffer: String::new(),
            });
        }
        KeyCode::Char('a') => {
            app.input = Some(Input {
                prompt: Prompt::AddBoundary {
                    index: app.selected,
                },
                buffer: String::new(),
            });
        }
        KeyCode::Char('x') => app.apply_op(Operation::RemoveBoundary {
            boundary: app.selected_boundary(),
        }),
        KeyCode::Char('>') => app.apply_op(Operation::MoveBoundary {
            boundary: app.selected_boundary(),
            delta: 1,
        }),
        KeyCode::Char('<') => app.apply_op(Operation::MoveBoundary {
            boundary: app.selected_boundary(),
            delta: -1,
        }),

        // ---- reorder (history-rewriting) ----
        KeyCode::Char('J') => {
            if app.selected + 1 < app.commit_count() {
                app.reorder_to(app.selected + 1);
            }
        }
        KeyCode::Char('K') => {
            if app.selected > 0 {
                app.reorder_to(app.selected - 1);
            }
        }

        // ---- undo ----
        KeyCode::Char('u') => app.apply_op(Operation::Undo),
        _ => {}
    }
    Ok(())
}

fn handle_input_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.input = None,
        KeyCode::Enter => {
            if let Some(input) = app.input.take() {
                let name = input.buffer.trim().to_string();
                if name.is_empty() {
                    app.status = "name cannot be empty".into();
                } else {
                    let op = match input.prompt {
                        Prompt::Rename { boundary } => Operation::RenameBranch { boundary, name },
                        Prompt::AddBoundary { index } => Operation::AddBoundary { index, name },
                    };
                    app.apply_op(op);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(input) = app.input.as_mut() {
                input.buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(input) = app.input.as_mut() {
                input.buffer.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_quit_confirm_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.quit = true,
        _ => app.confirm_quit = false,
    }
    Ok(())
}

/// The resolve-or-undo choice after a conflicting operation.
fn handle_conflict_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        // Resolve: suspend to the shell in the mid-rebase state.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.conflict_prompt = false;
            app.suspend = true;
            app.quit = true;
        }
        // Undo: back the operation out cleanly.
        KeyCode::Char('u') | KeyCode::Char('U') | KeyCode::Esc => {
            app.conflict_prompt = false;
            match app.engine.undo_conflict() {
                Ok(()) => app.after_change(),
                Err(e) => app.status = format!("{e:#}"),
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut App, m: event::MouseEvent) -> Result<()> {
    // While a modal (help / prompt / quit-guard / conflict) is up, ignore the
    // mouse.
    if app.show_help || app.input.is_some() || app.confirm_quit || app.conflict_prompt {
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
    // Reserve a one-row footer for prompts / status / hints.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());
    let body = outer[0];

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(body);
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
    draw_footer(f, app, outer[1]);

    if app.show_help {
        draw_help(f, body);
    }
}

/// The footer line: an active prompt, the quit-guard, a status/error message,
/// or the default key hint — in that priority.
fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (text, style) = if app.conflict_prompt {
        (
            "conflict — [R]esolve in the shell, or [U]ndo the operation?".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(input) = &app.input {
        (
            format!("{}: {}\u{2588}", input.prompt.label(), input.buffer),
            Style::default().fg(Color::Cyan),
        )
    } else if app.confirm_quit {
        (
            "quit with unsaved operations? [y/N]".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if !app.status.is_empty() {
        (app.status.clone(), Style::default().fg(Color::Red))
    } else {
        (
            "j/k move  J/K reorder  r rename  a add-branch  x dissolve  </> shift  \
             u undo  C-r redo  ? help  q quit"
                .to_string(),
            Style::default().fg(Color::DarkGray),
        )
    };
    f.render_widget(Paragraph::new(Span::styled(text, style)), area);
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
        ("J / K", "reorder the selected commit down / up"),
        ("r", "rename the selected commit's branch"),
        ("a", "add a boundary after the selected commit"),
        ("x", "dissolve the selected commit's branch"),
        ("< / >", "shift the branch boundary by a commit"),
        ("u", "undo"),
        ("Ctrl-r", "redo"),
        ("mouse click", "select commit / focus pane"),
        ("mouse wheel", "scroll pane under cursor"),
        ("?", "toggle this help"),
        ("q / Esc", "quit (asks if operations are pending)"),
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

    /// A repo whose untracked `feature` branch has three commits, the last two
    /// of which edit the same line — so reordering across them conflicts.
    fn repo_with_conflict() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(dir, &["add", "seed.txt"]);
        git(dir, &["commit", "-q", "-m", "seed"]);
        git(dir, &["checkout", "-q", "-b", "feature"]);
        for (content, msg) in [
            ("L1\nL2\nL3\n", "base lines"),
            ("L1\nA\nL3\n", "edit to A"),
            ("L1\nB\nL3\n", "edit to B"),
        ] {
            std::fs::write(dir.join("f.txt"), content).unwrap();
            git(dir, &["add", "f.txt"]);
            git(dir, &["commit", "-q", "-m", msg]);
        }
        tmp
    }

    /// Build an app over a fresh feature-branch repo and run `f` with the cwd
    /// lock held for the whole call — `App`'s git reads and every op mutate the
    /// process cwd, so the lock must span them all.
    fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> T {
        with_app_over(repo_with_feature_branch(), f)
    }

    fn with_app_over<T>(tmp: tempfile::TempDir, f: impl FnOnce(&mut App) -> T) -> T {
        let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_current_dir(tmp.path()).unwrap();
        let mut app = App::new(Engine::load().expect("engine loads")).expect("app builds");
        f(&mut app)
    }

    fn press(app: &mut App, c: char) {
        handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
    }

    fn key(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, c);
        }
    }

    fn buffer_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn boundary_names(app: &App) -> Vec<String> {
        app.engine
            .line()
            .boundaries
            .iter()
            .map(|b| b.name.clone())
            .collect()
    }

    #[test]
    fn renders_the_three_panes_with_queue_message_and_diff() {
        with_app(|app| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
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
        });
    }

    #[test]
    fn q_quits_and_help_toggles() {
        with_app(|app| {
            press(app, '?');
            assert!(app.show_help);
            // Any key dismisses help rather than acting.
            press(app, 'q');
            assert!(!app.show_help);
            assert!(!app.quit, "the key that dismisses help does not also quit");

            press(app, 'q');
            assert!(app.quit);
        });
    }

    #[test]
    fn j_and_k_move_the_selection() {
        with_app(|app| {
            assert_eq!(app.selected, 0);
            press(app, 'j');
            assert_eq!(app.selected, 1);
            press(app, 'k');
            assert_eq!(app.selected, 0);
            // Can't move above the front.
            press(app, 'k');
            assert_eq!(app.selected, 0);
        });
    }

    #[test]
    fn add_boundary_via_prompt_splits_the_branch() {
        with_app(|app| {
            assert_eq!(boundary_names(app), vec!["feature"]);
            // Select the front commit, add a boundary after it.
            key(app, KeyCode::Char('g'));
            press(app, 'a');
            assert!(app.input.is_some(), "prompt opened");
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert!(app.input.is_none(), "prompt closed after apply");
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(boundary_names(app), vec!["api", "feature"]);
        });
    }

    #[test]
    fn quit_guard_prompts_once_operations_are_pending() {
        with_app(|app| {
            // Make a change so the undo stack is non-empty.
            key(app, KeyCode::Char('g'));
            press(app, 'a');
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert!(app.engine.has_pending());

            press(app, 'q');
            assert!(app.confirm_quit, "quitting with pending ops asks first");
            assert!(!app.quit);
            // Confirming quits.
            press(app, 'y');
            assert!(app.quit);
        });
    }

    #[test]
    fn undo_after_a_boundary_op_restores_the_single_branch() {
        with_app(|app| {
            key(app, KeyCode::Char('g'));
            press(app, 'a');
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert_eq!(boundary_names(app), vec!["api", "feature"]);

            press(app, 'u'); // undo
            assert_eq!(boundary_names(app), vec!["feature"]);
            assert!(!app.engine.has_pending(), "undo emptied the stack");
        });
    }

    #[test]
    fn a_failed_op_surfaces_in_the_status_line() {
        with_app(|app| {
            // Adding a boundary after the last commit of the only branch is
            // rejected (it would leave an empty branch).
            key(app, KeyCode::Char('G'));
            press(app, 'a');
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert!(!app.status.is_empty(), "error shown in status line");
            assert_eq!(boundary_names(app), vec!["feature"], "nothing changed");
        });
    }

    #[test]
    fn shift_j_reorders_and_the_selection_follows() {
        // feature.txt and more.txt touch different files, so this is clean.
        with_app(|app| {
            let subjects_before: Vec<String> = app
                .engine
                .line()
                .commits
                .iter()
                .map(|c| c.subject.clone())
                .collect();
            key(app, KeyCode::Char('g')); // select the front commit
            press(app, 'J'); // move it down one
            assert_eq!(app.selected, 1, "selection follows the moved commit");
            let subjects_after: Vec<String> = app
                .engine
                .line()
                .commits
                .iter()
                .map(|c| c.subject.clone())
                .collect();
            assert_ne!(subjects_before, subjects_after, "the order changed");
            assert_eq!(subjects_before[0], subjects_after[1], "front commit moved down");
        });
    }

    #[test]
    fn a_conflicting_reorder_raises_the_prompt_and_undo_backs_out() {
        with_app_over(repo_with_conflict(), |app| {
            // Move "edit to B" (last) up onto "edit to A" — a line-2 conflict.
            key(app, KeyCode::Char('G'));
            press(app, 'K');
            assert!(app.conflict_prompt, "the conflict prompt is raised");
            assert!(app.engine.conflicted());

            // Undo backs it out cleanly.
            press(app, 'u');
            assert!(!app.conflict_prompt);
            assert!(!app.engine.conflicted());
            assert!(app.status.is_empty(), "clean undo: {}", app.status);
        });
    }

    #[test]
    fn resolving_a_conflict_sets_suspend_and_quits() {
        with_app_over(repo_with_conflict(), |app| {
            key(app, KeyCode::Char('G'));
            press(app, 'K');
            assert!(app.conflict_prompt);

            press(app, 'r'); // resolve in the shell
            assert!(app.suspend, "suspends to the shell");
            assert!(app.quit);
            // Leave the mid-rebase state for the test's own cleanup.
            let _ = git_queue_git_abort();
        });
    }

    /// Abort any rebase left in progress by a test (best-effort cleanup).
    fn git_queue_git_abort() -> std::io::Result<std::process::ExitStatus> {
        Command::new("git").args(["rebase", "--abort"]).status()
    }
}

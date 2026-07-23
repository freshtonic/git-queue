//! The ratatui view for `git queue tui` (ADR-0001).
//!
//! This is the deliberately-thin, essentially-untested shell in front of the
//! headless [`Engine`]: it owns terminal setup/teardown, the event loop, the
//! three-pane layout, and the modal states (message editor, split selector,
//! conflict / dissolve / quit prompts), but no domain logic — every mutation
//! goes through an [`Operation`](crate::engine::Operation) on the engine, which
//! decides whether it completed or conflicted.
//!
//! Rendering decisions that are pure (diff-line classification, queue-row
//! ordering) live in [`render`](crate::render) and [`engine`](crate::engine)
//! so they can be unit-tested; the code here is the ratatui wiring around them.

use crate::engine::{Applied, Engine, Operation, Row, SplitKind, SplitLine};
use crate::git;
use crate::render::{classify_diff_line, DiffLine};
use std::collections::HashSet;
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

/// What an in-progress message edit will do when applied.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MsgTarget {
    /// Reword the selected commit.
    Reword,
    /// Squash the commit at `index` into its older neighbour, using the edited
    /// text as the combined description.
    Squash { index: usize },
    /// Split the commit at `index`, peeling `selected` into a new commit whose
    /// message is the edited text.
    Split {
        index: usize,
        selected: HashSet<usize>,
    },
}

/// The interactive split selector over a commit's diff.
struct SplitState {
    index: usize,
    lines: Vec<SplitLine>,
    /// `change_index`es peeled into the new (newer) commit.
    selected: HashSet<usize>,
    /// Cursor position within `lines` (always on a selectable line).
    cursor: usize,
    /// List scroll/selection state, kept so mouse clicks hit-test with the
    /// same offset the last frame rendered.
    list_state: ListState,
}

/// An in-progress message edit: a buffer plus what applying it will do.
#[derive(Debug, Clone)]
struct MsgEdit {
    buffer: String,
    target: MsgTarget,
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
    /// The editable message buffer and its target, when the message pane is
    /// being edited. Applies only on an explicit action (Ctrl-S).
    msg_edit: Option<MsgEdit>,
    /// Showing the "apply / discard your message edit?" prompt (raised when
    /// leaving the editor with an unapplied buffer).
    msg_apply_discard: bool,
    /// A branch (boundary index) left empty by a delete, offered for dissolve.
    dissolve_prompt: Option<usize>,
    /// The active split line-selector, if any.
    split: Option<SplitState>,
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
            msg_edit: None,
            msg_apply_discard: false,
            dissolve_prompt: None,
            split: None,
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

    /// Delete the selected commit; on success, offer to dissolve any branch it
    /// emptied.
    fn delete_selected(&mut self) {
        match self.engine.apply(Operation::Delete {
            index: self.selected,
        }) {
            Ok(Applied::Done) => {
                self.after_change();
                self.check_dissolve();
            }
            Ok(Applied::Conflict) => self.conflict_prompt = true,
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// Raise the dissolve prompt if a branch is now empty.
    fn check_dissolve(&mut self) {
        self.dissolve_prompt = self.engine.empty_branches().into_iter().next();
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

    /// Begin editing the selected commit's message in a local buffer.
    fn start_message_edit(&mut self) {
        self.msg_edit = Some(MsgEdit {
            buffer: self.message.clone(),
            target: MsgTarget::Reword,
        });
        self.focus = Pane::Message;
    }

    /// Whether a reword edit buffer differs from the committed message. A squash
    /// edit is never "dirty" — leaving it simply cancels the pending squash.
    fn message_dirty(&self) -> bool {
        self.msg_edit
            .as_ref()
            .is_some_and(|e| e.target == MsgTarget::Reword && e.buffer != self.message)
    }

    /// Apply the buffered message edit (reword or squash). Keeps a reword buffer
    /// on error so the user can retry; clears it on success.
    fn apply_message_edit(&mut self) {
        let Some(edit) = self.msg_edit.clone() else {
            return;
        };
        match edit.target {
            MsgTarget::Reword => match self.engine.apply(Operation::Reword {
                index: self.selected,
                message: edit.buffer,
            }) {
                Ok(Applied::Done) => {
                    self.msg_edit = None;
                    self.msg_apply_discard = false;
                    self.after_change();
                }
                // Reword can't conflict today, but stay honest if it ever does.
                Ok(Applied::Conflict) => {
                    self.msg_edit = None;
                    self.msg_apply_discard = false;
                    self.conflict_prompt = true;
                }
                // Drop the prompt so the error is visible; keep the buffer.
                Err(e) => {
                    self.msg_apply_discard = false;
                    self.status = format!("{e:#}");
                }
            },
            MsgTarget::Squash { index } => {
                self.msg_edit = None;
                self.msg_apply_discard = false;
                self.apply_squash(index, Some(edit.buffer));
            }
            MsgTarget::Split { index, selected } => {
                match self.engine.apply(Operation::Split {
                    index,
                    selected,
                    message: edit.buffer,
                }) {
                    Ok(Applied::Done) => {
                        self.msg_edit = None;
                        self.selected = index; // the older piece keeps this slot
                        self.after_change();
                    }
                    Ok(Applied::Conflict) => {
                        self.msg_edit = None;
                        self.conflict_prompt = true;
                    }
                    Err(e) => self.status = format!("{e:#}"), // keep the buffer to retry
                }
            }
        }
    }

    /// Enter the split line-selector for the selected commit.
    fn start_split(&mut self) {
        match self.engine.split_lines(self.selected) {
            Ok(lines) => {
                let Some(cursor) = lines.iter().position(|l| l.change_index.is_some()) else {
                    self.status = "this commit has no lines to split".into();
                    return;
                };
                self.split = Some(SplitState {
                    index: self.selected,
                    lines,
                    selected: HashSet::new(),
                    cursor,
                    list_state: ListState::default(),
                });
                self.focus = Pane::Diff;
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// Squash the selected commit into its older neighbour, prompting for a
    /// combined message first when both descriptions are non-empty.
    fn squash_selected(&mut self) {
        if self.selected == 0 {
            self.status = "the front commit has nothing older to squash into".into();
            return;
        }
        let index = self.selected;
        match self.engine.squash_needs_message(index) {
            Ok(true) => {
                let buffer = self.engine.squash_default_message(index).unwrap_or_default();
                self.msg_edit = Some(MsgEdit {
                    buffer,
                    target: MsgTarget::Squash { index },
                });
                self.focus = Pane::Message;
            }
            Ok(false) => self.apply_squash(index, None),
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn apply_squash(&mut self, index: usize, message: Option<String>) {
        match self.engine.apply(Operation::Squash { index, message }) {
            Ok(Applied::Done) => {
                self.selected = index - 1; // follow the combined commit
                self.after_change();
                self.check_dissolve();
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
    /// reset the message/diff scroll. Guards an empty line (which a rewrite
    /// that auto-dropped every commit could produce) rather than panicking.
    fn refresh_selection(&mut self) -> Result<()> {
        let commits = &self.engine.line().commits;
        let Some(commit) = commits.get(self.selected).or_else(|| commits.last()) else {
            self.message.clear();
            self.diff.clear();
            return Ok(());
        };
        let sha = commit.sha.clone();
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
    if app.split.is_some() {
        return handle_split_key(app, key);
    }
    if app.conflict_prompt {
        return handle_conflict_key(app, key);
    }
    if app.dissolve_prompt.is_some() {
        return handle_dissolve_key(app, key);
    }
    if app.msg_apply_discard {
        return handle_msg_prompt_key(app, key);
    }
    if app.msg_edit.is_some() {
        return handle_msg_edit_key(app, key);
    }
    if app.input.is_some() {
        return handle_input_key(app, key);
    }
    if app.confirm_quit {
        return handle_quit_confirm_key(app, key);
    }
    handle_normal_key(app, key)
}

/// Editing the message pane. The reword applies only on Ctrl-S; Esc leaves
/// (prompting apply/discard if the buffer is dirty).
fn handle_msg_edit_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('s') if ctrl => app.apply_message_edit(),
        KeyCode::Esc => {
            if app.message_dirty() {
                app.msg_apply_discard = true;
            } else {
                app.msg_edit = None; // clean reword, or cancel a squash
            }
        }
        KeyCode::Enter => {
            if let Some(e) = app.msg_edit.as_mut() {
                e.buffer.push('\n');
            }
        }
        KeyCode::Backspace => {
            if let Some(e) = app.msg_edit.as_mut() {
                e.buffer.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(e) = app.msg_edit.as_mut() {
                e.buffer.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

/// The apply/discard choice when leaving the message editor with unapplied
/// edits.
fn handle_msg_prompt_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('y') => app.apply_message_edit(),
        KeyCode::Char('d') | KeyCode::Char('n') => {
            app.msg_edit = None;
            app.msg_apply_discard = false;
        }
        KeyCode::Esc => app.msg_apply_discard = false, // cancel: back to editing
        _ => {}
    }
    Ok(())
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
        KeyCode::Char('e') => app.start_message_edit(),
        KeyCode::Char('s') => app.squash_selected(),
        KeyCode::Char('S') => app.start_split(),
        KeyCode::Char('D') => app.delete_selected(),
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

/// The interactive split selector: move the cursor over `+`/`-` lines, Space
/// toggles which piece a line goes into, Enter confirms (then prompts for the
/// peeled commit's message), Esc cancels.
fn handle_split_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    let Some(st) = app.split.as_mut() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => app.split = None,
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(n) = next_selectable(&st.lines, st.cursor, 1) {
                st.cursor = n;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(n) = next_selectable(&st.lines, st.cursor, -1) {
                st.cursor = n;
            }
        }
        KeyCode::Char(' ') => {
            if let Some(ci) = st.lines[st.cursor].change_index {
                if !st.selected.insert(ci) {
                    st.selected.remove(&ci);
                }
            }
        }
        KeyCode::Enter => {
            let total = st.lines.iter().filter(|l| l.change_index.is_some()).count();
            if st.selected.is_empty() || st.selected.len() >= total {
                app.status = "select some — but not all — lines to peel off".into();
            } else {
                let index = st.index;
                let selected = st.selected.clone();
                app.split = None;
                app.msg_edit = Some(MsgEdit {
                    buffer: String::new(),
                    target: MsgTarget::Split { index, selected },
                });
                app.focus = Pane::Message;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The next selectable line index from `from` in direction `dir` (+1/-1).
fn next_selectable(lines: &[SplitLine], from: usize, dir: isize) -> Option<usize> {
    let mut i = from as isize;
    loop {
        i += dir;
        if i < 0 || i as usize >= lines.len() {
            return None;
        }
        if lines[i as usize].change_index.is_some() {
            return Some(i as usize);
        }
    }
}

/// The dissolve choice for a branch a delete left empty.
fn handle_dissolve_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    let Some(boundary) = app.dissolve_prompt else {
        return Ok(());
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            app.dissolve_prompt = None;
            match app.engine.apply(Operation::RemoveBoundary { boundary }) {
                Ok(_) => {
                    app.after_change();
                    app.check_dissolve(); // a delete may have emptied more than one
                }
                Err(e) => app.status = format!("{e:#}"),
            }
        }
        _ => app.dissolve_prompt = None, // keep the empty branch
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
    // While a modal (help / prompt / quit-guard / conflict / message edit) is
    // up, ignore the mouse.
    // In the split selector, a left-click toggles the line under the cursor.
    if let Some(st) = app.split.as_mut() {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let inner_top = app.diff_area.y + 1;
            if m.row >= inner_top {
                // Add the list's scroll offset so clicks hit-test correctly
                // once the diff has scrolled.
                let index = st.list_state.offset() + (m.row - inner_top) as usize;
                if let Some(line) = st.lines.get(index) {
                    if let Some(ci) = line.change_index {
                        st.cursor = index;
                        if !st.selected.insert(ci) {
                            st.selected.remove(&ci);
                        }
                    }
                }
            }
        }
        return Ok(());
    }
    if app.show_help
        || app.input.is_some()
        || app.confirm_quit
        || app.conflict_prompt
        || app.msg_edit.is_some()
        || app.dissolve_prompt.is_some()
    {
        return Ok(());
    }
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
    // The diff pane flips to the interactive selector during a split.
    if app.split.is_some() {
        draw_split(f, app, right[1]);
    } else {
        draw_diff(f, app, right[1]);
    }
    draw_footer(f, app, outer[1]);

    if app.show_help {
        draw_help(f, body);
    }
}

/// The footer line: an active prompt, the quit-guard, a status/error message,
/// or the default key hint — in that priority.
fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (text, style) = if let Some(st) = &app.split {
        (
            format!(
                "split — Space toggle · j/k move · Enter confirm · Esc cancel  \
                 ({} line(s) peeled)",
                st.selected.len()
            ),
            Style::default().fg(Color::Cyan),
        )
    } else if app.conflict_prompt {
        (
            "conflict — [R]esolve in the shell, or [U]ndo the operation?".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(b) = app.dissolve_prompt {
        let name = app.engine.branch_name(b).to_string();
        let pr = app
            .engine
            .pr_of(b)
            .map(|n| format!(" (has open PR #{n})"))
            .unwrap_or_default();
        (
            format!("branch `{name}` is now empty{pr} — dissolve it? [y/N]"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.msg_apply_discard {
        (
            "unapplied message edit — [a]pply / [d]iscard / Esc cancel".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.msg_edit.is_some() {
        (
            "editing message — Ctrl-S apply · Esc leave".to_string(),
            Style::default().fg(Color::Cyan),
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
            "j/k move  J/K reorder  e message  s squash  S split  D delete  \
             r rename  a add-branch  x dissolve  </> shift  u undo  C-r redo  \
             ? help  q quit"
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
                let mut spans = vec![
                    Span::styled(format!("  {id:<10} "), Style::default().fg(Color::Blue)),
                    Span::raw(c.subject.clone()),
                ];
                if c.empty {
                    spans.push(Span::styled(
                        "  (empty)",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(Line::from(spans))
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
    let (title, body, scroll) = match &app.msg_edit {
        // A block cursor marks the edit point (buffer end).
        Some(edit) => {
            let title = match edit.target {
                MsgTarget::Reword => "message*",
                MsgTarget::Squash { .. } => "squash message*",
                MsgTarget::Split { .. } => "new commit message*",
            };
            (title, format!("{}\u{2588}", edit.buffer), 0)
        }
        None => ("message", app.message.clone(), app.msg_scroll),
    };
    let editing = app.msg_edit.is_some();
    let p = Paragraph::new(body)
        .block(pane_block(title, editing || app.focus == Pane::Message))
        .scroll((scroll, 0));
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

/// Render the interactive split selector into the diff pane: each `+`/`-` line
/// gets a ○/◉ marker for its piece, the cursor line is highlighted.
fn draw_split(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let st = app.split.as_mut().expect("split active");
    let items: Vec<ListItem> = st
        .lines
        .iter()
        .map(|l| {
            let selected = l
                .change_index
                .is_some_and(|ci| st.selected.contains(&ci));
            let (marker, style) = match l.kind {
                SplitKind::Meta => ("  ", Style::default().fg(Color::DarkGray)),
                SplitKind::Context => ("  ", Style::default()),
                SplitKind::Added => (
                    if selected { "◉ " } else { "○ " },
                    Style::default().fg(Color::Green),
                ),
                SplitKind::Removed => (
                    if selected { "◉ " } else { "○ " },
                    Style::default().fg(Color::Red),
                ),
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::styled(l.text.clone(), style),
            ]))
        })
        .collect();
    let cursor = st.cursor;
    st.list_state.select(Some(cursor));
    let list = List::new(items)
        .block(pane_block("split — Space toggles, Enter confirms", true))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut st.list_state);
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
        ("e", "edit the message (Ctrl-S applies, Esc leaves)"),
        ("s", "squash into the older neighbour"),
        ("S", "split the commit (line-level selector)"),
        ("D", "delete the selected commit"),
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

    /// A two-branch tracked queue (`a` ← `b`), built via the real binary.
    fn repo_with_two_branches() -> tempfile::TempDir {
        use assert_cmd::Command as Bin;
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "T"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(dir, &["add", "seed.txt"]);
        git(dir, &["commit", "-q", "-m", "seed"]);
        let bin = |args: &[&str]| {
            Bin::cargo_bin("git-queue")
                .unwrap()
                .current_dir(dir)
                .args(args)
                .assert()
                .success();
        };
        for (branch, file) in [("a", "a.txt"), ("b", "b.txt")] {
            bin(&["create", branch]);
            std::fs::write(dir.join(file), file).unwrap();
            git(dir, &["add", file]);
            bin(&["commit", "-m", &format!("{branch} work")]);
        }
        tmp
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

    /// An untracked `feature` branch with one commit that adds a three-line
    /// file — splittable at the line level.
    fn repo_with_multiline_commit() -> tempfile::TempDir {
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
        std::fs::write(dir.join("f.txt"), "A\nB\nC\n").unwrap();
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", "abc"]);
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

    fn ctrl(app: &mut App, c: char) {
        handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)).unwrap();
    }

    #[test]
    fn shift_d_deletes_the_selected_commit() {
        with_app(|app| {
            let before = app.commit_count();
            key(app, KeyCode::Char('g')); // front commit
            press(app, 'D');
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(app.commit_count(), before - 1, "one commit removed");
        });
    }

    #[test]
    fn deleting_a_branches_last_commit_offers_dissolve() {
        with_app_over(repo_with_two_branches(), |app| {
            assert_eq!(boundary_names(app), vec!["a", "b"]);
            key(app, KeyCode::Char('G')); // b's commit (the tip)
            press(app, 'D');
            assert_eq!(app.dissolve_prompt, Some(1), "b emptied — dissolve offered");

            press(app, 'y'); // dissolve
            assert!(app.dissolve_prompt.is_none());
            assert_eq!(boundary_names(app), vec!["a"]);
        });
    }

    #[test]
    fn declining_dissolve_keeps_the_empty_branch() {
        with_app_over(repo_with_two_branches(), |app| {
            key(app, KeyCode::Char('G'));
            press(app, 'D');
            assert_eq!(app.dissolve_prompt, Some(1));
            press(app, 'n'); // keep it
            assert!(app.dissolve_prompt.is_none());
            assert_eq!(boundary_names(app), vec!["a", "b"], "b kept, now empty");
        });
    }

    #[test]
    fn editing_and_applying_rewords_the_selected_commit() {
        with_app(|app| {
            let before = app.message.clone();
            press(app, 'e');
            assert!(app.msg_edit.is_some(), "entered edit mode");
            // The user rewrites the whole message.
            app.msg_edit = Some(MsgEdit {
                buffer: "a brand new subject".into(),
                target: MsgTarget::Reword,
            });
            ctrl(app, 's'); // apply
            assert!(app.msg_edit.is_none(), "applied and left edit mode");
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(
                app.engine.line().commits[app.selected].subject,
                "a brand new subject"
            );
            assert_ne!(app.message, before, "the pane refreshed to the new message");
        });
    }

    #[test]
    fn typing_does_not_touch_git_until_apply() {
        with_app(|app| {
            let tip_before = app.engine.line().commits[app.selected].sha.clone();
            press(app, 'e');
            press(app, 'X');
            press(app, 'Y');
            assert!(app.message_dirty());
            // No apply yet: the commit is untouched.
            assert_eq!(app.engine.line().commits[app.selected].sha, tip_before);
        });
    }

    #[test]
    fn leaving_a_dirty_edit_prompts_apply_or_discard() {
        with_app(|app| {
            press(app, 'e');
            press(app, 'Z'); // dirty the buffer
            assert!(app.message_dirty());
            key(app, KeyCode::Esc); // navigate away
            assert!(app.msg_apply_discard, "apply/discard prompt raised");

            press(app, 'd'); // discard
            assert!(app.msg_edit.is_none());
            assert!(!app.msg_apply_discard);
        });
    }

    #[test]
    fn leaving_a_clean_edit_needs_no_prompt() {
        with_app(|app| {
            press(app, 'e');
            key(app, KeyCode::Esc);
            assert!(app.msg_edit.is_none(), "left edit mode directly");
            assert!(!app.msg_apply_discard);
        });
    }

    #[test]
    fn squashing_the_front_commit_is_rejected() {
        with_app(|app| {
            key(app, KeyCode::Char('g')); // front commit
            press(app, 's');
            assert!(app.msg_edit.is_none());
            assert!(!app.status.is_empty(), "reports there is nothing older");
        });
    }

    #[test]
    fn squash_prompts_for_a_combined_message_then_folds() {
        with_app(|app| {
            let before = app.commit_count();
            key(app, KeyCode::Char('G')); // last commit
            press(app, 's');
            assert!(app.msg_edit.is_some(), "combined-message prompt opened");
            ctrl(app, 's'); // apply with the default combined message
            assert!(app.msg_edit.is_none());
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(app.commit_count(), before - 1, "folded into one commit");
            assert_eq!(app.selected, 0, "selection follows the combined commit");
        });
    }

    #[test]
    fn cross_boundary_squash_offers_to_dissolve_the_emptied_branch() {
        with_app_over(repo_with_two_branches(), |app| {
            key(app, KeyCode::Char('G')); // b's only commit
            press(app, 's');
            assert!(app.msg_edit.is_some());
            ctrl(app, 's'); // squash into a
            assert_eq!(app.dissolve_prompt, Some(1), "b emptied, dissolve offered");
            press(app, 'y');
            assert_eq!(boundary_names(app), vec!["a"]);
        });
    }

    #[test]
    fn split_selector_toggles_lines_and_peels_into_a_new_commit() {
        with_app_over(repo_with_multiline_commit(), |app| {
            let before = app.commit_count();
            press(app, 'S');
            assert!(app.split.is_some(), "entered the split selector");

            // Toggle the line under the cursor into the peeled piece.
            key(app, KeyCode::Char(' '));
            assert_eq!(app.split.as_ref().unwrap().selected.len(), 1);

            key(app, KeyCode::Enter); // confirm the selection
            assert!(app.split.is_none());
            assert!(app.msg_edit.is_some(), "prompts for the peeled commit's message");

            press(app, 'p'); // type a message
            ctrl(app, 's'); // apply
            assert!(app.msg_edit.is_none());
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(app.commit_count(), before + 1, "split produced a second commit");
        });
    }

    #[test]
    fn escape_cancels_the_split_selector() {
        with_app_over(repo_with_multiline_commit(), |app| {
            let before = app.commit_count();
            press(app, 'S');
            assert!(app.split.is_some());
            key(app, KeyCode::Esc);
            assert!(app.split.is_none(), "cancelled");
            assert_eq!(app.commit_count(), before, "nothing changed");
        });
    }

    #[test]
    fn confirming_split_with_all_lines_selected_is_rejected() {
        with_app_over(repo_with_multiline_commit(), |app| {
            press(app, 'S');
            // Toggle every selectable line.
            loop {
                key(app, KeyCode::Char(' '));
                let st = app.split.as_ref().unwrap();
                if next_selectable(&st.lines, st.cursor, 1).is_none() {
                    break;
                }
                key(app, KeyCode::Char('j'));
            }
            key(app, KeyCode::Enter);
            assert!(app.split.is_some(), "still in the selector");
            assert!(!app.status.is_empty(), "rejected selecting all lines");
        });
    }
}

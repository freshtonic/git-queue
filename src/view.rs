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
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
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

/// A pane divider the user is dragging to resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Divider {
    /// Between the queue (left) and the right column.
    Vertical,
    /// Between the message (top) and diff (bottom) in the right column.
    Horizontal,
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
    help_scroll: u16,
    /// The selected commit's cached message and diff (recomputed on move).
    message: String,
    diff: String,
    /// Scroll offset of the queue list, kept so mouse clicks hit-test correctly.
    list_state: ListState,
    /// Pane rectangles from the last frame, for mouse hit-testing.
    queue_area: Rect,
    message_area: Rect,
    diff_area: Rect,
    /// The whole body (all three panes) and the right column, for resize math.
    body_area: Rect,
    right_area: Rect,
    /// Split ratios (percentages), adjustable by dragging the dividers: `h_split`
    /// is the queue's width, `v_split` the message pane's height.
    h_split: u16,
    v_split: u16,
    /// Whether the right-hand message / diff panes are shown. Hiding one gives
    /// the other the full right column; hiding both lets the queue fill the body.
    show_message: bool,
    show_diff: bool,
    /// The divider currently being dragged, if any.
    drag: Option<Divider>,
    /// The repository's web URL (for opening PR links), resolved once at start.
    repo_url: Option<String>,
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
    /// When moving a commit: the index of the picked-up commit. While `Some`,
    /// `move_cursor` is the destination gap (0..=N) and the move applies on
    /// Enter — one rebase, not one per keystroke.
    move_from: Option<usize>,
    /// The destination gap for a move: 0 = before the front commit, N = after
    /// the tip. Gap `g` means "place just before commit `g`".
    move_cursor: usize,
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
            help_scroll: 0,
            message: String::new(),
            diff: String::new(),
            list_state: ListState::default(),
            queue_area: Rect::default(),
            message_area: Rect::default(),
            diff_area: Rect::default(),
            body_area: Rect::default(),
            right_area: Rect::default(),
            h_split: 42,
            v_split: 40,
            show_message: true,
            show_diff: true,
            drag: None,
            repo_url: git::github_repo_url(&crate::meta::remote()),
            input: None,
            confirm_quit: false,
            conflict_prompt: false,
            msg_edit: None,
            msg_apply_discard: false,
            dissolve_prompt: None,
            move_from: None,
            move_cursor: 0,
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
    /// Pick up the selected commit to move it: enter move mode with the
    /// destination cursor at the commit's current position.
    fn start_move(&mut self) {
        if self.commit_count() < 2 {
            self.status = "nothing to reorder".into();
            return;
        }
        self.move_from = Some(self.selected);
        self.move_cursor = self.selected; // its own front gap
    }

    /// Move the destination cursor within the gaps `0..=N`.
    fn move_cursor_by(&mut self, delta: isize) {
        let n = self.commit_count();
        let next = (self.move_cursor as isize + delta).clamp(0, n as isize);
        self.move_cursor = next as usize;
    }

    fn cancel_move(&mut self) {
        self.move_from = None;
    }

    /// Apply the pending move: place the picked-up commit at the cursor gap.
    /// Placing it back where it started just cancels (no rebase).
    fn place_move(&mut self) {
        let Some(from) = self.move_from.take() else {
            return;
        };
        let g = self.move_cursor;
        // Gaps `from` and `from+1` are the commit's own position → no-op.
        if g == from || g == from + 1 {
            return;
        }
        let to = if g < from { g } else { g - 1 };
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

    /// Whether a pane is currently visible.
    fn pane_visible(&self, pane: Pane) -> bool {
        match pane {
            Pane::Queue => true,
            Pane::Message => self.show_message,
            Pane::Diff => self.show_diff,
        }
    }

    /// Move keyboard focus to the next visible pane.
    fn cycle_focus(&mut self) {
        let order = [Pane::Queue, Pane::Message, Pane::Diff];
        let cur = order.iter().position(|&p| p == self.focus).unwrap_or(0);
        for step in 1..=order.len() {
            let cand = order[(cur + step) % order.len()];
            if self.pane_visible(cand) {
                self.focus = cand;
                return;
            }
        }
    }

    fn toggle_message_pane(&mut self) {
        self.show_message = !self.show_message;
        if !self.pane_visible(self.focus) {
            self.focus = Pane::Queue;
        }
    }

    fn toggle_diff_pane(&mut self) {
        self.show_diff = !self.show_diff;
        if !self.pane_visible(self.focus) {
            self.focus = Pane::Queue;
        }
    }

    /// The pane divider (if any) the cursor is on, for a resize drag.
    fn divider_at(&self, col: u16, row: u16) -> Option<Divider> {
        // Vertical divider: the shared border between the queue and the right
        // column (only when a right pane is shown).
        if self.right_area.width > 0 {
            let vx = self.queue_area.right();
            let in_rows = row >= self.body_area.y && row < self.body_area.bottom();
            if in_rows && (col == vx || col + 1 == vx) {
                return Some(Divider::Vertical);
            }
        }
        // Horizontal divider: between message and diff (only when both shown).
        if self.message_area.height > 0 && self.diff_area.height > 0 {
            let hy = self.message_area.bottom();
            let in_cols = col >= self.right_area.x && col < self.right_area.right();
            if in_cols && (row == hy || row + 1 == hy) {
                return Some(Divider::Horizontal);
            }
        }
        None
    }

    /// Update the split ratio the user is dragging to the cursor position.
    fn resize_to(&mut self, col: u16, row: u16) {
        match self.drag {
            Some(Divider::Vertical) if self.body_area.width > 0 => {
                let rel = (col.saturating_sub(self.body_area.x)) as u32 * 100
                    / self.body_area.width as u32;
                self.h_split = (rel as u16).clamp(15, 80);
            }
            Some(Divider::Horizontal) if self.right_area.height > 0 => {
                let rel = (row.saturating_sub(self.right_area.y)) as u32 * 100
                    / self.right_area.height as u32;
                self.v_split = (rel as u16).clamp(15, 85);
            }
            _ => {}
        }
    }

    /// The PR URL if the cursor is on a branch header's PR link, else `None`.
    fn pr_link_at(&self, col: u16, row: u16) -> Option<String> {
        let repo = self.repo_url.as_ref()?;
        let inner_top = self.queue_area.y + 1;
        if row < inner_top {
            return None;
        }
        let display_index = self.list_state.offset() + (row - inner_top) as usize;
        let rows = self.engine.line().rows();
        let Row::Branch { boundary } = rows.get(display_index)? else {
            return None;
        };
        let n = self.engine.pr_of(*boundary)?;
        let name = self.engine.branch_name(*boundary);
        // Content starts after the left border (1) + highlight gutter (2).
        let link_start = self.queue_area.x + 3 + format!("[{name}] ").chars().count() as u16;
        (col >= link_start).then(|| format!("{repo}/pull/{n}"))
    }
}

/// Open `url` in the user's default browser (best-effort).
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
        // Block for one event, then drain any already-queued ones so a flood of
        // mouse-wheel / drag events collapses into a single redraw instead of
        // one redraw per event.
        handle_event(app, event::read()?)?;
        while !app.quit && event::poll(std::time::Duration::ZERO)? {
            handle_event(app, event::read()?)?;
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn handle_event(app: &mut App, event: Event) -> Result<()> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key)?,
        Event::Mouse(m) => handle_mouse(app, m)?,
        _ => {}
    }
    Ok(())
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    // Modal states consume input first, in priority order.
    if app.show_help {
        // Scroll the help; any other key dismisses it.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
            KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
            KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
            _ => {
                app.show_help = false;
                app.help_scroll = 0;
            }
        }
        return Ok(());
    }
    if app.split.is_some() {
        return handle_split_key(app, key);
    }
    if app.move_from.is_some() {
        return handle_move_key(app, key);
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
        KeyCode::Tab => app.cycle_focus(),
        KeyCode::Char('1') => app.toggle_message_pane(),
        KeyCode::Char('2') => app.toggle_diff_pane(),
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

        // ---- reorder: pick up the commit, then position and place it ----
        KeyCode::Char(' ') => app.start_move(),

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

/// Positioning a picked-up commit: j/k move the destination cursor, Enter
/// places it (one rebase), Esc cancels.
fn handle_move_key(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => app.move_cursor_by(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_cursor_by(-1),
        KeyCode::Char('g') => app.move_cursor = 0,
        KeyCode::Char('G') => app.move_cursor = app.commit_count(),
        KeyCode::Enter => app.place_move(),
        KeyCode::Esc | KeyCode::Char('q') => app.cancel_move(),
        _ => {}
    }
    Ok(())
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
    // Pane-divider drags work regardless of mode (the panes stay visible).
    match m.kind {
        MouseEventKind::Up(_) => app.drag = None,
        MouseEventKind::Drag(MouseButton::Left) if app.drag.is_some() => {
            app.resize_to(m.column, m.row);
            return Ok(());
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(d) = app.divider_at(m.column, m.row) {
                app.drag = Some(d);
                return Ok(());
            }
        }
        _ => {}
    }
    if app.drag.is_some() {
        return Ok(()); // swallow stray events mid-drag
    }

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
        || app.move_from.is_some()
    {
        return Ok(());
    }
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if intersects(app.queue_area, m.column, m.row) {
                // A click on a branch's PR link opens it in the browser.
                if let Some(url) = app.pr_link_at(m.column, m.row) {
                    open_url(&url);
                } else {
                    app.focus = Pane::Queue;
                    click_select_commit(app, m.row);
                    app.refresh_selection()?;
                }
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

fn intersects(area: Rect, col: u16, row: u16) -> bool {
    col >= area.x && col < area.right() && row >= area.y && row < area.bottom()
}

/// The insertion-cursor line shown between commits while moving one.
fn move_cursor_item(inner_width: usize) -> ListItem<'static> {
    let label = "▸ Enter: place here · Esc: cancel ";
    let dashes = inner_width.saturating_sub(label.chars().count());
    ListItem::new(Line::from(Span::styled(
        format!("{label}{}", "─".repeat(dashes)),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
}

/// Greedy word-wrap `s` to `width` columns, returning at least one line. Used to
/// soft-wrap a commit subject that doesn't fit the queue pane.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 || s.chars().count() <= width {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in s.split_whitespace() {
        let wlen = word.chars().count();
        if cur_len == 0 {
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_len = wlen;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    // A full-width header, the body, and a one-row footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());
    draw_header(f, outer[0]);
    let body = outer[1];
    app.body_area = body;

    let right_shown = app.show_message || app.show_diff;

    // Horizontal split: the right column only exists if a right pane is shown.
    let (queue_area, right_col) = if right_shown {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(app.h_split),
                Constraint::Percentage(100 - app.h_split),
            ])
            .split(body);
        (chunks[0], Some(chunks[1]))
    } else {
        (body, None)
    };
    app.queue_area = queue_area;
    app.right_area = right_col.unwrap_or_default();
    // Reset the right-pane rects; only the visible ones get set below.
    app.message_area = Rect::default();
    app.diff_area = Rect::default();

    draw_queue(f, app, queue_area);

    if let Some(right) = right_col {
        match (app.show_message, app.show_diff) {
            (true, true) => {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(app.v_split),
                        Constraint::Percentage(100 - app.v_split),
                    ])
                    .split(right);
                app.message_area = split[0];
                app.diff_area = split[1];
                draw_message(f, app, split[0]);
                draw_diff_or_split(f, app, split[1]);
            }
            (true, false) => {
                app.message_area = right;
                draw_message(f, app, right);
            }
            (false, true) => {
                app.diff_area = right;
                draw_diff_or_split(f, app, right);
            }
            (false, false) => {}
        }
    }

    draw_footer(f, app, outer[2]);

    if app.show_help {
        draw_help(f, body, app.help_scroll);
    }
}

/// The diff pane, or the interactive line-selector when a split is in progress.
fn draw_diff_or_split(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    if app.split.is_some() {
        draw_split(f, app, area);
    } else {
        draw_diff(f, app, area);
    }
}

/// The full-width header bar.
fn draw_header(f: &mut ratatui::Frame, area: Rect) {
    let text = format!(" git-queue-tui v{}", env!("CARGO_PKG_VERSION"));
    let header = Paragraph::new(text).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(header, area);
}

/// The footer line: an active prompt, the quit-guard, a status/error message,
/// or the default key hint — in that priority.
fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let (text, style) = if let Some(from) = app.move_from {
        let subject = app
            .engine
            .line()
            .commits
            .get(from)
            .map(|c| c.subject.as_str())
            .unwrap_or("commit");
        (
            format!("moving `{subject}` — j/k position · Enter place · Esc cancel"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(st) = &app.split {
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
            "j/k select  Space move  e message  s squash  S split  D delete  \
             r rename  a new-branch  x dissolve  1/2 hide panes  u undo  \
             C-r redo  ? help  q quit"
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
    let move_from = app.move_from;
    let move_cursor = app.move_cursor;
    let commit_count = app.commit_count();
    let line = app.engine.line();
    let (rows, selected_row) = app.rows_and_selected_row();
    // Cached PR numbers per boundary (avoids borrowing the engine in the map).
    let prs: Vec<Option<u64>> = (0..line.boundaries.len())
        .map(|i| app.engine.pr_of(i))
        .collect();

    // Commit row prefix: `<id:10> <sha:8> `; continuation lines align under the
    // subject. Wrap width is what's left of the pane after border + gutter.
    const PREFIX_WIDTH: usize = 10 + 1 + 8 + 1; // id, space, short sha, space
    let content_width = (area.width as usize).saturating_sub(2 + 2); // borders + gutter
    let subject_width = content_width.saturating_sub(PREFIX_WIDTH).max(8);
    let indent = " ".repeat(PREFIX_WIDTH);

    let mut items: Vec<ListItem> = Vec::new();
    // The list row of the insertion cursor while moving (for scroll/highlight).
    let mut cursor_row: Option<usize> = None;
    for r in &rows {
        // The insertion cursor sits just before the commit at `move_cursor`.
        if let Row::Commit { index } = r {
            if move_from.is_some() && *index == move_cursor {
                cursor_row = Some(items.len());
                items.push(move_cursor_item(content_width));
            }
        }
        match r {
            Row::Branch { boundary } => {
                let mut spans = vec![Span::styled(
                    format!("[{}]", line.boundaries[*boundary].name),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )];
                // The PR number renders as a clickable link (click to open).
                if let Some(n) = prs[*boundary] {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        format!("#{n}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                }
                items.push(ListItem::new(Line::from(spans)));
            }
            Row::Commit { index } => {
                let c = &line.commits[*index];
                let id = match &c.id {
                    Some(id) => id.chars().take(10).collect::<String>(),
                    None => "(no id)".to_string(),
                };
                let short: String = c.sha.chars().take(8).collect();
                let prefix = format!("{id:<10} {short:<8} ");
                let mut subject = c.subject.clone();
                if c.empty {
                    subject.push_str("  (empty)");
                }
                // The picked-up commit fades out while it's being moved.
                let dim = move_from == Some(*index);
                let prefix_style = if dim {
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
                } else {
                    Style::default().fg(Color::Blue)
                };
                let text_style = if dim {
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                };
                // Soft-wrap the subject; continuation lines keep the indent.
                let wrapped = wrap_text(&subject, subject_width);
                let mut lines: Vec<Line> = Vec::new();
                for (i, chunk) in wrapped.into_iter().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(prefix.clone(), prefix_style),
                            Span::styled(chunk, text_style),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw(indent.clone()),
                            Span::styled(chunk, text_style),
                        ]));
                    }
                }
                items.push(ListItem::new(lines));
            }
        }
    }
    // The cursor can also sit after the tip (gap == commit count).
    if move_from.is_some() && move_cursor >= commit_count {
        cursor_row = Some(items.len());
        items.push(move_cursor_item(content_width));
    }

    // While moving, track the cursor line (so it scrolls into view); otherwise
    // the selected commit.
    let highlight_row = if move_from.is_some() {
        cursor_row
    } else {
        Some(selected_row)
    };
    app.list_state.select(highlight_row);
    let list = List::new(items)
        .block(pane_block("queue", app.focus == Pane::Queue))
        // A clear marker + reversed style so the selected commit is unmistakable.
        .highlight_symbol("❯ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
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

/// Compact key → one-liner, for the obvious navigation/view keys.
const HELP_KEYS: &[(&str, &str)] = &[
    ("j / k  ·  ↓ / ↑", "select the next / previous commit"),
    ("g / G", "select the first / last commit"),
    ("Tab", "cycle focus between visible panes"),
    ("Ctrl-d / Ctrl-u  ·  PgDn / PgUp", "scroll the focused pane"),
    ("1 / 2", "hide or show the message / diff pane"),
    ("?", "toggle this help  ·  q / Esc  quit"),
];

/// Key → (title, longer explanation), for the operations that aren't obvious.
const HELP_OPS: &[(&str, &str, &str)] = &[
    (
        "Space",
        "Move (reorder)",
        "Press Space to pick up the selected commit (it fades out); then j/k move \
         an insertion cursor between commits, Enter places it there, Esc \
         cancels. The rebase happens once, on place — not on every keystroke. \
         The commit keeps its Stable-Commit-Id; crossing a branch boundary \
         reassigns it to the branch it lands in. A conflict prompts Resolve \
         (drop to the shell) or Undo.",
    ),
    (
        "e",
        "Reword",
        "Make the message pane editable for the selected commit. Typing changes \
         nothing until you apply with Ctrl-S, which rewrites the message \
         (preserving the id) and rebases the commits after it. Leaving with \
         unapplied edits asks apply/discard.",
    ),
    (
        "s",
        "Squash",
        "Fold the selected commit into the one before it (its older neighbour). \
         The older commit keeps its id; the two descriptions are combined \
         (you're prompted to trim when both are non-empty). Squashing across a \
         branch boundary lands the result in the older branch, and offers to \
         dissolve the newer branch if that empties it.",
    ),
    (
        "S",
        "Split",
        "Divide the selected commit into two. The diff pane becomes a selector: \
         Space toggles which added/removed lines peel into a new commit. The \
         older piece keeps the original id and message; the peeled piece gets a \
         message you write and a fresh id. Repeat on the remainder for more \
         pieces.",
    ),
    (
        "D",
        "Delete",
        "Remove the selected commit; the commits after it replay onto its parent \
         (with the resolve/undo escape hatch on conflict). If deleting empties a \
         branch, you're offered to dissolve it.",
    ),
    (
        "a",
        "New branch",
        "Start a new branch at the selected commit: that commit becomes the new \
         branch's front, and the new branch takes the commits from there up to \
         the current branch's tip. The current branch keeps the commits before \
         it. Ref-only — no commit is rewritten.",
    ),
    (
        "r",
        "Rename branch",
        "Rename the branch that owns the selected commit. Ref-only.",
    ),
    (
        "x",
        "Dissolve branch",
        "Remove the selected commit's branch, folding its commits into a \
         neighbour (its child, or its parent if it's the tip). The commits stay; \
         only the branch ref and its PR association go. Ref-only.",
    ),
    (
        "< / >",
        "Shift boundary",
        "Move the boundary below the selected commit's branch by one commit, \
         shifting a commit between the two adjacent branches. Ref-only.",
    ),
    (
        "u  ·  Ctrl-r",
        "Undo / Redo",
        "Undo or redo any operation from this session — unlimited. Refs and queue \
         metadata are restored exactly; git's reflog is the durable backstop \
         across sessions.",
    ),
];

fn draw_help(f: &mut ratatui::Frame, area: Rect, scroll: u16) {
    let cyan = Style::default().fg(Color::Cyan);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    let section = |lines: &mut Vec<Line>, title: &str| {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    };

    lines.push(Line::from(Span::styled(
        "git queue tui — help",
        bold.add_modifier(Modifier::UNDERLINED),
    )));

    section(&mut lines, "Navigate & view");
    for (k, desc) in HELP_KEYS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<26} ", k), cyan),
            Span::raw(*desc),
        ]));
    }

    section(&mut lines, "Operations");
    for (k, title, explanation) in HELP_OPS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<10} ", k), cyan),
            Span::styled(title.to_string(), bold),
        ]));
        // The explanation wraps under a hanging indent.
        for para in wrap_text(explanation, (area.width as usize).saturating_sub(20).max(20)) {
            lines.push(Line::from(Span::styled(format!("             {para}"), dim)));
        }
        lines.push(Line::from(""));
    }

    section(&mut lines, "Mouse");
    for (k, desc) in [
        ("click", "select a commit · focus a pane · open a PR link (#N)"),
        ("drag", "drag a pane divider to resize"),
        ("wheel", "scroll the pane under the cursor"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<10} ", k), cyan),
            Span::raw(desc),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j/k or ↑/↓ scroll · any other key closes",
        dim,
    )));

    let popup = centered_rect(78, 84, area);
    f.render_widget(Clear, popup);
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" help "),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
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
            // Start a new branch at the tip commit; `feature` keeps the front.
            key(app, KeyCode::Char('G'));
            press(app, 'a');
            assert!(app.input.is_some(), "prompt opened");
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert!(app.input.is_none(), "prompt closed after apply");
            assert!(app.status.is_empty(), "no error: {}", app.status);
            assert_eq!(boundary_names(app), vec!["feature", "api"]);
        });
    }

    #[test]
    fn quit_guard_prompts_once_operations_are_pending() {
        with_app(|app| {
            // Make a change so the undo stack is non-empty.
            key(app, KeyCode::Char('G'));
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
            key(app, KeyCode::Char('G'));
            press(app, 'a');
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert_eq!(boundary_names(app), vec!["feature", "api"]);

            press(app, 'u'); // undo
            assert_eq!(boundary_names(app), vec!["feature"]);
            assert!(!app.engine.has_pending(), "undo emptied the stack");
        });
    }

    #[test]
    fn a_failed_op_surfaces_in_the_status_line() {
        with_app(|app| {
            // Starting a new branch at the branch's *first* commit is rejected
            // (it would leave the original branch empty).
            key(app, KeyCode::Char('g'));
            press(app, 'a');
            type_str(app, "api");
            key(app, KeyCode::Enter);
            assert!(!app.status.is_empty(), "error shown in status line");
            assert_eq!(boundary_names(app), vec!["feature"], "nothing changed");
        });
    }

    #[test]
    fn pick_up_position_and_place_reorders_a_commit() {
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
            press(app, ' '); // pick it up
            assert_eq!(app.move_from, Some(0), "picked up the front commit");
            press(app, 'j'); // cursor gap 0 → 1
            press(app, 'j'); // → 2 (after the tip)
            key(app, KeyCode::Enter); // place it there (one rebase)
            assert!(app.move_from.is_none(), "move completed");
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
    fn move_mode_renders_the_insertion_cursor() {
        with_app(|app| {
            press(app, ' '); // pick up
            press(app, 'j'); // position the cursor
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
            let text = buffer_text(terminal.backend());
            assert!(text.contains("place here"), "insertion cursor rendered");
        });
    }

    #[test]
    fn placing_a_commit_at_its_own_gap_is_a_no_op() {
        with_app(|app| {
            key(app, KeyCode::Char('g'));
            press(app, ' '); // pick up commit 0, cursor at gap 0
            key(app, KeyCode::Enter); // place at its own gap → cancel
            assert!(app.move_from.is_none());
            assert!(!app.engine.has_pending(), "no rebase happened");
        });
    }

    #[test]
    fn escape_cancels_a_move() {
        with_app(|app| {
            press(app, ' ');
            assert!(app.move_from.is_some());
            key(app, KeyCode::Esc);
            assert!(app.move_from.is_none(), "move cancelled");
            assert!(!app.engine.has_pending());
        });
    }

    #[test]
    fn a_conflicting_move_raises_the_prompt_and_undo_backs_out() {
        with_app_over(repo_with_conflict(), |app| {
            // Place "edit to B" (last) before "edit to A" — a line-2 conflict.
            key(app, KeyCode::Char('G'));
            press(app, ' '); // pick up index 2
            press(app, 'k'); // cursor gap 2 → 1
            key(app, KeyCode::Enter); // place → conflict
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
            press(app, ' ');
            press(app, 'k');
            key(app, KeyCode::Enter);
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
    fn help_shows_operation_explanations_and_scrolls() {
        with_app(|app| {
            press(app, '?');
            assert!(app.show_help);
            press(app, 'j'); // scroll down
            assert_eq!(app.help_scroll, 1);
            press(app, 'k');
            assert_eq!(app.help_scroll, 0);

            let mut terminal = Terminal::new(TestBackend::new(120, 44)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
            let text = buffer_text(terminal.backend());
            assert!(text.contains("reorder"), "operation titles are shown");
            assert!(
                text.contains("fades"),
                "longer explanations are shown"
            );

            // A non-scroll key closes the help.
            press(app, 'q');
            assert!(!app.show_help);
            assert!(!app.quit, "closing help does not quit");
        });
    }

    #[test]
    fn wrap_text_splits_on_word_boundaries() {
        assert_eq!(wrap_text("short", 10), vec!["short"]);
        assert_eq!(wrap_text("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap_text("anything", 0), vec!["anything"], "width 0 → one line");
    }

    #[test]
    fn toggling_hides_and_restores_the_right_panes() {
        with_app(|app| {
            assert!(app.show_message && app.show_diff);
            press(app, '1');
            assert!(!app.show_message);
            press(app, '2');
            assert!(!app.show_diff, "both right panes hidden");
            press(app, '1');
            assert!(app.show_message, "restored");
        });
    }

    #[test]
    fn focus_cycling_skips_hidden_panes() {
        with_app(|app| {
            app.show_message = false;
            app.focus = Pane::Queue;
            app.cycle_focus();
            assert_eq!(app.focus, Pane::Diff, "the hidden message pane is skipped");
        });
    }

    #[test]
    fn the_header_and_short_sha_render() {
        with_app(|app| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
            let text = buffer_text(terminal.backend());
            assert!(text.contains("git-queue-tui v"), "header rendered");
            let short: String = app.engine.line().commits[app.selected]
                .sha
                .chars()
                .take(8)
                .collect();
            assert!(text.contains(&short), "short sha shown alongside the id");
        });
    }

    #[test]
    fn dragging_the_vertical_divider_resizes_the_panes() {
        with_app(|app| {
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap(); // populates the pane rects
            let vx = app.queue_area.right();
            let row = app.body_area.y + 1;
            assert_eq!(app.divider_at(vx, row), Some(Divider::Vertical));

            let before = app.h_split;
            app.drag = Some(Divider::Vertical);
            app.resize_to(app.body_area.x + app.body_area.width * 3 / 4, row);
            assert!(app.h_split > before, "dragging right widened the queue");
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

//! The headless engine behind `git queue tui`.
//!
//! The engine owns the editing state of the current [queue line] — the ordered
//! commit sequence (front/oldest → tip) and the branch [boundaries] over it —
//! and (in later tickets) the undo/redo stack and the operations that mutate
//! git. It is deliberately *headless*: it touches `git`/`meta` but never the
//! terminal, so the interactive view sits in front of it and tests drive it
//! in-process (via `lib.rs`) instead of spawning the binary.
//!
//! This ticket is the scaffold: load the current line into the model and guard
//! the entry conditions. The operations-as-data type is defined even where its
//! variants are not yet applied — later tickets fill in `apply`.
//!
//! [queue line]: crate::queue::Line
//! [boundaries]: Boundary

use crate::git;
use crate::ident;
use crate::meta;
use crate::queue::Queue;
use anyhow::{bail, Result};
use std::collections::HashSet;

/// One commit of the loaded queue line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The commit's current SHA. Churns on every rewrite; use `id` for a
    /// stable handle.
    pub sha: String,
    /// The [Stable-Commit-Id](crate::ident) read from the commit's message,
    /// where present. Unstamped commits (e.g. an untracked branch's) carry
    /// `None`; any commit the TUI later creates is stamped so the queue never
    /// leaves with untracked commits.
    pub id: Option<String>,
    /// The commit's subject line.
    pub subject: String,
    /// Whether the commit introduces no change (an empty commit). Rendered
    /// `(empty)` when it still has a description; auto-dropped when it does not.
    pub empty: bool,
}

impl Commit {
    /// Whether the commit has a description (a non-empty subject). An empty,
    /// description-less commit is auto-dropped; an empty *described* one is kept.
    pub fn described(&self) -> bool {
        !self.subject.trim().is_empty()
    }
}

/// A branch [boundary](crate::queue) over the commit sequence: `name` owns the
/// contiguous run of commits ending just before index `end` (its exclusive
/// upper bound into [`EditableLine::commits`]). Boundaries are front → tip, their
/// `end`s strictly increase, and the last one's `end` equals the commit count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// The branch's full ref name (e.g. `queue/feature/api`).
    pub name: String,
    /// Exclusive upper index into [`EditableLine::commits`] where this branch's run
    /// ends. The run is `commits[prev_end..end]`.
    pub end: usize,
}

/// The loaded queue line: the commit sequence and the boundaries over it.
#[derive(Debug, Clone)]
pub struct EditableLine {
    /// The branch the line sits on (its front boundary's parent). Usually
    /// trunk, but any branch can be a base.
    pub base: String,
    /// Every commit of the line, front (oldest, merges first) → tip (newest).
    pub commits: Vec<Commit>,
    /// The branch boundaries over `commits`, front → tip. Never empty for a
    /// successfully loaded line.
    pub boundaries: Vec<Boundary>,
}

impl EditableLine {
    /// The run of commits owned by the boundary at `index`, front → tip.
    pub fn commits_of(&self, index: usize) -> &[Commit] {
        let start = if index == 0 {
            0
        } else {
            self.boundaries[index - 1].end
        };
        &self.commits[start..self.boundaries[index].end]
    }

    /// The boundary (branch) that owns the commit at `commit_index`.
    pub fn boundary_of(&self, commit_index: usize) -> usize {
        self.boundaries
            .iter()
            .position(|b| commit_index < b.end)
            .unwrap_or(self.boundaries.len().saturating_sub(1))
    }

    /// The queue pane's rows, front (top) → tip (bottom): each branch header
    /// followed by the commits it owns. Pure, so ordering is unit-tested
    /// without a repo.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (b, boundary) in self.boundaries.iter().enumerate() {
            rows.push(Row::Branch { boundary: b });
            let start = if b == 0 {
                0
            } else {
                self.boundaries[b - 1].end
            };
            for index in start..boundary.end {
                rows.push(Row::Commit { index });
            }
        }
        rows
    }
}

/// A row of the queue pane, indexing back into an [`EditableLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A branch header — the boundary at this index in `boundaries`.
    Branch { boundary: usize },
    /// A commit — at this index in `commits`.
    Commit { index: usize },
}

/// The kind of a line in the split selector's rendering of a commit's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    /// A file/hunk header — shown, not selectable.
    Meta,
    /// An unchanged context line — shown, not selectable.
    Context,
    /// An added (`+`) line — selectable.
    Added,
    /// A removed (`-`) line — selectable.
    Removed,
}

/// One display line of the split selector. Selectable lines (`Added`/`Removed`)
/// carry a dense `change_index` used to express the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitLine {
    pub kind: SplitKind,
    pub text: String,
    pub change_index: Option<usize>,
}

// --- internal diff model, shared by the selector and the split reconstruction ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkLine {
    kind: LineKind,
    text: String,
    /// Dense index over all selectable (added/removed) lines in the diff.
    change_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    /// 0-based start line of this hunk's region within the *new* file version.
    v_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileDiff {
    path: String,
    hunks: Vec<Hunk>,
    /// True when git rendered a binary change (no selectable lines).
    binary: bool,
}

/// Parse a unified diff (from `git diff <parent> <commit>`) into per-file
/// hunks, numbering the selectable (`+`/`-`) lines densely. Pure; unit-tested.
fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut change_counter = 0usize;
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            // `a/<path> b/<path>` — take the b-side path.
            let path = rest
                .split_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff {
                path,
                hunks: Vec::new(),
                binary: false,
            });
        } else if raw.starts_with("Binary files") {
            if let Some(f) = files.last_mut() {
                f.binary = true;
            }
        } else if let Some(rest) = raw.strip_prefix("@@") {
            // `@@ -a,b +c,d @@` — c is the 1-based new-file start line.
            let v_start = rest
                .split_once('+')
                .and_then(|(_, r)| r.split([',', ' ']).next())
                .and_then(|n| n.parse::<usize>().ok())
                .map(|n| n.saturating_sub(1))
                .unwrap_or(0);
            if let Some(f) = files.last_mut() {
                f.hunks.push(Hunk {
                    v_start,
                    lines: Vec::new(),
                });
            }
        } else if raw.starts_with("+++") || raw.starts_with("---") {
            // File-header markers; the path came from the `diff --git` line.
        } else if let Some(f) = files.last_mut() {
            let Some(hunk) = f.hunks.last_mut() else {
                continue;
            };
            let (kind, text) = if let Some(t) = raw.strip_prefix('+') {
                (LineKind::Added, t)
            } else if let Some(t) = raw.strip_prefix('-') {
                (LineKind::Removed, t)
            } else if let Some(t) = raw.strip_prefix(' ') {
                (LineKind::Context, t)
            } else {
                continue; // "\ No newline at end of file" and blanks
            };
            let change_index = if kind == LineKind::Context {
                None
            } else {
                let i = change_counter;
                change_counter += 1;
                Some(i)
            };
            hunk.lines.push(HunkLine {
                kind,
                text: text.to_string(),
                change_index,
            });
        }
    }
    files
}

/// Reconstruct the intermediate version of a file (`M`) for a split: the new
/// version with the *selected* changes reverted — selected additions removed,
/// selected removals restored. `selected` holds the `change_index`es peeled
/// into the second (newer) commit. Pure; unit-tested.
fn reconstruct_middle(file: &FileDiff, v_content: &str, selected: &HashSet<usize>) -> Option<String> {
    if file.binary {
        return None; // binary changes stay whole in the older piece
    }
    let mut v_lines: Vec<String> = v_content.lines().map(str::to_string).collect();
    // Splice each hunk's region bottom-up so earlier line numbers stay valid.
    for hunk in file.hunks.iter().rev() {
        // Lines present in the new version within this hunk (context + added).
        let v_len = hunk
            .lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Added))
            .count();
        let mut region: Vec<String> = Vec::new();
        for l in &hunk.lines {
            let selected_line = l.change_index.is_some_and(|i| selected.contains(&i));
            let keep = match l.kind {
                LineKind::Context => true,
                // A selected removal is still present in M (the newer piece
                // removes it); an unselected removal is gone (the older piece).
                LineKind::Removed => selected_line,
                // A selected addition is not yet in M; an unselected one is.
                LineKind::Added => !selected_line,
            };
            if keep {
                region.push(l.text.clone());
            }
        }
        let end = (hunk.v_start + v_len).min(v_lines.len());
        let start = hunk.v_start.min(v_lines.len());
        v_lines.splice(start..end, region);
    }
    let mut result = v_lines.join("\n");
    // Preserve a trailing newline (git files usually end with one).
    if v_content.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    Some(result)
}

/// An edit expressed as data. The view produces these from user input; the
/// engine applies them. Every variant the TUI will ever perform is named here
/// so undo becomes a snapshot↔operation pair and the engine stays directly
/// testable — but this scaffolding ticket does not yet *apply* any of them
/// (see [`Engine::apply`]). Later tickets fill the variants in, roughly in the
/// spine order: boundary edits (ref-only) → reorder/reword/delete → squash →
/// split → undo/redo.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Move the commit at `from` to sit at index `to` within the line,
    /// reassigning it to whichever branch's run it lands in.
    Reorder { from: usize, to: usize },
    /// Squash the commit at `index` into its adjacent, older (front-ward)
    /// neighbour. `message` is the combined description when the caller
    /// prompted for one (both were non-empty); `None` lets the engine pick the
    /// non-empty side or concatenate.
    Squash {
        index: usize,
        message: Option<String>,
    },
    /// Split the commit at `index` into two: the `selected` diff-line
    /// `change_index`es are peeled into a new, newer commit with `message` and
    /// a fresh id, while the older piece keeps the original id and message.
    /// Repeatable on the remainder for N pieces.
    Split {
        index: usize,
        selected: HashSet<usize>,
        message: String,
    },
    /// Rewrite the message of the commit at `index`, preserving its
    /// Stable-Commit-Id.
    Reword { index: usize, message: String },
    /// Delete the commit at `index`.
    Delete { index: usize },
    /// Start a new branch `name` at the commit at `index`: that commit becomes
    /// the new branch's front, and the new branch takes the tip-ward remainder
    /// of its current branch (matching `git queue edit`).
    AddBoundary { index: usize, name: String },
    /// Remove the boundary at `boundary`, dissolving that branch into its
    /// neighbour.
    RemoveBoundary { boundary: usize },
    /// Shift the boundary at `boundary` by `delta` commits, moving commits
    /// between the two adjacent branches.
    MoveBoundary { boundary: usize, delta: isize },
    /// Rename the branch at `boundary`.
    RenameBranch { boundary: usize, name: String },
    /// Undo the last applied operation.
    Undo,
    /// Redo the last undone operation.
    Redo,
}

/// The headless editing engine over the current queue line.
#[derive(Debug)]
pub struct Engine {
    line: EditableLine,
    /// The queue name new branches are recorded under and (when the line is
    /// namespaced) prefixed with.
    qname: String,
    /// Whether short branch names are namespaced to `queue/<qname>/<short>`.
    namespaced: bool,
    /// The branch HEAD is on; it follows renames/dissolves and is the landing
    /// point on exit.
    current: String,
    /// The branch the TUI launched from (for the exit summary).
    launched_from: String,
    /// Branches present at load with their cached PR numbers, so exit can warn
    /// about dissolved branches that had an open PR.
    original: Vec<(String, Option<u64>)>,
    /// Snapshots taken before each applied operation; undo restores the top.
    undo: Vec<Snapshot>,
    /// Snapshots of undone operations, for redo.
    redo: Vec<Snapshot>,
    /// Whether any operation has mutated git this session (gates the exit
    /// summary — a pure read-only session leaves the repo, and the output,
    /// untouched).
    touched: bool,
    /// When a rewriting operation conflicts, the pre-operation snapshot is
    /// parked here (and a rebase is left in progress) until the caller either
    /// undoes the conflict or suspends to the shell to resolve it.
    pending_conflict: Option<Snapshot>,
    /// Cached PR number per boundary, aligned with `line.boundaries`. Recomputed
    /// on load/reload — never per render — so the view can query it every frame
    /// without shelling out to `git config`.
    pr_cache: Vec<Option<u64>>,
}

/// The outcome of applying an operation.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// The operation completed and the model is up to date.
    Done,
    /// The operation conflicted: the repo is in a mid-rebase state. The caller
    /// must resolve it — [`Engine::undo_conflict`] to back out cleanly, or
    /// suspend to the shell (the user finishes the rebase, then re-runs
    /// `git queue tui`).
    Conflict,
}

/// A branch's full restorable state: its ref and the queue metadata that moves
/// with it. Captured before every operation so undo can put it back exactly —
/// the rewritten commit *objects* survive in git until GC (ADR-0002), so
/// restoring a ref to its old sha brings the old commit back too.
#[derive(Debug, Clone)]
struct BranchSnap {
    name: String,
    sha: String,
    parent: String,
    parent_sha: Option<String>,
    queue: Option<String>,
    pr: Option<u64>,
    description: Option<String>,
}

/// A whole-line snapshot: every branch's state plus which branch HEAD was on.
#[derive(Debug, Clone)]
struct Snapshot {
    branches: Vec<BranchSnap>,
    head: String,
}

impl Engine {
    /// Load the current queue line into the engine, applying the TUI's entry
    /// guards. Reuses `edit`'s scoping: the current queue line (an untracked
    /// branch opens as one provisional section over trunk). Refuses a forked
    /// line, a dirty worktree, and an empty queue.
    ///
    /// This is headless — it never checks for a TTY. The non-TTY guard lives
    /// in the command layer (`commands::tui`) so that tests can drive `load`
    /// in-process without a terminal.
    pub fn load() -> Result<Engine> {
        git::ensure_repo()?;
        if git::rebase_in_progress() {
            bail!(
                "a rebase is in progress — finish resolving it (`git status`, then \
                 `git rebase --continue` or `--abort`), then re-run `git queue tui`"
            );
        }
        if !git::worktree_clean() {
            bail!(
                "working tree has uncommitted changes; commit or stash them before \
                 opening the queue in the TUI"
            );
        }

        let queue = Queue::load()?;
        let branch = git::current_branch()?;
        let (branches, base) = Self::scope(&queue, &branch)?;
        let line = Self::build_line(base, &branches)?;
        if line.commits.is_empty() {
            bail!("the queue has no commits to edit");
        }

        let qname = branch_queue_name(&branches).unwrap_or_else(|| branch.replace('/', "-"));
        let namespaced = branches.iter().any(|b| b.starts_with("queue/"));
        let original: Vec<(String, Option<u64>)> =
            branches.iter().map(|b| (b.clone(), meta::pr(b))).collect();
        let pr_cache = line.boundaries.iter().map(|b| meta::pr(&b.name)).collect();

        Ok(Engine {
            line,
            qname,
            namespaced,
            current: branch.clone(),
            launched_from: branch,
            original,
            undo: Vec::new(),
            redo: Vec::new(),
            touched: false,
            pending_conflict: None,
            pr_cache,
        })
    }

    /// Resolve the current queue line's branches and base, refusing a forked
    /// line. An untracked branch resolves to one provisional section over trunk.
    fn scope(queue: &Queue, branch: &str) -> Result<(Vec<String>, String)> {
        if queue.is_tracked(branch) {
            let line = queue.line_through(branch)?;
            // A fork is any branch in the editing range with more than one
            // tracked child (covers a fork above *or* below the current
            // branch, which `line.fork_at` alone would miss).
            if line.branches.iter().any(|b| queue.children(b).len() > 1) {
                bail!(
                    "`{branch}` is on a forked queue line; the TUI cannot rewrite history \
                     shared with a sibling line. Use `git queue edit` for ref-only \
                     boundary changes, or checkout a single unforked line first"
                );
            }
            Ok((line.branches, line.base))
        } else {
            Ok((vec![branch.to_string()], queue.trunk.clone()))
        }
    }

    /// Build the commit sequence (front → tip) and the per-branch boundaries
    /// over it, by walking each branch's run.
    fn build_line(base: String, branches: &[String]) -> Result<EditableLine> {
        let mut commits = Vec::new();
        let mut boundaries = Vec::new();
        let mut parent = base.clone();
        for b in branches {
            for (sha, id, subject) in git::commits_between_with_ids(&parent, b)? {
                let empty = git::commit_is_empty(&sha);
                commits.push(Commit {
                    sha,
                    id,
                    subject,
                    empty,
                });
            }
            boundaries.push(Boundary {
                name: b.clone(),
                end: commits.len(),
            });
            parent = b.clone();
        }
        Ok(EditableLine {
            base,
            commits,
            boundaries,
        })
    }

    /// The loaded queue line: commit sequence and boundaries.
    pub fn line(&self) -> &EditableLine {
        &self.line
    }

    /// The branch the TUI was launched from.
    pub fn launched_from(&self) -> &str {
        &self.launched_from
    }

    /// The branch HEAD is currently on (the exit landing point).
    pub fn landing_branch(&self) -> &str {
        &self.current
    }

    /// The queue name new branches are recorded under.
    pub fn queue_name(&self) -> &str {
        &self.qname
    }

    /// Whether any operation is on the undo stack (drives the quit-guard).
    pub fn has_pending(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether any operation has mutated git this session.
    pub fn changed(&self) -> bool {
        self.touched
    }

    /// The final branch chain, front → tip, as `(parent, branch)` pairs.
    pub fn layout(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut parent = self.line.base.clone();
        for b in &self.line.boundaries {
            out.push((parent.clone(), b.name.clone()));
            parent = b.name.clone();
        }
        out
    }

    /// Branches that were present at load but have since been dissolved, and
    /// that carried a cached PR number — the exit summary warns about these.
    pub fn dissolved_with_prs(&self) -> Vec<(String, u64)> {
        let present: HashSet<&str> = self.line.boundaries.iter().map(|b| b.name.as_str()).collect();
        self.original
            .iter()
            .filter(|(name, _)| !present.contains(name.as_str()))
            .filter_map(|(name, pr)| pr.map(|n| (name.clone(), n)))
            .collect()
    }

    /// Apply an operation. Boundary edits, undo/redo, and reorder are handled
    /// here; the remaining history-rewriting variants land in later tickets.
    ///
    /// Most operations return [`Applied::Done`]; a rewriting operation that
    /// conflicts returns [`Applied::Conflict`] with a rebase in progress (see
    /// [`Engine::undo_conflict`] / [`Engine::conflicted`]).
    pub fn apply(&mut self, op: Operation) -> Result<Applied> {
        if self.pending_conflict.is_some() {
            bail!("a conflict is pending; resolve it in your shell or undo it first");
        }
        match op {
            Operation::RenameBranch { boundary, name } => {
                self.rename_branch(boundary, &name)?;
                Ok(Applied::Done)
            }
            Operation::AddBoundary { index, name } => {
                self.add_boundary(index, &name)?;
                Ok(Applied::Done)
            }
            Operation::RemoveBoundary { boundary } => {
                self.remove_boundary(boundary)?;
                Ok(Applied::Done)
            }
            Operation::MoveBoundary { boundary, delta } => {
                self.move_boundary(boundary, delta)?;
                Ok(Applied::Done)
            }
            Operation::Reorder { from, to } => self.reorder(from, to),
            Operation::Reword { index, message } => {
                self.reword(index, &message)?;
                Ok(Applied::Done)
            }
            Operation::Delete { index } => self.delete(index),
            Operation::Squash { index, message } => self.squash(index, message.as_deref()),
            Operation::Split {
                index,
                selected,
                message,
            } => self.split(index, &selected, &message),
            Operation::Undo => {
                self.undo()?;
                Ok(Applied::Done)
            }
            Operation::Redo => {
                self.redo()?;
                Ok(Applied::Done)
            }
        }
    }

    /// Whether a conflicted operation is awaiting resolution.
    pub fn conflicted(&self) -> bool {
        self.pending_conflict.is_some()
    }

    /// Back out a conflicted operation: abort the rebase and restore the
    /// pre-operation snapshot exactly.
    pub fn undo_conflict(&mut self) -> Result<()> {
        let Some(snap) = self.pending_conflict.take() else {
            bail!("no conflict to undo");
        };
        if git::rebase_in_progress() {
            git::rebase_abort()?;
        }
        self.restore(&snap)?;
        Ok(())
    }

    // ---- reorder (history-rewriting) ----

    /// Move the commit at `from` to sit at index `to` within the line,
    /// rebasing immediately. The commit keeps its Stable-Commit-Id; crossing a
    /// boundary reassigns it to the branch whose run it lands in (via
    /// `--update-refs`, exactly like `git queue move`).
    fn reorder(&mut self, from: usize, to: usize) -> Result<Applied> {
        let n = self.line.commits.len();
        if from >= n || to >= n {
            bail!("reorder index out of range");
        }
        if from == to {
            return Ok(Applied::Done);
        }

        let todo = reorder_todo(&self.line, from, to);
        let snap = self.snapshot()?;
        let base = self.line.base.clone();
        let top = self.line.boundaries.last().unwrap().name.clone();
        let land = self.current.clone();
        match git::rebase_with_todo_stop(&base, &top, &todo)? {
            git::Rewrite::Clean => {
                self.finish_rewrite(&land)?;
                self.undo.push(snap);
                self.redo.clear();
                self.touched = true;
                // A reorder can leave a commit empty (two commits touching the
                // same lines); auto-drop the undescribed ones (jj).
                self.auto_drop_empties(&land)?;
                Ok(Applied::Done)
            }
            git::Rewrite::Conflict => {
                self.pending_conflict = Some(snap);
                self.touched = true;
                Ok(Applied::Conflict)
            }
        }
    }

    // ---- delete (history-rewriting) ----

    /// Delete the commit at `index`: drop it from the line and replay
    /// descendants (through the conflict escape hatch if needed). Explicit
    /// delete always removes, empty or not. Afterwards, any commit left empty
    /// *and* description-less is auto-dropped.
    fn delete(&mut self, index: usize) -> Result<Applied> {
        let n = self.line.commits.len();
        if index >= n {
            bail!("delete index out of range");
        }
        if n == 1 {
            bail!("cannot delete the queue's only commit");
        }
        let mut drop = HashSet::new();
        drop.insert(index);
        let todo = drop_todo(&self.line, &drop);
        let snap = self.snapshot()?;
        let base = self.line.base.clone();
        let top = self.line.boundaries.last().unwrap().name.clone();
        let land = self.current.clone();
        match git::rebase_with_todo_stop(&base, &top, &todo)? {
            git::Rewrite::Clean => {
                self.finish_rewrite(&land)?;
                // Record undo before the empty-cleanup, so a delete stays
                // undoable even if the (rare) cleanup step fails.
                self.undo.push(snap);
                self.redo.clear();
                self.touched = true;
                self.auto_drop_empties(&land)?;
                Ok(Applied::Done)
            }
            git::Rewrite::Conflict => {
                self.pending_conflict = Some(snap);
                self.touched = true;
                Ok(Applied::Conflict)
            }
        }
    }

    /// Drop any commit that is empty *and* description-less (jj's rule). An
    /// empty commit that still has a description is kept and rendered
    /// `(empty)`. Dropping empty commits cannot conflict.
    fn auto_drop_empties(&mut self, land: &str) -> Result<()> {
        for _ in 0..64 {
            let drop: HashSet<usize> = self
                .line
                .commits
                .iter()
                .enumerate()
                .filter(|(_, c)| c.empty && !c.described())
                .map(|(i, _)| i)
                .collect();
            if drop.is_empty() {
                return Ok(());
            }
            let todo = drop_todo(&self.line, &drop);
            let base = self.line.base.clone();
            let top = self.line.boundaries.last().unwrap().name.clone();
            match git::rebase_with_todo_stop(&base, &top, &todo)? {
                git::Rewrite::Clean => self.finish_rewrite(land)?,
                git::Rewrite::Conflict => {
                    // Empty drops shouldn't conflict; if one does, don't leave a
                    // dangling rebase.
                    git::rebase_abort()?;
                    bail!("auto-dropping empty commits unexpectedly conflicted");
                }
            }
        }
        Ok(())
    }

    /// Boundary indices whose branch currently owns no commits (e.g. after a
    /// delete emptied it) — the view offers to dissolve these.
    pub fn empty_branches(&self) -> Vec<usize> {
        (0..self.line.boundaries.len())
            .filter(|&i| self.line.commits_of(i).is_empty())
            .collect()
    }

    /// The cached PR number of the branch at `boundary`, if any. O(1) — no
    /// git call — so the view can call it every frame.
    pub fn pr_of(&self, boundary: usize) -> Option<u64> {
        self.pr_cache.get(boundary).copied().flatten()
    }

    /// The branch name at `boundary`.
    pub fn branch_name(&self, boundary: usize) -> &str {
        &self.line.boundaries[boundary].name
    }

    // ---- split (history-rewriting) ----

    /// The split selector's view of the commit at `index`: its diff flattened
    /// into displayable lines, the `+`/`-` ones carrying a dense `change_index`.
    pub fn split_lines(&self, index: usize) -> Result<Vec<SplitLine>> {
        if index >= self.line.commits.len() {
            bail!("split index out of range");
        }
        let diff = git::commit_diff(&self.line.commits[index].sha)?;
        let mut out = Vec::new();
        for f in parse_diff(&diff) {
            out.push(SplitLine {
                kind: SplitKind::Meta,
                text: format!("── {} ──", f.path),
                change_index: None,
            });
            if f.binary {
                out.push(SplitLine {
                    kind: SplitKind::Meta,
                    text: "(binary — peeled into the new commit)".into(),
                    change_index: None,
                });
                continue;
            }
            for h in &f.hunks {
                out.push(SplitLine {
                    kind: SplitKind::Meta,
                    text: "@@".into(),
                    change_index: None,
                });
                for l in &h.lines {
                    let (kind, prefix) = match l.kind {
                        LineKind::Added => (SplitKind::Added, '+'),
                        LineKind::Removed => (SplitKind::Removed, '-'),
                        LineKind::Context => (SplitKind::Context, ' '),
                    };
                    out.push(SplitLine {
                        kind,
                        text: format!("{prefix}{}", l.text),
                        change_index: l.change_index,
                    });
                }
            }
        }
        Ok(out)
    }

    /// The number of selectable (`+`/`-`) lines in the commit at `index`.
    pub fn split_change_count(&self, index: usize) -> Result<usize> {
        let diff = git::commit_diff(&self.line.commits[index].sha)?;
        Ok(parse_diff(&diff)
            .iter()
            .flat_map(|f| &f.hunks)
            .flat_map(|h| &h.lines)
            .filter(|l| l.change_index.is_some())
            .count())
    }

    /// Split the commit at `index` into two. The `selected` change-lines are
    /// peeled into a new, newer commit (fresh id + `message`); the older piece
    /// keeps the original id and message. Descendants replay cleanly (the new
    /// tip has the same tree). One undo entry.
    fn split(&mut self, index: usize, selected: &HashSet<usize>, message: &str) -> Result<Applied> {
        if index >= self.line.commits.len() {
            bail!("split index out of range");
        }
        let total = self.split_change_count(index)?;
        if selected.is_empty() || selected.len() >= total {
            bail!("select some — but not all — lines to peel into the new commit");
        }

        let c_sha = self.line.commits[index].sha.clone();
        let parent_sha = if index == 0 {
            git::rev_parse(&self.line.base)?
        } else {
            self.line.commits[index - 1].sha.clone()
        };
        let diff = git::commit_diff(&c_sha)?;
        let files = parse_diff(&diff);

        // Build the older piece's tree: parent + the *unselected* changes.
        let parent_tree = git::tree_of(&parent_sha)?;
        let mut changes: Vec<(String, Option<String>)> = Vec::new();
        for f in &files {
            let v = git::file_at(&c_sha, &f.path);
            let Some(m) = reconstruct_middle(f, &v, selected) else {
                continue; // binary: stays whole in the newer piece
            };
            let parent_has = !git::file_at(&parent_sha, &f.path).is_empty();
            if m.is_empty() {
                if parent_has {
                    changes.push((f.path.clone(), None)); // removed in the older piece
                }
                // else: new file, fully peeled — absent from the older piece
            } else {
                changes.push((f.path.clone(), Some(m)));
            }
        }
        let m_tree = git::build_tree(&parent_tree, &changes)?;

        // Older piece keeps C's message (and id); newer piece is auto-stamped.
        let older = git::commit_tree(&m_tree, &parent_sha, &git::commit_message(&c_sha)?)?;
        let newer_msg = with_preserved_id(message, Some(&ident::new_id()));
        let newer = git::commit_tree(&git::tree_of(&c_sha)?, &older, &newer_msg)?;

        let snap = self.snapshot()?;
        // Replay C's descendants onto the newer piece (clean: same tree).
        let mut mapping: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut prev = newer.clone();
        for c in &self.line.commits[index + 1..] {
            prev = git::cherry_pick_onto(&prev, &c.sha)?;
            mapping.insert(c.sha.clone(), prev.clone());
        }

        // Point every branch at its tip's new sha.
        let boundaries = self.line.boundaries.clone();
        let mut parent = self.line.base.clone();
        for b in &boundaries {
            let tip_old = &self.line.commits[b.end - 1].sha;
            let new_tip = if *tip_old == c_sha {
                newer.clone()
            } else if let Some(n) = mapping.get(tip_old) {
                n.clone()
            } else {
                tip_old.clone()
            };
            git::force_ref(&b.name, &new_tip)?;
            meta::set_parent(&b.name, &parent)?;
            meta::set_parent_sha(&b.name, &git::rev_parse(&parent)?)?;
            parent = b.name.clone();
        }

        let land = self.current.clone();
        git::checkout_quiet(&land)?;
        git::reset_hard_head()?;
        self.reload()?;
        self.undo.push(snap);
        self.redo.clear();
        self.touched = true;
        self.auto_drop_empties(&land)?;
        Ok(Applied::Done)
    }

    // ---- squash (history-rewriting) ----

    /// Whether squashing the commit at `index` into its older neighbour needs a
    /// combined-message prompt — true only when *both* descriptions are
    /// non-empty (otherwise the non-empty side is used, or the result is empty).
    pub fn squash_needs_message(&self, index: usize) -> Result<bool> {
        if index == 0 || index >= self.line.commits.len() {
            bail!("no older commit to squash into");
        }
        let older = message_body(&self.line.commits[index - 1].sha)?;
        let newer = message_body(&self.line.commits[index].sha)?;
        Ok(!older.trim().is_empty() && !newer.trim().is_empty())
    }

    /// The default combined description shown in the squash prompt: the older
    /// body then the newer, blank line between (each without its id trailer).
    pub fn squash_default_message(&self, index: usize) -> Result<String> {
        if index == 0 || index >= self.line.commits.len() {
            bail!("no older commit to squash into");
        }
        Ok(combine_bodies(
            &message_body(&self.line.commits[index - 1].sha)?,
            &message_body(&self.line.commits[index].sha)?,
        ))
    }

    /// Squash the commit at `index` into its adjacent older neighbour. The
    /// older commit keeps its Stable-Commit-Id; the newer is absorbed. A
    /// cross-boundary squash lands the result in the older branch and may empty
    /// the newer one (see [`Engine::empty_branches`]).
    fn squash(&mut self, index: usize, message: Option<&str>) -> Result<Applied> {
        if index == 0 {
            bail!("the front commit has no older neighbour to squash into");
        }
        if index >= self.line.commits.len() {
            bail!("squash index out of range");
        }
        // Combined body: the caller's trimmed text, else the non-empty side or
        // a concatenation; then the older commit's id, preserved.
        let body = match message {
            Some(m) => m.to_string(),
            None => combine_bodies(
                &message_body(&self.line.commits[index - 1].sha)?,
                &message_body(&self.line.commits[index].sha)?,
            ),
        };
        let final_message = with_preserved_id(&body, self.line.commits[index - 1].id.as_deref());

        let todo = squash_todo(&self.line, index);
        let snap = self.snapshot()?;
        let base = self.line.base.clone();
        let top = self.line.boundaries.last().unwrap().name.clone();
        let land = self.current.clone();
        match git::rebase_squash_stop(&base, &top, &todo, &final_message)? {
            git::Rewrite::Clean => {
                self.finish_rewrite(&land)?;
                self.undo.push(snap);
                self.redo.clear();
                self.touched = true;
                self.auto_drop_empties(&land)?;
                Ok(Applied::Done)
            }
            git::Rewrite::Conflict => {
                self.pending_conflict = Some(snap);
                self.touched = true;
                Ok(Applied::Conflict)
            }
        }
    }

    // ---- reword (message-only rewrite) ----

    /// Rewrite the message of the commit at `index`, preserving its
    /// Stable-Commit-Id and rebasing descendants (message-only, so the replay
    /// is clean). One undo entry.
    fn reword(&mut self, index: usize, new_text: &str) -> Result<()> {
        if index >= self.line.commits.len() {
            bail!("reword index out of range");
        }
        let message = with_preserved_id(new_text, self.line.commits[index].id.as_deref());
        let todo = reword_todo(&self.line, index);
        let snap = self.snapshot()?;
        let base = self.line.base.clone();
        let top = self.line.boundaries.last().unwrap().name.clone();
        let land = self.current.clone();
        git::rebase_with_todo_message(&base, &top, &todo, &message)?;
        self.finish_rewrite(&land)?;
        self.undo.push(snap);
        self.redo.clear();
        self.touched = true;
        Ok(())
    }

    /// After a clean rewriting rebase: refresh each branch's rebase anchor to
    /// its parent's new tip, land HEAD on `land`, snap the worktree, reload.
    fn finish_rewrite(&mut self, land: &str) -> Result<()> {
        let names: Vec<String> = self.line.boundaries.iter().map(|b| b.name.clone()).collect();
        let mut parent = self.line.base.clone();
        for b in &names {
            meta::set_parent_sha(b, &git::rev_parse(&parent)?)?;
            parent = b.clone();
        }
        git::checkout_quiet(land)?;
        git::reset_hard_head()?;
        self.reload()?;
        Ok(())
    }

    // ---- boundary operations (ref-only; no commit is rewritten) ----

    /// Rename the branch at `boundary`.
    fn rename_branch(&mut self, boundary: usize, new_name: &str) -> Result<()> {
        let resolved = self.resolve_name(new_name);
        let old = self.line.boundaries[boundary].name.clone();
        if resolved == old {
            return Ok(());
        }
        if git::branch_exists(&resolved) {
            bail!("branch `{resolved}` already exists; pick a different name");
        }
        let mut segs = self.line.boundaries.clone();
        segs[boundary].name = resolved.clone();
        let land = if self.current == old {
            resolved
        } else {
            self.current.clone()
        };
        self.commit_op(segs, land)
    }

    /// Start a new branch `name` **at** commit `index`: the highlighted commit
    /// becomes the front of the new branch, which takes the tip-ward remainder
    /// of its current branch; the current branch keeps the commits before it.
    /// This matches `git queue edit`, where a header sits above the commits it
    /// owns — so the new branch's header lands on the highlighted commit.
    fn add_boundary(&mut self, index: usize, name: &str) -> Result<()> {
        let resolved = self.resolve_name(name);
        if git::branch_exists(&resolved) {
            bail!("branch `{resolved}` already exists; pick a different name");
        }
        let b = self.line.boundary_of(index);
        let start = if b == 0 {
            0
        } else {
            self.line.boundaries[b - 1].end
        };
        if index == start {
            bail!(
                "`{}` already starts at that commit; highlight a later commit to \
                 split off a new branch",
                self.line.boundaries[b].name
            );
        }
        let mut segs = self.line.boundaries.clone();
        let end = segs[b].end;
        // The existing branch keeps [start..index); the new branch owns
        // [index..end) with the highlighted commit as its front.
        segs[b].end = index;
        segs.insert(
            b + 1,
            Boundary {
                name: resolved,
                end,
            },
        );
        let land = self.current.clone();
        self.commit_op(segs, land)
    }

    /// Remove the boundary at `boundary`, dissolving that branch into its
    /// neighbour (its child, or its parent when it is the tip).
    fn remove_boundary(&mut self, boundary: usize) -> Result<()> {
        if self.line.boundaries.len() < 2 {
            bail!("cannot dissolve the only branch of the line");
        }
        let removed = self.line.boundaries[boundary].name.clone();
        let is_tip = boundary == self.line.boundaries.len() - 1;
        let survivor = if is_tip {
            self.line.boundaries[boundary - 1].name.clone()
        } else {
            self.line.boundaries[boundary + 1].name.clone()
        };
        let mut segs = self.line.boundaries.clone();
        segs.remove(boundary);
        // Removing the tip boundary would leave its commits unowned; extend the
        // new last branch to cover them.
        segs.last_mut().unwrap().end = self.line.commits.len();
        let land = if self.current == removed {
            survivor
        } else {
            self.current.clone()
        };
        self.commit_op(segs, land)
    }

    /// Shift the boundary at `boundary` by `delta` commits, moving commits
    /// between it and the next branch.
    fn move_boundary(&mut self, boundary: usize, delta: isize) -> Result<()> {
        if boundary + 1 >= self.line.boundaries.len() {
            bail!("the last branch's boundary is fixed at the tip");
        }
        let lower = if boundary == 0 {
            0
        } else {
            self.line.boundaries[boundary - 1].end as isize
        };
        let upper = self.line.boundaries[boundary + 1].end as isize;
        let new_end = self.line.boundaries[boundary].end as isize + delta;
        if new_end <= lower || new_end >= upper {
            bail!("that shift would empty a branch");
        }
        let mut segs = self.line.boundaries.clone();
        segs[boundary].end = new_end as usize;
        let land = self.current.clone();
        self.commit_op(segs, land)
    }

    // ---- undo / redo ----

    fn undo(&mut self) -> Result<()> {
        let Some(snap) = self.undo.pop() else {
            bail!("nothing to undo");
        };
        let redo = self.snapshot()?;
        self.restore(&snap)?;
        self.redo.push(redo);
        Ok(())
    }

    fn redo(&mut self) -> Result<()> {
        let Some(snap) = self.redo.pop() else {
            bail!("nothing to redo");
        };
        let undo = self.snapshot()?;
        self.restore(&snap)?;
        self.undo.push(undo);
        Ok(())
    }

    // ---- shared mutation machinery ----

    /// Namespace a short branch name to `queue/<qname>/<short>` when the line
    /// is namespaced; names with a `/`, existing line branches, and plain-named
    /// queues stay as-is (the same rule as `edit`/`create`).
    fn resolve_name(&self, n: &str) -> String {
        if n.contains('/') || self.line.boundaries.iter().any(|b| b.name == n) || !self.namespaced {
            n.to_string()
        } else {
            format!("queue/{}/{n}", self.qname)
        }
    }

    /// Snapshot before an operation, apply it by materialising the new segment
    /// layout, land HEAD on `land`, reload the model, and push the undo entry.
    fn commit_op(&mut self, segments: Vec<Boundary>, land: String) -> Result<()> {
        let snap = self.snapshot()?;
        self.materialise(&segments)?;
        git::checkout_quiet(&land)?;
        self.reload()?;
        self.undo.push(snap);
        self.redo.clear();
        self.touched = true;
        Ok(())
    }

    /// Rewrite branch refs and parent pointers to match `segments` over the
    /// current (fixed) commit sequence, and delete branches that dropped out.
    /// Ref-only: no commit object is rewritten. Leaves HEAD detached.
    fn materialise(&self, segments: &[Boundary]) -> Result<()> {
        git::detach_head()?;
        let target: HashSet<&str> = segments.iter().map(|s| s.name.as_str()).collect();
        let mut parent = self.line.base.clone();
        for s in segments {
            let tip_sha = self.line.commits[s.end - 1].sha.clone();
            if git::branch_exists(&s.name) {
                git::force_ref(&s.name, &tip_sha)?;
            } else {
                git::create_branch(&s.name, &tip_sha)?;
            }
            meta::set_parent(&s.name, &parent)?;
            meta::set_parent_sha(&s.name, &git::rev_parse(&parent)?)?;
            meta::set_branch_queue(&s.name, &self.qname)?;
            parent = s.name.clone();
        }
        for old in self.line.boundaries.iter().map(|b| b.name.clone()) {
            if !target.contains(old.as_str()) {
                meta::untrack(&old);
                git::run(&["branch", "-q", "-D", &old])?;
            }
        }
        meta::touch_queue(&self.qname);
        Ok(())
    }

    /// Capture the whole line's restorable state.
    fn snapshot(&self) -> Result<Snapshot> {
        let mut branches = Vec::new();
        for b in self.line.boundaries.iter().map(|x| &x.name) {
            branches.push(BranchSnap {
                name: b.clone(),
                sha: git::rev_parse(b)?,
                parent: meta::parent(b).unwrap_or_else(|| self.line.base.clone()),
                parent_sha: meta::parent_sha(b),
                queue: meta::branch_queue(b),
                pr: meta::pr(b),
                description: meta::description(b),
            });
        }
        Ok(Snapshot {
            branches,
            head: self.current.clone(),
        })
    }

    /// Restore a snapshot: delete branches the operation created, put every
    /// snapshotted branch's ref and metadata back, and return HEAD. Reloads.
    fn restore(&mut self, snap: &Snapshot) -> Result<()> {
        git::detach_head()?;
        let keep: HashSet<&str> = snap.branches.iter().map(|b| b.name.as_str()).collect();
        for b in self
            .line
            .boundaries
            .iter()
            .map(|x| x.name.clone())
            .collect::<Vec<_>>()
        {
            if !keep.contains(b.as_str()) {
                meta::untrack(&b);
                git::run(&["branch", "-q", "-D", &b])?;
            }
        }
        for bs in &snap.branches {
            if git::branch_exists(&bs.name) {
                git::force_ref(&bs.name, &bs.sha)?;
            } else {
                git::create_branch(&bs.name, &bs.sha)?;
            }
            meta::set_parent(&bs.name, &bs.parent)?;
            if let Some(s) = &bs.parent_sha {
                meta::set_parent_sha(&bs.name, s)?;
            }
            if let Some(q) = &bs.queue {
                meta::set_branch_queue(&bs.name, q)?;
            }
            if let Some(pr) = bs.pr {
                meta::set_pr(&bs.name, pr)?;
            }
            if let Some(d) = &bs.description {
                meta::set_description(&bs.name, d)?;
            }
        }
        git::checkout_quiet(&snap.head)?;
        self.reload()?;
        Ok(())
    }

    /// Rebuild the in-memory model from the current git state (after HEAD has
    /// been put on a valid line branch). Runs no guards.
    fn reload(&mut self) -> Result<()> {
        let queue = Queue::load()?;
        let branch = git::current_branch()?;
        let (branches, base) = Self::scope(&queue, &branch)?;
        self.line = Self::build_line(base, &branches)?;
        self.pr_cache = self.line.boundaries.iter().map(|b| meta::pr(&b.name)).collect();
        self.current = branch;
        Ok(())
    }
}

/// Build the interactive-rebase todo for `reorder(from, to)` over `line`.
///
/// Emits the picks in the current front → tip order with an `update-ref` line
/// after each **non-leaf** branch's tip (the leaf branch updates implicitly),
/// then relocates only the moved commit's `pick` line to directly follow the
/// commit that precedes its target position (or to the front). Moving just the
/// pick — and leaving every `update-ref` line where it sits — is what makes a
/// commit that crosses a boundary join the branch it lands in and shrink the
/// branch it left (mirrors the CLI's `reorder-todo`). Pure, so it is
/// unit-tested without a repo.
fn reorder_todo(line: &EditableLine, from: usize, to: usize) -> String {
    let commits = &line.commits;
    let pick = |i: usize| format!("pick {} {}", commits[i].sha, commits[i].subject);

    // `update-ref` after each non-leaf branch's tip commit.
    let last = line.boundaries.len() - 1;
    let mut ref_after: std::collections::HashMap<usize, &str> = std::collections::HashMap::new();
    for (bi, b) in line.boundaries.iter().enumerate() {
        if bi != last {
            ref_after.insert(b.end - 1, b.name.as_str());
        }
    }

    // The full todo in current order.
    let mut lines: Vec<String> = Vec::new();
    for i in 0..commits.len() {
        lines.push(pick(i));
        if let Some(name) = ref_after.get(&i) {
            lines.push(format!("update-ref refs/heads/{name}"));
        }
    }

    // Relocate the moved pick to follow the target's preceding commit.
    let moved_line = pick(from);
    lines.retain(|l| l != &moved_line);
    let mut order: Vec<usize> = (0..commits.len()).collect();
    let m = order.remove(from);
    order.insert(to, m);
    let idx = if to == 0 {
        0
    } else {
        let anchor = pick(order[to - 1]);
        lines
            .iter()
            .position(|l| *l == anchor)
            .map(|i| i + 1)
            .unwrap_or(0)
    };
    lines.insert(idx, moved_line);
    lines.join("\n") + "\n"
}

/// Build the rebase todo for `reword(index)`: the line's picks front → tip with
/// an `update-ref` after each non-leaf branch tip, and the target commit's line
/// marked `reword` so git stops to take the new message. Pure; unit-tested.
fn reword_todo(line: &EditableLine, index: usize) -> String {
    let commits = &line.commits;
    let last = line.boundaries.len() - 1;
    let mut ref_after: std::collections::HashMap<usize, &str> = std::collections::HashMap::new();
    for (bi, b) in line.boundaries.iter().enumerate() {
        if bi != last {
            ref_after.insert(b.end - 1, b.name.as_str());
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (i, c) in commits.iter().enumerate() {
        let verb = if i == index { "reword" } else { "pick" };
        lines.push(format!("{verb} {} {}", c.sha, c.subject));
        if let Some(name) = ref_after.get(&i) {
            lines.push(format!("update-ref refs/heads/{name}"));
        }
    }
    lines.join("\n") + "\n"
}

/// Build the rebase todo that drops the commits in `drop`: every other pick in
/// front → tip order, with the `update-ref` lines kept in place (so dropping a
/// branch's tip shrinks it, and dropping a branch's only commit empties it).
/// Pure; unit-tested.
fn drop_todo(line: &EditableLine, drop: &HashSet<usize>) -> String {
    let commits = &line.commits;
    let last = line.boundaries.len() - 1;
    let mut ref_after: std::collections::HashMap<usize, &str> = std::collections::HashMap::new();
    for (bi, b) in line.boundaries.iter().enumerate() {
        if bi != last {
            ref_after.insert(b.end - 1, b.name.as_str());
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (i, c) in commits.iter().enumerate() {
        if !drop.contains(&i) {
            lines.push(format!("pick {} {}", c.sha, c.subject));
        }
        if let Some(name) = ref_after.get(&i) {
            lines.push(format!("update-ref refs/heads/{name}"));
        }
    }
    lines.join("\n") + "\n"
}

/// Build the rebase todo for `squash(index)`: the line's picks front → tip with
/// the target marked `squash` so it folds into the previous commit. For a
/// cross-boundary squash the older branch's `update-ref` is deferred to *after*
/// the squash line, so that branch captures the combined commit (and the newer
/// branch may end up empty). Pure; unit-tested.
fn squash_todo(line: &EditableLine, index: usize) -> String {
    let commits = &line.commits;
    let last = line.boundaries.len() - 1;
    let mut ref_after: std::collections::HashMap<usize, &str> = std::collections::HashMap::new();
    for (bi, b) in line.boundaries.iter().enumerate() {
        if bi != last {
            ref_after.insert(b.end - 1, b.name.as_str());
        }
    }
    let cross = line.boundary_of(index - 1) != line.boundary_of(index);

    let mut lines: Vec<String> = Vec::new();
    for (i, c) in commits.iter().enumerate() {
        let verb = if i == index { "squash" } else { "pick" };
        lines.push(format!("{verb} {} {}", c.sha, c.subject));
        if let Some(name) = ref_after.get(&i) {
            // Defer the older branch's ref past the squash so it lands there.
            if !(cross && i == index - 1) {
                lines.push(format!("update-ref refs/heads/{name}"));
            }
        }
        if cross && i == index {
            if let Some(name) = ref_after.get(&(index - 1)) {
                lines.push(format!("update-ref refs/heads/{name}"));
            }
        }
    }
    lines.join("\n") + "\n"
}

/// A commit's message with its `Stable-Commit-Id` trailer lines removed and
/// surrounding whitespace trimmed — the human-authored description.
fn message_body(sha: &str) -> Result<String> {
    let prefix = format!("{}:", ident::TRAILER);
    let msg = git::commit_message(sha)?;
    Ok(msg
        .lines()
        .filter(|l| !l.trim_start().starts_with(&prefix))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string())
}

/// Combine two descriptions for a squash: the non-empty one when only one has
/// text, else both with a blank line between.
fn combine_bodies(older: &str, newer: &str) -> String {
    match (older.trim(), newer.trim()) {
        ("", n) => n.to_string(),
        (o, "") => o.to_string(),
        (o, n) => format!("{o}\n\n{n}"),
    }
}

/// The final reword message: the user's text with any `Stable-Commit-Id` lines
/// stripped, then the original id re-appended as the sole trailer — so the
/// change keeps its identity no matter how the user edited the pane. A commit
/// that had no id keeps none.
fn with_preserved_id(new_text: &str, id: Option<&str>) -> String {
    let prefix = format!("{}:", ident::TRAILER);
    let body = new_text
        .lines()
        .filter(|l| !l.trim_start().starts_with(&prefix))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_end();
    match id {
        Some(id) => format!("{body}\n\n{}: {id}\n", ident::TRAILER),
        None => format!("{body}\n"),
    }
}

/// The queue name a set of line branches belongs to: an explicit `queueName`
/// membership wins, else a `queue/<name>/…` prefix. `None` for a plain,
/// unnamed (e.g. provisional untracked) line.
fn branch_queue_name(branches: &[String]) -> Option<String> {
    for b in branches {
        if let Some(n) = meta::branch_queue(b) {
            return Some(n);
        }
    }
    for b in branches {
        if let Some(rest) = b.strip_prefix("queue/") {
            if let Some((n, _)) = rest.split_once('/') {
                return Some(n.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str) -> Commit {
        Commit {
            sha: subject.into(),
            id: None,
            subject: subject.into(),
            empty: false,
        }
    }

    /// Two branches: `a` owns commits 0,1; `b` owns commit 2.
    fn sample() -> EditableLine {
        EditableLine {
            base: "main".into(),
            commits: vec![commit("c0"), commit("c1"), commit("c2")],
            boundaries: vec![
                Boundary {
                    name: "a".into(),
                    end: 2,
                },
                Boundary {
                    name: "b".into(),
                    end: 3,
                },
            ],
        }
    }

    #[test]
    fn rows_interleave_branch_headers_front_to_tip() {
        let line = sample();
        assert_eq!(
            line.rows(),
            vec![
                Row::Branch { boundary: 0 },
                Row::Commit { index: 0 },
                Row::Commit { index: 1 },
                Row::Branch { boundary: 1 },
                Row::Commit { index: 2 },
            ]
        );
    }

    #[test]
    fn boundary_of_maps_commits_to_their_branch() {
        let line = sample();
        assert_eq!(line.boundary_of(0), 0);
        assert_eq!(line.boundary_of(1), 0);
        assert_eq!(line.boundary_of(2), 1);
    }

    #[test]
    fn reorder_todo_moves_a_pick_across_a_boundary_update_ref() {
        // a owns c0,c1 (update-ref after c1); b (leaf) owns c2.
        // Moving c1 (index 1) to index 2 relocates only its pick, leaving
        // `update-ref a` after c0 — so a shrinks to [c0] and c1 joins b.
        let todo = reorder_todo(&sample(), 1, 2);
        assert_eq!(
            todo,
            "pick c0 c0\nupdate-ref refs/heads/a\npick c2 c2\npick c1 c1\n"
        );
    }

    #[test]
    fn reorder_todo_to_the_front_puts_the_pick_first() {
        // Move the leaf's commit (index 2) to the front.
        let todo = reorder_todo(&sample(), 2, 0);
        assert_eq!(
            todo,
            "pick c2 c2\npick c0 c0\npick c1 c1\nupdate-ref refs/heads/a\n"
        );
    }

    #[test]
    fn drop_todo_omits_the_dropped_pick_and_keeps_update_refs() {
        // Dropping c1 (a's tip) leaves `update-ref a` after c0 — a shrinks.
        let mut drop = HashSet::new();
        drop.insert(1usize);
        let todo = drop_todo(&sample(), &drop);
        assert_eq!(
            todo,
            "pick c0 c0\nupdate-ref refs/heads/a\npick c2 c2\n"
        );
    }

    #[test]
    fn squash_todo_same_branch_folds_into_the_previous_pick() {
        // Squash c1 into c0 (both in a): a's update-ref stays after the squash.
        let todo = squash_todo(&sample(), 1);
        assert_eq!(
            todo,
            "pick c0 c0\nsquash c1 c1\nupdate-ref refs/heads/a\npick c2 c2\n"
        );
    }

    #[test]
    fn squash_todo_across_a_boundary_defers_the_older_ref() {
        // Squash c2 (b) into c1 (a's tip): a's update-ref moves past the squash
        // so a captures the combined commit; b (leaf) ends up empty.
        let todo = squash_todo(&sample(), 2);
        assert_eq!(
            todo,
            "pick c0 c0\npick c1 c1\nsquash c2 c2\nupdate-ref refs/heads/a\n"
        );
    }

    #[test]
    fn combine_bodies_picks_the_nonempty_side_or_joins() {
        assert_eq!(combine_bodies("older", "newer"), "older\n\nnewer");
        assert_eq!(combine_bodies("", "newer"), "newer");
        assert_eq!(combine_bodies("older", ""), "older");
    }

    #[test]
    fn parse_diff_numbers_selectable_lines() {
        let diff = [
            "diff --git a/f.txt b/f.txt",
            "--- a/f.txt",
            "+++ b/f.txt",
            "@@ -1,2 +1,2 @@",
            "-old",
            "+new",
            " ctx",
        ]
        .join("\n")
            + "\n";
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "f.txt");
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Removed);
        assert_eq!(lines[0].change_index, Some(0));
        assert_eq!(lines[1].kind, LineKind::Added);
        assert_eq!(lines[1].change_index, Some(1));
        assert_eq!(lines[2].kind, LineKind::Context);
        assert_eq!(lines[2].change_index, None);
    }

    #[test]
    fn reconstruct_middle_reverts_only_selected_changes() {
        let file = FileDiff {
            path: "f".into(),
            binary: false,
            hunks: vec![Hunk {
                v_start: 0,
                lines: vec![
                    HunkLine {
                        kind: LineKind::Added,
                        text: "A".into(),
                        change_index: Some(0),
                    },
                    HunkLine {
                        kind: LineKind::Added,
                        text: "B".into(),
                        change_index: Some(1),
                    },
                ],
            }],
        };
        // Peel B -> the middle keeps only A.
        assert_eq!(
            reconstruct_middle(&file, "A\nB\n", &HashSet::from([1])),
            Some("A\n".into())
        );
        // Peel A -> the middle keeps only B.
        assert_eq!(
            reconstruct_middle(&file, "A\nB\n", &HashSet::from([0])),
            Some("B\n".into())
        );
    }

    #[test]
    fn reword_todo_marks_the_target_and_keeps_the_rest() {
        let todo = reword_todo(&sample(), 0);
        assert_eq!(
            todo,
            "reword c0 c0\npick c1 c1\nupdate-ref refs/heads/a\npick c2 c2\n"
        );
    }

    #[test]
    fn with_preserved_id_reattaches_the_original_trailer() {
        // A fresh message gains the original id.
        assert_eq!(
            with_preserved_id("hello world", Some("q-abc")),
            "hello world\n\nStable-Commit-Id: q-abc\n"
        );
        // Any id line the user's text carried is replaced by the original.
        assert_eq!(
            with_preserved_id("subject\n\nStable-Commit-Id: q-typo", Some("q-abc")),
            "subject\n\nStable-Commit-Id: q-abc\n"
        );
        // An unstamped commit stays unstamped.
        assert_eq!(with_preserved_id("just text", None), "just text\n");
    }
}

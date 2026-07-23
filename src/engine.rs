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
use crate::queue::Queue;
use anyhow::{bail, Result};

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
    /// neighbour.
    Squash { index: usize },
    /// Split the commit at `index` into two pieces (line-level selection is
    /// carried by the view; repeatable for N pieces).
    Split { index: usize },
    /// Rewrite the message of the commit at `index`, preserving its
    /// Stable-Commit-Id.
    Reword { index: usize, message: String },
    /// Delete the commit at `index`.
    Delete { index: usize },
    /// Add a branch boundary after the commit at `index`, naming the new
    /// (front-ward) branch `name`.
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
    launched_from: String,
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
        if !git::worktree_clean() {
            bail!(
                "working tree has uncommitted changes; commit or stash them before \
                 opening the queue in the TUI"
            );
        }

        let queue = Queue::load()?;
        let branch = git::current_branch()?;

        // Scope: the current queue line. An untracked branch opens as one
        // provisional section over trunk, exactly as `edit` treats it.
        let (branches, base) = if queue.is_tracked(&branch) {
            let line = queue.line_through(&branch)?;
            // Refuse a forked line: rewriting commits shared with a sibling
            // line could orphan it, and forked-line editing is out of scope in
            // v1. A fork is any branch in the editing range with more than one
            // tracked child (covers a fork above *or* below the current
            // branch, which `line.fork_at` alone would miss).
            if line.branches.iter().any(|b| queue.children(b).len() > 1) {
                bail!(
                    "`{branch}` is on a forked queue line; the TUI cannot rewrite history \
                     shared with a sibling line. Use `git queue edit` for ref-only \
                     boundary changes, or checkout a single unforked line first"
                );
            }
            (line.branches, line.base)
        } else {
            (vec![branch.clone()], queue.trunk.clone())
        };

        // Every commit of the line, front → tip, recording where each branch's
        // run ends as a boundary.
        let mut commits = Vec::new();
        let mut boundaries = Vec::new();
        let mut parent = base.clone();
        for b in &branches {
            for (sha, id, subject) in git::commits_between_with_ids(&parent, b)? {
                commits.push(Commit { sha, id, subject });
            }
            boundaries.push(Boundary {
                name: b.clone(),
                end: commits.len(),
            });
            parent = b.clone();
        }

        if commits.is_empty() {
            bail!("the queue has no commits to edit");
        }

        Ok(Engine {
            line: EditableLine {
                base,
                commits,
                boundaries,
            },
            launched_from: branch,
        })
    }

    /// The loaded queue line: commit sequence and boundaries.
    pub fn line(&self) -> &EditableLine {
        &self.line
    }

    /// The branch the TUI was launched from — HEAD is returned here (or its
    /// nearest surviving neighbour) on exit.
    pub fn launched_from(&self) -> &str {
        &self.launched_from
    }

    /// Apply an operation to the line. No variant is handled yet — this ticket
    /// only establishes the seam; later tickets implement each in turn.
    pub fn apply(&mut self, op: Operation) -> Result<()> {
        bail!("operation not yet supported: {op:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str) -> Commit {
        Commit {
            sha: subject.into(),
            id: None,
            subject: subject.into(),
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
}

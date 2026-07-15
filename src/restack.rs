//! The restack engine: after a branch's commits change, move all descendant
//! branches (forks included) onto the new tip.
//!
//! Primary path is `git replay --contained` — one atomic, worktree-free
//! operation that rewrites the whole subtree. If replay can't apply cleanly
//! (a conflict), we fall back to a per-branch worktree rebase that *persists*
//! the conflict markers into the committed files (always succeeds) and flag the
//! affected branches loudly.

use crate::git::{self, Replayed};
use crate::meta;
use crate::stack::Stack;
use anyhow::Result;

#[derive(Default)]
pub struct Report {
    /// Branches that were moved.
    pub restacked: Vec<String>,
    /// Branches left holding persisted conflict markers.
    pub conflicted: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.restacked.is_empty()
    }
}

/// Restack every descendant of `changed` onto its current tip. Safe to call
/// when nothing is stale (it becomes a no-op).
pub fn propagate(stack: &Stack, changed: &str) -> Result<Report> {
    let new_tip = git::rev_parse(changed)?;
    let mut report = Report::default();
    let mut moved_head = false;

    // Handle each direct child's subtree independently — this makes forks at
    // `changed` correct, and each subtree's own forks are handled by
    // `--contained` within one replay.
    for child in stack.children(changed) {
        let anchor = match meta::parent_sha(&child) {
            Some(sha) => sha,
            None => git::merge_base(changed, &child)?,
        };
        if anchor == new_tip {
            continue; // already based on the current tip
        }

        let subtree = subtree(stack, &child);
        let leaves = stack.leaves_under(&child);
        let ranges: Vec<String> = leaves.iter().map(|l| format!("{anchor}..{l}")).collect();

        match git::replay_restack(&new_tip, &ranges)? {
            Replayed::Applied => {}
            Replayed::Failed(msg) => {
                eprintln!(
                    "note: clean replay of the `{child}` subtree failed ({}); \
                     falling back to a conflict-persisting rebase.",
                    msg.lines().next().unwrap_or("conflict")
                );
                fallback_rebase(stack, &subtree)?; // checks out branches → HEAD moves
                moved_head = true;
            }
        }

        // Refresh anchors to the new parent tips and detect persisted markers.
        for b in &subtree {
            if let Some(parent) = stack.parent_of(b) {
                let ptip = git::rev_parse(parent)?;
                meta::set_parent_sha(b, &ptip)?;
            }
            let has_markers = git::has_conflict_markers(b);
            meta::set_conflicted(b, has_markers)?;
            if has_markers {
                report.conflicted.push(b.clone());
            }
            report.restacked.push(b.clone());
        }
    }

    // The fallback rebase leaves HEAD on the last rebased branch; put it back.
    if moved_head {
        git::checkout_quiet(changed)?;
    }

    Ok(report)
}

/// Reconcile EVERY tracked branch onto its parent's current tip, bottom-up.
/// Catches any staleness — a moved trunk, a mid-stack branch that took on
/// remote commits, etc. Returns without restoring HEAD (the caller does that,
/// since a fallback rebase may have moved it).
pub fn restack_forest(stack: &Stack) -> Result<Report> {
    let mut report = Report::default();
    for b in stack.topo_order() {
        let parent = match stack.parent_of(&b) {
            Some(p) => p.to_string(),
            None => continue,
        };
        let ptip = git::rev_parse(&parent)?;
        let anchor = match meta::parent_sha(&b) {
            Some(sha) => sha,
            None => git::merge_base(&parent, &b)?,
        };
        if anchor == ptip {
            continue;
        }
        let ranges = vec![format!("{anchor}..{b}")];
        match git::replay_restack(&ptip, &ranges)? {
            Replayed::Applied => {}
            Replayed::Failed(msg) => {
                eprintln!(
                    "note: clean replay of `{b}` failed ({}); persisting conflict markers.",
                    msg.lines().next().unwrap_or("conflict")
                );
                git::rebase_persist(&ptip, &anchor, &b)?;
            }
        }
        meta::set_parent_sha(&b, &git::rev_parse(&parent)?)?;
        let has_markers = git::has_conflict_markers(&b);
        meta::set_conflicted(&b, has_markers)?;
        if has_markers {
            report.conflicted.push(b.clone());
        }
        report.restacked.push(b);
    }
    Ok(report)
}

/// Per-branch, marker-persisting rebase used when replay can't apply cleanly.
/// Processes bottom-up so each branch rebases onto its already-updated parent.
fn fallback_rebase(stack: &Stack, subtree_topo: &[String]) -> Result<()> {
    for b in subtree_topo {
        let parent = match stack.parent_of(b) {
            Some(p) => p,
            None => continue,
        };
        let ptip = git::rev_parse(parent)?;
        let anchor = match meta::parent_sha(b) {
            Some(sha) => sha,
            None => git::merge_base(parent, b)?,
        };
        if anchor == ptip {
            continue;
        }
        git::rebase_persist(&ptip, &anchor, b)?;
    }
    Ok(())
}

/// `child` and all its descendants, topologically ordered.
fn subtree(stack: &Stack, child: &str) -> Vec<String> {
    let mut v = vec![child.to_string()];
    v.extend(stack.descendants_topo(child));
    v
}

/// Print a loud, hard-to-miss warning that conflict markers were left behind.
pub fn warn_conflicts(conflicted: &[String]) {
    eprintln!("\n\x1b[1;33m╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  ⚠  CONFLICTS WERE PERSISTED AS MARKERS DURING RESTACK        ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝\x1b[0m");
    eprintln!("The following branches now contain <<<<<<< conflict markers that");
    eprintln!("were committed so the restack could finish. Fix them before submitting:");
    for b in conflicted {
        eprintln!("    • {b}");
    }
    eprintln!("Search for `<<<<<<<` on each branch, resolve, and commit.\n");
}

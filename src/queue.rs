//! The queue domain model, reconstructed from git-config metadata.
//!
//! Branches form a forest via parent pointers. A chain ends at its *base*: the
//! first untracked ancestor. That is usually trunk, but any branch can be a
//! base — a queue started on a release branch merges into that release branch.
//! A *queue line* is the linear chain from just above its base up to a leaf.
//! Numbered PRs and requeueing operate on queue lines.

use crate::meta;
use anyhow::{bail, Result};
use std::collections::HashMap;

const MAX_DEPTH: usize = 1000; // cycle guard

pub(crate) struct Queue {
    pub trunk: String,
    /// branch -> parent branch
    parents: HashMap<String, String>,
}

impl Queue {
    pub(crate) fn load() -> Result<Self> {
        let trunk = meta::trunk()?;
        let mut parents = HashMap::new();
        for b in meta::tracked_branches() {
            if let Some(p) = meta::parent(&b) {
                parents.insert(b, p);
            }
        }
        Ok(Self { trunk, parents })
    }

    pub(crate) fn is_tracked(&self, branch: &str) -> bool {
        self.parents.contains_key(branch)
    }

    pub(crate) fn parent_of(&self, branch: &str) -> Option<&str> {
        self.parents.get(branch).map(std::string::String::as_str)
    }

    /// Direct children of `branch`, sorted for deterministic output.
    pub(crate) fn children(&self, branch: &str) -> Vec<String> {
        let mut kids: Vec<String> = self
            .parents
            .iter()
            .filter(|(_, p)| p.as_str() == branch)
            .map(|(c, _)| c.clone())
            .collect();
        kids.sort();
        kids
    }

    /// Bottoms of all queues: tracked branches whose parent is untracked
    /// (i.e. sits directly on a base — trunk or otherwise), sorted.
    pub(crate) fn roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .parents
            .iter()
            .filter(|(_, p)| !self.parents.contains_key(*p))
            .map(|(b, _)| b.clone())
            .collect();
        roots.sort();
        roots
    }

    /// Tracked branches with no tracked children (tops of lines), sorted.
    pub(crate) fn leaves(&self) -> Vec<String> {
        let mut leaves: Vec<String> = self
            .parents
            .keys()
            .filter(|b| self.children(b).is_empty())
            .cloned()
            .collect();
        leaves.sort();
        leaves
    }

    /// Every distinct base branch (untracked parents of tracked branches),
    /// sorted and deduplicated.
    pub(crate) fn bases(&self) -> Vec<String> {
        let mut bases: Vec<String> = self
            .parents
            .values()
            .filter(|p| !self.parents.contains_key(*p))
            .cloned()
            .collect();
        bases.sort();
        bases.dedup();
        bases
    }

    /// Chain from just-above-the-base up to and including `branch`,
    /// bottom-first. The base is the first untracked ancestor. Errors on a
    /// cycle.
    pub(crate) fn chain_to_base(&self, branch: &str) -> Result<Vec<String>> {
        let mut chain = vec![branch.to_string()];
        let mut cur = branch.to_string();
        for _ in 0..MAX_DEPTH {
            match self.parents.get(&cur) {
                // An untracked parent is the line's base: stop below it.
                Some(p) if !self.parents.contains_key(p) => {
                    chain.reverse();
                    return Ok(chain);
                }
                Some(p) => {
                    chain.push(p.clone());
                    cur = p.clone();
                }
                // Only reachable on the first step: an untracked `branch`.
                None => bail!("`{branch}` is not a tracked queue branch"),
            }
        }
        bail!("parent chain for `{branch}` is too deep or cyclic");
    }

    /// The full linear queue line through `branch`, bottom-first, extending
    /// upward while each branch has exactly one child. Stops (without error) at
    /// the first fork; `fork_at` reports where, so callers can warn.
    pub(crate) fn line_through(&self, branch: &str) -> Result<Line> {
        let mut branches = self.chain_to_base(branch)?;
        let base = self
            .parent_of(&branches[0])
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("`{}` has no parent branch", branches[0]))?;
        let mut fork_at = None;
        // `branches` is never empty (chain_to_base returns at least one), so the
        // `while let` always enters; it breaks at a leaf or a fork.
        while let Some(top) = branches.last().cloned() {
            let kids = self.children(&top);
            match kids.len() {
                0 => break,
                1 => branches.extend(kids), // exactly one child
                _ => {
                    fork_at = Some(top);
                    break;
                }
            }
        }
        Ok(Line {
            branches,
            base,
            fork_at,
        })
    }

    /// All descendants of `branch` (children, grandchildren, ...), topologically
    /// ordered (parents before children). Excludes `branch` itself.
    pub(crate) fn descendants_topo(&self, branch: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut frontier = self.children(branch);
        while let Some(b) = frontier.pop() {
            out.push(b.clone());
            frontier.extend(self.children(&b));
        }
        out.sort_by_key(|b| (self.depth(b), b.clone()));
        out.dedup();
        out
    }

    /// Leaf branches at or under `branch` (no tracked children). If `branch`
    /// itself has no children, returns just `[branch]`.
    pub(crate) fn leaves_under(&self, branch: &str) -> Vec<String> {
        let mut leaves = Vec::new();
        let mut frontier = vec![branch.to_string()];
        while let Some(b) = frontier.pop() {
            let kids = self.children(&b);
            if kids.is_empty() {
                leaves.push(b);
            } else {
                frontier.extend(kids);
            }
        }
        leaves.sort();
        leaves.dedup();
        leaves
    }

    /// Depth of `branch` below trunk (trunk's direct children are depth 1).
    fn depth(&self, branch: &str) -> usize {
        self.chain_to_base(branch).map_or(usize::MAX, |c| c.len())
    }

    /// All tracked branches in topological order (parents before children).
    pub(crate) fn topo_order(&self) -> Vec<String> {
        let mut branches: Vec<String> = self.parents.keys().cloned().collect();
        branches.sort_by_key(|b| (self.depth(b), b.clone()));
        branches
    }
}

/// A linear queue line.
pub(crate) struct Line {
    /// Bottom-first branch names (excludes the base).
    pub branches: Vec<String>,
    /// The branch this line merges into: the bottom branch's (untracked)
    /// parent. Usually trunk, but any branch can be a base.
    pub base: String,
    /// Set if the line stopped early because a branch had multiple children.
    pub fork_at: Option<String>,
}

impl Line {
    /// The tip branch (last in the line). A line always has at least one
    /// branch, so this never panics.
    #[allow(clippy::unwrap_used)] // invariant: line_through returns ≥1 branch
    pub(crate) fn top(&self) -> &str {
        self.branches.last().unwrap()
    }
}

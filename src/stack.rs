//! The stack domain model, reconstructed from git-config metadata.
//!
//! Branches form a forest rooted at `trunk` via parent pointers. A *stack line*
//! is the linear chain from just above trunk up to a leaf. Numbered PRs and
//! restacking operate on stack lines.

use crate::meta;
use anyhow::{bail, Result};
use std::collections::HashMap;

const MAX_DEPTH: usize = 1000; // cycle guard

pub struct Stack {
    pub trunk: String,
    /// branch -> parent branch
    parents: HashMap<String, String>,
}

impl Stack {
    pub fn load() -> Result<Stack> {
        let trunk = meta::trunk()?;
        let mut parents = HashMap::new();
        for b in meta::tracked_branches() {
            if let Some(p) = meta::parent(&b) {
                parents.insert(b, p);
            }
        }
        Ok(Stack { trunk, parents })
    }

    pub fn is_tracked(&self, branch: &str) -> bool {
        self.parents.contains_key(branch)
    }

    pub fn parent_of(&self, branch: &str) -> Option<&str> {
        self.parents.get(branch).map(|s| s.as_str())
    }

    /// Direct children of `branch`, sorted for deterministic output.
    pub fn children(&self, branch: &str) -> Vec<String> {
        let mut kids: Vec<String> = self
            .parents
            .iter()
            .filter(|(_, p)| p.as_str() == branch)
            .map(|(c, _)| c.clone())
            .collect();
        kids.sort();
        kids
    }

    /// Branches directly on trunk (bottoms of stacks), sorted.
    pub fn roots(&self) -> Vec<String> {
        self.children(&self.trunk)
    }

    /// Chain from just-above-trunk up to and including `branch`, bottom-first.
    /// Errors on a cycle or a parent chain that never reaches trunk.
    pub fn downstack(&self, branch: &str) -> Result<Vec<String>> {
        let mut chain = vec![branch.to_string()];
        let mut cur = branch.to_string();
        for _ in 0..MAX_DEPTH {
            match self.parents.get(&cur) {
                Some(p) if *p == self.trunk => {
                    chain.reverse();
                    return Ok(chain);
                }
                Some(p) => {
                    chain.push(p.clone());
                    cur = p.clone();
                }
                None => bail!(
                    "branch `{branch}` is not connected to trunk `{}` (missing parent for `{cur}`)",
                    self.trunk
                ),
            }
        }
        bail!("parent chain for `{branch}` is too deep or cyclic");
    }

    /// The full linear stack line through `branch`, bottom-first, extending
    /// upward while each branch has exactly one child. Stops (without error) at
    /// the first fork; `fork_at` reports where, so callers can warn.
    pub fn line_through(&self, branch: &str) -> Result<Line> {
        let mut branches = self.downstack(branch)?;
        let mut fork_at = None;
        loop {
            let top = branches.last().unwrap().clone();
            let kids = self.children(&top);
            match kids.len() {
                0 => break,
                1 => branches.push(kids.into_iter().next().unwrap()),
                _ => {
                    fork_at = Some(top);
                    break;
                }
            }
        }
        Ok(Line { branches, fork_at })
    }

    /// All descendants of `branch` (children, grandchildren, ...), topologically
    /// ordered (parents before children). Excludes `branch` itself.
    pub fn descendants_topo(&self, branch: &str) -> Vec<String> {
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
    pub fn leaves_under(&self, branch: &str) -> Vec<String> {
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
        self.downstack(branch)
            .map(|c| c.len())
            .unwrap_or(usize::MAX)
    }

    /// All tracked branches in topological order (parents before children).
    pub fn topo_order(&self) -> Vec<String> {
        let mut branches: Vec<String> = self.parents.keys().cloned().collect();
        branches.sort_by_key(|b| (self.depth(b), b.clone()));
        branches
    }
}

/// A linear stack line.
pub struct Line {
    /// Bottom-first branch names (excludes trunk).
    pub branches: Vec<String>,
    /// Set if the line stopped early because a branch had multiple children.
    pub fork_at: Option<String>,
}

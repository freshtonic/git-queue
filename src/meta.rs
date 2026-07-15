//! Stack metadata, persisted in the repo's own git config.
//!
//! Keys:
//!   stack.trunk                       -> trunk branch name
//!   stack.remote                      -> remote name (default "origin")
//!   branch.<name>.stackParent         -> parent branch of <name>
//!   branch.<name>.stackParentSha      -> parent tip when last (re)based; the
//!                                        rebase anchor used by `sync`
//!   branch.<name>.stackPr             -> cached PR number

use crate::git;
use anyhow::{bail, Result};

fn config_get(key: &str) -> Option<String> {
    git::out(&["config", "--local", "--get", key])
        .ok()
        .filter(|s| !s.is_empty())
}

fn config_set(key: &str, value: &str) -> Result<()> {
    git::run(&["config", "--local", key, value])
}

fn config_unset(key: &str) {
    // Ignore errors: unsetting a missing key is fine.
    let _ = git::ok(&["config", "--local", "--unset", key]);
}

pub fn remote() -> String {
    config_get("stack.remote").unwrap_or_else(|| "origin".to_string())
}

/// Configured trunk, or a best-effort detection of `main`/`master`.
pub fn trunk() -> Result<String> {
    if let Some(t) = config_get("stack.trunk") {
        return Ok(t);
    }
    for candidate in ["main", "master"] {
        if git::branch_exists(candidate) {
            return Ok(candidate.to_string());
        }
    }
    bail!("no trunk configured and neither `main` nor `master` exists; run `git stack init --trunk <branch>`");
}

pub fn set_trunk(name: &str) -> Result<()> {
    config_set("stack.trunk", name)
}

fn parent_key(branch: &str) -> String {
    format!("branch.{branch}.stackParent")
}
fn parent_sha_key(branch: &str) -> String {
    format!("branch.{branch}.stackParentSha")
}
fn pr_key(branch: &str) -> String {
    format!("branch.{branch}.stackPr")
}

pub fn parent(branch: &str) -> Option<String> {
    config_get(&parent_key(branch))
}

pub fn set_parent(branch: &str, parent: &str) -> Result<()> {
    config_set(&parent_key(branch), parent)
}

pub fn parent_sha(branch: &str) -> Option<String> {
    config_get(&parent_sha_key(branch))
}

pub fn set_parent_sha(branch: &str, sha: &str) -> Result<()> {
    config_set(&parent_sha_key(branch), sha)
}

pub fn pr(branch: &str) -> Option<u64> {
    config_get(&pr_key(branch)).and_then(|s| s.parse().ok())
}

pub fn set_pr(branch: &str, number: u64) -> Result<()> {
    config_set(&pr_key(branch), &number.to_string())
}

pub fn untrack(branch: &str) {
    config_unset(&parent_key(branch));
    config_unset(&parent_sha_key(branch));
    config_unset(&pr_key(branch));
}

/// All branches that have a `stackParent` recorded.
pub fn tracked_branches() -> Vec<String> {
    // `--get-regexp` matches against canonical (lower-cased variable) key names.
    let raw = match git::out(&[
        "config",
        "--local",
        "--get-regexp",
        r"^branch\..*\.stackparent$",
    ]) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter_map(|line| {
            // line: `branch.<name>.stackparent <parent>`
            let key = line.split_whitespace().next()?;
            let inner = key.strip_prefix("branch.")?;
            let name = inner.strip_suffix(".stackparent")?;
            Some(name.to_string())
        })
        .collect()
}

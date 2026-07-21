//! Queue metadata, persisted in the repo's own git config.
//!
//! Keys:
//!   queue.trunk                       -> trunk branch name
//!   queue.remote                      -> remote name (default "origin")
//!   queue.gate                        -> merge-order gate mode
//!   queue.<qname>.description         -> the queue's description
//!   queue.<qname>.createdat           -> epoch seconds, set once
//!   queue.<qname>.modifiedat          -> epoch seconds, touched by queue ops
//!   branch.<name>.queueParent         -> parent branch of <name>
//!   branch.<name>.queueParentSha      -> parent tip when last (re)based; the
//!                                        rebase anchor used by `sync`
//!   branch.<name>.queuePr             -> cached PR number
//!   branch.<name>.queueName           -> name of the queue this branch is in
//!   branch.<name>.queueDescription    -> branch text set by `describe-branch`

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
    config_get("queue.remote").unwrap_or_else(|| "origin".to_string())
}

/// Merge-order gate mode: `Some("status")` once `git queue protect` has
/// enabled it; `None` means no gating.
pub fn gate() -> Option<String> {
    config_get("queue.gate")
}

pub fn set_gate(mode: &str) -> Result<()> {
    config_set("queue.gate", mode)
}

/// Configured trunk, or a best-effort detection of `main`/`master`.
pub fn trunk() -> Result<String> {
    if let Some(t) = config_get("queue.trunk") {
        return Ok(t);
    }
    for candidate in ["main", "master"] {
        if git::branch_exists(candidate) {
            return Ok(candidate.to_string());
        }
    }
    bail!("no trunk configured and neither `main` nor `master` exists; run `git queue init --trunk <branch>`");
}

pub fn set_trunk(name: &str) -> Result<()> {
    config_set("queue.trunk", name)
}

fn parent_key(branch: &str) -> String {
    format!("branch.{branch}.queueParent")
}
fn parent_sha_key(branch: &str) -> String {
    format!("branch.{branch}.queueParentSha")
}
fn pr_key(branch: &str) -> String {
    format!("branch.{branch}.queuePr")
}
fn description_key(branch: &str) -> String {
    format!("branch.{branch}.queueDescription")
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

/// The user-authored description of what this branch/PR is about.
pub fn description(branch: &str) -> Option<String> {
    config_get(&description_key(branch))
}

pub fn set_description(branch: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        config_unset(&description_key(branch));
        Ok(())
    } else {
        config_set(&description_key(branch), text)
    }
}

pub fn untrack(branch: &str) {
    config_unset(&parent_key(branch));
    config_unset(&parent_sha_key(branch));
    config_unset(&pr_key(branch));
    config_unset(&description_key(branch));
    config_unset(&queue_name_key(branch));
}

/// Detached queue-editing state, set by `git queue checkout <commit>`:
/// the commit HEAD was placed on, and the top branch of its line.
pub fn detached_state() -> Option<(String, String)> {
    Some((
        config_get("queue.detachedoriginal")?,
        config_get("queue.detachedtop")?,
    ))
}

pub fn set_detached_state(original: &str, top: &str) -> Result<()> {
    config_set("queue.detachedoriginal", original)?;
    config_set("queue.detachedtop", top)
}

pub fn clear_detached_state() {
    config_unset("queue.detachedoriginal");
    config_unset("queue.detachedtop");
}

fn queue_name_key(branch: &str) -> String {
    format!("branch.{branch}.queueName")
}

/// The named queue this branch belongs to, if recorded.
pub fn branch_queue(branch: &str) -> Option<String> {
    config_get(&queue_name_key(branch))
}

pub fn set_branch_queue(branch: &str, queue: &str) -> Result<()> {
    config_set(&queue_name_key(branch), queue)
}

/// Queue names must be usable inside branch names and config subsections.
pub fn validate_queue_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("queue name `{name}` is invalid: use letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

pub fn queue_description(queue: &str) -> Option<String> {
    config_get(&format!("queue.{queue}.description"))
}

pub fn set_queue_description(queue: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        config_unset(&format!("queue.{queue}.description"));
        Ok(())
    } else {
        config_set(&format!("queue.{queue}.description"), text)
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record activity on a queue: sets createdAt once, bumps modifiedAt.
pub fn touch_queue(queue: &str) {
    let created = format!("queue.{queue}.createdat");
    if config_get(&created).is_none() {
        let _ = config_set(&created, &now_epoch().to_string());
    }
    let _ = config_set(
        &format!("queue.{queue}.modifiedat"),
        &now_epoch().to_string(),
    );
}

/// Last-activity time of a queue (modifiedAt, falling back to createdAt).
pub fn queue_touched_at(queue: &str) -> u64 {
    config_get(&format!("queue.{queue}.modifiedat"))
        .or_else(|| config_get(&format!("queue.{queue}.createdat")))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Every queue name that has metadata or a member branch.
pub fn all_queue_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(raw) = git::out(&["config", "--local", "--get-regexp", r"^queue\..+\..+$"]) {
        for line in raw.lines() {
            if let Some(key) = line.split_whitespace().next() {
                let parts: Vec<&str> = key.splitn(3, '.').collect();
                if parts.len() == 3 && parts[0] == "queue" {
                    names.push(parts[1].to_string());
                }
            }
        }
    }
    if let Ok(raw) = git::out(&[
        "config",
        "--local",
        "--get-regexp",
        r"^branch\..*\.queuename$",
    ]) {
        for line in raw.lines() {
            if let Some(name) = line.split_whitespace().nth(1) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// All branches that have a `queueParent` recorded.
pub fn tracked_branches() -> Vec<String> {
    // `--get-regexp` matches against canonical (lower-cased variable) key names.
    let raw = match git::out(&[
        "config",
        "--local",
        "--get-regexp",
        r"^branch\..*\.queueparent$",
    ]) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter_map(|line| {
            // line: `branch.<name>.queueparent <parent>`
            let key = line.split_whitespace().next()?;
            let inner = key.strip_prefix("branch.")?;
            let name = inner.strip_suffix(".queueparent")?;
            Some(name.to_string())
        })
        .collect()
}

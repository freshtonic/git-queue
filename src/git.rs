//! Thin wrappers over the `git` executable.
//!
//! We shell out rather than link a git library: rebase/conflict semantics are
//! then exactly what the user would get by hand, and the dependency surface
//! stays tiny.

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Run `git <args>` and capture trimmed stdout. Errors if git exits non-zero.
pub fn out(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `git <args>` inheriting stdio, so progress (rebase, push) is visible.
/// Errors if git exits non-zero.
pub fn run(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` exited with {}", args.join(" "), status);
    }
    Ok(())
}

/// Run `git <args>`, returning whether it exited zero. Never errors on
/// non-zero (used for boolean probes like `merge-base --is-ancestor`).
pub fn ok(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fail early with a friendly message if we are not inside a git work tree.
pub fn ensure_repo() -> Result<()> {
    if !ok(&["rev-parse", "--git-dir"]) {
        bail!("not inside a git repository (run this from within your repo)");
    }
    Ok(())
}

pub fn current_branch() -> Result<String> {
    let b = out(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if b == "HEAD" {
        bail!("you are in a detached HEAD state; check out a branch first");
    }
    Ok(b)
}

pub fn rev_parse(rev: &str) -> Result<String> {
    out(&["rev-parse", "--verify", "--quiet", rev])
        .map_err(|_| anyhow!("cannot resolve revision `{rev}`"))
}

pub fn branch_exists(name: &str) -> bool {
    ok(&[
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{name}"),
    ])
}

/// Is `ancestor` an ancestor of `descendant`?
pub fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    ok(&["merge-base", "--is-ancestor", ancestor, descendant])
}

pub fn merge_base(a: &str, b: &str) -> Result<String> {
    out(&["merge-base", a, b])
}

pub fn checkout(branch: &str) -> Result<()> {
    run(&["checkout", branch])
}

/// Create `name` at `start_point` without checking it out.
pub fn create_branch(name: &str, start_point: &str) -> Result<()> {
    run(&["branch", name, start_point])
}

/// Subject line of the tip commit of `branch`.
pub fn tip_subject(branch: &str) -> Result<String> {
    out(&["log", "-1", "--format=%s", branch])
}

/// Number of commits in `base..branch` (i.e. unique to `branch`).
pub fn ahead_count(base: &str, branch: &str) -> Result<usize> {
    let s = out(&["rev-list", "--count", &format!("{base}..{branch}")])?;
    Ok(s.parse().unwrap_or(0))
}

/// True if a rebase (merge or apply backend) is currently in progress.
pub fn rebase_in_progress() -> bool {
    let dir = match out(&["rev-parse", "--git-dir"]) {
        Ok(d) => PathBuf::from(d),
        Err(_) => return false,
    };
    dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists()
}

/// `git rebase --onto <new_base> <upstream> <branch>`.
pub fn rebase_onto(new_base: &str, upstream: &str, branch: &str) -> Result<()> {
    run(&["rebase", "--onto", new_base, upstream, branch])
}

pub fn fetch(remote: &str) -> Result<()> {
    run(&["fetch", remote])
}

/// Force-with-lease push, setting upstream. Shows git's own progress output.
pub fn push(remote: &str, branch: &str) -> Result<()> {
    run(&["push", "--force-with-lease", "-u", remote, branch])
}

/// Move a branch ref to `sha` without checking it out.
pub fn force_ref(branch: &str, sha: &str) -> Result<()> {
    run(&["update-ref", &format!("refs/heads/{branch}"), sha])
}

/// The remote-tracking ref for the trunk, e.g. `origin/main`, if it exists.
pub fn remote_trunk(remote: &str, trunk: &str) -> Option<String> {
    let r = format!("{remote}/{trunk}");
    if ok(&[
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/remotes/{r}"),
    ]) {
        Some(r)
    } else {
        None
    }
}

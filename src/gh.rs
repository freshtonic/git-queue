//! Wrappers over the GitHub `gh` CLI for pull-request management.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // title/base are fetched for completeness; not all are read yet
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String, // OPEN | CLOSED | MERGED
    #[serde(rename = "baseRefName")]
    pub base: String,
    /// APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED | null
    #[serde(rename = "reviewDecision", default)]
    pub review_decision: Option<String>,
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh").args(args).output().with_context(|| {
        format!(
            "failed to spawn `gh {}` (is the GitHub CLI installed?)",
            args.join(" ")
        )
    })?;
    if !output.status.success() {
        bail!(
            "`gh {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// True if `gh` is installed and authenticated.
pub fn ready() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Find an existing PR whose head is `branch` (any state), if any.
pub fn find(branch: &str) -> Result<Option<Pr>> {
    let json = gh(&[
        "pr",
        "list",
        "--head",
        branch,
        "--state",
        "all",
        "--limit",
        "1",
        "--json",
        "number,title,url,state,baseRefName,reviewDecision",
    ])?;
    let mut prs: Vec<Pr> = serde_json::from_str(&json).context("parsing gh pr list output")?;
    Ok(prs.pop())
}

/// Create a PR for `head` targeting `base`. Returns the new PR's number.
pub fn create(head: &str, base: &str, title: &str, body: &str, draft: bool) -> Result<u64> {
    let mut args = vec![
        "pr", "create", "--head", head, "--base", base, "--title", title, "--body", body,
    ];
    if draft {
        args.push("--draft");
    }
    // gh prints the new PR URL on stdout; re-query for the canonical number.
    gh(&args)?;
    match find(head)? {
        Some(pr) => Ok(pr.number),
        None => bail!("created a PR for `{head}` but could not read it back"),
    }
}

/// Close a PR without merging it.
pub fn close(number: u64) -> Result<()> {
    gh(&["pr", "close", &number.to_string()])?;
    Ok(())
}

/// Reopen a closed PR. Fails if it can't be reopened (e.g. its base was deleted).
pub fn reopen(number: u64) -> Result<()> {
    gh(&["pr", "reopen", &number.to_string()])?;
    Ok(())
}

/// Update an existing PR's base, title and body.
pub fn edit(number: u64, base: &str, title: &str, body: &str) -> Result<()> {
    let num = number.to_string();
    gh(&[
        "pr", "edit", &num, "--base", base, "--title", title, "--body", body,
    ])?;
    Ok(())
}

/// Post a commit status on `sha` (the merge-order gate). `gh api` resolves the
/// `{owner}/{repo}` placeholders from the current repository's remote.
pub fn set_commit_status(
    sha: &str,
    context: &str,
    success: bool,
    description: &str,
    target_url: Option<&str>,
) -> Result<()> {
    let path = format!("repos/{{owner}}/{{repo}}/statuses/{sha}");
    let state = format!("state={}", if success { "success" } else { "failure" });
    let context = format!("context={context}");
    let description = format!("description={description}");
    let mut args = vec![
        "api",
        "--silent",
        &path,
        "-f",
        &state,
        "-f",
        &context,
        "-f",
        &description,
    ];
    let url;
    if let Some(u) = target_url {
        url = format!("target_url={u}");
        args.extend(["-f", &url]);
    }
    gh(&args)?;
    Ok(())
}

//! Implementations of each `git stack` subcommand.

use crate::render::{self, Entry, PrRef};
use crate::stack::Stack;
use crate::{gh, git, meta, restack};
use anyhow::{bail, Result};

/// `git stack init [--trunk <branch>]`
pub fn init(trunk: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = match trunk {
        Some(t) => t,
        None => meta::trunk()?, // detect main/master
    };
    if !git::branch_exists(&trunk) {
        bail!("trunk branch `{trunk}` does not exist");
    }
    meta::set_trunk(&trunk)?;
    println!("Initialized git-stack. Trunk is `{trunk}`.");
    println!("Create your first stacked branch with:  git stack create <name>");
    Ok(())
}

/// `git stack create <name>` — new branch on top of the current one.
pub fn create(name: &str) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    if git::branch_exists(name) {
        bail!("branch `{name}` already exists");
    }
    let parent = git::current_branch()?;
    let parent_sha = git::rev_parse(&parent)?;

    git::create_branch(name, &parent)?;
    meta::set_parent(name, &parent)?;
    meta::set_parent_sha(name, &parent_sha)?;
    git::checkout(name)?;

    if parent == trunk {
        println!("Created `{name}` on trunk `{trunk}`. It is the bottom of a new stack.");
    } else {
        println!("Created `{name}` on top of `{parent}`.");
    }
    println!("Make your commits, then `git stack submit` to open PRs.");
    Ok(())
}

/// `git stack track [--parent <branch>]` — adopt the current branch.
pub fn track(parent: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    let branch = git::current_branch()?;
    if branch == trunk {
        bail!("cannot track the trunk branch itself");
    }
    let parent = match parent {
        Some(p) => p,
        None => trunk.clone(),
    };
    if !git::branch_exists(&parent) {
        bail!("parent branch `{parent}` does not exist");
    }
    if !git::is_ancestor(&parent, &branch) {
        bail!(
            "`{parent}` is not an ancestor of `{branch}`; pass the correct --parent or rebase first"
        );
    }
    let base = git::merge_base(&parent, &branch)?;
    meta::set_parent(&branch, &parent)?;
    meta::set_parent_sha(&branch, &base)?;
    println!("Tracking `{branch}` with parent `{parent}`.");
    Ok(())
}

/// `git stack untrack` — forget the current branch's stack metadata.
pub fn untrack() -> Result<()> {
    git::ensure_repo()?;
    let branch = git::current_branch()?;
    meta::untrack(&branch);
    println!("Stopped tracking `{branch}`.");
    Ok(())
}

/// `git stack status`
pub fn status() -> Result<()> {
    git::ensure_repo()?;
    let stack = Stack::load()?;
    let current = git::current_branch()?;

    // Choose a line to show: the current branch's, or the first root's.
    let anchor = if stack.is_tracked(&current) {
        current.clone()
    } else if current == stack.trunk {
        match stack.roots().into_iter().next() {
            Some(r) => r,
            None => {
                println!("No stacks yet. Create one with `git stack create <name>`.");
                return Ok(());
            }
        }
    } else {
        println!("Branch `{current}` is not tracked. Adopt it with `git stack track`.");
        return Ok(());
    };

    let line = stack.line_through(&anchor)?;
    let entries = build_entries(&line.branches)?;
    let fork = line.fork_at.as_deref();
    print!(
        "{}",
        render::status_tree(&entries, &current, &stack.trunk, fork)
    );
    Ok(())
}

/// `git stack prev` / `down` — check out the parent branch.
pub fn prev() -> Result<()> {
    git::ensure_repo()?;
    let branch = git::current_branch()?;
    match meta::parent(&branch) {
        Some(p) => {
            git::checkout(&p)?;
            Ok(())
        }
        None => bail!("`{branch}` has no tracked parent"),
    }
}

/// `git stack next` / `up` — check out the child branch.
pub fn next() -> Result<()> {
    git::ensure_repo()?;
    let stack = Stack::load()?;
    let branch = git::current_branch()?;
    let kids = stack.children(&branch);
    match kids.len() {
        0 => bail!("`{branch}` is at the top of its stack (no children)"),
        1 => git::checkout(&kids[0]),
        _ => {
            let list = kids.join("\n  ");
            bail!("`{branch}` has multiple children; check one out directly:\n  {list}")
        }
    }
}

/// `git stack sync [--no-push]` — pull in commits others pushed to stack
/// branches, restack the whole stack onto the latest trunk, and (by default)
/// push every branch back with `--force-with-lease` so remote work is never
/// clobbered.
pub fn sync(no_push: bool) -> Result<()> {
    git::ensure_repo()?;
    if git::rebase_in_progress() {
        bail!("a rebase is already in progress; finish it (`git rebase --continue`/`--abort`) then re-run `git stack sync`");
    }
    std::env::set_var(git::GUARD_ENV, "1"); // suppress our hooks during internal git ops
    let stack = Stack::load()?;
    let remote = meta::remote();
    let original = git::current_branch()?;

    println!("Fetching `{remote}`...");
    if let Err(e) = git::fetch(&remote) {
        eprintln!("warning: fetch failed; syncing against local refs only: {e}");
    }

    // Bring local trunk up to the remote trunk tip.
    if let Some(remote_trunk) = git::remote_trunk(&remote, &stack.trunk) {
        let tip = git::rev_parse(&remote_trunk)?;
        if original == stack.trunk {
            let _ = git::merge_ff_only(&remote_trunk);
        } else {
            let _ = git::force_ref(&stack.trunk, &tip);
        }
    }

    // Pull in commits teammates pushed to our stack branches (bottom-up).
    for branch in stack.topo_order() {
        match incorporate_remote(&branch, &remote, &original)? {
            Some(RemoteAction::FastForwarded) => {
                println!("Pulled remote commits into `{branch}` (fast-forward).")
            }
            Some(RemoteAction::Rebased) => {
                println!("Merged your local commits on top of remote `{branch}`.")
            }
            None => {}
        }
    }

    // Reconcile the whole forest onto updated parents (new engine).
    let report = restack::restack_forest(&stack)?;

    // Return to where we started before pushing.
    git::checkout_quiet(&original)?;

    // Push every branch back, with lease, unless asked not to.
    if !no_push {
        for branch in stack.topo_order() {
            println!("Pushing `{branch}`...");
            git::push(&remote, &branch)?;
        }
    }

    if !report.conflicted.is_empty() {
        restack::warn_conflicts(&report.conflicted);
    } else {
        println!("Stack is in sync with `{}`.", stack.trunk);
    }
    Ok(())
}

enum RemoteAction {
    FastForwarded,
    Rebased,
}

/// Integrate `origin/<branch>` into the local branch, if the remote has commits
/// we don't. Fast-forwards when we have nothing unique; otherwise replays our
/// unique commits onto the remote tip (patch-id dedup, conflict markers
/// persisted). Returns what it did, or `None` if there was nothing to pull.
fn incorporate_remote(branch: &str, remote: &str, current: &str) -> Result<Option<RemoteAction>> {
    let remote_sha = match git::remote_branch(remote, branch) {
        Some(sha) => sha,
        None => return Ok(None), // branch not on the remote yet
    };
    let local_sha = git::rev_parse(branch)?;
    if local_sha == remote_sha {
        return Ok(None);
    }
    if git::is_ancestor(&remote_sha, &local_sha) {
        return Ok(None); // we're ahead; nothing to pull
    }
    if git::is_ancestor(&local_sha, &remote_sha) {
        // Remote strictly ahead: fast-forward.
        if branch == current {
            git::merge_ff_only(&format!("{remote}/{branch}"))?;
        } else {
            git::force_ref(branch, &remote_sha)?;
        }
        return Ok(Some(RemoteAction::FastForwarded));
    }
    // Diverged: replay our unique commits onto the remote tip.
    let base = git::merge_base(&format!("{remote}/{branch}"), branch)?;
    git::rebase_persist(&remote_sha, &base, branch)?;
    Ok(Some(RemoteAction::Rebased))
}

/// `git stack submit [--draft]` — push the current stack line and open/update
/// its numbered PRs.
pub fn submit(draft: bool) -> Result<()> {
    git::ensure_repo()?;
    if !gh::ready() {
        bail!("`gh` is not installed or not authenticated; run `gh auth login`");
    }
    let stack = Stack::load()?;
    let current = git::current_branch()?;
    if !stack.is_tracked(&current) {
        bail!("`{current}` is not tracked; run `git stack create` or `git stack track` first");
    }
    let line = stack.line_through(&current)?;
    if let Some(fork) = &line.fork_at {
        eprintln!("warning: `{fork}` has multiple children; submitting only this line.");
    }
    let branches = &line.branches;
    let total = branches.len();

    // Guard: don't open an empty PR.
    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        if git::ahead_count(&base, b)? == 0 {
            bail!("`{b}` has no commits beyond `{base}`; add a commit before submitting");
        }
    }

    // Pass 1: push every branch (bottom-first so bases exist), then
    // create-or-find its PR to learn the number.
    let remote = meta::remote();
    let mut prs: Vec<Option<PrRef>> = vec![None; total];
    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        println!("Pushing `{b}`...");
        git::push(&remote, b)?;

        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);

        let number = match gh::find(b)? {
            Some(pr) => pr.number,
            None => {
                // Temporary body; the real nav block is written in pass 2.
                gh::create(b, &base, &title, "Opening…", draft)?
            }
        };
        meta::set_pr(b, number)?;
    }

    // Re-read PR metadata now that all exist (numbers, urls, states).
    for (i, b) in branches.iter().enumerate() {
        if let Some(pr) = gh::find(b)? {
            prs[i] = Some(PrRef {
                number: pr.number,
                url: pr.url,
                state: pr.state,
            });
        }
    }

    // Pass 2: write correct base, numbered title and shared nav block on each.
    let entries: Vec<Entry> = branches
        .iter()
        .enumerate()
        .map(|(i, b)| Entry {
            branch: b.clone(),
            pr: prs[i].clone(),
            conflicted: meta::conflicted(b),
        })
        .collect();

    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        let number = match &prs[i] {
            Some(p) => p.number,
            None => continue,
        };
        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);
        let nav = render::nav_block(&entries, b, &stack.trunk);
        let body = render::compose_body("", &nav);
        gh::edit(number, &base, &title, &body)?;
    }

    println!("\nSubmitted {total} PR(s):");
    for (i, b) in branches.iter().enumerate() {
        if let Some(p) = &prs[i] {
            println!("  [{}/{}] {}  {}", i + 1, total, b, p.url);
        }
    }
    Ok(())
}

/// `git stack commit [-m <msg>]` — make a NEW commit on the current branch,
/// then restack all descendants onto the new tip (`git replay`, with a
/// marker-persisting fallback on conflict).
pub fn commit(message: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    // Suppress our own hooks for the internal git calls; we restack explicitly.
    std::env::set_var(git::GUARD_ENV, "1");
    let stack = Stack::load()?;
    let current = git::current_branch()?;

    git::commit(message.as_deref())?;

    if !stack.is_tracked(&current) {
        return Ok(()); // not a stack branch; just a normal commit
    }
    let report = restack::propagate(&stack, &current)?;
    finish_restack(&report, &current);
    Ok(())
}

/// `git stack amend` — fold STAGED changes into the current branch's tip commit
/// via `git history fixup`, atomically updating every descendant.
pub fn amend() -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let current = git::current_branch()?;
    if !git::staged_changes() {
        bail!("nothing staged — `git add` the changes you want to fold into `{current}` first");
    }
    if git::history_fixup("HEAD")? {
        bail!(
            "amend aborted: folding these changes into `{current}` would conflict with a descendant.\n\
             Nothing was changed. Either resolve on the descendant first, or use `git stack commit` to add a separate commit."
        );
    }
    refresh_descendant_anchors(&current)?;
    println!("Amended the tip commit of `{current}` and updated all descendants.");
    Ok(())
}

/// `git stack reword [<commit>]` — rewrite a commit message via
/// `git history reword`, atomically updating descendants. Defaults to HEAD.
pub fn reword(commit: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let current = git::current_branch()?;
    let target = commit.unwrap_or_else(|| "HEAD".to_string());
    if git::history_reword(&target)? {
        bail!("reword aborted (rewriting `{target}` would conflict with a descendant); nothing changed");
    }
    refresh_descendant_anchors(&current)?;
    println!("Reworded `{target}` and updated descendants of `{current}`.");
    Ok(())
}

/// `git stack restack` — restack descendants of the current branch onto its
/// current tip. `--auto` (used by hooks) stays quiet when there's nothing to do
/// and on non-stack branches.
pub fn restack(auto: bool) -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let stack = Stack::load()?;
    let current = git::current_branch()?;
    if !stack.is_tracked(&current) && current != stack.trunk {
        if auto {
            return Ok(());
        }
        bail!("`{current}` is not part of a stack");
    }
    let report = restack::propagate(&stack, &current)?;
    if auto && report.is_empty() && report.conflicted.is_empty() {
        return Ok(());
    }
    finish_restack(&report, &current);
    Ok(())
}

/// Report the outcome of a restack (loud warning if markers were persisted).
fn finish_restack(report: &restack::Report, branch: &str) {
    if !report.conflicted.is_empty() {
        restack::warn_conflicts(&report.conflicted);
    } else if !report.is_empty() {
        println!(
            "Restacked {} descendant branch(es) of `{branch}`.",
            report.restacked.len()
        );
    }
}

/// After git-history rewrote `branch`'s history, its descendants' stored parent
/// anchors are stale; refresh them to the new parent tips.
fn refresh_descendant_anchors(branch: &str) -> Result<()> {
    let stack = Stack::load()?;
    for b in stack.descendants_topo(branch) {
        if let Some(parent) = stack.parent_of(&b) {
            let ptip = git::rev_parse(parent)?;
            meta::set_parent_sha(&b, &ptip)?;
        }
        // A clean history rewrite leaves no markers.
        meta::set_conflicted(&b, false)?;
    }
    Ok(())
}

const HOOK_BEGIN: &str = "# >>> git-stack >>>";
const HOOK_END: &str = "# <<< git-stack <<<";

fn hook_snippet(only_amend: bool) -> String {
    let gate = if only_amend {
        "[ \"$1\" = \"amend\" ] || exit 0\n"
    } else {
        ""
    };
    format!(
        "{HOOK_BEGIN}\n\
         [ -n \"$GIT_STACK_IN_RESTACK\" ] && exit 0\n\
         {gate}command -v git-stack >/dev/null 2>&1 && git-stack restack --auto || true\n\
         {HOOK_END}\n"
    )
}

/// `git stack hooks install` — make plain `git commit`/amend auto-restack.
pub fn hooks_install() -> Result<()> {
    git::ensure_repo()?;
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-path", "hooks"])?);
    std::fs::create_dir_all(&dir)?;
    install_hook(&dir.join("post-commit"), &hook_snippet(false))?;
    install_hook(&dir.join("post-rewrite"), &hook_snippet(true))?;
    println!("Installed git-stack hooks. Plain `git commit` and `git commit --amend` on a");
    println!("stack branch will now auto-restack descendants.");
    Ok(())
}

/// `git stack hooks uninstall` — remove the git-stack hook blocks.
pub fn hooks_uninstall() -> Result<()> {
    git::ensure_repo()?;
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-path", "hooks"])?);
    for name in ["post-commit", "post-rewrite"] {
        remove_hook(&dir.join(name))?;
    }
    println!("Removed git-stack hooks.");
    Ok(())
}

fn install_hook(path: &std::path::Path, snippet: &str) -> Result<()> {
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    if contents.contains(HOOK_BEGIN) {
        return Ok(()); // already installed
    }
    let new = if contents.trim().is_empty() {
        format!("#!/bin/sh\n{snippet}")
    } else {
        // Append to an existing hook without clobbering it.
        format!("{}\n{snippet}", contents.trim_end())
    };
    std::fs::write(path, new)?;
    make_executable(path)?;
    Ok(())
}

fn remove_hook(path: &std::path::Path) -> Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let (Some(start), Some(end)) = (contents.find(HOOK_BEGIN), contents.find(HOOK_END)) else {
        return Ok(());
    };
    let after = end + HOOK_END.len();
    let mut stripped = String::new();
    stripped.push_str(&contents[..start]);
    stripped.push_str(contents[after..].trim_start_matches('\n'));
    // If nothing but a shebang/whitespace remains, remove the hook entirely.
    if stripped
        .lines()
        .all(|l| l.trim().is_empty() || l.starts_with("#!"))
    {
        std::fs::remove_file(path)?;
    } else {
        std::fs::write(path, stripped)?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Assemble render entries (branch + cached PR) for a bottom-first branch list.
fn build_entries(branches: &[String]) -> Result<Vec<Entry>> {
    let mut entries = Vec::with_capacity(branches.len());
    for b in branches {
        let pr = meta::pr(b).map(|number| PrRef {
            number,
            url: String::new(),
            state: "?".to_string(),
        });
        entries.push(Entry {
            branch: b.clone(),
            pr,
            conflicted: meta::conflicted(b),
        });
    }
    Ok(entries)
}

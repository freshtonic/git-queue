//! Implementations of each `git stack` subcommand.

use crate::render::{self, Entry, PrRef};
use crate::stack::Stack;
use crate::{gh, git, meta, restack};
use anyhow::{bail, Context, Result};

/// Enforce merge order with draft state: the bottom-most open PR is marked
/// ready; every open PR above it is marked draft (GitHub disables the merge
/// button on draft PRs, regardless of base branch, without blocking pushes).
/// Merged/closed PRs are skipped. Only toggles when a PR's state is wrong.
fn apply_draft_gate(prs: &[Option<PrRef>]) -> Result<()> {
    let mut bottom_done = false;
    for pr in prs.iter().flatten() {
        if pr.state != "OPEN" {
            continue; // merged/closed PRs are never gated
        }
        if !bottom_done {
            if pr.is_draft {
                gh::set_draft(pr.number, false)?; // the bottom PR is mergeable
            }
            bottom_done = true;
        } else if !pr.is_draft {
            gh::set_draft(pr.number, true)?; // block the ones above it
        }
    }
    Ok(())
}

/// `git stack doctor` — report-only diagnostics for merge-order enforcement.
pub fn doctor() -> Result<()> {
    git::ensure_repo()?;
    println!("git stack doctor — merge-order enforcement\n");

    match meta::gate().as_deref() {
        Some("draft") => {
            println!("  \u{2713} gate: enabled (draft mode)");
            println!("    Non-bottom PRs are kept as drafts so they can't be merged out of order;");
            println!("    `git stack submit` readies the bottom PR and drafts the ones above it.");
        }
        Some(other) => {
            println!("  ! gate: unknown mode `{other}` \u{2014} run `git stack protect` to (re)enable draft mode");
        }
        None => {
            println!("  \u{2717} gate: not enabled \u{2014} run `git stack protect` to turn it on");
        }
    }

    if gh::ready() {
        println!("  \u{2713} GitHub CLI: authenticated");
    } else {
        println!("  ! GitHub CLI: not authenticated (`gh auth login`) \u{2014} needed for `submit` to set draft state");
    }

    println!("\nNote: draft is a soft gate \u{2014} a reviewer can mark a PR ready and merge it deliberately.");
    Ok(())
}

/// `git stack protect` — enable draft-based merge-order enforcement.
///
/// Draft is the one GitHub mechanism that composes with base-chaining: a draft
/// PR's merge button is disabled regardless of base branch, and (unlike branch
/// rules / rulesets) draft status does not block pushing to the branch.
pub fn protect() -> Result<()> {
    git::ensure_repo()?;
    meta::set_gate("draft")?;
    println!("Enabled draft-based merge-order enforcement for this repository.\n");
    println!("`git stack submit` now keeps the bottom (mergeable) PR ready and marks every PR");
    println!("above it as a draft, so they can't be merged out of order. As PRs land, `git stack");
    println!("sync` + `git stack submit` readies the new bottom PR.\n");
    println!("No GitHub setup or admin rights needed. It is a soft gate: a reviewer can mark a PR");
    println!("ready and merge it deliberately. Run `git stack submit` now to apply it.");
    Ok(())
}

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

/// `git stack split` — split the current branch's commits into a stack of
/// branches. Opens a `rebase -i`-style editor where each commit is prefixed
/// with the branch it should belong to; consecutive commits sharing a name
/// become one branch, and the groups stack in order (file order = merge order).
pub fn split() -> Result<()> {
    git::ensure_repo()?;
    if !git::worktree_clean() {
        bail!("working tree has uncommitted changes; commit or stash them before splitting");
    }
    let stack = Stack::load()?;
    let branch = git::current_branch()?;
    let base = if stack.is_tracked(&branch) {
        stack.parent_of(&branch).unwrap().to_string()
    } else {
        stack.trunk.clone()
    };

    let commits = git::commits_between(&base, &branch)?;
    if commits.len() < 2 {
        bail!("`{branch}` has fewer than 2 commits beyond `{base}`; nothing to split");
    }

    let assignments = edit_split_plan(&branch, &commits)?;
    let segments = parse_segments(&assignments, &commits)?;
    if segments.len() < 2 {
        println!("All commits stayed in one branch — nothing split.");
        return Ok(());
    }

    // Names must be free (unless it's the original branch we're reusing).
    for (name, _) in &segments {
        if name != &branch && git::branch_exists(name) {
            bail!("branch `{name}` already exists; pick a different name");
        }
    }

    // Detach so we can move/create refs freely, then place a branch at each
    // group boundary and wire up the parent pointers bottom-up.
    git::detach_head()?;
    let mut parent = base.clone();
    for (name, tip_sha) in &segments {
        if git::branch_exists(name) {
            git::force_ref(name, tip_sha)?;
        } else {
            git::create_branch(name, tip_sha)?;
        }
        meta::set_parent(name, &parent)?;
        meta::set_parent_sha(name, &git::rev_parse(&parent)?)?;
        parent = name.clone();
    }

    let top = segments.last().unwrap().0.clone();
    git::checkout(&top)?;

    // If the original branch wasn't reused as a segment, it still points at the
    // old tip; leave it but tell the user.
    let reused = segments.iter().any(|(n, _)| n == &branch);
    println!("Split `{branch}` into {} stacked branches:", segments.len());
    let mut p = base.clone();
    for (name, _) in &segments {
        println!("  {p} ← {name}");
        p = name.clone();
    }
    if !reused {
        println!("note: `{branch}` still points at the old tip; delete it if you don't need it.");
    }
    println!("Now on `{top}`. Run `git stack submit` to open the PRs.");
    Ok(())
}

/// Open an editor to assign each commit to a branch. Returns `(branch, sha)`
/// pairs in commit order.
fn edit_split_plan(branch: &str, commits: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-dir"])?);
    let path = dir.join("STACK_SPLIT");
    let mut body = String::new();
    for (sha, subject) in commits {
        body.push_str(&format!(
            "{branch} {} {subject}\n",
            &sha[..sha.len().min(12)]
        ));
    }
    let template = format!(
        "{body}\n\
         # Split `{branch}` into a stack. The first token on each line is the branch\n\
         # that commit belongs to — edit it. Consecutive commits with the SAME branch\n\
         # become one PR; groups stack top-to-bottom in this file (top = merges first).\n\
         # Do not reorder or delete commit lines. Lines starting with '#' are ignored.\n"
    );
    std::fs::write(&path, template)?;

    let editor = git::out(&["var", "GIT_EDITOR"])?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()
        .context("failed to launch editor")?;
    if !status.success() {
        bail!("editor exited with an error; split cancelled");
    }
    let raw = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);

    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let name = it.next().unwrap().to_string();
        let sha = it.next().unwrap_or("").to_string();
        out.push((name, sha));
    }
    Ok(out)
}

/// Validate the edited plan against the original commit order and collapse it
/// into contiguous `(branch, tip_full_sha)` segments in merge order.
fn parse_segments(
    assignments: &[(String, String)],
    commits: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    if assignments.len() != commits.len() {
        bail!(
            "expected {} commit lines but found {}; do not add or remove lines",
            commits.len(),
            assignments.len()
        );
    }
    let mut segments: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (i, (name, short)) in assignments.iter().enumerate() {
        let (full_sha, _) = &commits[i];
        // Guard against reordering: the short sha must match this position.
        if !short.is_empty() && !full_sha.starts_with(short.as_str()) {
            bail!("commit lines were reordered; that isn't supported yet — keep them in order");
        }
        if name.is_empty() {
            bail!(
                "commit {} has no branch name",
                &full_sha[..12.min(full_sha.len())]
            );
        }
        match segments.last_mut() {
            Some((last_name, tip)) if last_name == name => {
                *tip = full_sha.clone(); // extend current group
            }
            _ => {
                if seen.contains(name) {
                    bail!("branch `{name}` appears in non-adjacent groups; commits for one branch must be contiguous");
                }
                seen.push(name.clone());
                segments.push((name.clone(), full_sha.clone()));
            }
        }
    }
    Ok(segments)
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

/// `git stack describe [-m <text>]` — set the description of what the current
/// branch/PR is about. It becomes the body of the PR (below the stack list) on
/// the next `submit`. Opens `$EDITOR` when `-m` is omitted.
pub fn describe(message: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let stack = Stack::load()?;
    let branch = git::current_branch()?;
    if !stack.is_tracked(&branch) {
        bail!("`{branch}` is not a stack branch; `git stack create`/`track` it first");
    }
    let text = match message {
        Some(m) => m,
        None => edit_description(&branch, meta::description(&branch).as_deref())?,
    };
    meta::set_description(&branch, &text)?;
    if text.trim().is_empty() {
        println!("Cleared the description for `{branch}`.");
    } else {
        println!(
            "Saved the description for `{branch}`. It will appear in the PR on `git stack submit`."
        );
    }
    Ok(())
}

/// Open the user's git editor on a temp file seeded with `existing`, and return
/// the edited text (lines starting with `#` are stripped as comments).
fn edit_description(branch: &str, existing: Option<&str>) -> Result<String> {
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-dir"])?);
    let path = dir.join("STACK_DESCRIBE");
    let template = format!(
        "{}\n\n# Describe what `{branch}` is about. This becomes the PR body\n\
         # (below the auto-generated stack list). Lines starting with '#' are ignored.\n",
        existing.unwrap_or("")
    );
    std::fs::write(&path, template)?;

    let editor = git::out(&["var", "GIT_EDITOR"])?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh") // $0
        .arg(&path) // $1
        .status()
        .context("failed to launch editor")?;
    if !status.success() {
        bail!("editor exited with an error; description unchanged");
    }
    let raw = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    let cleaned: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    Ok(cleaned.join("\n").trim().to_string())
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
    let mut stack = Stack::load()?;
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

    // Heal branches orphaned by a merged-and-deleted parent. `--delete-branch`
    // (and manual branch deletion) removes the branch's git config, including
    // our `stackParent`, leaving its children pointing at a branch that no
    // longer exists. Reparent those onto trunk.
    stack = heal_dangling_parents(stack)?;

    // Prune branches whose PRs have merged: reparent their children onto the
    // nearest surviving ancestor (trunk, once the bottom lands) and drop them
    // from the stack. The restack below then rebases the survivors onto trunk.
    if gh::ready() {
        stack = prune_merged(stack)?;
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

/// Reparent tracked branches whose parent is gone (not trunk, not tracked, and
/// no such branch exists) onto trunk. Returns the reloaded stack if anything
/// changed.
fn heal_dangling_parents(stack: Stack) -> Result<Stack> {
    let mut changed = false;
    for b in stack.topo_order() {
        let parent = match stack.parent_of(&b) {
            Some(p) => p.to_string(),
            None => continue,
        };
        if parent == stack.trunk || stack.is_tracked(&parent) || git::branch_exists(&parent) {
            continue;
        }
        meta::set_parent(&b, &stack.trunk)?;
        eprintln!(
            "note: `{b}`'s parent `{parent}` is gone (merged?); reparented onto `{}`.",
            stack.trunk
        );
        changed = true;
    }
    if changed {
        Stack::load()
    } else {
        Ok(stack)
    }
}

/// Reparent the children of any MERGED-PR branch onto their nearest surviving
/// ancestor, untrack the merged branches, and return the reloaded stack.
fn prune_merged(stack: Stack) -> Result<Stack> {
    use std::collections::HashSet;
    let mut merged: HashSet<String> = HashSet::new();
    for b in stack.topo_order() {
        // Best-effort: ignore lookup failures (e.g. not a GitHub repo).
        if let Some(pr) = gh::find(&b).ok().flatten() {
            if pr.state == "MERGED" {
                merged.insert(b);
            }
        }
    }
    if merged.is_empty() {
        return Ok(stack);
    }
    // Reparent survivors whose parent has merged (walk up past merged ancestors).
    for b in stack.topo_order() {
        if merged.contains(&b) {
            continue;
        }
        let current_parent = stack.parent_of(&b).unwrap().to_string();
        let mut new_parent = current_parent.clone();
        while merged.contains(&new_parent) {
            new_parent = stack
                .parent_of(&new_parent)
                .map(|s| s.to_string())
                .unwrap_or_else(|| stack.trunk.clone());
        }
        if new_parent != current_parent {
            meta::set_parent(&b, &new_parent)?;
        }
    }
    for b in &merged {
        meta::untrack(b);
        println!("`{b}` has merged — dropped from the stack, children reparented.");
    }
    Stack::load()
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

    let base_of = |i: usize| -> String {
        if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        }
    };

    // Look up existing PRs once. A MERGED/CLOSED PR is "frozen": we must not push
    // to it, recreate it, or edit its base (GitHub rejects a base change on a
    // closed PR). We still list it (with its merged/closed emoji).
    let remote = meta::remote();
    let existing: Vec<Option<gh::Pr>> = branches
        .iter()
        .map(|b| gh::find(b))
        .collect::<Result<_>>()?;
    // Only MERGED PRs are "frozen" (left untouched). A CLOSED PR in the active
    // line is revived below — GitHub closes a stacked PR when its base branch is
    // deleted (e.g. `--delete-branch` on the PR below), and we must not skip it.
    let frozen: Vec<bool> = existing
        .iter()
        .map(|p| p.as_ref().map(|pr| pr.state.as_str()) == Some("MERGED"))
        .collect();

    // Guard: don't open an empty PR (already-landed branches are skipped).
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            continue;
        }
        let base = base_of(i);
        if git::ahead_count(&base, b)? == 0 {
            bail!("`{b}` has no commits beyond `{base}`; add a commit before submitting");
        }
    }

    // Pass 1: push active branches (bottom-first so bases exist) and
    // create-or-find their PRs.
    let mut prs: Vec<Option<PrRef>> = vec![None; total];
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            prs[i] = existing[i].as_ref().map(pr_ref); // keep untouched
            continue;
        }
        let base = base_of(i);
        println!("Pushing `{b}`...");
        git::push(&remote, b)?;

        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);
        let number = match &existing[i] {
            Some(pr) if pr.state == "OPEN" => pr.number,
            // CLOSED (base branch was deleted): reopen it, or open a fresh PR if
            // it can't be reopened (its old base is gone).
            Some(pr) => match gh::reopen(pr.number) {
                Ok(()) => pr.number,
                Err(_) => gh::create(b, &base, &title, "Opening…", draft)?,
            },
            None => gh::create(b, &base, &title, "Opening…", draft)?,
        };
        meta::set_pr(b, number)?;
    }

    // Re-read active PRs now that any new ones exist.
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            continue;
        }
        if let Some(pr) = gh::find(b)? {
            prs[i] = Some(pr_ref(&pr));
        }
    }

    // Pass 2: write correct base, numbered title and shared nav block on the
    // ACTIVE PRs (frozen ones are left exactly as they are).
    let entries: Vec<Entry> = branches
        .iter()
        .enumerate()
        .map(|(i, b)| Entry {
            branch: b.clone(),
            pr: prs[i].clone(),
            conflicted: git::has_conflict_markers(b),
        })
        .collect();

    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            continue;
        }
        let number = match &prs[i] {
            Some(p) => p.number,
            None => continue,
        };
        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);
        let nav = render::nav_block(&entries, b, &stack.trunk);
        let description = meta::description(b).unwrap_or_default();
        let body = render::compose_body(&description, &nav);
        gh::edit(number, &base_of(i), &title, &body)?;
    }

    // Enforce merge order with draft state, if enabled.
    let gated = meta::gate().as_deref() == Some("draft");
    if gated {
        apply_draft_gate(&prs)?;
    }

    println!("\nSubmitted {total} PR(s):");
    for (i, b) in branches.iter().enumerate() {
        if let Some(p) = &prs[i] {
            println!("  [{}/{}] {}  {}", i + 1, total, b, p.url);
        }
    }
    if gated {
        println!("Merge gate active: the bottom PR is ready; the rest are drafts.");
    }
    Ok(())
}

fn pr_ref(pr: &gh::Pr) -> PrRef {
    PrRef {
        number: pr.number,
        url: pr.url.clone(),
        state: pr.state.clone(),
        review: pr.review_decision.clone(),
        is_draft: pr.is_draft,
    }
}

/// `git stack yank` — close every open (non-merged) PR in the current stack.
/// Merged PRs are left alone; local branches and metadata are untouched.
pub fn yank() -> Result<()> {
    git::ensure_repo()?;
    if !gh::ready() {
        bail!("`gh` is not installed or not authenticated; run `gh auth login`");
    }
    let stack = Stack::load()?;
    let current = git::current_branch()?;
    if !stack.is_tracked(&current) {
        bail!("`{current}` is not a stack branch");
    }
    let line = stack.line_through(&current)?;
    let mut closed = 0;
    for b in &line.branches {
        if let Some(pr) = gh::find(b)? {
            if pr.state == "OPEN" {
                gh::close(pr.number)?;
                println!("Closed #{} ({b})", pr.number);
                closed += 1;
            }
        }
    }
    match closed {
        0 => println!("No open PRs in this stack to close."),
        n => println!("Closed {n} open PR(s). Merged PRs and local branches are untouched."),
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
    // `git history fixup` may report a conflict (non-zero), but it can also exit
    // 0 while doing nothing when the fold would conflict with a descendant.
    // Detect BOTH: verify the commit actually changed before claiming success —
    // otherwise we'd falsely tell the user their work was folded.
    let before = git::rev_parse(&current)?;
    let reported_conflict = git::history_fixup("HEAD")?;
    let after = git::rev_parse(&current)?;
    if reported_conflict || before == after {
        bail!(
            "amend could not fold your changes into `{current}`: doing so would conflict with a \
             descendant branch, so nothing was changed (your staged changes are intact).\n\
             Resolve the conflict on the descendant first, or use `git stack commit` to add a \
             separate commit instead."
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
            review: None,
            is_draft: false,
        });
        entries.push(Entry {
            branch: b.clone(),
            pr,
            // Detect markers live so `status` can never go stale.
            conflicted: git::has_conflict_markers(b),
        });
    }
    Ok(entries)
}

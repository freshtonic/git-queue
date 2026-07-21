//! Implementations of each `git queue` subcommand.

use crate::queue::{Line, Queue};
use crate::render::{self, Entry, PrRef};
use crate::{gh, git, ident, meta, requeue};
use anyhow::{bail, Context, Result};

/// Advertise merge order with commit statuses: the bottom-most open PR's head
/// gets a green `git-queue/merge-order` status; every open PR above it gets a
/// red one naming (and linking to) the PR that must merge first. Advisory: it
/// signals in the checks UI without touching draft state, branch rules, or
/// pushes. Assumes the branches were just pushed, so local tips == PR heads.
fn apply_status_gate(entries: &[Entry]) -> Result<()> {
    for s in render::gate_plan(entries) {
        let sha = git::rev_parse(&s.branch)?;
        gh::set_commit_status(
            &sha,
            render::GATE_CONTEXT,
            s.success,
            &s.description,
            s.target_url.as_deref(),
        )?;
    }
    Ok(())
}

/// `git queue doctor` — report-only diagnostics for merge-order enforcement.
pub fn doctor() -> Result<()> {
    git::ensure_repo()?;
    println!("git queue doctor — merge-order enforcement\n");

    match meta::gate().as_deref() {
        Some("status") => {
            println!("  \u{2713} gate: enabled (status mode)");
            println!(
                "    `git queue submit` posts a `{}` commit status on every open PR:",
                render::GATE_CONTEXT
            );
            println!("    green \u{2713} on the PR at the front of the queue, red \u{2717} (\u{201c}merge PR #N first\u{201d})");
            println!("    on the ones behind it.");
        }
        Some(other) => {
            println!("  ! gate: unknown mode `{other}` \u{2014} run `git queue protect` to (re)enable status mode");
        }
        None => {
            println!("  \u{2717} gate: not enabled \u{2014} run `git queue protect` to turn it on");
        }
    }

    if gh::ready() {
        println!("  \u{2713} GitHub CLI: authenticated");
    } else {
        println!("  ! GitHub CLI: not authenticated (`gh auth login`) \u{2014} needed for `submit` to post merge-order statuses");
    }

    println!("\nNote: the gate is advisory \u{2014} a red \u{2717} in the checks list warns reviewers off,");
    println!("but it does not disable the merge button.");
    Ok(())
}

/// `git queue protect` — enable status-based merge-order signalling.
///
/// A commit status is the one advisory GitHub mechanism that composes with
/// base-chaining: it shows up in every PR's checks UI (red ✗ with "merge PR #N
/// first") while leaving the PRs as normal, reviewable, non-draft PRs and
/// never blocking pushes. Anything that actually disables the merge button
/// requires base-branch rules, which also gate pushes to the queue branches.
pub fn protect() -> Result<()> {
    git::ensure_repo()?;
    meta::set_gate("status")?;
    println!("Enabled status-based merge-order signalling for this repository.\n");
    println!(
        "`git queue submit` now posts a `{}` commit status on every open",
        render::GATE_CONTEXT
    );
    println!("PR in the queue: green \u{2713} on the front (mergeable) PR, red \u{2717} \u{201c}merge PR #N");
    println!(
        "first\u{201d} on every PR above it. As PRs land, `git queue sync` + `git queue submit`"
    );
    println!("promote the PR now at the front to green.\n");
    println!("No GitHub setup or admin rights needed, and PRs stay normal (no drafts). The gate");
    println!("is advisory: the red \u{2717} warns reviewers, but the merge button still works.");
    println!("Run `git queue submit` now to apply it.");
    Ok(())
}

/// `git queue init [--trunk <branch>]`
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
    println!("Initialized git-queue. Trunk is `{trunk}`.");
    println!("Create your first queued branch with:  git queue create <name>");
    Ok(())
}

/// `git queue create <name> [--base <branch>]` — new branch queued after the
/// current one (or on an explicit `--base` branch).
pub fn create(name: &str, base: Option<&str>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    if git::branch_exists(name) {
        bail!("branch `{name}` already exists");
    }
    let parent = match base {
        Some(b) => {
            if !git::branch_exists(b) {
                bail!("base branch `{b}` does not exist");
            }
            b.to_string()
        }
        None => git::current_branch()?,
    };
    let parent_sha = git::rev_parse(&parent)?;

    git::create_branch(name, &parent)?;
    meta::set_parent(name, &parent)?;
    meta::set_parent_sha(name, &parent_sha)?;
    git::checkout(name)?;

    if parent == trunk {
        println!("Created `{name}` on trunk `{trunk}`. It is the front of a new queue.");
    } else if meta::parent(&parent).is_none() {
        println!("Created `{name}` on `{parent}`. It is the front of a new queue;");
        println!("its PR will target `{parent}` (the merge base), not `{trunk}`.");
    } else {
        println!("Created `{name}` after `{parent}` in the queue.");
    }
    println!("Make your commits, then `git queue submit` to open PRs.");
    Ok(())
}

/// `git queue split` — split the current branch's commits into a queue of
/// branches. Opens a `rebase -i`-style editor where each commit is prefixed
/// with the branch it should belong to; consecutive commits sharing a name
/// become one branch, and the groups queue in order (file order = merge order).
pub fn split() -> Result<()> {
    git::ensure_repo()?;
    if !git::worktree_clean() {
        bail!("working tree has uncommitted changes; commit or stash them before splitting");
    }
    let queue = Queue::load()?;
    let branch = git::current_branch()?;
    let base = if queue.is_tracked(&branch) {
        queue.parent_of(&branch).unwrap().to_string()
    } else {
        queue.trunk.clone()
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
    println!("Split `{branch}` into {} queued branches:", segments.len());
    let mut p = base.clone();
    for (name, _) in &segments {
        println!("  {p} ← {name}");
        p = name.clone();
    }
    if !reused {
        println!("note: `{branch}` still points at the old tip; delete it if you don't need it.");
    }
    println!("Now on `{top}`. Run `git queue submit` to open the PRs.");
    Ok(())
}

/// Open an editor to assign each commit to a branch. Returns `(branch, sha)`
/// pairs in commit order.
fn edit_split_plan(branch: &str, commits: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-dir"])?);
    let path = dir.join("QUEUE_SPLIT");
    let mut body = String::new();
    for (sha, subject) in commits {
        body.push_str(&format!(
            "{branch} {} {subject}\n",
            &sha[..sha.len().min(12)]
        ));
    }
    let template = format!(
        "{body}\n\
         # Split `{branch}` into a queue. The first token on each line is the branch\n\
         # that commit belongs to — edit it. Consecutive commits with the SAME branch\n\
         # become one PR; groups queue top-to-bottom in this file (top = merges first).\n\
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

/// `git queue track [--parent <branch>]` — adopt the current branch. Offers
/// to stamp `Queued-Commit-Id` trailers onto the adopted commits (a history rewrite,
/// so it asks first; `--stamp-ids`/`--no-stamp-ids` decide non-interactively).
pub fn track(parent: Option<String>, stamp_ids: bool, no_stamp_ids: bool) -> Result<()> {
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

    // Offer stable change identity to the adopted commits.
    let missing: Vec<String> = git::queue_ids(&format!("{base}..{branch}"))?
        .into_iter()
        .filter(|(_, id)| id.is_none())
        .map(|(sha, _)| sha)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let n = missing.len();
    let stamp = if stamp_ids {
        true
    } else if no_stamp_ids {
        false
    } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!();
        println!("`{branch}` has {n} commit(s) without a Queued-Commit-Id (stable change identity");
        println!("that survives rebases; used for safe syncing and squash-merge detection).");
        println!("Stamping rewrites those commits — their hashes change. If the branch is");
        println!("already pushed, the next `git queue sync`/`submit` will force-push it (with");
        println!("lease), and anyone who fetched the old hashes will need to reset onto the");
        println!("new ones. Any open PR keeps working.");
        print!("Stamp them now? [Y/n] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
    } else {
        eprintln!(
            "note: {n} commit(s) have no Queued-Commit-Id; re-run `git queue track --stamp-ids` to \
             stamp them (rewrites their hashes)."
        );
        false
    };
    if !stamp {
        return Ok(());
    }
    if !git::worktree_clean() {
        eprintln!("note: working tree has uncommitted changes; skipping id stamping. Commit or");
        eprintln!("stash, then re-run `git queue track --stamp-ids`.");
        return Ok(());
    }
    git::rebase_stamp_ids(&base, &branch, &missing)?;
    println!("Stamped {n} commit(s) with Queued-Commit-Ids (their hashes changed).");
    Ok(())
}

/// `git queue untrack` — forget the current branch's queue metadata.
pub fn untrack() -> Result<()> {
    git::ensure_repo()?;
    let branch = git::current_branch()?;
    meta::untrack(&branch);
    println!("Stopped tracking `{branch}`.");
    Ok(())
}

/// `git queue describe [-m <text>]` — set the description of what the current
/// branch/PR is about. It becomes the body of the PR (below the queue list) on
/// the next `submit`. Opens `$EDITOR` when `-m` is omitted.
pub fn describe(message: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let branch = git::current_branch()?;
    if !queue.is_tracked(&branch) {
        bail!("`{branch}` is not a queue branch; `git queue create`/`track` it first");
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
            "Saved the description for `{branch}`. It will appear in the PR on `git queue submit`."
        );
    }
    Ok(())
}

/// Open the user's git editor on a temp file seeded with `existing`, and return
/// the edited text (lines starting with `#` are stripped as comments).
fn edit_description(branch: &str, existing: Option<&str>) -> Result<String> {
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-dir"])?);
    let path = dir.join("QUEUE_DESCRIBE");
    let template = format!(
        "{}\n\n# Describe what `{branch}` is about. This becomes the PR body\n\
         # (below the auto-generated queue list). Lines starting with '#' are ignored.\n",
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

/// `git queue status`
pub fn status() -> Result<()> {
    show_tree(false)
}

/// `git queue log` — the status tree, with each branch's commits indented one
/// level beneath it, newest first, each prefixed by its abbreviated Queued-Commit-Id.
pub fn log() -> Result<()> {
    show_tree(true)
}

fn show_tree(with_commits: bool) -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let current = git::current_branch()?;

    // Choose a line to show: the current branch's; or, standing on trunk or a
    // base branch, the first queue rooted here.
    let anchor = if queue.is_tracked(&current) {
        current.clone()
    } else if let Some(child) = queue.children(&current).into_iter().next() {
        child
    } else if current == queue.trunk {
        match queue.roots().into_iter().next() {
            Some(r) => r,
            None => {
                println!("No queues yet. Create one with `git queue create <name>`.");
                return Ok(());
            }
        }
    } else {
        println!("Branch `{current}` is not tracked. Adopt it with `git queue track`.");
        return Ok(());
    };

    let line = queue.line_through(&anchor)?;
    let mut entries = build_entries(&line.branches)?;
    for (i, e) in entries.iter_mut().enumerate() {
        let parent = if i == 0 {
            line.base.clone()
        } else {
            line.branches[i - 1].clone()
        };
        if let Ok(ids) = git::queue_ids(&format!("{parent}..{}", e.branch)) {
            let have = ids.iter().filter(|(_, id)| id.is_some()).count();
            e.ids = Some((have, ids.len()));
        }
        if with_commits {
            if let Ok(commits) = git::commits_with_ids(&format!("{parent}..{}", e.branch)) {
                e.commits = commits;
            }
        }
    }
    let fork = line.fork_at.as_deref();
    print!(
        "{}",
        render::status_tree(
            &entries,
            &current,
            &line.base,
            line.base == queue.trunk,
            fork
        )
    );
    Ok(())
}

/// `git queue prev` / `down` — check out the parent branch.
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

/// `git queue next` / `up` — check out the child branch.
pub fn next() -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let branch = git::current_branch()?;
    let kids = queue.children(&branch);
    match kids.len() {
        0 => bail!("`{branch}` is at the top of its queue (no children)"),
        1 => git::checkout(&kids[0]),
        _ => {
            let list = kids.join("\n  ");
            bail!("`{branch}` has multiple children; check one out directly:\n  {list}")
        }
    }
}

/// `git queue sync [--no-push]` — pull in commits others pushed to queue
/// branches, requeue the whole queue onto the latest trunk, and (by default)
/// push every branch back with `--force-with-lease` so remote work is never
/// clobbered.
pub fn sync(no_push: bool) -> Result<()> {
    git::ensure_repo()?;
    if git::rebase_in_progress() {
        bail!("a rebase is already in progress; finish it (`git rebase --continue`/`--abort`) then re-run `git queue sync`");
    }
    std::env::set_var(git::GUARD_ENV, "1"); // suppress our hooks during internal git ops
    let mut queue = Queue::load()?;
    let remote = meta::remote();
    let original = git::current_branch()?;
    let started_clean = git::worktree_clean();

    println!("Fetching `{remote}`...");
    if let Err(e) = git::fetch(&remote) {
        eprintln!("warning: fetch failed; syncing against local refs only: {e}");
    }

    // Bring every line's local base (trunk, release branches, ...) up to its
    // remote tip. Bases are treated as remote-canonical, like trunk always was.
    for base in queue.bases() {
        if let Some(remote_base) = git::remote_trunk(&remote, &base) {
            let tip = git::rev_parse(&remote_base)?;
            if original == base {
                let _ = git::merge_ff_only(&remote_base);
            } else {
                let _ = git::force_ref(&base, &tip);
            }
        }
    }

    // Heal branches orphaned by a merged-and-deleted parent. `--delete-branch`
    // (and manual branch deletion) removes the branch's git config, including
    // our `queueParent`, leaving its children pointing at a branch that no
    // longer exists. Reparent those onto trunk.
    queue = heal_dangling_parents(queue)?;

    // Prune branches whose PRs have merged: reparent their children onto the
    // nearest surviving ancestor (trunk, once the bottom lands) and drop them
    // from the queue. The requeue below then rebases the survivors onto trunk.
    queue = prune_landed_by_id(queue)?;
    if gh::ready() {
        queue = prune_merged(queue)?;
    }

    // Pull in commits teammates pushed to our queue branches (bottom-up).
    for branch in queue.topo_order() {
        match incorporate_remote(&branch, &remote, &original)? {
            Some(RemoteAction::FastForwarded) => {
                println!("Pulled remote commits into `{branch}` (fast-forward).")
            }
            Some(RemoteAction::Pulled(n)) => {
                println!("Pulled {n} teammate commit(s) into `{branch}`.")
            }
            None => {}
        }
    }

    // Detach HEAD while refs move: `git replay` updates refs in place without
    // touching the worktree, so requeueing the checked-out branch would leave
    // the worktree stale. Detaching makes the checkout below a real one.
    git::detach_head()?;

    // Reconcile the whole forest onto updated parents (new engine).
    let report = requeue::requeue_forest(&queue)?;

    // Return to where we started before pushing. Requeueing moves refs without
    // touching the worktree, so if `original` itself was requeued the files on
    // disk are stale; with a clean starting tree it is safe to snap them to the
    // branch tip.
    git::checkout_quiet(&original)?;
    if started_clean {
        git::reset_hard_head()?;
    } else {
        eprintln!(
            "note: the worktree had local changes when sync started; if `{original}` was \
             rebased, run `git reset --hard` once your changes are safe."
        );
    }

    // Push every branch back, with lease, unless asked not to. A push can
    // legitimately fail mid-sync — e.g. GitHub marks queued PRs merged and
    // auto-deletes their branches the moment an earlier push makes their
    // commits reachable from the base — so keep going and say so instead of
    // dying half-way through.
    let mut push_failures = 0usize;
    if !no_push {
        for branch in queue.topo_order() {
            println!("Pushing `{branch}`...");
            if let Err(e) = git::push(&remote, &branch) {
                eprintln!("warning: push of `{branch}` failed: {e:#}");
                push_failures += 1;
            }
        }
        if push_failures > 0 {
            eprintln!(
                "note: {push_failures} push(es) failed. If a PR merged or its branch was \
                 deleted on the remote mid-sync, re-run `git queue sync` — it will prune \
                 merged branches and settle the rest."
            );
        }
    }

    // Reconcile PRs on every *published* line — one with at least one PR
    // anywhere in it, including a PR that predates the branch becoming a
    // queue. Missing PRs are opened; existing ones get their base, numbered
    // title and nav block rewritten. Lines with no PRs at all stay local:
    // `git queue submit` publishes those deliberately.
    if !no_push && gh::ready() {
        for leaf in queue.leaves() {
            let line = match queue.line_through(&leaf) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let published = line
                .branches
                .iter()
                .any(|b| gh::find(b).ok().flatten().is_some());
            if !published {
                continue;
            }
            let outcome = reconcile_line_prs(&line, false, None, false)?;
            report_line(&line, &outcome, "Reconciled")?;
        }
    }

    if !report.conflicted.is_empty() {
        requeue::warn_conflicts(&report.conflicted);
    } else {
        let bases = queue.bases();
        match bases.as_slice() {
            [] => println!("Nothing to sync yet."),
            [one] => println!("Queue is in sync with `{one}`."),
            many => println!(
                "Queues are in sync with their bases: {}.",
                many.iter()
                    .map(|b| format!("`{b}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(())
}

/// Reparent tracked branches whose parent is gone (not trunk, not tracked, and
/// no such branch exists) onto trunk. Returns the reloaded queue if anything
/// changed.
fn heal_dangling_parents(queue: Queue) -> Result<Queue> {
    let mut changed = false;
    for b in queue.topo_order() {
        let parent = match queue.parent_of(&b) {
            Some(p) => p.to_string(),
            None => continue,
        };
        if queue.is_tracked(&parent) || git::branch_exists(&parent) {
            continue;
        }
        meta::set_parent(&b, &queue.trunk)?;
        eprintln!(
            "note: `{b}`'s parent `{parent}` is gone (merged?); reparented onto `{}`.",
            queue.trunk
        );
        changed = true;
    }
    if changed {
        Queue::load()
    } else {
        Ok(queue)
    }
}

/// Reparent the children of any MERGED-PR branch onto their nearest surviving
/// ancestor, untrack the merged branches, and return the reloaded queue.
fn prune_merged(queue: Queue) -> Result<Queue> {
    use std::collections::HashSet;
    let mut merged: HashSet<String> = HashSet::new();
    for b in queue.topo_order() {
        // Best-effort: ignore lookup failures (e.g. not a GitHub repo).
        if let Some(pr) = gh::find(&b).ok().flatten() {
            if pr.state == "MERGED" {
                merged.insert(b);
            }
        }
    }
    drop_from_queue(queue, &merged, "has merged")
}

/// Drop branches whose every commit already landed on trunk, detected by
/// Queued-Commit-Id correspondence — which survives squash merges that destroy both
/// SHAs and patch-ids. Only branches where *all* commits carry an id are
/// considered (no guessing). Pure git; needs no GitHub access.
fn prune_landed_by_id(queue: Queue) -> Result<Queue> {
    use std::collections::HashSet;
    let mut landed: HashSet<String> = HashSet::new();
    for b in queue.topo_order() {
        let parent = queue.parent_of(&b).unwrap().to_string();
        let (Ok(pb), Ok(tb)) = (
            git::merge_base(&parent, &b),
            git::merge_base(&queue.trunk, &b),
        ) else {
            continue;
        };
        let Ok(ids) = git::queue_ids(&format!("{pb}..{b}")) else {
            continue;
        };
        if ids.is_empty() || ids.iter().any(|(_, id)| id.is_none()) {
            continue;
        }
        let Ok(trunk_text) = git::log_messages(&format!("{tb}..{}", queue.trunk)) else {
            continue;
        };
        if ids
            .iter()
            .all(|(_, id)| trunk_text.contains(id.as_deref().unwrap()))
        {
            landed.insert(b);
        }
    }
    drop_from_queue(
        queue,
        &landed,
        "has landed on trunk (Queued-Commit-Ids found)",
    )
}

/// Untrack every branch in `gone`, reparenting survivors onto their nearest
/// surviving ancestor, and return the reloaded queue.
fn drop_from_queue(
    queue: Queue,
    gone: &std::collections::HashSet<String>,
    why: &str,
) -> Result<Queue> {
    if gone.is_empty() {
        return Ok(queue);
    }
    for b in queue.topo_order() {
        if gone.contains(&b) {
            continue;
        }
        let current_parent = queue.parent_of(&b).unwrap().to_string();
        let mut new_parent = current_parent.clone();
        while gone.contains(&new_parent) {
            new_parent = queue
                .parent_of(&new_parent)
                .map(|s| s.to_string())
                .unwrap_or_else(|| queue.trunk.clone());
        }
        if new_parent != current_parent {
            meta::set_parent(&b, &new_parent)?;
        }
    }
    for b in gone {
        meta::untrack(b);
        println!("`{b}` {why} — dropped from the queue, children reparented.");
    }
    Queue::load()
}

enum RemoteAction {
    FastForwarded,
    Pulled(usize),
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
    // If the remote tip is a position this local branch has already been at
    // (it's in the branch's reflog), the remote holds nothing we haven't
    // seen — it's just stale relative to a local rewrite (amend, move, an
    // unpushed requeue). Pulling it back in would re-apply our own commits
    // on top of their old selves and manufacture self-conflicts; the rewrite
    // is authoritative and `--force-with-lease` replaces the remote at push.
    if git::was_previous_position(branch, &remote_sha) {
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
    // Diverged: pull in only what is genuinely new. Queued-Commit-Id correspondence
    // separates teammate work from stale copies of our own rewritten commits;
    // id-less commits fall back to patch-equivalence (`git cherry`).
    let mb = git::merge_base(&format!("{remote}/{branch}"), branch)?;
    let local_ids: std::collections::HashSet<String> = git::queue_ids(&format!("{mb}..{branch}"))?
        .into_iter()
        .filter_map(|(_, id)| id)
        .collect();
    let patch_fresh: std::collections::HashSet<String> = git::cherry_fresh(branch, &remote_sha)?
        .into_iter()
        .collect();
    let fresh: Vec<String> = git::queue_ids(&format!("{mb}..{remote_sha}"))?
        .into_iter()
        .filter(|(sha, id)| match id {
            Some(id) => !local_ids.contains(id),
            None => patch_fresh.contains(sha),
        })
        .map(|(sha, _)| sha)
        .collect();
    if fresh.is_empty() {
        return Ok(None); // the remote only has stale copies of our own commits
    }
    let n = fresh.len();
    git::cherry_pick_persist(branch, &fresh)?;
    Ok(Some(RemoteAction::Pulled(n)))
}

/// The outcome of reconciling one line's PRs.
struct LinePrs {
    entries: Vec<Entry>,
    prs: Vec<Option<PrRef>>,
}

/// Reconcile a line's PRs with its branches: revive or create missing PRs,
/// then rewrite base, numbered title and the shared nav block on every open
/// one. When `push` names a remote, each active branch is pushed (front-first,
/// so bases exist) before its PR is touched — submit's path; sync passes
/// `None` because it already pushed. `strict` bails on branches with no
/// commits beyond their base (submit); otherwise they are skipped with a note
/// (sync must not die mid-reconciliation).
fn reconcile_line_prs(
    line: &Line,
    draft: bool,
    push: Option<&str>,
    strict: bool,
) -> Result<LinePrs> {
    let branches = &line.branches;
    let total = branches.len();
    let base_of = |i: usize| -> String {
        if i == 0 {
            line.base.clone()
        } else {
            branches[i - 1].clone()
        }
    };

    // Look up existing PRs once. A MERGED PR is "frozen": we must not push to
    // it, recreate it, or edit its base. A CLOSED PR is revived below — GitHub
    // closes a queued PR when its base branch is deleted (e.g. `--delete-branch`
    // on the PR below), and we must not skip it.
    let existing: Vec<Option<gh::Pr>> = branches
        .iter()
        .map(|b| gh::find(b))
        .collect::<Result<_>>()?;
    let frozen: Vec<bool> = existing
        .iter()
        .map(|p| p.as_ref().map(|pr| pr.state.as_str()) == Some("MERGED"))
        .collect();

    // Guard: don't open an empty PR (already-landed branches are skipped).
    let mut empty = vec![false; total];
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            continue;
        }
        if git::ahead_count(&base_of(i), b)? == 0 {
            if strict {
                bail!(
                    "`{b}` has no commits beyond `{}`; add a commit before submitting",
                    base_of(i)
                );
            }
            if existing[i].is_none() {
                eprintln!(
                    "note: `{b}` has no commits beyond `{}`; not opening a PR for it.",
                    base_of(i)
                );
                empty[i] = true;
            }
        }
    }

    // Pass 1: (optionally push and) create-or-revive each active branch's PR.
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] || empty[i] {
            continue;
        }
        let base = base_of(i);
        if let Some(remote) = push {
            println!("Pushing `{b}`...");
            git::push(remote, b)?;
        }
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
            None => {
                let n = gh::create(b, &base, &title, "Opening…", draft)?;
                println!("Opened PR #{n} for `{b}` (base `{base}`).");
                n
            }
        };
        meta::set_pr(b, number)?;
    }

    // Re-read active PRs now that any new ones exist.
    let mut full: Vec<Option<gh::Pr>> = vec![None; total];
    let mut prs: Vec<Option<PrRef>> = vec![None; total];
    for (i, b) in branches.iter().enumerate() {
        if frozen[i] {
            prs[i] = existing[i].as_ref().map(pr_ref); // keep untouched
            continue;
        }
        if let Some(pr) = gh::find(b)? {
            prs[i] = Some(pr_ref(&pr));
            full[i] = Some(pr);
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
            ids: None,
            commits: Vec::new(),
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
        // Renumber the PR's existing title rather than imposing the commit
        // subject: titles are review-facing and often hand-written (especially
        // on PRs that predate the queue). New PRs were just created from the
        // tip subject, so this is a no-op for them.
        let subject = match &full[i] {
            Some(pr) if !pr.title.trim().is_empty() => pr.title.clone(),
            _ => git::tip_subject(b)?,
        };
        let title = render::numbered_title(&subject, i, total);
        let nav = render::nav_block(&entries, b, &line.base);
        let description = match meta::description(b).filter(|d| !d.trim().is_empty()) {
            Some(d) => d,
            None => {
                // No local description: adopt the PR's existing body (minus any
                // previous nav block) so updating a PR that predates the queue
                // never wipes what its author wrote.
                let adopted = full[i]
                    .as_ref()
                    .map(|pr| render::strip_block(&pr.body).trim().to_string())
                    .unwrap_or_default();
                let adopted = if adopted == "Opening…" {
                    String::new()
                } else {
                    adopted
                };
                if !adopted.is_empty() {
                    meta::set_description(b, &adopted)?;
                }
                adopted
            }
        };
        let body = render::compose_body(&description, &nav);
        gh::edit(number, &base_of(i), &title, &body)?;
    }

    Ok(LinePrs { entries, prs })
}

/// Apply the status gate (if enabled) and print the line's PR listing.
fn report_line(line: &Line, outcome: &LinePrs, heading: &str) -> Result<()> {
    let gate = meta::gate();
    let gated = gate.as_deref() == Some("status");
    if let Some(other) = gate.as_deref().filter(|g| *g != "status") {
        eprintln!("warning: unknown queue.gate mode `{other}` — no merge gate applied; run `git queue protect` to enable status mode");
    }
    if gated {
        apply_status_gate(&outcome.entries)?;
    }
    let total = line.branches.len();
    println!("\n{heading} {total} PR(s):");
    for (i, b) in line.branches.iter().enumerate() {
        if let Some(p) = &outcome.prs[i] {
            println!("  [{}/{}] {}  {}", i + 1, total, b, p.url);
        }
    }
    if gated {
        println!("Merge gate active: the front PR's `{}` status is \u{2713}; the rest are \u{2717} until it lands.", render::GATE_CONTEXT);
    }
    Ok(())
}

/// `git queue submit [--draft]` — push the current queue line and open/update
/// its numbered PRs.
pub fn submit(draft: bool) -> Result<()> {
    git::ensure_repo()?;
    if !gh::ready() {
        bail!("`gh` is not installed or not authenticated; run `gh auth login`");
    }
    let queue = Queue::load()?;
    let current = git::current_branch()?;
    if !queue.is_tracked(&current) {
        bail!("`{current}` is not tracked; run `git queue create` or `git queue track` first");
    }
    let line = queue.line_through(&current)?;
    if let Some(fork) = &line.fork_at {
        eprintln!("warning: `{fork}` has multiple children; submitting only this line.");
    }
    let remote = meta::remote();
    let outcome = reconcile_line_prs(&line, draft, Some(&remote), true)?;
    report_line(&line, &outcome, "Submitted")
}

fn pr_ref(pr: &gh::Pr) -> PrRef {
    PrRef {
        number: pr.number,
        url: pr.url.clone(),
        state: pr.state.clone(),
        review: pr.review_decision.clone(),
    }
}

/// Resolve a user-supplied commit-ish for queue commands: a git revision, or
/// a `Queued-Commit-Id` — full, or a unique prefix such as the abbreviated
/// form `git queue log` displays. Id lookup is scoped to the line's commits.
fn resolve_queue_rev(line: &Line, arg: &str) -> Result<String> {
    let arg = arg.trim();
    if arg.starts_with("q-") {
        let top = line.branches.last().unwrap();
        let ids = git::queue_ids(&format!("{}..{top}", line.base))?;
        let matches: Vec<&(String, Option<String>)> = ids
            .iter()
            .filter(|(_, id)| {
                id.as_deref()
                    .map(|i| i == arg || i.starts_with(arg))
                    .unwrap_or(false)
            })
            .collect();
        match matches.as_slice() {
            [(sha, _)] => return Ok(sha.clone()),
            [] => {
                // Fall through: maybe it's a real revision that happens to
                // start with `q-`.
                if let Ok(sha) = git::rev_parse(arg) {
                    return Ok(sha);
                }
                bail!("no commit in this queue has Queued-Commit-Id `{arg}`");
            }
            many => bail!(
                "Queued-Commit-Id prefix `{arg}` is ambiguous ({} matches); use more characters",
                many.len()
            ),
        }
    }
    git::rev_parse(arg).with_context(|| format!("`{arg}` is not a commit"))
}

/// `git queue move <commit>[..<commit>] --new-parent <commit>` — relocate a
/// commit (or an inclusive range of consecutive commits) to directly follow
/// `--new-parent`, within one PR or across PRs. The whole line is rewritten in
/// place: everything after the removal and insertion points is rebased, branch
/// refs ride along (`--update-refs`), and conflicts are persisted as markers.
/// The moved commits join the branch segment that `--new-parent` belongs to.
pub fn move_commits(spec: &str, new_parent: &str) -> Result<()> {
    git::ensure_repo()?;
    if git::rebase_in_progress() {
        bail!("a rebase is already in progress; finish it (`git rebase --continue`/`--abort`) then re-run `git queue move`");
    }
    if !git::worktree_clean() {
        bail!("working tree has uncommitted changes; commit or stash them before moving commits");
    }
    std::env::set_var(git::GUARD_ENV, "1");
    let queue = Queue::load()?;
    let original = git::current_branch()?;
    if !queue.is_tracked(&original) {
        bail!("`{original}` is not a queue branch");
    }
    let line = queue.line_through(&original)?;
    if let Some(fork) = &line.fork_at {
        eprintln!("warning: `{fork}` has multiple children; moving within this line only (other lines requeue on the next sync).");
    }
    let top = line.branches.last().unwrap().clone();

    // The line's commits, front-first, and their positions.
    let commits = git::commits_between(&line.base, &top)?;
    let pos: std::collections::HashMap<&str, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, (sha, _))| (sha.as_str(), i))
        .collect();
    let resolve = |rev: &str| -> Result<String> { resolve_queue_rev(&line, rev) };

    // <commit> or an inclusive <first>..<last> range.
    let (first, last) = match spec.split_once("..") {
        Some((a, b)) => (resolve(a)?, resolve(b)?),
        None => {
            let one = resolve(spec)?;
            (one.clone(), one)
        }
    };
    let in_line = |sha: &String, what: &str| -> Result<usize> {
        pos.get(sha.as_str()).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "{what} `{}` is not part of this queue (`{}`..`{top}`)",
                &sha[..8],
                line.base
            )
        })
    };
    let (mut ia, mut ib) = (in_line(&first, "commit")?, in_line(&last, "commit")?);
    if ia > ib {
        std::mem::swap(&mut ia, &mut ib);
    }

    // --new-parent: a queue commit outside the moved range, or the base tip
    // (which moves the range to the very front of the queue).
    let p = resolve(new_parent)?;
    let after = if p == git::rev_parse(&line.base)? {
        None
    } else {
        let ip = in_line(&p, "--new-parent")?;
        if (ia..=ib).contains(&ip) {
            bail!("--new-parent is inside the range being moved");
        }
        Some(ip)
    };

    let already = match after {
        None => ia == 0,
        Some(ip) => ip + 1 == ia,
    };
    if already {
        println!("Nothing to move: the commits already follow `{new_parent}`.");
        return Ok(());
    }

    let move_shas: Vec<String> = commits[ia..=ib].iter().map(|(s, _)| s.clone()).collect();
    let after_sha = after.map(|ip| commits[ip].0.clone());
    let tip_before = git::rev_parse(&top)?;

    git::rebase_reorder_persist(&line.base, &top, &move_shas, after_sha.as_deref())?;

    if git::rev_parse(&top)? == tip_before {
        bail!("the move did not apply (the rebase was aborted); the queue is unchanged");
    }

    // Refresh every branch's rebase anchor to its parent's new tip.
    for (i, br) in line.branches.iter().enumerate() {
        let parent = if i == 0 {
            line.base.clone()
        } else {
            line.branches[i - 1].clone()
        };
        meta::set_parent_sha(br, &git::rev_parse(&parent)?)?;
    }

    // The rebase leaves HEAD on the top branch; go back and refresh the
    // worktree (we required a clean tree above, so this is safe).
    git::checkout_quiet(&original)?;
    git::reset_hard_head()?;

    let dest = match &after {
        None => format!("the front of the queue (directly on `{}`)", line.base),
        Some(ip) => {
            let (sha, subject) = &commits[*ip];
            format!("directly after {} ({subject})", &sha[..8])
        }
    };
    println!("Moved {} commit(s) to {dest}.", move_shas.len());

    let mut conflicted = Vec::new();
    for (i, br) in line.branches.iter().enumerate() {
        if git::has_conflict_markers(br) {
            conflicted.push(br.clone());
        }
        let parent = if i == 0 {
            line.base.clone()
        } else {
            line.branches[i - 1].clone()
        };
        if git::rev_parse(br)? == git::rev_parse(&parent)? {
            eprintln!("note: `{br}` no longer has any commits of its own.");
        }
    }
    if conflicted.is_empty() {
        println!(
            "Run `git queue sync` (or `submit`) to push the rewritten queue and refresh its PRs."
        );
    } else {
        requeue::warn_conflicts(&conflicted);
    }
    Ok(())
}

/// Hidden `stamp-todo` subcommand: GIT_SEQUENCE_EDITOR for id stamping.
/// Marks the picks named by GIT_QUEUE_REWORD_SHAS as `reword`, so git stops
/// at each one and our GIT_EDITOR (add-queue-id) appends the trailer.
/// Untouched picks keep their SHAs where possible.
pub fn stamp_todo(path: &std::path::Path) -> Result<()> {
    let shas: Vec<String> = std::env::var("GIT_QUEUE_REWORD_SHAS")
        .context("GIT_QUEUE_REWORD_SHAS is not set")?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let todo = std::fs::read_to_string(path)?;
    let mut rewritten = Vec::new();
    let mut marked = 0usize;
    for line in todo.lines() {
        let mut it = line.split_whitespace();
        let is_target = it.next() == Some("pick")
            && it
                .next()
                .map(|abbrev| shas.iter().any(|f| f.starts_with(abbrev)))
                .unwrap_or(false);
        if is_target {
            marked += 1;
            rewritten.push(format!("reword{}", &line[4..]));
        } else {
            rewritten.push(line.to_string());
        }
    }
    if marked != shas.len() {
        bail!(
            "todo mismatch: expected {} pick(s) to reword, found {marked}",
            shas.len()
        );
    }
    std::fs::write(path, rewritten.join("\n") + "\n")?;
    Ok(())
}

/// Hidden `reorder-todo` subcommand: the GIT_SEQUENCE_EDITOR used by
/// [`git::rebase_reorder_persist`]. Relocates the pick lines named by
/// GIT_QUEUE_MOVE_SHAS to directly follow the pick for GIT_QUEUE_MOVE_AFTER
/// (or to the top of the todo when it is empty). `update-ref` lines stay put,
/// which is what carries branch membership.
pub fn reorder_todo(path: &std::path::Path) -> Result<()> {
    let shas: Vec<String> = std::env::var("GIT_QUEUE_MOVE_SHAS")
        .context("GIT_QUEUE_MOVE_SHAS is not set")?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let after = std::env::var("GIT_QUEUE_MOVE_AFTER").unwrap_or_default();

    let todo = std::fs::read_to_string(path)?;
    let pick_sha = |line: &str| -> Option<String> {
        let mut it = line.split_whitespace();
        (it.next() == Some("pick")).then(|| it.next().unwrap_or("").to_string())
    };
    // Todo picks use abbreviated SHAs of the original commits.
    let matches = |abbrev: &str, full: &str| !abbrev.is_empty() && full.starts_with(abbrev);

    let mut moved = Vec::new();
    let mut rest = Vec::new();
    for line in todo.lines() {
        let is_moved = pick_sha(line)
            .map(|a| shas.iter().any(|f| matches(&a, f)))
            .unwrap_or(false);
        if is_moved {
            moved.push(line.to_string());
        } else {
            rest.push(line.to_string());
        }
    }
    if moved.len() != shas.len() {
        bail!(
            "todo mismatch: expected {} pick(s) to move, found {}",
            shas.len(),
            moved.len()
        );
    }
    let idx = if after.is_empty() {
        0
    } else {
        match rest
            .iter()
            .position(|l| pick_sha(l).map(|a| matches(&a, &after)).unwrap_or(false))
        {
            Some(i) => i + 1,
            None => bail!("todo mismatch: --new-parent pick not found"),
        }
    };
    rest.splice(idx..idx, moved);
    std::fs::write(path, rest.join("\n") + "\n")?;
    Ok(())
}

/// Hidden `add-queue-id` subcommand: the commit-msg hook body. Stamps a
/// `Queued-Commit-Id` trailer on the message being committed, but only on tracked
/// queue branches and only for non-empty messages. Silent otherwise — it runs
/// on every commit in a hooked repo.
pub fn add_queue_id(path: &std::path::Path) -> Result<()> {
    if git::ensure_repo().is_err() {
        return Ok(());
    }
    // During an id-stamping rebase HEAD is detached; the driver vouches for
    // the commits instead of the branch check.
    let stamping = std::env::var("GIT_QUEUE_STAMP_ALL").is_ok();
    if !stamping {
        let Ok(branch) = git::current_branch() else {
            return Ok(());
        };
        if meta::parent(&branch).is_none() {
            return Ok(());
        }
    }
    let msg = std::fs::read_to_string(path).unwrap_or_default();
    let has_content = msg
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
    if !has_content {
        return Ok(());
    }
    git::add_trailer_to_file(path, &ident::new_id())
}

/// `git queue yank` — close every open (non-merged) PR in the current queue.
/// Merged PRs are left alone; local branches and metadata are untouched.
pub fn yank() -> Result<()> {
    git::ensure_repo()?;
    if !gh::ready() {
        bail!("`gh` is not installed or not authenticated; run `gh auth login`");
    }
    let queue = Queue::load()?;
    let current = git::current_branch()?;
    if !queue.is_tracked(&current) {
        bail!("`{current}` is not a queue branch");
    }
    let line = queue.line_through(&current)?;
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
        0 => println!("No open PRs in this queue to close."),
        n => println!("Closed {n} open PR(s). Merged PRs and local branches are untouched."),
    }
    Ok(())
}

/// `git queue commit [-m <msg>]` — make a NEW commit on the current branch,
/// then requeue all descendants onto the new tip (`git replay`, with a
/// marker-persisting fallback on conflict).
pub fn commit(message: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    // Suppress our own hooks for the internal git calls; we requeue explicitly.
    std::env::set_var(git::GUARD_ENV, "1");
    let queue = Queue::load()?;
    let current = git::current_branch()?;

    git::commit(message.as_deref())?;

    if !queue.is_tracked(&current) {
        return Ok(()); // not a queue branch; just a normal commit
    }
    // Change identity from birth: if the commit-msg hook isn't installed,
    // stamp the Queued-Commit-Id trailer here (before descendants requeue).
    if git::queue_id_of("HEAD").is_none() {
        git::amend_head_add_queue_id(&ident::new_id())?;
    }
    let report = requeue::propagate(&queue, &current)?;
    finish_requeue(&report, &current);
    Ok(())
}

/// `git queue amend` — fold STAGED changes into the current branch's tip commit
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
             Resolve the conflict on the descendant first, or use `git queue commit` to add a \
             separate commit instead."
        );
    }
    refresh_descendant_anchors(&current)?;
    println!("Amended the tip commit of `{current}` and updated all descendants.");
    Ok(())
}

/// `git queue reword [<commit>]` — rewrite a commit message via
/// `git history reword`, atomically updating descendants. Defaults to HEAD.
pub fn reword(commit: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let current = git::current_branch()?;
    let target = match commit {
        Some(c) if c.starts_with("q-") => {
            let queue = Queue::load()?;
            if !queue.is_tracked(&current) {
                bail!("`{current}` is not a queue branch; ids can only name queued commits");
            }
            let line = queue.line_through(&current)?;
            resolve_queue_rev(&line, &c)?
        }
        Some(c) => c,
        None => "HEAD".to_string(),
    };
    if git::history_reword(&target)? {
        bail!("reword aborted (rewriting `{target}` would conflict with a descendant); nothing changed");
    }
    refresh_descendant_anchors(&current)?;
    println!("Reworded `{target}` and updated descendants of `{current}`.");
    Ok(())
}

/// `git queue requeue` — requeue descendants of the current branch onto its
/// current tip. `--auto` (used by hooks) stays quiet when there's nothing to do
/// and on non-queue branches.
pub fn requeue(auto: bool) -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let queue = Queue::load()?;
    let current = git::current_branch()?;
    if !queue.is_tracked(&current) && current != queue.trunk {
        if auto {
            return Ok(());
        }
        bail!("`{current}` is not part of a queue");
    }
    let report = requeue::propagate(&queue, &current)?;
    if auto && report.is_empty() && report.conflicted.is_empty() {
        return Ok(());
    }
    finish_requeue(&report, &current);
    Ok(())
}

/// Report the outcome of a requeue (loud warning if markers were persisted).
fn finish_requeue(report: &requeue::Report, branch: &str) {
    if !report.conflicted.is_empty() {
        requeue::warn_conflicts(&report.conflicted);
    } else if !report.is_empty() {
        println!(
            "Requeueed {} descendant branch(es) of `{branch}`.",
            report.requeued.len()
        );
    }
}

/// After git-history rewrote `branch`'s history, its descendants' stored parent
/// anchors are stale; refresh them to the new parent tips.
fn refresh_descendant_anchors(branch: &str) -> Result<()> {
    let queue = Queue::load()?;
    for b in queue.descendants_topo(branch) {
        if let Some(parent) = queue.parent_of(&b) {
            let ptip = git::rev_parse(parent)?;
            meta::set_parent_sha(&b, &ptip)?;
        }
    }
    Ok(())
}

const HOOK_BEGIN: &str = "# >>> git-queue >>>";
const HOOK_END: &str = "# <<< git-queue <<<";

fn hook_snippet(only_amend: bool) -> String {
    let gate = if only_amend {
        "[ \"$1\" = \"amend\" ] || exit 0\n"
    } else {
        ""
    };
    format!(
        "{HOOK_BEGIN}\n\
         [ -n \"$GIT_QUEUE_IN_REQUEUE\" ] && exit 0\n\
         {gate}command -v git-queue >/dev/null 2>&1 && git-queue requeue --auto || true\n\
         {HOOK_END}\n"
    )
}

/// The commit-msg hook: stamp a `Queued-Commit-Id` trailer on commits made on queue
/// branches, so every change has a stable identity from birth.
fn id_hook_snippet() -> String {
    format!(
        "{HOOK_BEGIN}\n\
         command -v git-queue >/dev/null 2>&1 && git-queue add-queue-id \"$1\" || true\n\
         {HOOK_END}\n"
    )
}

/// `git queue hooks install` — make plain `git commit`/amend auto-requeue.
pub fn hooks_install() -> Result<()> {
    git::ensure_repo()?;
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-path", "hooks"])?);
    std::fs::create_dir_all(&dir)?;
    install_hook(&dir.join("post-commit"), &hook_snippet(false))?;
    install_hook(&dir.join("post-rewrite"), &hook_snippet(true))?;
    install_hook(&dir.join("commit-msg"), &id_hook_snippet())?;
    println!("Installed git-queue hooks. Plain `git commit` and `git commit --amend` on a");
    println!("queue branch will now auto-requeue descendants.");
    Ok(())
}

/// `git queue hooks uninstall` — remove the git-queue hook blocks.
pub fn hooks_uninstall() -> Result<()> {
    git::ensure_repo()?;
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-path", "hooks"])?);
    for name in ["post-commit", "post-rewrite", "commit-msg"] {
        remove_hook(&dir.join(name))?;
    }
    println!("Removed git-queue hooks.");
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
        });
        entries.push(Entry {
            branch: b.clone(),
            pr,
            // Detect markers live so `status` can never go stale.
            conflicted: git::has_conflict_markers(b),
            ids: None,
            commits: Vec::new(),
        });
    }
    Ok(entries)
}

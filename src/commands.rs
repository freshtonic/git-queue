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

/// Ask a yes/no question on the TTY; `default_yes` decides bare Enter.
fn confirm(question: &str, default_yes: bool) -> bool {
    use std::io::Write;
    print!("{question} [{}] ", if default_yes { "Y/n" } else { "y/N" });
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    match answer.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// `git queue setup [--yes] [--undo]` — interactive, per-step opt-in setup:
/// the git hooks, the merge-order gate, the Claude Code skill, and (when
/// other agents are detected) an AGENTS.md section. `--yes` accepts the two
/// repo-local steps non-interactively; the integrations always ask.
pub fn setup(yes: bool, undo: bool) -> Result<()> {
    git::ensure_repo()?;
    if undo {
        return setup_undo();
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    if !tty && !yes {
        bail!("`git queue setup` is interactive; pass --yes to accept the repo-local steps");
    }
    let ask = |q: &str| -> bool {
        if tty {
            confirm(q, true)
        } else {
            yes
        }
    };

    // 1. Hooks.
    if ask(
        "Install the git hooks? (plain `git commit`/`--amend` auto-requeue descendants;
new queue commits get a Stable-Commit-Id)",
    ) {
        hooks_install()?;
    }
    // 2. Merge-order gate.
    if ask(
        "Enable the merge-order gate? (submit/sync post a red/green commit status per PR
so reviewers see which PR merges next)",
    ) {
        meta::set_gate("status")?;
        println!("Merge-order gate enabled.");
    }
    // 3. Claude Code skill (only when Claude Code is present; interactive only).
    let home = std::env::var("HOME").unwrap_or_default();
    let claude_dir = std::path::Path::new(&home).join(".claude");
    if tty
        && claude_dir.exists()
        && confirm(
            "Claude Code detected. Install the `using-git-queue` skill so Claude drives
git-queue correctly?",
            true,
        )
    {
        let dir = claude_dir.join("skills").join("using-git-queue");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), EMBEDDED_SKILL)?;
        println!("Installed {}.", dir.join("SKILL.md").display());
    }
    // 4. Other agents -> AGENTS.md (the cross-agent convention).
    let others: Vec<&str> = [
        ("codex", ".codex"),
        ("cursor", ".cursor"),
        ("gemini", ".gemini"),
        ("windsurf", ".windsurf"),
        ("copilot", ".config/github-copilot"),
    ]
    .iter()
    .filter(|(_, d)| std::path::Path::new(&home).join(d).exists())
    .map(|(n, _)| *n)
    .collect();
    if tty
        && !others.is_empty()
        && confirm(
            &format!(
                "Detected other agents ({}). Add a git-queue section to this repo's AGENTS.md
(read by Codex, Cursor, Copilot and most agent CLIs)?",
                others.join(", ")
            ),
            true,
        )
    {
        write_agents_md_section()?;
    }
    println!(
        "
Setup done. `git queue doctor` reports the current state."
    );
    Ok(())
}

fn setup_undo() -> Result<()> {
    hooks_uninstall()?;
    let _ = git::ok(&["config", "--local", "--unset", "queue.gate"]);
    println!("Merge-order gate disabled.");
    let home = std::env::var("HOME").unwrap_or_default();
    let skill = std::path::Path::new(&home).join(".claude/skills/using-git-queue/SKILL.md");
    if skill.exists() {
        std::fs::remove_file(&skill).ok();
        println!("Removed {}.", skill.display());
    }
    strip_agents_md_section()?;
    Ok(())
}

const EMBEDDED_SKILL: &str = include_str!("../skills/using-git-queue/SKILL.md");
const AGENTS_BEGIN: &str = "<!-- git-queue:agents:begin -->";
const AGENTS_END: &str = "<!-- git-queue:agents:end -->";

/// Idempotently write a marker-delimited git-queue section into AGENTS.md.
fn write_agents_md_section() -> Result<()> {
    let section = format!(
        "{AGENTS_BEGIN}
## git-queue (PR queues)

         This repo uses git-queue for stacked/queued PRs. Rules:

         - See `git queue --help` (man page) for every command; `git queue status`/`log` show the queue.
         - Never hand-rebase a queue branch: use `git queue commit`/`amend`/`move`/`checkout`.
         - After changing history, run `git queue sync` to requeue, push (lease) and refresh PRs.
         - PRs merge front-first; never merge a PR whose `git-queue/merge-order` status is red.
         - Commits carry `Stable-Commit-Id:` trailers — preserve commit messages when rewriting.
         {AGENTS_END}
"
    );
    let path = std::path::Path::new("AGENTS.md");
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = match (existing.find(AGENTS_BEGIN), existing.find(AGENTS_END)) {
        (Some(a), Some(b)) if b > a => format!(
            "{}{}{}",
            &existing[..a],
            section.trim_end(),
            &existing[b + AGENTS_END.len()..]
        ),
        _ if existing.is_empty() => section,
        _ => format!(
            "{}
{}",
            existing.trim_end(),
            section
        ),
    };
    std::fs::write(path, updated)?;
    println!("Wrote the git-queue section of AGENTS.md.");
    Ok(())
}

fn strip_agents_md_section() -> Result<()> {
    let path = std::path::Path::new("AGENTS.md");
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    if let (Some(a), Some(b)) = (existing.find(AGENTS_BEGIN), existing.find(AGENTS_END)) {
        if b > a {
            let rest = format!("{}{}", &existing[..a], &existing[b + AGENTS_END.len()..]);
            if rest.trim().is_empty() {
                std::fs::remove_file(path).ok();
            } else {
                std::fs::write(path, rest)?;
            }
            println!("Removed the git-queue section of AGENTS.md.");
        }
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
            println!("  ! gate: unknown mode `{other}` \u{2014} run `git queue setup` to (re)enable status mode");
        }
        None => {
            println!("  \u{2717} gate: not enabled \u{2014} run `git queue setup` to turn it on");
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

/// `git queue create <name> [--base <branch>]` — new branch queued after the
/// current one (or on an explicit `--base` branch).
pub fn create(name: &str, base: Option<&str>, queue_flag: Option<&str>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    let parent = match base {
        Some(b) => {
            let b = resolve_branch_arg(b)?;
            if !git::branch_exists(&b) {
                bail!("base branch `{b}` does not exist");
            }
            b
        }
        None => git::current_branch()?,
    };
    // Every queue is named: inherit when extending, otherwise ask/take one.
    // `namespaced` decides whether the new branch lives under queue/<name>/…:
    // true when the queue name is explicit or the queue already follows the
    // convention — so you type short names and never the prefix (as with
    // split), and plain-named queues stay plain.
    let (qname, namespaced) = if meta::parent(&parent).is_some() {
        let q = Queue::load()?;
        let line = q.line_through(&parent)?;
        match line_queue_name(&line) {
            Some(n) => {
                let ns = line
                    .branches
                    .first()
                    .map(|b| b.starts_with("queue/"))
                    .unwrap_or(false);
                (n, ns)
            }
            None => bail!("this queue has no name; run `git queue name <name>` first"),
        }
    } else {
        require_queue_name(queue_flag, name)?
    };
    let name = if name.contains('/') || !namespaced {
        name.to_string()
    } else {
        format!("queue/{qname}/{name}")
    };
    let name = name.as_str();
    if git::branch_exists(name) {
        bail!("branch `{name}` already exists");
    }
    let parent_sha = git::rev_parse(&parent)?;

    git::create_branch(name, &parent)?;
    meta::set_parent(name, &parent)?;
    meta::set_parent_sha(name, &parent_sha)?;
    meta::set_branch_queue(name, &qname)?;
    meta::touch_queue(&qname);
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
pub fn split(delete_original: bool, queue_flag: Option<&str>) -> Result<()> {
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
    // Every queue is named, and split's branches live under queue/<name>/…
    let qname = if queue.is_tracked(&branch) {
        match line_queue_name(&queue.line_through(&branch)?) {
            Some(n) => n,
            None => require_queue_name(queue_flag, &branch)?.0,
        }
    } else {
        require_queue_name(queue_flag, &branch)?.0
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
    // Segment branches live under the queue's namespace; names the user
    // already fully qualified are kept as-is.
    let segments: Vec<(String, String)> = segments
        .into_iter()
        .map(|(n, sha)| {
            let full = if n.starts_with("queue/") || n == branch {
                n
            } else {
                format!("queue/{qname}/{n}")
            };
            (full, sha)
        })
        .collect();

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
        meta::set_branch_queue(name, &qname)?;
        parent = name.clone();
    }
    meta::touch_queue(&qname);

    let top = segments.last().unwrap().0.clone();
    git::checkout(&top)?;

    println!("Split `{branch}` into {} queued branches:", segments.len());
    let mut p = base.clone();
    for (name, _) in &segments {
        println!("  {p} ← {name}");
        p = name.clone();
    }

    // If the original branch wasn't reused as a segment name, it's now fully
    // redundant: the last segment's tip IS the old tip, so the old ref merely
    // duplicates it (and any queue config it carried would read as a phantom
    // fork). Untrack it, and offer to delete it.
    let reused = segments.iter().any(|(n, _)| n == &branch);
    if !reused {
        meta::untrack(&branch);
        let delete = delete_original
            || (std::io::IsTerminal::is_terminal(&std::io::stdin()) && {
                print!(
                    "`{branch}` is now fully covered by `{top}` — delete the old branch? [Y/n] "
                );
                use std::io::Write;
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).ok();
                matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
            });
        if delete {
            git::run(&["branch", "-q", "-D", &branch])?;
            println!("Deleted `{branch}`.");
            if git::remote_branch(&meta::remote(), &branch).is_some() {
                println!(
                    "note: it still exists on the remote; remove it with `git push {} --delete {branch}`.",
                    meta::remote()
                );
            }
        } else {
            println!("note: `{branch}` kept; it duplicates `{top}` — delete it whenever with `git branch -D {branch}`.");
        }
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
/// to stamp `Stable-Commit-Id` trailers onto the adopted commits (a history rewrite,
/// so it asks first; `--stamp-ids`/`--no-stamp-ids` decide non-interactively).
pub fn track(
    parent: Option<String>,
    stamp_ids: bool,
    no_stamp_ids: bool,
    split_after: bool,
    delete_original: bool,
    queue_flag: Option<&str>,
) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    let branch = git::current_branch()?;
    if branch == trunk {
        bail!("cannot track the trunk branch itself");
    }
    if split_after && !git::worktree_clean() {
        bail!("working tree has uncommitted changes; commit or stash them before `track --split`");
    }
    let parent = match parent {
        Some(p) => resolve_branch_arg(&p)?,
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
    // Every queue is named: inherit when adopting into an existing queue,
    // otherwise ask/take one.
    let qname = {
        let q = Queue::load()?;
        let line = q.line_through(&branch)?;
        match line_queue_name(&line) {
            Some(n) => n,
            None => require_queue_name(queue_flag, &branch)?.0,
        }
    };
    meta::set_branch_queue(&branch, &qname)?;
    meta::touch_queue(&qname);
    println!("Tracking `{branch}` with parent `{parent}` in queue `{qname}`.");

    // Offer stable change identity to the adopted commits.
    let missing: Vec<String> = git::queue_ids(&format!("{base}..{branch}"))?
        .into_iter()
        .filter(|(_, id)| id.is_none())
        .map(|(sha, _)| sha)
        .collect();
    if missing.is_empty() {
        return split_if_requested(split_after, delete_original, &base, &branch);
    }
    let n = missing.len();
    let stamp = if stamp_ids {
        true
    } else if no_stamp_ids {
        false
    } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!();
        println!("`{branch}` has {n} commit(s) without a Stable-Commit-Id (stable change identity");
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
            "note: {n} commit(s) have no Stable-Commit-Id; re-run `git queue track --stamp-ids` to \
             stamp them (rewrites their hashes)."
        );
        false
    };
    if !stamp {
        return split_if_requested(split_after, delete_original, &base, &branch);
    }
    if !git::worktree_clean() {
        eprintln!("note: working tree has uncommitted changes; skipping id stamping. Commit or");
        eprintln!("stash, then re-run `git queue track --stamp-ids`.");
        return split_if_requested(split_after, delete_original, &base, &branch);
    }
    git::rebase_stamp_ids(&base, &branch, &missing)?;
    println!("Stamped {n} commit(s) with Stable-Commit-Ids (their hashes changed).");
    split_if_requested(split_after, delete_original, &base, &branch)
}

/// The `--split` tail of `track`: hand off to the split editor, unless the
/// adopted branch is too small to divide.
fn split_if_requested(
    requested: bool,
    delete_original: bool,
    base: &str,
    branch: &str,
) -> Result<()> {
    if !requested {
        return Ok(());
    }
    if git::ahead_count(base, branch)? < 2 {
        println!("`{branch}` has fewer than 2 commits — nothing to split.");
        return Ok(());
    }
    split(delete_original, None)
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
    let line = queue.line_through(&branch)?;
    let Some(qname) = line_queue_name(&line) else {
        bail!("this queue has no name yet; run `git queue name <name>` first");
    };
    let text = match message {
        Some(m) => m,
        None => edit_description(
            &format!("queue `{qname}`"),
            meta::queue_description(&qname).as_deref(),
            "the whole queue (the \"About this queue\" section of every PR in it)",
        )?,
    };
    meta::set_queue_description(&qname, &text)?;
    meta::touch_queue(&qname);
    if text.trim().is_empty() {
        println!("Cleared the description of queue `{qname}`.");
    } else {
        println!("Saved the description of queue `{qname}`. Every PR in the queue shows it");
        println!("under \"About this queue\" on the next `git queue submit`/`sync`.");
    }
    Ok(())
}

/// `git queue describe-branch [-m <text>]` — describe what the current branch
/// is about; becomes the "About this branch" section of its PR.
pub fn describe_branch(message: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let branch = git::current_branch()?;
    if !queue.is_tracked(&branch) {
        bail!("`{branch}` is not a queue branch; `git queue create`/`track` it first");
    }
    let text = match message {
        Some(m) => m,
        None => edit_description(
            &format!("`{branch}`"),
            meta::description(&branch).as_deref(),
            "this branch (the \"About this branch\" section of its PR)",
        )?,
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

/// `git queue name [<name>]` — show or set the current queue's name. Setting
/// records membership on every branch of the line.
pub fn name(new_name: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let branch = git::current_branch()?;
    if !queue.is_tracked(&branch) {
        bail!("`{branch}` is not a queue branch");
    }
    let line = queue.line_through(&branch)?;
    match new_name {
        None => match line_queue_name(&line) {
            Some(n) => println!("{n}"),
            None => println!("(this queue has no name; set one with `git queue name <name>`)"),
        },
        Some(n) => {
            meta::validate_queue_name(&n)?;
            for b in &line.branches {
                meta::set_branch_queue(b, &n)?;
            }
            meta::touch_queue(&n);
            println!("Named this queue `{n}` ({} branches).", line.branches.len());
        }
    }
    Ok(())
}

/// `git queue ls` — every queue in the repo, most recently touched first.
pub fn ls() -> Result<()> {
    git::ensure_repo()?;
    let queue = Queue::load()?;
    let current = git::current_branch().ok();

    // Group tracked lines by queue name (leaf-per-line; forks share a name).
    let mut queues: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut unnamed: Vec<String> = Vec::new();
    for leaf in queue.leaves() {
        let Ok(line) = queue.line_through(&leaf) else {
            continue;
        };
        match line_queue_name(&line) {
            Some(n) => {
                let e = queues.entry(n).or_default();
                for b in &line.branches {
                    if !e.contains(b) {
                        e.push(b.clone());
                    }
                }
            }
            None => unnamed.push(line.branches.first().cloned().unwrap_or(leaf)),
        }
    }
    // Named queues with metadata but no live branches still show up.
    for n in meta::all_queue_names() {
        queues.entry(n).or_default();
    }
    if queues.is_empty() && unnamed.is_empty() {
        println!("No queues yet. Create one with `git queue create <name>`.");
        return Ok(());
    }
    let mut ordered: Vec<(String, Vec<String>)> = queues.into_iter().collect();
    ordered.sort_by_key(|(n, _)| std::cmp::Reverse(meta::queue_touched_at(n)));
    for (n, branches) in &ordered {
        let here = current
            .as_deref()
            .map(|c| branches.iter().any(|b| b == c))
            .unwrap_or(false);
        let marker = if here { "  ← current" } else { "" };
        let desc = meta::queue_description(n)
            .map(|d| {
                let first = d.lines().next().unwrap_or("").trim().to_string();
                format!("  — {first}")
            })
            .unwrap_or_default();
        println!(
            "{n}  ({} branch{}){desc}{marker}",
            branches.len(),
            if branches.len() == 1 { "" } else { "es" }
        );
        for b in branches {
            println!("    {b}");
        }
    }
    for front in unnamed {
        println!("(unnamed queue starting at `{front}` — run `git queue name <name>` from it)");
    }
    Ok(())
}

/// Open the user's git editor on a temp file seeded with `existing`, and return
/// the edited text (lines starting with `#` are stripped as comments).
fn edit_description(what: &str, existing: Option<&str>, becomes: &str) -> Result<String> {
    let dir = std::path::PathBuf::from(git::out(&["rev-parse", "--git-dir"])?);
    let path = dir.join("QUEUE_DESCRIBE");
    let template = format!(
        "{}\n\n# Describe {what}. This becomes the PR text for {becomes}.\n\
         # Lines starting with '#' are ignored.\n",
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
/// level beneath it, newest first, each prefixed by its abbreviated Stable-Commit-Id.
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

    // Show the WHOLE tree the anchor belongs to, forks included: the current
    // chain renders at indent 0, and each forked subtree renders one level in,
    // directly above the branch it forks from.
    let chain = queue.chain_to_base(&anchor)?;
    let root = chain[0].clone();
    let base = queue
        .parent_of(&root)
        .expect("root is tracked, so it has a parent")
        .to_string();
    fn topdown(
        queue: &Queue,
        branch: &str,
        indent: usize,
        chain: &[String],
        out: &mut Vec<(String, usize)>,
    ) {
        let kids = queue.children(branch);
        let main = kids
            .iter()
            .find(|k| chain.contains(k))
            .or_else(|| kids.first())
            .cloned();
        if let Some(m) = &main {
            topdown(queue, m, indent, chain, out);
        }
        for k in kids.iter().filter(|k| Some(*k) != main.as_ref()) {
            topdown(queue, k, indent + 1, chain, out);
        }
        out.push((branch.to_string(), indent));
    }
    let mut ordered: Vec<(String, usize)> = Vec::new();
    topdown(&queue, &root, 0, &chain, &mut ordered);

    let branches: Vec<String> = ordered.iter().map(|(b, _)| b.clone()).collect();
    let mut entries = build_entries(&branches)?;
    for (e, (_, indent)) in entries.iter_mut().zip(&ordered) {
        e.indent = *indent;
        let parent = queue
            .parent_of(&e.branch)
            .expect("tracked branch has a parent")
            .to_string();
        if e.conflicted {
            e.conflicts = git::conflict_files(&e.branch);
        }
        if with_commits {
            if let Ok(commits) = git::commits_with_ids(&format!("{parent}..{}", e.branch)) {
                e.commits = commits;
            }
        }
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let color = tty && std::env::var_os("NO_COLOR").is_none();
    let repo_url = if tty && terminal_renders_hyperlinks() {
        git::github_repo_url(&meta::remote())
    } else {
        None
    };
    print!(
        "{}",
        render::status_tree(
            &entries,
            &current,
            &base,
            base == queue.trunk,
            color,
            repo_url.as_deref()
        )
    );
    Ok(())
}

/// Best-effort detection of OSC 8 hyperlink support — there is no capability
/// query, so this is the allowlist heuristic other CLIs use.
fn terminal_renders_hyperlinks() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let term_program = var("TERM_PROGRAM");
    if matches!(
        term_program.as_str(),
        "iTerm.app" | "WezTerm" | "ghostty" | "Hyper" | "vscode" | "Tabby"
    ) {
        return true;
    }
    if !var("KITTY_WINDOW_ID").is_empty() || !var("WT_SESSION").is_empty() {
        return true;
    }
    if var("VTE_VERSION")
        .parse::<u32>()
        .map(|v| v >= 5000)
        .unwrap_or(false)
    {
        return true;
    }
    if var("KONSOLE_VERSION")
        .parse::<u32>()
        .map(|v| v >= 201100)
        .unwrap_or(false)
    {
        return true;
    }
    let term = var("TERM");
    term.contains("kitty") || term.contains("wezterm") || term.contains("foot")
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

    // Stamp Stable-Commit-Ids onto any queue commits that lack them: identity
    // is one of the invariants sync converges. Message-only rewrites, so no
    // conflicts are possible; --update-refs carries branch refs along.
    for leaf in queue.leaves() {
        let Ok(line) = queue.line_through(&leaf) else {
            continue;
        };
        let top = line.branches.last().unwrap().clone();
        let Ok(ids) = git::queue_ids(&format!("{}..{top}", line.base)) else {
            continue;
        };
        let missing: Vec<String> = ids
            .into_iter()
            .filter(|(_, id)| id.is_none())
            .map(|(sha, _)| sha)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let n = missing.len();
        git::rebase_stamp_ids(&line.base, &top, &missing)?;
        for (i, br) in line.branches.iter().enumerate() {
            let parent = if i == 0 {
                line.base.clone()
            } else {
                line.branches[i - 1].clone()
            };
            meta::set_parent_sha(br, &git::rev_parse(&parent)?)?;
        }
        println!("Stamped {n} commit(s) with Stable-Commit-Ids (queue ending at `{top}`).");
    }
    git::checkout_quiet(&original)?;

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
            if push_would_mislabel_child(&queue, &branch) {
                push_failures += 1;
                continue;
            }
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
            if line_queue_name(&line).is_none() {
                eprintln!(
                    "warning: skipping PR reconciliation for the queue ending at `{leaf}` — it \
                     has no name. Run `git queue name <name>` from one of its branches."
                );
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

/// Refuse to push a branch whose tip contains an OPEN child PR's head tip.
/// GitHub marks a PR merged the instant its base branch contains its head —
/// permanently — so pushing a collapsed parent (or the parent of a branch
/// whose commits were all moved away) would mislabel a mid-queue PR as merged
/// while the front of the queue is still open. Returns true if pushing is safe.
fn push_would_mislabel_child(queue: &Queue, branch: &str) -> bool {
    for child in queue.children(branch) {
        let (Ok(ct), Ok(bt)) = (git::rev_parse(&child), git::rev_parse(branch)) else {
            continue;
        };
        if !git::is_ancestor(&ct, &bt) {
            continue; // normal queue shape: child extends parent
        }
        let open_pr = gh::find(&child)
            .ok()
            .flatten()
            .map(|pr| pr.state == "OPEN")
            .unwrap_or(false);
        if open_pr {
            eprintln!(
                "warning: not pushing `{branch}` — its tip contains the head of `{child}`'s \
                 OPEN PR, and GitHub would permanently mark that PR as merged. The queue \
                 looks collapsed (or `{child}` has no commits of its own); untangle it with \
                 `git queue move`, or close the child PR first."
            );
            return true;
        }
    }
    false
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
/// Stable-Commit-Id correspondence — which survives squash merges that destroy both
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
        "has landed on trunk (Stable-Commit-Ids found)",
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
    // Diverged: pull in only what is genuinely new. Stable-Commit-Id correspondence
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

/// The name of the queue a line belongs to: recorded membership first, then
/// the `queue/<name>/…` branch-naming convention.
fn line_queue_name(line: &Line) -> Option<String> {
    for b in &line.branches {
        if let Some(n) = meta::branch_queue(b) {
            return Some(n);
        }
    }
    for b in &line.branches {
        if let Some(rest) = b.strip_prefix("queue/") {
            if let Some((n, _)) = rest.split_once('/') {
                return Some(n.to_string());
            }
        }
    }
    None
}

/// Ask for (or take) a queue name, mandatorily. Order: explicit flag, TTY
/// prompt, then — non-interactive with no flag — a fallback so scripts keep
/// working, announced loudly.
fn require_queue_name(flag: Option<&str>, fallback: &str) -> Result<(String, bool)> {
    if let Some(n) = flag {
        meta::validate_queue_name(n)?;
        return Ok((n.to_string(), true));
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        print!("Name this queue: ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        let answer = answer.trim().to_string();
        if !answer.is_empty() {
            meta::validate_queue_name(&answer)?;
            return Ok((answer, true));
        }
    }
    let fallback = fallback.replace('/', "-");
    meta::validate_queue_name(&fallback)?;
    eprintln!("note: queue named `{fallback}` (rename any time with `git queue name <name>`).");
    Ok((fallback, false))
}

/// Resolve a branch argument, accepting short names inside namespaced queues:
/// an exact branch name wins; otherwise a unique `queue/*/<arg>` match does.
fn resolve_branch_arg(arg: &str) -> Result<String> {
    if git::branch_exists(arg) {
        return Ok(arg.to_string());
    }
    let suffix = format!("/{arg}");
    let matches: Vec<String> = meta::tracked_branches()
        .into_iter()
        .filter(|b| b.ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Ok(arg.to_string()), // let the caller produce its natural error
        many => bail!("`{arg}` is ambiguous: {}", many.join(", ")),
    }
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
    let Some(queue_name) = line_queue_name(line) else {
        bail!("this queue has no name (needed for its PRs); run `git queue name <name>` first");
    };
    let queue_description = meta::queue_description(&queue_name).unwrap_or_default();
    meta::touch_queue(&queue_name);
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
            // Don't hand GitHub a reason to mislabel the next PR as merged:
            // pushing a branch whose tip contains an open child PR's head
            // makes that permanent.
            let collapsed_child = branches.get(i + 1).is_some_and(|child| {
                existing[i + 1]
                    .as_ref()
                    .map(|pr| pr.state == "OPEN")
                    .unwrap_or(false)
                    && match (git::rev_parse(child), git::rev_parse(b)) {
                        (Ok(ct), Ok(bt)) => git::is_ancestor(&ct, &bt),
                        _ => false,
                    }
            });
            if collapsed_child {
                eprintln!(
                    "warning: not pushing `{b}` — its tip contains the head of the next \
                     PR in the queue, and GitHub would permanently mark that PR as merged. \
                     Untangle with `git queue move`, or close the child PR first."
                );
            } else {
                println!("Pushing `{b}`...");
                git::push(remote, b)?;
            }
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
            conflicts: Vec::new(),
            commits: Vec::new(),
            indent: 0,
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
        let nav = render::nav_block(&entries, b, &line.base, &queue_name);
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
        let body = render::compose_body(&queue_description, &description, &nav);
        gh::edit(number, &base_of(i), &title, &body)?;
    }

    Ok(LinePrs { entries, prs })
}

/// Apply the status gate (if enabled) and print the line's PR listing.
fn report_line(line: &Line, outcome: &LinePrs, heading: &str) -> Result<()> {
    let gate = meta::gate();
    let gated = gate.as_deref() == Some("status");
    if let Some(other) = gate.as_deref().filter(|g| *g != "status") {
        eprintln!("warning: unknown queue.gate mode `{other}` — no merge gate applied; run `git queue setup` to enable status mode");
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
/// a `Stable-Commit-Id` — full, or a unique prefix such as the abbreviated
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
                bail!("no commit in this queue has Stable-Commit-Id `{arg}`");
            }
            many => bail!(
                "Stable-Commit-Id prefix `{arg}` is ambiguous ({} matches); use more characters",
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
/// `Stable-Commit-Id` trailer on the message being committed, but only on tracked
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
        match git::current_branch() {
            Ok(branch) => {
                if meta::parent(&branch).is_none() {
                    return Ok(());
                }
            }
            // Detached: stamp only inside a queue-editing session, where a
            // plain `git commit` inserts a new queue commit.
            Err(_) => {
                if meta::detached_state().is_none() {
                    return Ok(());
                }
            }
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

/// `git queue checkout <commit>` — detach HEAD on a commit of the current
/// queue (named by SHA or Stable-Commit-Id) for in-place editing. From there,
/// plain `git commit` INSERTS a new commit after it and `git commit --amend`
/// REVISES it (message — and Stable-Commit-Id — carried over); either way the
/// rest of the queue rebases on top, via the hooks or `git queue requeue`.
pub fn checkout(arg: &str) -> Result<()> {
    git::ensure_repo()?;
    std::env::set_var(git::GUARD_ENV, "1");
    let queue = Queue::load()?;

    // The line in play: from the current branch, or — already detached via a
    // previous `git queue checkout` — from the recorded top branch.
    let line = if let Ok(current) = git::current_branch() {
        if !queue.is_tracked(&current) {
            bail!("`{current}` is not a queue branch");
        }
        queue.line_through(&current)?
    } else if let Some((_, top)) = meta::detached_state() {
        queue.line_through(&top)?
    } else {
        bail!("HEAD is detached outside a queue-editing session; check out a queue branch first");
    };
    let top = line.branches.last().unwrap().clone();

    // Checking out a branch of the line reattaches and ends the session
    // (short names resolve inside namespaced queues).
    let reattach = line
        .branches
        .iter()
        .find(|b| *b == arg || b.ends_with(&format!("/{arg}")))
        .cloned()
        .or_else(|| (arg == line.base).then(|| line.base.clone()));
    if let Some(target) = reattach {
        let arg = target.as_str();
        if !git::tracked_clean() {
            bail!("stage or tracked files have changes; commit or stash them first");
        }
        git::checkout_quiet(arg)?;
        meta::clear_detached_state();
        println!("Back on `{arg}`.");
        return Ok(());
    }

    let sha = resolve_queue_rev(&line, arg)?;
    let commits = git::commits_between(&line.base, &top)?;
    if !commits.iter().any(|(s, _)| s == &sha) {
        bail!(
            "`{arg}` is not a commit of this queue (`{}`..`{top}`)",
            line.base
        );
    }
    if !git::tracked_clean() {
        bail!(
            "stage or tracked files have changes; commit or stash them before `git queue checkout`"
        );
    }

    git::run(&["checkout", "-q", "--detach", &sha])?;
    meta::set_detached_state(&sha, &top)?;
    let subject = git::tip_subject("HEAD")?;
    println!("Detached at {} ({subject}).", &sha[..8]);
    println!("Edit away — `git add` then:");
    println!("  git commit           inserts a NEW commit right after this one");
    println!("  git commit --amend   revises this commit (its Stable-Commit-Id is kept)");
    println!("The rest of the queue rebases on top automatically (with hooks installed);");
    println!("otherwise run `git queue requeue`. Return with `git queue checkout {top}`.");
    Ok(())
}

/// Reintegrate after editing at a detached queue commit: rebase everything
/// that followed the original commit onto the new HEAD (branch refs ride
/// along via --update-refs), re-anchor the line, and stay detached at the
/// new commit so editing can continue.
fn reintegrate_detached(auto: bool, original: &str, top: &str) -> Result<()> {
    let head = git::rev_parse("HEAD")?;
    if head == original {
        if !auto {
            println!("Nothing to reintegrate: HEAD is still the checked-out commit.");
        }
        return Ok(());
    }
    git::rebase_persist(&head, original, top)?;
    // The rebase leaves HEAD on `top`; go back to the edited commit.
    git::run(&["checkout", "-q", "--detach", &head])?;
    meta::set_detached_state(&head, top)?;

    let queue = Queue::load()?;
    let line = queue.line_through(top)?;
    let mut conflicted = Vec::new();
    for (i, br) in line.branches.iter().enumerate() {
        let parent = if i == 0 {
            line.base.clone()
        } else {
            line.branches[i - 1].clone()
        };
        meta::set_parent_sha(br, &git::rev_parse(&parent)?)?;
        if git::has_conflict_markers(br) {
            conflicted.push(br.clone());
        }
    }
    if let Some(qname) = line_queue_name(&line) {
        meta::touch_queue(&qname);
    }
    println!(
        "Reintegrated: the rest of the queue is rebased onto {} — still detached here.",
        &head[..8]
    );
    if conflicted.is_empty() {
        println!(
            "When you're done editing: `git queue checkout {top}` to reattach, then \
             `git queue sync` to push and refresh PRs."
        );
    } else {
        requeue::warn_conflicts(&conflicted);
    }
    Ok(())
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
    // stamp the Stable-Commit-Id trailer here (before descendants requeue).
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
    // A queue-editing session (git queue checkout) reintegrates instead.
    if git::current_branch().is_err() {
        if let Some((original, top)) = meta::detached_state() {
            return reintegrate_detached(auto, &original, &top);
        }
        if auto {
            return Ok(());
        }
        bail!("HEAD is detached; check out a branch first");
    }
    let queue = Queue::load()?;
    let current = git::current_branch()?;
    meta::clear_detached_state(); // attached again: any old session is over
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

/// The commit-msg hook: stamp a `Stable-Commit-Id` trailer on commits made on queue
/// branches, so every change has a stable identity from birth.
fn id_hook_snippet() -> String {
    format!(
        "{HOOK_BEGIN}\n\
         command -v git-queue >/dev/null 2>&1 && git-queue add-queue-id \"$1\" || true\n\
         {HOOK_END}\n"
    )
}

/// `git queue hooks install` — make plain `git commit`/amend auto-requeue.
fn hooks_install() -> Result<()> {
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
fn hooks_uninstall() -> Result<()> {
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
            conflicts: Vec::new(),
            commits: Vec::new(),
            indent: 0,
        });
    }
    Ok(entries)
}

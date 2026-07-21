//! Thin wrappers over the `git` executable.
//!
//! We shell out rather than link a git library: rebase/conflict semantics are
//! then exactly what the user would get by hand, and the dependency surface
//! stays tiny.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Env var set on child git processes while git-queue is requeueing, so our
/// own hooks can detect the reentry and skip (avoiding infinite recursion).
pub const GUARD_ENV: &str = "GIT_QUEUE_IN_REQUEUE";

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

pub fn checkout_quiet(branch: &str) -> Result<()> {
    run(&["checkout", "-q", branch])
}

/// Snap index and worktree to HEAD. Discards local changes — callers must
/// ensure the worktree was clean before the refs moved under it.
pub fn reset_hard_head() -> Result<()> {
    run(&["reset", "-q", "--hard"])
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

/// Commits in `base..tip`, oldest first, as `(full_sha, subject)` pairs.
pub fn commits_between(base: &str, tip: &str) -> Result<Vec<(String, String)>> {
    let raw = out(&[
        "log",
        "--reverse",
        "--format=%H%x09%s",
        &format!("{base}..{tip}"),
    ])?;
    Ok(raw
        .lines()
        .filter_map(|l| {
            let (sha, subject) = l.split_once('\t')?;
            Some((sha.to_string(), subject.to_string()))
        })
        .collect())
}

/// True if the index has no staged changes and no tracked file is modified
/// (untracked files are allowed).
pub fn tracked_clean() -> bool {
    out(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| s.is_empty())
        .unwrap_or(false)
}

/// True if the work tree and index are clean.
pub fn worktree_clean() -> bool {
    out(&["status", "--porcelain"])
        .map(|s| s.is_empty())
        .unwrap_or(false)
}

/// Detach HEAD at its current commit (so no branch ref is "checked out").
pub fn detach_head() -> Result<()> {
    run(&["checkout", "-q", "--detach"])
}

/// True if a rebase (merge or apply backend) is currently in progress.
pub fn rebase_in_progress() -> bool {
    let dir = match out(&["rev-parse", "--git-dir"]) {
        Ok(d) => PathBuf::from(d),
        Err(_) => return false,
    };
    dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists()
}

/// Fetch with `--prune`: stale remote-tracking refs for branches deleted on
/// the remote (e.g. auto-deleted when their PR merged) must not survive, or
/// sync would "pull" from ghost branches and push with dead leases.
pub fn fetch(remote: &str) -> Result<()> {
    run(&["fetch", "--prune", remote])
}

/// SHA of a remote-tracking branch `<remote>/<branch>`, if it exists.
pub fn remote_branch(remote: &str, branch: &str) -> Option<String> {
    let r = format!("{remote}/{branch}");
    out(&["rev-parse", "--verify", "--quiet", &r])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Fast-forward the *currently checked-out* branch to `target` (updates the
/// work tree). Fails if it isn't a fast-forward.
pub fn merge_ff_only(target: &str) -> Result<()> {
    run(&["merge", "--ff-only", target])
}

/// Force-with-lease push, setting upstream. Shows git's own progress output.
pub fn push(remote: &str, branch: &str) -> Result<()> {
    run(&["push", "--force-with-lease", "-u", remote, branch])
}

/// Move a branch ref to `sha` without checking it out.
pub fn force_ref(branch: &str, sha: &str) -> Result<()> {
    run(&["update-ref", &format!("refs/heads/{branch}"), sha])
}

/// True if there are staged changes in the index.
pub fn staged_changes() -> bool {
    // `git diff --cached --quiet` exits 1 when there is something staged.
    !ok(&["diff", "--cached", "--quiet"])
}

/// The https URL of the GitHub repo behind `remote`, parsed from its URL
/// (ssh or https form), if it is a GitHub remote.
pub fn github_repo_url(remote: &str) -> Option<String> {
    let url = out(&["remote", "get-url", remote]).ok()?;
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))?;
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    Some(format!("https://github.com/{path}"))
}

/// Paths in `rev`'s tree that contain conflict markers.
pub fn conflict_files(rev: &str) -> Vec<String> {
    out(&["grep", "-I", "-l", "-e", "^<<<<<<< ", rev])
        .map(|raw| {
            raw.lines()
                .filter_map(|l| l.split_once(':').map(|(_, p)| p.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// True if the tree at `rev` contains textual conflict markers.
pub fn has_conflict_markers(rev: &str) -> bool {
    ok(&["grep", "-I", "-l", "-e", "^<<<<<<< ", rev])
}

/// Make a normal commit on the current branch.
pub fn commit(message: Option<&str>) -> Result<()> {
    // Suppress our own hooks during the internal commit; git-queue does the
    // requeue itself right after.
    let mut cmd = Command::new("git");
    cmd.env(GUARD_ENV, "1");
    match message {
        Some(m) => cmd.args(["commit", "-m", m]),
        None => cmd.args(["commit"]),
    };
    let status = cmd.status().context("failed to spawn `git commit`")?;
    if !status.success() {
        bail!("`git commit` failed");
    }
    Ok(())
}

/// `git history fixup <commit>` — fold staged changes into `commit`, atomically
/// updating every descendant branch. Returns `true` if it aborted because the
/// rewrite would conflict with a descendant (git history is atomic and cannot
/// persist markers). Any other failure is an error.
pub fn history_fixup(commit: &str) -> Result<bool> {
    let out = Command::new("git")
        .args(["history", "fixup", commit])
        .env(GUARD_ENV, "1")
        .output()
        .context("failed to spawn `git history fixup`")?;
    if out.status.success() {
        return Ok(false);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("conflict") {
        return Ok(true);
    }
    bail!("`git history fixup` failed:\n{}", err.trim());
}

/// `git history reword <commit>` — rewrite a commit message (opens the editor),
/// atomically updating descendants. Returns `true` on conflict abort.
pub fn history_reword(commit: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["history", "reword", commit])
        .env(GUARD_ENV, "1")
        .status()
        .context("failed to spawn `git history reword`")?;
    // reword can only conflict via replay of descendants; a non-zero exit with
    // an unchanged repo means it aborted. Treat non-zero as conflict abort.
    Ok(!status.success())
}

/// Outcome of a `git replay` requeue attempt.
pub enum Replayed {
    Applied,
    /// Replay could not apply cleanly (typically a conflict); message is stderr.
    Failed(String),
}

/// Requeue every branch contained in `ranges` onto `onto` in one operation via
/// `git replay --contained`, applying the emitted ref updates atomically with
/// `git update-ref --stdin`. No worktree is touched.
pub fn replay_requeue(onto: &str, ranges: &[String]) -> Result<Replayed> {
    let mut args: Vec<String> = vec![
        "replay".into(),
        "--onto".into(),
        onto.into(),
        "--contained".into(),
    ];
    args.extend(ranges.iter().cloned());
    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let out = Command::new("git")
        .args(&argrefs)
        .env(GUARD_ENV, "1")
        .output()
        .context("failed to spawn `git replay`")?;
    if !out.status.success() {
        return Ok(Replayed::Failed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Replayed::Applied); // nothing to update
    }

    let mut child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .env(GUARD_ENV, "1")
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn `git update-ref --stdin`")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(&out.stdout)
        .context("writing replay plan to update-ref")?;
    if !child.wait()?.success() {
        bail!("failed to apply replay ref updates");
    }
    Ok(Replayed::Applied)
}

/// Fallback requeue of a single branch that NEVER leaves an interactive
/// conflict state: on conflict it stages the marker-filled files, commits them,
/// and continues, so it always finishes. `--update-refs` moves any intermediate
/// branch refs in the rebased range. Detect persisted markers afterwards with
/// [`has_conflict_markers`].
pub fn rebase_persist(onto: &str, upstream: &str, branch: &str) -> Result<()> {
    let mut initial = Command::new("git");
    initial.args([
        "-c",
        "core.editor=true",
        "rebase",
        "--update-refs",
        "--onto",
        onto,
        upstream,
        branch,
    ]);
    quiet_git(&mut initial);
    let _ = initial.status().context("failed to spawn `git rebase`")?;
    drive_rebase_to_completion(branch)
}

/// Rewrite the whole line `base..top_branch` in place via an interactive
/// rebase whose todo we edit programmatically: the picks for `move_shas` are
/// relocated to directly follow `after` (or to the very front when `None`).
/// `--update-refs` carries every intermediate branch ref along, and conflicts
/// are persisted as markers exactly like [`rebase_persist`].
pub fn rebase_reorder_persist(
    base: &str,
    top_branch: &str,
    move_shas: &[String],
    after: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the git-queue executable")?;
    let mut initial = Command::new("git");
    initial.args([
        "-c",
        "core.editor=true",
        "rebase",
        "-i",
        "--update-refs",
        "--empty=keep",
        "--onto",
        base,
        base,
        top_branch,
    ]);
    quiet_git(&mut initial);
    // Our own binary rewrites the todo; the spec travels via the environment.
    initial
        .env(
            "GIT_SEQUENCE_EDITOR",
            format!("\"{}\" reorder-todo", exe.display()),
        )
        .env("GIT_QUEUE_MOVE_SHAS", move_shas.join(" "))
        .env("GIT_QUEUE_MOVE_AFTER", after.unwrap_or(""));
    let _ = initial.status().context("failed to spawn `git rebase`")?;
    drive_rebase_to_completion(top_branch)
}

/// Apply `shas` (front-first) on top of `branch` with conflict markers
/// persisted, leaving HEAD on `branch`. The cherry-pick analogue of
/// [`rebase_persist`].
pub fn cherry_pick_persist(branch: &str, shas: &[String]) -> Result<()> {
    run(&["checkout", "-q", branch])?;
    for sha in shas {
        let mut pick = Command::new("git");
        pick.args(["cherry-pick", "--allow-empty", sha]);
        quiet_git(&mut pick);
        let _ = pick.status().context("failed to spawn `git cherry-pick`")?;
        drive_cherry_pick_to_completion(branch)?;
    }
    Ok(())
}

fn cherry_pick_in_progress() -> bool {
    out(&["rev-parse", "--git-path", "CHERRY_PICK_HEAD"])
        .map(|p| std::path::Path::new(&p).exists())
        .unwrap_or(false)
}

fn drive_cherry_pick_to_completion(what: &str) -> Result<()> {
    let mut guard = 0;
    while cherry_pick_in_progress() {
        guard += 1;
        if guard > 5000 {
            let _ = Command::new("git")
                .args(["cherry-pick", "--abort"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            bail!("cherry-pick onto `{what}` did not converge; aborted");
        }
        let mut add = Command::new("git");
        add.args(["add", "-A"]);
        quiet_git(&mut add);
        let _ = add.status();
        let sub: &[&str] = if staged_changes() {
            &["cherry-pick", "--continue"]
        } else {
            &["cherry-pick", "--skip"]
        };
        let mut step = Command::new("git");
        step.args(sub);
        quiet_git(&mut step);
        let _ = step.status();
    }
    Ok(())
}

/// Rewrite `upstream..branch` in place, stamping a `Stable-Commit-Id` trailer onto
/// the commits in `shas`: an interactive rebase whose todo marks those picks
/// as `reword`, with our own binary as the message editor (it appends the
/// trailer and exits). Content is untouched, so no conflicts can arise.
pub fn rebase_stamp_ids(upstream: &str, branch: &str, shas: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the git-queue executable")?;
    let exe = exe.display();
    let mut initial = Command::new("git");
    initial.args(["rebase", "-i", "--update-refs", upstream, branch]);
    initial
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(GUARD_ENV, "1")
        .env("GIT_SEQUENCE_EDITOR", format!("\"{exe}\" stamp-todo"))
        .env("GIT_EDITOR", format!("\"{exe}\" add-queue-id"))
        .env("GIT_QUEUE_REWORD_SHAS", shas.join(" "))
        .env("GIT_QUEUE_STAMP_ALL", "1");
    let _ = initial.status().context("failed to spawn `git rebase`")?;
    drive_rebase_to_completion(branch)
}

/// Silence git's rebase chatter (conflict hints etc.) — it would contradict
/// the "it succeeded" outcome. Our loud banner is the user-facing signal.
fn quiet_git(c: &mut Command) {
    c.stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("GIT_EDITOR", "true")
        .env(GUARD_ENV, "1");
}

/// Keep stepping an in-progress rebase, staging conflict markers as the
/// "resolution" of each stop, until it finishes.
fn drive_rebase_to_completion(what: &str) -> Result<()> {
    let mut guard = 0;
    while rebase_in_progress() {
        guard += 1;
        if guard > 5000 {
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            bail!("requeue of `{what}` did not converge; aborted the rebase");
        }
        // Stage the conflict markers as the "resolution".
        let mut add = Command::new("git");
        add.args(["add", "-A"]);
        quiet_git(&mut add);
        let _ = add.status();

        let sub: &[&str] = if staged_changes() {
            &["rebase", "--continue"]
        } else {
            &["rebase", "--skip"]
        };
        let mut step = Command::new("git");
        step.args(sub);
        quiet_git(&mut step);
        let _ = step.status();
    }
    Ok(())
}

/// Concatenated commit messages (`%B`) of `range` — used to find Stable-Commit-Ids
/// embedded in squash-merge bodies.
pub fn log_messages(range: &str) -> Result<String> {
    out(&["log", "--format=%B", range])
}

/// Append a `Stable-Commit-Id` trailer to the commit-message file at `path`, unless
/// one is already present. `git interpret-trailers` handles placement.
pub fn add_trailer_to_file(path: &std::path::Path, id: &str) -> Result<()> {
    run(&[
        "interpret-trailers",
        "--if-exists",
        "doNothing",
        "--trailer",
        &format!("{}: {id}", crate::ident::TRAILER),
        "--in-place",
        &path.to_string_lossy(),
    ])
}

/// The `Stable-Commit-Id` of each commit in `range`, front-first: `(sha, id?)`.
pub fn queue_ids(range: &str) -> Result<Vec<(String, Option<String>)>> {
    let raw = out(&[
        "log",
        "--reverse",
        &format!(
            "--format=%H%x09%(trailers:key={},valueonly,separator=%x20)",
            crate::ident::TRAILER
        ),
        range,
    ])?;
    Ok(raw
        .lines()
        .map(|l| {
            let (sha, id) = l.split_once('\t').unwrap_or((l, ""));
            let id = id.split_whitespace().next().map(str::to_string);
            (sha.to_string(), id)
        })
        .collect())
}

/// Commits in `range`, NEWEST first, as `(Stable-Commit-Id?, subject)` pairs.
pub fn commits_with_ids(range: &str) -> Result<Vec<(Option<String>, String)>> {
    // The leading `|` anchors each record: `out()` trims the whole capture,
    // which would otherwise eat the leading tab of an id-less first commit.
    let raw = out(&[
        "log",
        &format!(
            "--format=|%(trailers:key={},valueonly,separator=%x20)%x09%s",
            crate::ident::TRAILER
        ),
        range,
    ])?;
    Ok(raw
        .lines()
        .filter_map(|l| {
            let (id, subject) = l.strip_prefix('|')?.split_once('\t')?;
            let id = id.split_whitespace().next().map(str::to_string);
            Some((id, subject.to_string()))
        })
        .collect())
}

/// The `Stable-Commit-Id` of `rev`'s commit message, if any.
pub fn queue_id_of(rev: &str) -> Option<String> {
    out(&[
        "log",
        "-1",
        &format!(
            "--format=%(trailers:key={},valueonly,separator=%x20)",
            crate::ident::TRAILER
        ),
        rev,
    ])
    .ok()
    .and_then(|s| s.split_whitespace().next().map(str::to_string))
}

/// Rewrite HEAD's message to add a `Stable-Commit-Id` trailer (content untouched).
pub fn amend_head_add_queue_id(id: &str) -> Result<()> {
    let msg = out(&["log", "-1", "--format=%B", "HEAD"])?;
    let tmp = std::env::temp_dir().join(format!("git-queue-msg-{}", std::process::id()));
    std::fs::write(&tmp, msg + "\n").context("writing temp commit message")?;
    add_trailer_to_file(&tmp, id)?;
    let res = run(&[
        "commit",
        "--amend",
        "--no-verify",
        "--allow-empty",
        "-q",
        "-F",
        &tmp.to_string_lossy(),
    ]);
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Patch-equivalence of `head` commits against `upstream`, via `git cherry`:
/// the SHAs (front-first) of commits in `head` whose patch is NOT already
/// present in `upstream`.
pub fn cherry_fresh(upstream: &str, head: &str) -> Result<Vec<String>> {
    let raw = out(&["cherry", upstream, head])?;
    Ok(raw
        .lines()
        .filter_map(|l| l.strip_prefix("+ ").map(str::to_string))
        .collect())
}

/// True if `sha` is a position `branch` has previously been at, per the
/// branch's reflog. Used to tell "the remote has new work" apart from "the
/// remote is just our own stale, pre-rewrite state".
pub fn was_previous_position(branch: &str, sha: &str) -> bool {
    out(&[
        "reflog",
        "show",
        "--format=%H",
        &format!("refs/heads/{branch}"),
    ])
    .map(|log| log.lines().any(|l| l == sha))
    .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    #[test]
    fn github_urls_parse_from_both_remote_forms() {
        // Pure parsing check via the same logic, exercised on literals.
        for (input, want) in [
            (
                "git@github.com:freshtonic/git-queue.git",
                "https://github.com/freshtonic/git-queue",
            ),
            (
                "https://github.com/freshtonic/git-queue",
                "https://github.com/freshtonic/git-queue",
            ),
            ("ssh://git@github.com/o/r.git", "https://github.com/o/r"),
        ] {
            let path = input
                .strip_prefix("git@github.com:")
                .or_else(|| input.strip_prefix("ssh://git@github.com/"))
                .or_else(|| input.strip_prefix("https://github.com/"))
                .unwrap();
            let path = path
                .strip_suffix(".git")
                .unwrap_or(path)
                .trim_end_matches('/');
            assert_eq!(format!("https://github.com/{path}"), want);
        }
    }
}

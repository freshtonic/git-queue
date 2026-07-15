//! End-to-end tests exercising the `git-stack` binary against throwaway repos.
//!
//! These cover the git-only commands (init, create, status, track, sync).
//! `submit` requires an authenticated `gh` and a GitHub remote, so it is not
//! exercised here.

use assert_cmd::Command;
use std::path::Path;
use std::process::Command as StdCommand;
use tempfile::TempDir;

/// Run a raw git command in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Capture stdout of a raw git command in `dir`.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Our binary, rooted in `dir`.
fn stack(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("git-stack").unwrap();
    c.current_dir(dir);
    c
}

/// Commit `file` with some content, on the current branch.
fn commit(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), file).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", &format!("add {file}")]);
}

/// A fresh repo on `main` with one initial commit.
fn new_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    commit(dir, "seed.txt");
    tmp
}

/// Stage `file` with `content` (does not commit).
fn stage(dir: &Path, file: &str, content: &str) {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    StdCommand::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

#[test]
fn commit_on_mid_branch_restacks_descendants() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");

    // New commit on mid-stack branch `a`.
    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "a2.txt", "more");
    stack(dir)
        .args(["commit", "-m", "more work on a"])
        .assert()
        .success();

    // `b` must have been restacked onto the new `a` tip and still be intact.
    assert!(is_ancestor(dir, "a", "b"), "b not restacked onto new a");
    assert_eq!(git_out(dir, &["show", "b:a2.txt"]), "more");
    assert_eq!(git_out(dir, &["show", "b:b.txt"]), "b.txt");
    // HEAD restored to `a`.
    assert_eq!(git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "a");
}

#[test]
fn commit_restacks_a_fork_in_one_go() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "b1"]).assert().success();
    commit(dir, "b1.txt");
    // Fork: second child on `a`.
    git(dir, &["checkout", "-q", "a"]);
    stack(dir).args(["create", "b2"]).assert().success();
    commit(dir, "b2.txt");

    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "shared.txt", "x");
    stack(dir)
        .args(["commit", "-m", "shared change"])
        .assert()
        .success();

    for leaf in ["b1", "b2"] {
        assert!(is_ancestor(dir, "a", leaf), "{leaf} not restacked");
        assert_eq!(git_out(dir, &["show", &format!("{leaf}:shared.txt")]), "x");
    }
}

#[test]
fn amend_folds_staged_changes_and_updates_descendants() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");

    git(dir, &["checkout", "-q", "a"]);
    let commits_before = git_out(dir, &["rev-list", "--count", "main..a"]);
    stage(dir, "folded.txt", "folded");
    stack(dir).arg("amend").assert().success();

    // Amend folds — it does NOT add a commit.
    assert_eq!(
        git_out(dir, &["rev-list", "--count", "main..a"]),
        commits_before
    );
    assert!(is_ancestor(dir, "a", "b"), "b not updated after amend");
    assert_eq!(git_out(dir, &["show", "b:folded.txt"]), "folded");
}

#[test]
fn conflicting_restack_persists_markers_and_flags_branch() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    stage(dir, "shared.txt", "base\n");
    git(dir, &["commit", "-q", "-m", "a: add shared"]);
    stack(dir).args(["create", "b"]).assert().success();
    stage(dir, "shared.txt", "b-version\n");
    git(dir, &["commit", "-q", "-m", "b: change shared"]);

    // New commit on `a` that touches the same file -> replaying `b` conflicts.
    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "shared.txt", "a-version\n");
    // Must still SUCCEED (markers are persisted, not left mid-rebase).
    stack(dir)
        .args(["commit", "-m", "a: conflicting change"])
        .assert()
        .success();

    // `b` now carries conflict markers and is flagged.
    assert!(
        git_out(dir, &["show", "b:shared.txt"]).contains("<<<<<<<"),
        "expected persisted conflict markers on b"
    );
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.b.stackConflicted"]),
        "true"
    );
    // No rebase left in progress.
    assert!(!dir.join(".git/rebase-merge").exists());
    assert!(!dir.join(".git/rebase-apply").exists());

    // status surfaces the warning marker.
    let out = stack(dir).arg("status").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("⚠"));
}

#[test]
fn hooks_autorestack_on_plain_commit() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");
    stack(dir).args(["hooks", "install"]).assert().success();

    // Plain `git commit` on `a`, with our binary on PATH for the hook to find.
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_git-stack")).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "hooked.txt", "hooked");
    let status = StdCommand::new("git")
        .args(["commit", "-q", "-m", "plain commit on a"])
        .current_dir(dir)
        .env("PATH", path)
        .status()
        .unwrap();
    assert!(status.success());

    // The post-commit hook should have auto-restacked `b`.
    assert!(is_ancestor(dir, "a", "b"), "hook did not restack b");
    assert_eq!(git_out(dir, &["show", "b:hooked.txt"]), "hooked");
}

/// A repo with a bare `origin` remote and `main` pushed. Returns both temp dirs
/// (keep the remote alive for the test's duration).
fn new_repo_with_remote() -> (TempDir, TempDir) {
    let tmp = new_repo();
    let dir = tmp.path();
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "--bare", "-q", "-b", "main"]);
    git(
        dir,
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(dir, &["push", "-q", "origin", "main"]);
    (tmp, remote)
}

/// First field of `git ls-remote origin <ref>` (the remote's SHA), or "".
fn ls_remote(dir: &Path, refname: &str) -> String {
    git_out(dir, &["ls-remote", "origin", refname])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

#[test]
fn sync_pulls_teammate_commits_and_pushes_with_lease() {
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");
    git(dir, &["push", "-q", "-u", "origin", "a", "b"]);

    // Simulate a teammate pushing a commit to `a`, then rewind our local `a`
    // so the remote is strictly ahead of us.
    git(dir, &["checkout", "-q", "a"]);
    commit(dir, "teammate.txt");
    git(dir, &["push", "-q", "origin", "a"]);
    git(dir, &["reset", "-q", "--hard", "HEAD~1"]);

    git(dir, &["checkout", "-q", "b"]);
    stack(dir).arg("sync").assert().success();

    // Teammate's commit was pulled into `a`, and `b` restacked on top of it.
    assert_eq!(git_out(dir, &["show", "a:teammate.txt"]), "teammate.txt");
    assert!(is_ancestor(dir, "a", "b"), "b not restacked onto updated a");
    assert_eq!(git_out(dir, &["show", "b:teammate.txt"]), "teammate.txt");
    // Our restacked `b` was pushed back to the remote.
    assert_eq!(ls_remote(dir, "b"), git_out(dir, &["rev-parse", "b"]));
}

#[test]
fn sync_no_push_leaves_remote_untouched() {
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    git(dir, &["push", "-q", "-u", "origin", "a"]);
    let remote_a_before = ls_remote(dir, "a");

    // Add a local commit that would normally be pushed.
    commit(dir, "local.txt");
    stack(dir).args(["sync", "--no-push"]).assert().success();

    // Remote `a` is unchanged; local is ahead.
    assert_eq!(ls_remote(dir, "a"), remote_a_before);
    assert_ne!(ls_remote(dir, "a"), git_out(dir, &["rev-parse", "a"]));
}

#[test]
fn init_detects_and_records_trunk() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    assert_eq!(git_out(dir, &["config", "--local", "stack.trunk"]), "main");
}

#[test]
fn create_builds_a_tracked_chain() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();

    stack(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    // We should now be on feat-b.
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-b"
    );
    // Parent pointers recorded.
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.feat-a.stackParent"]),
        "main"
    );
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.feat-b.stackParent"]),
        "feat-a"
    );

    // Status lists both branches and marks the current one.
    let out = stack(dir).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("feat-a"), "status missing feat-a:\n{text}");
    assert!(text.contains("feat-b"), "status missing feat-b:\n{text}");
    assert!(
        text.contains("← current"),
        "status missing current marker:\n{text}"
    );
}

#[test]
fn prev_and_next_navigate_the_stack() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    stack(dir).arg("prev").assert().success();
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-a"
    );
    stack(dir).arg("next").assert().success();
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-b"
    );
}

#[test]
fn sync_restacks_onto_advanced_trunk() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();

    stack(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    stack(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    // Advance trunk with a new commit that the stack doesn't have yet.
    git(dir, &["checkout", "-q", "main"]);
    commit(dir, "trunk.txt");

    // Restack (no remote -> warns and uses local trunk).
    stack(dir).args(["sync", "--no-push"]).assert().success();

    // feat-b must now contain the new trunk commit AND both stack commits,
    // and trunk must be an ancestor of feat-b.
    git(dir, &["checkout", "-q", "feat-b"]);
    for f in ["trunk.txt", "a.txt", "b.txt"] {
        assert!(dir.join(f).exists(), "feat-b missing {f} after sync");
    }
    let main_tip = git_out(dir, &["rev-parse", "main"]);
    assert!(
        StdCommand::new("git")
            .args(["merge-base", "--is-ancestor", &main_tip, "feat-b"])
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "main is not an ancestor of feat-b after sync"
    );
}

#[test]
fn track_adopts_an_existing_branch() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();

    // Hand-made branch off main.
    git(dir, &["checkout", "-q", "-b", "hotfix"]);
    commit(dir, "hotfix.txt");

    stack(dir).arg("track").assert().success();
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.hotfix.stackParent"]),
        "main"
    );
}

#[test]
fn untrack_removes_metadata() {
    let tmp = new_repo();
    let dir = tmp.path();
    stack(dir).arg("init").assert().success();
    stack(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");

    stack(dir).arg("untrack").assert().success();
    // Config key should be gone (git config --get exits non-zero).
    let missing = StdCommand::new("git")
        .args(["config", "--local", "--get", "branch.feat-a.stackParent"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!missing.status.success(), "stackParent should be unset");
}

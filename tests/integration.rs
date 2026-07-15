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
    stack(dir).arg("sync").assert().success();

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

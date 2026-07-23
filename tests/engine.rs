//! Integration tests for the headless `git queue tui` engine, driven
//! **in-process** against throwaway tempdir repos.
//!
//! Unlike `tests/integration.rs`, which spawns the `git-queue` binary, these
//! call `git_queue::engine::Engine` as a library — the interactive view can't
//! be driven through `assert_cmd`, so the engine seam is where the behaviour is
//! asserted. The engine's `git`/`meta` helpers operate on the *process* current
//! directory, so each test builds its repo with explicit `--current-dir` binary
//! calls, then takes `CWD_LOCK` and `set_current_dir`s into the repo for the
//! single in-process `Engine::load()` call (process cwd is global, so the lock
//! serialises those loads across the parallel test runner).

use assert_cmd::Command;
use git_queue::engine::{Boundary, Engine};
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serialises the process-cwd swaps that in-process `Engine::load()` needs.
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Run a raw git command in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Our binary, rooted in `dir`.
fn queue(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("git-queue").unwrap();
    c.current_dir(dir);
    c
}

/// A fresh repo on `main` with one initial commit.
fn new_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    // Keep test repos hermetic: the host's global config may sign commits
    // with a key that isn't loaded.
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["config", "tag.gpgsign", "false"]);
    commit(dir, "seed.txt");
    tmp
}

/// Commit `file` (content = its name) with a raw `git commit`, on the current
/// branch. Without the commit-msg hook this leaves no Stable-Commit-Id.
fn commit(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), file).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", &format!("add {file}")]);
}

/// Stage `file` and record it with `git queue commit -m msg`, which stamps a
/// Stable-Commit-Id on a tracked branch.
fn queue_commit(dir: &Path, file: &str, msg: &str) {
    std::fs::write(dir.join(file), file).unwrap();
    git(dir, &["add", file]);
    queue(dir).args(["commit", "-m", msg]).assert().success();
}

/// Load the engine with the process cwd pointed at `dir`, serialised so
/// parallel tests don't race the global cwd.
fn load_in(dir: &Path) -> anyhow::Result<Engine> {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_current_dir(dir).unwrap();
    Engine::load()
}

#[test]
fn loads_multibranch_line_with_sequence_boundaries_and_ids() {
    let tmp = new_repo();
    let dir = tmp.path();

    // Branch `a` owns two commits (one unstamped, one stamped via
    // `queue commit`); branch `b` owns one unstamped commit on top.
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "c1.txt");
    queue_commit(dir, "c2.txt", "second change on a");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "c3.txt");

    let engine = load_in(dir).expect("load multi-branch line");
    let line = engine.line();

    assert_eq!(line.base, "main");
    assert_eq!(engine.launched_from(), "b");

    // Front → tip commit sequence.
    let subjects: Vec<&str> = line.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(
        subjects,
        vec!["add c1.txt", "second change on a", "add c3.txt"]
    );

    // Boundary positions: `a` ends after its two commits, `b` after the third.
    assert_eq!(
        line.boundaries,
        vec![
            Boundary {
                name: "a".into(),
                end: 2,
            },
            Boundary {
                name: "b".into(),
                end: 3,
            },
        ]
    );
    assert_eq!(line.commits_of(0).len(), 2);
    assert_eq!(line.commits_of(1).len(), 1);

    // Stable-Commit-Ids read only where present.
    assert!(line.commits[0].id.is_none(), "raw commit has no id");
    assert!(
        line.commits[1]
            .id
            .as_deref()
            .is_some_and(|s| s.starts_with("q-")),
        "queue commit is stamped"
    );
    assert!(line.commits[2].id.is_none(), "raw commit has no id");
}

#[test]
fn untracked_branch_loads_as_single_provisional_section() {
    let tmp = new_repo();
    let dir = tmp.path();

    // A plain, untracked branch — no `queue create`, no metadata.
    git(dir, &["checkout", "-q", "-b", "feature"]);
    commit(dir, "u1.txt");
    commit(dir, "u2.txt");

    let engine = load_in(dir).expect("load untracked branch");
    let line = engine.line();

    assert_eq!(line.base, "main");
    assert_eq!(line.commits.len(), 2);
    assert_eq!(
        line.boundaries,
        vec![Boundary {
            name: "feature".into(),
            end: 2,
        }],
        "an untracked branch is one provisional section over trunk"
    );
}

#[test]
fn refuses_a_dirty_worktree() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "c1.txt");

    // Uncommitted modification to a tracked file.
    std::fs::write(dir.join("c1.txt"), "changed").unwrap();

    let err = load_in(dir).unwrap_err();
    assert!(
        format!("{err:#}").contains("uncommitted changes"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn refuses_an_empty_queue() {
    let tmp = new_repo();
    let dir = tmp.path();
    // `a` is created at main's tip with no commits of its own.
    queue(dir).args(["create", "a"]).assert().success();

    let err = load_in(dir).unwrap_err();
    assert!(
        format!("{err:#}").contains("no commits"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn refuses_a_forked_line() {
    let tmp = new_repo();
    let dir = tmp.path();

    // `a` forks into `b1` and `b2`.
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a1.txt");
    queue(dir).args(["create", "b1"]).assert().success();
    commit(dir, "b1.txt");
    git(dir, &["checkout", "-q", "a"]);
    queue(dir).args(["create", "b2"]).assert().success();
    commit(dir, "b2.txt");

    let err = load_in(dir).unwrap_err();
    assert!(
        format!("{err:#}").contains("forked queue line"),
        "unexpected error: {err:#}"
    );
}

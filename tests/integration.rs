//! End-to-end tests exercising the `git-queue` binary against throwaway repos.
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
fn queue(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("git-queue").unwrap();
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

/// The installed git's (major, minor) version.
fn git_version() -> (u32, u32) {
    let out = StdCommand::new("git")
        .arg("--version")
        .output()
        .expect("git --version");
    let s = String::from_utf8_lossy(&out.stdout);
    let ver = s
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("0.0");
    let mut parts = ver.split('.').filter_map(|p| p.parse().ok());
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// Returns true (and prints a skip note) when the installed git is older than
/// `min`, so a test can early-return. `commit`/`sync` need `git replay` (2.44);
/// `amend`/`reword` need `git history` (2.55). CI may run an older git.
fn skip_below(min: (u32, u32), feature: &str) -> bool {
    let v = git_version();
    let ok = v.0 > min.0 || (v.0 == min.0 && v.1 >= min.1);
    if !ok {
        eprintln!(
            "SKIP: {feature} needs git >= {}.{} (have {}.{})",
            min.0, min.1, v.0, v.1
        );
    }
    !ok
}

const REPLAY: (u32, u32) = (2, 44); // `git replay`
const HISTORY: (u32, u32) = (2, 55); // `git history`

#[test]
fn commit_on_mid_branch_requeues_descendants() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");

    // New commit on mid-queue branch `a`.
    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "a2.txt", "more");
    queue(dir)
        .args(["commit", "-m", "more work on a"])
        .assert()
        .success();

    // `b` must have been requeued onto the new `a` tip and still be intact.
    assert!(is_ancestor(dir, "a", "b"), "b not requeued onto new a");
    assert_eq!(git_out(dir, &["show", "b:a2.txt"]), "more");
    assert_eq!(git_out(dir, &["show", "b:b.txt"]), "b.txt");
    // HEAD restored to `a`.
    assert_eq!(git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "a");
}

#[test]
fn commit_requeues_a_fork_in_one_go() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "b1"]).assert().success();
    commit(dir, "b1.txt");
    // Fork: second child on `a`.
    git(dir, &["checkout", "-q", "a"]);
    queue(dir).args(["create", "b2"]).assert().success();
    commit(dir, "b2.txt");

    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "shared.txt", "x");
    queue(dir)
        .args(["commit", "-m", "shared change"])
        .assert()
        .success();

    for leaf in ["b1", "b2"] {
        assert!(is_ancestor(dir, "a", leaf), "{leaf} not requeued");
        assert_eq!(git_out(dir, &["show", &format!("{leaf}:shared.txt")]), "x");
    }
}

#[test]
fn amend_folds_staged_changes_and_updates_descendants() {
    if skip_below(HISTORY, "git history") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");

    git(dir, &["checkout", "-q", "a"]);
    let commits_before = git_out(dir, &["rev-list", "--count", "main..a"]);
    stage(dir, "folded.txt", "folded");
    queue(dir).arg("amend").assert().success();

    // Amend folds — it does NOT add a commit.
    assert_eq!(
        git_out(dir, &["rev-list", "--count", "main..a"]),
        commits_before
    );
    assert!(is_ancestor(dir, "a", "b"), "b not updated after amend");
    assert_eq!(git_out(dir, &["show", "b:folded.txt"]), "folded");
}

#[test]
fn conflicting_requeue_persists_markers_and_flags_branch() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    stage(dir, "shared.txt", "base\n");
    git(dir, &["commit", "-q", "-m", "a: add shared"]);
    queue(dir).args(["create", "b"]).assert().success();
    stage(dir, "shared.txt", "b-version\n");
    git(dir, &["commit", "-q", "-m", "b: change shared"]);

    // New commit on `a` that touches the same file -> replaying `b` conflicts.
    git(dir, &["checkout", "-q", "a"]);
    stage(dir, "shared.txt", "a-version\n");
    // Must still SUCCEED (markers are persisted, not left mid-rebase).
    queue(dir)
        .args(["commit", "-m", "a: conflicting change"])
        .assert()
        .success();

    // `b` now carries conflict markers.
    assert!(
        git_out(dir, &["show", "b:shared.txt"]).contains("<<<<<<<"),
        "expected persisted conflict markers on b"
    );
    // No rebase left in progress.
    assert!(!dir.join(".git/rebase-merge").exists());
    assert!(!dir.join(".git/rebase-apply").exists());

    // status surfaces the warning marker.
    let out = queue(dir).arg("status").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("⚠"));
}

#[test]
fn amend_on_conflict_errors_and_preserves_staged_work() {
    if skip_below(HISTORY, "git history") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "db"]).assert().success();
    stage(dir, "shared.txt", "db-original\n");
    git(dir, &["commit", "-q", "-m", "db: add shared"]);
    queue(dir).args(["create", "ui"]).assert().success();
    stage(dir, "shared.txt", "ui-edit\n");
    git(dir, &["commit", "-q", "-m", "ui: edit shared"]);

    // Stage a revision to db's commit that will conflict when propagated to ui.
    git(dir, &["checkout", "-q", "db"]);
    stage(dir, "shared.txt", "db-revised\n");

    // amend must FAIL loudly — never claim success while folding nothing.
    queue(dir).arg("amend").assert().failure();

    // db's commit is unchanged and the staged work is preserved.
    assert_eq!(git_out(dir, &["show", "db:shared.txt"]), "db-original");
    assert!(
        !git_out(dir, &["diff", "--cached", "--name-only"]).is_empty(),
        "staged changes should be preserved after a failed amend"
    );
    // ui is not stranded.
    assert!(
        is_ancestor(dir, "db", "ui"),
        "ui must still descend from db"
    );
}

#[test]
fn resolving_markers_clears_the_status_warning() {
    if skip_below(REPLAY, "git replay") || skip_below(HISTORY, "git history") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "db"]).assert().success();
    stage(dir, "f.txt", "base\n");
    git(dir, &["commit", "-q", "-m", "db base"]);
    queue(dir).args(["create", "ui"]).assert().success();
    stage(dir, "f.txt", "ui-version\n");
    git(dir, &["commit", "-q", "-m", "ui change"]);

    // Conflicting commit on db -> ui gets persisted markers.
    git(dir, &["checkout", "-q", "db"]);
    stage(dir, "f.txt", "db-version\n");
    queue(dir)
        .args(["commit", "-m", "db conflicting"])
        .assert()
        .success();
    let flagged = queue(dir).arg("status").output().unwrap();
    assert!(
        String::from_utf8_lossy(&flagged.stdout).contains("⚠"),
        "status should warn while markers exist"
    );

    // Resolve on ui and amend; status must stop warning (no stale flag).
    git(dir, &["checkout", "-q", "ui"]);
    stage(dir, "f.txt", "ui-version\n");
    queue(dir).arg("amend").assert().success();

    assert!(
        !git_out(dir, &["show", "ui:f.txt"]).contains("<<<<<<<"),
        "ui should be clean after resolution"
    );
    let cleared = queue(dir).arg("status").output().unwrap();
    assert!(
        !String::from_utf8_lossy(&cleared.stdout).contains("⚠"),
        "status must not warn after markers are resolved"
    );
}

#[test]
fn hooks_autorequeue_on_plain_commit() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");
    queue(dir).args(["hooks", "install"]).assert().success();

    // Plain `git commit` on `a`, with our binary on PATH for the hook to find.
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_git-queue")).parent().unwrap();
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

    // The post-commit hook should have auto-requeued `b`.
    assert!(is_ancestor(dir, "a", "b"), "hook did not requeue b");
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
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "b.txt");
    git(dir, &["push", "-q", "-u", "origin", "a", "b"]);

    // A teammate pushes a commit to `a` from their own clone: the commit is
    // genuinely new to us (never in our reflog), and the remote is strictly
    // ahead of our local `a`.
    let mate = TempDir::new().unwrap();
    let mate_dir = mate.path();
    git(
        mate_dir,
        &["clone", "-q", _remote.path().to_str().unwrap(), "."],
    );
    git(mate_dir, &["config", "user.email", "mate@example.com"]);
    git(mate_dir, &["config", "user.name", "Mate"]);
    git(mate_dir, &["checkout", "-q", "a"]);
    commit(mate_dir, "teammate.txt");
    git(mate_dir, &["push", "-q", "origin", "a"]);

    git(dir, &["checkout", "-q", "b"]);
    queue(dir).arg("sync").assert().success();

    // Teammate's commit was pulled into `a`, and `b` requeued on top of it.
    assert_eq!(git_out(dir, &["show", "a:teammate.txt"]), "teammate.txt");
    assert!(is_ancestor(dir, "a", "b"), "b not requeued onto updated a");
    assert_eq!(git_out(dir, &["show", "b:teammate.txt"]), "teammate.txt");
    // Our requeued `b` was pushed back to the remote.
    assert_eq!(ls_remote(dir, "b"), git_out(dir, &["rev-parse", "b"]));
}

#[test]
fn sync_no_push_leaves_remote_untouched() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    git(dir, &["push", "-q", "-u", "origin", "a"]);
    let remote_a_before = ls_remote(dir, "a");

    // Add a local commit that would normally be pushed.
    commit(dir, "local.txt");
    queue(dir).args(["sync", "--no-push"]).assert().success();

    // Remote `a` is unchanged; local is ahead.
    assert_eq!(ls_remote(dir, "a"), remote_a_before);
    assert_ne!(ls_remote(dir, "a"), git_out(dir, &["rev-parse", "a"]));
}

/// True if `rev:path` exists in the repo.
fn exists_at(dir: &Path, rev: &str, path: &str) -> bool {
    StdCommand::new("git")
        .args(["cat-file", "-e", &format!("{rev}:{path}")])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
}

#[test]
fn split_divides_a_branch_into_a_queue() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "feature"]).assert().success();
    commit(dir, "c1.txt");
    commit(dir, "c2.txt");
    commit(dir, "c3.txt");

    // Editor assigns commit 1 -> api, 2 -> service, 3 -> ui.
    // perl -i is portable across macOS/Linux; BSD `sed -i ''` differs from GNU.
    let editor = "perl -i -pe 's/^feature /api / if $. == 1; s/^feature /service / if $. == 2; s/^feature /ui / if $. == 3'";
    queue(dir)
        .env("GIT_EDITOR", editor)
        .arg("split")
        .assert()
        .success();

    // Three tracked branches with the right parent chain.
    assert_eq!(git_out(dir, &["config", "branch.api.queueParent"]), "main");
    assert_eq!(
        git_out(dir, &["config", "branch.service.queueParent"]),
        "api"
    );
    assert_eq!(
        git_out(dir, &["config", "branch.ui.queueParent"]),
        "service"
    );

    // Each branch contains exactly its own slice of history.
    assert!(exists_at(dir, "api", "c1.txt"));
    assert!(
        !exists_at(dir, "api", "c2.txt"),
        "api should not include c2"
    );
    assert!(exists_at(dir, "service", "c2.txt"));
    assert!(
        !exists_at(dir, "service", "c3.txt"),
        "service should not include c3"
    );
    for f in ["c1.txt", "c2.txt", "c3.txt"] {
        assert!(exists_at(dir, "ui", f), "ui should include {f}");
    }

    // Ends up checked out on the top of the new queue.
    assert_eq!(git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "ui");
}

#[test]
fn describe_stores_and_clears_description() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");

    queue(dir)
        .args(["describe", "-m", "Adds the API layer"])
        .assert()
        .success();
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.a.queueDescription"]),
        "Adds the API layer"
    );

    // Empty description clears it.
    queue(dir).args(["describe", "-m", ""]).assert().success();
    let cleared = StdCommand::new("git")
        .args(["config", "--local", "--get", "branch.a.queueDescription"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!cleared.status.success(), "description should be unset");
}

#[test]
fn init_detects_and_records_trunk() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    assert_eq!(git_out(dir, &["config", "--local", "queue.trunk"]), "main");
}

#[test]
fn create_builds_a_tracked_chain() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();

    queue(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    // We should now be on feat-b.
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-b"
    );
    // Parent pointers recorded.
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.feat-a.queueParent"]),
        "main"
    );
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.feat-b.queueParent"]),
        "feat-a"
    );

    // Status lists both branches and marks the current one.
    let out = queue(dir).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("feat-a"), "status missing feat-a:\n{text}");
    assert!(text.contains("feat-b"), "status missing feat-b:\n{text}");
    assert!(
        text.contains("← current"),
        "status missing current marker:\n{text}"
    );
}

#[test]
fn prev_and_next_navigate_the_queue() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    queue(dir).arg("prev").assert().success();
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-a"
    );
    queue(dir).arg("next").assert().success();
    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-b"
    );
}

#[test]
fn sync_requeues_onto_advanced_trunk() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();

    queue(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "feat-b"]).assert().success();
    commit(dir, "b.txt");

    // Advance trunk with a new commit that the queue doesn't have yet.
    git(dir, &["checkout", "-q", "main"]);
    commit(dir, "trunk.txt");

    // Requeue (no remote -> warns and uses local trunk).
    queue(dir).args(["sync", "--no-push"]).assert().success();

    // feat-b must now contain the new trunk commit AND both queue commits,
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
    queue(dir).arg("init").assert().success();

    // Hand-made branch off main.
    git(dir, &["checkout", "-q", "-b", "hotfix"]);
    commit(dir, "hotfix.txt");

    queue(dir).arg("track").assert().success();
    assert_eq!(
        git_out(dir, &["config", "--local", "branch.hotfix.queueParent"]),
        "main"
    );
}

#[test]
fn untrack_removes_metadata() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "feat-a"]).assert().success();
    commit(dir, "a.txt");

    queue(dir).arg("untrack").assert().success();
    // Config key should be gone (git config --get exits non-zero).
    let missing = StdCommand::new("git")
        .args(["config", "--local", "--get", "branch.feat-a.queueParent"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!missing.status.success(), "queueParent should be unset");
}

#[test]
fn create_builds_a_queue_on_a_release_branch() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();

    // A release branch off main with its own commit becomes the base.
    git(dir, &["checkout", "-q", "-b", "release-1.2"]);
    commit(dir, "rel.txt");
    queue(dir).args(["create", "fix-a"]).assert().success();
    commit(dir, "a.txt");
    queue(dir).args(["create", "fix-b"]).assert().success();
    commit(dir, "b.txt");

    assert_eq!(
        git_out(dir, &["config", "branch.fix-a.queueParent"]),
        "release-1.2"
    );
    assert_eq!(
        git_out(dir, &["config", "branch.fix-b.queueParent"]),
        "fix-a"
    );

    // status shows the queue rooted at its base, not at trunk.
    let out = queue(dir).arg("status").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(stdout.contains("release-1.2 (base)"), "{stdout}");
    assert!(
        stdout.contains("fix-a") && stdout.contains("fix-b"),
        "{stdout}"
    );

    // status also works from the base branch itself.
    git(dir, &["checkout", "-q", "release-1.2"]);
    let out = queue(dir).arg("status").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(stdout.contains("fix-b"), "{stdout}");
}

#[test]
fn sync_requeues_queue_onto_advanced_base_branch() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();

    git(dir, &["checkout", "-q", "-b", "release-1.2"]);
    commit(dir, "rel.txt");
    queue(dir).args(["create", "fix-a"]).assert().success();
    commit(dir, "a.txt");

    // The base advances with a hotpatch the queue doesn't have yet.
    git(dir, &["checkout", "-q", "release-1.2"]);
    commit(dir, "hotpatch.txt");
    git(dir, &["checkout", "-q", "fix-a"]);

    queue(dir).args(["sync", "--no-push"]).assert().success();

    git(dir, &["checkout", "-q", "fix-a"]);
    for f in ["rel.txt", "hotpatch.txt", "a.txt"] {
        assert!(dir.join(f).exists(), "fix-a missing {f} after sync");
    }
    assert!(is_ancestor(dir, "release-1.2", "fix-a"));
    // Trunk stays out of the queue: main must not be an ancestor of the new
    // base commits' line beyond the fork point (rel.txt is not on main).
    git(dir, &["checkout", "-q", "main"]);
    assert!(
        !dir.join("rel.txt").exists(),
        "release commit leaked onto main"
    );
}

#[test]
fn create_with_explicit_base_flag() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();

    git(dir, &["checkout", "-q", "-b", "release-1.2"]);
    commit(dir, "rel.txt");
    git(dir, &["checkout", "-q", "main"]);

    // Start a queue on the release branch without checking it out first.
    queue(dir)
        .args(["create", "fix-a", "--base", "release-1.2"])
        .assert()
        .success();

    assert_eq!(
        git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "fix-a"
    );
    assert_eq!(
        git_out(dir, &["config", "branch.fix-a.queueParent"]),
        "release-1.2"
    );
    // The new branch starts at the base's tip, not at main's.
    assert_eq!(
        git_out(dir, &["rev-parse", "fix-a"]),
        git_out(dir, &["rev-parse", "release-1.2"])
    );

    // A bogus base is rejected.
    queue(dir)
        .args(["create", "fix-b", "--base", "nope"])
        .assert()
        .failure();
}

#[test]
fn sync_prunes_tracking_refs_of_branches_deleted_on_the_remote() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a.txt");
    git(dir, &["push", "-q", "-u", "origin", "a"]);

    // The branch disappears from the remote (e.g. auto-deleted when its PR
    // merged), leaving a stale origin/a tracking ref behind.
    git(dir, &["push", "-q", "origin", "--delete", "a"]);
    let tip_before = git_out(dir, &["rev-parse", "a"]);

    queue(dir).args(["sync", "--no-push"]).assert().success();

    // The stale tracking ref is pruned, and the ghost branch must not have
    // been "pulled" into the local branch.
    let stale = StdCommand::new("git")
        .args(["rev-parse", "--verify", "origin/a"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!stale.status.success(), "origin/a should have been pruned");
    assert_eq!(
        git_out(dir, &["rev-parse", "a"]),
        tip_before,
        "local branch mangled"
    );
}

fn sha(dir: &Path, rev: &str) -> String {
    git_out(dir, &["rev-parse", rev])
}

fn subjects(dir: &Path, range: &str) -> Vec<String> {
    git_out(dir, &["log", "--reverse", "--format=%s", range])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn move_reorders_commits_within_a_branch() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "one.txt");
    let c1 = sha(dir, "HEAD");
    commit(dir, "two.txt");
    let c2 = sha(dir, "HEAD");

    queue(dir)
        .args(["move", &c1, "--new-parent", &c2])
        .assert()
        .success();

    assert_eq!(subjects(dir, "main..a"), vec!["add two.txt", "add one.txt"]);
    for f in ["one.txt", "two.txt"] {
        assert!(dir.join(f).exists(), "{f} missing after move");
    }
}

#[test]
fn move_commit_to_a_different_branch() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "a1.txt");
    let a1 = sha(dir, "HEAD");
    queue(dir).args(["create", "b"]).assert().success();
    commit(dir, "b1.txt");
    commit(dir, "b2.txt");
    let b2 = sha(dir, "HEAD");

    // Move b's tip commit into branch `a` (directly after a's tip commit).
    queue(dir)
        .args(["move", &b2, "--new-parent", &a1])
        .assert()
        .success();

    assert_eq!(subjects(dir, "main..a"), vec!["add a1.txt", "add b2.txt"]);
    assert_eq!(subjects(dir, "a..b"), vec!["add b1.txt"]);
    assert!(is_ancestor(dir, "a", "b"), "b must still build on a");
    git(dir, &["checkout", "-q", "a"]);
    assert!(dir.join("b2.txt").exists() && !dir.join("b1.txt").exists());
}

#[test]
fn move_an_inclusive_range_of_commits() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "f1.txt");
    let f1 = sha(dir, "HEAD");
    commit(dir, "f2.txt");
    let f2 = sha(dir, "HEAD");
    commit(dir, "f3.txt");
    let f3 = sha(dir, "HEAD");

    // Move [f1..f2] (inclusive) after f3.
    queue(dir)
        .args(["move", &format!("{f1}..{f2}"), "--new-parent", &f3])
        .assert()
        .success();

    assert_eq!(
        subjects(dir, "main..a"),
        vec!["add f3.txt", "add f1.txt", "add f2.txt"]
    );
}

#[test]
fn move_persists_conflict_markers() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    std::fs::write(dir.join("c.txt"), "one\n").unwrap();
    git(dir, &["add", "c.txt"]);
    git(dir, &["commit", "-q", "-m", "write one"]);
    std::fs::write(dir.join("c.txt"), "two\n").unwrap();
    git(dir, &["add", "c.txt"]);
    git(dir, &["commit", "-q", "-m", "write two"]);
    let second = sha(dir, "HEAD");
    let base_tip = sha(dir, "main");

    // Moving "write two" to the front makes it apply before "write one":
    // a conflict, persisted as markers.
    queue(dir)
        .args(["move", &second, "--new-parent", &base_tip])
        .assert()
        .success();

    let content = std::fs::read_to_string(dir.join("c.txt")).unwrap();
    assert!(
        content.contains("<<<<<<<"),
        "markers not persisted: {content}"
    );
}

#[test]
fn move_rejects_commits_outside_the_queue() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    let seed = sha(dir, "main");
    queue(dir).args(["create", "a"]).assert().success();
    commit(dir, "x.txt");
    let x = sha(dir, "HEAD");
    commit(dir, "y.txt");
    let y = sha(dir, "HEAD");

    // A trunk commit is not part of the queue.
    queue(dir)
        .args(["move", &seed, "--new-parent", &x])
        .assert()
        .failure();
    // --new-parent inside the moved range is rejected.
    queue(dir)
        .args(["move", &format!("{x}..{y}"), "--new-parent", &y])
        .assert()
        .failure();
}

#[test]
fn sync_does_not_pull_back_our_own_stale_remote_state() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-q", "-m", "add f"]);
    git(dir, &["push", "-q", "-u", "origin", "a"]);

    // Rewrite the commit locally (as amend/move/an unpushed requeue would);
    // the remote now holds only our stale pre-rewrite state.
    std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-q", "--amend", "-m", "add f (amended)"]);

    // Two no-push syncs in a row: neither may re-apply the old remote commit
    // on top of the rewrite.
    for _ in 0..2 {
        queue(dir).args(["sync", "--no-push"]).assert().success();
    }

    assert_eq!(subjects(dir, "main..a"), vec!["add f (amended)"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "v2\n",
        "rewrite lost or conflicted"
    );
}

fn queue_id_of(dir: &Path, rev: &str) -> String {
    git_out(
        dir,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Queued-Commit-Id,valueonly)",
            rev,
        ],
    )
    .trim()
    .to_string()
}

#[test]
fn commit_msg_hook_stamps_queue_ids_on_queue_branches_only() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    queue(dir).args(["hooks", "install"]).assert().success();

    let bin_dir = Path::new(env!("CARGO_BIN_EXE_git-queue")).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    let plain_commit = |dir: &Path, file: &str, msg: &str| {
        stage(dir, file, file);
        assert!(StdCommand::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(dir)
            .env("PATH", &path)
            .status()
            .unwrap()
            .success());
    };

    // On a queue branch: the hook stamps a Queued-Commit-Id.
    plain_commit(dir, "on-queue.txt", "queue work");
    let id = queue_id_of(dir, "HEAD");
    assert!(id.starts_with("q-") && id.len() == 28, "bad id: {id:?}");

    // On trunk (untracked): no trailer.
    git(dir, &["checkout", "-q", "main"]);
    plain_commit(dir, "on-main.txt", "trunk work");
    assert_eq!(queue_id_of(dir, "HEAD"), "");
}

#[test]
fn queue_commit_stamps_an_id_without_hooks() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    stage(dir, "a.txt", "a");
    queue(dir)
        .args(["commit", "-m", "add a"])
        .assert()
        .success();

    let id = queue_id_of(dir, "HEAD");
    assert!(id.starts_with("q-"), "no Queued-Commit-Id injected: {id:?}");
    // The id survives a requeue (message is carried through the rewrite).
    git(dir, &["checkout", "-q", "main"]);
    commit(dir, "trunk.txt");
    git(dir, &["checkout", "-q", "a"]);
    queue(dir).args(["sync", "--no-push"]).assert().success();
    assert_eq!(queue_id_of(dir, "a"), id, "id lost in requeue");
}

#[test]
fn sync_pulls_teammate_commits_but_not_stale_copies_of_ours() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let (tmp, _remote) = new_repo_with_remote();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    stage(dir, "f.txt", "v1\n");
    queue(dir)
        .args(["commit", "-m", "add f"])
        .assert()
        .success();
    git(dir, &["push", "-q", "-u", "origin", "a"]);

    // A teammate adds a commit on top of the remote's copy...
    let mate = TempDir::new().unwrap();
    let mate_dir = mate.path();
    git(
        mate_dir,
        &["clone", "-q", _remote.path().to_str().unwrap(), "."],
    );
    git(mate_dir, &["config", "user.email", "mate@example.com"]);
    git(mate_dir, &["config", "user.name", "Mate"]);
    git(mate_dir, &["checkout", "-q", "a"]);
    commit(mate_dir, "teammate.txt");
    git(mate_dir, &["push", "-q", "origin", "a"]);

    // ...while we rewrite our commit locally (same Queued-Commit-Id, new content).
    std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-q", "--amend", "--no-edit"]);

    queue(dir).args(["sync", "--no-push"]).assert().success();

    // The teammate's commit came in exactly once; our stale copy did not.
    assert_eq!(
        subjects(dir, "main..a"),
        vec!["add f", "add teammate.txt"],
        "wrong commits after mixed-divergence sync"
    );
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "v2\n");
    assert!(dir.join("teammate.txt").exists());
}

#[test]
fn sync_drops_branches_whose_ids_landed_on_trunk() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    stage(dir, "a.txt", "a");
    queue(dir)
        .args(["commit", "-m", "add a"])
        .assert()
        .success();
    let id = queue_id_of(dir, "HEAD");
    queue(dir).args(["create", "b"]).assert().success();
    stage(dir, "b.txt", "b");
    queue(dir)
        .args(["commit", "-m", "add b"])
        .assert()
        .success();

    // Simulate a GitHub squash-merge of `a`'s PR: one new trunk commit whose
    // body carries the constituent Queued-Commit-Id (as GitHub's default template does).
    git(dir, &["checkout", "-q", "main"]);
    stage(dir, "a.txt", "a");
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            &format!("add a (#1)\n\nQueued-Commit-Id: {id}"),
        ],
    );

    git(dir, &["checkout", "-q", "b"]);
    queue(dir).args(["sync", "--no-push"]).assert().success();

    // `a` was detected as landed and dropped; `b` reparented onto trunk.
    let gone = StdCommand::new("git")
        .args(["config", "--local", "--get", "branch.a.queueParent"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!gone.status.success(), "a should have been dropped");
    assert_eq!(git_out(dir, &["config", "branch.b.queueParent"]), "main");
    assert!(dir.join("b.txt").exists());
}

#[test]
fn track_stamp_ids_flag_stamps_adopted_commits() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    git(dir, &["checkout", "-q", "-b", "adopted"]);
    commit(dir, "one.txt");
    commit(dir, "two.txt");

    queue(dir).args(["track", "--stamp-ids"]).assert().success();

    for rev in ["adopted", "adopted~1"] {
        let id = queue_id_of(dir, rev);
        assert!(id.starts_with("q-"), "{rev} not stamped: {id:?}");
    }
    // Content intact, trunk untouched.
    assert!(dir.join("one.txt").exists() && dir.join("two.txt").exists());
    assert_eq!(
        git_out(dir, &["config", "branch.adopted.queueParent"]),
        "main"
    );
    assert_eq!(queue_id_of(dir, "main"), "");
}

#[test]
fn track_without_stamping_keeps_hashes() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    git(dir, &["checkout", "-q", "-b", "adopted"]);
    commit(dir, "one.txt");
    let before = sha(dir, "adopted");

    // --no-stamp-ids, and also the non-TTY default (no flag): both keep SHAs.
    queue(dir)
        .args(["track", "--no-stamp-ids"])
        .assert()
        .success();
    assert_eq!(sha(dir, "adopted"), before);
    assert_eq!(queue_id_of(dir, "adopted"), "");

    queue(dir).arg("untrack").assert().success();
    queue(dir).arg("track").assert().success(); // stdin is not a TTY here
    assert_eq!(sha(dir, "adopted"), before);
    assert_eq!(queue_id_of(dir, "adopted"), "");
}

#[test]
fn log_shows_indented_commits_with_id_prefixes() {
    if skip_below(REPLAY, "git replay") {
        return;
    }
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).arg("init").assert().success();
    queue(dir).args(["create", "a"]).assert().success();
    stage(dir, "a1.txt", "a1");
    queue(dir)
        .args(["commit", "-m", "add a1"])
        .assert()
        .success();
    stage(dir, "a2.txt", "a2");
    queue(dir)
        .args(["commit", "-m", "add a2"])
        .assert()
        .success();
    queue(dir).args(["create", "b"]).assert().success();
    // One commit WITHOUT an id (plain git, no hooks installed).
    stage(dir, "b1.txt", "b1");
    git(dir, &["commit", "-q", "-m", "add b1 plain"]);

    let out = queue(dir).arg("log").assert().success();
    let text = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let lines: Vec<&str> = text.lines().collect();

    // Branch lines with commits indented beneath, newest first.
    let bi = lines.iter().position(|l| l.starts_with("◉ b")).unwrap();
    assert!(lines[bi + 1].starts_with("    (no id)"), "{text}");
    assert!(lines[bi + 1].contains("add b1 plain"), "{text}");
    let ai = lines.iter().position(|l| l.starts_with("◯ a")).unwrap();
    assert!(lines[ai + 1].starts_with("    q-"), "{text}");
    assert!(lines[ai + 1].contains("add a2"), "newest first: {text}");
    assert!(lines[ai + 2].contains("add a1"), "{text}");
    // Abbreviated: the id prefix column is 10 chars, not the full 28.
    let id_col = lines[ai + 1]
        .trim_start()
        .split_whitespace()
        .next()
        .unwrap();
    assert_eq!(id_col.len(), 10, "{id_col}");
    // status stays commit-free.
    let plain = queue(dir).arg("status").assert().success();
    let ptext = String::from_utf8_lossy(&plain.get_output().stdout).to_string();
    assert!(!ptext.contains("add a1"), "{ptext}");
}

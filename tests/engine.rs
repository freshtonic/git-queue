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
use git_queue::engine::{Applied, Boundary, Engine, Operation};
use std::collections::HashSet;
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

/// Capture trimmed stdout of a raw git command in `dir`.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sha(dir: &Path, rev: &str) -> String {
    git_out(dir, &["rev-parse", rev])
}

fn branch_exists(dir: &Path, name: &str) -> bool {
    StdCommand::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")])
        .current_dir(dir)
        .status()
        .unwrap()
        .success()
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

/// Write `content` to `file` and record it as a queue commit with `msg`.
fn write_commit(dir: &Path, file: &str, content: &str, msg: &str) {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    queue(dir).args(["commit", "-m", msg]).assert().success();
}

/// Record an empty commit on the current branch (optionally message-less).
fn empty_commit(dir: &Path, message: &str, allow_empty_message: bool) {
    let mut args = vec!["commit", "--allow-empty", "-q"];
    if allow_empty_message {
        args.push("--allow-empty-message");
    }
    args.push("-m");
    args.push(message);
    git(dir, &args);
}

fn rebase_active(dir: &Path) -> bool {
    dir.join(".git/rebase-merge").exists() || dir.join(".git/rebase-apply").exists()
}

/// Finish an in-progress rebase after staging the resolution (no editor).
fn rebase_continue(dir: &Path) {
    let ok = StdCommand::new("git")
        .args(["rebase", "--continue"])
        .current_dir(dir)
        .env("GIT_EDITOR", "true")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git rebase --continue failed");
}

/// Load the engine with the process cwd pointed at `dir`, serialised so
/// parallel tests don't race the global cwd.
fn load_in(dir: &Path) -> anyhow::Result<Engine> {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_current_dir(dir).unwrap();
    Engine::load()
}

/// Load an engine and run `f` against it with the cwd lock held for the whole
/// call — every `apply` mutates git in the process cwd, so the lock must span
/// them all.
fn with_engine<T>(dir: &Path, f: impl FnOnce(&mut Engine) -> T) -> T {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_current_dir(dir).unwrap();
    let mut engine = Engine::load().expect("engine loads");
    f(&mut engine)
}

fn names(e: &Engine) -> Vec<String> {
    e.line().boundaries.iter().map(|b| b.name.clone()).collect()
}

fn ids(e: &Engine) -> Vec<Option<String>> {
    e.line().commits.iter().map(|c| c.id.clone()).collect()
}

#[test]
fn rename_branch_is_ref_only_and_reparents_children() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "a change");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "b.txt", "b change");
    let a_sha = sha(dir, "a");

    with_engine(dir, |e| {
        let ids_before = ids(e);
        e.apply(Operation::RenameBranch {
            boundary: 0,
            name: "api".into(),
        })
        .unwrap();
        assert_eq!(names(e), vec!["api", "b"]);
        assert_eq!(ids_before, ids(e), "commit ids unchanged (ref-only)");
    });

    assert!(!branch_exists(dir, "a"), "old name gone");
    assert_eq!(sha(dir, "api"), a_sha, "api sits at a's old tip (no rewrite)");
    assert_eq!(git_out(dir, &["config", "branch.b.queueParent"]), "api");
    assert_eq!(git_out(dir, &["config", "branch.api.queueParent"]), "main");
}

#[test]
fn add_boundary_splits_a_branch_ref_only() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "feat"]).assert().success();
    queue_commit(dir, "c0.txt", "zero");
    queue_commit(dir, "c1.txt", "one");
    queue_commit(dir, "c2.txt", "two");
    let tip = sha(dir, "feat");

    with_engine(dir, |e| {
        let c0 = e.line().commits[0].sha.clone();
        // Start a new branch AT commit index 1: the highlighted commit becomes
        // the front of `api`, which takes the tip-ward remainder; `feat` keeps
        // the front commit.
        e.apply(Operation::AddBoundary {
            index: 1,
            name: "api".into(),
        })
        .unwrap();
        assert_eq!(names(e), vec!["feat", "api"]);
        assert_eq!(e.line().commits_of(0).len(), 1, "feat keeps the front commit");
        assert_eq!(e.line().commits_of(1).len(), 2, "api owns from the highlight up");
        assert_eq!(e.line().commits[0].sha, c0, "commit not rewritten");
    });

    assert_eq!(sha(dir, "api"), tip, "api tips at the branch's original tip");
    assert_eq!(git_out(dir, &["config", "branch.api.queueParent"]), "feat");
    assert_eq!(git_out(dir, &["config", "branch.feat.queueParent"]), "main");
}

#[test]
fn add_boundary_at_the_front_commit_is_rejected() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "feat"]).assert().success();
    queue_commit(dir, "c0.txt", "zero");
    queue_commit(dir, "c1.txt", "one");

    with_engine(dir, |e| {
        // Starting a new branch at the branch's first commit would empty it.
        assert!(e
            .apply(Operation::AddBoundary {
                index: 0,
                name: "api".into(),
            })
            .is_err());
        assert_eq!(names(e), vec!["feat"], "nothing changed");
    });
}

#[test]
fn remove_boundary_dissolves_into_neighbour() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "a change");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "b.txt", "b change");
    let b_tip = sha(dir, "b");

    with_engine(dir, |e| {
        let shas: Vec<_> = e.line().commits.iter().map(|c| c.sha.clone()).collect();
        e.apply(Operation::RemoveBoundary { boundary: 0 }).unwrap();
        assert_eq!(names(e), vec!["b"], "a dissolved into b");
        assert_eq!(e.line().commits.len(), 2, "b now owns both commits");
        let after: Vec<_> = e.line().commits.iter().map(|c| c.sha.clone()).collect();
        assert_eq!(shas, after, "commits not rewritten");
    });

    assert!(!branch_exists(dir, "a"), "dissolved branch deleted");
    assert_eq!(sha(dir, "b"), b_tip, "b's tip unchanged");
    assert_eq!(git_out(dir, &["config", "branch.b.queueParent"]), "main");
}

#[test]
fn move_boundary_reassigns_commits_between_branches() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "c0.txt", "zero");
    queue_commit(dir, "c1.txt", "one");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "c2.txt", "two");

    with_engine(dir, |e| {
        let c0 = e.line().commits[0].sha.clone();
        // a owns [c0, c1], b owns [c2]; shift the boundary front-ward by one.
        e.apply(Operation::MoveBoundary {
            boundary: 0,
            delta: -1,
        })
        .unwrap();
        assert_eq!(e.line().commits_of(0).len(), 1, "a keeps only c0");
        assert_eq!(e.line().commits_of(1).len(), 2, "b gains c1");
        assert_eq!(sha(dir, "a"), c0, "a's ref moved back to c0 (no rewrite)");
    });
}

#[test]
fn undo_and_redo_restore_exact_refs_and_metadata() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "a change");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "b.txt", "b change");
    // A cached PR on b, so we can prove metadata is restored, not just refs.
    git(dir, &["config", "branch.b.queuePr", "77"]);
    let a_sha = sha(dir, "a");

    with_engine(dir, |e| {
        e.apply(Operation::RenameBranch {
            boundary: 0,
            name: "api".into(),
        })
        .unwrap();
        assert!(branch_exists(dir, "api") && !branch_exists(dir, "a"));

        e.apply(Operation::Undo).unwrap();
        assert_eq!(names(e), vec!["a", "b"], "undo restores the branches");
        assert!(!branch_exists(dir, "api"));
        assert_eq!(sha(dir, "a"), a_sha, "a back at its exact old sha");
        assert_eq!(git_out(dir, &["config", "branch.b.queueParent"]), "a");
        assert_eq!(
            git_out(dir, &["config", "branch.b.queuePr"]),
            "77",
            "cached PR restored"
        );

        e.apply(Operation::Redo).unwrap();
        assert_eq!(names(e), vec!["api", "b"], "redo reapplies");
        assert!(branch_exists(dir, "api"));
    });
}

#[test]
fn reorder_within_a_branch_is_clean_and_preserves_ids() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    // Distinct files, so the reorder never conflicts.
    queue_commit(dir, "f0.txt", "zero");
    queue_commit(dir, "f1.txt", "one");
    queue_commit(dir, "f2.txt", "two");

    with_engine(dir, |e| {
        let before: Vec<(String, Option<String>)> = e
            .line()
            .commits
            .iter()
            .map(|c| (c.subject.clone(), c.id.clone()))
            .collect();
        // Move the last commit ("two") to the front.
        assert_eq!(
            e.apply(Operation::Reorder { from: 2, to: 0 }).unwrap(),
            Applied::Done
        );
        let after: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(after, vec!["two", "zero", "one"]);
        // Each id travels with its message across the rebase.
        let after_ids: std::collections::HashMap<String, Option<String>> = e
            .line()
            .commits
            .iter()
            .map(|c| (c.subject.clone(), c.id.clone()))
            .collect();
        for (subj, id) in &before {
            assert!(id.is_some(), "commit {subj} was stamped");
            assert_eq!(after_ids.get(subj).unwrap(), id, "id preserved for {subj}");
        }
    });
}

#[test]
fn reorder_across_a_boundary_reassigns_the_commit() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f0.txt", "zero");
    queue_commit(dir, "f1.txt", "one");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "f2.txt", "two");

    with_engine(dir, |e| {
        assert_eq!(names(e), vec!["a", "b"]);
        // a owns [zero, one], b owns [two]. Move "one" past the boundary.
        assert_eq!(
            e.apply(Operation::Reorder { from: 1, to: 2 }).unwrap(),
            Applied::Done
        );
        assert_eq!(e.line().commits_of(0).len(), 1, "a shrinks to one commit");
        assert_eq!(e.line().commits_of(1).len(), 2, "b gains the moved commit");
        let b_subjects: Vec<String> = e
            .line()
            .commits_of(1)
            .iter()
            .map(|c| c.subject.clone())
            .collect();
        assert!(
            b_subjects.contains(&"one".to_string()),
            "the moved commit joined b: {b_subjects:?}"
        );
    });
}

#[test]
fn reword_preserves_ids_rebases_descendants_and_is_undoable() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f0.txt", "zero");
    queue_commit(dir, "f1.txt", "one");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "f2.txt", "two");
    let b_tip_before = sha(dir, "b");

    with_engine(dir, |e| {
        let ids_before = ids(e);
        let shas_before: Vec<String> = e.line().commits.iter().map(|c| c.sha.clone()).collect();

        assert_eq!(
            e.apply(Operation::Reword {
                index: 0,
                message: "reworded front".into(),
            })
            .unwrap(),
            Applied::Done
        );
        assert_eq!(e.line().commits[0].subject, "reworded front");
        assert_eq!(ids(e), ids_before, "every id preserved across the reword");
        let shas_after: Vec<String> = e.line().commits.iter().map(|c| c.sha.clone()).collect();
        assert_ne!(shas_after, shas_before, "the commits were rewritten");
        assert_eq!(names(e), vec!["a", "b"]);
        assert_ne!(sha(dir, "b"), b_tip_before, "descendant branch b rebased");

        e.apply(Operation::Undo).unwrap();
        assert_eq!(e.line().commits[0].subject, "zero", "undo restores the message");
        assert_eq!(ids(e), ids_before);
    });

    assert_eq!(sha(dir, "b"), b_tip_before, "undo restored b's exact tip");
}

#[test]
fn delete_removes_a_commit_rebases_descendants_and_is_undoable() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f0.txt", "zero");
    queue_commit(dir, "f1.txt", "one");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "f2.txt", "two");
    let b_before = sha(dir, "b");

    with_engine(dir, |e| {
        assert_eq!(
            e.apply(Operation::Delete { index: 1 }).unwrap(),
            Applied::Done
        );
        let subjects: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(subjects, vec!["zero", "two"], "commit 'one' removed");
        assert_ne!(sha(dir, "b"), b_before, "descendant branch b rebased");

        e.apply(Operation::Undo).unwrap();
        let restored: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(restored, vec!["zero", "one", "two"]);
    });
    assert_eq!(sha(dir, "b"), b_before, "undo restored b exactly");
}

#[test]
fn deleting_a_branches_only_commit_empties_it_for_dissolve() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "a-commit");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "b.txt", "b-commit");

    with_engine(dir, |e| {
        // Delete b's only commit.
        e.apply(Operation::Delete { index: 1 }).unwrap();
        assert_eq!(e.empty_branches(), vec![1], "b is now empty");

        // Dissolve it.
        e.apply(Operation::RemoveBoundary { boundary: 1 }).unwrap();
        assert_eq!(names(e), vec!["a"]);
    });
    assert!(!branch_exists(dir, "b"));
}

#[test]
fn an_empty_described_commit_is_kept_and_marked_empty() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f0.txt", "real");
    empty_commit(dir, "marker", false); // empty but described

    with_engine(dir, |e| {
        assert_eq!(e.line().commits.len(), 2);
        assert!(e.line().commits[1].empty && e.line().commits[1].described());

        // Deleting the real commit must not auto-drop the described empty.
        e.apply(Operation::Delete { index: 0 }).unwrap();
        assert_eq!(e.line().commits.len(), 1, "the described empty survives");
        assert_eq!(e.line().commits[0].subject, "marker");
        assert!(e.line().commits[0].empty);
    });
}

#[test]
fn an_empty_description_less_commit_is_auto_dropped() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f0.txt", "real");
    empty_commit(dir, "", true); // empty and description-less (mid-line)
    queue_commit(dir, "f1.txt", "second");

    with_engine(dir, |e| {
        assert_eq!(e.line().commits.len(), 3, "loads all three commits");

        // Any rewrite triggers the auto-drop of the empty, undescribed commit.
        e.apply(Operation::Delete { index: 0 }).unwrap();
        let subjects: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(subjects, vec!["second"], "empty, undescribed commit dropped");
    });
}

#[test]
fn a_conflicting_delete_can_be_undone() {
    let tmp = conflicting_reorder_repo();
    let dir = tmp.path();
    let a_before = sha(dir, "a");

    with_engine(dir, |e| {
        // Deleting "edit to A" makes "edit to B" conflict (its context changed).
        assert_eq!(
            e.apply(Operation::Delete { index: 1 }).unwrap(),
            Applied::Conflict
        );
        assert!(rebase_active(dir));
        e.undo_conflict().unwrap();
        assert!(!rebase_active(dir));
    });
    assert_eq!(sha(dir, "a"), a_before, "repo restored after undo");
}

#[test]
fn squash_keeps_the_older_id_and_combines_descriptions() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "older body");
    queue_commit(dir, "b.txt", "newer body");

    with_engine(dir, |e| {
        let older_id = e.line().commits[0].id.clone();
        assert!(older_id.is_some());
        assert_eq!(
            e.apply(Operation::Squash {
                index: 1,
                message: None,
            })
            .unwrap(),
            Applied::Done
        );
        assert_eq!(e.line().commits.len(), 1, "folded into one commit");
        assert_eq!(e.line().commits[0].id, older_id, "older commit's id kept");
    });

    let msg = git_out(dir, &["show", "-s", "--format=%B", "a"]);
    assert!(
        msg.contains("older body") && msg.contains("newer body"),
        "combined description: {msg}"
    );
    let files = git_out(dir, &["ls-tree", "-r", "--name-only", "a"]);
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "both changes present: {files}"
    );
}

#[test]
fn cross_boundary_squash_lands_in_the_older_branch_and_empties_the_newer() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "a.txt", "a body");
    queue(dir).args(["create", "b"]).assert().success();
    queue_commit(dir, "b.txt", "b body");

    with_engine(dir, |e| {
        assert_eq!(names(e), vec!["a", "b"]);
        let a_id = e.line().commits[0].id.clone();
        // Squash b's only commit into a's tip.
        assert_eq!(
            e.apply(Operation::Squash {
                index: 1,
                message: None,
            })
            .unwrap(),
            Applied::Done
        );
        assert_eq!(e.line().commits_of(0).len(), 1, "a owns the combined commit");
        assert_eq!(e.line().commits[0].id, a_id, "older branch's id kept");
        assert_eq!(e.empty_branches(), vec![1], "b emptied — dissolve offered");

        e.apply(Operation::RemoveBoundary { boundary: 1 }).unwrap();
        assert_eq!(names(e), vec!["a"]);
    });
    assert!(!branch_exists(dir, "b"));
}

#[test]
fn a_tip_commit_with_an_empty_subject_is_still_loaded() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    queue_commit(dir, "f.txt", "real");
    // A non-empty commit whose subject is empty, at the tip.
    std::fs::write(dir.join("g.txt"), "g\n").unwrap();
    git(dir, &["add", "g.txt"]);
    git(dir, &["commit", "--allow-empty-message", "-q", "-m", ""]);

    with_engine(dir, |e| {
        assert_eq!(
            e.line().commits.len(),
            2,
            "the empty-subject tip commit is not silently dropped"
        );
        assert_eq!(e.line().commits[1].subject, "");
    });
}

#[test]
fn split_divides_a_commit_keeping_the_older_id_and_stamping_the_peeled_piece() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    // One commit that adds two lines to a new file.
    write_commit(dir, "f.txt", "A\nB\n", "both lines");

    with_engine(dir, |e| {
        let c_id = e.line().commits[0].id.clone();
        assert!(c_id.is_some());
        // Peel the second added line (change index 1) into the newer commit.
        assert_eq!(
            e.apply(Operation::Split {
                index: 0,
                selected: HashSet::from([1]),
                message: "the B line".into(),
            })
            .unwrap(),
            Applied::Done
        );
        assert_eq!(e.line().commits.len(), 2, "split into two commits");

        let older = &e.line().commits[0];
        let newer = &e.line().commits[1];
        assert_eq!(older.id, c_id, "older piece keeps the original id");
        assert_eq!(older.subject, "both lines", "older keeps the message");
        assert!(newer.id.is_some() && newer.id != c_id, "peeled piece freshly stamped");
        assert_eq!(newer.subject, "the B line");
    });

    // Content divides: older has only A; the tip has both.
    assert_eq!(git_out(dir, &["show", "HEAD~1:f.txt"]), "A");
    assert_eq!(git_out(dir, &["show", "HEAD:f.txt"]), "A\nB");
}

#[test]
fn split_repeats_to_produce_three_pieces() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    write_commit(dir, "f.txt", "A\nB\nC\n", "abc");

    with_engine(dir, |e| {
        // Peel C off the end.
        e.apply(Operation::Split {
            index: 0,
            selected: HashSet::from([2]),
            message: "third".into(),
        })
        .unwrap();
        assert_eq!(e.line().commits.len(), 2);
        // Peel B off the older remainder.
        e.apply(Operation::Split {
            index: 0,
            selected: HashSet::from([1]),
            message: "second".into(),
        })
        .unwrap();

        let subjects: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(subjects, vec!["abc", "second", "third"]);
    });
    assert_eq!(git_out(dir, &["show", "HEAD:f.txt"]), "A\nB\nC");
}

#[test]
fn split_rejects_selecting_all_or_no_lines() {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    write_commit(dir, "f.txt", "A\nB\n", "both");

    with_engine(dir, |e| {
        assert!(e
            .apply(Operation::Split {
                index: 0,
                selected: HashSet::new(),
                message: "x".into(),
            })
            .is_err());
        assert!(e
            .apply(Operation::Split {
                index: 0,
                selected: HashSet::from([0, 1]),
                message: "x".into(),
            })
            .is_err());
    });
}

/// A branch whose middle commit, when reordered, textually conflicts with the
/// one it crosses. c0 lays down three lines; c1 and c2 both edit line 2.
fn conflicting_reorder_repo() -> TempDir {
    let tmp = new_repo();
    let dir = tmp.path();
    queue(dir).args(["create", "a"]).assert().success();
    write_commit(dir, "f.txt", "L1\nL2\nL3\n", "base three lines");
    write_commit(dir, "f.txt", "L1\nA\nL3\n", "edit to A");
    write_commit(dir, "f.txt", "L1\nB\nL3\n", "edit to B");
    tmp
}

#[test]
fn a_conflicting_reorder_can_be_undone_cleanly() {
    let tmp = conflicting_reorder_repo();
    let dir = tmp.path();
    let a_before = sha(dir, "a");

    with_engine(dir, |e| {
        // Moving "edit to B" before "edit to A" conflicts on line 2.
        assert_eq!(
            e.apply(Operation::Reorder { from: 2, to: 1 }).unwrap(),
            Applied::Conflict
        );
        assert!(e.conflicted());
        assert!(rebase_active(dir), "left in the mid-rebase state");

        e.undo_conflict().unwrap();
        assert!(!e.conflicted());
        assert!(!rebase_active(dir), "undo aborted the rebase");
    });

    assert_eq!(sha(dir, "a"), a_before, "repo restored exactly after undo");
}

#[test]
fn a_conflicting_reorder_leaves_a_resolvable_mid_rebase_state() {
    let tmp = conflicting_reorder_repo();
    let dir = tmp.path();

    with_engine(dir, |e| {
        assert_eq!(
            e.apply(Operation::Reorder { from: 2, to: 1 }).unwrap(),
            Applied::Conflict
        );
    });

    // The user resolves in the shell (the "resolve" escape hatch) and finishes
    // the rebase with their normal git workflow.
    assert!(rebase_active(dir));
    std::fs::write(dir.join("f.txt"), "L1\nA\nL3\n").unwrap();
    git(dir, &["add", "f.txt"]);
    rebase_continue(dir);
    assert!(!rebase_active(dir), "the rebase finished");

    // Re-entering loads the reordered line cleanly.
    with_engine(dir, |e| {
        let subjects: Vec<String> = e.line().commits.iter().map(|c| c.subject.clone()).collect();
        assert_eq!(subjects, vec!["base three lines", "edit to B", "edit to A"]);
    });
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

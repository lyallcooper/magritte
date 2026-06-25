mod common;

use std::process::Command;

use common::TestRepo;
use magritte_core::{ConflictSide, Repo};

fn git_try(t: &TestRepo, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(t.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("spawn git")
        .status
        .success()
}

/// Conflict `f` (ours = "main change", theirs = "other change") via a paused
/// cherry-pick, then resolve by taking one side.
fn conflicted() -> (TestRepo, Repo, String) {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("init");
    t.git(["checkout", "-b", "other"]);
    t.write("f", "other change\n");
    t.commit_all("other");
    let pick = t.git(["rev-parse", "HEAD"]);
    t.git(["checkout", "main"]);
    t.write("f", "main change\n");
    t.commit_all("main");
    let repo = Repo::discover(t.path()).unwrap();
    assert!(!git_try(&t, &["cherry-pick", &pick]), "expected a conflict");
    (t, repo, pick)
}

#[test]
fn take_ours_keeps_head_version_and_marks_resolved() {
    let (t, repo, _) = conflicted();
    repo.resolve_conflict("f", ConflictSide::Ours).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "main change\n"
    );
    // No longer unmerged.
    assert!(t.git(["diff", "--name-only", "--diff-filter=U"]).is_empty());
}

#[test]
fn take_theirs_keeps_incoming_version() {
    let (t, repo, _) = conflicted();
    repo.resolve_conflict("f", ConflictSide::Theirs).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "other change\n"
    );
    assert!(t.git(["diff", "--name-only", "--diff-filter=U"]).is_empty());
}

mod common;

use std::process::Command;

use common::TestRepo;
use magritte_core::{Repo, SequenceKind};

/// Run git in the repo allowing a non-zero exit (e.g. a cherry-pick that
/// conflicts), returning whether it succeeded.
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

/// Set up `main` and `other` that both touch the same line, so cherry-picking
/// `other` onto `main` conflicts and leaves a paused cherry-pick.
fn conflicting_pick() -> (TestRepo, Repo, String) {
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
    (t, repo, pick)
}

#[test]
fn no_sequence_when_idle() {
    let (_t, repo, _) = conflicting_pick();
    assert!(repo.sequence().is_none());
}

#[test]
fn detects_conflicted_cherry_pick_and_aborts() {
    let (t, repo, pick) = conflicting_pick();

    // Cherry-pick conflicts and pauses.
    assert!(!git_try(&t, &["cherry-pick", &pick]), "expected a conflict");

    let seq = repo
        .sequence()
        .expect("a cherry-pick should be in progress");
    assert_eq!(seq.kind, SequenceKind::CherryPick);
    assert_eq!(seq.heading, "Cherry Picking");
    assert!(seq.kind.can_continue() && seq.kind.can_skip());

    // Aborting clears the in-progress state.
    repo.sequence_abort(SequenceKind::CherryPick).unwrap();
    assert!(repo.sequence().is_none());
    // …and restores the pre-pick content.
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "main change\n"
    );
}

#[test]
fn detects_in_progress_merge() {
    let (t, repo, _) = conflicting_pick();
    // Merge `other` into `main` — same conflicting line → paused merge.
    assert!(
        !git_try(&t, &["merge", "other"]),
        "expected a merge conflict"
    );

    let seq = repo.sequence().expect("a merge should be in progress");
    assert_eq!(seq.kind, SequenceKind::Merge);
    assert!(seq.heading.starts_with("Merging"));
    // A merge finishes by committing, not `--continue`.
    assert!(!seq.kind.can_continue());

    repo.sequence_abort(SequenceKind::Merge).unwrap();
    assert!(repo.sequence().is_none());
}

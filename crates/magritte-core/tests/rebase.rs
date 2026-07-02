mod common;

use common::TestRepo;
use magritte_core::{RebaseAction, Repo};

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

/// A line of `git log --format=%s` (subjects, newest first).
fn subjects(t: &TestRepo) -> Vec<String> {
    t.git(["log", "--format=%s"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// base + three commits A, B, C; returns (repo dir, base sha).
fn three_commits() -> (TestRepo, String) {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    let base = t.git(["rev-parse", "HEAD"]);
    for name in ["A", "B", "C"] {
        t.write(&format!("{name}.txt"), name);
        t.commit_all(name);
    }
    (t, base)
}

#[test]
fn todo_lists_commits_oldest_first_as_picks() {
    let (t, base) = three_commits();
    let todo = open(&t).rebase_todo(&base).unwrap();
    let names: Vec<&str> = todo.iter().map(|s| s.subject.as_str()).collect();
    assert_eq!(names, ["A", "B", "C"], "oldest first");
    assert!(todo.iter().all(|s| s.action == RebaseAction::Pick));
}

#[test]
fn drop_removes_a_commit() {
    let (t, base) = three_commits();
    let mut todo = open(&t).rebase_todo(&base).unwrap();
    // Drop B.
    todo[1].action = RebaseAction::Drop;
    open(&t).rebase_interactive(&base, &todo, &[]).unwrap();
    assert_eq!(subjects(&t), ["C", "A", "base"], "B is gone");
}

#[test]
fn reorder_swaps_commit_order() {
    let (t, base) = three_commits();
    let todo = open(&t).rebase_todo(&base).unwrap();
    // Reorder to A, C, B (C and B independent files, so no conflict).
    let reordered = vec![todo[0].clone(), todo[2].clone(), todo[1].clone()];
    open(&t).rebase_interactive(&base, &reordered, &[]).unwrap();
    assert_eq!(subjects(&t), ["B", "C", "A", "base"]);
}

#[test]
fn fixup_melds_into_previous() {
    let (t, base) = three_commits();
    let mut todo = open(&t).rebase_todo(&base).unwrap();
    // Fixup C into B: C's change stays, its message is dropped.
    todo[2].action = RebaseAction::Fixup;
    open(&t).rebase_interactive(&base, &todo, &[]).unwrap();
    assert_eq!(subjects(&t), ["B", "A", "base"], "C folded into B");
    // C's file change survived the fixup.
    assert!(t.path().join("C.txt").exists());
}

#[test]
fn reword_stops_for_app_managed_message_edit() {
    let (t, base) = three_commits();
    let original_b = t.git(["rev-parse", "HEAD~1"]);
    let mut todo = open(&t).rebase_todo(&base).unwrap();
    todo[1].action = RebaseAction::Reword;
    let repo = open(&t);
    repo.rebase_interactive(&base, &todo, &[]).unwrap();
    assert_eq!(
        repo.rebase_stopped_sha().as_deref(),
        Some(original_b.as_str())
    );
    repo.commit("B rewritten", magritte_core::CommitMode::Reword, &[])
        .unwrap();
    repo.sequence_continue(magritte_core::SequenceKind::Rebase)
        .unwrap();
    assert_eq!(subjects(&t), ["C", "B rewritten", "A", "base"]);
}

#[test]
fn plain_rebase_replays_onto_target() {
    // main gains M while feature (from base) has F; rebase feature onto main.
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("F.txt", "F");
    t.commit_all("F");
    t.git(["checkout", "main"]);
    t.write("M.txt", "M");
    t.commit_all("M");
    t.git(["checkout", "feature"]);

    open(&t).rebase("main", &[]).unwrap();
    assert_eq!(subjects(&t), ["F", "M", "base"], "F replayed on top of M");
}

#[test]
fn edit_todo_rewrites_the_remaining_plan() {
    // Pause at an edit stop on A, then drop C from the remaining plan.
    let (t, base) = three_commits();
    let mut todo = open(&t).rebase_todo(&base).unwrap();
    todo[0].action = RebaseAction::Edit;
    let repo = open(&t);
    repo.rebase_interactive(&base, &todo, &[]).unwrap();

    // The injected todo carries no subjects ("edit <oid>" lines), so identify
    // the remaining steps by resolving their oids.
    let remaining = repo.rebase_current_todo().unwrap();
    let names: Vec<String> = remaining
        .iter()
        .map(|s| t.git(["log", "-1", "--format=%s", &s.oid]))
        .collect();
    assert_eq!(names, ["B", "C"], "paused on A with B and C still planned");

    repo.rebase_edit_todo(&remaining[..1]).unwrap(); // keep only B
    repo.sequence_continue(magritte_core::SequenceKind::Rebase)
        .unwrap();
    assert_eq!(
        subjects(&t),
        ["B", "A", "base"],
        "C dropped via --edit-todo"
    );
}

#[test]
fn all_dropped_is_refused() {
    let (t, base) = three_commits();
    let mut todo = open(&t).rebase_todo(&base).unwrap();
    for step in &mut todo {
        step.action = RebaseAction::Drop;
    }
    assert!(open(&t).rebase_interactive(&base, &todo, &[]).is_err());
}

#[test]
fn fixup_and_autosquash_folds_into_target() {
    // base -- A -- B; a fixup targeting A, then autosquash, should fold the
    // fixup into A and leave B on top.
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.write("a.txt", "a");
    t.commit_all("A");
    let a = t.git(["rev-parse", "HEAD"]);
    t.write("b.txt", "b");
    t.commit_all("B");

    // A staged change to fold into A.
    t.write("a.txt", "a fixed");
    t.git(["add", "a.txt"]);
    let repo = open(&t);
    repo.commit_fixup(&a, &[]).unwrap();
    assert_eq!(subjects(&t), ["fixup! A", "B", "A", "base"], "fixup created on top of HEAD");

    repo.rebase_autosquash(&format!("{a}^"), &[]).unwrap();
    assert_eq!(subjects(&t), ["B", "A", "base"], "fixup folded into A");
    // A's content now carries the fix; B untouched.
    assert_eq!(t.git(["show", "HEAD~1:a.txt"]), "a fixed");
    assert!(t.path().join("b.txt").exists());
}

#[test]
fn squash_creates_squash_commit() {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.write("a.txt", "a");
    t.commit_all("A");
    let a = t.git(["rev-parse", "HEAD"]);
    t.write("a.txt", "a more");
    t.git(["add", "a.txt"]);

    open(&t).commit_squash(&a, &[]).unwrap();
    assert_eq!(subjects(&t), ["squash! A", "A", "base"]);
}

#[test]
fn upstream_merge_base_is_none_without_upstream() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    assert_eq!(open(&t).upstream_merge_base(), None);
}

mod common;

use common::TestRepo;
use magritte_core::{Repo, ResetMode};

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

/// Two commits both rewriting `f`; returns the first commit's sha.
fn two_commits() -> (TestRepo, String) {
    let t = TestRepo::new();
    t.write("f", "one\n");
    t.commit_all("first");
    let first = t.git(["rev-parse", "HEAD"]);
    t.write("f", "two\n");
    t.commit_all("second");
    (t, first)
}

fn head_subject(t: &TestRepo) -> String {
    t.git(["log", "-1", "--format=%s"])
}

fn read(t: &TestRepo, p: &str) -> String {
    std::fs::read_to_string(t.path().join(p)).unwrap()
}

#[test]
fn soft_moves_head_and_keeps_the_change_staged() {
    let (t, first) = two_commits();
    open(&t).reset(ResetMode::Soft, &first).unwrap();
    assert_eq!(head_subject(&t), "first");
    assert_eq!(read(&t, "f"), "two\n"); // worktree untouched
    assert!(!t.git(["diff", "--cached", "--name-only"]).is_empty()); // change is staged
}

#[test]
fn mixed_moves_head_and_unstages() {
    let (t, first) = two_commits();
    open(&t).reset(ResetMode::Mixed, &first).unwrap();
    assert_eq!(head_subject(&t), "first");
    assert_eq!(read(&t, "f"), "two\n");
    assert!(t.git(["diff", "--cached", "--name-only"]).is_empty()); // nothing staged
    assert!(!t.git(["diff", "--name-only"]).is_empty()); // change is unstaged
}

#[test]
fn hard_rewinds_the_worktree() {
    let (t, first) = two_commits();
    open(&t).reset(ResetMode::Hard, &first).unwrap();
    assert_eq!(head_subject(&t), "first");
    assert_eq!(read(&t, "f"), "one\n");
}

#[test]
fn keep_moves_head_but_preserves_unrelated_work() {
    let (t, first) = two_commits();
    t.write("g", "wip\n"); // unrelated untracked work
    open(&t).reset(ResetMode::Keep, &first).unwrap();
    assert_eq!(head_subject(&t), "first");
    assert_eq!(read(&t, "f"), "one\n");
    assert_eq!(read(&t, "g"), "wip\n"); // survived the reset
}

#[test]
fn index_only_leaves_head_and_worktree() {
    let (t, first) = two_commits();
    open(&t).reset(ResetMode::Index, &first).unwrap();
    assert_eq!(head_subject(&t), "second"); // HEAD unmoved
    assert_eq!(read(&t, "f"), "two\n"); // worktree unmoved
    assert_eq!(t.git(["show", ":f"]), "one"); // index holds the target's content
}

#[test]
fn worktree_only_leaves_head_and_index() {
    let (t, first) = two_commits();
    open(&t).reset(ResetMode::Worktree, &first).unwrap();
    assert_eq!(head_subject(&t), "second"); // HEAD unmoved
    assert_eq!(t.git(["show", ":f"]), "two"); // index unmoved
    assert_eq!(read(&t, "f"), "one\n"); // worktree rewound
}

#[test]
fn branch_reset_moves_a_non_current_branch_without_touching_the_checkout() {
    let (t, first) = two_commits();
    // `other` points at the second commit; we're still on main.
    t.git(["branch", "other"]);
    let head = t.git(["rev-parse", "HEAD"]);

    open(&t).branch_reset("other", &first).unwrap();
    assert_eq!(t.git(["rev-parse", "other"]), first); // branch moved
    assert_eq!(t.git(["rev-parse", "HEAD"]), head); // HEAD untouched
    assert_eq!(read(&t, "f"), "two\n"); // worktree untouched
                                        // The move is recorded in the branch's reflog like magit's update-ref -m.
    assert!(t
        .git(["reflog", "other", "-1", "--format=%gs"])
        .contains("reset: moving to"));
}

mod common;

use common::TestRepo;
use magritte_core::{CommitMode, Repo};

fn open(repo: &TestRepo) -> Repo {
    Repo::discover(repo.path()).expect("discover repo")
}

fn subject(repo: &TestRepo) -> String {
    repo.git(["log", "-1", "--format=%s"])
}

#[test]
fn create_commit_from_staged() {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");

    t.write("new.txt", "content\n");
    t.git(["add", "new.txt"]);

    open(&t)
        .commit("Add new file\n", CommitMode::Create, &[])
        .unwrap();

    assert_eq!(subject(&t), "Add new file");
    // The file is now committed; working tree clean.
    assert!(open(&t).status().unwrap().is_clean());
}

#[test]
fn create_fails_with_nothing_staged() {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");

    let err = open(&t).commit("empty\n", CommitMode::Create, &[]);
    assert!(err.is_err(), "committing nothing staged should fail");
}

#[test]
fn amend_incorporates_staged_changes() {
    let t = TestRepo::new();
    t.write("a.txt", "one\n");
    t.commit_all("first");
    let first = t.git(["rev-parse", "HEAD"]);

    // Stage another file and amend.
    t.write("b.txt", "two\n");
    t.git(["add", "b.txt"]);
    open(&t)
        .commit("first (amended)\n", CommitMode::Amend, &[])
        .unwrap();

    assert_eq!(subject(&t), "first (amended)");
    // HEAD was replaced, and b.txt is part of it now.
    assert_ne!(t.git(["rev-parse", "HEAD"]), first);
    let files = t.git(["show", "--stat", "--format=", "HEAD"]);
    assert!(files.contains("b.txt"));
}

#[test]
fn extend_keeps_message_and_adds_staged() {
    let t = TestRepo::new();
    t.write("a.txt", "one\n");
    t.commit_all("original message");

    t.write("b.txt", "two\n");
    t.git(["add", "b.txt"]);
    open(&t).commit_extend(&[]).unwrap();

    assert_eq!(subject(&t), "original message");
    let files = t.git(["show", "--stat", "--format=", "HEAD"]);
    assert!(files.contains("b.txt"));
}

#[test]
fn reword_changes_message_without_staging() {
    let t = TestRepo::new();
    t.write("a.txt", "one\n");
    t.commit_all("typo in mesage");

    // Stage a change that reword must NOT include.
    t.write("b.txt", "should stay staged\n");
    t.git(["add", "b.txt"]);

    open(&t)
        .commit("fix typo in message\n", CommitMode::Reword, &[])
        .unwrap();

    // Message changed...
    assert_eq!(subject(&t), "fix typo in message");
    // ...but b.txt is NOT in HEAD; it's still staged.
    let files = t.git(["show", "--stat", "--format=", "HEAD"]);
    assert!(!files.contains("b.txt"), "reword must not commit staged changes");
    let staged: Vec<_> = open(&t)
        .status()
        .unwrap()
        .staged()
        .map(|e| e.path.clone())
        .collect();
    assert!(staged.contains(&"b.txt".to_string()));
}

#[test]
fn head_message_returns_full_message() {
    let t = TestRepo::new();
    t.write("a.txt", "x\n");
    t.git(["add", "a.txt"]);
    t.git(["commit", "-m", "subject line", "-m", "body paragraph"]);

    let msg = open(&t).head_message().unwrap();
    assert!(msg.starts_with("subject line"));
    assert!(msg.contains("body paragraph"));
}

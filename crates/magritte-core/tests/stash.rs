mod common;

use common::TestRepo;
use magritte_core::{Repo, StashKind};

fn repo() -> (TestRepo, Repo) {
    let t = TestRepo::new();
    t.write("f", "one\n");
    t.commit_all("init");
    let repo = Repo::discover(t.path()).unwrap();
    (t, repo)
}

#[test]
fn push_list_and_pop_round_trip() {
    let (t, repo) = repo();

    // A tracked change to stash.
    t.write("f", "two\n");
    assert!(repo.stash_list().unwrap().is_empty());

    repo.stash_push(StashKind::Both, Some("my work"), false, &[])
        .unwrap();
    let stashes = repo.stash_list().unwrap();
    assert_eq!(stashes.len(), 1);
    assert_eq!(stashes[0].reference, "stash@{0}");
    assert!(stashes[0].message.contains("my work"));
    // Working tree is clean after stashing.
    assert_eq!(t.git(["status", "--porcelain"]), "");

    // Pop restores the change and empties the list.
    repo.stash_pop("stash@{0}").unwrap();
    assert!(repo.stash_list().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "two\n"
    );
}

#[test]
fn apply_keeps_the_stash_drop_removes_it() {
    let (t, repo) = repo();
    t.write("f", "changed\n");
    repo.stash_push(StashKind::Both, None, false, &[]).unwrap();

    // Apply leaves the stash in place.
    repo.stash_apply("stash@{0}").unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);

    // Drop removes it without touching the (already-applied) working tree.
    repo.stash_drop("stash@{0}").unwrap();
    assert!(repo.stash_list().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "changed\n"
    );
}

#[test]
fn push_can_include_untracked() {
    let (t, repo) = repo();
    t.write("new.txt", "hi\n"); // untracked

    // Without -u, an untracked-only change has nothing to stash.
    repo.stash_push(StashKind::Both, None, true, &[]).unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);
    // The untracked file was stashed away.
    assert!(!t.path().join("new.txt").exists());
}

#[test]
fn staged_stash_takes_only_the_index() {
    let (t, repo) = repo();
    t.write("staged.txt", "s\n");
    t.git(["add", "staged.txt"]);
    t.write("f", "unstaged\n"); // tracked, unstaged

    repo.stash_push(StashKind::Staged, None, false, &[])
        .unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);
    // The staged file went into the stash; the unstaged change stayed put.
    assert!(!t.path().join("staged.txt").exists());
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "unstaged\n"
    );

    // Popping brings the staged change back.
    repo.stash_pop("stash@{0}").unwrap();
    assert!(t.path().join("staged.txt").exists());
}

#[test]
fn keep_index_stashes_but_leaves_the_index_applied() {
    let (t, repo) = repo();
    t.write("staged.txt", "s\n");
    t.git(["add", "staged.txt"]);
    t.write("f", "unstaged\n");

    repo.stash_push(StashKind::KeepIndex, None, false, &[])
        .unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);
    // Both changes are in the stash, but the staged one is still applied.
    assert!(t.path().join("staged.txt").exists());
    assert!(t
        .git(["diff", "--cached", "--name-only"])
        .contains("staged.txt"));
    // The unstaged change was stashed away.
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "one\n"
    );
}

#[test]
fn push_can_be_limited_to_paths() {
    let (t, repo) = repo();
    t.write("f", "two\n");
    t.write("g", "g\n");
    t.commit_all("add g");
    t.write("f", "three\n");
    t.write("g", "changed\n");

    // Only `g` goes into the stash; `f`'s change stays in the worktree.
    repo.stash_push(StashKind::Both, None, false, &["g".to_string()])
        .unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "three\n"
    );
    assert_eq!(std::fs::read_to_string(t.path().join("g")).unwrap(), "g\n");
}

#[test]
fn stash_branch_checks_out_a_new_branch_and_applies() {
    let (t, repo) = repo();
    t.write("f", "wip\n");
    repo.stash_push(StashKind::Both, None, false, &[]).unwrap();

    repo.stash_branch("from-stash", "stash@{0}").unwrap();
    assert_eq!(t.git(["branch", "--show-current"]), "from-stash");
    // The stash applied cleanly, so it was dropped.
    assert!(repo.stash_list().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "wip\n"
    );
}

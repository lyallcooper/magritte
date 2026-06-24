mod common;

use common::TestRepo;
use magritte_core::Repo;

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

    repo.stash_push(Some("my work"), false).unwrap();
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
    repo.stash_push(None, false).unwrap();

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
    repo.stash_push(None, true).unwrap();
    assert_eq!(repo.stash_list().unwrap().len(), 1);
    // The untracked file was stashed away.
    assert!(!t.path().join("new.txt").exists());
}

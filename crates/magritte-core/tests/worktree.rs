mod common;

use common::TestRepo;
use magritte_core::Repo;

/// A repo with one commit on `main`.
fn repo() -> (TestRepo, Repo) {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    let repo = Repo::discover(t.path()).unwrap();
    (t, repo)
}

#[test]
fn lists_the_main_worktree_alone_then_added_ones() {
    let (_t, repo) = repo();
    // A unique, auto-cleaned parent for the linked worktrees (not the shared
    // temp root, which would collide across runs).
    let wt_root = tempfile::tempdir().unwrap();

    // A fresh repo has exactly its main worktree, marked main + current.
    let wts = repo.worktrees().unwrap();
    assert_eq!(wts.len(), 1);
    assert!(wts[0].is_main && wts[0].is_current);
    assert_eq!(wts[0].branch.as_deref(), Some("main"));

    // Add a linked worktree checking out a new branch.
    let dir = wt_root.path().join("wt-feature");
    repo.worktree_add_branch(dir.to_str().unwrap(), "feature", None)
        .unwrap();

    let wts = repo.worktrees().unwrap();
    assert_eq!(wts.len(), 2);
    let feature = wts.iter().find(|w| w.branch.as_deref() == Some("feature"));
    let feature = feature.expect("the feature worktree is listed");
    assert!(!feature.is_main, "a linked worktree isn't the main one");
    assert!(!feature.is_current, "we're not opened on it");
    assert!(feature.head.is_some());

    // Remove it and it's gone from the listing.
    repo.worktree_remove(dir.to_str().unwrap(), false).unwrap();
    assert_eq!(repo.worktrees().unwrap().len(), 1);
}

#[test]
fn add_checks_out_an_existing_ref_in_a_new_worktree() {
    let (_t, repo) = repo();
    repo.run(["branch", "dev"]).unwrap();
    let wt_root = tempfile::tempdir().unwrap();

    let dir = wt_root.path().join("wt-dev");
    repo.worktree_add(dir.to_str().unwrap(), "dev").unwrap();

    let wts = repo.worktrees().unwrap();
    assert!(wts.iter().any(|w| w.branch.as_deref() == Some("dev")));
    assert!(dir.join("f").exists(), "the ref's tree is checked out");
}

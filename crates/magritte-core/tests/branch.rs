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
fn create_lists_and_checkout() {
    let (t, repo) = repo();

    repo.create_branch("dev", None).unwrap();
    let branches = repo.local_branches().unwrap();
    assert!(branches.iter().any(|b| b == "dev"));
    assert!(branches.iter().any(|b| b == "main"));
    // Creating doesn't switch.
    assert_eq!(t.git(["branch", "--show-current"]), "main");

    repo.checkout("dev").unwrap();
    assert_eq!(t.git(["branch", "--show-current"]), "dev");
}

#[test]
fn create_and_checkout_switches() {
    let (t, repo) = repo();
    repo.create_and_checkout("feature/x", None).unwrap();
    assert_eq!(t.git(["branch", "--show-current"]), "feature/x");
}

#[test]
fn rename_and_delete() {
    let (t, repo) = repo();
    repo.create_branch("old", None).unwrap();

    repo.rename_branch("old", "new").unwrap();
    let branches = repo.local_branches().unwrap();
    assert!(branches.iter().any(|b| b == "new"));
    assert!(!branches.iter().any(|b| b == "old"));

    repo.delete_branch("new", false).unwrap();
    assert!(!repo.local_branches().unwrap().iter().any(|b| b == "new"));
    // Sanity: HEAD is still on main.
    assert_eq!(t.git(["branch", "--show-current"]), "main");
}

#[test]
fn checkout_remote_branch_creates_tracking() {
    // A bare remote with a `feature` branch the local repo has never seen.
    let (t, repo) = repo();
    let remote = tempfile::tempdir().unwrap();
    let remote_path = remote.path().to_str().unwrap();
    t.git(["init", "--bare", remote_path]);
    t.git(["remote", "add", "origin", remote_path]);
    t.git(["push", "origin", "main:feature"]);
    t.git(["fetch", "origin"]);

    // Checking out the remote-only `origin/feature` should create a local
    // `feature` tracking branch (magit-style DWIM), not detach HEAD.
    repo.checkout("origin/feature").unwrap();
    assert_eq!(t.git(["branch", "--show-current"]), "feature");
}

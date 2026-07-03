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
fn current_branch_on_normal_unborn_and_detached_head() {
    let (t, repo) = repo();
    assert_eq!(repo.current_branch().unwrap().as_deref(), Some("main"));

    // Unborn branch (fresh repo, no commits): still names the branch — the
    // push/pull/fetch transients must open before the first commit.
    let unborn = TestRepo::new();
    let unborn_repo = Repo::discover(unborn.path()).unwrap();
    assert_eq!(
        unborn_repo.current_branch().unwrap().as_deref(),
        Some("main")
    );
    assert!(
        unborn_repo.remote_targets().is_ok(),
        "remote_targets on an unborn branch"
    );

    // Detached HEAD: no branch.
    t.git(["checkout", "--detach", "HEAD"]);
    assert_eq!(repo.current_branch().unwrap(), None);
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
fn local_branches_tracking_reports_ahead_behind() {
    let (t, repo) = repo();
    let remote = tempfile::tempdir().unwrap();
    let remote_path = remote.path().to_str().unwrap();
    t.git(["init", "--bare", remote_path]);
    t.git(["remote", "add", "origin", remote_path]);
    t.git(["push", "-u", "origin", "main"]);

    // With an upstream and no divergence, ahead/behind are 0.
    let synced = repo.local_branches_tracking().unwrap();
    let main = synced.iter().find(|b| b.name == "main").unwrap();
    assert_eq!((main.ahead, main.behind), (0, 0));

    // Two local commits not pushed → ahead 2, behind 0.
    t.write("f", "one\n");
    t.commit_all("local 1");
    t.write("f", "two\n");
    t.commit_all("local 2");
    let ahead = repo.local_branches_tracking().unwrap();
    let main = ahead.iter().find(|b| b.name == "main").unwrap();
    assert_eq!((main.ahead, main.behind), (2, 0));

    // A branch with no upstream at all reports 0/0 (not an error).
    t.git(["branch", "orphan"]);
    let all = repo.local_branches_tracking().unwrap();
    let orphan = all.iter().find(|b| b.name == "orphan").unwrap();
    assert_eq!((orphan.ahead, orphan.behind), (0, 0));
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

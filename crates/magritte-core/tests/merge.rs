mod common;

use common::TestRepo;
use magritte_core::Repo;

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

#[test]
fn clean_merge_creates_a_merge_commit() {
    // Diverged branches with disjoint files: a true (non-ff) merge.
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("g.txt", "g\n");
    t.commit_all("g");
    t.git(["checkout", "main"]);
    t.write("h.txt", "h\n");
    t.commit_all("h");

    let repo = open(&t);
    repo.merge("feature", &[]).unwrap();
    assert!(t.path().join("g.txt").exists());
    assert!(t.path().join("h.txt").exists());
    assert!(repo.sequence().is_none(), "nothing left in progress");
    let parents = t.git(["rev-list", "--parents", "-1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3, "merge commit has two parents");
}

#[test]
fn fast_forward_merge_moves_head() {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("g.txt", "g\n");
    t.commit_all("g");
    let feature = t.git(["rev-parse", "HEAD"]);
    t.git(["checkout", "main"]);

    open(&t).merge("feature", &[]).unwrap();
    assert_eq!(t.git(["rev-parse", "HEAD"]), feature, "fast-forwarded");
    assert!(t.path().join("g.txt").exists());
}

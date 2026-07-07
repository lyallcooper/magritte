mod common;

use common::TestRepo;
use magritte_core::Repo;

fn open(t: &TestRepo) -> Repo {
    Repo::discover(t.path()).expect("discover repo")
}

/// `main` with one commit; a `feature` branch adds `feature.txt`; back on
/// `main`. Returns the feature commit's sha.
fn repo_with_feature_commit() -> (TestRepo, String) {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("feature.txt", "feature\n");
    t.commit_all("add feature");
    let rev = t.git(["rev-parse", "HEAD"]);
    t.git(["checkout", "main"]);
    (t, rev)
}

fn head_subject(t: &TestRepo) -> String {
    t.git(["log", "-1", "--format=%s"])
}

#[test]
fn cherry_pick_creates_a_commit() {
    let (t, rev) = repo_with_feature_commit();
    open(&t).cherry_pick_with_args(&rev, &[]).unwrap();
    assert_eq!(head_subject(&t), "add feature");
    assert!(t.path().join("feature.txt").exists());
    assert!(
        t.git(["status", "--porcelain"]).is_empty(),
        "clean after the pick"
    );
}

#[test]
fn cherry_apply_stages_without_committing() {
    let (t, rev) = repo_with_feature_commit();
    open(&t).cherry_apply_with_args(&rev, &[]).unwrap();
    assert_eq!(head_subject(&t), "base", "HEAD unmoved");
    assert!(t
        .git(["diff", "--cached", "--name-only"])
        .contains("feature.txt"));
}

#[test]
fn revert_creates_an_inverse_commit() {
    let (t, _) = repo_with_feature_commit();
    t.write("f", "changed\n");
    t.commit_all("second");
    open(&t)
        .revert_with_args("HEAD", &["--no-edit".to_string()])
        .unwrap();
    assert!(head_subject(&t).starts_with("Revert"));
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "base\n"
    );
}

#[test]
fn revert_no_commit_stages_the_inverse() {
    let (t, _) = repo_with_feature_commit();
    t.write("f", "changed\n");
    t.commit_all("second");
    open(&t).revert_no_commit_with_args("HEAD", &[]).unwrap();
    assert_eq!(head_subject(&t), "second", "HEAD unmoved");
    assert_eq!(t.git(["show", ":f"]), "base", "inverse staged");
}

mod common;

use common::TestRepo;
use magritte_core::Repo;

fn repo() -> (TestRepo, Repo) {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("init");
    let repo = Repo::discover(t.path()).unwrap();
    (t, repo)
}

#[test]
fn create_list_and_delete_lightweight_tag() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);

    repo.create_tag("v1.0.0", &head, false).unwrap();
    assert!(repo.tags().unwrap().iter().any(|t| t == "v1.0.0"));
    assert_eq!(t.git(["rev-parse", "v1.0.0"]), head);

    repo.delete_tag("v1.0.0").unwrap();
    assert!(!repo.tags().unwrap().iter().any(|t| t == "v1.0.0"));
}

#[test]
fn tags_around_reports_nearest_behind_and_ahead() {
    // v1 -- middle -- v2; from `middle`, v1 is one behind and v2 one ahead.
    let (t, repo) = repo();
    t.git(["tag", "v1"]);
    t.write("f", "2\n");
    t.commit_all("two");
    let middle = t.git(["rev-parse", "HEAD"]);
    t.write("f", "3\n");
    t.commit_all("three");
    t.git(["tag", "v2"]);

    t.git(["checkout", &middle]);
    let (current, next) = repo.tags_around();
    assert_eq!(current, Some(("v1".to_string(), 1)));
    assert_eq!(next, Some(("v2".to_string(), 1)));

    // Exactly on a tag: distance 0, and no distinct next tag.
    t.git(["checkout", "v2"]);
    let (current, next) = repo.tags_around();
    assert_eq!(current, Some(("v2".to_string(), 0)));
    assert_eq!(next, None);
}

#[test]
fn create_annotated_tag_with_message() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);

    repo.create_annotated_tag("v1.0.0", &head, false, "Release v1.0.0\n\nFirst release.")
        .unwrap();

    // A real tag object (annotated), pointing at HEAD.
    assert_eq!(t.git(["rev-parse", "v1.0.0^{}"]), head);
    assert_eq!(
        t.git(["for-each-ref", "--format=%(objecttype)", "refs/tags/v1.0.0"]),
        "tag"
    );
    // The multi-line message is recorded as the annotation.
    assert_eq!(
        t.git(["tag", "-l", "--format=%(contents)", "v1.0.0"])
            .trim(),
        "Release v1.0.0\n\nFirst release."
    );
}

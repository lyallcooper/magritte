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
fn create_annotated_tag_without_editor() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);

    repo.create_annotated_tag("v1.0.0", &head, false).unwrap();

    assert_eq!(t.git(["rev-parse", "v1.0.0^{}"]), head);
    assert_eq!(t.git(["for-each-ref", "--format=%(objecttype)", "refs/tags/v1.0.0"]), "tag");
}

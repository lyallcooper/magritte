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
fn nearest_tag_reports_reachable_tag_with_distance() {
    // v1 -- middle -- v2; from `middle`, v1 is one behind (v2, which merely
    // contains HEAD, is deliberately not reported — see `Repo::nearest_tag`).
    let (t, repo) = repo();
    t.git(["tag", "v1"]);
    t.write("f", "2\n");
    t.commit_all("two");
    let middle = t.git(["rev-parse", "HEAD"]);
    t.write("f", "3\n");
    t.commit_all("three");
    t.git(["tag", "v2"]);

    t.git(["checkout", &middle]);
    assert_eq!(repo.nearest_tag(), Some(("v1".to_string(), 1)));

    // Exactly on a tag: distance 0.
    t.git(["checkout", "v2"]);
    assert_eq!(repo.nearest_tag(), Some(("v2".to_string(), 0)));
}

#[test]
fn tags_listed_in_version_order_highest_first() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);
    // Create out of order, including a two-digit minor that lexical sorting
    // would misplace (v0.10.0 before v0.2.0).
    for name in ["v0.2.0", "v0.10.0", "v1.0.0", "v0.3.0"] {
        repo.create_tag(name, &head, false).unwrap();
    }

    assert_eq!(
        repo.tags().unwrap(),
        ["v1.0.0", "v0.10.0", "v0.3.0", "v0.2.0"]
    );
}

#[test]
fn list_releases_orders_and_filters() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);
    repo.create_annotated_tag("v0.9.0", &head, false, "Notes 0.9.0")
        .unwrap();
    repo.create_annotated_tag("v1.0.0", &head, false, "Notes 1.0.0")
        .unwrap();
    // A non-release tag is excluded from the release list.
    repo.create_tag("nightly", &head, false).unwrap();

    let releases = repo.list_releases().unwrap();
    let tags: Vec<&str> = releases.iter().map(|r| r.tag.as_str()).collect();
    assert_eq!(tags, ["v1.0.0", "v0.9.0"]);
    assert_eq!(releases[0].version, "1.0.0");
    assert_eq!(releases[0].message, "Notes 1.0.0");
}

#[test]
fn next_release_seed_from_release_commit_reapplies_prefix() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);
    repo.create_annotated_tag("v1.0.0", &head, false, "Notes 1.0.0")
        .unwrap();
    t.write("f", "2\n");
    t.commit_all("Release version 1.1.0");

    let seed = repo.next_release_seed().unwrap();
    assert_eq!(seed.tag, "v1.1.0");
    assert!(!seed.first);
}

#[test]
fn next_release_seed_without_release_commit_uses_highest_tag() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);
    repo.create_tag("v0.2.0", &head, false).unwrap();
    repo.create_tag("v0.10.0", &head, false).unwrap();

    let seed = repo.next_release_seed().unwrap();
    assert_eq!(seed.tag, "v0.10.0");
    assert!(!seed.first);
}

#[test]
fn release_message_substitutes_version_or_defaults() {
    let (t, repo) = repo();
    let head = t.git(["rev-parse", "HEAD"]);
    repo.create_annotated_tag("v1.0.0", &head, false, "Magritte 1.0.0")
        .unwrap();
    // The previous message's version is swapped for the new one.
    assert_eq!(repo.release_message("v1.1.0").unwrap(), "Magritte 1.1.0");
}

#[test]
fn release_message_defaults_to_repo_and_version() {
    let (_t, repo) = repo();
    // No prior release: fall back to "<Repo> <version>".
    let msg = repo.release_message("v2.0.0").unwrap();
    assert!(msg.ends_with("2.0.0"), "unexpected default message: {msg}");
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

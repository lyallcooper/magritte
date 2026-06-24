mod common;

use common::TestRepo;
use magritte_core::Repo;

#[test]
fn log_lists_commits_newest_first() {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit_all("first");
    t.write("f", "2\n");
    t.commit_all("second");
    t.write("f", "3\n");
    t.commit_all("third");
    let repo = Repo::discover(t.path()).unwrap();

    let entries = repo.log("HEAD", 10).unwrap();
    let subjects: Vec<&str> = entries.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(subjects, ["third", "second", "first"]);

    // Hashes are populated and abbreviated.
    assert!(entries.iter().all(|e| !e.short_hash.is_empty()));
    assert!(entries[0].short_hash.len() < 40);
    // HEAD's decorations name the branch.
    assert!(entries[0].refs.contains("main"));
}

#[test]
fn log_respects_the_limit() {
    let t = TestRepo::new();
    for i in 0..5 {
        t.write("f", &format!("{i}\n"));
        t.commit_all(&format!("c{i}"));
    }
    let repo = Repo::discover(t.path()).unwrap();
    assert_eq!(repo.log("HEAD", 2).unwrap().len(), 2);
}

#[test]
fn log_subjects_with_separators_survive() {
    // A subject containing the unit separator-adjacent chars and spaces parses
    // cleanly (records are NUL-delimited, fields unit-separated).
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit_all("feat: do a thing (with parens) and dashes");
    let repo = Repo::discover(t.path()).unwrap();
    let entries = repo.log("HEAD", 10).unwrap();
    assert_eq!(
        entries[0].subject,
        "feat: do a thing (with parens) and dashes"
    );
}

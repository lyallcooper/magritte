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

#[test]
fn log_with_grep_filters_by_message() {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit_all("init");
    t.write("f", "2\n");
    t.commit_all("fix the bug");
    let repo = Repo::discover(t.path()).unwrap();

    let matched = repo
        .log_with(&["--grep=bug".to_string(), "HEAD".to_string()])
        .unwrap();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].subject, "fix the bug");
}

#[test]
fn upstream_divergence_splits_unpushed_and_unpulled() {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");

    t.git(["checkout", "-b", "upstream"]);
    t.write("f", "upstream\n");
    t.commit_all("upstream-only");
    let upstream = t.git(["rev-parse", "HEAD"]);

    t.git(["checkout", "main"]);
    t.write("f", "local\n");
    t.commit_all("local-only");
    t.git(["remote", "add", "origin", "https://example.com/origin.git"]);
    t.git(["update-ref", "refs/remotes/origin/main", &upstream]);
    t.git(["config", "branch.main.remote", "origin"]);
    t.git(["config", "branch.main.merge", "refs/heads/main"]);

    let repo = Repo::discover(t.path()).unwrap();
    let (unpushed, unpulled) = repo.upstream_divergence().unwrap();

    assert_eq!(
        unpushed.iter().map(|e| e.subject.as_str()).collect::<Vec<_>>(),
        ["local-only"]
    );
    assert_eq!(
        unpulled.iter().map(|e| e.subject.as_str()).collect::<Vec<_>>(),
        ["upstream-only"]
    );
}

#[test]
fn authors_are_unique() {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit_all("a");
    t.write("f", "2\n");
    t.commit_all("b"); // same author, two commits
    let repo = Repo::discover(t.path()).unwrap();

    let authors = repo.authors().unwrap();
    assert_eq!(authors, vec!["Test <test@example.com>"]);
}

#[test]
fn reflog_lists_entries() {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit_all("init");
    t.write("f", "2\n");
    t.commit_all("second");
    let repo = Repo::discover(t.path()).unwrap();

    // Committing wrote reflog entries; the newest reflects the latest commit.
    let entries = repo.reflog(10).unwrap();
    assert!(!entries.is_empty());
    assert!(entries[0].refs.starts_with("HEAD@{0}"));
}

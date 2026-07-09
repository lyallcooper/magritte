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
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "merge commit has two parents"
    );
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

/// Diverged branches with disjoint files (a clean non-ff merge candidate).
fn diverged() -> TestRepo {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("g.txt", "g\n");
    t.commit_all("g");
    t.git(["checkout", "main"]);
    t.write("h.txt", "h\n");
    t.commit_all("h");
    t
}

#[test]
fn no_commit_merge_prepares_a_message() {
    let t = diverged();
    let repo = open(&t);

    // Nothing prepared before the merge.
    assert_eq!(repo.merge_msg().unwrap(), None);

    // The editmsg mechanics: merge --no-commit --no-ff stops before
    // committing, with MERGE_MSG holding git's prepared message.
    repo.merge(
        "feature",
        &["--no-ff".to_string(), "--no-commit".to_string()],
    )
    .unwrap();
    let msg = repo.merge_msg().unwrap().expect("prepared message");
    assert!(msg.contains("Merge branch 'feature'"), "got: {msg}");

    // Committing the prepared message concludes the merge.
    repo.commit(&msg, magritte_core::CommitMode::Create, &[])
        .unwrap();
    assert!(repo.sequence().is_none());
    assert_eq!(
        Some(t.git(["log", "-1", "--format=%s"]).as_str()),
        msg.lines().next()
    );
}

#[test]
fn merge_msg_strips_the_conflicts_comment_block() {
    // Both branches rewrite the same file: the merge stops on a conflict and
    // MERGE_MSG gains a `# Conflicts:` comment block, which must not leak into
    // the seeded message.
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("base");
    t.git(["checkout", "-b", "feature"]);
    t.write("f", "feature\n");
    t.commit_all("feature change");
    t.git(["checkout", "main"]);
    t.write("f", "main\n");
    t.commit_all("main change");

    let repo = open(&t);
    assert!(repo.merge("feature", &[]).is_err(), "conflicts");
    let msg = repo.merge_msg().unwrap().expect("prepared message");
    assert!(msg.contains("Merge branch 'feature'"), "got: {msg}");
    assert!(!msg.contains('#'), "comments stripped: {msg}");
    assert!(!msg.contains("Conflicts"), "comments stripped: {msg}");
}

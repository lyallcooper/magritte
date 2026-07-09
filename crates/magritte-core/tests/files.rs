mod common;

use common::TestRepo;
use magritte_core::Repo;

#[test]
fn tracked_files_lists_committed_paths() {
    let t = TestRepo::new();
    t.write("a.txt", "1\n");
    t.write("dir/b.txt", "2\n");
    t.commit_all("init");
    // An untracked file is not listed.
    t.write("untracked.txt", "x\n");
    let repo = Repo::discover(t.path()).unwrap();

    let mut files = repo.tracked_files().unwrap();
    files.sort();
    assert_eq!(files, vec!["a.txt".to_string(), "dir/b.txt".to_string()]);
}

#[test]
fn revision_files_lists_that_revisions_tree() {
    let t = TestRepo::new();
    t.write("a.txt", "1\n");
    t.commit_all("first");
    t.write("dir/b.txt", "2\n");
    t.commit_all("second");
    let repo = Repo::discover(t.path()).unwrap();

    // HEAD has both files; HEAD~1 predates dir/b.txt.
    let mut head = repo.revision_files("HEAD").unwrap();
    head.sort();
    assert_eq!(head, vec!["a.txt".to_string(), "dir/b.txt".to_string()]);
    assert_eq!(
        repo.revision_files("HEAD~1").unwrap(),
        vec!["a.txt".to_string()]
    );
}

#[test]
fn checkout_file_restores_one_file_from_a_revision() {
    let t = TestRepo::new();
    t.write("a.txt", "old\n");
    t.write("b.txt", "keep\n");
    t.commit_all("first");
    t.write("a.txt", "new\n");
    t.write("b.txt", "also new\n");
    t.commit_all("second");
    let repo = Repo::discover(t.path()).unwrap();

    repo.checkout_file("HEAD~1", "a.txt").unwrap();
    // Only a.txt was rewound (index and worktree); HEAD didn't move.
    assert_eq!(
        std::fs::read_to_string(t.path().join("a.txt")).unwrap(),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(t.path().join("b.txt")).unwrap(),
        "also new\n"
    );
    assert_eq!(t.git(["show", ":a.txt"]), "old");
    assert_eq!(t.git(["log", "-1", "--format=%s"]), "second");
}

#[test]
fn log_can_be_limited_to_a_path() {
    let t = TestRepo::new();
    t.write("a.txt", "1\n");
    t.commit_all("touch a");
    t.write("b.txt", "1\n");
    t.commit_all("touch b");
    let repo = Repo::discover(t.path()).unwrap();

    // `git log HEAD -- a.txt` sees only the commit that changed a.txt.
    let entries = repo
        .log_with(&["HEAD".to_string(), "--".to_string(), "a.txt".to_string()])
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].subject, "touch a");
}

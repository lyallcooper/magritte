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

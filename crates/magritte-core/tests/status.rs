mod common;

use common::TestRepo;
use magritte_core::status::parse_porcelain_v2;
use magritte_core::{Change, EntryKind, Repo};

fn open(repo: &TestRepo) -> Repo {
    Repo::discover(repo.path()).expect("discover repo")
}

#[test]
fn clean_repo_reports_branch_and_no_entries() {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");

    let status = open(&t).status().unwrap();
    assert_eq!(status.head.branch.as_deref(), Some("main"));
    assert!(!status.head.detached);
    assert!(
        status.is_clean(),
        "expected clean, got {:?}",
        status.entries
    );
}

#[test]
fn untracked_file_is_reported() {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");
    t.write("new.txt", "fresh\n");

    let status = open(&t).status().unwrap();
    let untracked: Vec<_> = status.untracked().collect();
    assert_eq!(untracked.len(), 1);
    assert_eq!(untracked[0].path, "new.txt");
    assert_eq!(untracked[0].kind, EntryKind::Untracked);
}

#[test]
fn staged_addition_and_worktree_modification() {
    let t = TestRepo::new();
    t.write("README.md", "hello\n");
    t.commit_all("initial");

    // Stage a new file...
    t.write("added.txt", "v1\n");
    t.git(["add", "added.txt"]);
    // ...and modify a tracked file without staging it.
    t.write("README.md", "hello world\n");

    let status = open(&t).status().unwrap();

    let added = status
        .entries
        .iter()
        .find(|e| e.path == "added.txt")
        .expect("added.txt present");
    assert_eq!(added.index, Change::Added);
    assert!(added.is_staged());
    assert!(!added.is_unstaged());

    let readme = status
        .entries
        .iter()
        .find(|e| e.path == "README.md")
        .expect("README.md present");
    assert_eq!(readme.worktree, Change::Modified);
    assert!(readme.is_unstaged());
    assert!(!readme.is_staged());
}

#[test]
fn partially_staged_file_appears_in_both_groups() {
    let t = TestRepo::new();
    t.write("file.txt", "line\n");
    t.commit_all("initial");

    t.write("file.txt", "line edited\n");
    t.git(["add", "file.txt"]);
    t.write("file.txt", "line edited again\n");

    let status = open(&t).status().unwrap();
    let entry = &status.entries[0];
    assert_eq!(entry.index, Change::Modified);
    assert_eq!(entry.worktree, Change::Modified);
    assert!(entry.is_staged() && entry.is_unstaged());
}

#[test]
fn staged_rename_carries_original_path() {
    let t = TestRepo::new();
    t.write("old_name.txt", "stable contents\n");
    t.commit_all("initial");

    t.git(["mv", "old_name.txt", "new_name.txt"]);

    let status = open(&t).status().unwrap();
    let entry = status
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::RenamedOrCopied)
        .expect("rename entry present");
    assert_eq!(entry.path, "new_name.txt");
    assert_eq!(entry.orig_path.as_deref(), Some("old_name.txt"));
    assert!(entry.is_staged());
}

#[test]
fn paths_with_spaces_are_preserved() {
    let t = TestRepo::new();
    t.write("README.md", "x\n");
    t.commit_all("initial");
    t.write("a file with spaces.txt", "y\n");

    let status = open(&t).status().unwrap();
    assert!(status
        .entries
        .iter()
        .any(|e| e.path == "a file with spaces.txt"));
}

/// Pure parser test: a rename record's original path is the *next* NUL field.
/// Constructing the bytes by hand keeps this deterministic across git versions.
#[test]
fn parser_handles_rename_extra_nul_field() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"# branch.oid abc123");
    bytes.push(0);
    bytes.extend_from_slice(b"# branch.head main");
    bytes.push(0);
    // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>\0<origPath>\0
    bytes.extend_from_slice(b"2 R. N... 100644 100644 100644 1111 2222 R100 dst.txt");
    bytes.push(0);
    bytes.extend_from_slice(b"src.txt");
    bytes.push(0);
    // A following ordinary record must still parse after the extra field.
    bytes.extend_from_slice(b"1 .M N... 100644 100644 100644 3333 4444 other.txt");
    bytes.push(0);

    let status = parse_porcelain_v2(&bytes).unwrap();
    assert_eq!(status.head.branch.as_deref(), Some("main"));
    assert_eq!(status.entries.len(), 2);

    let rename = &status.entries[0];
    assert_eq!(rename.kind, EntryKind::RenamedOrCopied);
    assert_eq!(rename.path, "dst.txt");
    assert_eq!(rename.orig_path.as_deref(), Some("src.txt"));
    assert_eq!(rename.index, Change::Renamed);
    assert_eq!(rename.worktree, Change::Unmodified);

    let other = &status.entries[1];
    assert_eq!(other.path, "other.txt");
    assert_eq!(other.worktree, Change::Modified);
}

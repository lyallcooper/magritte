mod common;

use common::TestRepo;
use magritte_core::diff::parse_diff;
use magritte_core::{DiffSource, LineKind, Repo};

fn open(repo: &TestRepo) -> Repo {
    Repo::discover(repo.path()).expect("discover repo")
}

#[test]
fn unstaged_modification_produces_hunk() {
    let t = TestRepo::new();
    t.write("file.txt", "a\nb\nc\n");
    t.commit_all("initial");
    t.write("file.txt", "a\nB\nc\n");

    let diff = open(&t)
        .diff_path(DiffSource::Unstaged, "file.txt")
        .unwrap()
        .expect("a diff");
    assert_eq!(diff.new_path, "file.txt");
    assert_eq!(diff.hunks.len(), 1);

    let hunk = &diff.hunks[0];
    let removed: Vec<_> = hunk
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Removed)
        .collect();
    let added: Vec<_> = hunk
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(added.len(), 1);
    assert_eq!(removed[0].content, "b");
    assert_eq!(added[0].content, "B");
    // The removed "b" was old line 2; the added "B" is new line 2.
    assert_eq!(removed[0].old_lineno, Some(2));
    assert_eq!(added[0].new_lineno, Some(2));
}

#[test]
fn staged_and_unstaged_are_distinct() {
    let t = TestRepo::new();
    t.write("file.txt", "one\n");
    t.commit_all("initial");

    // Stage a change, then make a further unstaged change.
    t.write("file.txt", "one\ntwo\n");
    t.git(["add", "file.txt"]);
    t.write("file.txt", "one\ntwo\nthree\n");

    let repo = open(&t);
    let staged = repo
        .diff_path(DiffSource::Staged, "file.txt")
        .unwrap()
        .expect("staged diff");
    let unstaged = repo
        .diff_path(DiffSource::Unstaged, "file.txt")
        .unwrap()
        .expect("unstaged diff");

    let staged_added: Vec<_> = staged.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    let unstaged_added: Vec<_> = unstaged.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(staged_added, vec!["two"]);
    assert_eq!(unstaged_added, vec!["three"]);
}

#[test]
fn unchanged_path_has_no_diff() {
    let t = TestRepo::new();
    t.write("file.txt", "stable\n");
    t.commit_all("initial");

    let diff = open(&t).diff_path(DiffSource::Unstaged, "file.txt").unwrap();
    assert!(diff.is_none());
}

#[test]
fn staged_new_file_is_flagged() {
    let t = TestRepo::new();
    t.write("README.md", "x\n");
    t.commit_all("initial");
    t.write("added.txt", "hello\nworld\n");
    t.git(["add", "added.txt"]);

    let diff = open(&t)
        .diff_path(DiffSource::Staged, "added.txt")
        .unwrap()
        .expect("diff for new file");
    assert!(diff.is_new);
    assert_eq!(diff.new_path, "added.txt");
    let added: Vec<_> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(added, vec!["hello", "world"]);
}

/// Pure parser test: multiple hunks, a new-file header, and the no-newline
/// marker, constructed by hand to be deterministic across git versions.
#[test]
fn parser_handles_multiple_hunks_and_no_newline() {
    let raw = "\
diff --git a/foo.txt b/foo.txt
index 1234567..89abcde 100644
--- a/foo.txt
+++ b/foo.txt
@@ -1,3 +1,3 @@ fn header
 context one
-old two
+new two
@@ -10,2 +10,3 @@
 ten
+inserted
 eleven
\\ No newline at end of file
";
    let files = parse_diff(raw.as_bytes()).unwrap();
    assert_eq!(files.len(), 1);
    let file = &files[0];
    assert_eq!(file.old_path, "foo.txt");
    assert_eq!(file.new_path, "foo.txt");
    assert_eq!(file.hunks.len(), 2);

    let first = &file.hunks[0];
    assert_eq!(
        (first.old_start, first.old_count, first.new_start, first.new_count),
        (1, 3, 1, 3)
    );
    assert_eq!(first.section_heading, "fn header");

    let second = &file.hunks[1];
    assert_eq!(second.old_start, 10);
    assert_eq!(second.new_start, 10);
    // The inserted line is new line 11 (10=ten, 11=inserted, then eleven).
    let inserted = second
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added)
        .unwrap();
    assert_eq!(inserted.content, "inserted");
    assert_eq!(inserted.new_lineno, Some(11));
    // The trailing marker is parsed as NoNewline and advances no counters.
    assert!(second.lines.iter().any(|l| l.kind == LineKind::NoNewline));
}

#[test]
fn line_counts_report_changed_lines() {
    let t = TestRepo::new();
    t.write("a.txt", "1\n2\n3\n");
    t.write("b.txt", "x\n");
    t.commit_all("init");
    // a.txt: change one line (1 add + 1 remove); b.txt: append two lines.
    t.write("a.txt", "1\nTWO\n3\n");
    t.write("b.txt", "x\ny\nz\n");

    let counts = open(&t).diff_line_counts(DiffSource::Unstaged).unwrap();
    let map: std::collections::HashMap<_, _> = counts.into_iter().collect();
    assert_eq!(map.get("a.txt"), Some(&2));
    assert_eq!(map.get("b.txt"), Some(&2));
}

#[test]
fn parser_detects_binary() {
    let raw = "\
diff --git a/img.png b/img.png
index 1234567..89abcde 100644
Binary files a/img.png and b/img.png differ
";
    let files = parse_diff(raw.as_bytes()).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].is_binary);
    assert!(files[0].hunks.is_empty());
}

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
        .diff_path(DiffSource::Unstaged, "file.txt", None)
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
        .diff_path(DiffSource::Staged, "file.txt", None)
        .unwrap()
        .expect("staged diff");
    let unstaged = repo
        .diff_path(DiffSource::Unstaged, "file.txt", None)
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

    let diff = open(&t)
        .diff_path(DiffSource::Unstaged, "file.txt", None)
        .unwrap();
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
        .diff_path(DiffSource::Staged, "added.txt", None)
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
        (
            first.old_start,
            first.old_count,
            first.new_start,
            first.new_count
        ),
        (1, 3, 1, 3)
    );
    assert_eq!(first.section_heading, "fn header");
    // Jump to the first *changed* line (new "new two" at line 2), not the
    // hunk's leading context (line 1).
    assert_eq!(first.first_change_new_line(), 2);

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

/// CRLF content lines must keep their `\r` so reconstructed patches preserve
/// the original line endings (a `str::lines()` split would drop it).
#[test]
fn parser_preserves_crlf_in_content() {
    let raw = concat!(
        "diff --git a/f.txt b/f.txt\n",
        "index 1111111..2222222 100644\n",
        "--- a/f.txt\n",
        "+++ b/f.txt\n",
        "@@ -1,2 +1,2 @@\n",
        " keep\r\n",
        "-old\r\n",
        "+new\r\n",
    );
    let files = parse_diff(raw.as_bytes()).unwrap();
    let hunk = &files[0].hunks[0];
    let added = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added)
        .unwrap();
    let context = hunk
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Context)
        .unwrap();
    assert_eq!(added.content, "new\r", "carriage return must be preserved");
    assert_eq!(context.content, "keep\r");
    // No spurious trailing empty context line from the final newline.
    assert!(!hunk
        .lines
        .iter()
        .any(|l| l.kind == LineKind::Context && l.content.is_empty()));
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

#[test]
fn tracked_vs_head_combines_staged_and_unstaged() {
    let t = TestRepo::new();
    t.write("file.txt", "a\nb\nc\n");
    t.commit_all("initial");
    // One staged change and one further unstaged change to the same file.
    t.write("file.txt", "A\nb\nc\n");
    t.git(["add", "file.txt"]);
    t.write("file.txt", "A\nb\nC\n");

    let diff = open(&t).diff_tracked_vs_head().unwrap();
    assert_eq!(diff.len(), 1, "the file appears once, not per source");
    let added: Vec<_> = diff[0]
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    // Both the staged (A) and unstaged (C) edits are in the commit-all preview.
    assert!(added.contains(&"A"), "staged edit present: {added:?}");
    assert!(added.contains(&"C"), "unstaged edit present: {added:?}");
}

#[test]
fn tracked_vs_head_on_unborn_branch_shows_staged() {
    // No commits yet: `git diff HEAD` would error, so the staged diff is used.
    let t = TestRepo::new();
    t.write("file.txt", "a\nb\n");
    t.git(["add", "file.txt"]);

    let diff = open(&t)
        .diff_tracked_vs_head()
        .expect("unborn branch must not error");
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].new_path, "file.txt");
}

#[test]
fn staged_rename_with_orig_path_diffs_as_rename() {
    // With only the new path in the pathspec, git reports the rename as a
    // whole-file addition; including the original path restores the rename
    // diff (the display and hunk-level unstage both depend on it).
    let t = TestRepo::new();
    t.write("old.txt", "one\ntwo\nthree\n");
    t.commit_all("initial");
    t.git(["mv", "old.txt", "new.txt"]);
    t.write("new.txt", "one\nTWO\nthree\n");
    t.git(["add", "new.txt"]);

    let diff = open(&t)
        .diff_path(DiffSource::Staged, "new.txt", Some("old.txt"))
        .unwrap()
        .expect("a diff");
    assert_eq!(diff.old_path, "old.txt");
    assert_eq!(diff.new_path, "new.txt");
    assert!(!diff.is_new, "a rename is not a new file");
    let changed: Vec<_> = diff.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(changed, ["two", "TWO"], "only the edit, not the whole file");
}

#[test]
fn parse_diff_skips_combined_cc_records() {
    // A conflicted merge emits `diff --cc` records with `@@@` hunks; they must
    // be skipped as a unit, not fed to the ordinary hunk parser (which would
    // fail the whole parse), and must not swallow a following ordinary record.
    let text = "\
diff --cc conflicted.txt
index 1111111,2222222..0000000
--- a/conflicted.txt
+++ b/conflicted.txt
@@@ -1,3 -1,3 +1,7 @@@
++<<<<<<< HEAD
 +ours
++=======
+ theirs
++>>>>>>> branch
diff --git a/plain.txt b/plain.txt
index 3333333..4444444 100644
--- a/plain.txt
+++ b/plain.txt
@@ -1 +1 @@
-a
+b
";
    let files = parse_diff(text.as_bytes()).expect("cc record must not fail the parse");
    assert_eq!(files.len(), 1, "only the ordinary record is modeled");
    assert_eq!(files[0].new_path, "plain.txt");
    assert_eq!(files[0].hunks.len(), 1);
}

#[test]
fn parse_diff_unquotes_c_quoted_paths() {
    // Even with core.quotepath=false git C-quotes paths containing quotes,
    // backslashes, or control characters on the header lines. The quoted form
    // of `we<TAB>ird"name.txt` is `"we\tird\"name.txt"`.
    let text = r#"diff --git "a/we\tird\"name.txt" "b/we\tird\"name.txt"
index 1111111..2222222 100644
--- "a/we\tird\"name.txt"
+++ "b/we\tird\"name.txt"
@@ -1 +1 @@
-a
+b
"#;
    let files = parse_diff(text.as_bytes()).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].new_path, "we\tird\"name.txt");
    assert_eq!(files[0].old_path, files[0].new_path);
}

#[test]
fn diff_path_survives_a_path_with_spaces() {
    // git appends a trailing tab to `---`/`+++` paths containing spaces; only
    // that tab may be stripped (not all whitespace, which can be part of a
    // filename).
    let t = TestRepo::new();
    t.write("has space.txt", "a\n");
    t.commit_all("initial");
    t.write("has space.txt", "b\n");

    let diff = open(&t)
        .diff_path(DiffSource::Unstaged, "has space.txt", None)
        .unwrap()
        .expect("a diff");
    assert_eq!(diff.new_path, "has space.txt");
}

#[test]
fn parse_diff_keeps_non_utf8_line_bytes() {
    // A Latin-1 0xE9 byte on a context and an added line: the display content
    // decodes lossily, but the original bytes stay available for patch
    // reconstruction.
    let mut raw: Vec<u8> = Vec::new();
    raw.extend_from_slice(b"diff --git a/f b/f\n");
    raw.extend_from_slice(b"index 1111111..2222222 100644\n");
    raw.extend_from_slice(b"--- a/f\n");
    raw.extend_from_slice(b"+++ b/f\n");
    raw.extend_from_slice(b"@@ -1,2 +1,2 @@\n");
    raw.extend_from_slice(b" caf\xE9 ctx\n");
    raw.extend_from_slice(b"-old\n");
    raw.extend_from_slice(b"+new caf\xE9\n");

    let files = parse_diff(&raw).unwrap();
    let hunk = &files[0].hunks[0];

    let ctx = &hunk.lines[0];
    assert_eq!(ctx.content, "caf\u{FFFD} ctx", "display is lossy");
    assert_eq!(ctx.content_bytes(), b"caf\xE9 ctx", "bytes are preserved");
    let removed = &hunk.lines[1];
    assert!(removed.raw.is_none(), "valid UTF-8 keeps no raw copy");
    assert_eq!(removed.content_bytes(), b"old");
    assert_eq!(hunk.lines[2].content_bytes(), b"new caf\xE9");
    // The header was pure ASCII, so no raw copy is kept.
    assert!(files[0].header_raw.is_none());
}

#[test]
fn line_changes_classifies_each_run() {
    let raw = "\
diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -1,6 +1,7 @@
 ctx
-replaced
+replacement a
+replacement b
 ctx
+added
 ctx
-removed
 ctx
";
    use magritte_core::LineChange::{Added, Changed, Removed};
    let files = parse_diff(raw.as_bytes()).unwrap();
    assert_eq!(
        files[0].hunks[0].line_changes(),
        vec![
            None,
            Some(Changed),
            Some(Changed),
            Some(Changed),
            None,
            Some(Added),
            None,
            Some(Removed),
            None,
        ]
    );
}

#[test]
fn line_changes_keeps_no_newline_marker_in_its_run() {
    let raw = "\
diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -1,2 +1,2 @@
 ctx
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
    use magritte_core::LineChange::Changed;
    let files = parse_diff(raw.as_bytes()).unwrap();
    assert_eq!(
        files[0].hunks[0].line_changes(),
        vec![
            None,
            Some(Changed),
            Some(Changed),
            Some(Changed),
            Some(Changed),
        ]
    );
}

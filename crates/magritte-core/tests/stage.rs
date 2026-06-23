mod common;

use common::TestRepo;
use magritte_core::stage::build_patch;
use magritte_core::{DiffSource, FileEntry, LineKind, Repo};

fn open(repo: &TestRepo) -> Repo {
    Repo::discover(repo.path()).expect("discover repo")
}

fn find(repo: &Repo, source: DiffSource, path: &str) -> Option<magritte_core::FileDiff> {
    repo.diff_path(source, path).unwrap()
}

/// Index of the first line in the hunk matching `(kind, content)`.
fn line_index(hunk: &magritte_core::Hunk, kind: LineKind, content: &str) -> usize {
    hunk.lines
        .iter()
        .position(|l| l.kind == kind && l.content == content)
        .unwrap_or_else(|| panic!("line {kind:?} {content:?} not found"))
}

fn entry<'a>(status: &'a magritte_core::Status, path: &str) -> Option<&'a FileEntry> {
    status.entries.iter().find(|e| e.path == path)
}

#[test]
fn stage_and_unstage_whole_file() {
    let t = TestRepo::new();
    t.write("file.txt", "a\n");
    t.commit_all("init");
    t.write("file.txt", "a\nb\n");

    let repo = open(&t);
    repo.stage_file("file.txt").unwrap();
    let s = repo.status().unwrap();
    assert!(entry(&s, "file.txt").unwrap().is_staged());
    assert!(!entry(&s, "file.txt").unwrap().is_unstaged());

    repo.unstage_file("file.txt").unwrap();
    let s = repo.status().unwrap();
    assert!(entry(&s, "file.txt").unwrap().is_unstaged());
    assert!(!entry(&s, "file.txt").unwrap().is_staged());
}

#[test]
fn stage_then_unstage_whole_hunk() {
    let t = TestRepo::new();
    t.write("file.txt", "a\nb\nc\n");
    t.commit_all("init");
    t.write("file.txt", "a\nB\nc\n");

    let repo = open(&t);
    let diff = find(&repo, DiffSource::Unstaged, "file.txt").unwrap();
    repo.stage_hunk(&diff, &diff.hunks[0]).unwrap();

    // Now staged shows the change; nothing remains unstaged.
    let staged = find(&repo, DiffSource::Staged, "file.txt").expect("staged diff");
    assert!(staged.hunks[0]
        .lines
        .iter()
        .any(|l| l.kind == LineKind::Added && l.content == "B"));
    assert!(find(&repo, DiffSource::Unstaged, "file.txt").is_none());

    // Unstage the hunk again -> back to unstaged only.
    let staged_again = find(&repo, DiffSource::Staged, "file.txt").unwrap();
    repo.unstage_hunk(&staged_again, &staged_again.hunks[0])
        .unwrap();
    assert!(find(&repo, DiffSource::Staged, "file.txt").is_none());
    assert!(find(&repo, DiffSource::Unstaged, "file.txt").is_some());
}

#[test]
fn discard_hunk_reverts_working_tree() {
    let t = TestRepo::new();
    t.write("file.txt", "keep\nchange me\n");
    t.commit_all("init");
    t.write("file.txt", "keep\nchanged\n");

    let repo = open(&t);
    let diff = find(&repo, DiffSource::Unstaged, "file.txt").unwrap();
    repo.discard_hunk(&diff, &diff.hunks[0]).unwrap();

    // The working tree is back to the committed content.
    assert!(find(&repo, DiffSource::Unstaged, "file.txt").is_none());
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert_eq!(contents, "keep\nchange me\n");
}

/// The crux of M3: stage only one of two changes in a single hunk.
#[test]
fn stage_subset_of_lines_in_a_hunk() {
    let t = TestRepo::new();
    t.write("file.txt", "1\n2\n3\n4\n5\n");
    t.commit_all("init");
    // Change lines 2 and 4; within 3 lines of each other -> one hunk.
    t.write("file.txt", "1\nTWO\n3\nFOUR\n5\n");

    let repo = open(&t);
    let diff = find(&repo, DiffSource::Unstaged, "file.txt").unwrap();
    let hunk = &diff.hunks[0];

    // Select only the first change (remove "2", add "TWO").
    let selected = vec![
        line_index(hunk, LineKind::Removed, "2"),
        line_index(hunk, LineKind::Added, "TWO"),
    ];
    repo.stage_lines(&diff, hunk, &selected).unwrap();

    // Staged side has only the first change...
    let staged = find(&repo, DiffSource::Staged, "file.txt").expect("staged diff");
    let staged_adds: Vec<_> = staged.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(staged_adds, vec!["TWO"]);

    // ...and the second change is still unstaged.
    let unstaged = find(&repo, DiffSource::Unstaged, "file.txt").expect("unstaged diff");
    let unstaged_adds: Vec<_> = unstaged.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.as_str())
        .collect();
    assert_eq!(unstaged_adds, vec!["FOUR"]);
}

/// Round-trip the line-subset: unstage just one change back out of the index.
#[test]
fn unstage_subset_of_lines() {
    let t = TestRepo::new();
    t.write("file.txt", "1\n2\n3\n4\n5\n");
    t.commit_all("init");
    t.write("file.txt", "1\nTWO\n3\nFOUR\n5\n");

    let repo = open(&t);
    // Stage everything first.
    repo.stage_file("file.txt").unwrap();
    let staged = find(&repo, DiffSource::Staged, "file.txt").unwrap();
    let hunk = &staged.hunks[0];

    // Unstage only the second change (remove "4", add "FOUR").
    let selected = vec![
        line_index(hunk, LineKind::Removed, "4"),
        line_index(hunk, LineKind::Added, "FOUR"),
    ];
    repo.unstage_lines(&staged, hunk, &selected).unwrap();

    // Index keeps the first change; the second is unstaged again.
    let staged_adds: Vec<_> = find(&repo, DiffSource::Staged, "file.txt").unwrap().hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.clone())
        .collect();
    assert_eq!(staged_adds, vec!["TWO"]);

    let unstaged_adds: Vec<_> = find(&repo, DiffSource::Unstaged, "file.txt").unwrap().hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.clone())
        .collect();
    assert_eq!(unstaged_adds, vec!["FOUR"]);
}

#[test]
fn discard_staged_file_reverts_to_head() {
    let t = TestRepo::new();
    t.write("file.txt", "original\n");
    t.commit_all("init");
    t.write("file.txt", "changed\n");
    let repo = open(&t);
    repo.stage_file("file.txt").unwrap();

    repo.discard_staged_file("file.txt").unwrap();

    let s = repo.status().unwrap();
    assert!(entry(&s, "file.txt").is_none(), "file should be clean");
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert_eq!(contents, "original\n");
}

/// The bug the reviewer flagged: discarding a *staged* change on an MM file
/// must drop the staged delta but PRESERVE the unrelated unstaged worktree edit
/// (the old `git checkout HEAD -- path` reverted the whole worktree to base).
/// The two edits are kept far apart so the staged hunk lifts out cleanly.
#[test]
fn discard_staged_file_preserves_unstaged_edit() {
    let t = TestRepo::new();
    let base: String = (1..=10).map(|n| format!("line{n}\n")).collect();
    t.write("file.txt", &base);
    t.commit_all("init");
    // Stage a change to line 2... (tokens chosen so neither contains the other)
    let mut lines: Vec<String> = (1..=10).map(|n| format!("line{n}")).collect();
    lines[1] = "STAGEDONLY".into();
    t.write("file.txt", &format!("{}\n", lines.join("\n")));
    t.git(["add", "file.txt"]);
    // ...then an unrelated unstaged change to line 9 (well outside the hunk).
    lines[8] = "WORKONLY".into();
    t.write("file.txt", &format!("{}\n", lines.join("\n")));

    let repo = open(&t);
    repo.discard_staged_file("file.txt").unwrap();

    // line 2 reverted (staged delta gone); line 9 still carries the unstaged edit.
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert!(
        contents.contains("\nline2\n"),
        "staged delta should be reverted"
    );
    assert!(
        !contents.contains("STAGEDONLY"),
        "staged delta should be gone"
    );
    assert!(
        contents.contains("WORKONLY"),
        "unstaged edit must be preserved"
    );
    let s = repo.status().unwrap();
    let e = entry(&s, "file.txt").unwrap();
    assert!(!e.is_staged(), "staged delta should be discarded");
    assert!(e.is_unstaged(), "unstaged edit should remain");
}

/// When the unstaged edit overlaps the staged hunk, the worktree `--reject`
/// step leaves a `.rej` and exits non-zero. The index delta is still removed,
/// but the partial result is reported as an error rather than swallowed.
#[test]
fn discard_staged_file_reports_partial_on_overlap() {
    let t = TestRepo::new();
    t.write("file.txt", "line1\nline2\nline3\n");
    t.commit_all("init");
    // Stage a change to line2, then make an *overlapping* unstaged edit to it.
    t.write("file.txt", "line1\nSTAGEDONLY\nline3\n");
    t.git(["add", "file.txt"]);
    t.write("file.txt", "line1\nWORKONLY\nline3\n");

    let repo = open(&t);
    let result = repo.discard_staged_file("file.txt");
    assert!(
        result.is_err(),
        "overlapping worktree hunk should report partial"
    );

    // The index delta was still removed (nothing staged), and the worktree
    // edit is preserved (a .rej was written rather than clobbering it).
    let s = repo.status().unwrap();
    assert!(!entry(&s, "file.txt").unwrap().is_staged());
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert!(contents.contains("WORKONLY"), "unstaged edit preserved");
}

/// A staged brand-new file (no further edits) is deleted on discard.
#[test]
fn discard_staged_new_file_deletes_it() {
    let t = TestRepo::new();
    t.write("keep.txt", "x\n");
    t.commit_all("init");
    t.write("added.txt", "new\n");
    t.git(["add", "added.txt"]);

    let repo = open(&t);
    repo.discard_staged_file("added.txt").unwrap();
    assert!(
        !t.path().join("added.txt").exists(),
        "new file should be removed"
    );
    assert!(repo.status().unwrap().is_clean());
}

/// A staged new file that also has unstaged edits falls back to untracked
/// (the file and its content stay; it's just no longer staged).
#[test]
fn discard_staged_new_file_with_unstaged_becomes_untracked() {
    let t = TestRepo::new();
    t.write("keep.txt", "x\n");
    t.commit_all("init");
    t.write("added.txt", "v1\n");
    t.git(["add", "added.txt"]);
    t.write("added.txt", "v1\nv2\n"); // unstaged edit on top of the staged add

    let repo = open(&t);
    repo.discard_staged_file("added.txt").unwrap();

    assert!(t.path().join("added.txt").exists(), "file should be kept");
    let s = repo.status().unwrap();
    let e = entry(&s, "added.txt").unwrap();
    assert_eq!(e.kind, magritte_core::EntryKind::Untracked);
}

/// Discarding a staged deletion resurrects the file.
/// Unstaging a staged rename must fully undo it in the index, not leave the
/// original path's deletion staged (`git reset -- <new>` alone is incomplete).
#[test]
fn unstage_staged_rename_is_complete() {
    let t = TestRepo::new();
    t.write("old.txt", "stable\n");
    t.commit_all("init");
    t.git(["mv", "old.txt", "new.txt"]);

    let repo = open(&t);
    repo.unstage_file("new.txt").unwrap();

    // Nothing should remain staged (no lingering `D old.txt`).
    let s = repo.status().unwrap();
    assert!(
        s.staged().next().is_none(),
        "rename should be fully unstaged"
    );
    // The rename is now reflected only in the worktree (old gone, new present).
    assert!(!t.path().join("old.txt").exists());
    assert!(t.path().join("new.txt").exists());
}

#[test]
fn discard_staged_deletion_resurrects() {
    let t = TestRepo::new();
    t.write("doomed.txt", "alive\n");
    t.commit_all("init");
    t.git(["rm", "doomed.txt"]); // stages the deletion

    let repo = open(&t);
    repo.discard_staged_file("doomed.txt").unwrap();

    // No longer a staged deletion; HEAD content is back in the index.
    let s = repo.status().unwrap();
    assert!(!entry(&s, "doomed.txt")
        .map(|e| e.is_staged())
        .unwrap_or(false));
}

/// Discarding a staged rename renames the file back to its original path.
#[test]
fn discard_staged_rename_renames_back() {
    let t = TestRepo::new();
    t.write("old.txt", "stable contents here\n");
    t.commit_all("init");
    t.git(["mv", "old.txt", "new.txt"]); // stages the rename

    let repo = open(&t);
    repo.discard_staged_file("new.txt").unwrap();

    assert!(
        t.path().join("old.txt").exists(),
        "should be renamed back to old"
    );
    assert!(!t.path().join("new.txt").exists());
    assert!(
        repo.status().unwrap().is_clean(),
        "tree should be clean again"
    );
}

#[test]
fn discard_staged_hunk_reverts_index_and_worktree() {
    let t = TestRepo::new();
    t.write("file.txt", "a\nb\nc\n");
    t.commit_all("init");
    t.write("file.txt", "a\nB\nc\n");
    let repo = open(&t);
    repo.stage_file("file.txt").unwrap();

    let staged = find(&repo, DiffSource::Staged, "file.txt").unwrap();
    repo.discard_staged_hunk(&staged, &staged.hunks[0]).unwrap();

    let s = repo.status().unwrap();
    assert!(entry(&s, "file.txt").is_none(), "expected clean tree");
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert_eq!(contents, "a\nb\nc\n");
}

#[test]
fn discard_staged_lines_subset() {
    let t = TestRepo::new();
    t.write("file.txt", "1\n2\n3\n4\n5\n");
    t.commit_all("init");
    t.write("file.txt", "1\nTWO\n3\nFOUR\n5\n");
    let repo = open(&t);
    repo.stage_file("file.txt").unwrap();

    // Discard only the first staged change (remove "2", add "TWO").
    let staged = find(&repo, DiffSource::Staged, "file.txt").unwrap();
    let hunk = &staged.hunks[0];
    let selected = vec![
        line_index(hunk, LineKind::Removed, "2"),
        line_index(hunk, LineKind::Added, "TWO"),
    ];
    repo.discard_staged_lines(&staged, hunk, &selected).unwrap();

    // First change gone from both sides; second change still staged.
    let contents = std::fs::read_to_string(t.path().join("file.txt")).unwrap();
    assert_eq!(contents, "1\n2\n3\nFOUR\n5\n");
    let staged_adds: Vec<_> = find(&repo, DiffSource::Staged, "file.txt").unwrap().hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.clone())
        .collect();
    assert_eq!(staged_adds, vec!["FOUR"]);
    assert!(find(&repo, DiffSource::Unstaged, "file.txt").is_none());
}

#[test]
fn stage_lines_across_two_hunks() {
    let t = TestRepo::new();
    let original: String = (1..=20).map(|n| format!("{n}\n")).collect();
    t.write("file.txt", &original);
    t.commit_all("init");
    // Change line 2 and line 18 -> far apart -> two separate hunks.
    let mut lines: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
    lines[1] = "TWO".to_string();
    lines[17] = "EIGHTEEN".to_string();
    t.write("file.txt", &format!("{}\n", lines.join("\n")));

    let repo = open(&t);
    let diff = find(&repo, DiffSource::Unstaged, "file.txt").unwrap();
    assert_eq!(diff.hunks.len(), 2, "expected two separate hunks");

    // Select the change in each hunk and stage both in one region apply.
    let sel0 = vec![
        line_index(&diff.hunks[0], LineKind::Removed, "2"),
        line_index(&diff.hunks[0], LineKind::Added, "TWO"),
    ];
    let sel1 = vec![
        line_index(&diff.hunks[1], LineKind::Removed, "18"),
        line_index(&diff.hunks[1], LineKind::Added, "EIGHTEEN"),
    ];
    repo.stage_file_lines(&diff, &[(0, sel0), (1, sel1)])
        .unwrap();

    // Both changes are now staged, and nothing remains unstaged.
    let staged_adds: Vec<_> = find(&repo, DiffSource::Staged, "file.txt")
        .unwrap()
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.clone())
        .collect();
    assert_eq!(staged_adds, vec!["TWO", "EIGHTEEN"]);
    assert!(find(&repo, DiffSource::Unstaged, "file.txt").is_none());
}

#[test]
fn discard_untracked_file_removes_it() {
    let t = TestRepo::new();
    t.write("README.md", "x\n");
    t.commit_all("init");
    t.write("junk.txt", "garbage\n");

    let repo = open(&t);
    repo.discard_untracked_file("junk.txt").unwrap();
    assert!(!t.path().join("junk.txt").exists());
}

/// Direct check of the selective patch builder: a forward subset drops
/// unselected additions and turns unselected removals into context.
#[test]
fn build_patch_forward_subset_conversions() {
    use magritte_core::diff::parse_diff;

    let raw = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,5 +1,5 @@
 1
-2
+TWO
 3
-4
+FOUR
 5
";
    let file = &parse_diff(raw.as_bytes()).unwrap()[0];
    let hunk = &file.hunks[0];
    // Select only the first change.
    let sel = vec![
        line_index(hunk, LineKind::Removed, "2"),
        line_index(hunk, LineKind::Added, "TWO"),
    ];
    let patch = build_patch(file, hunk, &sel, false);

    // The unselected "+FOUR" is dropped; the unselected "-4" becomes context.
    assert!(patch.contains("\n-2\n"));
    assert!(patch.contains("\n+TWO\n"));
    assert!(!patch.contains("+FOUR"));
    assert!(
        patch.contains("\n 4\n"),
        "unselected removal should be context"
    );
    // old side: 1,2,3,4,5 = 5 lines; new side: 1,TWO,3,4,5 = 5 lines.
    assert!(patch.contains("@@ -1,5 +1,5 @@"));
}

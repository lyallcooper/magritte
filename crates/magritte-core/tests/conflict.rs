mod common;

use std::process::Command;

use common::TestRepo;
use magritte_core::conflict::{parse_conflicts, resolve, Segment};
use magritte_core::{ConflictSide, Repo, Resolution};

fn git_try(t: &TestRepo, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(t.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("spawn git")
        .status
        .success()
}

/// Conflict `f` (ours = "main change", theirs = "other change") via a paused
/// cherry-pick, then resolve by taking one side.
fn conflicted() -> (TestRepo, Repo, String) {
    let t = TestRepo::new();
    t.write("f", "base\n");
    t.commit_all("init");
    t.git(["checkout", "-b", "other"]);
    t.write("f", "other change\n");
    t.commit_all("other");
    let pick = t.git(["rev-parse", "HEAD"]);
    t.git(["checkout", "main"]);
    t.write("f", "main change\n");
    t.commit_all("main");
    let repo = Repo::discover(t.path()).unwrap();
    assert!(!git_try(&t, &["cherry-pick", &pick]), "expected a conflict");
    (t, repo, pick)
}

#[test]
fn take_ours_keeps_head_version_and_marks_resolved() {
    let (t, repo, _) = conflicted();
    repo.resolve_conflict("f", ConflictSide::Ours).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "main change\n"
    );
    // No longer unmerged.
    assert!(t.git(["diff", "--name-only", "--diff-filter=U"]).is_empty());
}

#[test]
fn take_theirs_keeps_incoming_version() {
    let (t, repo, _) = conflicted();
    repo.resolve_conflict("f", ConflictSide::Theirs).unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "other change\n"
    );
    assert!(t.git(["diff", "--name-only", "--diff-filter=U"]).is_empty());
}

// --- The marker parser/resolver (the resolve view's engine) ----------------

/// A real merge conflict in `f` (ours = "our change", theirs = "their change",
/// base = "base"), via a failed `git merge other`.
fn merge_conflicted(conflict_style: Option<&str>) -> (TestRepo, Repo) {
    let t = TestRepo::new();
    t.write("f", "start\nbase\nend\n");
    t.commit_all("init");
    if let Some(style) = conflict_style {
        t.git(["config", "merge.conflictStyle", style]);
    }
    t.git(["checkout", "-b", "other"]);
    t.write("f", "start\ntheir change\nend\n");
    t.commit_all("other");
    t.git(["checkout", "main"]);
    t.write("f", "start\nour change\nend\n");
    t.commit_all("main");
    assert!(!git_try(&t, &["merge", "other"]), "expected a conflict");
    let repo = Repo::discover(t.path()).unwrap();
    (t, repo)
}

fn only_conflict(segments: &[Segment]) -> &magritte_core::conflict::Conflict {
    let conflicts: Vec<_> = segments
        .iter()
        .filter_map(|s| match s {
            Segment::Conflict(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(conflicts.len(), 1, "expected one conflict: {segments:?}");
    conflicts[0]
}

#[test]
fn parse_merge_conflict_and_round_trip_unresolved() {
    let (_t, repo) = merge_conflicted(None);
    let original = repo.read_worktree_file("f").unwrap();
    let segments = parse_conflicts(&original);
    let c = only_conflict(&segments);
    assert_eq!(c.ours_label, "HEAD");
    assert_eq!(c.theirs_label, "other");
    assert_eq!(c.ours, b"our change\n");
    assert_eq!(c.theirs, b"their change\n");
    assert_eq!(c.base, None);
    // With no choices the reassembled file is byte-identical.
    assert_eq!(resolve(&segments, &[]), original);
    assert_eq!(resolve(&segments, &[None]), original);
}

#[test]
fn resolve_ours_theirs_and_both() {
    let (_t, repo) = merge_conflicted(None);
    let original = repo.read_worktree_file("f").unwrap();
    let segments = parse_conflicts(&original);
    assert_eq!(
        resolve(&segments, &[Some(Resolution::Ours)]),
        b"start\nour change\nend\n"
    );
    assert_eq!(
        resolve(&segments, &[Some(Resolution::Theirs)]),
        b"start\ntheir change\nend\n"
    );
    // Both keeps ours then theirs (smerge-keep-all's order).
    assert_eq!(
        resolve(&segments, &[Some(Resolution::Both)]),
        b"start\nour change\ntheir change\nend\n"
    );
}

#[test]
fn write_resolved_and_stage_marks_the_path_resolved() {
    let (t, repo) = merge_conflicted(None);
    let original = repo.read_worktree_file("f").unwrap();
    let segments = parse_conflicts(&original);
    let resolved = resolve(&segments, &[Some(Resolution::Theirs)]);
    repo.write_worktree_file("f", &resolved).unwrap();
    repo.stage_file("f").unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("f")).unwrap(),
        "start\ntheir change\nend\n"
    );
    assert!(t.git(["diff", "--name-only", "--diff-filter=U"]).is_empty());
}

#[test]
fn diff3_markers_parse_the_base() {
    let (_t, repo) = merge_conflicted(Some("diff3"));
    let original = repo.read_worktree_file("f").unwrap();
    let segments = parse_conflicts(&original);
    let c = only_conflict(&segments);
    assert_eq!(c.base.as_deref(), Some(b"base\n".as_slice()));
    assert!(c.base_label.is_some());
    // Unresolved still round-trips byte-identically with the extra marker.
    assert_eq!(resolve(&segments, &[None]), original);
    assert_eq!(
        resolve(&segments, &[Some(Resolution::Base)]),
        b"start\nbase\nend\n"
    );
}

#[test]
fn crlf_content_survives_byte_exactly() {
    let content: &[u8] =
        b"top\r\n<<<<<<< HEAD\r\nours line\r\n=======\r\ntheirs line\r\n>>>>>>> other\r\nbottom\r\n";
    let segments = parse_conflicts(content);
    let c = only_conflict(&segments);
    // Labels are the marker text without the line ending.
    assert_eq!(c.ours_label, "HEAD");
    assert_eq!(c.theirs_label, "other");
    assert_eq!(c.ours, b"ours line\r\n");
    assert_eq!(resolve(&segments, &[None]), content);
    assert_eq!(
        resolve(&segments, &[Some(Resolution::Both)]),
        b"top\r\nours line\r\ntheirs line\r\nbottom\r\n"
    );
}

#[test]
fn stray_separator_without_markers_is_plain_text() {
    let content: &[u8] = b"a\n=======\nb\n>>>>>>> huh\n";
    let segments = parse_conflicts(content);
    assert_eq!(segments, vec![Segment::Text(content.to_vec())]);
}

#[test]
fn malformed_and_nested_markers_fall_back_to_text() {
    // An opening marker that never closes parses as text.
    let unclosed: &[u8] = b"<<<<<<< HEAD\nours\n=======\ntheirs\n";
    assert_eq!(
        parse_conflicts(unclosed),
        vec![Segment::Text(unclosed.to_vec())]
    );
    // A nested start abandons the first run as text; the second, complete
    // conflict still parses — and the whole file round-trips.
    let nested: &[u8] = b"<<<<<<< A\nx\n<<<<<<< B\nb\n=======\nt\n>>>>>>> C\n";
    let segments = parse_conflicts(nested);
    assert_eq!(resolve(&segments, &[]), nested);
    let c = only_conflict(&segments);
    assert_eq!(c.ours_label, "B");
    assert_eq!(segments[0], Segment::Text(b"<<<<<<< A\nx\n".to_vec()));
    // Marker runs longer than seven characters aren't markers.
    let long: &[u8] = b"<<<<<<<< A\nx\n========\ny\n>>>>>>>> B\n";
    assert_eq!(parse_conflicts(long), vec![Segment::Text(long.to_vec())]);
}

//! Standalone smoke tests for the unified-diff parser, on literal git-format
//! output — so the foundation crate's parser is testable without git (the
//! deep corpus lives in magritte-core's integration tests, which exercise it
//! through real `git diff` output).

use magritte_vcs::diff::{parse_diff, LineKind};

#[test]
fn modify_with_missing_final_newline() {
    let text = "diff --git a/f.txt b/f.txt\n\
                index 0123456..89abcde 100644\n\
                --- a/f.txt\n\
                +++ b/f.txt\n\
                @@ -1,2 +1,2 @@\n \
                one\n\
                -two\n\
                +TWO\n\
                \\ No newline at end of file\n";
    let files = parse_diff(text.as_bytes()).unwrap();
    assert_eq!(files.len(), 1);
    let f = &files[0];
    assert_eq!(
        (f.old_path.as_str(), f.new_path.as_str()),
        ("f.txt", "f.txt")
    );
    assert!(!f.is_new && !f.is_deleted && !f.is_binary);
    assert_eq!(f.hunks.len(), 1);
    let kinds: Vec<LineKind> = f.hunks[0].lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [
            LineKind::Context,
            LineKind::Removed,
            LineKind::Added,
            LineKind::NoNewline
        ]
    );
    assert_eq!(f.hunks[0].lines[2].new_lineno, Some(2));
}

#[test]
fn pure_rename_has_no_hunks() {
    let text = "diff --git a/old.txt b/new.txt\n\
                similarity index 100%\n\
                rename from old.txt\n\
                rename to new.txt\n";
    let files = parse_diff(text.as_bytes()).unwrap();
    assert_eq!(files[0].old_path, "old.txt");
    assert_eq!(files[0].new_path, "new.txt");
    assert!(files[0].hunks.is_empty());
}

#[test]
fn binary_and_new_file_flags() {
    let text = "diff --git a/img.png b/img.png\n\
                new file mode 100644\n\
                index 0000000..1234567\n\
                Binary files /dev/null and b/img.png differ\n";
    let files = parse_diff(text.as_bytes()).unwrap();
    assert!(files[0].is_new);
    assert!(files[0].is_binary);
}

#[test]
fn c_quoted_paths_unescape() {
    let text = "diff --git \"a/we\\tird.txt\" \"b/we\\tird.txt\"\n\
                index 0123456..89abcde 100644\n\
                --- \"a/we\\tird.txt\"\n\
                +++ \"b/we\\tird.txt\"\n\
                @@ -1 +1 @@\n\
                -a\n\
                +b\n";
    let files = parse_diff(text.as_bytes()).unwrap();
    assert_eq!(files[0].new_path, "we\tird.txt");
}

#[test]
fn crlf_content_survives_byte_exact() {
    let text = b"diff --git a/f.txt b/f.txt\n\
                index 0123456..89abcde 100644\n\
                --- a/f.txt\n\
                +++ b/f.txt\n\
                @@ -1 +1 @@\n\
                -old\r\n\
                +new\r\n";
    let files = parse_diff(text).unwrap();
    let lines = &files[0].hunks[0].lines;
    // The trailing \r is content, not line-ending noise.
    assert_eq!(lines[0].content, "old\r");
    assert_eq!(lines[1].content, "new\r");
}

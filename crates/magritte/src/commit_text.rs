//! Commit-message text shaping: the 50/72 ruler and body wrap/reflow used by the
//! in-app commit editor. Pure string functions (plus a cursor `Position` helper)
//! with no UI state, so they unit-test directly.

use gpui_component::input::Position;

/// Break a single line into pieces no longer than `width` characters, splitting
/// at the last space at or before the limit. A word longer than `width` (no
/// usable space) is left intact on its own piece rather than chopped.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = line;
    loop {
        if rest.chars().count() <= width {
            pieces.push(rest.to_string());
            break;
        }
        // Last space whose preceding text fits in `width` columns.
        let split = rest
            .char_indices()
            .enumerate()
            .take_while(|&(ci, _)| ci <= width)
            .filter(|&(ci, (_, ch))| ch == ' ' && ci > 0)
            .last()
            .map(|(_, (bi, _))| bi);
        match split {
            Some(bi) => {
                pieces.push(rest[..bi].to_string());
                rest = &rest[bi + 1..]; // drop the space we broke on
            }
            None => {
                pieces.push(rest.to_string()); // unbreakable long word
                break;
            }
        }
    }
    pieces
}

/// Auto-wrap the commit body *only when the cursor is at the end of an
/// over-long line* — i.e. while typing at the end of a line — so that editing
/// in the middle of the message never reflows text under the user. The summary
/// (line 0) is never wrapped. Returns the rewrapped text when a wrap happened.
/// `cursor` is a byte offset (as the input reports it); because wrapping only
/// turns a space into a newline, that offset stays valid in the result.
pub(crate) fn wrap_at_cursor(text: &str, cursor: usize, width: usize) -> Option<String> {
    let mut line_start = 0; // byte offset of the current line's first char
    for (i, line) in text.split('\n').enumerate() {
        let line_end = line_start + line.len(); // byte offset before the '\n'
        if cursor <= line_end {
            // The cursor is on this line. Wrap only when it's at the very end of
            // the line, the line isn't the summary, and it overruns the width.
            if cursor != line_end || i == 0 || line.chars().count() <= width {
                return None;
            }
            let pieces = wrap_line(line, width);
            if pieces.len() <= 1 {
                return None; // unbreakable (e.g. a single long word)
            }
            let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            lines.splice(i..=i, pieces);
            return Some(lines.join("\n"));
        }
        line_start = line_end + 1; // + the '\n' byte
    }
    None
}

/// Reflow the commit *body* to `width`: each blank-line-separated paragraph is
/// joined into one line then re-wrapped, collapsing runs of whitespace. The
/// summary (line 0) and blank separator lines are left untouched. Unlike
/// [`wrap_at_cursor`], this *re-joins* manually-broken lines, so it's an
/// explicit action rather than something to run while typing.
pub(crate) fn reflow_body(text: &str, width: usize) -> String {
    match text.split_once('\n') {
        None => text.to_string(),
        Some((summary, rest)) => format!("{summary}\n{}", reflow_lines(rest, width)),
    }
}

/// Reflow a block of body lines (see [`reflow_body`], which handles skipping
/// the summary): paragraphs re-wrap at `width`, blank separator lines stay.
/// Structure is respected: an indented line is preformatted (kept verbatim —
/// the git convention for code blocks and quoted output), and a bullet
/// (`- * + •` or `1.`/`1)`) starts its own paragraph that re-wraps with a
/// hanging indent, its continuation lines joined bullet-style.
pub(crate) fn reflow_lines(block: &str, width: usize) -> String {
    // The open paragraph: the first line's prefix (a bullet marker or
    // nothing), the hanging prefix for wrapped continuations, and its lines.
    struct Para<'a> {
        first: String,
        cont: String,
        lines: Vec<&'a str>,
    }
    let mut out: Vec<String> = Vec::new();
    let mut para: Option<Para> = None;
    fn flush(out: &mut Vec<String>, para: &mut Option<Para>, width: usize) {
        let Some(p) = para.take() else { return };
        let collapsed = p.lines.join(" ");
        let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
        let inner = width.saturating_sub(p.first.chars().count()).max(1);
        for (i, piece) in wrap_line(&collapsed, inner).into_iter().enumerate() {
            let prefix = if i == 0 { &p.first } else { &p.cont };
            out.push(format!("{prefix}{piece}"));
        }
    }
    for line in block.split('\n') {
        if line.trim().is_empty() {
            flush(&mut out, &mut para, width);
            out.push(String::new());
        } else if let Some(marker) = bullet_marker(line) {
            flush(&mut out, &mut para, width);
            para = Some(Para {
                cont: " ".repeat(marker.chars().count()),
                lines: vec![&line[marker.len()..]],
                first: marker.to_string(),
            });
        } else if line.starts_with([' ', '\t']) {
            // Indented: a bullet's continuation joins it; anything else is
            // purposeful indentation, kept as-is.
            match &mut para {
                Some(p) if !p.cont.is_empty() => p.lines.push(line),
                _ => {
                    flush(&mut out, &mut para, width);
                    out.push(line.to_string());
                }
            }
        } else {
            match &mut para {
                Some(p) => p.lines.push(line),
                None => {
                    para = Some(Para {
                        first: String::new(),
                        cont: String::new(),
                        lines: vec![line],
                    });
                }
            }
        }
    }
    flush(&mut out, &mut para, width);
    out.join("\n")
}

/// The bullet marker (including its trailing space) opening a list item:
/// `- `, `* `, `+ `, `• `, or a number with `. `/`) `.
fn bullet_marker(line: &str) -> Option<&str> {
    for b in ['-', '*', '+', '•'] {
        if let Some(rest) = line.strip_prefix(b) {
            if rest.starts_with(' ') {
                return Some(&line[..b.len_utf8() + 1]);
            }
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && (line[digits..].starts_with(". ") || line[digits..].starts_with(") ")) {
        return Some(&line[..digits + 2]);
    }
    None
}

/// The character-column range of the part of the summary (line 0) that overruns
/// `limit` columns, as `(start, end)` for a diagnostic `Position` (whose
/// `character` field is a 0-based character count). `None` when the summary
/// fits.
pub(crate) fn title_overflow(text: &str, limit: usize) -> Option<(u32, u32)> {
    let title = text.split('\n').next().unwrap_or("");
    let chars = title.chars().count();
    if chars <= limit {
        return None;
    }
    Some((limit as u32, chars as u32))
}

/// The minimal single-range difference between `old` and `new`: the byte
/// range in `old` to replace and the byte range in `new` holding the
/// replacement (longest common prefix and suffix trimmed, on char
/// boundaries). Equal strings yield two empty ranges.
pub(crate) fn diff_splice(
    old: &str,
    new: &str,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let mut p = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !(old.is_char_boundary(p) && new.is_char_boundary(p)) {
        p -= 1;
    }
    let mut q = old[p..]
        .bytes()
        .rev()
        .zip(new[p..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !(old.is_char_boundary(old.len() - q) && new.is_char_boundary(new.len() - q)) {
        q -= 1;
    }
    (p..old.len() - q, p..new.len() - q)
}

/// UTF-8 byte range → UTF-16 code-unit range: `replace_text_in_range` is the
/// one input API that speaks UTF-16.
pub(crate) fn byte_range_to_utf16(
    text: &str,
    range: &std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    let start: usize = text[..range.start].chars().map(char::len_utf16).sum();
    let len: usize = text[range.start..range.end]
        .chars()
        .map(char::len_utf16)
        .sum();
    start..start + len
}

/// Convert a byte offset into `text` (as the input reports the cursor) to a
/// 0-based line / character-column [`Position`], for restoring the cursor after
/// a programmatic edit.
pub(crate) fn byte_offset_to_position(text: &str, offset: usize) -> Position {
    let (mut line, mut col, mut bytes) = (0u32, 0u32, 0usize);
    for ch in text.chars() {
        if bytes >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1; // character column
        }
        bytes += ch.len_utf8();
    }
    Position::new(line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_overflow_flags_only_past_the_limit() {
        // Within the limit: no overflow.
        assert_eq!(title_overflow("a short summary", 50), None);
        // Exactly at the limit: still fine.
        let fifty = "x".repeat(50);
        assert_eq!(title_overflow(&fifty, 50), None);
        // One over: range covers just the overflow (col 50..51).
        let fifty_one = "x".repeat(51);
        assert_eq!(title_overflow(&fifty_one, 50), Some((50, 51)));
        // Only the first line (summary) counts; a long body doesn't trigger it.
        assert_eq!(title_overflow("ok\n\nbody line", 50), None);
    }

    #[test]
    fn wrap_at_cursor_only_wraps_at_end_of_an_overlong_body_line() {
        // A wrappable body line (~114 chars of short words) with the cursor at
        // its end.
        let body = "alpha beta gamma delta ".repeat(5);
        let body = body.trim_end();
        let text = format!("summary\n\n{body}");
        let cursor = text.len(); // at the very end
        let wrapped = wrap_at_cursor(&text, cursor, 72).expect("should wrap");
        let body_lines: Vec<&str> = wrapped.lines().skip(2).collect();
        assert!(body_lines.len() > 1, "long body line should wrap");
        assert!(body_lines.iter().all(|l| l.chars().count() <= 72));
        // Only a space turned into a newline: total byte length is unchanged.
        assert_eq!(wrapped.len(), text.len());
    }

    #[test]
    fn wrap_at_cursor_ignores_mid_line_edits_and_the_summary() {
        let body = "alpha beta gamma delta ".repeat(5);
        let text = format!("summary\n\n{}", body.trim_end());
        // Cursor in the middle of the long body line: no wrap (don't reflow
        // under the user as they edit earlier in the line).
        let mid = "summary\n\n".len() + 10;
        assert_eq!(wrap_at_cursor(&text, mid, 72), None);
        // An over-long *summary* with the cursor at its end is never wrapped.
        let long_summary = "x".repeat(90);
        assert_eq!(wrap_at_cursor(&long_summary, long_summary.len(), 72), None);
    }

    #[test]
    fn wrap_at_cursor_leaves_unbreakable_long_words() {
        let word = "x".repeat(100);
        let text = format!("summary\n\n{word}");
        assert_eq!(wrap_at_cursor(&text, text.len(), 72), None);
    }

    #[test]
    fn reflow_rejoins_then_rewraps_paragraphs() {
        // Two short manually-broken lines in one paragraph rejoin and re-wrap.
        let text = "summary\n\nthese are\nseveral short\nlines";
        let reflowed = reflow_body(text, 72);
        assert_eq!(reflowed, "summary\n\nthese are several short lines");

        // A blank line separates paragraphs, which stay separate.
        let text = "summary\n\npara one here\n\npara two here";
        let reflowed = reflow_body(text, 72);
        assert_eq!(reflowed, "summary\n\npara one here\n\npara two here");
    }

    #[test]
    fn reflow_preserves_structure() {
        // Indented lines are preformatted: kept verbatim, never joined.
        let text = "s\n\nintro text\n    code line one\n    code line two";
        assert_eq!(reflow_body(text, 72), text);

        // Bullets are their own paragraphs — consecutive items never merge —
        // and re-wrap with a hanging indent, joining their continuations.
        let text = "s\n\n- first item\n- second item that is a bit longer\nand continues here";
        assert_eq!(
            reflow_body(text, 24),
            "s\n\n- first item\n- second item that is a\n  bit longer and\n  continues here"
        );
        // An indented continuation joins its bullet.
        let text = "s\n\n- item text\n  indented continuation";
        assert_eq!(
            reflow_body(text, 72),
            "s\n\n- item text indented continuation"
        );
        // Numbered lists too.
        let text = "s\n\n1. one\n2) two";
        assert_eq!(reflow_body(text, 72), text);
    }

    #[test]
    fn diff_splice_minimal_ranges() {
        assert_eq!(diff_splice("abc", "abc"), (3..3, 3..3));
        assert_eq!(diff_splice("abc", "aXc"), (1..2, 1..2));
        assert_eq!(diff_splice("ab", "aXb"), (1..1, 1..2));
        assert_eq!(diff_splice("aXb", "ab"), (1..2, 1..1));
        assert_eq!(diff_splice("aa", "aaa"), (2..2, 2..3));
        // Multibyte boundaries: é (2 bytes) vs e.
        let (o, n) = diff_splice("xéy", "xey");
        assert!("xéy".is_char_boundary(o.start) && "xéy".is_char_boundary(o.end));
        assert_eq!(&"xey"[n.clone()], "e");
        assert_eq!(&"xéy"[o], "é");
    }

    #[test]
    fn byte_offset_to_position_tracks_lines() {
        assert_eq!(byte_offset_to_position("abc", 2), Position::new(0, 2));
        // Offset just past the first newline -> start of line 1.
        assert_eq!(byte_offset_to_position("ab\ncd", 3), Position::new(1, 0));
        assert_eq!(byte_offset_to_position("ab\ncd", 5), Position::new(1, 2));
        // Multi-byte char: column counts characters, offset counts bytes.
        assert_eq!(byte_offset_to_position("é x", 3), Position::new(0, 2));
    }
}

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
pub(crate) fn reflow_lines(block: &str, width: usize) -> String {
    let body: Vec<&str> = block.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if body[i].trim().is_empty() {
            out.push(String::new());
            i += 1;
        } else {
            let start = i;
            while i < body.len() && !body[i].trim().is_empty() {
                i += 1;
            }
            let collapsed = body[start..i].join(" ");
            let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
            out.extend(wrap_line(&collapsed, width));
        }
    }
    out.join("\n")
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
    fn byte_offset_to_position_tracks_lines() {
        assert_eq!(byte_offset_to_position("abc", 2), Position::new(0, 2));
        // Offset just past the first newline -> start of line 1.
        assert_eq!(byte_offset_to_position("ab\ncd", 3), Position::new(1, 0));
        assert_eq!(byte_offset_to_position("ab\ncd", 5), Position::new(1, 2));
        // Multi-byte char: column counts characters, offset counts bytes.
        assert_eq!(byte_offset_to_position("é x", 3), Position::new(0, 2));
    }
}

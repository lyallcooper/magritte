//! Modal Vim editing for the in-app commit editor: a pure command engine
//! (modes, motions, text objects, operators, surround) that maps keystrokes to
//! [`Action`]s over a plain string buffer. This module tree knows nothing about
//! gpui or `InputState`; the app layer (`apply.rs`) routes keys in and applies
//! the returned actions. See `docs/dev/vim-mode.md` for the design.
//!
//! Every offset in this module is a **UTF-8 byte offset at a char boundary**.
//! The helpers at the bottom of this file are the shared vocabulary for moving
//! between offsets, lines, and columns; use them instead of raw arithmetic so
//! multi-byte text can never be split.

pub(crate) mod apply;
mod engine;
mod motion;
mod surround;
#[cfg(test)]
mod tests;
mod text_object;

pub(crate) use engine::VimState;

use std::ops::Range;

/// The current editing mode. `Visual { linewise: false }` is `v`,
/// `{ linewise: true }` is `V`. Operator-pending and other mid-sequence states
/// live inside [`VimState`], not here — the app only needs these three to
/// route keys and render the indicator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Normal,
    Insert,
    Visual { linewise: bool },
}

/// A keystroke, as the engine sees it. The app layer converts gpui keystrokes
/// (via `key_char`, so shifted symbols arrive as the produced character).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    Ctrl(char),
}

/// What a keystroke does to the buffer. The engine may return several (e.g. a
/// delete yanks into the system clipboard *and* edits). Mode changes are not
/// actions — the app reads [`VimState::mode`] after each `handle_key`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    /// Move the cursor to this byte offset.
    MoveCursor(usize),
    /// Replace a range of the buffer.
    Edit(EditOp),
    /// Mirror yanked/deleted text to the system clipboard (the engine's
    /// unnamed register is updated internally).
    Yank(String),
    Undo,
    Redo,
    /// `.`: replay the last change. The app fetches it with
    /// [`VimState::begin_repeat`], feeds the recorded keys back through
    /// `handle_key`, re-inserts the captured Insert-mode text, and closes
    /// with [`VimState::end_repeat`].
    Repeat,
    /// `ZZ`: submit the commit message.
    Commit,
    /// `ZQ`: cancel the editor (the app's discard-confirm flow applies).
    Quit,
    /// `gq`: reflow the message body.
    Reflow,
    /// Unhandled or invalid input — swallow the key, optionally flash.
    Beep,
}

/// One buffer edit: replace `range` with `text`, then put the cursor at
/// `cursor` (a byte offset into the *post-edit* buffer).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct EditOp {
    pub(crate) range: Range<usize>,
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

/// How a motion combines with an operator (`:help motion.txt`): exclusive
/// omits the end position's character, inclusive includes it, linewise expands
/// to whole lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// Where a motion landed and how operators should treat it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MotionTarget {
    /// Byte offset at a char boundary.
    pub(crate) pos: usize,
    pub(crate) kind: MotionKind,
}

/// The motions the engine can ask [`motion::eval`] to compute. Counts are
/// passed separately; `GotoLine(None)` is `G` without a count (last line).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Motion {
    /// `h` — exclusive; stops at the start of the line.
    Left,
    /// `l` — exclusive; stops at the end of the line (see [`clamp_normal`]).
    Right,
    /// `j` — linewise. Uses the desired column (chars; `usize::MAX` = line end).
    Down,
    /// `k` — linewise.
    Up,
    /// `0` — exclusive.
    LineStart,
    /// `^` — exclusive; first non-blank of the line.
    FirstNonBlank,
    /// `$` — inclusive; with a count, end of count-1 lines below.
    LineEnd,
    /// `w`/`W` — exclusive.
    WordForward { big: bool },
    /// `b`/`B` — exclusive.
    WordBack { big: bool },
    /// `e`/`E` — inclusive.
    WordEnd { big: bool },
    /// `gg`/`G`/`{count}G` — linewise; `Some(n)` is a 1-based line number,
    /// `None` is the last line. The landing column is the first non-blank.
    GotoLine(Option<usize>),
    /// `f`/`t`/`F`/`T` (and `;`/`,` replays) — `f`/`t` inclusive, `F`/`T`
    /// exclusive. Fails (None) if the char isn't found on the cursor's line.
    /// `repeat` is set for `;`/`,`: a till motion then skips a target the
    /// cursor is already adjacent to (`:help ;`).
    Find {
        kind: FindKind,
        target: char,
        repeat: bool,
    },
    /// `}` — exclusive.
    ParagraphForward,
    /// `{` — exclusive.
    ParagraphBack,
    /// `%` — inclusive; jump to the match of the nearest bracket at or after
    /// the cursor on this line. Fails if there is none or it is unbalanced.
    MatchPair,
    /// `Enter`/`+` — linewise; count lines down, to the first non-blank.
    NextLineStart,
    /// `-` — linewise; count lines up, to the first non-blank.
    PrevLineStart,
    /// `Space` — exclusive; right, crossing line ends (each end-of-line counts
    /// as one position, like Vim's `<Space>` with default `whichwrap`).
    SpaceRight,
    /// `Backspace` — exclusive; left, crossing line ends.
    BackspaceLeft,
}

/// The four find flavors: `f` / `F` / `t` / `T`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FindKind {
    FindFwd,
    FindBack,
    TillFwd,
    TillBack,
}

impl FindKind {
    /// The flavor `;` repeats (same) — `,` uses [`FindKind::reversed`].
    pub(super) fn reversed(self) -> FindKind {
        match self {
            FindKind::FindFwd => FindKind::FindBack,
            FindKind::FindBack => FindKind::FindFwd,
            FindKind::TillFwd => FindKind::TillBack,
            FindKind::TillBack => FindKind::TillFwd,
        }
    }
}

// --- Shared text helpers -------------------------------------------------
//
// All take/return byte offsets at char boundaries and tolerate any in-bounds
// boundary offset (including one sitting on a `\n` or at `text.len()`).

/// Class of a character for word motions. With `big` (W/B/E), everything
/// non-blank is one class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CharClass {
    Blank,
    Word,
    Punct,
}

pub(super) fn char_class(c: char, big: bool) -> CharClass {
    if c == ' ' || c == '\t' || c == '\n' {
        CharClass::Blank
    } else if big || c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Byte offset of the start of the line containing `pos`.
pub(super) fn line_start(text: &str, pos: usize) -> usize {
    text[..pos.min(text.len())].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset of the end of the line containing `pos`: its `\n`, or
/// `text.len()` for the last line.
pub(super) fn line_end(text: &str, pos: usize) -> usize {
    let pos = pos.min(text.len());
    text[pos..].find('\n').map_or(text.len(), |i| pos + i)
}

/// Offset just past the char starting at `pos` (or `len` if at the end).
pub(super) fn next_char(text: &str, pos: usize) -> usize {
    match text[pos.min(text.len())..].chars().next() {
        Some(c) => pos + c.len_utf8(),
        None => text.len(),
    }
}

/// Start of the char before `pos` (or 0 if at the start).
pub(super) fn prev_char(text: &str, pos: usize) -> usize {
    text[..pos.min(text.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// The char starting at `pos`, if any.
pub(super) fn char_at(text: &str, pos: usize) -> Option<char> {
    text[pos.min(text.len())..].chars().next()
}

/// First non-blank (not space/tab) offset of the line containing `pos`; a
/// blank line yields its last char's start; an empty line its start.
pub(super) fn first_non_blank(text: &str, pos: usize) -> usize {
    let start = line_start(text, pos);
    let end = line_end(text, pos);
    for (i, c) in text[start..end].char_indices() {
        if c != ' ' && c != '\t' {
            return start + i;
        }
    }
    if start == end {
        start
    } else {
        // All-blank line: the last char of the line, like Vim's `^`.
        prev_char(text, end).max(start)
    }
}

/// Column (in chars) of `pos` within its line.
pub(super) fn char_col(text: &str, pos: usize) -> usize {
    let start = line_start(text, pos);
    text[start..pos.min(text.len())].chars().count()
}

/// Offset of the char at column `col` (in chars) on the line containing
/// `line_pos`, clamped to the line's last char for Normal mode (`usize::MAX`
/// means "line end"). An empty line yields its start.
pub(super) fn offset_at_col(text: &str, line_pos: usize, col: usize) -> usize {
    let start = line_start(text, line_pos);
    let end = line_end(text, line_pos);
    if start == end {
        return start;
    }
    let mut at = start;
    for (c, (i, _)) in text[start..end].char_indices().enumerate() {
        if c == col {
            return start + i;
        }
        at = start + i;
    }
    at // col past the line: its last char
}

/// Clamp `pos` to a valid Normal-mode cursor position: on a char that isn't a
/// line's `\n`, or the start of an empty line (including the empty last line
/// after a trailing `\n`, and 0 for an empty buffer).
pub(super) fn clamp_normal(text: &str, pos: usize) -> usize {
    let mut pos = pos.min(text.len());
    // Align to a char boundary (floor).
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    if pos == text.len() {
        // EOF is valid only when the buffer is empty or ends in an empty line.
        if text.is_empty() || text.ends_with('\n') {
            return pos;
        }
        return prev_char(text, pos);
    }
    if char_at(text, pos) == Some('\n') && pos > line_start(text, pos) {
        return prev_char(text, pos);
    }
    pos
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn line_bounds() {
        let t = "ab\ncd\n";
        assert_eq!(line_start(t, 0), 0);
        assert_eq!(line_start(t, 2), 0); // on the \n
        assert_eq!(line_start(t, 4), 3);
        assert_eq!(line_start(t, 6), 6); // empty last line
        assert_eq!(line_end(t, 0), 2);
        assert_eq!(line_end(t, 2), 2);
        assert_eq!(line_end(t, 3), 5);
        assert_eq!(line_end(t, 6), 6);
    }

    #[test]
    fn char_steps_multibyte() {
        let t = "é✓x";
        assert_eq!(next_char(t, 0), 2);
        assert_eq!(next_char(t, 2), 5);
        assert_eq!(prev_char(t, 5), 2);
        assert_eq!(prev_char(t, 2), 0);
        assert_eq!(prev_char(t, 0), 0);
        assert_eq!(next_char(t, 6), 6);
    }

    #[test]
    fn first_non_blank_cases() {
        assert_eq!(first_non_blank("  ab", 0), 2);
        assert_eq!(first_non_blank("ab", 1), 0);
        assert_eq!(first_non_blank("   ", 0), 2); // all blank: last char
        assert_eq!(first_non_blank("", 0), 0);
        assert_eq!(first_non_blank("x\n  y", 3), 4);
        assert_eq!(first_non_blank("x\n\nz", 2), 2); // empty line
    }

    #[test]
    fn columns() {
        let t = "aé✓b\ncd";
        assert_eq!(char_col(t, 0), 0);
        assert_eq!(char_col(t, 3), 2); // after a + é
        assert_eq!(char_col(t, 9), 1); // 'd'
        assert_eq!(offset_at_col(t, 0, 2), 3);
        assert_eq!(offset_at_col(t, 0, usize::MAX), 6); // 'b'
        assert_eq!(offset_at_col(t, 8, 9), 9); // clamped to 'd'
        assert_eq!(offset_at_col("a\n\nb", 2, 5), 2); // empty line
    }

    #[test]
    fn normal_clamp() {
        assert_eq!(clamp_normal("abc", 3), 2); // EOF -> last char
        assert_eq!(clamp_normal("abc", 1), 1);
        assert_eq!(clamp_normal("ab\ncd", 2), 1); // on \n -> last char
        assert_eq!(clamp_normal("ab\n\ncd", 3), 3); // empty line: on its \n ok
        assert_eq!(clamp_normal("ab\n", 3), 3); // empty last line
        assert_eq!(clamp_normal("", 0), 0);
        assert_eq!(clamp_normal("é", 1), 0); // mid-char floors
        assert_eq!(clamp_normal("é", 2), 0);
    }
}

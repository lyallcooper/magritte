//! Motion evaluation: where a [`Motion`] lands from a given cursor position.
//! Pure functions over `&str` + byte offsets; no engine state. The engine
//! applies operator range rules — this module only finds the landing position
//! and tags it with the motion's [`MotionKind`].

use super::*;

/// Evaluate `motion` from `cursor` (a char-boundary byte offset), `count`
/// times (`count >= 1`). `desired_col` is the sticky column (in chars,
/// `usize::MAX` = line end) that `Down`/`Up` aim for; other motions ignore it.
///
/// Returns `None` when the motion fails entirely (Vim beeps): `h` at column 0,
/// `j` on the last line, an `f` whose target isn't on the line, a `%` with no
/// bracket, `{`/`}` with nowhere to go... A motion that can only *partially*
/// satisfy its count still succeeds with how far it got (e.g. `10l` near the
/// line end stops at the last char; `100G` past the end lands on the last
/// line), matching Vim.
///
/// The returned position is the raw landing offset — for exclusive motions
/// like `w` this may be a line's first column or `text.len()`; the engine
/// clamps for plain cursor motion and applies `:help exclusive-linewise`
/// adjustments for operators. It must always sit on a char boundary.
pub(super) fn eval(
    text: &str,
    cursor: usize,
    count: usize,
    motion: Motion,
    desired_col: usize,
) -> Option<MotionTarget> {
    let cursor = cursor.min(text.len());
    match motion {
        Motion::Left => {
            let start = line_start(text, cursor);
            if cursor <= start {
                return None;
            }
            let mut p = cursor;
            for _ in 0..count {
                if p <= start {
                    break;
                }
                p = prev_char(text, p);
            }
            some(p, MotionKind::Exclusive)
        }
        Motion::Right => {
            // May land on the line's `\n` (raw): `dl` at the last char needs
            // it; the engine clamps plain moves.
            let end = line_end(text, cursor);
            if cursor >= end {
                return None;
            }
            let mut p = cursor;
            for _ in 0..count {
                if p >= end {
                    break;
                }
                p = next_char(text, p);
            }
            some(p, MotionKind::Exclusive)
        }
        Motion::Down => {
            // `j`/`k` fail outright when the count overshoots (`:help j`).
            let mut p = cursor;
            for _ in 0..count {
                let le = line_end(text, p);
                if le >= text.len() {
                    return None;
                }
                p = le + 1;
            }
            some(offset_at_col(text, p, desired_col), MotionKind::Linewise)
        }
        Motion::Up => {
            let mut p = line_start(text, cursor);
            for _ in 0..count {
                if p == 0 {
                    return None;
                }
                p = line_start(text, p - 1);
            }
            some(offset_at_col(text, p, desired_col), MotionKind::Linewise)
        }
        Motion::LineStart => some(line_start(text, cursor), MotionKind::Exclusive),
        Motion::FirstNonBlank => some(first_non_blank(text, cursor), MotionKind::Exclusive),
        Motion::LineEnd => {
            let mut p = cursor;
            for _ in 1..count {
                let le = line_end(text, p);
                if le >= text.len() {
                    break;
                }
                p = le + 1;
            }
            let start = line_start(text, p);
            let end = line_end(text, p);
            let pos = if start == end {
                start
            } else {
                prev_char(text, end)
            };
            some(pos, MotionKind::Inclusive)
        }
        Motion::WordForward { big } => steps(cursor, count, MotionKind::Exclusive, |p| {
            word_forward(text, p, big)
        }),
        Motion::WordBack { big } => steps(cursor, count, MotionKind::Exclusive, |p| {
            word_back(text, p, big)
        }),
        Motion::WordEnd { big } => steps(cursor, count, MotionKind::Inclusive, |p| {
            word_end(text, p, big)
        }),
        Motion::GotoLine(n) => {
            let mut p = 0;
            for _ in 1..n.unwrap_or(usize::MAX) {
                let le = line_end(text, p);
                if le >= text.len() {
                    break; // clamp to the last line
                }
                p = le + 1;
            }
            some(first_non_blank(text, p), MotionKind::Linewise)
        }
        Motion::Find {
            kind,
            target,
            repeat,
        } => find(text, cursor, count, kind, target, repeat),
        Motion::ParagraphForward => paragraph(text, cursor, count, true),
        Motion::ParagraphBack => paragraph(text, cursor, count, false),
        Motion::MatchPair => {
            let end = line_end(text, cursor);
            let (bpos, bch) = text[cursor.min(end)..end]
                .char_indices()
                .map(|(i, c)| (cursor + i, c))
                .find(|&(_, c)| "()[]{}".contains(c))?;
            some(match_bracket(text, bpos, bch)?, MotionKind::Inclusive)
        }
        Motion::NextLineStart => {
            let mut p = cursor;
            for _ in 0..count {
                let le = line_end(text, p);
                if le >= text.len() {
                    return None;
                }
                p = le + 1;
            }
            some(first_non_blank(text, p), MotionKind::Linewise)
        }
        Motion::PrevLineStart => {
            let mut p = line_start(text, cursor);
            for _ in 0..count {
                if p == 0 {
                    return None;
                }
                p = line_start(text, p - 1);
            }
            some(first_non_blank(text, p), MotionKind::Linewise)
        }
        Motion::FirstNonBlankDown => {
            let mut p = cursor;
            for _ in 1..count {
                let le = line_end(text, p);
                if le >= text.len() {
                    return None;
                }
                p = le + 1;
            }
            some(first_non_blank(text, p), MotionKind::Linewise)
        }
        Motion::SpaceRight => steps(cursor, count, MotionKind::Exclusive, |p| {
            space_right(text, p)
        }),
        Motion::BackspaceLeft => steps(cursor, count, MotionKind::Exclusive, |p| {
            backspace_left(text, p)
        }),
    }
}

fn some(pos: usize, kind: MotionKind) -> Option<MotionTarget> {
    Some(MotionTarget { pos, kind })
}

/// Apply `step` up to `count` times; a step that can't advance stops the loop
/// (partial-count success). None only if the first step didn't move.
fn steps(
    start: usize,
    count: usize,
    kind: MotionKind,
    mut step: impl FnMut(usize) -> Option<usize>,
) -> Option<MotionTarget> {
    let mut p = start;
    for _ in 0..count {
        match step(p) {
            Some(n) if n != p => p = n,
            _ => break,
        }
    }
    (p != start).then_some(MotionTarget { pos: p, kind })
}

/// One `w` step: past the current word (a same-class run) and any blanks to
/// the start of the next word. An empty line is a word (`:help w`); the
/// buffer end is the raw landing when there is no next word.
fn word_forward(text: &str, pos: usize, big: bool) -> Option<usize> {
    let mut p = pos;
    let cls = char_class(char_at(text, p)?, big);
    if cls == CharClass::Blank {
        p = next_char(text, p);
    } else {
        while let Some(c) = char_at(text, p) {
            if char_class(c, big) != cls {
                break;
            }
            p = next_char(text, p);
        }
    }
    while let Some(c) = char_at(text, p) {
        if char_class(c, big) != CharClass::Blank {
            break;
        }
        if c == '\n' && line_start(text, p) == p {
            break; // empty line counts as a word
        }
        p = next_char(text, p);
    }
    Some(p)
}

/// One `b` step: back over blanks (an empty line is a word) to the start of
/// the previous same-class run, or the start of the current one if mid-word.
fn word_back(text: &str, pos: usize, big: bool) -> Option<usize> {
    if pos == 0 {
        return None;
    }
    let mut p = prev_char(text, pos);
    loop {
        let c = char_at(text, p)?;
        if char_class(c, big) != CharClass::Blank {
            break;
        }
        if c == '\n' && line_start(text, p) == p {
            return Some(p); // empty line counts as a word
        }
        if p == 0 {
            return Some(0); // only blanks before: buffer start
        }
        p = prev_char(text, p);
    }
    let cls = char_class(char_at(text, p)?, big);
    while p > 0 {
        let q = prev_char(text, p);
        if char_class(char_at(text, q)?, big) != cls {
            break;
        }
        p = q;
    }
    Some(p)
}

/// One `e` step: forward to the last char of the current run, or of the next
/// word if already there. Empty lines are not word ends (`:help e`).
fn word_end(text: &str, pos: usize, big: bool) -> Option<usize> {
    let mut p = next_char(text, pos);
    loop {
        let c = char_at(text, p)?; // only blanks left: no move
        if char_class(c, big) != CharClass::Blank {
            break;
        }
        p = next_char(text, p);
    }
    let cls = char_class(char_at(text, p)?, big);
    loop {
        let n = next_char(text, p);
        match char_at(text, n) {
            Some(c) if char_class(c, big) == cls => p = n,
            _ => return Some(p),
        }
    }
}

/// `f`/`t`/`F`/`T`: the count'th occurrence on the cursor's line, or None.
/// Unlike other counted motions there is no partial success (`:help f`).
fn find(
    text: &str,
    cursor: usize,
    count: usize,
    kind: FindKind,
    ch: char,
    repeat: bool,
) -> Option<MotionTarget> {
    match kind {
        FindKind::FindFwd | FindKind::TillFwd => {
            let till = kind == FindKind::TillFwd;
            let end = line_end(text, cursor);
            let from = if cursor >= end {
                end
            } else {
                next_char(text, cursor)
            };
            let mut found = text[from..end]
                .char_indices()
                .filter(|&(_, c)| c == ch)
                .map(|(i, _)| from + i);
            let mut first = found.next()?;
            if till && repeat && prev_char(text, first) == cursor {
                first = found.next()?; // `;` skips an adjacent target (`:help ;`)
            }
            let pos = std::iter::once(first).chain(found).nth(count - 1)?;
            let pos = if till { prev_char(text, pos) } else { pos };
            some(pos, MotionKind::Inclusive)
        }
        FindKind::FindBack | FindKind::TillBack => {
            let till = kind == FindKind::TillBack;
            let start = line_start(text, cursor);
            let mut found = text[start..cursor]
                .char_indices()
                .rev()
                .filter(|&(_, c)| c == ch)
                .map(|(i, _)| start + i);
            let mut first = found.next()?;
            if till && repeat && next_char(text, first) == cursor {
                first = found.next()?;
            }
            let pos = std::iter::once(first).chain(found).nth(count - 1)?;
            let pos = if till { next_char(text, pos) } else { pos };
            some(pos, MotionKind::Exclusive)
        }
    }
}

/// `{`/`}` per Vim's `findpar`: from the cursor's line, skip any leading run
/// of truly empty lines, then a paragraph, landing on the next empty line's
/// start — or the buffer edge (`text.len()`/0) when there is none. Hitting
/// the edge satisfies the current repeat but fails if more repeats remain.
fn paragraph(text: &str, cursor: usize, count: usize, forward: bool) -> Option<MotionTarget> {
    // A landing that doesn't move the cursor is a failed motion (`{` at the
    // buffer start beeps).
    let done = |pos: usize| {
        if pos == cursor {
            None
        } else {
            some(pos, MotionKind::Exclusive)
        }
    };
    let mut line = line_start(text, cursor);
    for n in 0..count {
        let mut did_skip = false;
        let mut first = true;
        loop {
            let empty = line == line_end(text, line);
            if !empty {
                did_skip = true;
            }
            if !first && did_skip && empty {
                break;
            }
            if forward {
                let le = line_end(text, line);
                if le >= text.len() {
                    if n + 1 < count {
                        return None;
                    }
                    return done(text.len());
                }
                line = le + 1;
            } else {
                if line == 0 {
                    if n + 1 < count {
                        return None;
                    }
                    return done(0);
                }
                line = line_start(text, line - 1);
            }
            first = false;
        }
    }
    done(line)
}

/// The match of the bracket at `pos`, nesting-aware over the whole buffer
/// (only the bracket's own pair nests, as in Vim's `%`).
fn match_bracket(text: &str, pos: usize, ch: char) -> Option<usize> {
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0usize;
    if forward {
        for (i, c) in text[pos..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(pos + i);
                }
            }
        }
    } else {
        for (i, c) in text[..next_char(text, pos)].char_indices().rev() {
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// One `<Space>` step: the next char, where a non-empty line's `\n` is
/// crossed as part of the same step (Vim's default `whichwrap=b,s`). An empty
/// line's `\n` is a landing spot.
fn space_right(text: &str, pos: usize) -> Option<usize> {
    if pos >= text.len() {
        return None;
    }
    let mut p = next_char(text, pos);
    if char_at(text, p) == Some('\n') && p > line_start(text, p) {
        p = next_char(text, p);
    }
    if p >= text.len() && !text.ends_with('\n') {
        return None; // past the last char: not a position
    }
    Some(p)
}

/// One `<BS>` step: the mirror of [`space_right`].
fn backspace_left(text: &str, pos: usize) -> Option<usize> {
    if pos == 0 {
        return None;
    }
    let mut p = prev_char(text, pos);
    if char_at(text, p) == Some('\n') && p > line_start(text, p) {
        p = prev_char(text, p);
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use FindKind::*;
    use MotionKind::*;

    fn ev(text: &str, cursor: usize, count: usize, m: Motion) -> Option<(usize, MotionKind)> {
        eval(text, cursor, count, m, char_col(text, cursor)).map(|t| (t.pos, t.kind))
    }

    fn fnd(kind: FindKind, target: char, repeat: bool) -> Motion {
        Motion::Find {
            kind,
            target,
            repeat,
        }
    }

    type Case = (
        &'static str,
        usize,
        usize,
        Motion,
        Option<(usize, MotionKind)>,
    );

    #[track_caller]
    fn check(cases: &[Case]) {
        for &(text, cursor, count, m, expect) in cases {
            assert_eq!(
                ev(text, cursor, count, m),
                expect,
                "{m:?} x{count} in {text:?} at {cursor}"
            );
        }
    }

    #[test]
    fn left_right() {
        check(&[
            ("abc", 1, 1, Motion::Left, Some((0, Exclusive))),
            ("abc", 0, 1, Motion::Left, None),
            ("abc", 2, 5, Motion::Left, Some((0, Exclusive))), // partial
            ("aé✓b", 6, 1, Motion::Left, Some((3, Exclusive))),
            ("a\nbc", 2, 1, Motion::Left, None), // col 0 of line 2
            ("abc", 0, 1, Motion::Right, Some((1, Exclusive))),
            ("abc", 2, 1, Motion::Right, Some((3, Exclusive))), // raw line end
            ("abc", 0, 10, Motion::Right, Some((3, Exclusive))), // partial
            ("aé✓b", 1, 1, Motion::Right, Some((3, Exclusive))),
            ("ab\ncd", 1, 1, Motion::Right, Some((2, Exclusive))), // the \n
            ("a\n\nb", 2, 1, Motion::Right, None),                 // empty line
            ("", 0, 1, Motion::Right, None),
        ]);
    }

    #[test]
    fn down_up() {
        check(&[
            ("ab\ncd\nef", 0, 1, Motion::Down, Some((3, Linewise))),
            ("ab\ncd\nef", 1, 2, Motion::Down, Some((7, Linewise))),
            ("ab\ncd", 0, 2, Motion::Down, None), // overshoot fails whole
            ("ab\n", 0, 1, Motion::Down, Some((3, Linewise))), // empty last line
            ("ab\n", 3, 1, Motion::Down, None),
            ("é✓\nab", 2, 1, Motion::Down, Some((7, Linewise))), // col 1
            ("ab\ncd\nef", 7, 1, Motion::Up, Some((4, Linewise))),
            ("ab\ncd", 1, 1, Motion::Up, None),
            ("ab\ncd", 4, 5, Motion::Up, None),
            ("ab\n", 3, 1, Motion::Up, Some((0, Linewise))),
        ]);
        // Sticky column: shorter line clamps, usize::MAX = line's last char.
        assert_eq!(
            eval("abcd\nx\nefgh", 3, 1, Motion::Down, 3).map(|t| t.pos),
            Some(5)
        );
        assert_eq!(
            eval("ab\ncdef", 0, 1, Motion::Down, usize::MAX).map(|t| t.pos),
            Some(6)
        );
        assert_eq!(
            eval("a\n\nb", 0, 1, Motion::Down, usize::MAX).map(|t| t.pos),
            Some(2) // empty line: its start
        );
    }

    #[test]
    fn line_start_end() {
        check(&[
            ("ab\ncd", 4, 1, Motion::LineStart, Some((3, Exclusive))),
            ("  ab", 3, 1, Motion::FirstNonBlank, Some((2, Exclusive))),
            ("abc", 0, 1, Motion::LineEnd, Some((2, Inclusive))),
            ("aé✓", 0, 1, Motion::LineEnd, Some((3, Inclusive))), // ✓'s start
            ("ab\ncd", 0, 2, Motion::LineEnd, Some((4, Inclusive))),
            ("ab\ncd", 0, 9, Motion::LineEnd, Some((4, Inclusive))), // clamped
            ("\nx", 0, 1, Motion::LineEnd, Some((0, Inclusive))),    // empty line
            ("", 0, 1, Motion::LineEnd, Some((0, Inclusive))),
        ]);
    }

    #[test]
    fn word_forward() {
        let w = Motion::WordForward { big: false };
        let big = Motion::WordForward { big: true };
        check(&[
            ("foo bar", 0, 1, w, Some((4, Exclusive))),
            ("foo bar", 1, 1, w, Some((4, Exclusive))), // from mid-word
            ("foo(bar", 0, 1, w, Some((3, Exclusive))), // punct run = word
            ("foo(bar", 0, 1, big, Some((7, Exclusive))), // one WORD: raw EOF
            ("foo bar", 4, 1, w, Some((7, Exclusive))), // last word: raw EOF
            ("foo ", 3, 1, w, Some((4, Exclusive))),
            ("a\n\nb", 0, 1, w, Some((2, Exclusive))), // empty line is a word
            ("a\n\nb", 2, 1, w, Some((3, Exclusive))),
            ("a\n \nb", 0, 1, w, Some((4, Exclusive))), // blank line is not
            ("foo bar baz", 0, 2, w, Some((8, Exclusive))),
            ("foo bar", 0, 9, w, Some((7, Exclusive))), // partial count
            ("é✓ b", 0, 1, w, Some((2, Exclusive))),    // é word, ✓ punct
            ("é✓ b", 2, 1, w, Some((6, Exclusive))),
            ("", 0, 1, w, None),
            ("ab\n", 3, 1, w, None), // empty last line: nowhere to go
        ]);
    }

    #[test]
    fn word_back() {
        let b = Motion::WordBack { big: false };
        let big = Motion::WordBack { big: true };
        check(&[
            ("foo bar", 4, 1, b, Some((0, Exclusive))),
            ("foo bar", 6, 1, b, Some((4, Exclusive))),
            ("foo bar", 5, 1, b, Some((4, Exclusive))), // mid-word: its start
            ("foo()bar", 5, 1, b, Some((3, Exclusive))), // punct run
            ("foo()bar", 5, 1, big, Some((0, Exclusive))),
            ("foo bar baz", 10, 2, b, Some((4, Exclusive))),
            ("foo bar baz", 8, 9, b, Some((0, Exclusive))), // partial count
            ("  ab", 2, 1, b, Some((0, Exclusive))),        // only blanks: 0
            ("a\n\nb", 3, 1, b, Some((2, Exclusive))),      // empty line
            ("a\n\nb", 2, 1, b, Some((0, Exclusive))),
            ("é✓x", 5, 1, b, Some((2, Exclusive))),
            ("abc", 0, 1, b, None),
            ("", 0, 1, b, None),
        ]);
    }

    #[test]
    fn word_end() {
        let e = Motion::WordEnd { big: false };
        let big = Motion::WordEnd { big: true };
        check(&[
            ("foo bar", 0, 1, e, Some((2, Inclusive))),
            ("foo bar", 2, 1, e, Some((6, Inclusive))), // at end: next word's
            ("foo(x", 0, 1, e, Some((2, Inclusive))),
            ("foo((x", 0, 2, e, Some((4, Inclusive))), // punct run end
            ("foo((x", 0, 1, big, Some((5, Inclusive))),
            ("a\n\nb", 0, 1, e, Some((3, Inclusive))), // empty line not a word
            ("foo bar", 0, 9, e, Some((6, Inclusive))), // partial count
            ("foo", 2, 1, e, None),                    // at last word end
            ("x  ", 1, 1, e, None),                    // only blanks left
            ("𝄞𝄞 x", 0, 1, e, Some((4, Inclusive))),
            ("", 0, 1, e, None),
        ]);
    }

    #[test]
    fn goto_line() {
        check(&[
            (
                "a\nb\nc",
                4,
                1,
                Motion::GotoLine(Some(1)),
                Some((0, Linewise)),
            ),
            (
                "a\nb\nc",
                0,
                1,
                Motion::GotoLine(Some(2)),
                Some((2, Linewise)),
            ),
            (
                "a\nb\nc",
                0,
                1,
                Motion::GotoLine(Some(99)),
                Some((4, Linewise)), // clamped to the last line
            ),
            ("a\n  b", 0, 1, Motion::GotoLine(None), Some((4, Linewise))),
            ("", 0, 1, Motion::GotoLine(None), Some((0, Linewise))),
        ]);
    }

    #[test]
    fn find_till() {
        check(&[
            (
                "abcabc",
                0,
                1,
                fnd(FindFwd, 'c', false),
                Some((2, Inclusive)),
            ),
            (
                "abcabc",
                0,
                2,
                fnd(FindFwd, 'c', false),
                Some((5, Inclusive)),
            ),
            ("abcabc", 0, 3, fnd(FindFwd, 'c', false), None), // no partial
            ("abca", 0, 1, fnd(FindFwd, 'a', false), Some((3, Inclusive))), // strictly after
            ("ab\ncd", 0, 1, fnd(FindFwd, 'c', false), None), // current line only
            ("abc", 0, 1, fnd(TillFwd, 'c', false), Some((1, Inclusive))),
            // Adjacent target: plain `t` lands in place, `;` skips it.
            (
                "abxbx",
                1,
                1,
                fnd(TillFwd, 'x', false),
                Some((1, Inclusive)),
            ),
            ("abxbx", 1, 1, fnd(TillFwd, 'x', true), Some((3, Inclusive))),
            ("abxb", 1, 1, fnd(TillFwd, 'x', true), None),
            (
                "abcabc",
                5,
                1,
                fnd(FindBack, 'a', false),
                Some((3, Exclusive)),
            ),
            (
                "abcabc",
                5,
                2,
                fnd(FindBack, 'a', false),
                Some((0, Exclusive)),
            ),
            ("abcabc", 5, 3, fnd(FindBack, 'a', false), None),
            (
                "abcabc",
                3,
                1,
                fnd(FindBack, 'a', false),
                Some((0, Exclusive)),
            ), // not the cursor's char
            (
                "axbxc",
                4,
                1,
                fnd(TillBack, 'x', false),
                Some((4, Exclusive)),
            ),
            (
                "axbxc",
                4,
                1,
                fnd(TillBack, 'x', true),
                Some((2, Exclusive)),
            ),
            ("axb", 2, 1, fnd(TillBack, 'x', true), None),
            ("é✓é", 0, 1, fnd(FindFwd, 'é', false), Some((5, Inclusive))),
            ("aé✓", 0, 1, fnd(TillFwd, '✓', false), Some((1, Inclusive))),
            ("é✓é", 5, 1, fnd(TillBack, 'é', false), Some((2, Exclusive))),
            ("", 0, 1, fnd(FindFwd, 'x', false), None),
        ]);
    }

    #[test]
    fn paragraphs() {
        let (f, b) = (Motion::ParagraphForward, Motion::ParagraphBack);
        check(&[
            ("aa\n\nbb\n\ncc", 0, 1, f, Some((3, Exclusive))),
            ("aa\n\nbb\n\ncc", 0, 2, f, Some((7, Exclusive))),
            ("aa\n\nbb\n\ncc", 0, 3, f, Some((10, Exclusive))), // EOF boundary
            ("aa\n\nbb\n\ncc", 0, 4, f, None),                  // counts left after the edge
            ("aa\n\nbb\n\ncc", 3, 1, f, Some((7, Exclusive))),  // from empty line
            ("a\n\n\nb\n\nc", 2, 1, f, Some((6, Exclusive))),   // empty run collapses
            ("aa\nbb", 0, 1, f, Some((5, Exclusive))),
            ("a\n \nb", 0, 1, f, Some((5, Exclusive))), // blank line not empty
            ("", 0, 1, f, None),                        // empty buffer: nowhere to go
            ("aa\n\nbb", 4, 1, b, Some((3, Exclusive))),
            ("aa\n\nbb", 5, 1, b, Some((3, Exclusive))),
            ("aa\n\nbb", 3, 1, b, Some((0, Exclusive))), // from the empty line
            ("aa\n\nbb", 4, 2, b, Some((0, Exclusive))),
            ("aa\n\nbb", 4, 3, b, None),
            ("aa\n\n\nbb", 5, 1, b, Some((4, Exclusive))), // nearest empty line
            ("aa\n\n\nbb", 4, 1, b, Some((0, Exclusive))), // empty run collapses
        ]);
    }

    #[test]
    fn match_pair() {
        let m = Motion::MatchPair;
        check(&[
            ("a(b)c", 0, 1, m, Some((3, Inclusive))), // first bracket after cursor
            ("a(b)c", 3, 1, m, Some((1, Inclusive))),
            ("((x))", 0, 1, m, Some((4, Inclusive))), // nesting
            ("((x))", 1, 1, m, Some((3, Inclusive))),
            ("(a[b])", 2, 1, m, Some((4, Inclusive))), // pairs nest independently
            ("{a\nb}", 0, 1, m, Some((4, Inclusive))), // match spans lines
            ("(\n)", 2, 1, m, Some((0, Inclusive))),
            ("ab\n()", 0, 1, m, None), // bracket not on cursor's line
            ("(((", 0, 1, m, None),    // unbalanced
            ("abc", 0, 1, m, None),
            ("(é✓)", 0, 1, m, Some((6, Inclusive))),
            ("", 0, 1, m, None),
        ]);
    }

    #[test]
    fn line_starts() {
        let (n, p) = (Motion::NextLineStart, Motion::PrevLineStart);
        check(&[
            ("ab\n  cd", 0, 1, n, Some((5, Linewise))),
            ("ab\ncd\nef", 0, 2, n, Some((6, Linewise))),
            ("ab\ncd", 3, 1, n, None),
            ("ab\ncd", 0, 2, n, None), // overshoot fails whole
            ("  ab\ncd", 5, 1, p, Some((2, Linewise))),
            ("ab\ncd\nef", 6, 2, p, Some((0, Linewise))),
            ("ab\ncd", 1, 1, p, None),
            ("ab\ncd", 4, 5, p, None),
        ]);
    }

    #[test]
    fn space_backspace() {
        let (s, b) = (Motion::SpaceRight, Motion::BackspaceLeft);
        check(&[
            ("ab", 0, 1, s, Some((1, Exclusive))),
            ("ab\ncd", 1, 1, s, Some((3, Exclusive))), // EOL is one step
            ("ab\ncd", 0, 3, s, Some((4, Exclusive))),
            ("a\n\nb", 0, 1, s, Some((2, Exclusive))), // lands on empty line
            ("a\n\nb", 2, 1, s, Some((3, Exclusive))),
            ("ab", 1, 1, s, None),                 // last char: past the buffer
            ("ab", 0, 9, s, Some((1, Exclusive))), // partial count
            ("ab\n", 1, 1, s, Some((3, Exclusive))), // empty last line is a spot
            ("ab\n", 3, 1, s, None),
            ("é✓", 0, 1, s, Some((2, Exclusive))),
            ("", 0, 1, s, None),
            ("ab\ncd", 3, 1, b, Some((1, Exclusive))),
            ("a\n\nb", 3, 1, b, Some((2, Exclusive))),
            ("a\n\nb", 2, 1, b, Some((0, Exclusive))),
            ("ab\n", 3, 1, b, Some((1, Exclusive))),
            ("ab", 1, 5, b, Some((0, Exclusive))), // partial count
            ("é✓", 2, 1, b, Some((0, Exclusive))),
            ("ab", 0, 1, b, None),
            ("", 0, 1, b, None),
        ]);
    }
}

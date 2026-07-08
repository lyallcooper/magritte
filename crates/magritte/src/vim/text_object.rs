//! Text objects (`:help text-objects`): the byte range an `i`/`a` object
//! covers from a given cursor position. Pure functions over `&str`.

use super::*;
use std::ops::Range;

/// Resolve a text object at `cursor`. `around` is `a` vs `i`; `obj` is the
/// object key: `w`/`W` (word/WORD), `"` `'` `` ` `` (quotes), and the bracket
/// pairs `( ) b`, `[ ]`, `{ } B`, `< >`. `count` repeats word objects
/// (`2aw`) and selects enclosing pairs for brackets (`2i(` = one pair out).
///
/// Returns `None` when there is no such object at the cursor (Vim beeps):
/// no quote pair on the line, cursor not inside (or on) a matching bracket
/// pair, `iw` on an empty buffer...
pub(super) fn text_object(
    text: &str,
    cursor: usize,
    around: bool,
    obj: char,
    count: usize,
) -> Option<Range<usize>> {
    let cursor = clamp_normal(text, cursor);
    let count = count.max(1);
    match obj {
        'w' => word_object(text, cursor, around, false, count),
        'W' => word_object(text, cursor, around, true, count),
        '"' | '\'' | '`' => quote_object(text, cursor, around, obj, count),
        '(' | ')' | 'b' => bracket_object(text, cursor, around, '(', ')', count),
        '[' | ']' => bracket_object(text, cursor, around, '[', ']', count),
        '{' | '}' | 'B' => bracket_object(text, cursor, around, '{', '}', count),
        '<' | '>' => bracket_object(text, cursor, around, '<', '>', count),
        _ => None,
    }
}

// --- Words ----------------------------------------------------------------

/// `iw`/`aw` (`:help aw`). Units are maximal same-class runs — word chars,
/// punctuation, or blanks (spaces/tabs, never crossing a newline); a newline
/// is its own unit (an empty line is a word). `iw` spans `count` consecutive
/// units from the cursor's. `aw` takes `count` words each with the blanks
/// between them, then the trailing blanks — or, when there are none, the
/// leading blanks; starting on blanks it takes them plus the following word.
fn word_object(
    text: &str,
    cursor: usize,
    around: bool,
    big: bool,
    count: usize,
) -> Option<Range<usize>> {
    // On an empty line (or the empty last line): `aw` takes its newline —
    // the one under the cursor, or the preceding one at EOF — while `iw`
    // selects nothing (`diw` there is a no-op in Vim, `daw` joins).
    let Some(c) = char_at(text, cursor) else {
        if text.is_empty() {
            return None;
        }
        if !around {
            return Some(cursor..cursor);
        }
        return text
            .ends_with('\n')
            .then(|| prev_char(text, cursor)..cursor);
    };
    if c == '\n' {
        return Some(if around {
            cursor..next_char(text, cursor)
        } else {
            cursor..cursor
        });
    }
    let start = unit_start(text, cursor, big);
    let mut end = unit_end(text, start, big);
    if !around {
        for _ in 1..count {
            if end >= text.len() {
                break;
            }
            end = unit_end(text, end, big);
        }
        return Some(start..end);
    }
    if char_class(c, big) == CharClass::Blank {
        // On blanks: the blank run plus the following word, `count` times.
        // Trailing blanks reach across the newline to the next line's word
        // (`daw` on end-of-line blanks eats the blanks, the newline, and the
        // word after it, like Vim).
        for i in 0..count {
            let mut at = end;
            if i > 0 && is_line_blank(text, at) {
                at = unit_end(text, at, big);
            }
            if char_at(text, at) == Some('\n') {
                at = next_char(text, at);
                while is_line_blank(text, at) {
                    at = unit_end(text, at, big);
                }
            }
            match char_at(text, at) {
                Some(n) if n != '\n' && char_class(n, big) != CharClass::Blank => {
                    end = unit_end(text, at, big);
                }
                _ => break,
            }
        }
        return Some(start..end);
    }
    // On a word: `count` words with the blanks between them...
    for _ in 1..count {
        let mut at = end;
        if is_line_blank(text, at) {
            at = unit_end(text, at, big);
        }
        match char_at(text, at) {
            Some(n) if n != '\n' && char_class(n, big) != CharClass::Blank => {
                end = unit_end(text, at, big);
            }
            _ => break,
        }
    }
    // ...plus the trailing blanks, or the leading ones when none trail.
    let mut s = start;
    if is_line_blank(text, end) {
        end = unit_end(text, end, big);
    } else {
        while s > 0 && is_line_blank(text, prev_char(text, s)) {
            s = prev_char(text, s);
        }
    }
    Some(s..end)
}

fn is_line_blank(text: &str, pos: usize) -> bool {
    matches!(char_at(text, pos), Some(' ' | '\t'))
}

/// Start of the word-object unit containing `pos` (`pos` must be on a char
/// that isn't a newline).
fn unit_start(text: &str, pos: usize, big: bool) -> usize {
    let Some(c) = char_at(text, pos) else {
        return pos;
    };
    let class = char_class(c, big);
    let mut start = pos;
    while start > 0 {
        let p = prev_char(text, start);
        match char_at(text, p) {
            Some(pc) if pc != '\n' && char_class(pc, big) == class => start = p,
            _ => break,
        }
    }
    start
}

/// End of the word-object unit starting at `pos`.
fn unit_end(text: &str, pos: usize, big: bool) -> usize {
    match char_at(text, pos) {
        None => pos,
        Some('\n') => next_char(text, pos),
        Some(c) => {
            let class = char_class(c, big);
            let mut end = next_char(text, pos);
            while char_at(text, end).is_some_and(|n| n != '\n' && char_class(n, big) == class) {
                end = next_char(text, end);
            }
            end
        }
    }
}

// --- Quotes ---------------------------------------------------------------

/// `i"`/`a"` (`:help aquote`): line-scoped. Quotes on the cursor's line pair
/// up from the line start; the object is the pair the cursor is in (a cursor
/// on a quote char is inside its pair) or the next pair after it. `a"` adds
/// the trailing blanks, or the leading ones when none trail; `2i"` includes
/// the quotes without any blanks (`:help v_iquote`).
fn quote_object(
    text: &str,
    cursor: usize,
    around: bool,
    quote: char,
    count: usize,
) -> Option<Range<usize>> {
    let ls = line_start(text, cursor);
    let le = line_end(text, cursor);
    let mut quotes = text[ls..le]
        .char_indices()
        .filter(|&(_, c)| c == quote)
        .map(|(i, _)| ls + i);
    let (open, close) = loop {
        match (quotes.next(), quotes.next()) {
            (Some(o), Some(c)) if cursor <= c => break (o, c),
            (Some(_), Some(_)) => {}
            _ => return None,
        }
    };
    if around {
        let mut s = open;
        let mut e = next_char(text, close);
        let e0 = e;
        while e < le && is_line_blank(text, e) {
            e = next_char(text, e);
        }
        if e == e0 {
            while s > ls && is_line_blank(text, prev_char(text, s)) {
                s = prev_char(text, s);
            }
        }
        Some(s..e)
    } else if count >= 2 {
        Some(open..next_char(text, close))
    } else {
        Some(next_char(text, open)..close)
    }
}

// --- Brackets -------------------------------------------------------------

/// `i(`/`a(` and friends: the nearest enclosing pair (whole-buffer,
/// nesting-aware; a cursor on either delimiter is inside), `count - 1`
/// enclosing levels further out. `a` includes the delimiters. `i` on an
/// empty pair yields the empty range just after the open delimiter (`ci(`
/// on `()` inserts between them; `di(` is a no-op).
fn bracket_object(
    text: &str,
    cursor: usize,
    around: bool,
    open: char,
    close: char,
    count: usize,
) -> Option<Range<usize>> {
    let (mut o, mut c) = match char_at(text, cursor) {
        Some(ch) if ch == open => (cursor, match_forward(text, cursor, open, close)?),
        Some(ch) if ch == close => (match_backward(text, cursor, open, close)?, cursor),
        _ => {
            // Prefer the enclosing pair; with none, Vim seeks forward to the
            // next pair (even on a later line).
            let o = unmatched_open_before(text, cursor, open, close)
                .or_else(|| text[cursor..].find(open).map(|i| cursor + i))?;
            (o, match_forward(text, o, open, close)?)
        }
    };
    for _ in 1..count {
        o = unmatched_open_before(text, o, open, close)?;
        c = match_forward(text, o, open, close)?;
    }
    if around {
        return Some(o..next_char(text, c));
    }
    let mut s = next_char(text, o);
    let mut e = c;
    // Multiline inner block, as in Vim: an open delimiter at end of line puts
    // the content start on the next line, and a close preceded only by
    // blanks then keeps its indent.
    if char_at(text, s) == Some('\n') && next_char(text, s) <= e {
        s = next_char(text, s);
        let cls = line_start(text, c);
        if cls >= s && text[cls..c].chars().all(|ch| ch == ' ' || ch == '\t') {
            e = cls;
        }
    }
    Some(s..e)
}

/// Matching close for the `open` delimiter at `open_pos`.
fn match_forward(text: &str, open_pos: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text[open_pos.min(text.len())..].char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open_pos + i);
            }
        }
    }
    None
}

/// Matching open for the `close` delimiter at `close_pos`.
fn match_backward(text: &str, close_pos: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text[..next_char(text, close_pos)].char_indices().rev() {
        if ch == close {
            depth += 1;
        } else if ch == open {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// The innermost `open` before `before` with no matching close before it.
fn unmatched_open_before(text: &str, before: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text[..before.min(text.len())].char_indices().rev() {
        if ch == close {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn check(
        text: &str,
        cursor: usize,
        around: bool,
        obj: char,
        count: usize,
        want: Option<(usize, usize)>,
    ) {
        let got = text_object(text, cursor, around, obj, count).map(|r| (r.start, r.end));
        assert_eq!(
            got, want,
            "{text:?} cursor={cursor} around={around} obj={obj:?} count={count}"
        );
    }

    #[test]
    fn inner_word() {
        for &(text, cursor, count, want) in &[
            ("foo bar", 1, 1, Some((0, 3))),
            ("foo bar", 3, 1, Some((3, 4))), // on the blank: the blank run
            ("foo  bar", 4, 1, Some((3, 5))),
            ("foo.bar", 0, 1, Some((0, 3))), // punct is its own word
            ("foo.bar", 3, 1, Some((3, 4))),
            ("foo bar", 0, 2, Some((0, 4))), // 2iw: word + blanks
            ("foo bar", 0, 3, Some((0, 7))),
            ("foo.bar", 0, 2, Some((0, 4))), // no blanks: word + punct
            ("foo bar", 0, 99, Some((0, 7))), // count clamps at EOF
            ("ab  \ncd", 2, 1, Some((2, 4))), // blanks stop at the newline
            ("a\n\nb", 2, 1, Some((2, 2))),  // empty line: iw selects nothing
            ("ab", 2, 1, Some((0, 2))),      // EOF clamps onto the last word
            ("", 0, 1, None),
            ("héllo wörld", 3, 1, Some((0, 6))),
            ("héllo wörld", 8, 1, Some((7, 13))),
            ("✓✓ x", 0, 1, Some((0, 6))), // symbol (punct) run
            ("𝄞a", 0, 1, Some((0, 4))),   // 4-byte punct, then a word
        ] {
            check(text, cursor, false, 'w', count, want);
        }
    }

    #[test]
    fn inner_big_word() {
        for &(text, cursor, count, want) in &[
            ("foo.bar baz", 0, 1, Some((0, 7))),
            ("foo.bar baz", 7, 1, Some((7, 8))),
            ("a✓b c", 0, 1, Some((0, 5))),
        ] {
            check(text, cursor, false, 'W', count, want);
        }
    }

    #[test]
    fn around_word() {
        for &(text, cursor, count, want) in &[
            ("foo bar", 0, 1, Some((0, 4))),  // trailing blank
            ("foo bar", 4, 1, Some((3, 7))),  // no trailing: leading blank
            ("x foo.", 2, 1, Some((1, 5))),   // punct next: leading blank
            ("foo", 1, 1, Some((0, 3))),      // no blanks either side
            ("foo  bar", 3, 1, Some((3, 8))), // on blanks: them + next word
            ("x  a b", 1, 2, Some((1, 6))),   // on blanks, count 2
            ("ab  ", 2, 1, Some((2, 4))),     // blanks with no word after
            ("ab  \ncd", 2, 1, Some((2, 7))), // trailing blanks cross the newline
            ("a\n\nb", 2, 1, Some((2, 3))),   // empty line
            ("a b c", 0, 2, Some((0, 4))),    // 2aw: two words + trailing
            ("a b", 0, 2, Some((0, 3))),      // 2aw at line end: no blanks left
            ("foo.bar", 0, 2, Some((0, 4))),
            ("é ✓", 0, 1, Some((0, 3))),
        ] {
            check(text, cursor, true, 'w', count, want);
        }
        check("a.b c", 0, true, 'W', 1, Some((0, 4)));
    }

    #[test]
    fn quotes() {
        for &(text, cursor, around, obj, count, want) in &[
            (r#"say "hi" now"#, 5, false, '"', 1, Some((5, 7))),
            (r#"say "hi" now"#, 5, true, '"', 1, Some((4, 9))), // + trailing blank
            (r#"say "hi" now"#, 4, false, '"', 1, Some((5, 7))), // on the open
            (r#"say "hi" now"#, 7, false, '"', 1, Some((5, 7))), // on the close
            (r#"say "hi" now"#, 0, false, '"', 1, Some((5, 7))), // before: next pair
            (r#"say "hi" now"#, 9, false, '"', 1, None),        // after the last pair
            (r#""a" "b""#, 3, false, '"', 1, Some((5, 6))),     // between pairs: next
            (r#""a"#, 1, false, '"', 1, None),                  // unmatched
            (r#""""#, 0, false, '"', 1, Some((1, 1))),          // empty content
            (r#""""#, 0, true, '"', 1, Some((0, 2))),
            (r#"x "y" z"#, 3, false, '"', 2, Some((2, 5))), // 2i" includes quotes
            (r#"x  "y""#, 4, true, '"', 1, Some((1, 6))),   // leading when no trailing
            ("\"a\"  ", 1, true, '"', 1, Some((0, 5))),
            ("\"é✓\"", 3, false, '"', 1, Some((1, 6))),
            ("\"a\nb\"", 0, false, '"', 1, None), // line-scoped
            ("x\n\"a\" b", 3, false, '"', 1, Some((3, 4))),
            ("a 'b' c", 3, false, '\'', 1, Some((3, 4))),
            ("`x`", 1, false, '`', 1, Some((1, 2))),
        ] {
            check(text, cursor, around, obj, count, want);
        }
    }

    #[test]
    fn brackets() {
        for &(text, cursor, around, obj, count, want) in &[
            ("a(b)c", 2, false, '(', 1, Some((2, 3))),
            ("a(b)c", 2, true, '(', 1, Some((1, 4))),
            ("a(b)c", 2, false, ')', 1, Some((2, 3))), // close/`b` aliases
            ("a(b)c", 2, false, 'b', 1, Some((2, 3))),
            ("a(b)c", 1, false, '(', 1, Some((2, 3))), // on the open
            ("a(b)c", 3, false, '(', 1, Some((2, 3))), // on the close
            ("((x))", 2, false, '(', 1, Some((2, 3))),
            ("((x))", 2, false, '(', 2, Some((1, 4))), // count: one level out
            ("((x))", 2, true, '(', 2, Some((0, 5))),
            ("((x))", 2, false, '(', 3, None),
            ("(a) b", 4, false, '(', 1, None), // cursor outside the pair
            ("(a", 1, false, '(', 1, None),    // unbalanced
            ("()", 0, false, '(', 1, Some((1, 1))), // empty: range at open+1
            ("()", 0, true, '(', 1, Some((0, 2))),
            ("([x])", 2, false, '(', 1, Some((1, 4))), // other kinds ignored
            ("([x])", 2, false, '[', 1, Some((2, 3))),
            ("{x\ny}", 3, false, 'B', 1, Some((1, 4))), // pairs cross lines
            ("[é]", 1, false, '[', 1, Some((1, 3))),
            ("[é]", 1, true, '[', 1, Some((0, 4))),
            ("<a>", 1, false, '>', 1, Some((1, 2))),
            ("abc", 1, false, 'q', 1, None), // unknown object key
        ] {
            check(text, cursor, around, obj, count, want);
        }
    }

    #[test]
    fn brackets_multiline_inner() {
        for &(text, cursor, around, count, want) in &[
            // Open at end of line: content starts on the next line...
            ("foo(\n  bar\n)", 6, false, 1, Some((5, 11))),
            ("foo(\n  bar\n)", 6, true, 1, Some((3, 12))),
            // ...and a close preceded only by blanks keeps its indent.
            ("a(\n b\n  )", 4, false, 1, Some((3, 6))),
            // Close mid-line: content runs right up to it.
            ("(\nx y)", 2, false, 1, Some((2, 5))),
            // Open mid-line: the newline stays in the content.
            ("(a\n)", 1, false, 1, Some((1, 3))),
        ] {
            check(text, cursor, around, '(', count, want);
        }
    }

    #[test]
    fn no_panics_on_any_boundary() {
        let text = "é(✓ \"𝄞 x\" [a]\n{b\n} 'c'\n";
        let objs = [
            'w', 'W', '"', '\'', '`', '(', ')', 'b', '[', ']', '{', '}', 'B', '<', '>', 'q',
        ];
        for (i, _) in text.char_indices().chain([(text.len(), ' ')]) {
            for &obj in &objs {
                for around in [false, true] {
                    for count in [0, 1, 2, 5] {
                        let _ = text_object(text, i, around, obj, count);
                    }
                }
            }
        }
    }
}

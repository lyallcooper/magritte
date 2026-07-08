//! Surround (vim-surround's MVP): add (`ys`), delete (`ds`), and change
//! (`cs`) bracket/quote pairs. Pure functions over `&str` returning a single
//! [`EditOp`] that rewrites the affected span.

use super::*;

/// The pair a surround target char names: `(open, close, padded)`. `padded`
/// is true for the literal opening bracket spellings, which add (`ys`) or eat
/// (`ds`) one blank of inner padding; closing spellings, aliases, and quotes
/// are tight.
fn pair(c: char) -> Option<(char, char, bool)> {
    Some(match c {
        '(' => ('(', ')', true),
        '{' => ('{', '}', true),
        '[' => ('[', ']', true),
        '<' => ('<', '>', true),
        ')' | 'b' => ('(', ')', false),
        '}' | 'B' => ('{', '}', false),
        ']' | 'r' => ('[', ']', false),
        '>' | 'a' => ('<', '>', false),
        '"' | '\'' | '`' => (c, c, false),
        _ => return None,
    })
}

/// `pos` floored to a char boundary at or below it (and to `text.len()`).
fn floor_boundary(text: &str, pos: usize) -> usize {
    let mut pos = pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Wrap `range` in the pair named by `c`. Opening bracket chars add inner
/// spaces (`(` → `( x )`); closing chars and quotes don't (`)` → `(x)`).
/// Aliases: `b` = `)`, `B` = `}`, `r` = `]`, `a` = `>`. Returns `None` for a
/// char that names no pair. The cursor lands on the opening delimiter.
pub(super) fn add(text: &str, range: std::ops::Range<usize>, c: char) -> Option<EditOp> {
    let (open, close, padded) = pair(c)?;
    let start = floor_boundary(text, range.start);
    let end = floor_boundary(text, range.end).max(start);
    let pad = if padded { " " } else { "" };
    let content = &text[start..end];
    Some(EditOp {
        range: start..end,
        text: format!("{open}{pad}{content}{pad}{close}"),
        cursor: start,
    })
}

/// Delete the nearest enclosing pair named by `c` around `cursor` (`ds"`,
/// `ds(`). Opening-char targets also eat the inner padding spaces, closing
/// chars don't, matching vim-surround. Returns `None` when no enclosing pair
/// is found. The cursor lands where the opening delimiter was.
pub(super) fn delete(text: &str, cursor: usize, c: char) -> Option<EditOp> {
    let (range, kept) = unwrap_pair(text, cursor, c)?;
    let post = format!("{}{}{}", &text[..range.start], kept, &text[range.end..]);
    let cursor = clamp_normal(&post, range.start);
    Some(EditOp {
        range,
        text: kept,
        cursor,
    })
}

/// Replace the nearest enclosing pair `from` around `cursor` with the pair
/// `to` (`cs"'`, `cs(]`). Combines [`delete`]'s targeting with [`add`]'s
/// insertion rules. The cursor lands on the new opening delimiter.
pub(super) fn change(text: &str, cursor: usize, from: char, to: char) -> Option<EditOp> {
    let (open, close, padded) = pair(to)?;
    let (range, kept) = unwrap_pair(text, cursor, from)?;
    let pad = if padded { " " } else { "" };
    let start = range.start;
    Some(EditOp {
        range,
        text: format!("{open}{pad}{kept}{pad}{close}"),
        cursor: start,
    })
}

/// The full span `open..close_end` of the nearest pair named by `c` enclosing
/// `cursor`, and the inner content to keep. Opening-char targets give up one
/// adjacent blank per side to the delimiters (`\s\=` in vim-surround). Fails
/// on an empty pair, exactly like vim-surround (its `di(` keeps nothing and
/// bails).
fn unwrap_pair(text: &str, cursor: usize, c: char) -> Option<(std::ops::Range<usize>, String)> {
    let (open, close, padded) = pair(c)?;
    let cursor = floor_boundary(text, cursor);
    let (open_pos, close_pos) = if open == close {
        find_quote_pair(text, cursor, open)?
    } else {
        find_bracket_pair(text, cursor, open, close)?
    };
    let mut kept = &text[next_char(text, open_pos)..close_pos];
    if kept.is_empty() {
        return None;
    }
    if padded {
        if let Some(rest) = kept.strip_prefix([' ', '\t']) {
            kept = rest;
        }
        if let Some(rest) = kept.strip_suffix([' ', '\t']) {
            kept = rest;
        }
    }
    Some((open_pos..next_char(text, close_pos), kept.to_string()))
}

/// Nesting-aware enclosing bracket pair, whole buffer. A cursor sitting on
/// either delimiter selects that pair.
fn find_bracket_pair(text: &str, cursor: usize, open: char, close: char) -> Option<(usize, usize)> {
    if char_at(text, cursor) == Some(open) {
        let close_pos = scan_fwd(text, next_char(text, cursor), open, close)?;
        return Some((cursor, close_pos));
    }
    // A cursor on the closing char falls out naturally: the forward scan
    // matches it at depth 0.
    let open_pos = scan_back(text, cursor, open, close)?;
    let close_pos = scan_fwd(text, cursor, open, close)?;
    Some((open_pos, close_pos))
}

/// Nearest unmatched `open` strictly before `upto`.
fn scan_back(text: &str, upto: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text[..upto].char_indices().rev() {
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

/// Nearest unmatched `close` at or after `from`.
fn scan_fwd(text: &str, from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text[from..].char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some(from + i);
            }
            depth -= 1;
        }
    }
    None
}

/// Quote pair on the cursor's line, like Vim's quote text objects: quotes
/// pair up from the line start, and the pair chosen is the first whose close
/// is at or after the cursor (so a cursor before a string reaches forward,
/// and one on either quote selects that string).
fn find_quote_pair(text: &str, cursor: usize, q: char) -> Option<(usize, usize)> {
    let start = line_start(text, cursor);
    let end = line_end(text, cursor);
    let mut quotes = text[start..end]
        .char_indices()
        .filter(|&(_, ch)| ch == q)
        .map(|(i, _)| start + i);
    while let (Some(open), Some(close)) = (quotes.next(), quotes.next()) {
        if cursor <= close {
            return Some((open, close));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected `(buffer after, cursor)`; `None` = the command fails.
    type Want<'a> = Option<(&'a str, usize)>;

    fn post(text: &str, e: &EditOp) -> String {
        format!(
            "{}{}{}",
            &text[..e.range.start],
            e.text,
            &text[e.range.end..]
        )
    }

    #[test]
    fn add_cases() {
        // (text, range, char) -> (buffer after, cursor)
        let cases: &[(&str, Range<usize>, char, &str, usize)] = &[
            ("hello", 0..5, '"', "\"hello\"", 0),
            ("hello", 0..5, '\'', "'hello'", 0),
            ("hello", 0..5, '`', "`hello`", 0),
            // Opening chars pad; closing chars and aliases are tight.
            ("x", 0..1, '(', "( x )", 0),
            ("x", 0..1, ')', "(x)", 0),
            ("x", 0..1, 'b', "(x)", 0),
            ("x", 0..1, '{', "{ x }", 0),
            ("x", 0..1, '}', "{x}", 0),
            ("x", 0..1, 'B', "{x}", 0),
            ("x", 0..1, '[', "[ x ]", 0),
            ("x", 0..1, ']', "[x]", 0),
            ("x", 0..1, 'r', "[x]", 0),
            ("x", 0..1, '<', "< x >", 0),
            ("x", 0..1, '>', "<x>", 0),
            ("x", 0..1, 'a', "<x>", 0),
            ("say hello now", 4..9, ')', "say (hello) now", 4),
            // Multi-line range (visual S / linewise targets).
            ("ab\ncd", 0..5, '}', "{ab\ncd}", 0),
            // Multibyte content and surroundings.
            ("éé ✓𝄞", 5..12, '{', "éé { ✓𝄞 }", 5),
            ("", 0..0, '"', "\"\"", 0),
        ];
        for (text, range, c, want, cur) in cases {
            let e = add(text, range.clone(), *c)
                .unwrap_or_else(|| panic!("add {c:?} on {text:?} failed"));
            assert_eq!(post(text, &e), *want, "add {c:?} on {text:?}");
            assert_eq!(e.cursor, *cur, "add {c:?} on {text:?} cursor");
        }
        // Unknown pair chars.
        assert_eq!(add("x", 0..1, 'z'), None);
        assert_eq!(add("x", 0..1, 'w'), None);
        // Out-of-bounds / mid-char range floors instead of panicking.
        let e = add("é", 1..5, '(').unwrap();
        assert_eq!(post("é", &e), "( é )");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_cases() {
        // (text, cursor, char) -> Some((buffer after, cursor)) or None
        let cases: &[(&str, usize, char, Want)] = &[
            // Quotes: enclosed, on either delimiter, or before the string.
            ("say \"hi\" now", 5, '"', Some(("say hi now", 4))),
            ("say \"hi\" now", 4, '"', Some(("say hi now", 4))),
            ("say \"hi\" now", 7, '"', Some(("say hi now", 4))),
            ("say \"hi\" now", 0, '"', Some(("say hi now", 4))),
            ("say \"hi\" now", 9, '"', None), // past the pair
            // Quote content is kept verbatim (no padding rules).
            ("\" hi \"", 2, '"', Some((" hi ", 0))),
            // Quotes pair line-locally.
            ("\"a\nb\"", 0, '"', None),
            ("x\n\"a\"\ny", 3, '"', Some(("x\na\ny", 2))),
            // Cursor on a quote pairs by parity from line start.
            ("\"a\"b\"", 2, '"', Some(("ab\"", 0))),
            ("\"a\"b\"", 4, '"', None), // trailing unpaired quote
            // Brackets: opening target eats one inner blank per side...
            ("a ( b ) c", 4, '(', Some(("a b c", 2))),
            ("a (  b  ) c", 5, '(', Some(("a  b  c", 2))),
            // ...closing targets and aliases keep the padding.
            ("a ( b ) c", 4, ')', Some(("a  b  c", 2))),
            ("a ( b ) c", 4, 'b', Some(("a  b  c", 2))),
            ("a {x} c", 3, 'B', Some(("a x c", 2))),
            ("a [x] c", 3, 'r', Some(("a x c", 2))),
            ("a <x> c", 3, 'a', Some(("a x c", 2))),
            // Nesting: nearest enclosing pair; on a delimiter, that pair.
            ("((x))", 2, ')', Some(("(x)", 1))),
            ("((x))", 0, ')', Some(("(x)", 0))),
            ("((x))", 3, ')', Some(("(x)", 1))),
            ("((x))", 4, ')', Some(("(x)", 0))),
            // Brackets span lines.
            ("{a\nb}", 3, 'B', Some(("a\nb", 0))),
            ("{a\nb}", 3, '{', Some(("a\nb", 0))),
            // Empty pair: vim-surround bails; a lone blank inside survives.
            ("()", 0, '(', None),
            ("()", 0, ')', None),
            ("\"\"", 0, '"', None),
            ("( )", 1, '(', Some(("", 0))),
            ("( )", 1, ')', Some((" ", 0))),
            ("x( )y", 2, '(', Some(("xy", 1))),
            // No enclosing pair.
            ("abc", 1, '(', None),
            ("(x) y", 4, ')', None),
            ("", 0, '"', None),
            ("", 0, '(', None),
            // Multibyte.
            ("x \"é✓\" y", 5, '"', Some(("x é✓ y", 2))),
            ("(𝄞)", 1, '(', Some(("𝄞", 0))),
            ("(𝄞)", 3, ')', Some(("𝄞", 0))), // mid-char cursor floors
        ];
        for (text, cursor, c, want) in cases {
            let got = delete(text, *cursor, *c).map(|e| (post(text, &e), e.cursor));
            let want = want.map(|(t, c)| (t.to_string(), c));
            assert_eq!(got, want, "ds{c} on {text:?} at {cursor}");
        }
    }

    #[test]
    fn change_cases() {
        // (text, cursor, from, to) -> Some((buffer after, cursor)) or None
        let cases: &[(&str, usize, char, char, Want)] = &[
            // cs)( adds padding; cs(" strips the old padding, adds tight.
            ("(x)", 1, ')', '(', Some(("( x )", 0))),
            ("( x )", 2, '(', '"', Some(("\"x\"", 0))),
            ("( x )", 2, ')', '"', Some(("\" x \"", 0))),
            ("\"hi\"", 1, '"', '\'', Some(("'hi'", 0))),
            ("say \"hi\"", 5, '"', 'b', Some(("say (hi)", 4))),
            ("(x)", 1, 'b', ']', Some(("[x]", 0))),
            ("( x )", 2, '(', '<', Some(("< x >", 0))),
            // Multibyte.
            ("`é✓`", 1, '`', '<', Some(("< é✓ >", 0))),
            // Unknown chars and empty pairs fail.
            ("\"hi\"", 1, '"', 'z', None),
            ("(x)", 1, 'z', ')', None),
            ("()", 0, '(', '"', None),
            ("abc", 1, '(', ')', None),
        ];
        for (text, cursor, from, to, want) in cases {
            let got = change(text, *cursor, *from, *to).map(|e| (post(text, &e), e.cursor));
            let want = want.map(|(t, c)| (t.to_string(), c));
            assert_eq!(got, want, "cs{from}{to} on {text:?} at {cursor}");
        }
    }
}

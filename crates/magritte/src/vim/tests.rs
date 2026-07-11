//! End-to-end engine tests: feed key sequences through [`VimState`] and
//! assert the resulting buffer, cursor, and mode. The tables encode observable
//! Vim behavior (verified against `:help motion.txt`, `:help text-objects`,
//! `:help word-motions`, and vim-surround), so they are the spec the engine
//! and its leaf modules must match.

use super::*;

const N: Mode = Mode::Normal;
const I: Mode = Mode::Insert;
const V: Mode = Mode::Visual {
    kind: VisualKind::Char,
};
const VL: Mode = Mode::Visual {
    kind: VisualKind::Line,
};
const VB: Mode = Mode::Visual {
    kind: VisualKind::Block,
};

/// A headless stand-in for the app layer: applies the engine's actions to a
/// plain string buffer, mirroring `apply.rs` (Insert-mode keys are typed
/// straight into the buffer; only `Esc` reaches the engine there).
struct Buf {
    text: String,
    cursor: usize,
    clipboard: Option<String>,
    vim: VimState,
    log: Vec<Action>,
}

impl Buf {
    fn new(text: &str, cursor: usize) -> Buf {
        assert!(text.is_char_boundary(cursor), "bad test setup");
        Buf {
            text: text.to_string(),
            cursor,
            clipboard: None,
            vim: VimState::new(),
            log: Vec::new(),
        }
    }

    fn feed(&mut self, keys: &str) {
        for key in parse_keys(keys) {
            self.feed_key(key);
        }
    }

    fn feed_key(&mut self, key: Key) {
        if self.vim.in_insert() && key != Key::Escape {
            self.type_insert(key);
            return;
        }
        let actions = self.vim.handle_key(&self.text, self.cursor, key);
        for action in actions {
            self.apply(action);
        }
    }

    /// What the app's `InputState` would do with a key while in Insert mode.
    fn type_insert(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            Key::Enter => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            Key::Backspace if self.cursor > 0 => {
                let at = prev_char(&self.text, self.cursor);
                self.text.replace_range(at..self.cursor, "");
                self.cursor = at;
            }
            _ => {}
        }
    }

    fn apply(&mut self, action: Action) {
        match &action {
            Action::MoveCursor(pos) => {
                self.assert_boundary(*pos, "MoveCursor");
                self.cursor = *pos;
            }
            Action::Edit(op) => {
                assert!(
                    op.range.start <= op.range.end && op.range.end <= self.text.len(),
                    "Edit range {:?} out of bounds (len {})",
                    op.range,
                    self.text.len()
                );
                self.assert_boundary(op.range.start, "Edit range start");
                self.assert_boundary(op.range.end, "Edit range end");
                self.text.replace_range(op.range.clone(), &op.text);
                self.assert_boundary(op.cursor, "Edit cursor");
                self.cursor = op.cursor;
            }
            Action::Yank(s) => self.clipboard = Some(s.clone()),
            // `.`: replay like the app layer does — feed the recorded keys,
            // re-type the captured Insert text, close with Esc.
            Action::Repeat(count) => {
                if let Some((keys, typed)) = self.vim.begin_repeat(*count) {
                    for k in keys {
                        self.feed_key(k);
                    }
                    if self.vim.in_insert() {
                        for c in typed.chars() {
                            self.type_insert(Key::Char(c));
                        }
                        self.feed_key(Key::Escape);
                    }
                    self.vim.end_repeat();
                }
            }
            // Scroll only moves the viewport, which the harness doesn't model.
            Action::Commit
            | Action::Quit { .. }
            | Action::ReflowRange(_)
            | Action::ReflowRangeKeep(_)
            | Action::Help
            | Action::Scroll(_)
            | Action::Error(_)
            | Action::Beep => {}
        }
        self.log.push(action);
    }

    fn assert_boundary(&self, pos: usize, what: &str) {
        assert!(
            pos <= self.text.len() && self.text.is_char_boundary(pos),
            "{what} {pos} not a char boundary of {:?}",
            self.text
        );
    }

    /// A bell rang: a plain Beep or an echoed `:` error (both flash the
    /// indicator in the app).
    fn beeped(&self) -> bool {
        self.log
            .iter()
            .any(|a| matches!(a, Action::Beep | Action::Error(_)))
    }

    fn error(&self) -> Option<&str> {
        self.log.iter().rev().find_map(|a| match a {
            Action::Error(m) => Some(m.as_str()),
            _ => None,
        })
    }
}

/// Split a `"he|llo"` spec into (text, cursor). The bar sits before the char
/// the cursor is on.
fn parse_spec(spec: &str) -> (String, usize) {
    let at = spec.find('|').expect("spec needs a | cursor marker");
    (spec.replace('|', ""), at)
}

fn show(text: &str, cursor: usize) -> String {
    let mut s = text.to_string();
    s.insert(cursor, '|');
    s
}

/// Tokenize a key string: plain chars, with `<esc>` `<cr>` `<bs>` `<space>`
/// `<tab>` `<left>` `<right>` `<up>` `<down>` `<c-x>` escapes.
fn parse_keys(s: &str) -> Vec<Key> {
    let mut out = Vec::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '<' {
            out.push(Key::Char(c));
            continue;
        }
        let mut name = String::new();
        for n in it.by_ref() {
            if n == '>' {
                break;
            }
            name.push(n);
        }
        out.push(match name.as_str() {
            "esc" => Key::Escape,
            "cr" => Key::Enter,
            "lt" => Key::Char('<'),
            "bs" => Key::Backspace,
            "space" => Key::Char(' '),
            "tab" => Key::Char('\t'),
            "left" => Key::Left,
            "right" => Key::Right,
            "up" => Key::Up,
            "down" => Key::Down,
            n if n.len() == 3 && n.starts_with("c-") => Key::Ctrl(n.chars().nth(2).unwrap()),
            other => panic!("unknown key <{other}>"),
        });
    }
    out
}

#[track_caller]
fn run(spec: &str, keys: &str) -> Buf {
    let (text, cursor) = parse_spec(spec);
    let mut buf = Buf::new(&text, cursor);
    buf.feed(keys);
    buf
}

/// `beep`: Some(true) = at least one Beep required, Some(false) = none
/// allowed, None = don't care.
#[track_caller]
fn check_full(spec: &str, keys: &str, want: &str, mode: Mode, beep: Option<bool>) -> Buf {
    let buf = run(spec, keys);
    let got = show(&buf.text, buf.cursor);
    assert_eq!(got, want, "{keys:?} on {spec:?}");
    assert_eq!(buf.vim.mode(), mode, "{keys:?} on {spec:?}: wrong mode");
    match beep {
        Some(true) => assert!(buf.beeped(), "{keys:?} on {spec:?}: expected a beep"),
        Some(false) => assert!(!buf.beeped(), "{keys:?} on {spec:?}: unexpected beep"),
        None => {}
    }
    buf
}

/// Ends in Normal mode, no beep along the way.
#[track_caller]
fn check(spec: &str, keys: &str, want: &str) {
    check_full(spec, keys, want, N, Some(false));
}

/// Ends in Insert mode, no beep.
#[track_caller]
fn check_i(spec: &str, keys: &str, want: &str) {
    check_full(spec, keys, want, I, Some(false));
}

/// Ends in Normal mode; at least one beep happened (usually with
/// `want == spec`: the failed command was a no-op).
#[track_caller]
fn check_beep(spec: &str, keys: &str, want: &str) {
    check_full(spec, keys, want, N, Some(true));
}

/// Ends in Normal mode; beep or not unspecified.
#[track_caller]
fn check_any(spec: &str, keys: &str, want: &str) {
    check_full(spec, keys, want, N, None);
}

/// Result plus the system-clipboard mirror.
#[track_caller]
fn check_clip(spec: &str, keys: &str, want: &str, clip: &str) {
    let buf = check_full(spec, keys, want, N, Some(false));
    assert_eq!(
        buf.clipboard.as_deref(),
        Some(clip),
        "{keys:?} on {spec:?}: wrong clipboard"
    );
}

// --- Plain motions -------------------------------------------------------

#[test]
fn motions_h_l() {
    for (spec, keys, want) in [
        ("a|bc", "h", "|abc"),
        ("ab|c", "h", "a|bc"),
        ("ab|c", "10h", "|abc"),
        ("a|bc", "l", "ab|c"),
        ("|abc", "10l", "ab|c"), // partial count still moves
        ("|abc", "3l", "ab|c"),
        ("ab|c\nd", "l", "ab|c\nd"), // `l` never wraps; see beep below
        ("a|bc", "<left>", "|abc"),
        ("a|bc", "<right>", "ab|c"),
    ] {
        check_any(spec, keys, want);
    }
    check_beep("|abc", "h", "|abc"); // col 0
    check_beep("ab\n|cd", "h", "ab\n|cd");
    check_beep("ab|c", "l", "ab|c"); // last char of the line
    check_beep("a|b\ncd", "l", "a|b\ncd");
    check_beep("|\nab", "l", "|\nab"); // empty line
}

#[test]
fn motions_j_k_sticky_column() {
    for (spec, keys, want) in [
        ("a|b\ncd", "j", "ab\nc|d"),
        ("ab\nc|d", "k", "a|b\ncd"),
        ("|a\nb\nc", "2j", "a\nb\n|c"),
        ("a|b\ncd", "<down>", "ab\nc|d"),
        ("ab\nc|d", "<up>", "a|b\ncd"),
        ("a|b\ncd", "<c-n>", "ab\nc|d"),
        ("ab\nc|d", "<c-p>", "a|b\ncd"),
        // Desired column survives a shorter line in between.
        ("abcd|ef\nxy\nlmnopq", "j", "abcdef\nx|y\nlmnopq"),
        ("abcd|ef\nxy\nlmnopq", "jj", "abcdef\nxy\nlmno|pq"),
        // `$` pins the desired column to the line end.
        ("|ab\nlmnop", "$j", "ab\nlmno|p"),
        ("|abcdef\nxy\nlmnopq", "$jj", "abcdef\nxy\nlmnop|q"),
        // A horizontal move resets the desired column.
        ("ab|cd\nwxyz", "hj", "abcd\nw|xyz"),
        // Landing on an empty line, then back out.
        ("a|b\n\ncd", "j", "ab\n|\ncd"),
        ("a|b\n\ncd", "jj", "ab\n\nc|d"),
    ] {
        check(spec, keys, want);
    }
    check_beep("ab\n|cd", "j", "ab\n|cd"); // last line
    check_beep("|ab\ncd", "k", "|ab\ncd"); // first line
                                           // A count that overshoots fails the whole motion (Vim beeps, no move).
    check_beep("|a\nb\nc", "5j", "|a\nb\nc");
    check_beep("a\nb\n|c", "5k", "a\nb\n|c");
    check_beep("|a\nb\nc", "d5j", "|a\nb\nc");
}

#[test]
fn motions_line_start_end() {
    for (spec, keys, want) in [
        ("ab|c", "0", "|abc"),
        ("ab\ncd|e", "0", "ab\n|cde"),
        ("  a|b", "^", "  |ab"),
        ("a|b  ", "^", "|ab  "),
        ("|   ", "^", "  | "), // all-blank line: last char
        ("|   ", "$", "  | "),
        ("a|bc", "$", "ab|c"),
        ("|abc\ndefg", "2$", "abc\ndef|g"),
        ("|\nab", "$", "|\nab"), // empty line: stays, no beep
        ("ab\n|", "$", "ab\n|"),
    ] {
        check(spec, keys, want);
    }
}

#[test]
fn motions_words() {
    for (spec, keys, want) in [
        ("|hello world", "w", "hello |world"),
        ("hel|lo world", "w", "hello |world"),
        ("|foo.bar", "w", "foo|.bar"),
        ("foo|.bar", "w", "foo.|bar"),
        ("|ab cd ef", "2w", "ab cd |ef"),
        ("|ab cd", "5w", "ab c|d"), // count over: end of the last word
        ("foo |bar", "w", "foo ba|r"),
        ("|ab\n\ncd", "w", "ab\n|\ncd"), // empty line is a word
        ("|ab\n\ncd", "2w", "ab\n\n|cd"),
        ("ab |\ncd", "w", "ab \n|cd"),
        ("|foo.bar baz", "W", "foo.bar |baz"),
        ("hello |world", "b", "|hello world"),
        ("hel|lo", "b", "|hello"),
        ("foo.|bar", "b", "foo|.bar"),
        ("a-b |c", "b", "a-|b c"),
        ("ab\n|cd", "b", "|ab\ncd"),
        ("ab\n\n|cd", "b", "ab\n|\ncd"),
        ("foo.bar |baz", "B", "|foo.bar baz"),
        ("|ab cd", "e", "a|b cd"),
        ("a|b cd", "e", "ab c|d"),
        ("|foo.bar", "e", "fo|o.bar"),
        ("|foo.bar baz", "2e", "foo|.bar baz"),
        ("|ab cd", "3e", "ab c|d"),      // partial count
        ("a|b\n\ncd", "e", "ab\n\nc|d"), // e skips empty lines
        ("|foo.bar baz", "E", "foo.ba|r baz"),
        ("ab \n|cd", "b", "|ab \ncd"),
    ] {
        check(spec, keys, want);
    }
    check_beep("foo ba|r", "w", "foo ba|r"); // last char of the buffer
    check_beep("|abc", "b", "|abc");
    check_beep("ab c|d", "e", "ab c|d");
}

#[test]
fn motions_goto_line() {
    for (spec, keys, want) in [
        ("  ab\nc|d", "gg", "  |ab\ncd"),
        ("a|b\n  cd", "G", "ab\n  |cd"),
        ("|ab\n  cd\nef", "2G", "ab\n  |cd\nef"),
        ("ab\n  cd\ne|f", "1G", "|ab\n  cd\nef"),
        ("|ab\ncd", "100G", "ab\n|cd"), // past the end: last line
        ("|ab\ncd", "2gg", "ab\n|cd"),
        ("a|b\n", "G", "ab\n|"), // empty last line
        // A linewise jump resets the desired column for j/k.
        ("abc|d\nef\nghij", "Gkk", "|abcd\nef\nghij"),
    ] {
        check(spec, keys, want);
    }
}

#[test]
fn motions_find_till() {
    for (spec, keys, want) in [
        ("|hello world", "fo", "hell|o world"),
        ("|hello world", "2fo", "hello w|orld"),
        ("|hello world", "fo;", "hello w|orld"),
        ("|hello", "fl;", "hel|lo"),
        ("hello w|orld", "Fo", "hell|o world"),
        ("|hello world", "to", "hel|lo world"),
        // `;` after `t` skips a target the cursor is already adjacent to.
        ("|hello world", "to;", "hello |world"),
        ("|axax", "tx;", "ax|ax"),
        ("hello wo|rld", "To", "hello wo|rld"),
        ("hello wor|ld", "To", "hello wo|rld"),
        ("|axbxcx", "fx2;", "axbxc|x"),
        // (a bare `,` is now the with-editor leader; with an operator
        // pending it still reverses the find, tested below) // `,` reverses the direction
        // A bare `,` pends as the leader; a non-leader key after it runs the
        // deferred reverse-find first.
        ("hello w|orld", "Fo,l", "hello wo|rld"),
        ("|axbxcxd", "2tx", "ax|bxcxd"),
        // A count on `;` suppresses the adjacent-target skip (probed: `tx`
        // then `2;` stops before the 2nd x, not the 3rd).
        ("|axbxcxd", "tx2;", "ax|bxcxd"),
        ("|axbxcxdxe", "tx3;", "axbx|cxdxe"),
        ("axbxcx|d", "Tx2;", "axbx|cxd"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|abc", "fz", "|abc"); // not on the line
    check_beep("ab|c\nzd", "fz", "ab|c\nzd"); // never crosses lines
    check_beep("|abc", ";", "|abc"); // no previous find
    check_beep("|abc", "d,", "|abc"); // (bare `,` pends as the leader)
}

#[test]
fn motions_paragraph_percent() {
    for (spec, keys, want) in [
        ("|a\nb\n\nc", "}", "a\nb\n|\nc"),
        ("a\nb\n\n|c", "}", "a\nb\n\n|c"), // end of buffer: clamps to last char
        ("|a\nb\n\n\nc", "}", "a\nb\n|\n\nc"),
        ("a\nb\n\n|c", "{", "a\nb\n|\nc"),
        ("a\n|b\n\nc", "{", "|a\nb\n\nc"),
        ("|a(b)c", "%", "a(b|)c"),
        ("a(b|)c", "%", "a|(b)c"),
        ("(|(a))", "%", "((a|))"),
        ("a(|b)c", "%", "a|(b)c"), // nearest bracket at/after the cursor
        ("|a[b]c", "%", "a[b|]c"),
        ("|a{b}c", "%", "a{b|}c"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|a", "{", "|a");
    check_beep("|abc", "%", "|abc");
    check_beep("a|(bc", "%", "a|(bc"); // unbalanced
    check_beep("|abc)d", "%", "|abc)d");
}

#[test]
fn motions_line_starts() {
    for (spec, keys, want) in [
        ("a|b\n  cd", "<cr>", "ab\n  |cd"),
        ("a|b\n  cd", "+", "ab\n  |cd"),
        ("|ab\ncd\n  ef", "2<cr>", "ab\ncd\n  |ef"),
        ("  ab\nc|d", "-", "  |ab\ncd"),
        ("ab\ncd\n e|f", "2-", "|ab\ncd\n ef"),
    ] {
        check(spec, keys, want);
    }
    check_beep("ab\n|cd", "<cr>", "ab\n|cd"); // last line
    check_beep("a|b\ncd", "-", "a|b\ncd"); // first line
}

#[test]
fn motions_space_backspace() {
    for (spec, keys, want) in [
        ("|ab\ncd", "<space>", "a|b\ncd"),
        ("a|b\ncd", "<space>", "ab\n|cd"), // crosses the line end
        ("|ab\ncd", "2<space>", "ab\n|cd"),
        ("|ab\ncd", "3<space>", "ab\nc|d"),
        ("|a\n\nb", "<space>", "a\n|\nb"), // an empty line is one position
        ("|a\n\nb", "2<space>", "a\n\n|b"),
        ("ab\n|cd", "<bs>", "a|b\ncd"),
        ("ab\nc|d", "2<bs>", "a|b\ncd"),
        ("a\n\n|b", "<bs>", "a\n|\nb"),
        ("a\n|\nb", "<bs>", "|a\n\nb"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|ab", "<bs>", "|ab");
    check_beep("a|b", "<space>", "a|b"); // end of buffer
}

// --- Operators -----------------------------------------------------------

#[test]
fn operators_dw() {
    for (spec, keys, want) in [
        ("|foo bar", "dw", "|bar"),
        ("f|oo bar", "dw", "f|bar"),
        // Last word of a line: `dw` stops at the word end and keeps the
        // newline (`:help word-motions` operator special case).
        ("foo |bar\nbaz", "dw", "foo| \nbaz"),
        ("foo |bar", "dw", "foo| "),
        ("ab |\ncd", "dw", "a|b\ncd"), // from trailing blanks: eats the blanks only
        ("|  foo\nbar", "dw", "|foo\nbar"), // from the indent: eats the indent only
        // The word-motion special case preempts `exclusive-linewise`: `dw`
        // stays charwise even when it starts at/before the first non-blank.
        ("|foo\n\nbar", "dw", "|\n\nbar"),
        ("|  foo\nbar", "d2w", "|\nbar"),
        // d2w through the end of the buffer.
        ("ab |cd\nef", "d2w", "ab| "),
        // `db` (no forward special case) does turn linewise from the indent
        // (`:help exclusive-linewise`).
        ("  foo\n|bar", "db", "|bar"),
        ("abc |def", "db", "|def"),
        ("abc d|ef", "db", "abc |ef"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|", "dw", "|"); // empty buffer
}

#[test]
fn operators_cw_ce() {
    // `cw` on a non-blank acts like `ce` (`:help cw`).
    check_i("|foo bar", "cw", "| bar");
    check_i("f|oo bar", "cw", "f| bar");
    check_i("a|b cd", "cw", "a| cd");
    check_i("foo|.bar", "cw", "foo|bar");
    // ...but on a blank it acts like `dw` (deletes just the blanks).
    check_i("foo| bar", "cw", "foo|bar");
    check_i("foo|   bar", "cw", "foo|bar");
    check_i("|foo bar", "c2w", "|"); // with a count, cw acts like c2e
    check_i("|foo.bar", "cW", "|");
}

#[test]
fn operators_line_end() {
    check("a|bcdef", "d$", "|a");
    check("a|bcdef", "D", "|a");
    check("|abc", "d$", "|");
    check("a|bc\nd", "D", "|a\nd");
    check_i("ab|cdef", "C", "ab|");
    check_i("|abc", "C", "|");
    check("a|bc\ndefg", "d2$", "|a"); // $ with a count: end of the next line
    check_any("ab\n|\ncd", "D", "ab\n|\ncd"); // empty line: nothing to delete
}

#[test]
fn operators_linewise() {
    for (spec, keys, want) in [
        ("a|b\ncd\nef", "dd", "|cd\nef"),
        ("ab\n|cd", "dd", "|ab"), // last line: eats the newline before it
        ("|ab", "dd", "|"),
        ("|a\nb\nc", "2dd", "|c"),
        ("a\n|b\nc", "5dd", "|a"),            // partial count
        ("|ab\n  cd\nef", "dd", "  |cd\nef"), // cursor at first non-blank
        ("|a\nb\nc", "dj", "|c"),
        ("a\n|b\nc", "dk", "|c"),
        ("ab\n|cd\nef", "dG", "|ab"),
        ("|ab\ncd", "dG", "|"),
        ("ab\n|cd\nef", "dgg", "|ef"),
        ("ab\ncd\ne|f", "d2G", "|ab"),
        ("|ab\ncd\nef", "d<cr>", "|ef"),
    ] {
        check(spec, keys, want);
    }
    check_beep("a\n|b", "dj", "a\n|b");
    check_beep("|a\nb", "dk", "|a\nb");
    check_beep("|", "dd", "|");

    // cc/S clear the lines but keep the trailing newline.
    check_i("ab\nc|d\nef", "cc", "ab\n|\nef");
    check_i("|  ab", "cc", "|");
    check_i("|\nb", "cc", "|\nb");
    check_i("|ab\ncd", "S", "|\ncd");
    check_i("ab\n|cd\nef", "cG", "ab\n|");
    check_i("a\nb\nc|d", "2S", "a\nb\n|");
}

#[test]
fn operators_paragraph() {
    // d} from at/before the first non-blank turns linewise, keeping the
    // separating empty line (`:help exclusive-linewise`).
    check("|ab\ncd\n\nef", "d}", "|\nef");
    // From mid-line it is charwise and the newline before the empty line
    // survives (the exclusive end backs up to the previous line).
    check("a|b\n\ncd", "d}", "|a\n\ncd");
    check("ab\n\nc|d", "d{", "ab\n|d");
    check("a|b\n\ncd", "y}", "a|b\n\ncd");
    // Backward yank moves the cursor to the start of the yanked region.
    check("ab\n\nc|d", "y{", "ab\n|\ncd");
}

#[test]
fn operators_counts_and_find() {
    // Counts before and after the operator multiply.
    check("|one two three four five six seven", "2d3w", "|seven");
    check("|one two three four five six seven", "d6w", "|seven");
    check("|axbxc", "d2fx", "|c");
    check("|abcxdef", "dfx", "|def"); // f is inclusive
    check("|abcxdef", "dtx", "|xdef");
    check("abc|xdef", "dFa", "|xdef");
    check("ab,c|d", "dT,", "ab,|d");
    // A failed motion beeps and cancels the pending operator: the trailing
    // `x` runs as a fresh command.
    let buf = check_full("|abc", "dfzx", "|bc", N, Some(true));
    assert!(buf.beeped());
    check_full("|abc", "dhx", "|bc", N, Some(true));
    check_full("|abc", "d<esc>x", "|bc", N, Some(false)); // Esc cancels silently
    check_beep("|abc", "dz", "|abc"); // no such motion
}

#[test]
fn operators_yank() {
    check_clip("abc |def", "yw", "abc |def", "def");
    check_clip("|abc def", "yw", "|abc def", "abc ");
    check_clip("abc de|f", "ye", "abc de|f", "f");
    check_clip("a|bc", "y$", "a|bc", "bc");
    check_clip("|ab\ncd", "yy", "|ab\ncd", "ab\n");
    check_clip("|ab\ncd", "2yy", "|ab\ncd", "ab\ncd\n");
    check_clip("|ab\ncd", "Y", "|ab\ncd", "ab\n");
    check_clip("a|b\ncd", "yj", "a|b\ncd", "ab\ncd\n");
    // Linewise yank of the current line keeps the cursor where it was.
    check("ab|cd", "yy", "ab|cd");
    // Backward charwise yank moves to the start of the yank.
    check_clip("abc |def", "yb", "|abc def", "abc ");
    check_clip("f|oo bar", "yiw", "|foo bar", "foo");
    // d and x mirror into the clipboard too.
    check_clip("|foo bar", "dw", "|bar", "foo ");
    check_clip("|ab", "x", "|b", "a");
}

// --- Text objects --------------------------------------------------------

#[test]
fn objects_words() {
    for (spec, keys, want) in [
        ("f|oo bar", "diw", "| bar"),
        ("|foo bar", "diw", "| bar"),
        ("fo|o bar", "diw", "| bar"),
        ("foo|   bar", "diw", "foo|bar"), // on blanks: just the blanks
        ("foo|..bar", "diw", "foo|bar"),  // punct run is a word
        ("f|oo bar", "daw", "|bar"),      // trailing blanks included
        ("foo b|ar", "daw", "fo|o"),      // no trailing: leading blanks
        ("foo|.bar", "daw", "foo|bar"),
        ("foo |  bar", "daw", "fo|o"), // on blanks: blanks + next word
        ("a |bb ccc dd", "2daw", "a |dd"),
        ("|a b c", "d2iw", "|b c"),
        ("|a b c", "d3iw", "| c"),
    ] {
        check(spec, keys, want);
    }
    check_i("f|oo bar", "ciw", "| bar");
    check_i("foo |  bar", "ciw", "foo|bar");
    check_beep("|", "diw", "|");
    // On an empty line there is no word: nothing happens.
    check_any("ab\n|\ncd", "diw", "ab\n|\ncd");
}

#[test]
fn objects_quotes() {
    for (spec, keys, want) in [
        ("a \"b|c\" d", "di\"", "a \"|\" d"),
        ("a \"b|c\" d", "da\"", "a |d"), // a" eats trailing blanks
        ("a \"b|c\"", "da\"", "|a"),     // ...or leading ones if no trailing
        ("a |\"bc\" d", "di\"", "a \"|\" d"), // on the opening quote
        ("a \"bc|\" d", "di\"", "a \"|\" d"), // on the closing quote
        ("|a \"bc\" d", "di\"", "a \"|\" d"), // before the pair: seeks forward
        ("a 'b|c' d", "di'", "a '|' d"),
        ("a `b|c` d", "di`", "a `|` d"),
        ("x 'a|b' 'cd'", "di'", "x '|' 'cd'"),
    ] {
        check(spec, keys, want);
    }
    check_i("a \"b|c\" d", "ci\"", "a \"|\" d");
    check_i("a 'b|c' d", "ci'", "a '|' d");
    // Empty pair: the cursor moves inside; nothing is deleted.
    check("a |\"\" d", "di\"", "a \"|\" d");
    check_beep("a|bc", "di\"", "a|bc"); // no quotes on the line
    check_beep("ab\n|cd", "di(", "ab\n|cd");
}

#[test]
fn objects_brackets() {
    for (spec, keys, want) in [
        ("a(b|c)d", "di(", "a(|)d"),
        ("a(b|c)d", "di)", "a(|)d"),
        ("a(b|c)d", "dib", "a(|)d"),
        ("a(b|c)d", "da(", "a|d"),
        ("a(b|c)d", "dab", "a|d"),
        ("a|(bc)d", "di(", "a(|)d"), // on the opening bracket
        ("a(bc|)d", "di(", "a(|)d"), // on the closing bracket
        ("a(b(c|d)e)f", "di(", "a(b(|)e)f"),
        ("a(b(c|d)e)f", "2di(", "a(|)f"), // count selects the enclosing pair
        ("a(b(c|d)e)f", "2da(", "a|f"),
        ("|ab (cd)", "di(", "ab (|)"), // before the pair: seeks forward
        ("a{b|c}d", "di{", "a{|}d"),
        ("a{b|c}d", "diB", "a{|}d"),
        ("a{b|c}d", "da}", "a|d"),
        ("a[b|c]d", "di[", "a[|]d"),
        ("a[b|c]d", "di]", "a[|]d"),
        ("a<b|c>d", "di<lt>", "a<|>d"),
        ("a<b|c>d", "da>", "a|d"),
        // Braces on their own lines: the inner lines go entirely.
        ("{\na|b\ncd\n}", "di{", "{\n|}"),
    ] {
        check(spec, keys, want);
    }
    check_i("a(b|c)d", "ci(", "a(|)d");
    check_i("a<b|c>d", "ci<lt>", "a<|>d");
    // A multiline inner block is linewise: change leaves an empty line.
    check_i("{\na|b\ncd\n}", "ci{", "{\n|\n}");
    // Empty pair: cursor moves inside, nothing deleted; change inserts there.
    check("a|()d", "di(", "a(|)d");
    check_i("a|()d", "ci(", "a(|)d");
    check_beep("a|bc", "di(", "a|bc");
    check_beep("a(b|c", "di(", "a(b|c"); // unbalanced
}

// --- Visual mode ---------------------------------------------------------

#[test]
fn visual_mode_switching() {
    check_full("a|bc", "v", "a|bc", V, Some(false));
    check_full("a|bc", "V", "a|bc", VL, Some(false));
    check_full("a|bc", "vV", "a|bc", VL, Some(false));
    check_full("a|bc", "Vv", "a|bc", V, Some(false));
    check_full("a|bc", "vv", "a|bc", N, Some(false));
    check_full("a|bc", "VV", "a|bc", N, Some(false));
    check_full("a|bc", "v<esc>", "a|bc", N, Some(false));
    // Esc leaves the cursor where the visual cursor was.
    check("a|bcd", "vl<esc>", "ab|cd");
}

#[test]
fn visual_charwise() {
    for (spec, keys, want) in [
        ("a|bcd", "vld", "a|d"),
        ("a|bcd", "vlx", "a|d"),
        ("a|bcd", "vd", "a|cd"),         // one-char selection
        ("a|bc", "v$d", "|a"),           // $ is inclusive
        ("a|bc\nd", "v$d", "a|d"),       // ...and takes the newline with it
        ("a|b\ncd", "vjd", "|a"),        // j keeps the column: selection reaches 'd'
        ("abcd|ef\nxy", "vjd", "abc|d"), // j clamps to the short line
        // o swaps the ends; further motion extends the other side.
        ("a|bcd", "vllod", "|a"),
        ("ab|cdef", "vlohd", "a|ef"),
        ("ab\nc|d\nef", "vGd", "ab\nc|f"),
        ("ab\nc|d\nef", "vggd", "|\nef"),
    ] {
        check(spec, keys, want);
    }
    check_full("a|bcd", "vllo", "a|bcd", V, Some(false)); // cursor back at the anchor
    check_i("a|bcd", "vlc", "a|d");
    check_i("a|bcd", "vls", "a|d");
    check_clip("a|bcde", "v2ly", "a|bcde", "bcd"); // y returns to the start
    check_clip("a|bc\nd", "v$y", "a|bc\nd", "bc\n");
}

#[test]
fn visual_linewise() {
    for (spec, keys, want) in [
        ("ab\nc|d\nef", "Vd", "ab\n|ef"),
        ("|ab\ncd\nef", "Vjd", "|ef"),
        ("|ab\ncd\nef", "VGd", "|"),
        ("ab\ncd\ne|f", "Vggd", "|"),
        ("a|b\ncd", "vjVd", "|"), // switching to V reshapes the selection
        ("|ab\ncd", "Vyjp", "ab\ncd\n|ab"),
    ] {
        check(spec, keys, want);
    }
    check_i("ab\nc|d\nef", "Vc", "ab\n|\nef"); // linewise change keeps the newline
    check_clip("a|b\ncd", "Vy", "|ab\ncd", "ab\n"); // cursor to the range start
    check_clip("a|b\ncd", "Vjy", "|ab\ncd", "ab\ncd\n");
}

#[test]
fn visual_objects() {
    check("f|oo bar", "viwd", "| bar");
    check("f|oo bar", "vawd", "|bar");
    check_clip("f|oo bar", "viwy", "|foo bar", "foo");
    check_clip("|foo bar", "viwy", "|foo bar", "foo");
    check_clip("f|oo bar", "vawy", "|foo bar", "foo ");
    check_clip("foo| bar", "viwy", "foo| bar", " ");
    check("a(b|c)d", "vi(d", "a(|)d");
    check("a \"b|c\" d", "va\"d", "a |d");
    check_i("a(b|c)d", "vi(c", "a(|)d");
}

#[test]
fn visual_put_replace_join() {
    // Visual p replaces the selection with the register; the cursor lands on
    // the last pasted character.
    check("|abc def", "ywwvep", "abc abc| ");
    check("|abcd", "ylvlp", "|acd");
    // r fills every selected char; ~ toggles the selection's case.
    check("a|bcd", "vlrz", "a|zzd");
    check("|ab\ncd", "Vrz", "|zz\ncd");
    check("a|bCd", "vl~", "a|Bcd");
    // Visual J joins the selected lines.
    check("|a\nb\nc", "VjJ", "a| b\nc");
    check("|a\nb\nc", "VjjJ", "a b| c");
    check("|a\nb\nc", "vjJ", "a| b\nc");
    check_beep("a|bc", "vp", "a|bc"); // empty register
}

// --- Visual Block mode -----------------------------------------------------
// The expected buffers, cursors, and paddings below were probed against
// Vim 9.2 (`vim -es -u NONE`).

#[test]
fn block_mode_switching() {
    check_full("a|bc", "<c-v>", "a|bc", VB, Some(false));
    check_full("a|bc", "v<c-v>", "a|bc", VB, Some(false));
    check_full("a|bc", "V<c-v>", "a|bc", VB, Some(false));
    check_full("a|bc", "<c-v>v", "a|bc", V, Some(false));
    check_full("a|bc", "<c-v>V", "a|bc", VL, Some(false));
    // The current kind's own key drops back to Normal.
    check_full("a|bc", "<c-v><c-v>", "a|bc", N, Some(false));
    check_full("a|bc", "<c-v><esc>", "a|bc", N, Some(false));
    // Esc leaves the cursor where the block cursor was.
    check("a|bcd\nefgh", "<c-v>jl<esc>", "abcd\nef|gh");
}

#[test]
fn block_ranges_geometry() {
    // One byte range per covered line; a line shorter than the left column
    // yields an empty range at its end.
    let buf = run("|abcdef\nab\nabcdef", "llll<c-v>jj");
    assert_eq!(
        buf.vim.block_ranges(&buf.text, buf.cursor),
        Some(vec![4..5, 9..9, 14..15])
    );
    // The cursor corner on a short line sits one past its last char.
    let buf = run("|abcdef\nabc\nx", "llll<c-v>j");
    assert_eq!(
        buf.vim.block_ranges(&buf.text, buf.cursor),
        Some(vec![3..5, 10..10])
    );
    // `$` extends every line's range to its end.
    let buf = run("|abcdef\nab\nabcdef", "l<c-v>jj$");
    assert_eq!(
        buf.vim.block_ranges(&buf.text, buf.cursor),
        Some(vec![1..6, 8..9, 11..16])
    );
    // Not blockwise: no ranges.
    let buf = run("a|bc", "v");
    assert_eq!(buf.vim.block_ranges(&buf.text, buf.cursor), None);
}

#[test]
fn block_delete() {
    for (spec, keys, want) in [
        // The rectangle between the anchor and the cursor, as one edit;
        // cursor at the block's top-left.
        (
            "|abcdef\nabcdef\nabcdef",
            "ll<c-v>jjld",
            "ab|ef\nabef\nabef",
        ),
        ("|abcdef\nabcdef", "l<c-v>jlx", "a|def\nadef"),
        // Lines shorter than the left column are untouched.
        ("|abcdef\nab\nabcdef", "llll<c-v>jjd", "abcd|f\nab\nabcdf"),
        // The cursor corner on a short line sits one past its last char.
        ("|abcdef\nab\nabcdef", "llll<c-v>jd", "ab|f\nab\nabcdef"),
        ("|abcdef\nabc\nx", "llll<c-v>jd", "abc|f\nabc\nx"),
        ("|abcdef\na\nx", "llll<c-v>jd", "a|f\na\nx"),
        // A line ending inside the block loses its tail.
        ("|abcdef\nabc\nabcdef", "l<c-v>jj3ld", "a|f\na\naf"),
        // `$` extends to each line's end — even ones past the corners.
        ("|abcdef\nab\nabcdef", "ll<c-v>jj$d", "a|b\nab\nab"),
        ("|abcdef\nabcdefgh", "l<c-v>$jd", "|a\na"),
        // A column-setting motion drops the $-extension (v_b_dollar).
        ("|abcdef\nabcdef", "l<c-v>j$hd", "a|f\naf"),
        ("|abcdef\nabcdef", "<c-v>j$0ld", "|cdef\ncdef"),
        // Multibyte: columns are chars, not bytes.
        ("a|ébé\naébé", "<c-v>jd", "a|bé\nabé"),
        // D deletes to each line's end, like $d.
        ("|abcdef\nabcdef", "l<c-v>jD", "|a\na"),
    ] {
        check(spec, keys, want);
    }
    // The register mirrors the segments joined with newlines.
    check_clip(
        "|abcdef\nab\nabcdef",
        "ll<c-v>jjly",
        "ab|cdef\nab\nabcdef",
        "cd\n\ncd",
    );
    // A block with nothing in it (empty lines) deletes nothing.
    check_any("|\n\nab", "<c-v>jd", "|\n\nab");
}

#[test]
fn block_yank_put() {
    for (spec, keys, want) in [
        // y: cursor to the block's top-left.
        ("|abcdef\nabcdef", "jlll<c-v>khy", "ab|cdef\nabcdef"),
        // p pastes one column right of the cursor, P at it; missing lines
        // are created padded with spaces; cursor at the paste's top-left.
        (
            "|abcdef\nabcdef",
            "l<c-v>jly0jllllp",
            "abcdef\nabcde|bcf\n     bc",
        ),
        (
            "|abcdef\nabcdef",
            "l<c-v>jly0jllllP",
            "abcdef\nabcd|bcef\n    bc",
        ),
        ("|abcd\nefgh\nxy", "<c-v>jlyjjlp", "abcd\nefgh\nxy|ab\n  ef"),
        // d fills the register the same blockwise way.
        (
            "|abcdef\nabcdef\nxyzw",
            "l<c-v>jldGp",
            "adef\nadef\nx|bcyzw\n bc",
        ),
        // A count repeats the segments horizontally.
        ("|abcd\nefgh", "<c-v>jly02p", "a|ababbcd\neefeffgh"),
        // Short target lines pad out to the paste column; segments pad to
        // the block width when text follows them (the empty one too).
        (
            "|ab\n\ncd\nwxyz\nwxyz\nwxyz",
            "<c-v>jjyjjjlp",
            "ab\n\ncd\nwx|ayz\nwx yz\nwxcyz",
        ),
    ] {
        check(spec, keys, want);
    }
    // Pasting a block register over a Visual selection is out of scope.
    let buf = run("|ab\ncd", "<c-v>jyjvp");
    assert!(buf.beeped());
    assert_eq!(show(&buf.text, buf.cursor), "ab\n|cd");
}

#[test]
fn block_change() {
    for (spec, keys, want) in [
        // c deletes the block, inserts on the top line, and replicates the
        // typed text onto the other lines at Esc.
        (
            "|abcdef\nabcdef\nabcdef",
            "ll<c-v>jjlcXY<esc>",
            "abX|Yef\nabXYef\nabXYef",
        ),
        // Lines shorter than the left column are skipped.
        (
            "|abcdef\nab\nabcdef",
            "llll<c-v>jjcXY<esc>",
            "abcdX|Yf\nab\nabcdXYf",
        ),
        // $c clears to each line's end first; C is the same.
        (
            "|abcdef\nab\nabcdef",
            "ll<c-v>jj$cXY<esc>",
            "abX|Y\nabXY\nabXY",
        ),
        ("|abcdef\nabcdef", "l<c-v>jCXY<esc>", "aX|Y\naXY"),
    ] {
        check(spec, keys, want);
    }
    // s is a synonym for c.
    check_i("|abcd\nabcd", "l<c-v>js", "a|cd\nacd");
}

#[test]
fn block_insert_ia() {
    for (spec, keys, want) in [
        // I inserts at the block's left edge on every line, skipping ones
        // too short to reach it; cursor to the block's top-left at Esc.
        ("|abcdef\nabcdef", "ll<c-v>jIXY<esc>", "ab|XYcdef\nabXYcdef"),
        (
            "|abcdef\nab\nabcdef",
            "llll<c-v>jjIXY<esc>",
            "abcd|XYef\nab\nabcdXYef",
        ),
        // A line exactly reaching the left column gets the text appended.
        (
            "|abcdef\nabcd\nabcdef",
            "llll<c-v>jjIXY<esc>",
            "abcd|XYef\nabcdXY\nabcdXYef",
        ),
        ("|ab\n\ncd", "<c-v>jjIX<esc>", "|Xab\nX\nXcd"),
        // A appends after the right edge, padding shorter lines with spaces.
        (
            "|abcdef\nab\nabcdef",
            "ll<c-v>jjlAXY<esc>",
            "ab|cdXYef\nab  XY\nabcdXYef",
        ),
        (
            "|abcdef\nab\nabcdef",
            "llll<c-v>jjAXY<esc>",
            "abcd|eXYf\nab   XY\nabcdeXYf",
        ),
        ("|ab\n\ncd", "<c-v>jjAX<esc>", "|aXb\n X\ncXd"),
        // $A appends at each line's end, no padding.
        (
            "|abcdef\nab\nabcdef",
            "ll<c-v>jj$AXY<esc>",
            "ab|cdefXY\nabXY\nabcdefXY",
        ),
        // An insert spanning lines isn't replicated.
        (
            "|abcdef\nabcdef",
            "l<c-v>jIX<cr>Y<esc>",
            "aX\n|Ybcdef\nabcdef",
        ),
    ] {
        check(spec, keys, want);
    }
}

#[test]
fn block_replace() {
    for (spec, keys, want) in [
        // r fills the rectangle, clamped to each line; cursor at top-left.
        ("|abcdef\nab\nabcdef", "l<c-v>jj2lrz", "a|zzzef\naz\nazzzef"),
        ("|abcdef\nabcdef", "ll<c-v>jlrz", "ab|zzef\nabzzef"),
        // $r replaces to each line's end.
        ("|abcdef\nab\nabcdef", "ll<c-v>jj$rz", "ab|zzzz\nab\nabzzzz"),
    ] {
        check(spec, keys, want);
    }
    // r<Enter> has no blockwise meaning here (the cursor stays where the
    // block cursor was).
    check_full("|ab\nab", "<c-v>jr<cr>", "ab\n|ab", VB, Some(true));
}

#[test]
fn block_shift() {
    for (spec, keys, want) in [
        // >/< shift at the block's *left edge*, not the line start
        // (`:help v_b_>`, probed with sw=2); cursor to the block-left
        // column of the top line.
        ("ab|cdef\nabcdef", "<c-v>j>", "ab|  cdef\nab  cdef"),
        ("|abcdef\nabcdef", "<c-v>j>", "|  abcdef\n  abcdef"),
        ("ab|cdef\nabcdef", "<c-v>j2>", "ab|    cdef\nab    cdef"),
        // Short lines take the blanks at their end; empty lines are skipped.
        (
            "ab|cdef\nab\nabcdef",
            "<c-v>jj>",
            "ab|  cdef\nab  \nab  cdef",
        ),
        ("ab|cdef\n\nabcdef", "<c-v>jj>", "ab|  cdef\n\nab  cdef"),
        // < strips whitespace at the block's left column…
        ("|  abcd\n  abcd", "<c-v>j<lt>", "|abcd\nabcd"),
        ("|\tab\n\tcd", "<c-v>j<lt>", "|ab\ncd"),
        // …and is a quiet no-op where there is none.
        ("  |abcd\n  abcd", "<c-v>j<lt>", "  |abcd\n  abcd"),
    ] {
        check(spec, keys, want);
    }
}

#[test]
fn block_corner_swap() {
    // O swaps the corners horizontally: the cursor keeps its row and takes
    // the anchor's column (probed).
    check_full("a|bcd\nefgh", "<c-v>jlO", "abcd\ne|fgh", VB, Some(false));
    // Further motion extends from the swapped corner.
    check_clip("a|bcd\nefgh", "<c-v>jlOly", "ab|cd\nefgh", "c\ng");
    // Outside block mode O swaps fully, like o.
    check_full("a|bcd", "vllO", "a|bcd", V, Some(false));
    check("ab|cdef", "vlOhd", "a|ef");
}

#[test]
fn block_dot_repeat() {
    // '.' replays the recorded keys, so a block delete repeats relative to
    // the new cursor position.
    check(
        "|abcdef\nabcdef\nabcdef\nabcdef",
        "l<c-v>jld<space>jj.",
        "adef\nadef\nab|ef\nabef",
    );
    // The replicated insert replays too (the typed text is re-entered and
    // the closing Esc re-replicates).
    check(
        "|abcd\nabcd\nabcd\nabcd",
        "l<c-v>jIX<esc>jj.",
        "aXbcd\naXbcd\na|Xbcd\naXbcd",
    );
}

#[test]
fn block_shared_commands() {
    // gq, :, and J act on the covered lines whatever the visual kind.
    let buf = run("s\n|b1\nb2\nb3", "<c-v>jgq");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if r.start >= 2 && r.end <= 8)),
        "blockwise gq reflows the covered lines: {:?}",
        buf.log
    );
    check("|a1\na2\na3", "<c-v>j:s/a/b/<cr>", "b1\n|b2\na3");
    check("|a\nb\nc", "<c-v>jJ", "a| b\nc");
    // o swaps the corners.
    check_full("a|bcd\nefgh", "<c-v>jlo", "a|bcd\nefgh", VB, Some(false));
    // Unsupported blockwise commands beep rather than misapply.
    let buf = run("a|bc\ndef", "<c-v>j~");
    assert!(buf.beeped());
    assert_eq!(show(&buf.text, buf.cursor), "abc\nd|ef");
}

// --- z scrolling -----------------------------------------------------------

#[test]
fn z_scroll() {
    for (keys, align) in [
        ("zz", ScrollAlign::Center),
        ("zt", ScrollAlign::Top),
        ("zb", ScrollAlign::Bottom),
    ] {
        let buf = run("a|b\ncd", keys);
        assert!(
            buf.log.contains(&Action::Scroll(align)),
            "{keys}: {:?}",
            buf.log
        );
        assert_eq!(show(&buf.text, buf.cursor), "a|b\ncd", "{keys}: no move");
        assert!(!buf.beeped(), "{keys}");
    }
    // z. / z<CR> / z- also move to the first non-blank.
    for (keys, align) in [
        ("z.", ScrollAlign::Center),
        ("z<cr>", ScrollAlign::Top),
        ("z-", ScrollAlign::Bottom),
    ] {
        let buf = run("ab\n  c|d", keys);
        assert!(buf.log.contains(&Action::Scroll(align)), "{keys}");
        assert_eq!(show(&buf.text, buf.cursor), "ab\n  |cd", "{keys}");
    }
    // Anything else after z beeps.
    check_beep("|ab", "zq", "|ab");
    // z works from Visual without leaving it, and shows in the mode bar.
    let buf = run("a|bc", "vzz");
    assert_eq!(buf.vim.mode(), V);
    assert!(buf.log.contains(&Action::Scroll(ScrollAlign::Center)));
    let (text, cursor) = parse_spec("|ab");
    let mut buf = Buf::new(&text, cursor);
    buf.feed("z");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("z"));
}

// --- Registers and put ---------------------------------------------------

#[test]
fn registers_put_linewise() {
    for (spec, keys, want) in [
        ("|ab\ncd", "yyp", "ab\n|ab\ncd"),
        ("ab\n|cd", "yyp", "ab\ncd\n|cd"), // after the last line: no trailing \n
        ("ab\n|cd", "yyP", "ab\n|cd\ncd"),
        ("|ab\ncd", "yyP", "|ab\nab\ncd"),
        ("  a|b", "yyp", "  ab\n  |ab"), // cursor at the first non-blank
        ("ab\n|cd", "ddp", "ab\n|cd"),   // dd on the last line, then p
        ("a|b\ncd", "ddp", "cd\n|ab"),
        ("|ab\ncd", "2yyjp", "ab\ncd\n|ab\ncd"),
        ("|ab", "Yp", "ab\n|ab"),
    ] {
        check(spec, keys, want);
    }
}

#[test]
fn registers_put_charwise() {
    for (spec, keys, want) in [
        ("|abc def", "ywwP", "abc abc| def"), // cursor on the last pasted char
        ("a|bc", "y$$p", "abcb|c"),
        ("|ab", "yl2p", "aa|ab"), // count repeats
        ("|ab", "xp", "b|a"),     // x fills the register
        ("a|b", "Xp", "b|a"),
        ("a|b\n", "yljp", "ab\n|b"), // put on an empty line
        ("a|b\n", "yljP", "ab\n|b"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|ab", "p", "|ab"); // nothing yanked yet
    check_beep("|ab", "P", "|ab");
}

// --- Simple edits --------------------------------------------------------

#[test]
fn edits_x_shift_x() {
    for (spec, keys, want) in [
        ("|abc", "x", "|bc"),
        ("ab|c", "x", "a|b"), // cursor clamps to the new line end
        ("a|bc", "3x", "|a"), // count clamps at the line end
        ("a|b\ncd", "5x", "|a\ncd"),
        ("abc|d", "2X", "a|d"),
        ("ab|cd", "5X", "|cd"),
        ("a|b", "X", "|b"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|\nab", "x", "|\nab"); // empty line: nothing to delete
    check_beep("|ab", "X", "|ab"); // col 0
    check_i("a|bc", "s", "a|c");
    check_i("a|bcd", "2s", "a|d");
    check_i("|\nb", "s", "|\nb"); // s on an empty line still enters Insert
}

#[test]
fn edits_replace_char() {
    check("|abc", "rz", "|zbc");
    check("|abcd", "3rz", "zz|zd"); // cursor on the last replaced char
    check("a|bc", "ré", "a|éc");
    check("|éx", "ra", "|ax");
    check_beep("|abc", "5rz", "|abc"); // count overruns the line: no edit
    check_beep("|\nab", "rz", "|\nab");
    check_beep("ab|c", "2rz", "ab|c");
}

#[test]
fn edits_tilde() {
    for (spec, keys, want) in [
        ("|aBc", "~", "A|Bc"),
        ("a|b", "~", "a|B"), // at the line end the cursor stays put
        ("|abcd", "3~", "ABC|d"),
        ("|a.b", "3~", "A.|B"), // non-letters are skipped over, not changed
        ("|éx", "~", "É|x"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|\nab", "~", "|\nab");
}

#[test]
fn edits_join() {
    for (spec, keys, want) in [
        ("|foo\nbar", "J", "foo| bar"),
        ("|foo\n   bar", "J", "foo| bar"), // the next line's indent is swallowed
        ("|foo \nbar", "J", "foo |bar"),   // no double space after trailing blanks
        ("|foo\n\nbar", "J", "fo|o\nbar"), // joining an empty line adds no space
        ("|foo\n   ", "J", "fo|o"),        // ...nor does an all-blank one
        ("|a\nb\nc\nd", "3J", "a b| c\nd"),
        ("|a\nb\nc\nd", "J", "a| b\nc\nd"),
        ("f|oo\nbar\nbaz", "2J", "foo| bar\nbaz"), // 2J == J
    ] {
        check(spec, keys, want);
    }
    check_beep("|ab", "J", "|ab"); // nothing below
    check_beep("ab\n|cd", "J", "ab\n|cd");
}

// --- Insert entry and exit -----------------------------------------------

#[test]
fn insert_entry_positions() {
    check_i("a|bc", "i", "a|bc");
    check_i("a|bc", "a", "ab|c");
    check_i("a|b", "a", "ab|"); // a at the line end goes past the last char
    check_i("a|b\ncd", "a", "ab|\ncd");
    check_i("  a|b", "I", "  |ab");
    check_i("|   ", "I", "  | "); // all-blank line: before the last blank, like ^
    check_i("a|b\ncd", "A", "ab|\ncd");
    check_i("|\nb", "A", "|\nb"); // A on an empty line
    check_i("a|b\ncd", "o", "ab\n|\ncd");
    check_i("ab\nc|d", "o", "ab\ncd\n|");
    check_i("ab\nc|d", "O", "ab\n|\ncd");
    check_i("|ab", "O", "|\nab"); // O on the first line
    check_i("|", "i", "|");
    check_i("|", "o", "\n|");
}

#[test]
fn insert_escape() {
    // Esc steps left one column, but never across the line start.
    check("|abc", "iX<esc>", "|Xabc");
    check("ab\n|cd", "iXY<esc>", "ab\nX|Ycd");
    check("|abc", "a<esc>", "|abc");
    check("|ab", "A<esc>", "a|b");
    check("a|b", "o<esc>", "ab\n|");
    check("ab|c", "aXY<esc>", "abcX|Y");
    check("|abc", "i<esc>", "|abc");
    // Typed newlines and backspaces pass through the app layer.
    check("a|b", "iX<cr>Y<esc>", "aX\n|Yb");
    check("a|b", "iXY<bs><esc>", "a|Xb");
}

// --- Undo / redo ---------------------------------------------------------

#[test]
fn undo_redo_actions() {
    // Engine-native undo: one snapshot per change command (the widget's own
    // history groups edits by time, which breaks `dw..u`).
    check_beep("|abc", "u", "|abc"); // nothing to undo
    check_beep("|abc", "<c-r>", "|abc");
    check("a|bc", "xu", "a|bc");
    check("a|bc", "xu<c-r>", "a|c");
    check("|a b c d e", "dw..u", "|c d e"); // u undoes only the last dw
    check("|a b c", "dwdwuu", "|a b c");
    // An Insert session is a single undo unit…
    check("|foo bar", "ciwxyz<esc>u", "|foo bar");
    check("|ab", "ixyz<esc>u", "|ab");
    // …and an unchanged session adds no undo level (the u undoes the x).
    check("a|bc", "xi<esc>u", "a|bc");
    // A new change clears the redo stack.
    check_beep("a|bcd", "xux<c-r>", "a|cd");
    // `u` clears a pending count: the following x deletes one char.
    check_any("|abcd", "3ux", "|bcd");
}

// --- Surround (vim-surround semantics) -------------------------------------

#[test]
fn surround_add() {
    for (spec, keys, want) in [
        ("he|llo world", "ysiw\"", "|\"hello\" world"),
        ("hello wo|rld", "ysiw'", "hello |'world'"),
        ("he|llo", "ysiw`", "|`hello`"),
        // Closing chars (and quotes) wrap tight; opening chars add spaces.
        ("he|llo", "ysiw)", "|(hello)"),
        ("he|llo", "ysiw(", "|( hello )"),
        ("he|llo", "ysiw]", "|[hello]"),
        ("he|llo", "ysiw[", "|[ hello ]"),
        ("he|llo", "ysiw}", "|{hello}"),
        ("he|llo", "ysiw{", "|{ hello }"),
        ("he|llo", "ysiw>", "|<hello>"),
        // Aliases: b=) B=} r=] a=>.
        ("he|llo", "ysiwb", "|(hello)"),
        ("he|llo", "ysiwB", "|{hello}"),
        ("he|llo", "ysiwr", "|[hello]"),
        ("he|llo", "ysiwa", "|<hello>"),
        // With around-objects and motions.
        ("a(b|c)d", "ysa(]", "a|[(bc)]d"),
        ("|abcx d", "ysfx)", "|(abcx) d"),
        ("a|bc", "ys$)", "a|(bc)"),
        ("|hello world", "ys2w\"", "|\"hello world\""),
        // yss wraps the line's content, ignoring surrounding blanks.
        ("  he|llo world", "yss)", "  |(hello world)"),
        ("he|llo", "yss\"", "|\"hello\""),
        ("é|✓x y", "ysiw\"", "é|\"✓\"x y"), // iw is the ✓ punct run alone
    ] {
        check(spec, keys, want);
    }
    // Visual S wraps the selection; linewise S wraps the lines' content.
    check("a|bcd e", "vlS\"", "a|\"bc\"d e");
    check("|ab\ncd", "VS)", "|(ab)\ncd");
    check_beep("he|llo", "ysiwz", "he|llo"); // no such pair
    check_beep("|abc", "ysfz)", "|abc"); // failed motion cancels
}

#[test]
fn surround_delete() {
    for (spec, keys, want) in [
        ("\"he|llo\" x", "ds\"", "|hello x"),
        ("x '|ab' y", "ds'", "x |ab y"),
        ("a(b|c)d", "ds(", "a|bcd"),
        ("a(b|c)d", "ds)", "a|bcd"),
        ("a(b|c)d", "dsb", "a|bcd"),
        // The opening-char target also trims the inner padding.
        ("a( b|c )d", "ds(", "a|bcd"),
        ("a( b|c )d", "ds)", "a| bc d"),
        ("(a(b|c)d)", "ds)", "(a|bcd)"), // nearest enclosing pair
        ("a[b|c]d", "ds]", "a|bcd"),
        ("a{b|c}d", "ds}", "a|bcd"),
        ("a<b|c>d", "ds>", "a|bcd"),
    ] {
        check(spec, keys, want);
    }
    check_beep("a|bc", "ds\"", "a|bc"); // no enclosing pair
    check_beep("a|bc", "dsz", "a|bc");
}

#[test]
fn surround_change() {
    for (spec, keys, want) in [
        ("\"he|llo\" x", "cs\"'", "|'hello' x"),
        ("'he|llo'", "cs'\"", "|\"hello\""),
        ("(he|llo)", "cs)]", "|[hello]"),
        ("(he|llo)", "cs)(", "|( hello )"),
        ("( he|llo )", "cs(\"", "|\"hello\""), // from-( trims, to-" is tight
        ("[he|llo]", "cs]}", "|{hello}"),
        ("(he|llo)", "csb]", "|[hello]"),
        ("{a b|c d}", "cs})", "|(a bc d)"),
    ] {
        check(spec, keys, want);
    }
    check_beep("a|bc", "cs\"'", "a|bc");
    check_beep("(a|b)", "cs(z", "(a|b)");
}

// --- Multibyte -------------------------------------------------------------

#[test]
fn multibyte_mix() {
    // é is 2 bytes (and a word char); ✓ is 3 and 𝄞 4 (both punctuation-class),
    // so "é✓𝄞" is two words: "é" and "✓𝄞".
    for (spec, keys, want) in [
        ("|é✓𝄞 é✓𝄞", "l", "é|✓𝄞 é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "3l", "é✓𝄞| é✓𝄞"),
        ("é✓𝄞 é✓|𝄞", "h", "é✓𝄞 é|✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "w", "é|✓𝄞 é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "2w", "é✓𝄞 |é✓𝄞"),
        ("é✓𝄞 |é✓𝄞", "b", "é|✓𝄞 é✓𝄞"),
        ("|é✓𝄞", "e", "é✓|𝄞"),
        ("|é✓𝄞 é✓𝄞", "$", "é✓𝄞 é✓|𝄞"),
        ("|é✓𝄞 é✓𝄞", "f𝄞", "é✓|𝄞 é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "f𝄞;", "é✓𝄞 é✓|𝄞"),
        ("|é✓𝄞 é✓𝄞", "t✓;", "é✓𝄞 |é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "x", "|✓𝄞 é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "2x", "|𝄞 é✓𝄞"),
        ("é✓𝄞 é✓|𝄞", "X", "é✓𝄞 é|𝄞"),
        ("|é✓ 𝄞x", "de", "| 𝄞x"),
        ("|é✓𝄞 é✓𝄞", "dw", "|✓𝄞 é✓𝄞"),
        ("|é✓𝄞 é✓𝄞", "d2w", "|é✓𝄞"),
        ("é✓𝄞 é|✓𝄞", "diw", "é✓𝄞 |é"),
        ("a|é bé", "diw", "| bé"),
        ("|é✓𝄞 é✓𝄞", "d$", "|"),
        ("é|✓𝄞 x", "vld", "é| x"),
        ("|é✓𝄞", "rq", "|q✓𝄞"),
        ("|é✓𝄞 é", "yWWP", "é✓𝄞 é✓𝄞| é"),
    ] {
        check(spec, keys, want);
    }
    check_i("é✓𝄞 é|✓𝄞", "ciw", "é✓𝄞 é|");
    check("|é✓𝄞", "~", "É|✓𝄞");
    check("|é✓𝄞 é✓𝄞", "A<esc>", "é✓𝄞 é✓|𝄞");
}

// --- Deterministic no-panic fuzz -------------------------------------------

/// Every action the engine returns must keep offsets on char boundaries, no
/// matter what key soup arrives. A fixed-seed LCG makes failures replayable.
#[test]
fn fuzz_no_panic() {
    let seeds = [
        "é✓𝄞 abc\nxé y\n\n«def»\n",
        "hello world\nfoo   bar.baz\n\n\n  indented line\nend",
        "a\n",
        "",
        "🎵🎶 ✓\n\twords here\n(nested (par) [br] {cur})\n'q' \"dq\" `t`",
    ];
    let pool: Vec<Key> = {
        let mut p: Vec<Key> =
            "hjkl0^$wbeWBE gG fFtT;,{}%xXsSrRdcyvVpPuJoOiIaA~\"'()[]{}<>bBqzZ1290+-.:/?_!"
                .chars()
                .map(Key::Char)
                .collect();
        p.extend([
            Key::Enter,
            Key::Escape,
            Key::Escape,
            Key::Escape,
            Key::Backspace,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Ctrl('r'),
            Key::Ctrl('n'),
            Key::Ctrl('p'),
            Key::Ctrl('v'),
        ]);
        p
    };
    let mut rng: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (rng >> 33) as usize
    };
    for (si, seed) in seeds.iter().enumerate() {
        let mut buf = Buf::new(seed, 0);
        let mut recent: Vec<Key> = Vec::new();
        for step in 0..4000 {
            let key = pool[next() % pool.len()];
            // Mirror the app layer: in Insert mode only Esc reaches the engine
            // (typing is the input widget's business, exercised elsewhere).
            if buf.vim.in_insert() && key != Key::Escape {
                continue;
            }
            recent.push(key);
            if recent.len() > 40 {
                recent.remove(0);
            }
            let ctx = format!("seed {si}, step {step}, recent keys {recent:?}");
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                buf.feed_key(key);
            }));
            assert!(panicked.is_ok(), "engine panicked: {ctx}");
            assert!(
                buf.cursor <= buf.text.len() && buf.text.is_char_boundary(buf.cursor),
                "cursor {} off char boundary of {:?}: {ctx}",
                buf.cursor,
                buf.text
            );
        }
    }
}

// --- Review-workflow regressions -------------------------------------------
// Each case below reproduces a confirmed finding from the adversarial review,
// verified against Vim 9.2 before fixing.

#[test]
fn count_before_operator_reaches_goto() {
    // The count before d is the absolute line for G/gg, and counts multiply.
    check("|l1\nl2\nl3\nl4\nl5", "2dG", "|l3\nl4\nl5");
    check("l1\nl2\nl3\n|l4\nl5", "2dgg", "l1\n|l5");
    check("|l1\nl2\nl3\nl4\nl5\nl6\nl7", "2d3G", "|l7"); // 2×3 = line 6
    check("|l1\nl2\nl3", "d2G", "|l3");
}

#[test]
fn count_on_shift_d_c() {
    check_clip("a|bc\ndef\nghi", "2D", "|a\nghi", "bc\ndef");
    check_i("a|bc\ndef\nghi", "2C", "a|\nghi");
    check("a|bc", "2D", "|a"); // count past EOF clamps
}

#[test]
fn desired_col_resets() {
    // Edits re-anchor the sticky column (Vim resets curswant).
    check("abcd|e\ntuvwxyz", "$xj", "abcd\ntuv|wxyz");
    check("|abcdefg\nhi\ntuvwxyz", "$jxk", "|abcdefg\nh\ntuvwxyz");
    // Leaving Insert re-anchors it too.
    check("abcd|e\ntuvwxyz", "$a!<esc>j", "abcde!\ntuvwx|yz");
    // `$` before entering Visual must not extend the selection through the
    // newline (only `$` pressed inside Visual does).
    check("|abc\ndef", "$vd", "a|b\ndef");
    check_clip("ab|c\ndef", "$vy", "ab|c\ndef", "c");
    // …while v$ still reaches through the newline (curswant=MAXCOL in Visual).
    check("a|bc\nd", "v$d", "a|d");
}

#[test]
fn visual_uppercase_ops() {
    // Y/D/C act linewise whatever the visual kind; u/U set the case.
    check_clip("a|bc\ndef", "vY", "|abc\ndef", "abc\n");
    check("a|bc\ndef", "vjD", "|");
    check_i("a|bc\ndef", "vC", "|\ndef");
    check("a|BcD", "vlu", "a|bcD");
    check("a|bcd", "vlU", "a|BCd");
}

#[test]
fn replace_with_enter() {
    // r<CR> replaces the char(s) with one line break, cursor after it.
    check("a|bcd", "r<cr>", "a\n|cd");
    check("a|bcd", "2r<cr>", "a\n|d");
    check_beep("ab|c", "2r<cr>", "ab|c"); // not enough chars on the line
}

#[test]
fn huge_counts_do_not_panic() {
    // Nine-digit cap + saturating math: no overflow, no OOM-sized puts.
    check_any("|abc", "9999999999999999999d9999999999999999999w", "|");
    let buf = run("|ab", "yy999999999p");
    assert!(buf.text.len() <= (4 << 20) + 3, "put expansion uncapped");
    check("|abc def", "999999999w", "abc de|f"); // partial count still moves
}

#[test]
fn visual_put_linewise_matrix() {
    // Charwise register over a linewise selection: pastes as its own line.
    check("|ab\ncd\nef", "yljVp", "ab\n|a\nef");
    // Linewise register over a charwise selection: splits the line.
    check("|ab\ncd", "yyjviwp", "ab\n\n|ab\n");
    // The replaced selection takes the register's place (swap idiom)…
    let buf = run("|ab\ncd", "ylwvlp");
    assert_eq!(buf.text, "ab\na"); // 'cd' replaced by 'a'
    assert_eq!(buf.clipboard.as_deref(), Some("cd")); // …and is yanked out
                                                      // Linewise over linewise replaces the lines.
    check("|ab\ncd\nef", "yyjVp", "ab\n|ab\nef");
}

#[test]
fn aw_trailing_blanks_cross_newline() {
    check_clip("ab| \t\ncd", "daw", "a|b", " \t\ncd");
    check("ab|  \ncd ef", "daw", "ab| ef");
}

#[test]
fn aw_on_empty_line() {
    // From an empty line `aw` runs through the following word, covering
    // whole lines; a following empty line joins instead (probed against
    // Vim 9.2).
    check_clip("aa\n|\nbb\ncc", "daw", "aa\n|cc", "\nbb\n");
    check("aa\n|\nbb cc", "daw", "aa\n| cc");
    check_clip("aa\n|\n  bb", "daw", "|aa", "\n  bb\n");
    check("aa\n|\n\nbb", "daw", "aa\n|bb");
    check("aa\n|\n\n\nbb", "daw", "aa\n|\nbb");
    check("aa\n|\n \nbb", "daw", "|aa");
    check("aa\n|\nbb\ncc", "2daw", "|aa");
    check("aa\n|\nbb\ncc", "cawX<esc>", "aa\n|X\ncc");
    check_beep("aa\n|", "daw", "aa\n|"); // empty last line: no word
    check_beep("aa\n|\n ", "daw", "aa\n|\n ");
}

// --- Dot repeat, editor commands, and search --------------------------------

#[test]
fn dot_repeat_simple() {
    check("|ab cd ef", "dw.", "|ef");
    check("a|bcd", "x..", "|a");
    check_any("|abc", "2x.", "|"); // the second 2x runs out of chars
    check("a|b ab", "xw.", "a |b");
    check_beep("|ab", ".", "|ab"); // nothing to repeat
                                   // An undo between doesn't clobber the recorded change: x, undo, then
                                   // `.` re-deletes.
    let buf = run("a|bcd", "xu.");
    assert_eq!(buf.text, "acd");
}

#[test]
fn dot_repeat_count_replaces() {
    // A count on `.` replaces the change's own counts, wherever they were
    // typed (probed: Vim's redo buffer keeps one leading count).
    check("|abcdefgh", "2x3.", "|fgh");
    check("|abcdefgh", "2x.", "|efgh"); // no new count: the old one holds
    check("|a b c d e f g h", "d2w3.", "|f g h");
    check("|a b c d e f g h", "2d2w2.", "|g h");
    check("|zzzz", "3ix<esc>2.", "xxx|xxzzzz");
}

#[test]
fn insert_entry_counts() {
    // A count on an Insert-entry command repeats the typed text on Esc;
    // `o`/`O` open a line per repetition (all probed).
    check("|zz", "3ix<esc>", "xx|xzz");
    check("|zz", "3axy<esc>", "zxyxyx|yz");
    check("|zz", "3Ix<esc>", "xx|xzz");
    check("|zz", "3Ax<esc>", "zzxx|x");
    check("|zz", "3oab<esc>", "zz\nab\nab\na|b");
    check("|zz", "3Oab<esc>", "ab\nab\na|b\nzz");
    check("|zz", "3o<esc>", "zz\n\n\n|");
    // A multi-line insert repeats whole.
    check("|zz", "3ia<cr>b<esc>", "a\nba\nba\n|bzz");
    check("|zz", "3oa<cr>b<esc>", "zz\na\nb\na\nb\na\n|b");
    // Without a count nothing changes; a plain insert doesn't repeat.
    check("|zz", "1ix<esc>", "|xzz");
    check("|zz", "3i<esc>", "|zz");
}

#[test]
fn dot_repeat_insert_and_surround() {
    // The Insert session's text is captured and re-typed.
    check("|foo bar", "ciwnew<esc>w.", "new ne|w");
    check("|ab", "oxy<esc>.", "ab\nxy\nx|y");
    check("|foo bar", "ysiw\"W.", "\"foo\" |\"bar\"");
    check("|abcd", "vlrzll.", "zz|zz");
    check("x|y", "aQ<esc>.", "xyQ|Q"); // append repeats after the new char
}

#[test]
fn editor_commands() {
    let buf = run("|ab", "ZZ");
    assert!(buf.log.contains(&Action::Commit), "ZZ commits");
    let buf = run("|ab", "ZQ");
    assert!(
        buf.log.contains(&Action::Quit { force: false }),
        "ZQ cancels"
    );
    let buf = run("l1\nlo|ng", "gqq");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if *r == (3..7))),
        "gqq reflows the current line: {:?}",
        buf.log
    );
    check_beep("|ab", "Zx", "|ab");
    // Esc in idle Normal mode is a quiet no-op (cancel is ZQ).
    check("a|b", "<esc>", "a|b");
    // A committed/cancelled editor isn't a repeatable change.
    check_beep("|ab", "ZZ.", "|ab");
}

#[test]
fn search_basic() {
    check("|ab cd ab", "/cd<cr>", "ab |cd ab");
    check("|ab cd ab", "/ab<cr>", "ab cd |ab"); // strictly after the cursor
    check("ab cd a|b", "/ab<cr>", "|ab cd ab"); // wraps at EOF
    check("|ab ab ab", "/ab<cr>n", "ab ab |ab");
    check("|ab ab ab", "/ab<cr>nN", "ab |ab ab");
    check("a|b\ncd", "?a<cr>", "|ab\ncd"); // backward
    check("|ab ab ab", "/ab<cr>/<cr>", "ab ab |ab"); // empty / repeats
    check("|ab cd", "/x<bs>cd<cr>", "ab |cd"); // backspace edits the query
    check("|ab\ncd", "/<bs>j", "ab\n|cd"); // backspace on empty cancels
    check("|ab\ncd", "/cd<esc>j", "ab\n|cd"); // esc cancels the prompt
    check_beep("|abc", "/zz<cr>", "|abc"); // no match
    check_beep("|ab", "n", "|ab"); // nothing searched yet
                                   // Search across lines, multibyte content.
    check("é|✓\nx é✓", "/é✓<cr>", "é✓\nx |é✓");
}

#[test]
fn search_smartcase() {
    // An all-lowercase query matches case-insensitively…
    check("|ab CD ab", "/cd<cr>", "ab |CD ab");
    check("ab CD a|b", "?cd<cr>", "ab |CD ab");
    check("|ab CD cd", "/cd<cr>n", "ab CD |cd");
    // …any uppercase makes it exact.
    check("|ab cd CD", "/CD<cr>", "ab cd |CD");
    check_beep("|ab cd", "/CD<cr>", "|ab cd");
    // Multibyte case folding.
    check("|x É y", "/é<cr>", "x |É y");
}

#[test]
fn mouse_hooks() {
    // A click aborts a pending operator (the app calls cancel_pending on
    // mouse-down), so the next key isn't consumed as a motion.
    let (text, cursor) = parse_spec("|ab cd");
    let mut buf = Buf::new(&text, cursor);
    buf.feed("d");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("d"));
    buf.vim.cancel_pending();
    assert_eq!(buf.vim.pending_display(), None);
    buf.feed("w");
    assert_eq!(show(&buf.text, buf.cursor), "ab |cd"); // moved, deleted nothing

    // A drag becomes a Visual selection: anchor at the start, then operators
    // act on it.
    let (text, cursor) = parse_spec("|ab cd");
    let mut buf = Buf::new(&text, cursor);
    buf.vim.begin_visual(&buf.text.clone(), 0);
    buf.cursor = 3; // the app puts the cursor on the selection's last char
    buf.feed("d");
    assert_eq!(show(&buf.text, buf.cursor), "|d");
    let _ = (text, cursor);
}

#[test]
fn search_is_regex() {
    check("|ab a1 cd", "/a[0-9]<cr>", "ab |a1 cd");
    check("|ab xy ab", "/x.<cr>", "ab |xy ab");
    check("|one two", "/t\\w+<cr>", "one |two");
    check_beep("|ab", "/(<cr>", "|ab"); // invalid pattern: no match
    check("|aa ab", "/a+<cr>n", "aa |ab"); // n repeats the regex
}

// --- Underscore, comma leader, gq operator ---------------------------------

#[test]
fn underscore_motion() {
    check("ab\n  c|d", "_", "ab\n  |cd"); // current line's first non-blank
    check("a|b\n  cd", "2_", "ab\n  |cd"); // count-1 lines down
    check("a|b\ncd", "d_", "|cd"); // linewise: d_ == dd
    check("a|b\ncd\nef", "d2_", "|ef");
    check_beep("a|b", "3_", "a|b"); // past the last line
}

#[test]
fn comma_leader() {
    let buf = run("|ab", ",,");
    assert!(buf.log.contains(&Action::Commit), ",, commits");
    let buf = run("|ab", ",c");
    assert!(buf.log.contains(&Action::Commit), ",c commits");
    let buf = run("|ab", ",k");
    assert!(
        buf.log.contains(&Action::Quit { force: false }),
        ",k cancels"
    );
    // Any other key after `,` falls back to reverse-find repeat, then runs.
    check("x a |x b x", "fx,l", "x a x| b x");
    // With an operator pending, `,` stays the reverse-find repeat.
    check("axbx|cx", "Fxd,", "ax|b");
}

#[test]
fn gq_reflow_targets() {
    // gq{motion}: the covered lines.
    let buf = run("s\n|b1\nb2\nb3", "gqj");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if *r == (2..8))),
        "gqj covers two lines: {:?}",
        buf.log
    );
    // gqG: through the last line.
    let buf = run("s\n|b1\nb2\nb3", "gqG");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if r.start == 2 && r.end >= 9)),
        "gqG reaches EOF: {:?}",
        buf.log
    );
    // Visual gq: the selection.
    let buf = run("s\n|b1\nb2\nb3", "Vjgq");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if *r == (2..8))),
        "visual gq covers the selected lines: {:?}",
        buf.log
    );
    check_beep("|ab", "gqx", "|ab"); // not a motion
}

#[test]
fn gw_reflow_targets() {
    // gw is gq with the cursor kept on its text (the app side maps it); the
    // engine emits the keep variant over the same targets.
    let buf = run(
        "l1
lo|ng", "gww",
    );
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRangeKeep(r) if *r == (3..7))),
        "gww reflows the current line: {:?}",
        buf.log
    );
    // The doubled `w` is the linewise form, not the word motion (Vim: gww).
    let buf = run(
        "s
|b1 b2 b3",
        "gww",
    );
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRangeKeep(r) if *r == (2..10))),
        "gww is linewise, not to-next-word: {:?}",
        buf.log
    );
    // gw{motion} and visual gw, like gq's.
    let buf = run(
        "s
|b1
b2
b3",
        "gwj",
    );
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRangeKeep(r) if *r == (2..8))),
        "gwj covers two lines: {:?}",
        buf.log
    );
    let buf = run(
        "s
|b1
b2
b3",
        "Vjgw",
    );
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRangeKeep(r) if *r == (2..8))),
        "visual gw covers the selected lines: {:?}",
        buf.log
    );
    // gwip: objects work through the keep operator too.
    let buf = run(
        "s

|a b
c d",
        "gwip",
    );
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRangeKeep(r) if *r == (3..10))),
        "gwip covers the paragraph: {:?}",
        buf.log
    );
    check_beep("|ab", "gwq", "|ab"); // q doubles gq, not gw
}

#[test]
fn paragraph_objects() {
    // ip: the contiguous non-blank block, whole lines.
    check("aa\nb|b\n\ncc", "dip", "|\ncc");
    // ap adds the trailing blank lines — or the leading ones when none trail.
    check("aa\nb|b\n\ncc", "dap", "|cc");
    check("\n\naa\nb|b", "dap", "|");
    // ip on a blank block takes the blanks; ap adds the following paragraph.
    check("aa\n|\n\nbb", "dip", "aa\n|bb");
    check("aa\n|\n\nbb", "dap", "aa\n|");
    // cip clears the block and enters Insert.
    check_i("aa\nb|b\n\ncc", "cip", "|\n\ncc");
    // gqip reflows the paragraph.
    let buf = run("s\n\n|a b\nc d", "gqip");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if *r == (3..10))),
        "gqip covers the paragraph: {:?}",
        buf.log
    );
}

#[test]
fn sentence_objects() {
    check("One two. Thr|ee four. Five.", "dis", "One two. | Five.");
    check("One two. Thr|ee four. Five.", "das", "One two. |Five.");
    // `as` on the last sentence takes the leading whitespace instead.
    check("One two. Fi|ve.", "das", "One two|.");
    check("One two. Fi|ve.", "dis", "One two.| ");
    // Closing quotes/brackets belong to the sentence; count extends.
    check("(One.) Tw|o. Three.", "d2is", "(One.)| ");
    check_i("|One. Two.", "cis", "| Two.");
    // Sentences stop at the paragraph.
    check("A|a bb\n\ncc", "dis", "|\n\ncc");
}

#[test]
fn tag_objects() {
    check("x <b>bo|ld</b> y", "dit", "x <b>|</b> y");
    check("x <b>bo|ld</b> y", "dat", "x | y");
    check_i("x <b>bo|ld</b> y", "cit", "x <b>|</b> y");
    // Nested same-name tags pair by depth; count goes out.
    check("<i>a <i>|b</i> c</i>", "dit", "<i>a <i>|</i> c</i>");
    check("<i>a <i>|b</i> c</i>", "d2it", "<i>|</i>");
    // Attributes on the opener; cursor on a tag counts as inside.
    check("<a href=x>li|nk</a>", "dit", "<a href=x>|</a>");
    check("<b|>text</b>", "dit", "<b>|</b>");
    // Self-closing and non-tags don't pair.
    check_beep("a <br/> |b", "dit", "a <br/> |b");
    check("<b>a <br/> |b</b>", "dat", "|");
    check_beep("1 |< 2 > 3", "dit", "1 |< 2 > 3");
}

#[test]
fn comma_q_reflows_whole_message() {
    let buf = run("s\n|ab cd", ",q");
    assert!(
        buf.log
            .iter()
            .any(|a| matches!(a, Action::ReflowRange(r) if *r == (0..7))),
        ",q reflows the whole message: {:?}",
        buf.log
    );
}

#[test]
fn undo_skips_no_op_snapshots() {
    // gqq's ReflowRange snapshots before the app applies it; when the reflow
    // changes nothing (here the harness applies none), a single `u` still
    // reaches the real change underneath.
    let buf = run("a|bc", "xgqqu");
    assert_eq!(show(&buf.text, buf.cursor), "a|bc");
    assert!(buf.vim.undo_stack().is_empty(), "both snapshots consumed");
}

#[test]
fn search_query_preview() {
    let (text, cursor) = parse_spec("|ab cd");
    let mut buf = Buf::new(&text, cursor);
    buf.feed("/c");
    assert_eq!(buf.vim.search_query(), Some("c"));
    buf.feed("d");
    assert_eq!(buf.vim.search_query(), Some("cd"));
    buf.feed("<esc>");
    assert_eq!(buf.vim.search_query(), None);
}

// --- Indent operators -------------------------------------------------------

#[test]
fn indent_operators() {
    // >> / << on the current line(s); indent is two spaces.
    check("a|b\ncd", ">>", "  |ab\ncd");
    check("|ab\ncd", "2>>", "  |ab\n  cd");
    check("  a|b", "<lt><lt>", "|ab");
    check(" a|b", "<lt><lt>", "|ab"); // a lone space still dedents
    check("\ta|b", "<lt><lt>", "|ab"); // and a tab counts as one step
                                       // With a motion / text object (whole lines).
    check("|ab\ncd\nef", ">j", "  |ab\n  cd\nef");
    check("a|a bb\ncc\n\ndd", ">ip", "  |aa bb\n  cc\n\ndd");
    check("s\n\n|aa\nbb", ">G", "s\n\n  |aa\n  bb");
    // Counts multiply; blank lines don't gain trailing indent.
    check("|a\n\nb", "3>>", "  |a\n\n  b");
    // Visual shift exits to Normal, cursor at the first non-blank.
    check("|ab\ncd", "Vj>", "  |ab\n  cd");
    check("  ab\n  c|d", "vk<lt>", "|ab\ncd");
    // << with nothing to strip is a quiet no-op.
    check("|ab", "<lt><lt>", "|ab");
    // Dot-repeat and undo ride the normal edit path.
    check("|ab", ">>..", "      |ab");
    check("|ab", ">>u", "|ab");
}

// --- Ex (':') commands -------------------------------------------------------

#[test]
fn ex_editor_commands() {
    let buf = run("|ab", ":q<cr>");
    assert!(buf.log.contains(&Action::Quit { force: false }), ":q quits");
    let buf = run("|ab", ":q!<cr>");
    assert!(
        buf.log.contains(&Action::Quit { force: true }),
        ":q! force-quits"
    );
    for keys in [":w<cr>", ":wq<cr>", ":x<cr>"] {
        let buf = run("|ab", keys);
        assert!(buf.log.contains(&Action::Commit), "{keys} commits");
    }
    check_beep("|ab", ":wx<cr>", "|ab"); // unknown command
    check_beep("|ab", ":<cr>", "|ab"); // empty line
}

#[test]
fn ex_line_jump() {
    check("|ab\n  cd\nef", ":2<cr>", "ab\n  |cd\nef"); // first non-blank
    check("ab\nc|d", ":1<cr>", "|ab\ncd");
    check("|ab\ncd", ":100<cr>", "ab\n|cd"); // clamped to the last line
}

#[test]
fn ex_prompt_editing() {
    // Backspace edits the line; on an empty one it cancels the prompt.
    check("|ab\ncd", ":x<bs>2<cr>", "ab\n|cd");
    check("|ab\ncd", ":<bs>j", "ab\n|cd");
    check("|ab\ncd", ":q<esc>j", "ab\n|cd"); // Esc cancels
                                             // The prompt shows in the pending display as it is typed.
    let buf = run("|ab", ":s/a");
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":s/a"));
}

#[test]
fn ex_substitute() {
    // Current line, first occurrence per line; cursor at the changed line's
    // first non-blank. The trailing delimiter is optional.
    check("ab a|b\ncd", ":s/ab/xy/<cr>", "|xy ab\ncd");
    check("a|b\ncd", ":s/ab/xy<cr>", "|xy\ncd");
    // g substitutes every occurrence on the line; i ignores case (there is
    // no smartcase — the pattern is a plain regex).
    check("ab a|b\ncd", ":s/ab/xy/g<cr>", "|xy xy\ncd");
    check("A|B ab\ncd", ":s/ab/xy/gi<cr>", "|xy xy\ncd");
    check_beep("A|B\ncd", ":s/ab/xy/<cr>", "A|B\ncd");
    // % is every line; cursor lands on the last changed line.
    check("a|b\ncd ab", ":%s/ab/xy/<cr>", "xy\n|cd xy");
    // N,M ranges, with . and $ endpoints.
    check("a1\n|a2\na3\na4", ":1,2s/a/b/<cr>", "b1\n|b2\na3\na4");
    check("a1\n|a2\na3\na4", ":.,$s/a/b/<cr>", "a1\nb2\nb3\n|b4");
    check("|a1\na2\na3", ":2,3s/a/b/<cr>", "a1\nb2\n|b3");
    check("|a1\na2\na3", ":2s/a/b/<cr>", "a1\n|b2\na3"); // single-line range
}

#[test]
fn ex_substitute_replacement_refs() {
    // & and \0 are the whole match; \1..\9 the capture groups.
    check("|ab\ncd", ":s/ab/[&]/<cr>", "|[ab]\ncd");
    check("|ab\ncd", ":s/ab/\\0\\0/<cr>", "|abab\ncd");
    check("|ab cd", ":s/(a)(b)/\\2\\1/<cr>", "|ba cd");
    // Group refs don't glue onto trailing digits.
    check("|ab\ncd", ":s/(a)b/\\11/<cr>", "|a1\ncd");
    // \& is a literal ampersand, \\ a literal backslash, $ stays literal.
    check("|ab\ncd", ":s/ab/x\\&y/<cr>", "|x&y\ncd");
    check("|ab\ncd", ":s/ab/x\\\\y/<cr>", "|x\\y\ncd");
    check("|ab\ncd", ":s/ab/$5/<cr>", "|$5\ncd");
    // \/ is the escaped delimiter, in the pattern and the replacement.
    check("|a/b c", ":s/a\\/b/x/<cr>", "|x c");
    check("|ab c", ":s/ab/x\\/y/<cr>", "|x/y c");
}

#[test]
fn ex_substitute_errors() {
    check_beep("|ab\ncd", ":s/zz/x/<cr>", "|ab\ncd"); // no match (E486)
    check_beep("|ab\ncd", ":2s/ab/x/<cr>", "|ab\ncd"); // no match in the range
    check_beep("|ab\ncd", ":s/(/x/<cr>", "|ab\ncd"); // invalid regex
    check_beep("|ab\ncd", ":s/ab/x/z<cr>", "|ab\ncd"); // unknown flag
    check_beep("|ab\ncd", ":s//x/<cr>", "|ab\ncd"); // empty pattern
}

#[test]
fn ex_substitute_visual_range() {
    // Visual ':' leaves Visual and pre-fills the prompt with '<,'>.
    let buf = run("|a1\na2", "Vj:");
    assert_eq!(buf.vim.mode(), N);
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":'<,'>"));
    // The remembered range covers the selected lines, for V and v alike.
    check("a1\n|a2\na3\na4", "Vj:s/a/b/<cr>", "a1\nb2\n|b3\na4");
    check("a1\na|2\na3\na4", "vj:s/a/b/<cr>", "a1\nb2\n|b3\na4");
    // '<,'> in a prompt that wasn't opened from Visual beeps.
    check_beep("|a1\na2", ":'<lt>,'>s/a/b/<cr>", "|a1\na2");
}

#[test]
fn ex_commands_are_not_dot_repeatable() {
    // Vim's '.' repeats the last Normal-mode change, never a ':' command:
    // after x then :s, '.' re-runs the x (verified against Vim 9.2).
    check("a|b ab\ncd", "x:s/b/z/<cr>.", "| az\ncd");
    // A ':' substitution is a single undo unit.
    check("a|b\nab", ":%s/ab/xy/<cr>u", "a|b\nab");
}

#[test]
fn ex_help() {
    for keys in [":help<cr>", ":h<cr>"] {
        let buf = run("|ab", keys);
        assert!(buf.log.contains(&Action::Help), "{keys} opens help");
    }
}

#[test]
fn ex_errors_echo_messages() {
    // Failed ':' commands echo what went wrong (shown until the next key).
    let cases = [
        (":foo<cr>", "Not an editor command: foo"),
        (":s/zzz/x<cr>", "Pattern not found: zzz"),
        (":s/[/x<cr>", "Invalid pattern: ["),
        (":s/a/b/q<cr>", "Trailing characters: q"),
        (":'<lt>,'>s/a/b<cr>", "Mark not set"),
        (":1,xs/a/b<cr>", "Invalid range"),
        (":s//b<cr>", "Empty pattern"),
    ];
    for (keys, want) in cases {
        let buf = run("|ab", keys);
        assert_eq!(buf.error(), Some(want), "{keys}");
        assert_eq!(show(&buf.text, buf.cursor), "|ab", "{keys}: must not edit");
    }
}

#[test]
fn prompt_unhandled_keys_keep_input() {
    // An unhandled key at the `/`/`?`/`:` prompt (arrows, stray Ctrl
    // chords…) beeps but must not destroy the typed line.
    let mut buf = run("|ab cd", "/c");
    buf.feed("<left>");
    assert!(buf.beeped());
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/c"));
    buf.feed("d<cr>");
    assert_eq!(show(&buf.text, buf.cursor), "ab |cd");

    let mut buf = run("|ab\ncd", ":2");
    buf.feed("<c-x>");
    assert!(buf.beeped());
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":2"));
    buf.feed("<cr>");
    assert_eq!(show(&buf.text, buf.cursor), "ab\n|cd");
}

#[test]
fn prompt_history() {
    // Up recalls executed searches, newest first; Down walks back and then
    // restores the stashed live line. Past either end just rings the bell.
    let mut buf = run("|alpha beta", "/beta<cr>/alpha<cr>");
    buf.feed("/");
    buf.feed("<up>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/alpha"));
    buf.feed("<up>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/beta"));
    buf.feed("<up>"); // already at the oldest
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/beta"));
    buf.feed("<down>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/alpha"));
    buf.feed("<down>"); // back to the (empty) live line
    assert_eq!(buf.vim.pending_display().as_deref(), Some("/"));
    buf.feed("<esc>");

    // The ex prompt has its own history (C-p/C-n work too); typing resumes
    // editing the recalled line, and consecutive repeats aren't duplicated.
    buf.feed(":5<cr>:5<cr>:s/x/y<cr>");
    buf.feed(":");
    buf.feed("<c-p>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":s/x/y"));
    buf.feed("<c-p>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":5"));
    buf.feed("<c-p>"); // the two :5 runs collapsed into one entry
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":5"));
    buf.feed("<c-n>");
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":s/x/y"));
    buf.feed("6");
    assert_eq!(buf.vim.pending_display().as_deref(), Some(":s/x/y6"));
    buf.feed("<esc>");

    // A recalled search executes like a typed one.
    let mut buf = run("|alpha beta", "/beta<cr>gg");
    buf.feed("/");
    buf.feed("<up>");
    buf.feed("<cr>");
    assert_eq!(show(&buf.text, buf.cursor), "alpha |beta");
}

#[test]
fn ex_substitute_preview() {
    // The matches the substitution being typed would touch: first per line,
    // every one once `g` is typed, scoped to the range.
    let m = |spec: &str, keys: &str| {
        let buf = run(spec, keys);
        buf.vim.ex_matches(&buf.text, buf.cursor)
    };
    assert_eq!(m("aa a|a\nba", ":s/a"), vec![0..1]);
    assert_eq!(m("aa a|a\nba", ":%s/a"), vec![0..1, 7..8]);
    assert_eq!(
        m("aa a|a\nba", ":%s/a//g"),
        vec![0..1, 1..2, 3..4, 4..5, 7..8]
    );
    assert_eq!(m("aa a|a\nba", ":2s/a"), vec![7..8]);
    // Nothing while the pattern is empty, invalid mid-typing, or the line
    // isn't a substitution.
    assert!(m("a|a", ":s/").is_empty());
    assert!(m("a|a", ":s/[").is_empty());
    assert!(m("a|a", ":wq").is_empty());
    assert!(m("a|a", "/a").is_empty());
}

// --- Which-key hints and the [vim.keymap] user map -------------------------

/// [`run`] with a `[vim.keymap]` user map installed before the keys feed.
#[track_caller]
fn run_mapped(spec: &str, keys: &str, map: &[(&str, UserCmd)]) -> Buf {
    let (text, cursor) = parse_spec(spec);
    let mut buf = Buf::new(&text, cursor);
    buf.vim = VimState::with_user_map(map.iter().map(|(s, c)| (s.to_string(), *c)).collect());
    buf.feed(keys);
    buf
}

#[test]
fn which_key_hints_for_pending_states() {
    let hints = |keys: &str| run("|abc def\nghi", keys).vim.which_key_hints();
    // Every multi-key pending state offers continuations…
    for keys in [
        "d", "2d", "c", "y", "g", "dg", "Z", "z", ",", "di", "ya", "vi", "ys", "cs", "ds", ">",
        "gq", "ysiw",
    ] {
        assert!(!hints(keys).is_empty(), "{keys:?} should hint");
    }
    // …while idle, a bare count, the prompts, and the single-char waits
    // (f/r target) hint nothing.
    for keys in ["", "2", "/", "/ab", ":", ":s/a", "?", "f", "r", "i"] {
        assert!(hints(keys).is_empty(), "{keys:?} should not hint");
    }
    // The operator table leads with the doubled (linewise) key.
    assert_eq!(hints("d")[0].0, "d");
    assert_eq!(hints("gq")[0].0, "q");
    assert_eq!(hints("<lt>")[0].0, "<");
    // Z is the commit/cancel pair.
    assert_eq!(
        hints("Z"),
        vec![
            ("Z".to_string(), "Commit".to_string()),
            ("Q".to_string(), "Cancel".to_string())
        ]
    );
}

#[test]
fn user_map_single_key_fires() {
    for (name, cmd, want) in [
        ("commit", UserCmd::Commit, Action::Commit),
        ("cancel", UserCmd::Cancel, Action::Quit { force: false }),
        ("discard", UserCmd::Discard, Action::Quit { force: true }),
        ("help", UserCmd::Help, Action::Help),
    ] {
        let buf = run_mapped("a|bc", "Q", &[("Q", cmd)]);
        assert!(
            buf.log.contains(&want),
            "{name}: wrong action {:?}",
            buf.log
        );
        assert!(!buf.beeped(), "{name}: unexpected beep");
        assert_eq!(buf.text, "abc", "{name}: buffer must not change");
    }
    // Reflow covers the whole message, like `,q`.
    let buf = run_mapped("one\ntwo |three", "R", &[("R", UserCmd::Reflow)]);
    assert!(
        buf.log.contains(&Action::ReflowRange(0..13)),
        "{:?}",
        buf.log
    );
}

#[test]
fn user_map_multi_key_sequences() {
    let map = &[(",w", UserCmd::Commit), ("qq", UserCmd::Cancel)];
    let buf = run_mapped("a|bc", ",w", map);
    assert!(buf.log.contains(&Action::Commit));
    assert!(!buf.beeped());
    let buf = run_mapped("a|bc", "qq", map);
    assert!(buf.log.contains(&Action::Quit { force: false }));
    // The typed prefix shows in the indicator while the sequence is pending.
    let buf = run_mapped("a|bc", ",", map);
    assert_eq!(buf.vim.pending_display().as_deref(), Some(","));
    assert_eq!(buf.vim.mode(), N);
}

#[test]
fn user_map_shadows_builtins_and_dead_ends_beep() {
    // `xy` makes `x` a user prefix: the built-in delete-char never fires, and
    // a dead end beeps without replaying the swallowed keys.
    let map = &[("xy", UserCmd::Commit)];
    let buf = run_mapped("a|bc", "x", map);
    assert_eq!(buf.text, "abc", "x must be swallowed, not delete");
    assert_eq!(buf.vim.pending_display().as_deref(), Some("x"));
    let buf = run_mapped("a|bc", "xz", map);
    assert!(buf.beeped());
    assert_eq!(buf.text, "abc", "dead end must not replay x as delete");
    assert!(!buf.log.contains(&Action::Commit));
    let buf = run_mapped("a|bc", "xy", map);
    assert!(buf.log.contains(&Action::Commit));
    // A `,`-leading mapping shadows the whole comma leader: the built-in `,c`
    // dies (distinct leaders are the documented recommendation).
    let map = &[(",w", UserCmd::Commit)];
    let buf = run_mapped("a|bc", ",c", map);
    assert!(buf.beeped());
    assert!(!buf.log.contains(&Action::Commit));
    // A full-sequence collision wins over the built-in: user ZZ cancels.
    let buf = run_mapped("a|bc", "ZZ", &[("ZZ", UserCmd::Cancel)]);
    assert!(buf.log.contains(&Action::Quit { force: false }));
    assert!(!buf.log.contains(&Action::Commit));
}

#[test]
fn user_map_prefix_shows_in_which_key() {
    let map = &[("Qa", UserCmd::Commit), ("Qb", UserCmd::Cancel)];
    let buf = run_mapped("a|bc", "Q", map);
    assert_eq!(
        buf.vim.which_key_hints(),
        vec![
            ("a".to_string(), "Commit".to_string()),
            ("b".to_string(), "Cancel".to_string())
        ]
    );
}

#[test]
fn user_map_parses_config_entries() {
    let entries: std::collections::BTreeMap<String, String> = [
        ("Q", "cancel"),
        (",w", "commit"),
        ("", "commit"),       // empty sequence: skipped
        ("a b", "commit"),    // whitespace: skipped
        ("bad", "not-a-cmd"), // unknown command: skipped
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(
        parse_user_map(&entries),
        vec![
            (",w".to_string(), UserCmd::Commit),
            ("Q".to_string(), UserCmd::Cancel)
        ]
    );
}

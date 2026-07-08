//! End-to-end engine tests: feed key sequences through [`VimState`] and
//! assert the resulting buffer, cursor, and mode. The tables encode observable
//! Vim behavior (verified against `:help motion.txt`, `:help text-objects`,
//! `:help word-motions`, and vim-surround), so they are the spec the engine
//! and its leaf modules must match.

use super::*;

const N: Mode = Mode::Normal;
const I: Mode = Mode::Insert;
const V: Mode = Mode::Visual { linewise: false };
const VL: Mode = Mode::Visual { linewise: true };

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
            Action::Repeat => {
                if let Some((keys, typed)) = self.vim.begin_repeat() {
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
            Action::Undo
            | Action::Redo
            | Action::Commit
            | Action::Quit
            | Action::Reflow
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

    fn beeped(&self) -> bool {
        self.log.contains(&Action::Beep)
    }

    fn undos(&self) -> usize {
        self.log.iter().filter(|a| **a == Action::Undo).count()
    }

    fn redos(&self) -> usize {
        self.log.iter().filter(|a| **a == Action::Redo).count()
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
        ("axbx|cx", "Fx,", "axbxc|x"), // `,` reverses the direction
        ("hello w|orld", "Fo,", "hello w|orld"),
        ("|axbxcxd", "2tx", "ax|bxcxd"),
    ] {
        check(spec, keys, want);
    }
    check_beep("|abc", "fz", "|abc"); // not on the line
    check_beep("ab|c\nzd", "fz", "ab|c\nzd"); // never crosses lines
    check_beep("|abc", ";", "|abc"); // no previous find
    check_beep("|abc", ",", "|abc");
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
    let buf = run("|abc", "u");
    assert_eq!((buf.undos(), buf.redos()), (1, 0));
    assert_eq!(show(&buf.text, buf.cursor), "|abc");
    assert!(!buf.beeped());

    // `u` clears a pending count: one Undo, and the following x deletes one.
    let buf = check_full("|abcd", "3ux", "|bcd", N, Some(false));
    assert_eq!(buf.undos(), 1);

    let buf = run("|abc", "<c-r>");
    assert_eq!((buf.undos(), buf.redos()), (0, 1));

    let buf = check_full("|abcd", "2<c-r>x", "|bcd", N, Some(false));
    assert_eq!(buf.redos(), 1);
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
            "hjkl0^$wbeWBE gG fFtT;,{}%xXsSrRdcyvVpPuJoOiIaA~\"'()[]{}<>bBqz1290+-"
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

// --- Dot repeat, editor commands, and search --------------------------------

#[test]
fn dot_repeat_simple() {
    check("|ab cd ef", "dw.", "|ef");
    check("a|bcd", "x..", "|a");
    check_any("|abc", "2x.", "|"); // the second 2x runs out of chars
    check("a|b ab", "xw.", "a |b");
    check_beep("|ab", ".", "|ab"); // nothing to repeat
                                   // An undo between doesn't clobber the recorded change.
    let buf = run("a|bcd", "xu.");
    assert_eq!(buf.undos(), 1); // (the harness doesn't apply undo itself)
    assert_eq!(buf.text, "ad"); // x repeated after u
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
    assert!(buf.log.contains(&Action::Quit), "ZQ cancels");
    let buf = run("|ab", "gq");
    assert!(buf.log.contains(&Action::Reflow), "gq reflows");
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

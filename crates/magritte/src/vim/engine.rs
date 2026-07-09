//! The modal state machine: keystrokes → [`Action`]s, given the buffer and
//! cursor. Owns everything that spans keystrokes — pending operators and
//! multi-key sequences, counts, the Visual anchor, the last `f`/`t` for
//! `;`/`,`, the unnamed register, and the desired column for `j`/`k`.
//!
//! Operator/motion combination follows `:help motion.txt`, including the
//! exclusive-motion adjustments (an exclusive motion ending in column 1 backs
//! up to the previous line end and turns inclusive; if the start is also in
//! the indent it turns linewise) and the `cw`-acts-like-`ce` special case.

use super::motion;
use super::surround;
use super::text_object;
use super::*;

/// The three operators. `Change` is delete-then-Insert.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn key(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        }
    }
}

/// Who consumes the next resolved motion or text object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Consumer {
    /// Bare motion: move the cursor.
    Move,
    /// `d`/`c`/`y` with the count typed before the operator (0 = none).
    Op { op: Op, count: usize },
    /// `ys` awaiting its target.
    SurroundAdd,
    /// `gq` awaiting its target: the covered lines get reflowed.
    Reflow,
    /// `>`/`<` awaiting its target: the covered lines shift by one indent
    /// step (with the count typed before the operator).
    Shift { dedent: bool, count: usize },
}

/// Mid-sequence state, cleared by `Esc` or any invalid key.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Pending {
    None,
    /// After `d`/`c`/`y`/`ys`: the next keys form a motion or text object.
    AwaitMotion(Consumer),
    /// After `f`/`F`/`t`/`T`: awaiting the target char.
    Find {
        kind: FindKind,
        consumer: Consumer,
    },
    /// After `i`/`a` with an operator (or in Visual): awaiting the object char.
    Object {
        consumer: Consumer,
        around: bool,
    },
    /// After `g`: awaiting `g`.
    G(Consumer),
    /// After `r`: awaiting the replacement char.
    Replace,
    /// A surround target range is fixed (from `ys{target}`, `yss`, or Visual
    /// `S`): awaiting the pair char.
    SurroundChar {
        start: usize,
        end: usize,
    },
    /// After `ds`: awaiting the pair char to remove.
    SurroundDelete,
    /// After `cs`: awaiting the pair char to replace…
    SurroundChangeFrom,
    /// …then the pair char to replace it with.
    SurroundChangeTo {
        from: char,
    },
    /// After `Z`: awaiting `Z` (commit) or `Q` (cancel).
    Z,
    /// After a bare `,` in Normal mode: `,`/`c` commit, `k` cancels
    /// (evil-collection's with-editor leader keys); any other key falls back
    /// to `,`'s reverse-find-repeat and then runs normally.
    Comma,
    /// `/` or `?`: collecting the search query (shown live in the mode bar);
    /// Enter executes, Esc cancels, Backspace edits.
    Search {
        query: String,
        back: bool,
    },
}

/// The unnamed register: the last yanked or deleted text.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Register {
    text: String,
    linewise: bool,
}

/// All cross-keystroke Vim state for one editor. Create with [`VimState::new`]
/// (starts in Normal); feed keys through [`VimState::handle_key`]. `Clone` is
/// for the render overlay, which snapshots the state into a paint closure.
#[derive(Clone)]
pub(crate) struct VimState {
    mode: Mode,
    pending: Pending,
    /// Digits typed so far for the count in progress (may sit before an
    /// operator, after it, or both — they multiply, as in Vim).
    count: String,
    /// Visual anchor: byte offset of the anchor char's start.
    anchor: usize,
    /// Column (in chars) that `j`/`k` aim for; `usize::MAX` after `$`.
    desired_col: Option<usize>,
    /// Last `f`/`F`/`t`/`T` for `;` and `,`.
    last_find: Option<(FindKind, char)>,
    register: Option<Register>,
    /// Last `/`/`?` query and direction, for `n`/`N` (and an empty `/`).
    last_search: Option<(String, bool)>,
    /// Keys of the command in progress, kept while a multi-key sequence (or
    /// an Insert session it opened) is still running — the candidate for
    /// [`Self::last_change`].
    recording: Vec<Key>,
    /// The last buffer-changing command for `.`: its keys, plus the text the
    /// Insert session it opened typed (captured between entry and Esc).
    last_change: Option<(Vec<Key>, String)>,
    /// Where Insert-mode typing began, for the `.` text capture.
    insert_entry: Option<usize>,
    /// True while the app replays a `.` — suppresses re-recording.
    replaying: bool,
    /// Vim-level undo: one `(text, cursor)` snapshot per change command (an
    /// Insert session is one unit). The widget's own history groups edits by
    /// time, which is right for typing but wrong for `u` after `dw..`.
    undos: Vec<(String, usize)>,
    redos: Vec<(String, usize)>,
    /// Set by `u`/`C-r` so their own restoring edit isn't snapshotted.
    in_undo: bool,
}

impl VimState {
    pub(crate) fn new() -> Self {
        VimState {
            mode: Mode::Normal,
            pending: Pending::None,
            count: String::new(),
            anchor: 0,
            desired_col: None,
            last_find: None,
            register: None,
            last_search: None,
            recording: Vec::new(),
            last_change: None,
            insert_entry: None,
            replaying: false,
            undos: Vec::new(),
            redos: Vec::new(),
            in_undo: false,
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn in_insert(&self) -> bool {
        self.mode == Mode::Insert
    }

    /// The in-progress key sequence for the mode bar (e.g. `2d`, `ys`, `f`).
    pub(crate) fn pending_display(&self) -> Option<String> {
        let mut s = self.count.clone();
        match &self.pending {
            Pending::None => {}
            Pending::AwaitMotion(c) => s.push_str(&consumer_keys(c)),
            Pending::Find { kind, consumer } => {
                s.push_str(&consumer_keys(consumer));
                s.push(match kind {
                    FindKind::FindFwd => 'f',
                    FindKind::FindBack => 'F',
                    FindKind::TillFwd => 't',
                    FindKind::TillBack => 'T',
                });
            }
            Pending::Object { consumer, around } => {
                s.push_str(&consumer_keys(consumer));
                s.push(if *around { 'a' } else { 'i' });
            }
            Pending::G(c) => {
                s.push_str(&consumer_keys(c));
                s.push('g');
            }
            Pending::Replace => s.push('r'),
            Pending::SurroundChar { .. } => s.push_str("ys"),
            Pending::SurroundDelete => s.push_str("ds"),
            Pending::SurroundChangeFrom => s.push_str("cs"),
            Pending::SurroundChangeTo { from } => {
                s.push_str("cs");
                s.push(*from);
            }
            Pending::Z => s.push('Z'),
            Pending::Comma => s.push(','),
            Pending::Search { query, back } => {
                s.push(if *back { '?' } else { '/' });
                s.push_str(query);
            }
        }
        (!s.is_empty()).then_some(s)
    }

    /// Abort any half-typed command (a mouse click is Vim's `Esc` for
    /// pending state: `d` then a click shouldn't leave the delete armed).
    pub(crate) fn cancel_pending(&mut self) {
        self.pending = Pending::None;
        self.count.clear();
    }

    /// Enter charwise Visual mode with the anchor at `anchor` — the app maps
    /// a completed mouse drag-selection onto Visual so the two selection
    /// models don't coexist.
    pub(crate) fn begin_visual(&mut self, text: &str, anchor: usize) {
        self.cancel_pending();
        self.desired_col = None;
        self.anchor = clamp_normal(text, anchor);
        self.mode = Mode::Visual { linewise: false };
    }

    /// The query being typed at a `/`/`?` prompt, for the live match
    /// highlight (None outside the prompt or while it's still empty).
    pub(crate) fn search_query(&self) -> Option<&str> {
        match &self.pending {
            Pending::Search { query, .. } if !query.is_empty() => Some(query),
            _ => None,
        }
    }

    /// The Visual selection as a byte range (for the overlay and operators):
    /// charwise includes both endpoint chars; linewise covers whole lines
    /// including the trailing newline.
    pub(crate) fn visual_range(&self, text: &str, cursor: usize) -> Option<Range<usize>> {
        let Mode::Visual { linewise } = self.mode else {
            return None;
        };
        let a = clamp_normal(text, self.anchor);
        let c = clamp_normal(text, cursor);
        let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
        if linewise {
            let end = line_end(text, hi);
            Some(line_start(text, lo)..(end + usize::from(end < text.len())))
        } else if self.desired_col == Some(usize::MAX) {
            // After `$` the selection runs to the end of the line including
            // its newline (Vim's curswant=MAXCOL: `v$d` joins the lines).
            let end = line_end(text, hi);
            Some(lo..(end + usize::from(end < text.len())).min(text.len()))
        } else {
            Some(lo..next_char(text, hi))
        }
    }

    /// Feed one keystroke. `text`/`cursor` are the buffer's current contents
    /// and cursor byte offset; the returned actions describe what to do to the
    /// buffer. Read [`VimState::mode`] afterwards for the indicator/routing.
    pub(crate) fn handle_key(&mut self, text: &str, cursor: usize, key: Key) -> Vec<Action> {
        match self.mode {
            Mode::Insert => self.key_insert(text, cursor, key),
            Mode::Normal | Mode::Visual { .. } => {
                let cursor = clamp_normal(text, cursor);
                if !self.replaying {
                    self.recording.push(key);
                }
                let actions = self.key_modal(text, cursor, key);
                let edited = actions
                    .iter()
                    .any(|a| matches!(a, Action::Edit(_) | Action::ReflowRange(_)));
                // Any edit resets the sticky column, like Vim's curswant
                // (`$x` then `j` aims at the deletion column, not line end).
                if edited {
                    self.desired_col = None;
                }
                // One undo snapshot per change command; a command that opens
                // an Insert session snapshots here so the whole session is a
                // single undo unit, like Vim. Undo/redo's own restoring edit
                // is neither snapshotted nor recorded as a repeatable change.
                let was_undo = self.in_undo;
                self.in_undo = false;
                if was_undo {
                    self.recording.clear();
                } else {
                    if edited || self.mode == Mode::Insert {
                        self.undos.push((text.to_string(), cursor));
                        if self.undos.len() > 200 {
                            self.undos.remove(0);
                        }
                        self.redos.clear();
                    }
                    if !self.replaying {
                        self.remember_change(cursor, edited, &actions);
                    }
                }
                actions
            }
        }
    }

    /// Track the keys of the change in progress for `.`. A command that
    /// edited the buffer becomes the repeatable change; one that opened an
    /// Insert session stays open until [`Self::key_insert`] sees Esc and
    /// captures the typed text; anything else (a completed motion, yank,
    /// beep…) discards the recording.
    fn remember_change(&mut self, cursor: usize, edited: bool, actions: &[Action]) {
        if self.mode == Mode::Insert {
            // Typing starts wherever the command left the cursor.
            self.insert_entry = Some(
                actions
                    .iter()
                    .rev()
                    .find_map(|a| match a {
                        Action::Edit(e) => Some(e.cursor),
                        Action::MoveCursor(p) => Some(*p),
                        _ => None,
                    })
                    .unwrap_or(cursor),
            );
        } else if edited {
            self.last_change = Some((std::mem::take(&mut self.recording), String::new()));
        } else if self.pending == Pending::None
            && self.count.is_empty()
            && !matches!(self.mode, Mode::Visual { .. })
        {
            self.recording.clear();
        }
    }

    fn key_insert(&mut self, text: &str, cursor: usize, key: Key) -> Vec<Action> {
        // Only Esc is routed here; everything else goes straight to the input.
        if key != Key::Escape {
            return Vec::new();
        }
        self.mode = Mode::Normal;
        self.clear_pending();
        // Leaving Insert re-anchors the sticky column at the cursor.
        self.desired_col = None;
        let cursor = cursor.min(text.len());
        // An Insert session that left the buffer as it was isn't a change:
        // drop its undo snapshot (Vim adds no undo level for `i<Esc>`).
        if self.undos.last().is_some_and(|(t, _)| t == text) {
            self.undos.pop();
        }
        // Close the `.` recording: the change is the command's keys plus what
        // the Insert session typed (best-effort — the slice from entry to the
        // exit cursor covers plain typing; edits that moved before the entry
        // point just record less).
        if !self.replaying && !self.recording.is_empty() {
            let entry = self.insert_entry.take().unwrap_or(cursor);
            let typed =
                if entry <= cursor && text.is_char_boundary(entry) && text.is_char_boundary(cursor)
                {
                    text[entry..cursor].to_string()
                } else {
                    String::new()
                };
            self.last_change = Some((std::mem::take(&mut self.recording), typed));
        }
        // Step left one column, like Vim, unless at the line start.
        let pos = if cursor > line_start(text, cursor) {
            prev_char(text, cursor)
        } else {
            cursor
        };
        vec![Action::MoveCursor(clamp_normal(text, pos))]
    }

    /// Start a `.` replay: the recorded keys and the Insert text to re-type.
    /// [`Self::end_repeat`] must be called after feeding them back.
    pub(crate) fn begin_repeat(&mut self) -> Option<(Vec<Key>, String)> {
        let change = self.last_change.clone();
        if change.is_some() {
            self.replaying = true;
        }
        change
    }

    pub(crate) fn end_repeat(&mut self) {
        self.replaying = false;
    }

    #[cfg(test)]
    pub(crate) fn undo_stack(&self) -> &[(String, usize)] {
        &self.undos
    }

    /// An edit made outside the engine (the ⌥q reflow) still gets a Vim undo
    /// level. In Insert mode the open session's snapshot already covers it.
    pub(crate) fn note_external_change(&mut self, text: &str, cursor: usize) {
        if self.mode == Mode::Insert {
            return;
        }
        self.undos.push((text.to_string(), cursor));
        if self.undos.len() > 200 {
            self.undos.remove(0);
        }
        self.redos.clear();
    }

    fn key_modal(&mut self, text: &str, cursor: usize, key: Key) -> Vec<Action> {
        if key == Key::Escape {
            self.clear_pending();
            self.mode = Mode::Normal;
            return Vec::new();
        }

        // States that consume a raw character, before command dispatch.
        match self.pending.clone() {
            Pending::Find { kind, consumer } => {
                let Key::Char(target) = key else {
                    return self.beep();
                };
                self.pending = Pending::None;
                self.last_find = Some((kind, target));
                return self.resolve_motion(
                    text,
                    cursor,
                    Motion::Find {
                        kind,
                        target,
                        repeat: false,
                    },
                    consumer,
                );
            }
            Pending::Replace => {
                // `r<Enter>` replaces with a line break (`:help r`).
                let c = match key {
                    Key::Char(c) => c,
                    Key::Enter => '\n',
                    _ => return self.beep(),
                };
                self.pending = Pending::None;
                // Visual `r`: every selected char (newlines aside) becomes `c`.
                if let Some(range) = self.visual_range(text, cursor) {
                    self.mode = Mode::Normal;
                    self.take_count();
                    let replaced: String = text[range.clone()]
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { c })
                        .collect();
                    return vec![Action::Edit(EditOp {
                        cursor: range.start,
                        range,
                        text: replaced,
                    })];
                }
                let count = self.take_count().max(1);
                return self.replace_chars(text, cursor, c, count);
            }
            Pending::Object { consumer, around } => {
                let Key::Char(obj) = key else {
                    return self.beep();
                };
                self.pending = Pending::None;
                return self.resolve_object(text, cursor, around, obj, consumer);
            }
            Pending::G(consumer) => {
                self.pending = Pending::None;
                // `gq`: the reflow operator. In Visual it acts on the
                // selection at once; in Normal it awaits a motion/object
                // (`gqq` for lines, like `dd`).
                if key == Key::Char('q') && consumer == Consumer::Move {
                    if let Some(range) = self.visual_range(text, cursor) {
                        self.mode = Mode::Normal;
                        self.take_count();
                        return vec![Action::ReflowRange(range)];
                    }
                    self.pending = Pending::AwaitMotion(Consumer::Reflow);
                    return Vec::new();
                }
                let Key::Char('g') = key else {
                    return self.beep();
                };
                // A count typed before the operator is a line number too:
                // `2dgg` == `d2gg` (and they multiply, as elsewhere).
                let line = Some(goto_count(consumer, self.take_count()).max(1));
                return self.resolve_motion(text, cursor, Motion::GotoLine(line), consumer);
            }
            Pending::SurroundChar { start, end } => {
                let Key::Char(c) = key else {
                    return self.beep();
                };
                self.pending = Pending::None;
                let Some(edit) = surround::add(text, start..end, c) else {
                    return self.beep();
                };
                self.mode = Mode::Normal;
                return vec![Action::Edit(edit)];
            }
            Pending::SurroundDelete => {
                let Key::Char(c) = key else {
                    return self.beep();
                };
                self.pending = Pending::None;
                let Some(edit) = surround::delete(text, cursor, c) else {
                    return self.beep();
                };
                return vec![Action::Edit(edit)];
            }
            Pending::SurroundChangeFrom => {
                let Key::Char(c) = key else {
                    return self.beep();
                };
                self.pending = Pending::SurroundChangeTo { from: c };
                return Vec::new();
            }
            Pending::SurroundChangeTo { from } => {
                let Key::Char(to) = key else {
                    return self.beep();
                };
                self.pending = Pending::None;
                let Some(edit) = surround::change(text, cursor, from, to) else {
                    return self.beep();
                };
                return vec![Action::Edit(edit)];
            }
            Pending::Z => {
                self.pending = Pending::None;
                return match key {
                    Key::Char('Z') => vec![Action::Commit],
                    Key::Char('Q') => vec![Action::Quit],
                    _ => self.beep(),
                };
            }
            Pending::Comma => {
                self.pending = Pending::None;
                match key {
                    Key::Char(',') | Key::Char('c') => return vec![Action::Commit],
                    Key::Char('k') => return vec![Action::Quit],
                    // `,q`: reflow the whole message (the app skips the
                    // summary line, like ⌥q).
                    Key::Char('q') => return vec![Action::ReflowRange(0..text.len())],
                    _ => {
                        // Not a leader command: the comma meant reverse-find
                        // repeat. Run it, then this key from the landing spot
                        // (the buffer is unchanged by a move, so only the
                        // cursor needs forwarding).
                        let mut acts = Vec::new();
                        let mut at = cursor;
                        if let Some((kind, target)) = self.last_find {
                            let m = Motion::Find {
                                kind: kind.reversed(),
                                target,
                                repeat: true,
                            };
                            acts = self.resolve_motion(text, cursor, m, Consumer::Move);
                            if let Some(Action::MoveCursor(p)) = acts.last() {
                                at = *p;
                            }
                        }
                        acts.extend(self.key_modal(text, at, key));
                        return acts;
                    }
                }
            }
            Pending::Search { mut query, back } => {
                match key {
                    Key::Char(c) => {
                        query.push(c);
                        self.pending = Pending::Search { query, back };
                    }
                    Key::Backspace => {
                        // Backspace on an empty query cancels, like Vim.
                        if query.pop().is_some() {
                            self.pending = Pending::Search { query, back };
                        } else {
                            self.pending = Pending::None;
                        }
                    }
                    Key::Enter => {
                        self.pending = Pending::None;
                        // An empty `/` repeats the last search.
                        if !query.is_empty() {
                            self.last_search = Some((query, back));
                        }
                        return self.search(text, cursor, back);
                    }
                    _ => return self.beep(),
                }
                return Vec::new();
            }
            Pending::None | Pending::AwaitMotion(_) => {}
        }

        // Count digits: 1-9 always; 0 only continues a count. Capped at nine
        // digits, like Vim caps huge counts, so no arithmetic can overflow.
        if let Key::Char(c) = key {
            if c.is_ascii_digit() && (c != '0' || !self.count.is_empty()) {
                if self.count.len() < 9 {
                    self.count.push(c);
                }
                return Vec::new();
            }
        }

        let consumer = match self.pending {
            Pending::AwaitMotion(c) => c,
            _ => Consumer::Move,
        };

        // A bare `,` in Normal mode is the with-editor leader (`,,`/`,c`
        // commit, `,k` cancel); with an operator pending or in Visual it
        // stays the reverse-find repeat.
        if key == Key::Char(',') && consumer == Consumer::Move && self.mode == Mode::Normal {
            self.pending = Pending::Comma;
            return Vec::new();
        }

        // `G`: the count — typed before or after an operator — is an absolute
        // line number, not a repeat.
        if key == Key::Char('G') {
            self.pending = Pending::None;
            let count = goto_count(consumer, self.take_count());
            let line = (count > 0).then_some(count);
            return self.resolve_motion(text, cursor, Motion::GotoLine(line), consumer);
        }

        // Keys that are motions regardless of spelling.
        let motion = match key {
            Key::Enter => Some(Motion::NextLineStart),
            Key::Left => Some(Motion::Left),
            Key::Right => Some(Motion::Right),
            Key::Up => Some(Motion::Up),
            Key::Down => Some(Motion::Down),
            Key::Backspace => Some(Motion::BackspaceLeft),
            Key::Ctrl('n') => Some(Motion::Down),
            Key::Ctrl('p') => Some(Motion::Up),
            Key::Char(c) => char_motion(c, self.last_find),
            _ => None,
        };
        if let Some(m) = motion {
            self.pending = Pending::None;
            if let (Motion::Find { kind, .. }, Key::Char('f' | 'F' | 't' | 'T')) = (m, key) {
                // `f` itself: park until the target char arrives. (`;`/`,`
                // arrive with their stored target and fall through.)
                self.pending = Pending::Find { kind, consumer };
                return Vec::new();
            }
            return self.resolve_motion(text, cursor, m, consumer);
        }

        match key {
            Key::Ctrl('r') if consumer == Consumer::Move && self.mode == Mode::Normal => {
                self.take_count();
                let Some((next_text, next_cursor)) = self.redos.pop() else {
                    return self.beep();
                };
                self.undos.push((text.to_string(), cursor));
                self.in_undo = true;
                vec![Action::Edit(EditOp {
                    range: 0..text.len(),
                    text: next_text,
                    cursor: next_cursor,
                })]
            }
            Key::Char(c) => self.char_command(text, cursor, c, consumer),
            _ => self.beep(),
        }
    }

    /// Commands that aren't motions or digit/pending input.
    fn char_command(
        &mut self,
        text: &str,
        cursor: usize,
        c: char,
        consumer: Consumer,
    ) -> Vec<Action> {
        // Operator-pending: doubled operator = linewise on count lines;
        // `s` after `y`/`c`/`d` = surround; `i`/`a` = text object.
        if let Consumer::Op { op, count } = consumer {
            match c {
                _ if c == op.key() => {
                    self.pending = Pending::None;
                    let count = total(count, self.take_count());
                    return self.op_lines(text, cursor, op, count);
                }
                's' if op == Op::Yank => {
                    self.pending = Pending::AwaitMotion(Consumer::SurroundAdd);
                    return Vec::new();
                }
                's' if op == Op::Delete => {
                    self.pending = Pending::SurroundDelete;
                    return Vec::new();
                }
                's' if op == Op::Change => {
                    self.pending = Pending::SurroundChangeFrom;
                    return Vec::new();
                }
                'i' | 'a' => {
                    self.pending = Pending::Object {
                        consumer,
                        around: c == 'a',
                    };
                    return Vec::new();
                }
                'g' => {
                    self.pending = Pending::G(consumer);
                    return Vec::new();
                }
                _ => return self.beep(),
            }
        }
        if consumer == Consumer::SurroundAdd {
            match c {
                // `yss`: the line's content, sans leading/trailing blanks.
                's' => {
                    let start = first_non_blank(text, cursor);
                    let mut end = line_end(text, cursor);
                    while end > start
                        && matches!(char_at(text, prev_char(text, end)), Some(' ' | '\t'))
                    {
                        end = prev_char(text, end);
                    }
                    if start >= end {
                        return self.beep();
                    }
                    self.pending = Pending::SurroundChar { start, end };
                    return Vec::new();
                }
                'i' | 'a' => {
                    self.pending = Pending::Object {
                        consumer,
                        around: c == 'a',
                    };
                    return Vec::new();
                }
                'g' => {
                    self.pending = Pending::G(consumer);
                    return Vec::new();
                }
                _ => return self.beep(),
            }
        }
        if let Consumer::Shift { dedent, count } = consumer {
            match c {
                // `>>`/`<<`: the current `count` lines, like `dd`.
                _ if c == if dedent { '<' } else { '>' } => {
                    self.pending = Pending::None;
                    let count = total(count, self.take_count());
                    let start = line_start(text, cursor);
                    let mut end = line_end(text, cursor);
                    for _ in 1..count {
                        if end >= text.len() {
                            break;
                        }
                        end = line_end(text, next_char(text, end));
                    }
                    return self.shift_lines(text, start..end, dedent);
                }
                'i' | 'a' => {
                    self.pending = Pending::Object {
                        consumer,
                        around: c == 'a',
                    };
                    return Vec::new();
                }
                'g' => {
                    self.pending = Pending::G(consumer);
                    return Vec::new();
                }
                _ => return self.beep(),
            }
        }
        if consumer == Consumer::Reflow {
            match c {
                // `gqq`: the current `count` lines, like Vim — an overlong
                // line breaks onto new lines (nothing joins upward; the
                // paragraph form is `gqip`).
                'q' => {
                    self.pending = Pending::None;
                    let count = self.take_count().max(1);
                    let start = line_start(text, cursor);
                    let mut end = line_end(text, cursor);
                    for _ in 1..count {
                        if end >= text.len() {
                            break;
                        }
                        end = line_end(text, next_char(text, end));
                    }
                    return vec![Action::ReflowRange(start..end)];
                }
                'i' | 'a' => {
                    self.pending = Pending::Object {
                        consumer,
                        around: c == 'a',
                    };
                    return Vec::new();
                }
                'g' => {
                    self.pending = Pending::G(consumer);
                    return Vec::new();
                }
                _ => return self.beep(),
            }
        }

        // Visual-mode commands.
        if let Mode::Visual { linewise } = self.mode {
            match c {
                'd' | 'x' => return self.visual_op(text, cursor, Op::Delete),
                'c' | 's' => return self.visual_op(text, cursor, Op::Change),
                'y' => return self.visual_op(text, cursor, Op::Yank),
                // The uppercase forms operate on whole lines regardless of
                // the visual kind (`:help v_Y`).
                'Y' => {
                    self.mode = Mode::Visual { linewise: true };
                    return self.visual_op(text, cursor, Op::Yank);
                }
                'D' | 'X' => {
                    self.mode = Mode::Visual { linewise: true };
                    return self.visual_op(text, cursor, Op::Delete);
                }
                'C' | 'R' => {
                    self.mode = Mode::Visual { linewise: true };
                    return self.visual_op(text, cursor, Op::Change);
                }
                // `u`/`U`: set the selection's case.
                'u' | 'U' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    self.mode = Mode::Normal;
                    let recased: String = if c == 'u' {
                        text[range.clone()].to_lowercase()
                    } else {
                        text[range.clone()].to_uppercase()
                    };
                    return vec![Action::Edit(EditOp {
                        cursor: range.start,
                        range,
                        text: recased,
                    })];
                }
                'S' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    // Linewise S wraps the lines' content (no trailing \n).
                    let mut end = range.end;
                    if linewise {
                        while end > range.start
                            && matches!(char_at(text, prev_char(text, end)), Some('\n'))
                        {
                            end = prev_char(text, end);
                        }
                    }
                    self.pending = Pending::SurroundChar {
                        start: range.start,
                        end,
                    };
                    return Vec::new();
                }
                'o' => {
                    let a = self.anchor;
                    self.anchor = clamp_normal(text, cursor);
                    return vec![Action::MoveCursor(clamp_normal(text, a))];
                }
                'v' => {
                    self.mode = if linewise {
                        Mode::Visual { linewise: false }
                    } else {
                        Mode::Normal
                    };
                    return Vec::new();
                }
                'V' => {
                    self.mode = if linewise {
                        Mode::Normal
                    } else {
                        Mode::Visual { linewise: true }
                    };
                    return Vec::new();
                }
                'i' | 'a' => {
                    self.pending = Pending::Object {
                        consumer: Consumer::Move,
                        around: c == 'a',
                    };
                    return Vec::new();
                }
                'p' | 'P' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    let Some(reg) = self.register.clone() else {
                        // A failed visual command still leaves Visual mode.
                        self.mode = Mode::Normal;
                        return self.beep();
                    };
                    self.mode = Mode::Normal;
                    return self.visual_put(text, range, linewise, reg);
                }
                'g' => {
                    self.pending = Pending::G(Consumer::Move);
                    return Vec::new();
                }
                'r' => {
                    self.pending = Pending::Replace;
                    return Vec::new();
                }
                'J' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    self.mode = Mode::Normal;
                    // The selection's end is one past its last char; the last
                    // selected LINE is the one holding the char before it.
                    let last = prev_char(text, range.end).max(range.start);
                    return self.join_range(text, range.start..last);
                }
                '~' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    self.mode = Mode::Normal;
                    let toggled: String = text[range.clone()]
                        .chars()
                        .flat_map(toggle_char_case)
                        .collect();
                    return vec![Action::Edit(EditOp {
                        cursor: range.start,
                        range,
                        text: toggled,
                    })];
                }
                '>' | '<' => {
                    let Some(range) = self.visual_range(text, cursor) else {
                        return self.beep();
                    };
                    self.mode = Mode::Normal;
                    self.take_count();
                    return self.shift_lines(text, range, c == '<');
                }
                _ => return self.beep(),
            }
        }

        // Normal-mode commands (nothing pending).
        match c {
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' => self.enter_insert(text, cursor, c),
            'd' => self.start_op(Op::Delete),
            'c' => self.start_op(Op::Change),
            'y' => self.start_op(Op::Yank),
            '>' | '<' => {
                let count = self.take_count();
                self.pending = Pending::AwaitMotion(Consumer::Shift {
                    dedent: c == '<',
                    count,
                });
                Vec::new()
            }
            'v' => {
                self.take_count();
                self.anchor = cursor;
                self.mode = Mode::Visual { linewise: false };
                // An earlier `$`'s sticky end-of-line must not leak into the
                // selection (only `$` pressed in Visual extends to the
                // newline); re-anchor the column here.
                self.desired_col = None;
                Vec::new()
            }
            'V' => {
                self.take_count();
                self.anchor = cursor;
                self.mode = Mode::Visual { linewise: true };
                self.desired_col = None;
                Vec::new()
            }
            'x' => {
                let count = self.take_count().max(1);
                self.delete_chars_forward(text, cursor, count)
            }
            'X' => {
                let count = self.take_count().max(1);
                self.delete_chars_backward(text, cursor, count)
            }
            's' => {
                // Substitute = change `count` chars; on an empty line it just
                // enters Insert (op_on_range's empty-Change case).
                let count = self.take_count().max(1);
                let mut to = cursor;
                let end = line_end(text, cursor);
                for _ in 0..count {
                    if to >= end {
                        break;
                    }
                    to = next_char(text, to);
                }
                self.op_on_range(text, Op::Change, cursor..to, false, cursor)
            }
            'D' => {
                let count = self.take_count().max(1);
                self.op_to_line_end(text, cursor, Op::Delete, count)
            }
            'C' => {
                let count = self.take_count().max(1);
                self.op_to_line_end(text, cursor, Op::Change, count)
            }
            'Y' => {
                let count = self.take_count().max(1);
                self.op_lines(text, cursor, Op::Yank, count)
            }
            'S' => {
                let count = self.take_count().max(1);
                self.op_lines(text, cursor, Op::Change, count)
            }
            'r' => {
                self.pending = Pending::Replace;
                Vec::new()
            }
            '~' => {
                let count = self.take_count().max(1);
                self.toggle_case(text, cursor, count)
            }
            'J' => {
                let count = self.take_count().max(2);
                let start = line_start(text, cursor);
                let mut end = line_end(text, cursor);
                for _ in 1..count {
                    if end >= text.len() {
                        break;
                    }
                    end = line_end(text, next_char(text, end));
                }
                self.join_range(text, start..end)
            }
            'p' => {
                let count = self.take_count().max(1);
                self.put(text, cursor, count, true)
            }
            'P' => {
                let count = self.take_count().max(1);
                self.put(text, cursor, count, false)
            }
            'u' => {
                self.take_count();
                // Skip snapshots identical to the current text (a reflow that
                // turned out to change nothing).
                let mut top = self.undos.pop();
                while top.as_ref().is_some_and(|(t, _)| t == text) {
                    top = self.undos.pop();
                }
                let Some((prev_text, prev_cursor)) = top else {
                    return self.beep();
                };
                self.redos.push((text.to_string(), cursor));
                self.in_undo = true;
                vec![Action::Edit(EditOp {
                    range: 0..text.len(),
                    text: prev_text,
                    cursor: prev_cursor,
                })]
            }
            'g' => {
                self.pending = Pending::G(Consumer::Move);
                Vec::new()
            }
            '.' => {
                self.take_count();
                if self.last_change.is_none() {
                    return self.beep();
                }
                vec![Action::Repeat]
            }
            'Z' => {
                self.take_count();
                self.pending = Pending::Z;
                Vec::new()
            }
            '/' | '?' => {
                self.take_count();
                self.pending = Pending::Search {
                    query: String::new(),
                    back: c == '?',
                };
                Vec::new()
            }
            'n' | 'N' => {
                self.take_count();
                let Some(dir) = self.last_search.as_ref().map(|(_, back)| *back) else {
                    return self.beep();
                };
                self.search(text, cursor, dir != (c == 'N'))
            }
            _ => self.beep(),
        }
    }

    /// Jump to the next occurrence of the last search pattern — a regex with
    /// Vim 'smartcase' (no literal uppercase matches any case) — wrapping
    /// around the buffer like Vim's default 'wrapscan'.
    fn search(&mut self, text: &str, cursor: usize, back: bool) -> Vec<Action> {
        let Some(re) = self
            .last_search
            .as_ref()
            .and_then(|(query, _)| compile_search(query))
        else {
            return self.beep();
        };
        let found = if back {
            // Last match before the cursor, else the last match anywhere.
            let (mut before, mut any) = (None, None);
            let mut pos = 0;
            while let Some((s0, _)) = re.find_from(text, pos) {
                if s0 < cursor {
                    before = Some(s0);
                }
                any = Some(s0);
                if s0 >= text.len() {
                    break;
                }
                pos = next_char(text, s0);
            }
            before.or(any)
        } else {
            re.find_from(text, next_char(text, cursor))
                .or_else(|| re.find_from(text, 0))
                .map(|(s0, _)| s0)
        };
        let Some(pos) = found else {
            return self.beep();
        };
        let pos = clamp_normal(text, pos);
        self.desired_col = Some(char_col(text, pos));
        vec![Action::MoveCursor(pos)]
    }

    // --- Motion / object resolution ------------------------------------

    fn resolve_motion(
        &mut self,
        text: &str,
        cursor: usize,
        mut m: Motion,
        consumer: Consumer,
    ) -> Vec<Action> {
        let count = match consumer {
            Consumer::Op { count, .. } | Consumer::Shift { count, .. } => {
                total(count, self.take_count())
            }
            _ => self.take_count().max(1),
        };
        // `cw`/`cW` on a non-blank doesn't take the trailing blanks (`:help
        // cw`): change through the end of the current class run — which,
        // unlike `e`, never jumps to the next word from a word's last char.
        if let (Consumer::Op { op: Op::Change, .. }, Motion::WordForward { big }) = (consumer, m) {
            if let Some(c) =
                char_at(text, cursor).filter(|c| char_class(*c, big) != CharClass::Blank)
            {
                if count == 1 {
                    let cls = char_class(c, big);
                    let mut end = cursor;
                    loop {
                        let n = next_char(text, end);
                        match char_at(text, n).filter(|_| n > end) {
                            Some(c2) if char_class(c2, big) == cls => end = n,
                            _ => break,
                        }
                    }
                    let target = MotionTarget {
                        pos: end,
                        kind: MotionKind::Inclusive,
                    };
                    return self.apply_operator(text, cursor, Op::Change, m, target);
                }
                m = Motion::WordEnd { big };
            }
        }
        let desired = self.desired_col.unwrap_or_else(|| char_col(text, cursor));
        // The first vertical move fixes the sticky column so later `j`/`k`
        // aim for it even across shorter lines.
        if matches!(m, Motion::Down | Motion::Up) {
            self.desired_col = Some(desired);
        }
        let target = match motion::eval(text, cursor, count, m, desired) {
            Some(t) => t,
            // `ye`/`de` at the end of the buffer's last word still take the
            // char under the cursor (plain `e` fails there and beeps).
            None => match (consumer, m) {
                (Consumer::Op { .. }, Motion::WordEnd { big })
                    if char_at(text, cursor)
                        .is_some_and(|c| char_class(c, big) != CharClass::Blank) =>
                {
                    MotionTarget {
                        pos: cursor,
                        kind: MotionKind::Inclusive,
                    }
                }
                _ => return self.beep(),
            },
        };
        match consumer {
            Consumer::Move => {
                let pos = clamp_normal(text, target.pos);
                if pos == cursor
                    && target.pos != cursor
                    && !matches!(m, Motion::ParagraphForward | Motion::ParagraphBack)
                {
                    // The raw landing clamped back to where we started (`l` at
                    // the line's last char, `w` at the buffer's): a failed
                    // move. Paragraph motions are exempt — `}` parking at EOF
                    // from the last char is a quiet success in Vim.
                    return self.beep();
                }
                self.after_move(text, m, target.pos);
                vec![Action::MoveCursor(pos)]
            }
            Consumer::Op { op, .. } => self.apply_operator(text, cursor, op, m, target),
            Consumer::SurroundAdd => {
                let range = match operator_range(text, cursor, m, target) {
                    Some((range, _linewise)) => range,
                    None => return self.beep(),
                };
                self.pending = Pending::SurroundChar {
                    start: range.start,
                    end: range.end,
                };
                Vec::new()
            }
            Consumer::Reflow => {
                let Some((range, _)) = operator_range(text, cursor, m, target) else {
                    return self.beep();
                };
                vec![Action::ReflowRange(range)]
            }
            Consumer::Shift { dedent, .. } => {
                let Some((range, _)) = operator_range(text, cursor, m, target) else {
                    return self.beep();
                };
                self.shift_lines(text, range, dedent)
            }
        }
    }

    fn resolve_object(
        &mut self,
        text: &str,
        cursor: usize,
        around: bool,
        obj: char,
        consumer: Consumer,
    ) -> Vec<Action> {
        let count = match consumer {
            Consumer::Op { count, .. } | Consumer::Shift { count, .. } => {
                total(count, self.take_count())
            }
            _ => self.take_count().max(1),
        };
        let Some(range) = text_object::text_object(text, cursor, around, obj, count) else {
            return self.beep();
        };
        match consumer {
            Consumer::Move => {
                // Visual object selection (`viw`): reshape the selection.
                if matches!(self.mode, Mode::Visual { .. }) && range.start < range.end {
                    self.anchor = range.start;
                    return vec![Action::MoveCursor(clamp_normal(
                        text,
                        prev_char(text, range.end),
                    ))];
                }
                self.beep()
            }
            // An empty object (`diw` on an empty line, `di"` on `""`) moves
            // the cursor into place without editing; change still enters
            // Insert there (via op_on_range).
            Consumer::Op { op, .. } if range.start >= range.end && op != Op::Change => {
                vec![Action::MoveCursor(clamp_normal(text, range.start))]
            }
            Consumer::Op { op, .. } => {
                // An inner block covering whole lines operates linewise
                // (`ci{` on a multiline block leaves an empty line; `dip`
                // deletes lines).
                let linewise = matches!(
                    obj,
                    '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' | 'p'
                ) && range.start < range.end
                    && range.start == line_start(text, range.start)
                    && range.end == line_start(text, range.end);
                self.op_on_range(text, op, range, linewise, cursor)
            }
            Consumer::SurroundAdd => {
                self.pending = Pending::SurroundChar {
                    start: range.start,
                    end: range.end,
                };
                Vec::new()
            }
            Consumer::Reflow => {
                if range.start >= range.end {
                    return self.beep();
                }
                vec![Action::ReflowRange(range)]
            }
            Consumer::Shift { dedent, .. } => {
                if range.start >= range.end {
                    return self.beep();
                }
                self.shift_lines(text, range, dedent)
            }
        }
    }

    // --- Operators -------------------------------------------------------

    fn start_op(&mut self, op: Op) -> Vec<Action> {
        let count = self.take_count();
        self.pending = Pending::AwaitMotion(Consumer::Op { op, count });
        Vec::new()
    }

    fn apply_operator(
        &mut self,
        text: &str,
        cursor: usize,
        op: Op,
        m: Motion,
        target: MotionTarget,
    ) -> Vec<Action> {
        let Some((range, linewise)) = operator_range(text, cursor, m, target) else {
            return self.beep();
        };
        self.op_on_range(text, op, range, linewise, cursor)
    }

    /// Apply an operator to a resolved byte range. `cursor` is where the
    /// command started, for yank's cursor-placement rules.
    fn op_on_range(
        &mut self,
        text: &str,
        op: Op,
        range: Range<usize>,
        linewise: bool,
        cursor: usize,
    ) -> Vec<Action> {
        if range.start >= range.end {
            // Change on an empty target ("s" on an empty line, ci( on "()")
            // still enters Insert; delete/yank of nothing is a failed command.
            if op == Op::Change {
                self.mode = Mode::Insert;
                return vec![Action::MoveCursor(range.start.min(text.len()))];
            }
            return self.beep();
        }
        let yanked = text[range.clone()].to_string();
        // Linewise registers always carry the trailing newline (a linewise
        // range at EOF may lack one in the buffer); the system clipboard gets
        // the same normalized text.
        let reg_text = if linewise && !yanked.ends_with('\n') {
            format!("{yanked}\n")
        } else {
            yanked
        };
        self.register = Some(Register {
            text: reg_text.clone(),
            linewise,
        });
        match op {
            Op::Yank => {
                // Charwise: cursor to the start of the yanked text (a no-op
                // for forward motions). Linewise: first yanked line, keeping
                // the column (`yk` moves up, `yy`/`yj` stay put).
                let pos = if linewise {
                    offset_at_col(text, range.start, char_col(text, cursor))
                } else {
                    cursor.min(range.start)
                };
                vec![
                    Action::Yank(reg_text),
                    Action::MoveCursor(clamp_normal(text, pos)),
                ]
            }
            Op::Delete => {
                let mut range = range;
                if linewise && range.end >= text.len() && range.start > 0 {
                    // Deleting through the last line eats the newline before
                    // it, so no empty line is left behind.
                    range.start = prev_char(text, range.start);
                }
                let post = splice(text, &range, "");
                let cursor = if linewise {
                    first_non_blank(&post, range.start.min(post.len()))
                } else {
                    clamp_normal_after(&post, range.start)
                };
                vec![
                    Action::Yank(reg_text),
                    Action::Edit(EditOp {
                        range,
                        text: String::new(),
                        cursor,
                    }),
                ]
            }
            Op::Change => {
                let mut range = range;
                if linewise
                    && range.end > range.start
                    && matches!(char_at(text, prev_char(text, range.end)), Some('\n'))
                {
                    // `cc`/`S`/linewise change clears the lines' content but
                    // keeps the trailing newline.
                    range.end = prev_char(text, range.end);
                }
                self.mode = Mode::Insert;
                let mut actions = vec![Action::Yank(reg_text)];
                if range.start < range.end {
                    actions.push(Action::Edit(EditOp {
                        range: range.clone(),
                        text: String::new(),
                        cursor: range.start,
                    }));
                } else {
                    actions.push(Action::MoveCursor(range.start));
                }
                actions
            }
        }
    }

    /// `dd`/`cc`/`yy` and their shorthands: `count` whole lines from the
    /// cursor's line.
    fn op_lines(&mut self, text: &str, cursor: usize, op: Op, count: usize) -> Vec<Action> {
        let start = line_start(text, cursor);
        let mut end = line_end(text, cursor);
        for _ in 1..count {
            if end >= text.len() {
                break;
            }
            end = line_end(text, next_char(text, end));
        }
        let end = (end + usize::from(end < text.len())).min(text.len());
        self.op_on_range(text, op, start..end, true, cursor)
    }

    /// `D`/`C`: cursor to line end, charwise; a count extends to the end of
    /// `count - 1` lines below (`:help D`).
    fn op_to_line_end(&mut self, text: &str, cursor: usize, op: Op, count: usize) -> Vec<Action> {
        let mut end = line_end(text, cursor);
        for _ in 1..count {
            if end >= text.len() {
                break;
            }
            end = line_end(text, next_char(text, end));
        }
        self.op_on_range(text, op, cursor..end, false, cursor)
    }

    fn visual_op(&mut self, text: &str, cursor: usize, op: Op) -> Vec<Action> {
        let Some(range) = self.visual_range(text, cursor) else {
            return self.beep();
        };
        let linewise = matches!(self.mode, Mode::Visual { linewise: true });
        self.mode = Mode::Normal;
        self.take_count();
        // A visual yank puts the cursor at the start of the selection (unlike
        // `yy`, which stays put) — pass the selection start as the origin.
        self.op_on_range(text, op, range.clone(), linewise, range.start)
    }

    // --- Simple edits ------------------------------------------------------

    fn enter_insert(&mut self, text: &str, cursor: usize, c: char) -> Vec<Action> {
        self.take_count();
        self.mode = Mode::Insert;
        match c {
            'i' => Vec::new(),
            'a' => {
                // Append: after the current char (insert-mode cursors may sit
                // past the last char).
                let pos = if char_at(text, cursor).is_some_and(|ch| ch != '\n') {
                    next_char(text, cursor)
                } else {
                    cursor
                };
                vec![Action::MoveCursor(pos)]
            }
            'I' => vec![Action::MoveCursor(first_non_blank(text, cursor))],
            'A' => vec![Action::MoveCursor(line_end(text, cursor))],
            'o' => {
                let at = line_end(text, cursor);
                vec![Action::Edit(EditOp {
                    range: at..at,
                    text: "\n".into(),
                    cursor: at + 1,
                })]
            }
            'O' => {
                let at = line_start(text, cursor);
                vec![Action::Edit(EditOp {
                    range: at..at,
                    text: "\n".into(),
                    cursor: at,
                })]
            }
            _ => unreachable!(),
        }
    }

    fn delete_chars_forward(&mut self, text: &str, cursor: usize, count: usize) -> Vec<Action> {
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                break;
            }
            to = next_char(text, to);
        }
        if to == cursor {
            return self.beep();
        }
        let yanked = text[cursor..to].to_string();
        self.register = Some(Register {
            text: yanked.clone(),
            linewise: false,
        });
        let post = splice(text, &(cursor..to), "");
        vec![
            Action::Yank(yanked),
            Action::Edit(EditOp {
                range: cursor..to,
                text: String::new(),
                cursor: clamp_normal_after(&post, cursor),
            }),
        ]
    }

    fn delete_chars_backward(&mut self, text: &str, cursor: usize, count: usize) -> Vec<Action> {
        let start = line_start(text, cursor);
        let mut from = cursor;
        for _ in 0..count {
            if from <= start {
                break;
            }
            from = prev_char(text, from);
        }
        if from == cursor {
            return self.beep();
        }
        let yanked = text[from..cursor].to_string();
        self.register = Some(Register {
            text: yanked.clone(),
            linewise: false,
        });
        vec![
            Action::Yank(yanked),
            Action::Edit(EditOp {
                range: from..cursor,
                text: String::new(),
                cursor: from,
            }),
        ]
    }

    fn replace_chars(&mut self, text: &str, cursor: usize, c: char, count: usize) -> Vec<Action> {
        // `r` fails when there aren't `count` chars left on the line.
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                return self.beep();
            }
            to = next_char(text, to);
        }
        // `{count}r<CR>` replaces all `count` chars with a single line break,
        // cursor at the start of the new line (`:help r`).
        let (replacement, cursor_after) = if c == '\n' {
            ("\n".to_string(), cursor + 1)
        } else {
            let replacement: String = std::iter::repeat_n(c, count).collect();
            let after = cursor + replacement.len() - c.len_utf8();
            (replacement, after)
        };
        vec![Action::Edit(EditOp {
            range: cursor..to,
            text: replacement,
            cursor: cursor_after,
        })]
    }

    fn toggle_case(&mut self, text: &str, cursor: usize, count: usize) -> Vec<Action> {
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                break;
            }
            to = next_char(text, to);
        }
        if to == cursor {
            return self.beep();
        }
        let toggled: String = text[cursor..to]
            .chars()
            .flat_map(toggle_char_case)
            .collect();
        let post = splice(text, &(cursor..to), &toggled);
        let after = clamp_normal(&post, cursor + toggled.len());
        vec![Action::Edit(EditOp {
            range: cursor..to,
            text: toggled,
            cursor: after,
        })]
    }

    /// `>`/`<`: shift the whole lines covered by `range` by one indent step
    /// (two spaces — the hanging-bullet width; Vim's 'shiftwidth' default of
    /// 8 is wrong for commit messages). Indent skips blank lines, like Vim;
    /// dedent strips up to one step of spaces or a tab.
    fn shift_lines(&mut self, text: &str, range: Range<usize>, dedent: bool) -> Vec<Action> {
        const STEP: &str = "  ";
        let start = line_start(text, range.start.min(text.len()));
        let last = prev_char(text, range.end.min(text.len())).max(range.start);
        let end = line_end(text, last.max(start));
        let mut shifted = String::with_capacity(end - start + STEP.len() * 4);
        for (i, line) in text[start..end].split('\n').enumerate() {
            if i > 0 {
                shifted.push('\n');
            }
            if dedent {
                let trimmed = line
                    .strip_prefix(STEP)
                    .or_else(|| line.strip_prefix('\t'))
                    .or_else(|| line.strip_prefix(' '))
                    .unwrap_or(line);
                shifted.push_str(trimmed);
            } else if line.trim().is_empty() {
                shifted.push_str(line);
            } else {
                shifted.push_str(STEP);
                shifted.push_str(line);
            }
        }
        if shifted == text[start..end] {
            // Nothing to dedent: park on the first non-blank, like Vim's
            // silent `<<` on an unindented line.
            return vec![Action::MoveCursor(first_non_blank(text, start))];
        }
        let post = splice(text, &(start..end), &shifted);
        let cursor = first_non_blank(&post, start.min(post.len()));
        vec![Action::Edit(EditOp {
            range: start..end,
            text: shifted,
            cursor,
        })]
    }

    /// Join the lines covered by `range` (at least the cursor's line and the
    /// next): each newline (plus the following line's indent) becomes one
    /// space, unless the text before it already ends in whitespace.
    fn join_range(&mut self, text: &str, range: Range<usize>) -> Vec<Action> {
        let start = line_start(text, range.start);
        let mut end = line_end(text, range.end.max(range.start));
        // A charwise range within one line still joins it with the next.
        if line_end(text, start) == end && end < text.len() {
            end = line_end(text, next_char(text, end));
        }
        if line_end(text, start) == end {
            return self.beep(); // nothing to join with
        }
        let mut joined = String::new();
        let mut cursor_after = None;
        let mut rest = start;
        while rest < end {
            let le = line_end(text, rest);
            joined.push_str(&text[rest..le]);
            if le >= end {
                break;
            }
            // Skip the newline and the next line's leading blanks.
            let mut next = le + 1;
            while matches!(char_at(text, next), Some(' ' | '\t')) {
                next = next_char(text, next);
            }
            cursor_after = Some(start + joined.len());
            // One space at the seam — unless the left side already ends in
            // whitespace or there is nothing to join on the right (a blank or
            // empty line at the end contributes nothing, like Vim).
            if !joined.is_empty()
                && !joined.ends_with([' ', '\t'])
                && char_at(text, next).is_some_and(|c| c != '\n')
            {
                joined.push(' ');
            }
            rest = next;
        }
        let cursor = cursor_after.unwrap_or(start);
        let post = splice(text, &(start..end), &joined);
        vec![Action::Edit(EditOp {
            range: start..end,
            text: joined,
            cursor: clamp_normal(&post, cursor),
        })]
    }

    /// Visual `p`/`P`: the selection is replaced by the register, honoring
    /// each side's linewise-ness, and the replaced text takes the register's
    /// place (Vim's swap idiom).
    fn visual_put(
        &mut self,
        text: &str,
        range: Range<usize>,
        sel_linewise: bool,
        reg: Register,
    ) -> Vec<Action> {
        let cut = text[range.clone()].to_string();
        let sel_had_newline = cut.ends_with('\n');
        let cut = if sel_linewise && !sel_had_newline {
            format!("{cut}\n")
        } else {
            cut
        };
        self.register = Some(Register {
            text: cut.clone(),
            linewise: sel_linewise,
        });
        // A linewise register pastes as whole lines (splitting a charwise
        // selection's line); a charwise register over a linewise selection
        // becomes its own line. Match the selection's trailing-newline
        // presence so an EOF paste doesn't grow a stray empty line.
        let mut pasted = match (reg.linewise, sel_linewise) {
            (false, false) => reg.text.clone(),
            (true, false) => format!("\n{}", reg.text),
            (false, true) => format!("{}\n", reg.text),
            (true, true) => reg.text.clone(),
        };
        if sel_linewise && !sel_had_newline {
            // A linewise selection ending at EOF has no trailing newline to
            // give back; don't grow a stray empty last line.
            if let Some(s) = pasted.strip_suffix('\n') {
                pasted = s.to_string();
            }
        }
        let post = splice(text, &range, &pasted);
        let cursor = if reg.linewise || sel_linewise {
            // First non-blank of the first pasted line.
            let first = range.start + usize::from(pasted.starts_with('\n'));
            first_non_blank(&post, first.min(post.len()))
        } else {
            // Charwise over charwise: the last pasted char.
            let end = range.start + pasted.len();
            clamp_normal(
                &post,
                if end > range.start {
                    prev_char(&post, end)
                } else {
                    range.start
                },
            )
        };
        vec![
            Action::Yank(cut),
            Action::Edit(EditOp {
                range,
                text: pasted,
                cursor,
            }),
        ]
    }

    fn put(&mut self, text: &str, cursor: usize, count: usize, after: bool) -> Vec<Action> {
        let Some(reg) = self.register.clone() else {
            return self.beep();
        };
        // Cap the expansion so an absurd count can't OOM the app.
        const PUT_LIMIT: usize = 4 << 20;
        let count = count.min((PUT_LIMIT / reg.text.len().max(1)).max(1));
        let body = reg.text.repeat(count);
        if reg.linewise {
            let at = if after {
                let le = line_end(text, cursor);
                (le + usize::from(le < text.len())).min(text.len())
            } else {
                line_start(text, cursor)
            };
            // Pasting after the last line (no trailing newline): lead with a
            // newline and drop the register's trailing one.
            let (range, pasted) =
                if after && at == text.len() && !text.ends_with('\n') && !text.is_empty() {
                    (
                        at..at,
                        format!("\n{}", body.strip_suffix('\n').unwrap_or(&body)),
                    )
                } else {
                    (at..at, body)
                };
            let first_line = at + usize::from(pasted.starts_with('\n'));
            let post = splice(text, &range, &pasted);
            let cursor = first_non_blank(&post, first_line.min(post.len()));
            vec![Action::Edit(EditOp {
                range,
                text: pasted,
                cursor,
            })]
        } else {
            let at = if after && char_at(text, cursor).is_some_and(|c| c != '\n') {
                next_char(text, cursor)
            } else {
                cursor
            };
            let post = splice(text, &(at..at), &body);
            // Cursor on the last char of the pasted text.
            let cursor = clamp_normal(&post, prev_char(&post, at + body.len()).max(at));
            vec![Action::Edit(EditOp {
                range: at..at,
                text: body,
                cursor,
            })]
        }
    }

    // --- Small state helpers ---------------------------------------------

    fn after_move(&mut self, text: &str, m: Motion, pos: usize) {
        match m {
            Motion::Down | Motion::Up => {}
            Motion::LineEnd => self.desired_col = Some(usize::MAX),
            Motion::NextLineStart | Motion::PrevLineStart | Motion::GotoLine(_) => {
                self.desired_col = Some(char_col(text, clamp_normal(text, pos)));
            }
            _ => self.desired_col = Some(char_col(text, clamp_normal(text, pos))),
        }
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.parse().unwrap_or(0);
        self.count.clear();
        n
    }

    fn clear_pending(&mut self) {
        self.pending = Pending::None;
        self.count.clear();
    }

    fn beep(&mut self) -> Vec<Action> {
        self.clear_pending();
        vec![Action::Beep]
    }
}

/// Multiply the counts typed before and after an operator (`2d3w` = 6 words).
fn total(before: usize, after: usize) -> usize {
    before.max(1).saturating_mul(after.max(1))
}

/// The absolute line number for `G`/`gg`, combining a count typed before the
/// operator with one typed after it (0 = neither present).
fn goto_count(consumer: Consumer, after: usize) -> usize {
    let before = match consumer {
        Consumer::Op { count, .. } => count,
        _ => 0,
    };
    if before == 0 && after == 0 {
        0
    } else {
        total(before, after)
    }
}

fn toggle_char_case(ch: char) -> Vec<char> {
    if ch.is_lowercase() {
        ch.to_uppercase().collect()
    } else if ch.is_uppercase() {
        ch.to_lowercase().collect()
    } else {
        vec![ch]
    }
}

/// The motion a plain character names, if any. `f`/`F`/`t`/`T` return a
/// placeholder target — the engine parks in [`Pending::Find`] for the real
/// char; `;`/`,` replay `last_find`. (`G` is handled by the caller: its count
/// is an absolute line number.)
fn char_motion(c: char, last_find: Option<(FindKind, char)>) -> Option<Motion> {
    Some(match c {
        'h' => Motion::Left,
        'l' => Motion::Right,
        'j' => Motion::Down,
        'k' => Motion::Up,
        '0' => Motion::LineStart,
        '^' => Motion::FirstNonBlank,
        '$' => Motion::LineEnd,
        'w' => Motion::WordForward { big: false },
        'W' => Motion::WordForward { big: true },
        'b' => Motion::WordBack { big: false },
        'B' => Motion::WordBack { big: true },
        'e' => Motion::WordEnd { big: false },
        'E' => Motion::WordEnd { big: true },
        'f' => Motion::Find {
            kind: FindKind::FindFwd,
            target: '\0',
            repeat: false,
        },
        'F' => Motion::Find {
            kind: FindKind::FindBack,
            target: '\0',
            repeat: false,
        },
        't' => Motion::Find {
            kind: FindKind::TillFwd,
            target: '\0',
            repeat: false,
        },
        'T' => Motion::Find {
            kind: FindKind::TillBack,
            target: '\0',
            repeat: false,
        },
        ';' => {
            let (kind, target) = last_find?;
            Motion::Find {
                kind,
                target,
                repeat: true,
            }
        }
        ',' => {
            let (kind, target) = last_find?;
            Motion::Find {
                kind: kind.reversed(),
                target,
                repeat: true,
            }
        }
        '{' => Motion::ParagraphBack,
        '}' => Motion::ParagraphForward,
        '%' => Motion::MatchPair,
        '+' => Motion::NextLineStart,
        '-' => Motion::PrevLineStart,
        '_' => Motion::FirstNonBlankDown,
        ' ' => Motion::SpaceRight,
        _ => return None,
    })
}

fn consumer_keys(c: &Consumer) -> String {
    match c {
        Consumer::Move => String::new(),
        Consumer::Op { op, count } => {
            let mut s = String::new();
            if *count > 0 {
                s.push_str(&count.to_string());
            }
            s.push(op.key());
            s
        }
        Consumer::SurroundAdd => "ys".into(),
        Consumer::Reflow => "gq".into(),
        Consumer::Shift { dedent: false, .. } => ">".into(),
        Consumer::Shift { dedent: true, .. } => "<".into(),
    }
}

/// The buffer after replacing `range` with `with` (for computing post-edit
/// cursor positions).
fn splice(text: &str, range: &Range<usize>, with: &str) -> String {
    let mut s = String::with_capacity(text.len() - (range.end - range.start) + with.len());
    s.push_str(&text[..range.start]);
    s.push_str(with);
    s.push_str(&text[range.end..]);
    s
}

/// Clamp a post-deletion cursor for Normal mode: at `pos` if a char remains
/// there, else the line's last char.
fn clamp_normal_after(post: &str, pos: usize) -> usize {
    clamp_normal(post, pos.min(post.len()))
}

/// Turn a motion landing into the operator's byte range, applying the
/// exclusive-motion adjustments from `:help exclusive-linewise`. Returns the
/// range and whether it is linewise.
fn operator_range(
    text: &str,
    cursor: usize,
    m: Motion,
    target: MotionTarget,
) -> Option<(Range<usize>, bool)> {
    let (mut start, mut end) = if cursor <= target.pos {
        (cursor, target.pos)
    } else {
        (target.pos, cursor)
    };
    let mut kind = target.kind;

    if kind == MotionKind::Exclusive && end > start && end == line_start(text, end) {
        // The exclusive motion ends in column 1: back the end up to the
        // previous line's last char and turn inclusive (`dw` on a line's last
        // word doesn't eat the newline)…
        let nl = prev_char(text, end);
        let prev_line_start = line_start(text, nl);
        if nl > prev_line_start {
            end = prev_char(text, nl);
            kind = MotionKind::Inclusive;
        } else {
            end = nl; // previous line empty: exclusive of its newline
        }
        // …and if the start also sits at or before the first non-blank, the
        // whole motion turns linewise (`d}` in the indent deletes whole
        // lines). `w` is exempt: Vim's forward-word motion does its own end
        // adjustment and skips this conversion, so `dw` never goes linewise.
        if end > start
            && start <= first_non_blank(text, start)
            && !matches!(m, Motion::WordForward { .. })
        {
            kind = MotionKind::Linewise;
        }
    }

    match kind {
        MotionKind::Exclusive => Some((start..end, false)),
        MotionKind::Inclusive => {
            // An inclusive end never swallows a newline (`d$` on an empty
            // line is a no-op, not a join).
            let end = if char_at(text, end) == Some('\n') {
                end
            } else {
                next_char(text, end)
            };
            Some((start..end, false))
        }
        MotionKind::Linewise => {
            start = line_start(text, start);
            let le = line_end(text, end);
            end = (le + usize::from(le < text.len())).min(text.len());
            Some((start..end, true))
        }
    }
}

//! The gpui-facing side of Vim mode: convert keystrokes to engine [`Key`]s,
//! feed them through the editor's [`VimState`], and apply the returned
//! [`Action`]s to the commit editor's `InputState`. Also renders the pieces
//! the engine can't: the Visual-selection/block-cursor overlay (drawn with
//! `range_to_bounds`, since `InputState` exposes no way to set a selection)
//! and the mode bar's data.
//!
//! Focus is the routing switch: Insert mode focuses the input (typing, IME,
//! and the input's own keybindings work normally); Normal/Visual keep focus
//! on the view, so the input paints no caret, its bindings never match, and
//! every key flows through `on_capture_key` into the engine.

use super::{
    clamp_normal, first_non_blank, line_end, line_start, next_char, prev_char, Action, EditOp, Key,
    KeyModifiers, Mode, ModifiedKey, ScrollAlign, VisualKind,
};
use crate::*;
use gpui::{Bounds, Context, EntityInputHandler, KeyDownEvent, Pixels, Window};

impl StatusView {
    /// Route a commit-editor keystroke through Vim mode. Returns whether the
    /// key was consumed (the caller stops propagation). `key` is the already
    /// C-g-normalized key name from `on_capture_key`.
    pub(crate) fn handle_vim_key(
        &mut self,
        key: &str,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ed) = self.editor() else {
            return false;
        };
        let Some(vim) = ed.vim.as_ref() else {
            return false;
        };
        let mods = &event.keystroke.modifiers;
        if mods.function
            && !key
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| (1..=12).contains(&n))
        {
            return false;
        }
        if vim.in_insert() {
            // Insert mode is the input's: only Esc (or C-g mapped to it)
            // drops back to Normal.
            if key != "escape"
                || (event.keystroke.key == "escape"
                    && (mods.platform || mods.control || mods.alt || mods.shift || mods.function))
            {
                return false;
            }
        }
        let Some(k) = vim_key(key, event) else {
            // Unmapped printable keys must not reach the input in Normal /
            // Visual mode (they would insert); unknown named keys (page-up…)
            // fall through, where the editor screen ignores them.
            return !vim.in_insert()
                && event
                    .keystroke
                    .key_char
                    .as_ref()
                    .is_some_and(|ch| ch.chars().all(|c| !c.is_control()));
        };
        // Modified and otherwise user-only named keys enter the Vim engine only
        // when a custom mapping wants them. Unbound chords remain available to
        // macOS and the input. Cmd-Enter keeps its built-in submit behavior.
        if k.is_user_only() && !vim.handles_user_key(k) {
            if mods.platform && key == "enter" && !vim.in_insert() {
                self.submit_editor(window, cx);
                return true;
            }
            return false;
        }
        self.feed_vim(k, window, cx);
        true
    }

    /// Read the buffer, feed one key to the engine, apply the actions.
    fn feed_vim(&mut self, k: Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        // Any key clears the last echoed error, like Vim's command line.
        if let Some(ed) = self.editor_mut() {
            ed.vim_error = None;
        }
        let (text, cursor) = {
            let s = state.read(cx);
            (s.text().to_string(), s.cursor())
        };
        let actions = self
            .editor_mut()
            .and_then(|e| e.vim.as_mut())
            .map(|vim| vim.handle_key(&text, cursor, k))
            .unwrap_or_default();
        self.apply_vim_actions(actions, window, cx);
        self.sync_vim_focus(window, cx);
        // Re-arm the which-key panel against the state this key left behind.
        self.arm_vim_hints(cx);
        cx.notify();
    }

    /// `gq{target}`: apply [`reflow_edit`] to the editor's buffer.
    fn reflow_vim_range(
        &mut self,
        range: std::ops::Range<usize>,
        keep_cursor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        state.update(cx, |s, cx| {
            let text = s.text().to_string();
            let Some((span, reflowed, cursor)) = reflow_edit(&text, range, COMMIT_BODY_WIDTH)
            else {
                return;
            };
            // `gw` keeps the cursor on the text it was on. Reflowing only
            // moves whitespace, so the count of non-whitespace chars before
            // the cursor identifies the same spot afterwards.
            let ink_before = keep_cursor.then(|| {
                let at = s.cursor().min(text.len());
                text[..at].chars().filter(|c| !c.is_whitespace()).count()
            });
            s.replace_text_in_range(
                Some(commit_text::byte_range_to_utf16(&text, &span)),
                &reflowed,
                window,
                cx,
            );
            let post = s.text().to_string();
            let cursor = match ink_before {
                // Land on the next ink char (skipping the whitespace the
                // reflow may have put there) — probed against Vim 9.2, which
                // restores the cursor onto the same text character.
                Some(ink) => {
                    let base = byte_at_ink(&post, ink);
                    base + post[base..]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .map(char::len_utf8)
                        .sum::<usize>()
                }
                None => cursor,
            };
            s.set_cursor_position(
                commit_text::byte_offset_to_position(&post, cursor.min(post.len())),
                window,
                cx,
            );
        });
        // Refresh the summary warning against the reflowed text.
        self.on_editor_changed(window, cx);
    }

    /// A mouse press over the message in Vim Normal/Visual mode: abort any
    /// pending operator/count (a click is an implicit Esc) and hold off the
    /// blur-back so a drag can complete with the input focused.
    pub(crate) fn vim_mouse_down(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = self.editor_mut() else {
            return;
        };
        let Some(vim) = ed.vim.as_mut() else {
            return;
        };
        if vim.in_insert() {
            return;
        }
        vim.cancel_pending();
        ed.vim_error = None;
        ed.mouse_selecting = true;
        // The click cleared any pending sequence, so drop the which-key panel.
        self.arm_vim_hints(cx);
        cx.notify();
    }

    /// The release: a completed drag-selection becomes a Visual selection
    /// (anchor at the press point — a leftward drag anchors at the right
    /// end, like Vim — the native selection dropped in favor of the Vim
    /// overlay); a plain click just places the cursor. Either way focus goes
    /// back to the view.
    pub(crate) fn vim_mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor_mut() else {
            return;
        };
        if !std::mem::take(&mut ed.mouse_selecting) {
            return;
        }
        let state = ed.state.clone();
        let in_insert = ed.vim.as_ref().is_none_or(|v| v.in_insert());
        if in_insert {
            return;
        }
        let (text, sel, caret) = {
            let s = state.read(cx);
            (s.text().to_string(), s.selected_range(), s.cursor())
        };
        if sel.start < sel.end {
            // `selected_range` is normalized; the caret sits at the drag's
            // release end, so a caret at the start means a leftward drag.
            let (anchor, cursor) = if caret == sel.start {
                (prev_char(&text, sel.end), sel.start)
            } else {
                (sel.start, prev_char(&text, sel.end))
            };
            let cursor = clamp_normal(&text, cursor);
            if let Some(vim) = self.editor_mut().and_then(|e| e.vim.as_mut()) {
                vim.begin_visual(&text, anchor);
            }
            state.update(cx, |s, cx| {
                s.unselect(window, cx);
                s.set_cursor_position(
                    commit_text::byte_offset_to_position(&text, cursor),
                    window,
                    cx,
                );
            });
        }
        self.sync_vim_focus(window, cx);
        cx.notify();
    }

    /// Show the Vim-mode cheat sheet (`:help`, or a click on the mode chip).
    pub(crate) fn open_vim_help(&mut self, cx: &mut Context<Self>) {
        self.popup = Some(Popup::Dispatch(super::help::vim_help_menu()));
        cx.notify();
    }

    /// Keep focus in step with the mode: Insert focuses the input, everything
    /// else the view (which is what hides the input's caret in Normal mode —
    /// and set_cursor_position refocuses the input as a side effect, so this
    /// runs after every applied key).
    pub(crate) fn sync_vim_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor() else {
            return;
        };
        let Some(vim) = ed.vim.as_ref() else {
            return;
        };
        if vim.in_insert() {
            ed.state.read(cx).focus_handle(cx).focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }

    fn apply_vim_actions(
        &mut self,
        actions: Vec<Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        for action in actions {
            match action {
                Action::MoveCursor(pos) => state.update(cx, |s, cx| {
                    let text = s.text().to_string();
                    let pos = pos.min(text.len());
                    s.set_cursor_position(
                        commit_text::byte_offset_to_position(&text, pos),
                        window,
                        cx,
                    );
                }),
                Action::Edit(EditOp {
                    range,
                    text: replacement,
                    cursor,
                }) => state.update(cx, |s, cx| {
                    let text = s.text().to_string();
                    let range = range.start.min(text.len())..range.end.min(text.len());
                    s.replace_text_in_range(
                        Some(commit_text::byte_range_to_utf16(&text, &range)),
                        &replacement,
                        window,
                        cx,
                    );
                    let post = s.text().to_string();
                    s.set_cursor_position(
                        commit_text::byte_offset_to_position(&post, cursor.min(post.len())),
                        window,
                        cx,
                    );
                }),
                Action::Yank(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Action::Repeat(count) => {
                    let repeat = self
                        .editor_mut()
                        .and_then(|e| e.vim.as_mut())
                        .and_then(|vim| vim.begin_repeat(count));
                    if let Some((keys, typed)) = repeat {
                        for k in keys {
                            self.feed_vim(k, window, cx);
                        }
                        // A change that opened an Insert session re-types its
                        // text and closes back to Normal.
                        let in_insert = self
                            .editor()
                            .and_then(|e| e.vim.as_ref())
                            .is_some_and(|v| v.in_insert());
                        if in_insert {
                            if !typed.is_empty() {
                                state.update(cx, |s, cx| s.insert(typed, window, cx));
                            }
                            self.feed_vim(Key::Escape, window, cx);
                        }
                        if let Some(vim) = self.editor_mut().and_then(|e| e.vim.as_mut()) {
                            vim.end_repeat();
                        }
                    }
                }
                Action::Scroll(align) => state.update(cx, |s, cx| {
                    // Aim the cursor's line at the viewport edge. Soft wrap
                    // is off in the commit editor, so buffer lines are
                    // display rows. The viewport height isn't public, but
                    // the last layout brackets it to within a line: the
                    // visible range ends at the first row past the bottom
                    // edge, so that row's top minus the scroll position is
                    // the height. `set_scroll_offset` clamps to the valid
                    // range at the next layout.
                    let (Some(line_height), Some(rows)) = (s.line_height(), s.visible_row_range())
                    else {
                        return;
                    };
                    let text = s.text().to_string();
                    let row = text[..s.cursor().min(text.len())].matches('\n').count();
                    let mut offset = s.scroll_offset();
                    let viewport = (line_height * rows.end.saturating_sub(1) as f32 + offset.y)
                        .max(line_height);
                    let cursor_y = line_height * row as f32;
                    let y = match align {
                        ScrollAlign::Top => -cursor_y,
                        ScrollAlign::Center => (viewport - line_height) * 0.5 - cursor_y,
                        ScrollAlign::Bottom => viewport - line_height - cursor_y,
                    };
                    offset.y = y.min(px(0.));
                    s.set_scroll_offset(offset, cx);
                }),
                Action::Commit => self.submit_editor(window, cx),
                // `:q!` bypasses the discard confirmation.
                Action::Quit { force: true } => self.discard_editor(window, cx),
                Action::Quit { force: false } => self.cancel_editor(window, cx),
                Action::ReflowRange(range) => self.reflow_vim_range(range, false, window, cx),
                Action::ReflowRangeKeep(range) => self.reflow_vim_range(range, true, window, cx),
                Action::Help => self.open_vim_help(cx),
                Action::Error(msg) => {
                    if let Some(ed) = self.editor_mut() {
                        ed.vim_error = Some(msg);
                    }
                    self.vim_bell_flash(cx);
                }
                Action::Beep => self.vim_bell_flash(cx),
            }
        }
    }

    /// The header mode chip: label plus the in-progress key sequence.
    pub(crate) fn vim_indicator(
        &self,
        ed: &CommitEditor,
    ) -> Option<(&'static str, Option<String>)> {
        let vim = ed.vim.as_ref()?;
        let label = match vim.mode() {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual {
                kind: VisualKind::Char,
            } => "VISUAL",
            Mode::Visual {
                kind: VisualKind::Line,
            } => "V-LINE",
            Mode::Visual {
                kind: VisualKind::Block,
            } => "V-BLOCK",
        };
        Some((label, vim.pending_display()))
    }

    /// The overlay painting the Visual selection (per display line) and the
    /// Normal-mode block cursor over the message input. `range_to_bounds` is
    /// only meaningful after layout, so all geometry is computed in the paint
    /// closure; off-screen ranges just yield nothing.
    pub(crate) fn vim_overlay(&self, ed: &CommitEditor) -> Option<gpui::AnyElement> {
        let vim = ed.vim.as_ref()?;
        if vim.in_insert() {
            return None;
        }
        let vim = vim.clone();
        let state = ed.state.clone();
        let selection_bg = self.palette.visual;
        let cursor_bg = self.palette.fg.opacity(0.35);
        let search_bg = self.palette.banner;
        Some(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    // Clip to the overlay's own bounds: right after a resize
                    // (or with the cursor line scrolled out) range_to_bounds
                    // can report rects past the box's edge, and an unmasked
                    // quad would paint over the diff below.
                    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                        let s = state.read(cx);
                        let text = s.text().to_string();
                        let cursor = s.cursor();
                        // Zero-width rects are only meaningful for genuinely
                        // empty cells (empty line, EOF) — for a non-empty range
                        // they mean the line is scrolled out of view, and
                        // widening would paint a phantom stub at the viewport
                        // edge.
                        let paint = |window: &mut Window, b: Bounds<Pixels>, empty: bool, color| {
                            if b.size.width > px(0.0) {
                                window.paint_quad(gpui::fill(b, color));
                            } else if empty {
                                window.paint_quad(gpui::fill(widen(b), color));
                            }
                        };
                        if let Some(range) = vim.visual_range(&text, cursor) {
                            let mut at = range.start;
                            while at < range.end {
                                let le = line_end(&text, at).min(range.end);
                                if let Some(b) = s.range_to_bounds(&(at..le)) {
                                    paint(window, b, at == le, selection_bg);
                                }
                                at = next_char(&text, le.max(at));
                            }
                        }
                        // Blockwise selection: one rect per covered line (lines
                        // the block overhangs yield empty ranges — nothing).
                        if let Some(ranges) = vim.block_ranges(&text, cursor) {
                            for r in ranges {
                                if r.start < r.end {
                                    if let Some(b) = s.range_to_bounds(&r) {
                                        paint(window, b, false, selection_bg);
                                    }
                                }
                            }
                        }
                        // Incremental search: highlight every (smartcase regex)
                        // match of the pattern being typed at the `/`/`?` prompt
                        // (capped, in case a one-char query floods the message).
                        if let Some(re) = vim.search_query().and_then(super::compile_search) {
                            let mut from = 0;
                            for _ in 0..200 {
                                let Some((m0, mlen)) = re.find_from(&text, from) else {
                                    break;
                                };
                                let m1 = m0 + mlen;
                                let mut at = m0;
                                while at < m1 {
                                    let le = line_end(&text, at).min(m1);
                                    if let Some(b) = s.range_to_bounds(&(at..le)) {
                                        paint(window, b, false, search_bg);
                                    }
                                    at = next_char(&text, le.max(at));
                                }
                                from = next_char(&text, m0);
                            }
                        }
                        // Live `:s` preview: what the substitution being typed
                        // would touch (first match per line, all with `g`).
                        // Matches never span lines, so one rect each suffices.
                        for r in vim.ex_matches(&text, cursor) {
                            if let Some(b) = s.range_to_bounds(&r) {
                                paint(window, b, false, search_bg);
                            }
                        }
                        // Block cursor: the cell of the char under the cursor, or
                        // a half-width stub on empty lines / EOF.
                        let c0 = clamp_normal(&text, cursor);
                        let c1 = next_char(&text, c0);
                        let cell = if text[c0..c1.min(text.len())].starts_with('\n') || c0 == c1 {
                            c0..c0
                        } else {
                            c0..c1
                        };
                        if let Some(b) = s.range_to_bounds(&(cell.clone())) {
                            paint(window, b, cell.is_empty(), cursor_bg);
                        }
                    });
                },
            )
            // inset_0 (not size_full): an absolute canvas needs explicit
            // insets for its layout bounds to span the box — and the bounds
            // are what the paint closure masks to.
            .absolute()
            .inset_0()
            .into_any_element(),
        )
    }
}

/// The edit a `gq`/`,q` reflow of `range` should make: expand to whole lines
/// and reflow them at `width`, returning the byte span to replace, its
/// replacement, and the post-edit cursor (the reflowed block's first
/// non-blank). The summary line never reflows (the 50-col convention),
/// matching the ⌥q whole-body reflow. `None` when nothing changes.
fn reflow_edit(
    text: &str,
    range: std::ops::Range<usize>,
    width: usize,
) -> Option<(std::ops::Range<usize>, String, usize)> {
    let mut start = line_start(text, range.start.min(text.len()));
    // The range's end is exclusive (a linewise range ends just past its
    // trailing newline), so the last covered line is the one holding the
    // char before it.
    let last = prev_char(text, range.end.min(text.len())).max(range.start);
    let end = line_end(text, last);
    if start == 0 {
        // Skip the summary line.
        match text.find('\n') {
            Some(nl) if nl < end => start = nl + 1,
            _ => return None,
        }
    }
    let block = &text[start..end];
    let reflowed = commit_text::reflow_lines_joining(block, width);
    if reflowed == block {
        return None;
    }
    let cursor = first_non_blank(&reflowed, 0) + start;
    Some((start..end, reflowed, cursor))
}

/// The byte offset just past the `ink`-th non-whitespace char — the inverse
/// of counting ink before a cursor, for `gw`'s keep-the-spot mapping.
fn byte_at_ink(text: &str, ink: usize) -> usize {
    if ink == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, c) in text.char_indices() {
        if !c.is_whitespace() {
            seen += 1;
            if seen == ink {
                return i + c.len_utf8();
            }
        }
    }
    text.len()
}

/// Zero-width rects (empty lines, EOF) get a half-cell stub so they stay
/// visible.
fn widen(b: Bounds<Pixels>) -> Bounds<Pixels> {
    if b.size.width <= px(0.0) {
        Bounds::new(b.origin, gpui::size(b.size.height * 0.5, b.size.height))
    } else {
        b
    }
}

/// Convert a capture-phase gpui keystroke to an engine [`Key`]. Shifted
/// symbols arrive via `key_char` (`$`, `{`…), so this is keyboard-layout
/// aware. Named keys are safe to claim here: in Normal/Visual mode the input
/// is unfocused, so its own bindings for them can't fire.
fn vim_key(key: &str, event: &KeyDownEvent) -> Option<Key> {
    let ks = &event.keystroke;
    // Named keys first: `key` may be C-g already normalized to "escape", and
    // the control modifier must not turn it into a Ctrl chord.
    if key == "escape" && ks.key != "escape" {
        return Some(Key::Escape);
    }
    let plain = match key {
        "escape" => Key::Escape,
        "space" => Key::Char(' '),
        "enter" => Key::Enter,
        "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "tab" => Key::Char('\t'),
        "delete" => user_key(ModifiedKey::Delete),
        "home" => user_key(ModifiedKey::Home),
        "end" => user_key(ModifiedKey::End),
        "pageup" => user_key(ModifiedKey::PageUp),
        "pagedown" => user_key(ModifiedKey::PageDown),
        "insert" => user_key(ModifiedKey::Insert),
        name if name
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n)) =>
        {
            user_key(ModifiedKey::Function(name[1..].parse().ok()?))
        }
        _ => {
            let shifted = kbd::chord(key, ks.modifiers.shift, false, false, false);
            let character = if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.alt {
                // Configured chords name the physical key (`alt-s`), not the
                // composed character reported in `key_char` (`ß` on macOS).
                single_char(&shifted)
            } else {
                ks.key_char
                    .as_deref()
                    .and_then(single_char)
                    .or_else(|| single_char(&shifted))
            };
            Key::Char(character.filter(|c| !c.is_control())?)
        }
    };
    if ks.modifiers.control
        && !ks.modifiers.platform
        && !ks.modifiers.alt
        && single_char(key).is_some()
    {
        if let Key::Char(c) = plain {
            return Some(Key::Ctrl(c));
        }
    }
    if !ks.modifiers.platform
        && !ks.modifiers.control
        && !ks.modifiers.alt
        && (!ks.modifiers.shift || matches!(plain, Key::Char(_)))
    {
        return Some(plain);
    }
    let modified = match plain {
        Key::Char(c) => ModifiedKey::Char(c),
        Key::Enter => ModifiedKey::Enter,
        Key::Escape => ModifiedKey::Escape,
        Key::Backspace => ModifiedKey::Backspace,
        Key::Left => ModifiedKey::Left,
        Key::Right => ModifiedKey::Right,
        Key::Up => ModifiedKey::Up,
        Key::Down => ModifiedKey::Down,
        Key::Modified { key, .. } => key,
        Key::Ctrl(_) => return None,
    };
    Some(Key::Modified {
        key: modified,
        modifiers: KeyModifiers {
            cmd: ks.modifiers.platform,
            ctrl: ks.modifiers.control,
            alt: ks.modifiers.alt,
            // Shift is encoded by the produced printable character, but named
            // keys retain it as a distinct modifier (`shift-enter`).
            shift: ks.modifiers.shift && !matches!(modified, ModifiedKey::Char(_)),
        },
    })
}

fn user_key(key: ModifiedKey) -> Key {
    Key::Modified {
        key,
        modifiers: KeyModifiers::default(),
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{reflow_edit, vim_key};
    use crate::vim::{Key, KeyModifiers, ModifiedKey};
    use gpui::{KeyDownEvent, Keystroke, Modifiers};

    fn key_event(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.to_string(),
                key_char: key_char.map(str::to_string),
            },
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn gpui_modifier_chords_match_vim_config_keys() {
        let cmd = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert_eq!(
            vim_key("enter", &key_event("enter", None, cmd)),
            Some(Key::Modified {
                key: ModifiedKey::Enter,
                modifiers: KeyModifiers {
                    cmd: true,
                    ..KeyModifiers::default()
                }
            })
        );

        // Alt composition must still match the physical `alt-s` config key.
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            vim_key("s", &key_event("s", Some("ß"), alt)),
            Some(Key::Modified {
                key: ModifiedKey::Char('s'),
                modifiers: KeyModifiers {
                    alt: true,
                    ..KeyModifiers::default()
                }
            })
        );

        let cmd_shift = Modifiers {
            platform: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            vim_key("n", &key_event("n", None, cmd_shift)),
            Some(Key::Modified {
                key: ModifiedKey::Char('N'),
                modifiers: KeyModifiers {
                    cmd: true,
                    ..KeyModifiers::default()
                }
            })
        );

        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            vim_key("x", &key_event("x", None, ctrl)),
            Some(Key::Ctrl('x'))
        );
    }

    #[test]
    fn byte_at_ink_round_trips_through_a_reflow() {
        // gw's cursor mapping: ink count before the cursor in the old text
        // finds the same spot after whitespace-only changes.
        let before = "one two three";
        let after = "one two
three";
        // Cursor on the 't' of "three" (byte 8): 6 ink chars before it.
        let ink = before[..8].chars().filter(|c| !c.is_whitespace()).count();
        assert_eq!(ink, 6);
        // Just past "two"; the apply side then advances over whitespace so
        // the cursor lands back on the 't' (Vim 9.2's gw does the same).
        assert_eq!(super::byte_at_ink(after, ink), 7);
        assert_eq!(super::byte_at_ink(after, 0), 0);
        assert_eq!(super::byte_at_ink(after, 100), after.len());
        // Multibyte: counting is char-based, offsets byte-based.
        assert_eq!(super::byte_at_ink("𝄞 x", 1), 4);
    }

    #[test]
    fn utf16_ranges() {
        // "a𝄞b": 𝄞 is 4 bytes, 2 UTF-16 units.
        let t = "a𝄞b";
        assert_eq!(super::commit_text::byte_range_to_utf16(t, &(0..1)), 0..1);
        assert_eq!(super::commit_text::byte_range_to_utf16(t, &(1..5)), 1..3);
        assert_eq!(super::commit_text::byte_range_to_utf16(t, &(5..6)), 3..4);
    }

    #[test]
    fn reflow_edit_expands_to_whole_lines() {
        // A mid-line range reflows the whole covered lines; the cursor lands
        // on the block's first non-blank.
        let text = "summary\naa bb\ncc\n";
        let (span, out, cursor) = reflow_edit(text, 11..15, 72).unwrap();
        assert_eq!(span, 8..16); // "aa bb\ncc", whole lines
        assert_eq!(out, "aa bb cc");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn reflow_edit_linewise_end_excludes_next_line() {
        // A linewise range ends just past its trailing newline: the line at
        // that offset is not part of the target.
        let text = "s\naa bb\ncc dd\n";
        let (span, out, _) = reflow_edit(text, 2..8, 3).unwrap();
        assert_eq!(span, 2..7); // "aa bb" only, not "cc dd"
        assert_eq!(out, "aa\nbb");
    }

    #[test]
    fn reflow_edit_skips_summary_line() {
        // A range touching the summary starts below it…
        let text = "long summary\nbb cc\n";
        let (span, out, cursor) = reflow_edit(text, 0..19, 3).unwrap();
        assert_eq!(span, 13..18);
        assert_eq!(out, "bb\ncc");
        assert_eq!(cursor, 13);
        // …and a summary-only range reflows nothing.
        assert_eq!(reflow_edit("long summary\nbody", 0..5, 3), None);
        assert_eq!(reflow_edit("summary only", 0..12, 3), None);
    }

    #[test]
    fn reflow_edit_noop_returns_none() {
        assert_eq!(reflow_edit("s\nshort line\n", 2..12, 72), None);
        assert_eq!(reflow_edit("", 0..0, 72), None);
    }

    #[test]
    fn reflow_edit_cursor_at_first_non_blank() {
        // A hanging bullet keeps its indent; the cursor sits on the marker.
        let text = "s\n- aa bb cc\n";
        let (span, out, cursor) = reflow_edit(text, 2..12, 7).unwrap();
        assert_eq!(span, 2..12);
        assert_eq!(out, "- aa bb\n  cc");
        assert_eq!(cursor, 2);
    }
}

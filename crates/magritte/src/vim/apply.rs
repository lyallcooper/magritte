//! The gpui-facing side of Vim mode: convert keystrokes to engine [`Key`]s,
//! feed them through the editor's [`VimState`], and apply the returned
//! [`Action`]s to the commit editor's `InputState`. Also renders the pieces
//! the engine can't: the Visual-selection/block-cursor overlay (drawn with
//! `range_to_bounds`, since `InputState` exposes no way to set a selection)
//! and the header mode indicator's data.

use gpui::prelude::*;
use gpui::{Bounds, Context, EntityInputHandler, KeyDownEvent, Pixels, Window};
use gpui_component::input;

use super::{clamp_normal, line_end, next_char, Action, EditOp, Key, Mode};
use crate::*;

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
        // Cmd/function chords (⌘⏎ commit, ⌘C copy…) are never Vim's.
        if mods.platform || mods.function {
            return false;
        }
        if vim.in_insert() {
            // Insert mode is the input's: only Esc (or C-g mapped to it)
            // drops back to Normal.
            if key != "escape" {
                return false;
            }
        } else {
            // ⌥q (reflow) keeps working in Normal/Visual; any other alt
            // chord that would compose a character is swallowed so it can't
            // insert (C-g arrives here already normalized to "escape").
            if mods.alt && key != "escape" {
                return key != "q" && event.keystroke.key_char.is_some();
            }
            // Esc in idle Normal mode falls through to cancel the editor.
            if key == "escape" && vim.is_idle_normal() {
                return false;
            }
        }
        let Some(k) = vim_key(key, event) else {
            // Unmapped printable keys must not reach the input in Normal /
            // Visual mode (they would insert); named keys pass through — the
            // ones that matter arrive via the rerouted Input-context bindings
            // (`vim_bound_key`), the rest (page-up…) keep their behavior.
            return !vim.in_insert()
                && event
                    .keystroke
                    .key_char
                    .as_ref()
                    .is_some_and(|ch| ch.chars().all(|c| !c.is_control()));
        };
        self.feed_vim(k, window, cx);
        true
    }

    /// Handle one of the rerouted Input-context key actions (see the
    /// `Vim*` actions in main.rs): the Input binds these keys to its own edit
    /// actions, which fire even when `on_capture_key` stops the keystroke —
    /// so the bindings route here instead. In Vim Normal/Visual mode the key
    /// feeds the engine; everywhere else (Insert mode, the pickers, settings
    /// inputs, Vim off) the input's original action is re-dispatched.
    pub(crate) fn vim_bound_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let intercept = self.editor().is_some_and(|e| {
            !e.confirming_cancel && e.vim.as_ref().is_some_and(|v| !v.in_insert())
        });
        if intercept {
            let k = match key {
                "enter" | "shift-enter" => Some(Key::Enter),
                "backspace" => Some(Key::Backspace),
                "up" => Some(Key::Up),
                "down" => Some(Key::Down),
                "left" => Some(Key::Left),
                "right" => Some(Key::Right),
                // tab/delete and the rest are swallowed in Normal mode.
                _ => None,
            };
            if let Some(k) = k {
                self.feed_vim(k, window, cx);
            }
            return;
        }
        let action: Box<dyn gpui::Action> = match key {
            "enter" => Box::new(input::Enter {
                secondary: false,
                shift: false,
            }),
            "shift-enter" => Box::new(input::Enter {
                secondary: false,
                shift: true,
            }),
            "backspace" => Box::new(input::Backspace),
            "delete" => Box::new(input::Delete),
            "tab" => Box::new(input::IndentInline),
            "shift-tab" => Box::new(input::OutdentInline),
            "up" => Box::new(input::MoveUp),
            "down" => Box::new(input::MoveDown),
            "left" => Box::new(input::MoveLeft),
            "right" => Box::new(input::MoveRight),
            _ => return,
        };
        window.dispatch_action(action, cx);
    }

    /// Read the buffer, feed one key to the engine, apply the actions.
    fn feed_vim(&mut self, k: Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
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
        cx.notify();
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
                        Some(byte_range_to_utf16(&text, &range)),
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
                Action::Undo => window.dispatch_action(Box::new(input::Undo), cx),
                Action::Redo => window.dispatch_action(Box::new(input::Redo), cx),
                Action::Beep => {}
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
            Mode::Visual { linewise: false } => "VISUAL",
            Mode::Visual { linewise: true } => "V-LINE",
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
        Some(
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, cx| {
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
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        )
    }
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
/// aware. Keys the Input binds to actions (Enter, Backspace, arrows…) are
/// deliberately absent — they arrive through `vim_bound_key` instead, and
/// feeding them here too would double-handle them.
fn vim_key(key: &str, event: &KeyDownEvent) -> Option<Key> {
    let ks = &event.keystroke;
    // Named keys first: `key` may be C-g already normalized to "escape", and
    // the control modifier must not turn it into a Ctrl chord.
    match key {
        "escape" => return Some(Key::Escape),
        "space" => return Some(Key::Char(' ')),
        _ => {}
    }
    if ks.modifiers.control {
        return single_char(key).map(Key::Ctrl);
    }
    Some(Key::Char(
        single_char(ks.key_char.as_deref()?).filter(|c| !c.is_control())?,
    ))
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// UTF-8 byte range → UTF-16 code-unit range, the one conversion Vim mode
/// needs (only `replace_text_in_range` speaks UTF-16; every read is bytes).
fn byte_range_to_utf16(text: &str, range: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start: usize = text[..range.start].chars().map(char::len_utf16).sum();
    let len: usize = text[range.start..range.end]
        .chars()
        .map(char::len_utf16)
        .sum();
    start..start + len
}

#[cfg(test)]
mod tests {
    #[test]
    fn utf16_ranges() {
        // "a𝄞b": 𝄞 is 4 bytes, 2 UTF-16 units.
        let t = "a𝄞b";
        assert_eq!(super::byte_range_to_utf16(t, &(0..1)), 0..1);
        assert_eq!(super::byte_range_to_utf16(t, &(1..5)), 1..3);
        assert_eq!(super::byte_range_to_utf16(t, &(5..6)), 3..4);
    }
}

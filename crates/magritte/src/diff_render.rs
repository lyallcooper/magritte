//! The flattened-diff screens: the commit/diff detail views (shared row
//! renderer + drag-selection plumbing) and the commit-message editor with its
//! staged-diff preview. `impl StatusView` like the other view slices.

use std::ops::Range;

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, TextLayout, Window};
use gpui_component::input::Input;
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::Sizable;

use crate::render::{click_was_drag, color_run, offset_at, with_copy_menu, StyleRuns};
use crate::*;

/// The tallest the editor's message box may be in this window: the resize
/// drag and the height restored at editor open both clamp to this, so the
/// grip and a usable strip of diff preview stay on screen (below the editor
/// header) even in a short window.
pub(crate) fn editor_message_max_height(window: &Window) -> f32 {
    (window.viewport_size().height.as_f32() - 160.0).max(EDITOR_MESSAGE_HEIGHT_MIN)
}

impl StatusView {
    /// Render the commit message editor: a header, the editable text with a
    /// caret, all filling the window.
    pub(crate) fn render_editor(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        let title: SharedString = match &ed.after_submit {
            CommitAfterSubmit::CreateTag { name, .. } => format!("Annotate tag {name}").into(),
            _ => match ed.mode {
                CommitMode::Create => "Commit message",
                CommitMode::Amend => "Amend commit",
                CommitMode::Reword => "Reword commit",
            }
            .into(),
        };
        let submit_label = if matches!(ed.after_submit, CommitAfterSubmit::CreateTag { .. }) {
            "create tag"
        } else {
            "commit"
        };

        let root = div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .w_full()
            // The message editor and diff preview are monospace (the 50/72
            // ruler depends on column alignment).
            .font_family(self.font.clone())
            // No bottom padding: the diff preview runs flush to the window's
            // bottom edge (the message-only layout adds its own below).
            .pt_3()
            .px_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(self.palette.section).child(title))
                    .map(|el| {
                        if ed.confirming_cancel {
                            // Unsaved edits: confirm before discarding the message.
                            // The whole prompt sits in one group so an ignored
                            // keypress can flash its background (a warning wash),
                            // signalling that input is paused.
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_1()
                                    .rounded(px(3.0))
                                    .when(ed.flash, |p| p.bg(self.palette.banner))
                                    .child(
                                        div()
                                            .text_color(if ed.flash {
                                                self.palette.fg
                                            } else {
                                                self.palette.dim
                                            })
                                            // Amend/reword edit an existing
                                            // message: it's the changes being
                                            // discarded, not the message.
                                            .child(SharedString::from(if ed.initial.is_empty() {
                                                "Discard message?"
                                            } else {
                                                "Discard changes?"
                                            })),
                                    )
                                    .child(self.key_action(
                                        "editor-discard-yes",
                                        "y",
                                        "discard",
                                        view,
                                        Self::discard_editor,
                                    ))
                                    .child(self.key_action(
                                        "editor-discard-no",
                                        "n",
                                        "keep editing",
                                        view,
                                        Self::keep_editing,
                                    )),
                            )
                        } else {
                            // Vim mode uses evil's commit-buffer keys (ZZ
                            // finish, ZQ cancel) so Esc and the editing keys
                            // stay free for modal editing; its reflow (gq) and
                            // the rest live in the :help sheet, not up here.
                            let vim = ed.vim.is_some();
                            el.child(self.key_action(
                                "editor-commit",
                                if vim { "Z Z" } else { "cmd-enter" },
                                submit_label,
                                view,
                                Self::submit_editor,
                            ))
                            .when(!vim, |el| {
                                el.child(self.key_action(
                                    "editor-reflow",
                                    "alt-q",
                                    "reflow",
                                    view,
                                    Self::reflow_editor,
                                ))
                            })
                            .child(self.key_action(
                                "editor-cancel",
                                if vim { "Z Q" } else { "esc" },
                                "cancel",
                                view,
                                Self::cancel_editor,
                            ))
                        }
                    }),
            );

        // With a staged diff to review, the message takes a fixed band at the
        // top and the diff fills the rest (scrollable); otherwise the message
        // fills the window.
        // While the discard confirmation is up, disable the field so it grays
        // out — a clear cue that typing is paused until you answer y/n.
        let paused = ed.confirming_cancel;
        // Vim mode paints its Visual selection and block cursor as an overlay
        // sibling above the Input (InputState exposes no selection setter),
        // and watches the mouse: a press aborts pending operators, a
        // completed drag-selection becomes a Visual selection.
        let vim_active = ed.vim.is_some();
        let (v_mdown, v_mup, v_chip) = (view.clone(), view.clone(), view.clone());
        let message = move |input: gpui::Div| {
            let wrapped = input
                .relative()
                // The Input's default vertical padding is dead space above the
                // summary line and below the last one — zero it (the
                // horizontal padding stays, from the Input's own default).
                .child(
                    Input::new(&ed.state)
                        .h_full()
                        .pt_0()
                        .pb_0()
                        // Row-height lines (like every list in the app): the
                        // snugger leading also sits the first line against
                        // the box's top edge.
                        .line_height(px(self.row_h()))
                        .disabled(paused),
                )
                .children(self.vim_overlay(ed))
                .children(self.vim_which_key_overlay(ed))
                .children(self.vim_indicator_overlay(ed, &v_chip));
            if !vim_active {
                return wrapped;
            }
            wrapped
                .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                    v_mdown.update(cx, |this, cx| this.vim_mouse_down(cx));
                })
                .on_mouse_up(MouseButton::Left, move |_, window, cx: &mut App| {
                    v_mup.update(cx, |this, cx| this.vim_mouse_up(window, cx));
                })
        };
        // A tag message has no diff: the message fills the window. Otherwise
        // the split is reserved from the first frame (`diff_expected`), so the
        // async diff landing doesn't shift the layout; the message box's
        // bottom edge drags to resize it (persisted per repo). The diff starts
        // directly below the box, and the resize grip paints last so its thin
        // strip wins the hit test over both neighbors.
        if !ed.diff_expected {
            return root.pb_3().child(message(div().flex_grow(1.0).w_full()));
        }
        let root = root.child(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_grow(1.0)
                .w_full()
                .child(message(div().h(px(self.editor_message_height)).w_full()))
                .child(self.render_commit_diff(ed, view))
                .child(self.editor_resize_grip(view)),
        );
        // The drag handlers live on the editor root so the divider keeps
        // following the pointer when it leaves the thin handle mid-drag.
        let (v_move, v_up) = (view.clone(), view.clone());
        root.on_mouse_move(move |ev: &gpui::MouseMoveEvent, window, cx: &mut App| {
            if ev.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let max = editor_message_max_height(window);
            v_move.update(cx, |this, cx| {
                let Some((y0, h0)) = this.editor_resize else {
                    return;
                };
                let h = (h0 + (ev.position.y.as_f32() - y0)).clamp(EDITOR_MESSAGE_HEIGHT_MIN, max);
                if h != this.editor_message_height {
                    this.editor_message_height = h;
                    cx.notify();
                }
            });
        })
        .on_mouse_up(MouseButton::Left, move |_, _window, cx: &mut App| {
            v_up.update(cx, |this, _cx| {
                if this.editor_resize.take().is_some() {
                    this.persist_fold_state();
                }
            });
        })
    }

    /// The message box's resize handle: a thin strip straddling the box's
    /// bottom border (no divider row — the border itself is the edge), with a
    /// centered grip pill that brightens on hover. Dragging it resizes the
    /// message box; the strip is deliberately short so clicks just past it
    /// still reach the message's last line and the diff's first row.
    fn editor_resize_grip(&self, view: &Entity<Self>) -> impl IntoElement {
        const GRIP_GROUP: &str = "editor-resize-grip";
        let v = view.clone();
        let grip = self.palette.dim.opacity(0.4);
        let grip_hover = self.palette.dim;
        div()
            .id("editor-resize-grip")
            .group(GRIP_GROUP)
            .absolute()
            .top(px(self.editor_message_height - 4.0))
            .left_0()
            .w_full()
            .h(px(8.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_ns_resize()
            .child(
                div()
                    .w(px(36.0))
                    .h(px(4.0))
                    .rounded_full()
                    .bg(grip)
                    .group_hover(GRIP_GROUP, move |s| s.bg(grip_hover)),
            )
            .on_mouse_down(MouseButton::Left, {
                move |ev: &gpui::MouseDownEvent, _window, cx: &mut App| {
                    // The strip overlaps the input's bottom edge; without this
                    // the press also reaches the input, which starts a text
                    // drag-selection while the box is being resized.
                    cx.stop_propagation();
                    v.update(cx, |this, _cx| {
                        this.editor_resize =
                            Some((ev.position.y.as_f32(), this.editor_message_height));
                    });
                }
            })
    }

    /// The Vim mode indicator overlaid at the message box's bottom-right: the
    /// in-progress key sequence, live `/`//`:` command line, or echoed error
    /// to the left of the mode chip (NORMAL/INSERT/VISUAL). Every piece gets
    /// an opaque fill (the editor background, or a blend over it) — the
    /// overlay sits on the message text, so anything translucent would let it
    /// bleed through. Only the mode chip listens for the mouse (a click opens
    /// the :help sheet); everything else passes clicks to the input. Inset
    /// clear of the input's scrollbar.
    fn vim_indicator_overlay(&self, ed: &CommitEditor, view: &Entity<Self>) -> Option<gpui::Div> {
        let (label, pending) = self.vim_indicator(ed)?;
        let prompt = ed.vim.as_ref().is_some_and(|v| v.in_prompt());
        let chip_bg = self.palette.bg.blend(if ed.vim_bell {
            self.palette.removed_bg
        } else {
            self.palette.visual
        });
        let v = view.clone();
        Some(
            div()
                .absolute()
                .bottom(px(8.0))
                .right(px(16.0))
                .flex()
                .items_center()
                .gap_2()
                .when_some(ed.vim_error.clone(), |el, msg| {
                    el.child(
                        div()
                            .px_1()
                            .rounded(px(3.0))
                            .bg(self.palette.bg)
                            .text_color(self.palette.removed)
                            .child(SharedString::from(msg)),
                    )
                })
                .when_some(pending.filter(|_| ed.vim_error.is_none()), |el, keys| {
                    let keys = div()
                        .px_1()
                        .rounded(px(3.0))
                        .child(SharedString::from(keys));
                    el.child(if prompt {
                        // The live command line: full-color and tinted, so it
                        // reads as an active prompt rather than a hint.
                        keys.bg(self.palette.bg.blend(self.palette.selection))
                            .text_color(self.palette.fg)
                    } else {
                        keys.bg(self.palette.bg).text_color(self.palette.dim)
                    })
                })
                .child(
                    div()
                        .px_1()
                        .rounded(px(3.0))
                        .bg(chip_bg)
                        .text_color(match label {
                            _ if ed.vim_bell => self.palette.removed,
                            "INSERT" => self.palette.added,
                            "VISUAL" | "V-LINE" | "V-BLOCK" => self.palette.modified,
                            _ => self.palette.fg,
                        })
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                            cx.stop_propagation();
                            v.update(cx, |this, cx| this.open_vim_help(cx));
                        })
                        .child(SharedString::from(label)),
                ),
        )
    }

    /// The Vim which-key panel, anchored above the mode indicator: one row
    /// per possible continuation of the pending multi-key sequence (kbd cap +
    /// dim description), shown once the sequence has sat pending for a beat
    /// (`arm_vim_hints`). Additive to the indicator, which keeps showing the
    /// typed prefix — and deliberately inert: no mouse handlers, so it can't
    /// swallow the keys it hints at.
    fn vim_which_key_overlay(&self, ed: &CommitEditor) -> Option<gpui::Div> {
        if !ed.vim_hints {
            return None;
        }
        let hints = ed.vim.as_ref()?.which_key_hints();
        if hints.is_empty() {
            return None;
        }
        // Column-major, five rows per column, so even the longest table
        // (operator-pending) stays inside the default message-box height.
        let mut grid = div().flex().flex_row().items_start().gap_4();
        for chunk in hints.chunks(5) {
            grid = grid.child(div().flex().flex_col().items_start().gap_1().children(
                chunk.iter().map(|(keys, desc)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(kbd::key_chip(
                            keys,
                            self.palette.dim,
                            &self.font,
                            &self.system_ui_font,
                        ))
                        .child(
                            div()
                                .text_color(self.palette.dim)
                                .child(SharedString::from(desc.clone())),
                        )
                }),
            ));
        }
        Some(
            div()
                .absolute()
                // Clear of the mode indicator below: its 8px inset plus the
                // chip's height (one text line), which scales with the font.
                .bottom(px(8.0 + self.font_px() * 1.5 + 6.0))
                .right(px(16.0))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                // Opaque (the panel floats over the message text), matching
                // the bottom transient panel's fill.
                .bg(self.palette.bg.blend(self.palette.panel))
                .border_1()
                .border_color(self.palette.border)
                .text_xs()
                .child(grid),
        )
    }

    /// The read-only, scrollable staged-diff preview shown below the message.
    pub(crate) fn render_commit_diff(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        if ed.diff_loading {
            return div()
                .w_full()
                .flex_grow(1.0)
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new().small().color(self.palette.dim))
                .child(
                    div()
                        .text_xs()
                        .text_color(self.palette.dim)
                        .child("loading diff"),
                );
        }
        let count = commit_diff_view::visible_diff_rows(&ed.diff, &ed.diff_collapsed).len();
        div()
            .relative()
            .w_full()
            .flex_grow(1.0)
            .child(
                uniform_list("commit-diff", count, {
                    let view = view.clone();
                    move |range, _window, cx| {
                        let this = view.read(cx);
                        match this.editor() {
                            Some(ed) => {
                                let vis = commit_diff_view::visible_diff_rows(
                                    &ed.diff,
                                    &ed.diff_collapsed,
                                );
                                range
                                    .filter_map(|pos| vis.get(pos).copied())
                                    .filter_map(|ix| ed.diff.get(ix).map(|row| (ix, row)))
                                    .map(|(ix, row)| {
                                        let foldable = matches!(
                                            row,
                                            CommitDiffRow::File { .. }
                                                | CommitDiffRow::Hunk(_)
                                                | CommitDiffRow::Stats { .. }
                                                | CommitDiffRow::DetailsHeader
                                        );
                                        let collapsed = ed.diff_collapsed.contains(&ix);
                                        // The editor's diff preview is read-only chrome — no
                                        // char selection there, so drop the layout handle.
                                        let (content, _) = this
                                            .render_commit_diff_row(row, false, collapsed, None);
                                        // Every row highlights on hover; a line's
                                        // translucent tint blends over the wash so it
                                        // shows there too. Clicking a File/Hunk header
                                        // folds it (lines aren't clickable — no cursor).
                                        let hover = this.palette.selection;
                                        let mut wrap = div()
                                            .id(("commit-diff-row", ix))
                                            .w_full()
                                            .hover(move |s| s.bg(hover))
                                            .child(content);
                                        if foldable {
                                            let v = view.clone();
                                            wrap = wrap.cursor_pointer().on_click(
                                                move |_, _window, cx: &mut App| {
                                                    v.update(cx, |view, vcx| {
                                                        view.toggle_commit_diff_fold(ix, vcx)
                                                    });
                                                },
                                            );
                                        }
                                        wrap.into_any_element()
                                    })
                                    .collect::<Vec<_>>()
                            }
                            None => Vec::new(),
                        }
                    }
                })
                .track_scroll(&ed.diff_scroll)
                .size_full()
                .py_1(),
            )
            .vertical_scrollbar(&ed.diff_scroll)
    }

    /// Render a flattened-diff row. `sel`, when `Some`, is the char-selection
    /// byte range within *this* row's text (the caller supplies it only for the
    /// row that owns the active [`CharSelection`]). Returns the row element and,
    /// for the text-bearing kinds, the [`TextLayout`] of their [`StyledText`] so
    /// the drag handlers can hit-test pixels to byte offsets.
    pub(crate) fn render_commit_diff_row(
        &self,
        row: &CommitDiffRow,
        highlighted: bool,
        collapsed: bool,
        sel: Option<Range<usize>>,
    ) -> (AnyElement, Option<TextLayout>) {
        let base = div()
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .when(highlighted, |el| el.bg(self.palette.selection));
        // A fold triangle for the collapsible headers (File/Hunk): ▾ open, ▸ shut.
        let fold_marker = |el: gpui::Div| {
            el.child(
                div()
                    .w(px(12.0))
                    .flex_none()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(if collapsed { "▸" } else { "▾" })),
            )
        };
        match row {
            // The foldable header over the commit metadata: chevron + a dim
            // "Details" label, like the diffstat summary header.
            CommitDiffRow::DetailsHeader => (
                base.child(chevron(!collapsed, self.palette.dim))
                    .child(
                        div()
                            .text_color(self.palette.dim)
                            .child(SharedString::from("Details")),
                    )
                    .into_any_element(),
                None,
            ),
            // The metadata "Refs:" line: dim text with the ref names styled as
            // runs (color-coded by kind), so it's one selectable string.
            CommitDiffRow::Detail(text) if text.starts_with("Refs:") => {
                let upstream = self
                    .status
                    .as_ref()
                    .and_then(|s| s.head.upstream.as_deref());
                let prefix_end = "Refs:".len();
                let mut runs = StyleRuns::new();
                let mut cursor = 0;
                let mut search_from = prefix_end;
                for (label, kind) in parse_refs(text[prefix_end..].trim(), upstream) {
                    if let Some(rel) = text[search_from..].find(label.as_str()) {
                        let start = search_from + rel;
                        if cursor < start {
                            runs.push(color_run(cursor..start, self.palette.dim));
                        }
                        runs.push((start..start + label.len(), self.ref_style(kind)));
                        cursor = start + label.len();
                        search_from = cursor;
                    }
                }
                if cursor < text.len() {
                    runs.push(color_run(cursor..text.len(), self.palette.dim));
                }
                let (styled, layout) = self.selectable_text(text.clone(), runs, sel);
                (base.child(styled).into_any_element(), Some(layout))
            }
            CommitDiffRow::Detail(text) => {
                let (styled, layout) = self.selectable_text(text.clone(), Vec::new(), sel);
                (
                    base.text_color(self.palette.dim)
                        .child(styled)
                        .into_any_element(),
                    Some(layout),
                )
            }
            CommitDiffRow::Message(text) => {
                let (styled, layout) = self.selectable_text(text.clone(), Vec::new(), sel);
                (
                    base.text_color(self.palette.fg)
                        .child(styled)
                        .into_any_element(),
                    Some(layout),
                )
            }
            // A diffstat line: the path, then a git-style `N +++---` bar (total
            // changed + a scaled run of green `+` / red `-`). These sit together
            // in the block above the diffs.
            CommitDiffRow::StatLine {
                path,
                added,
                removed,
            } => {
                // One StyledText — `path N +++---` — with per-part colors, so the
                // whole line is char-selectable and copies as `commit_row_text`.
                let (plus, minus) = stat_bar(*added, *removed);
                let total = (added + removed).to_string();
                let (bar_plus, bar_minus) = ("+".repeat(plus), "-".repeat(minus));
                let text = format!("{path} {total} {bar_plus}{bar_minus}");
                let mid_end = path.len() + 1 + total.len() + 1; // " {total} "
                let runs = vec![
                    color_run(0..path.len(), self.palette.fg),
                    color_run(path.len()..mid_end, self.palette.dim),
                    color_run(mid_end..mid_end + bar_plus.len(), self.palette.added),
                    color_run(mid_end + bar_plus.len()..text.len(), self.palette.removed),
                ];
                let (styled, layout) = self.selectable_text(text, runs, sel);
                (base.child(styled).into_any_element(), Some(layout))
            }
            // Status-style file header: a colored change word ("modified") + path,
            // as one StyledText so the path is char-selectable.
            CommitDiffRow::File { change, path } => {
                let word = status_label::change_word(*change);
                let (text, runs) = if word.is_empty() {
                    (
                        path.clone(),
                        vec![color_run(0..path.len(), self.palette.fg)],
                    )
                } else {
                    let text = format!("{word} {path}");
                    let runs = vec![
                        color_run(
                            0..word.len(),
                            status_label::change_color(*change, &self.palette),
                        ),
                        color_run(word.len()..text.len(), self.palette.fg),
                    ];
                    (text, runs)
                };
                let (styled, layout) = self.selectable_text(text, runs, sel);
                (
                    fold_marker(base.gap_2()).child(styled).into_any_element(),
                    Some(layout),
                )
            }
            CommitDiffRow::Stats {
                files,
                insertions,
                deletions,
            } => {
                let text = diffstat_text(*files, *insertions, *deletions);
                let (styled, layout) = self.selectable_text(text, Vec::new(), sel);
                (
                    fold_marker(base.gap_2())
                        .text_color(self.palette.dim)
                        .child(styled)
                        .into_any_element(),
                    Some(layout),
                )
            }
            CommitDiffRow::Hunk(text) => {
                let (styled, layout) = self.selectable_text(text.clone(), Vec::new(), sel);
                (
                    fold_marker(base.gap_2())
                        .text_color(self.palette.hunk)
                        .child(styled)
                        .into_any_element(),
                    Some(layout),
                )
            }
            CommitDiffRow::Note(text) => {
                let (styled, layout) = self.selectable_text(text.clone(), Vec::new(), sel);
                (
                    base.text_color(self.palette.dim)
                        .child(styled)
                        .into_any_element(),
                    Some(layout),
                )
            }
            CommitDiffRow::Loading(text) => (
                base.gap_2()
                    .text_color(self.palette.dim)
                    .child(Spinner::new().xsmall().color(self.palette.dim))
                    .child(text.clone())
                    .into_any_element(),
                None,
            ),
            CommitDiffRow::Line { kind, spans } => {
                let (sign, sign_color, tint) = match kind {
                    LineKind::Added => ('+', self.palette.added, Some(self.palette.added_bg)),
                    LineKind::Removed => ('-', self.palette.removed, Some(self.palette.removed_bg)),
                    _ => (' ', self.palette.dim, None),
                };
                let (text, runs) = Self::spans_text_runs(spans);
                let (styled, layout) = self.selectable_text(text, runs, sel);
                let mut el = base;
                // The cursor/visual highlight must stay visible over an added/
                // removed row's tint, so it wins; the +/- sign color still marks
                // the line's kind. Only tint an unhighlighted line.
                if !highlighted {
                    if let Some(t) = tint {
                        el = el.bg(t);
                    }
                }
                (
                    el.child(
                        div()
                            .text_color(sign_color)
                            .child(SharedString::from(sign.to_string())),
                    )
                    .child(styled)
                    .into_any_element(),
                    Some(layout),
                )
            }
        }
    }

    /// The virtualized row list shared by the flattened diff screens: rows
    /// come from the active screen's [`FlatDiff`], with the cursor/visual
    /// highlight applied.
    fn flat_diff_body(
        &self,
        id: &'static str,
        fd: &FlatDiff,
        view: &Entity<Self>,
    ) -> gpui::UniformList {
        uniform_list(id, fd.visible_rows().len(), {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.flat_diff() {
                    Some(fd) => {
                        let vis = fd.visual.map(|a| (a.min(fd.selected), a.max(fd.selected)));
                        let visible = fd.visible_rows();
                        range
                            .filter_map(|pos| visible.get(pos).copied())
                            .filter_map(|ix| fd.rows.get(ix).map(|row| (ix, row)))
                            .map(|(ix, row)| {
                                // The char-selection range covering this row (partial
                                // on the endpoint rows, whole rows between).
                                let sel = fd.char_sel.and_then(|c| c.range_on(ix));
                                // A row mid-char-selection skips the full-row cursor
                                // wash so the char-range background stays visible; it
                                // *is* still the cursor row, just painted per-char.
                                let highlighted = sel.is_none()
                                    && (ix == fd.selected
                                        || vis.is_some_and(|(lo, hi)| ix >= lo && ix <= hi));
                                let foldable = matches!(
                                    row,
                                    CommitDiffRow::File { .. }
                                        | CommitDiffRow::Hunk(_)
                                        | CommitDiffRow::Stats { .. }
                                        | CommitDiffRow::DetailsHeader
                                );
                                let collapsed = fd.collapsed.contains(&ix);
                                let has_char_sel = sel.is_some();
                                let (content, layout) =
                                    this.render_commit_diff_row(row, highlighted, collapsed, sel);
                                // Plain click positions the cursor / toggles a fold; a
                                // left-drag selects — char-wise while it stays on this
                                // row, line-wise once it spans rows (see the handlers).
                                let hover = this.palette.hover;
                                let (down_layout, move_layout) = (layout.clone(), layout);
                                let (v_down, v_move, v_up, v_click) =
                                    (view.clone(), view.clone(), view.clone(), view.clone());
                                // No hover wash while this row shows a char selection,
                                // so the per-char background isn't washed over.
                                let hoverable = !highlighted && !has_char_sel;
                                div()
                                    .id(("flat-diff-row", ix))
                                    .w_full()
                                    .cursor_pointer()
                                    .when(hoverable, |d| d.hover(move |s| s.bg(hover)))
                                    .child(content)
                                    .on_mouse_down(MouseButton::Left, {
                                        move |ev: &MouseDownEvent, _window, cx: &mut App| {
                                            let offset = down_layout
                                                .as_ref()
                                                .map(|l| offset_at(l, ev.position));
                                            v_down.update(cx, |view, vcx| {
                                                if view.popup.is_some() {
                                                    return;
                                                }
                                                // This press is on a diff row (which
                                                // manages its own selection), not a
                                                // click-to-dismiss off the content.
                                                view.click_hit_selectable = true;
                                                if let Some(cv) = view.commit_view_mut() {
                                                    cv.header_sel = None;
                                                }
                                                if let Some(fd) = view.flat_diff_mut() {
                                                    fd.drag().mouse_down(ix, offset);
                                                    vcx.notify();
                                                }
                                            });
                                        }
                                    })
                                    .on_mouse_move({
                                        move |ev: &gpui::MouseMoveEvent, _window, cx: &mut App| {
                                            if ev.pressed_button != Some(MouseButton::Left) {
                                                return;
                                            }
                                            let offset = move_layout
                                                .as_ref()
                                                .map(|l| offset_at(l, ev.position));
                                            v_move.update(cx, |view, vcx| {
                                                let Some(fd) = view.flat_diff_mut() else {
                                                    return;
                                                };
                                                if fd.drag().mouse_move(ix, offset) {
                                                    vcx.notify();
                                                }
                                            });
                                        }
                                    })
                                    .on_mouse_up(MouseButton::Left, {
                                        move |_, _window, cx: &mut App| {
                                            v_up.update(cx, |view, vcx| {
                                                if let Some(fd) = view.flat_diff_mut() {
                                                    if fd.drag().mouse_up() {
                                                        vcx.notify();
                                                    }
                                                }
                                            });
                                        }
                                    })
                                    .on_click(
                                        move |ev: &gpui::ClickEvent, _window, cx: &mut App| {
                                            // A drag already selected; don't also click.
                                            if click_was_drag(ev) {
                                                return;
                                            }
                                            v_click.update(cx, |view, vcx| {
                                                if let Some(fd) = view.flat_diff_mut() {
                                                    // A click on a row that had a char
                                                    // selection only clears it (no fold).
                                                    if fd.char_click {
                                                        fd.char_click = false;
                                                        fd.char_sel = None;
                                                        vcx.notify();
                                                        return;
                                                    }
                                                    fd.selected = ix;
                                                    fd.visual = None;
                                                    fd.char_sel = None;
                                                    if foldable {
                                                        fd.toggle_fold(ix);
                                                    }
                                                    vcx.notify();
                                                }
                                            });
                                        },
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&fd.scroll)
        .flex_grow(1.0)
        // A drag past the list's ends clamps to the first/last visible row
        // instead of freezing (see drag_row_beyond_list). Indices map through
        // the fold projection, like the row handlers.
        .on_mouse_move({
            let view = view.clone();
            let scroll = fd.scroll.clone();
            move |ev: &gpui::MouseMoveEvent, _window, cx| {
                if ev.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                view.update(cx, |v, vcx| {
                    let row_h = v.row_h();
                    let Some(fd) = v.flat_diff_mut() else {
                        return;
                    };
                    let Some(anchor) = fd.drag_anchor else {
                        return;
                    };
                    let vis = fd.visible_rows();
                    let Some(pos) = drag_row_beyond_list(&scroll, vis.len(), ev.position, row_h)
                    else {
                        return;
                    };
                    let Some(&ix) = vis.get(pos) else {
                        return;
                    };
                    if ix == anchor {
                        return;
                    }
                    if fd.drag().mouse_move(ix, None) {
                        vcx.notify();
                    }
                });
            }
        })
    }

    /// Render a commit's diff detail (opened from the log): a header with the
    /// hash + subject, then the diff as the same rows the commit editor uses.
    pub(crate) fn render_commit_view(&self, cv: &CommitView, view: &Entity<Self>) -> gpui::Div {
        let body = self.flat_diff_body("commit-view-rows", &cv.body, view);

        // The identity line, on the header row beside the close button: dim
        // "Commit" + the full hash, as one drag-selectable string (its own
        // small selection state — see `CommitView::header_sel`). Right-click
        // copies the whole hash; `y`/Cmd-C copy a drag selection.
        let rev = cv.rev.clone();
        let text = format!("Commit {rev}");
        let label = "Commit".len();
        let runs = vec![
            color_run(0..label, self.palette.dim),
            color_run(label..text.len(), self.palette.fg),
        ];
        let sel = cv.header_sel.and_then(|c| c.range_on(0));
        let (styled, layout) = self.selectable_text(text, runs, sel);
        let (down_layout, move_layout) = (layout.clone(), layout);
        let (v_down, v_move, v_up) = (view.clone(), view.clone(), view.clone());
        let title = with_copy_menu(
            div()
                .id("commit-view-rev")
                .flex()
                .items_center()
                .child(styled)
                .on_mouse_down(gpui::MouseButton::Left, {
                    move |ev: &gpui::MouseDownEvent, _window, cx: &mut gpui::App| {
                        let offset = offset_at(&down_layout, ev.position);
                        v_down.update(cx, |v, vcx| {
                            if let Some(cv) = v.commit_view_mut() {
                                // One active selection: a header drag replaces any
                                // body selection.
                                cv.body.char_sel = None;
                                cv.body.visual = None;
                                cv.header_sel = None;
                                cv.header_drag = Some(offset);
                                vcx.notify();
                            }
                        });
                    }
                }),
            view,
            rev,
        );

        // The drag's move/up handlers live on the whole scaffold, not the
        // title: a fast drag overshoots the (text-sized) title's hitbox in a
        // few pixels, and gpui only delivers `on_mouse_move` inside the
        // element — pinned to the title, the selection would freeze at the
        // last in-bounds offset. Positions off the line clamp to its start/
        // end (`index_for_position`), so overshooting keeps selecting.
        self.screen_scaffold()
            .child(self.view_header(title, "close", view))
            .child(body)
            .on_mouse_move({
                move |ev: &gpui::MouseMoveEvent, _window, cx: &mut gpui::App| {
                    if ev.pressed_button != Some(gpui::MouseButton::Left) {
                        return;
                    }
                    v_move.update(cx, |v, vcx| {
                        if let Some(cv) = v.commit_view_mut() {
                            let Some(anchor) = cv.header_drag else {
                                return;
                            };
                            let offset = offset_at(&move_layout, ev.position);
                            let sel = CharSelection::on_row(0, anchor, offset);
                            if cv.header_sel != Some(sel) {
                                cv.header_sel = Some(sel);
                                vcx.notify();
                            }
                        }
                    });
                }
            })
            .on_mouse_up(gpui::MouseButton::Left, {
                move |_, _window, cx: &mut gpui::App| {
                    v_up.update(cx, |v, _| {
                        if let Some(cv) = v.commit_view_mut() {
                            cv.header_drag = None;
                        }
                    });
                }
            })
    }

    /// Render a standalone diff buffer opened from the `d` diff transient.
    pub(crate) fn render_diff_view(&self, dv: &DiffView, view: &Entity<Self>) -> gpui::Div {
        let body = self.flat_diff_body("diff-view-rows", &dv.body, view);

        self.screen_scaffold()
            .child(
                self.view_header(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(self.palette.fg)
                        .child(dv.title.clone()),
                    "close",
                    view,
                ),
            )
            .child(body)
    }
}

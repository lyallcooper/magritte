//! The flattened-diff screens: the commit/diff detail views (shared row
//! renderer + drag-selection plumbing) and the commit-message editor with its
//! staged-diff preview. `impl StatusView` like the other view slices.

use std::ops::Range;

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, TextLayout};
use gpui_component::input::Input;
use gpui_component::scroll::ScrollableElement;

use crate::render::{color_run, offset_at, StyleRuns};
use crate::*;
use gpui_component::menu::ContextMenuExt;

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
            .p_3()
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
                                            .child(SharedString::from("Discard message?")),
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
                            el.child(self.key_action(
                                "editor-commit",
                                "cmd-enter",
                                submit_label,
                                view,
                                Self::submit_editor,
                            ))
                            .child(self.key_action(
                                "editor-reflow",
                                "alt-q",
                                "reflow",
                                view,
                                Self::reflow_editor,
                            ))
                            .child(self.key_action(
                                "editor-cancel",
                                "esc",
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
        if ed.diff.is_empty() {
            root.child(
                div()
                    .flex_grow(1.0)
                    .w_full()
                    .child(Input::new(&ed.state).h_full().disabled(paused)),
            )
        } else {
            root.child(
                div()
                    .h(px(176.0))
                    .w_full()
                    .child(Input::new(&ed.state).h_full().disabled(paused)),
            )
            .child(self.render_commit_diff(ed, view))
        }
    }

    /// The read-only, scrollable staged-diff preview shown below the message.
    pub(crate) fn render_commit_diff(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        let count = commit_diff_view::visible_diff_rows(&ed.diff, &ed.diff_collapsed).len();
        div()
            .relative()
            .w_full()
            .flex_grow(1.0)
            .border_t_1()
            .border_color(self.palette.border)
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
            .h(px(ROW_HEIGHT))
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
                                // The char-selection range within this row (only the
                                // row that owns a non-empty selection paints one).
                                let sel = fd.char_sel.and_then(|c| {
                                    (c.row == ix && !c.is_empty()).then(|| c.range())
                                });
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
                                            // A drag (moved between down and up) already
                                            // selected; don't also click.
                                            if let gpui::ClickEvent::Mouse(e) = ev {
                                                let moved = (e.up.position.x - e.down.position.x)
                                                    .abs()
                                                    > px(4.0)
                                                    || (e.up.position.y - e.down.position.y).abs()
                                                        > px(4.0);
                                                if moved {
                                                    return;
                                                }
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
    }

    /// Render a commit's diff detail (opened from the log): a header with the
    /// hash + subject, then the diff as the same rows the commit editor uses.
    pub(crate) fn render_commit_view(&self, cv: &CommitView, view: &Entity<Self>) -> gpui::Div {
        let body = self.flat_diff_body("commit-view-rows", &cv.body, view);

        // The identity line, on the header row beside the close button: dim
        // "Commit" + the full hash. Right-click copies the hash (the chrome
        // Copy menu, like the title-bar refs); `y b` copies it too.
        let rev = cv.rev.clone();
        let title = div()
            .id("commit-view-rev")
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("Commit")),
            )
            .child(
                div()
                    .text_color(self.palette.fg)
                    .child(SharedString::from(rev.clone())),
            )
            .on_mouse_down(gpui::MouseButton::Right, {
                let view = view.clone();
                move |_, _window, cx: &mut gpui::App| {
                    let value = rev.clone();
                    view.update(cx, |v, vcx| {
                        v.pending_copy = Some(value);
                        v.ctx_menu_open = true;
                        vcx.notify();
                    });
                }
            })
            .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)));

        self.screen_scaffold()
            .child(self.view_header(title, "close", view))
            .child(body)
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

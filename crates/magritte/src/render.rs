//! Rendering core for [`StatusView`]: the shared text/selection and chrome
//! helpers every screen uses, the status list's `uniform_list` row renderer,
//! the bottom overlays (popups, which-key, toasts), and the `Render` impl.
//! Screen-specific layouts live in the sibling `title_bar`, `transient_render`,
//! `picker_render`, `list_render`, and `diff_render` modules — all `impl
//! StatusView` blocks over the same private fields.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Context, Entity, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, StyledText,
    TextLayout, Window,
};
use gpui_component::menu::ContextMenuExt;
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, IconName, Sizable};
use std::ops::Range;

/// Per-range styling over a string: `(byte range, HighlightStyle)` runs (color,
/// and — for ref tags — a background + weight), the shape
/// [`StatusView::selectable_text`] and the row-text helpers pass around.
pub(crate) type StyleRuns = Vec<(Range<usize>, HighlightStyle)>;

/// A plain color run (the common case: just a foreground color over a range).
pub(crate) fn color_run(range: Range<usize>, color: Hsla) -> (Range<usize>, HighlightStyle) {
    (
        range,
        HighlightStyle {
            color: Some(color),
            ..Default::default()
        },
    )
}

/// The whitespace-delimited word (token) of `text` containing byte `offset` —
/// used by right-click to select the sha/ref/word under the cursor.
pub(crate) fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = clamp_boundary(text, offset);
    let start = text[..offset]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        // Step past the whitespace char itself — `+ 1` would land mid-char on
        // multibyte whitespace (NBSP, ideographic space).
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let end = text[offset..]
        .find(char::is_whitespace)
        .map(|i| offset + i)
        .unwrap_or(text.len());
    start..end
}

/// Append `s` to `text` and push a color run covering it, so the runs tile the
/// string contiguously (a continuous selection background needs no gaps).
pub(crate) fn push_run(text: &mut String, runs: &mut StyleRuns, s: &str, color: Hsla) {
    let start = text.len();
    text.push_str(s);
    runs.push(color_run(start..text.len(), color));
}

/// Like [`push_run`] but with a full [`HighlightStyle`] (e.g. a styled ref tag).
pub(crate) fn push_styled(text: &mut String, runs: &mut StyleRuns, s: &str, style: HighlightStyle) {
    let start = text.len();
    text.push_str(s);
    runs.push((start..text.len(), style));
}

use crate::*;

/// Merge per-range color `runs` with an optional selection `sel` into the
/// sorted, non-overlapping `(Range, HighlightStyle)` list `StyledText` wants:
/// each piece keeps its span color, and pieces inside `sel` also get `sel_bg`.
/// Splits color runs at the selection boundaries so a partial-line selection
/// composes color + background without overlapping runs. With no color runs
/// (a single-colored row), only the selection range is emitted (background
/// over the inherited color).
fn merge_highlights(
    runs: &[(Range<usize>, HighlightStyle)],
    sel: Option<Range<usize>>,
    sel_bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let sel = sel.filter(|r| r.start < r.end);
    if runs.is_empty() {
        return sel
            .map(|r| {
                vec![(
                    r,
                    HighlightStyle {
                        background_color: Some(sel_bg),
                        ..Default::default()
                    },
                )]
            })
            .unwrap_or_default();
    }
    let mut out = Vec::new();
    for (run, base) in runs {
        let mut cuts = vec![run.start, run.end];
        if let Some(s) = &sel {
            if s.start > run.start && s.start < run.end {
                cuts.push(s.start);
            }
            if s.end > run.start && s.end < run.end {
                cuts.push(s.end);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let mut style = *base;
            // The selection background wins over a run's own (e.g. a ref tag's).
            if sel
                .as_ref()
                .is_some_and(|s| start >= s.start && end <= s.end)
            {
                style.background_color = Some(sel_bg);
            }
            out.push((start..end, style));
        }
    }
    out
}

/// The byte offset in a laid-out [`TextLayout`] nearest the window-absolute
/// `position`. `index_for_position` returns `Err(nearest)` past a line's end;
/// either way we want that nearest offset (so dragging off the right edge
/// selects to end-of-line rather than doing nothing).
pub(crate) fn offset_at(layout: &TextLayout, position: gpui::Point<gpui::Pixels>) -> usize {
    match layout.index_for_position(position) {
        Ok(index) | Err(index) => index,
    }
}

impl StatusView {
    /// The bottom popup panel (picker / transient): full-width, top border,
    /// panel background, padded column.
    pub(crate) fn bottom_panel(&self) -> gpui::Div {
        div()
            .w_full()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .py_2()
            .px_3()
            .flex()
            .flex_col()
    }

    /// A thin bottom bar (status toast, confirm prompt, visual indicator): one
    /// bordered row over `bg`.
    fn bottom_bar(&self, bg: Hsla) -> gpui::Div {
        div()
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(bg)
            .text_color(self.palette.fg)
    }

    /// Render `text` as a single [`StyledText`] with per-range colors (`runs`)
    /// and — when this row owns the active char selection — a selection
    /// background over `sel`. Returns the element's [`TextLayout`] (a shared
    /// handle) so a mouse handler can map pixels ↔ byte offsets after paint.
    ///
    /// Font/size are inherited from the surrounding view (via `with_highlights`,
    /// which resolves against the ambient text style), so no `TextStyle` is
    /// supplied here. `runs` may be empty for a single-colored row, whose base
    /// color then comes from the row's own `text_color`.
    pub(crate) fn selectable_text(
        &self,
        text: impl Into<SharedString>,
        runs: StyleRuns,
        sel: Option<Range<usize>>,
    ) -> (StyledText, TextLayout) {
        let text = text.into();
        // A selection can outlive the rows it was made against (a background
        // refresh rebuilds the list); clamp so a stale range can't feed
        // out-of-bounds offsets into the layout.
        let sel = sel.map(|r| {
            let end = clamp_boundary(&text, r.end);
            clamp_boundary(&text, r.start.min(end))..end
        });
        let highlights = merge_highlights(&runs, sel, self.palette.selection);
        let styled = StyledText::new(text).with_highlights(highlights);
        let layout = styled.layout().clone();
        (styled, layout)
    }

    /// The run style for a ref name embedded in a row's text — the same look
    /// refs always had (color-coded by kind: local blue, remote green, tag
    /// yellow, current branch bold), now as a selectable text run.
    pub(crate) fn ref_style(&self, kind: RefKind) -> HighlightStyle {
        let (color, bold) = match kind {
            RefKind::Tag => (self.palette.tag, false),
            RefKind::Head => (self.palette.branch_local, true),
            RefKind::Local => (self.palette.branch_local, false),
            RefKind::Remote | RefKind::SyncedHead => (self.palette.branch_remote, false),
        };
        HighlightStyle {
            color: Some(color),
            font_weight: bold.then_some(FontWeight::BOLD),
            ..Default::default()
        }
    }

    /// A status row's full selectable text and its style runs — every fragment
    /// (short hash, ref tags, subject / path / message) as one string so the
    /// whole row is char-selectable. `None` for a section header (chrome). Used
    /// by both [`render_row`] and the copy path, so offsets and copied text agree.
    ///
    /// [`render_row`]: Self::render_row
    pub(crate) fn selectable_row_text(&self, row: &Row) -> Option<(SharedString, StyleRuns)> {
        let one = |text: &str, color: Hsla| {
            (
                SharedString::from(text.to_string()),
                vec![color_run(0..text.len(), color)],
            )
        };
        match &row.kind {
            RowKind::Plain { text, color } => Some(one(text, *color)),
            RowKind::File { label, .. } => Some(one(label, self.palette.fg)),
            RowKind::HunkHeader { text, .. } => Some(one(text, self.palette.hunk)),
            RowKind::Diff { spans, .. } => {
                let (text, runs) = Self::spans_text_runs(spans);
                Some((SharedString::from(text), runs))
            }
            // `<hash> <refs…> <subject>`, the refs as styled tag runs.
            RowKind::Commit {
                short_hash,
                subject,
                refs,
                ..
            } => {
                let (mut text, mut runs) = (String::new(), StyleRuns::new());
                push_run(&mut text, &mut runs, short_hash, self.palette.dim);
                for (label, kind) in refs {
                    push_run(&mut text, &mut runs, " ", self.palette.fg);
                    push_styled(&mut text, &mut runs, label, self.ref_style(*kind));
                }
                push_run(&mut text, &mut runs, " ", self.palette.fg);
                push_run(&mut text, &mut runs, subject, self.palette.fg);
                Some((SharedString::from(text), runs))
            }
            // `<reference> <message>`.
            RowKind::Stash { reference, message } => {
                let (mut text, mut runs) = (String::new(), StyleRuns::new());
                push_run(&mut text, &mut runs, reference, self.palette.dim);
                push_run(&mut text, &mut runs, " ", self.palette.fg);
                push_run(&mut text, &mut runs, message, self.palette.fg);
                Some((SharedString::from(text), runs))
            }
            RowKind::Section { .. } => None,
        }
    }

    /// The concatenated text of `spans` and the per-span color runs over it —
    /// the input shape [`selectable_text`](Self::selectable_text) wants for a
    /// diff line's colored segments.
    pub(crate) fn spans_text_runs(spans: &[(String, Hsla)]) -> (String, StyleRuns) {
        let mut text = String::new();
        let mut runs = Vec::with_capacity(spans.len());
        for (segment, color) in spans {
            push_run(&mut text, &mut runs, segment, *color);
        }
        (text, runs)
    }

    /// A button label that gets a background highlight only when its containing
    /// [`KBD_ROW_GROUP`] row is hovered — so mousing over a keycap+label button
    /// highlights the text, not the keycap.
    pub(crate) fn hover_label(&self, text: &str, color: Hsla) -> gpui::Div {
        div()
            .px_1()
            .rounded(px(3.0))
            .text_color(color)
            .group_hover(KBD_ROW_GROUP, |s| s.bg(self.palette.visual))
            .child(SharedString::from(text.to_string()))
    }

    /// Render a key spec as a single keycap. A multi-keystroke sequence (e.g.
    /// `g r`) keeps its keys spaced *inside* the one cap (see [`format_keys`]).
    pub(crate) fn key_tokens(&self, keys: &str) -> gpui::Div {
        div().flex().items_center().child(kbd::key_chip(
            keys,
            self.palette.dim,
            &self.font,
            &self.system_ui_font,
        ))
    }

    /// A clickable key hint: a keycap + label that runs `action` (the same
    /// behavior its key triggers). Lets shown keys double as mouse buttons —
    /// used by the commit editor and settings screen.
    pub(crate) fn key_action(
        &self,
        id: &'static str,
        key: &'static str,
        label: &'static str,
        view: &Entity<Self>,
        action: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let view = view.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id))
            .child(kbd::key_chip(
                key,
                self.palette.dim,
                &self.font,
                &self.system_ui_font,
            ))
            .child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// The outer container every secondary view shares: a full-height,
    /// monospace, padded flex column that fills the space below the title bar.
    /// Callers add the header and body as children.
    pub(crate) fn screen_scaffold(&self) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .w_full()
            .flex_grow(1.0)
            .font_family(self.font.clone())
            .px_4()
            .pt_4()
            .gap_3()
    }

    /// A view's header row: the given `left` content, with a right-aligned `Esc`
    /// close button matching the settings screen. `label` adapts to context —
    /// "close" for a browser, "cancel" where leaving discards edits. Used by every
    /// secondary view so the close affordance sits in the same place everywhere.
    pub(crate) fn view_header(
        &self,
        left: impl IntoElement,
        label: &'static str,
        view: &Entity<Self>,
    ) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .child(left)
            .child(self.key_action("close-view", "esc", label, view, Self::close_screen))
    }

    /// A header hint for a registry command: the key is resolved from the live
    /// per-context keymap (so it always matches what the keyboard dispatches, and
    /// reflects the preset/remaps) and the click invokes the command by id. Only
    /// the terse `label` is supplied here; everything else derives from the
    /// registry, so header and dispatch can't drift apart.
    pub(crate) fn header_action(
        &self,
        id: &'static str,
        label: &'static str,
        view: &Entity<Self>,
    ) -> impl IntoElement {
        let default = commands().iter().find(|c| c.id == id).and_then(|c| c.key);
        let key = current_key(self.screen_bindings(), id, default).unwrap_or_default();
        let view = view.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id))
            .child(kbd::key_chip(
                &key,
                self.palette.dim,
                &self.font,
                &self.system_ui_font,
            ))
            .child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| v.invoke_command(id, window, vcx));
            })
    }

    /// A small dimmed `(i)` icon that reveals `explanation` in a tooltip on
    /// hover — for clarifying what a settings control does.
    pub(crate) fn info_icon(&self, id: String, explanation: &'static str) -> impl IntoElement {
        let font = self.font.clone();
        let dim = self.palette.dim;
        div()
            .id(SharedString::from(id.clone()))
            .relative()
            .child(track_target(id))
            .child(Icon::new(IconName::Info).xsmall().text_color(dim))
            // gpui's native tooltip (not the library's managed one) so we can
            // drop the show-delay to zero and bound the width so it wraps. The
            // library tooltip forces the theme's UI font; override it back to
            // our monospace chrome font so it matches the rest of the app.
            .tooltip(move |window, cx| {
                let font = font.clone();
                Tooltip::element(move |_, _| {
                    div()
                        .max_w(px(280.0))
                        .font_family(font.clone())
                        .child(SharedString::from(explanation))
                })
                .build(window, cx)
            })
            .tooltip_show_delay(Duration::ZERO)
    }

    pub(crate) fn render_row(&self, ix: usize, view: &Entity<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        // One id string per row per frame, shared by the element id and the
        // debug target registry.
        let row_id = SharedString::from(format!("status-row-{ix}"));
        let selected = ix == self.selected && row.selectable;
        let clickable = row.selectable || row.fold.is_some();
        let in_region = self
            .visual_range()
            .is_some_and(|(lo, hi)| ix >= lo && ix <= hi);
        // A row mid-char-selection paints its char range instead of the full-row
        // cursor wash (so the selection shows), and keeps its diff tint.
        let owns_char = self.char_sel.is_some_and(|c| c.row == ix && !c.is_empty());
        let char_range = self
            .char_sel
            .and_then(|c| (c.row == ix && !c.is_empty()).then(|| c.range()));
        let wash = selected && !owns_char;

        let mut el = div()
            .id(row_id.clone())
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .when(clickable, |el| el.cursor_pointer())
            .pl(px(ROW_PAD_LEFT + row.indent as f32 * INDENT_STEP));
        // In visual mode the whole region — including the current line — uses
        // the region color, so the cursor line doesn't stand out from it.
        // Otherwise the current line gets the selection accent.
        if in_region {
            el = el.bg(self.palette.visual);
        } else if wash {
            el = el.bg(self.palette.selection);
        } else if clickable && !owns_char {
            // A subtle hover on rows you can act on (not the current line or a
            // visual selection, which already have a background) — the theme's
            // explicit hover wash, so it reads as a preview of selecting.
            el = el.hover(|s| s.bg(self.palette.hover));
        }

        // Code-, diff-, and path-bearing rows render monospace (alignment and
        // code legibility); prose rows (sections, headers, messages) inherit the
        // UI font from the root.
        if matches!(
            row.kind,
            RowKind::Diff { .. }
                | RowKind::HunkHeader { .. }
                | RowKind::File { .. }
                | RowKind::Commit { .. }
                | RowKind::Stash { .. }
        ) {
            el = el.font_family(self.font.clone());
        }

        // A diff row's StyledText layout (for pixel↔offset hit-testing in the
        // drag handlers); `None` for every other row kind.
        let mut diff_layout: Option<TextLayout> = None;
        let content = match &row.kind {
            // Plain rows carry only their text (appended below).
            RowKind::Plain { .. } => el,
            RowKind::Section {
                title,
                count,
                expanded,
                refreshing,
            } => el
                .child(chevron(*expanded, self.palette.dim))
                .child(
                    div()
                        .text_color(self.palette.section)
                        .child(SharedString::from(title.clone())),
                )
                // The section count: just a dim number, no badge/tag chrome.
                // Omitted (None) for sections capped to a fixed size (recent).
                .when_some(*count, |el, count| {
                    el.child(
                        div()
                            .text_color(self.palette.dim)
                            .child(SharedString::from(count.to_string())),
                    )
                })
                // A subtle spinner while this (already-visible) section's listing
                // is being re-fetched. Gated on `busy` so it only appears after
                // the same delay as the global spinner — a fast refresh never
                // flashes it; first-load sections have no row yet so they pop in.
                .when(*refreshing && self.busy, |el| {
                    el.child(Spinner::new().xsmall().color(self.palette.dim))
                }),
            // The rows below build only their leading decorations; the row's
            // selectable text is appended uniformly after the match (from
            // `selectable_row_text`), so every git-output row is char-selectable.
            RowKind::File {
                status,
                status_color,
                expanded,
                ..
            } => {
                let lead = match expanded {
                    Some(e) => chevron(*e, self.palette.dim).into_any_element(),
                    None => div().w(px(14.0)).flex_shrink_0().into_any_element(),
                };
                let mut el = el.child(lead);
                // Only files with a status word get the fixed-width status
                // column; untracked files (no word) sit flush after the lead.
                if !status.is_empty() {
                    el = el.child(
                        div()
                            .w(px(STATUS_COL_WIDTH))
                            .flex_shrink_0()
                            .text_color(*status_color)
                            .child(SharedString::from(status.clone())),
                    );
                }
                el
            }
            RowKind::HunkHeader { expanded, .. } => el.child(chevron(*expanded, self.palette.dim)),
            RowKind::Diff { kind, .. } => {
                let tint = match kind {
                    LineKind::Added => Some(self.palette.added_bg),
                    LineKind::Removed => Some(self.palette.removed_bg),
                    _ => None,
                };
                let sign_color = match kind {
                    LineKind::Added => self.palette.added,
                    LineKind::Removed => self.palette.removed,
                    _ => self.palette.dim,
                };
                let sign = match kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    _ => ' ',
                };
                // Add/remove background tint, unless the row wears the cursor wash
                // or is in a line region (a char-selecting row keeps its tint).
                if let Some(t) = tint {
                    if !wash && !in_region {
                        el = el.bg(t);
                    }
                }
                el.child(
                    div()
                        .text_color(sign_color)
                        .child(SharedString::from(sign.to_string())),
                )
            }
            // Commit/stash rows: only a lead spacer to align under the section's
            // chevron. The hash, ref tags, and subject/message are the row's
            // selectable text (appended below, refs as styled runs).
            RowKind::Commit { .. } | RowKind::Stash { .. } => {
                el.child(div().w(px(14.0)).flex_shrink_0())
            }
        };
        // Append the row's selectable text as one StyledText (with the char
        // selection painted when this row owns it), capturing its layout for the
        // drag handlers. Section headers have none. The canonical text is kept
        // for the right-click word-select, so the (per-frame) row string is
        // assembled once, not twice.
        let mut right_text = None;
        let content = match self.selectable_row_text(row) {
            Some((text, runs)) => {
                right_text = Some(text.clone());
                let (styled, layout) = self.selectable_text(text, runs, char_range);
                diff_layout = Some(layout);
                content.child(styled)
            }
            None => content,
        };
        if clickable {
            // A right-click's word-select uses the row's layout + canonical text.
            let right_layout = diff_layout.clone();
            let (down_layout, move_layout) = (diff_layout.clone(), diff_layout);
            let el = content
                .relative()
                .child(track_target(row_id.to_string()))
                .on_click({
                    let view = view.clone();
                    move |ev: &gpui::ClickEvent, window, cx: &mut App| {
                        // A drag (moved between down and up) already selected text;
                        // don't also click (which would move the cursor / fold).
                        if let gpui::ClickEvent::Mouse(e) = ev {
                            if (e.up.position.x - e.down.position.x).abs() > px(4.0)
                                || (e.up.position.y - e.down.position.y).abs() > px(4.0)
                            {
                                return;
                            }
                        }
                        let double = ev.click_count() >= 2;
                        view.update(cx, |v, cx| {
                            // A click on a row that had a char selection only clears
                            // it — the next click acts as usual.
                            if v.selection.char_click {
                                v.selection.char_click = false;
                                v.char_sel = None;
                                cx.notify();
                                return;
                            }
                            v.char_sel = None;
                            // A lone click positions the cursor / toggles a fold; only
                            // a real double-click fires Enter (open) — so clicking a
                            // selected foldable row still expands/collapses it.
                            if double {
                                v.selected = ix;
                                if let Some(id) = v.resolve_binding("enter") {
                                    v.invoke_command(&id, window, cx);
                                }
                            } else {
                                v.click_row(ix, cx);
                            }
                        });
                    }
                })
                // Click-and-drag selects a range, like pressing `v` and moving.
                // Shift-click extends a selection from the current cursor (or
                // the existing anchor) to the clicked row, like a list widget.
                .on_mouse_down(MouseButton::Left, {
                    let view = view.clone();
                    move |ev: &MouseDownEvent, _window, cx: &mut App| {
                        // Byte offset under the press (only on a diff line); the
                        // anchor for a same-row char drag.
                        let offset = down_layout.as_ref().map(|l| offset_at(l, ev.position));
                        view.update(cx, |v, vcx| {
                            if v.popup.is_some() {
                                return;
                            }
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            // This press is on selectable text; the root's bubble
                            // handler must not treat it as a click-to-dismiss.
                            v.click_hit_selectable = true;
                            if ev.modifiers.shift {
                                let anchor = v.selection.visual.unwrap_or(v.selected);
                                v.selection.visual = (ix != anchor).then_some(anchor);
                                v.selected = ix;
                                v.selection.drag_anchor = None;
                                v.selection.char_anchor = None;
                                v.char_sel = None;
                                v.selection.shift_click = true;
                                v.selection.char_click = false;
                            } else {
                                v.status_drag().mouse_down(ix, offset);
                                v.selection.shift_click = false;
                            }
                            vcx.notify();
                        });
                    }
                })
                .on_mouse_move({
                    let view = view.clone();
                    move |ev: &gpui::MouseMoveEvent, _window, cx: &mut App| {
                        if ev.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        let offset = move_layout.as_ref().map(|l| offset_at(l, ev.position));
                        view.update(cx, |v, vcx| {
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            if v.status_drag().mouse_move(ix, offset) {
                                vcx.notify();
                            }
                        });
                    }
                })
                .on_mouse_up(MouseButton::Left, {
                    let view = view.clone();
                    move |_, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if v.status_drag().mouse_up() {
                                vcx.notify();
                            }
                        });
                    }
                });
            // Right-click selects the word (sha / ref / path token) under the
            // cursor — unless a line-wise region is in progress — then shows a
            // menu: the staging verbs that apply to the row, plus Copy.
            let view_r = view.clone();
            let el = el.on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, _window, cx: &mut App| {
                    let hit =
                        right_layout
                            .as_ref()
                            .zip(right_text.as_ref())
                            .map(|(layout, text)| {
                                let offset = offset_at(layout, ev.position);
                                (word_range(text, offset), offset)
                            });
                    view_r.update(cx, |v, vcx| {
                        if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                            return;
                        }
                        // This row's Copy uses the selection, not a chrome value.
                        v.pending_copy = None;
                        // Right-clicking *inside* the current selection keeps it (the
                        // menu copies it); clicking elsewhere clears it and selects
                        // the word at the click.
                        let inside = if let Some(anchor) = v.selection.visual {
                            let (lo, hi) = (anchor.min(v.selected), anchor.max(v.selected));
                            ix >= lo && ix <= hi
                        } else if let Some(c) = v.char_sel.filter(|c| !c.is_empty()) {
                            let r = c.range();
                            c.row == ix
                                && hit
                                    .as_ref()
                                    .is_some_and(|(_, o)| *o >= r.start && *o <= r.end)
                        } else {
                            false
                        };
                        if !inside {
                            v.selection.visual = None;
                            v.selected = ix;
                            v.char_sel = hit.and_then(|(w, _)| {
                                (!w.is_empty()).then_some(CharSelection {
                                    row: ix,
                                    anchor: w.start,
                                    cursor: w.end,
                                })
                            });
                            vcx.notify();
                        }
                    });
                },
            );
            match &row.target {
                Some(target) => {
                    let (can_stage, can_unstage, can_discard) = target_ops(target);
                    let conflicted = self.is_conflicted(target_path(target));
                    let (ours_label, theirs_label) = self.conflict_side_labels();
                    el.context_menu(move |mut menu, _window, _cx| {
                        // A conflicted file resolves by taking a whole side.
                        if conflicted {
                            menu = menu
                                .menu(ours_label, Box::new(CtxTakeOurs))
                                .menu(theirs_label, Box::new(CtxTakeTheirs))
                                .separator();
                        }
                        if can_stage {
                            menu = menu.menu("Stage", Box::new(CtxStage));
                        }
                        if can_unstage {
                            menu = menu.menu("Unstage", Box::new(CtxUnstage));
                        }
                        if can_discard {
                            menu = menu.menu("Discard", Box::new(CtxDiscard));
                        }
                        menu.separator().menu("Copy", Box::new(CtxCopy))
                    })
                    .into_any_element()
                }
                // Commits, stashes, plain rows: just Copy the selected word.
                None => el
                    .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)))
                    .into_any_element(),
            }
        } else {
            content.into_any_element()
        }
    }

    /// The pending-prefix strip, pinned to the window bottom. A lightweight line
    /// showing just the pressed key, until the which-key delay elapses — then it
    /// expands into the continuations (each `<prefix> <key>` and its command's
    /// label), like emacs' which-key.
    pub(crate) fn prefix_indicator(&self, window: &Window) -> Option<gpui::Div> {
        let pending = self.pending_prefix.as_ref()?;
        // The keys typed so far in a single keycap, with a trailing dash to show
        // the sequence is awaiting the next key (emacs' echo-area `g-` feedback).
        let typed = div()
            .flex()
            .items_center()
            .gap_1()
            .child(kbd::key_chip(
                &pending.seq,
                self.palette.dim,
                &self.font,
                &self.system_ui_font,
            ))
            .child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("-")),
            );
        let mut bar = div()
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .text_color(self.palette.dim)
            .text_xs()
            .flex()
            .flex_row()
            .items_start()
            .gap_6()
            .child(typed);
        if pending.which_key {
            // Group bindings by their immediate next key after the typed prefix.
            // A next key that completes a binding shows its command's label; one
            // that only leads deeper shows "…" to mark a further sub-sequence.
            let lead = format!("{} ", pending.seq);
            // Command id → title, built once (not re-scanned per continuation).
            let titles: std::collections::HashMap<String, String> = all_commands(&self.config)
                .map(|c| (c.id.to_string(), c.title.to_string()))
                .collect();
            let mut conts: std::collections::BTreeMap<String, Option<String>> =
                std::collections::BTreeMap::new();
            for (k, ids) in self.screen_bindings() {
                let Some(rest) = k.strip_prefix(&lead) else {
                    continue;
                };
                if ids.is_empty() {
                    continue;
                }
                let token = rest.split(' ').next().unwrap_or(rest).to_string();
                let completes = format!("{lead}{token}") == *k;
                if completes {
                    // A completing binding: only show it if it currently resolves
                    // to an *enabled* command (skip an at-point verb whose target
                    // isn't present, etc.) — don't advertise a dead key.
                    let Some(id) = self.resolve_binding(k) else {
                        continue;
                    };
                    let title = titles.get(&id).cloned();
                    // A completing binding's label wins over a sibling sub-prefix.
                    let entry = conts.entry(token).or_insert(None);
                    if title.is_some() {
                        *entry = title;
                    }
                } else {
                    // A deeper sub-prefix (shown as "…"); it leads to further keys.
                    conts.entry(token).or_insert(None);
                }
            }
            let entries: Vec<(String, Option<String>)> = conts.into_iter().collect();
            // Column-major like emacs' which-key: fill a column top-to-bottom,
            // then wrap into the next column once it would grow past ~a quarter
            // of the window height, so the strip grows vertically before widening.
            let vh = window.viewport_size().height.as_f32();
            let rows_per_col = (((vh / 4.0) / ROW_HEIGHT) as usize).clamp(1, entries.len().max(1));
            let mut grid = div().flex().flex_row().items_start().gap_x_6();
            for chunk in entries.chunks(rows_per_col) {
                let mut col = div().flex().flex_col().items_start().gap_1();
                for (token, title) in chunk {
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(kbd::key_chip(
                                token,
                                self.palette.dim,
                                &self.font,
                                &self.system_ui_font,
                            ))
                            .child(div().text_color(self.palette.dim).child(SharedString::from(
                                title.clone().unwrap_or_else(|| "…".to_string()),
                            ))),
                    );
                }
                grid = grid.child(col);
            }
            bar = bar.child(grid);
        }
        Some(bar)
    }

    /// The status/confirmation banner ("Copied …", errors), as a bottom-pinned
    /// bar. The full-window sub-views (settings, commit, log, …) append this so
    /// a copy confirmation is visible there too, not only in the status view.
    pub(crate) fn status_toast(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let msg = self.toast.message.clone()?;
        let bar = self
            .bottom_bar(self.palette.panel)
            .id("status-bar")
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _window, cx| {
                this.clear_status(cx);
            }))
            // Right-click copies the message — handy for a warning or error you
            // want to paste elsewhere. Includes the keycap prefix (e.g. the
            // `g x` of "g x is unbound") so the copied text reads in full.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _window, cx| {
                    let Some(msg) = this.toast.message.clone() else {
                        return;
                    };
                    let text = match &this.toast.keys {
                        Some(keys) => format!("{keys} {msg}"),
                        None => msg,
                    };
                    this.copy_to_clipboard(text, cx);
                }),
            );
        // A keys-led message (e.g. "g x is unbound") renders each typed key as a
        // keycap before the text, matching the which-key strip.
        if let Some(keys) = self.toast.keys.clone() {
            return Some(
                bar.flex()
                    .items_center()
                    .gap_2()
                    .child(kbd::key_chip(
                        &keys,
                        self.palette.dim,
                        &self.font,
                        &self.system_ui_font,
                    ))
                    .child(SharedString::from(msg)),
            );
        }
        Some(match () {
            // While a mutating job runs, hint that C-g/Esc cancels it.
            _ if self.job_cancel.is_some() => bar
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(SharedString::from(msg))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_color(self.palette.dim)
                        .child(kbd::key_chip(
                            "ctrl-g",
                            self.palette.dim,
                            &self.font,
                            &self.system_ui_font,
                        ))
                        .child(SharedString::from("to cancel")),
                ),
            // A plain message, possibly multi-line (a command's full output):
            // one row per line so it renders as a block, not run together.
            _ => bar.flex().flex_col().children(
                msg.lines()
                    .map(|l| SharedString::from(l.to_string()))
                    .collect::<Vec<_>>(),
            ),
        })
    }

    fn render_overlays(
        &self,
        mut root: gpui::Div,
        view: &Entity<Self>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if let Some(popup) = &self.popup {
            root = root.child(match popup {
                Popup::Transient(state) => self
                    .render_transient(&state.def, Some(state), window, view)
                    .into_any_element(),
                Popup::Dispatch(def) => self
                    .render_transient(def, None, window, view)
                    .into_any_element(),
                Popup::Picker(state) => self.render_picker(state, view).into_any_element(),
            });
        } else if let Some((prompt, _)) = &self.confirm {
            root = root.child(
                self.bottom_bar(self.palette.banner)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(SharedString::from(prompt.clone()))
                    .child(self.key_action("confirm-yes", "y", "yes", view, Self::confirm_yes))
                    .child(self.key_action("confirm-no", "n", "no", view, Self::confirm_no)),
            );
        } else if self.selection.visual.is_some() {
            root = root.child(
                self.bottom_bar(self.palette.visual)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from("VISUAL")),
                    )
                    .child(self.key_action("visual-stage", "s", "stage", view, Self::visual_stage))
                    .child(self.key_action(
                        "visual-unstage",
                        "u",
                        "unstage",
                        view,
                        Self::visual_unstage,
                    ))
                    .child(self.key_action(
                        "visual-discard",
                        "x",
                        "discard",
                        view,
                        Self::visual_discard,
                    ))
                    .child(self.key_action(
                        "visual-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::visual_cancel,
                    )),
            );
        } else {
            // The status/error/"Copied" banner: click it (or press Esc) to dismiss.
            root = root.children(self.status_toast(cx));
        }

        let bottom_bar = self.confirm.is_some()
            || self.selection.visual.is_some()
            || self.toast.message.is_some()
            || self.pending_prefix.is_some();
        if self.popup.is_none() && !bottom_bar {
            let tip_font = self.font.clone();
            root = root.child(
                div()
                    .absolute()
                    .bottom_3()
                    .right_4()
                    .child(track_target("dispatch-help"))
                    .child(
                        div()
                            .id("dispatch-help")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(14.0))
                            .cursor_pointer()
                            // A subtle opaque fill (with a faint border) so the
                            // button reads as a control, not clashing with the
                            // text it floats over; `occlude` keeps a click on it
                            // from falling through to the row beneath.
                            .bg(self.palette.panel)
                            .border_1()
                            .border_color(self.palette.border)
                            .occlude()
                            .text_color(self.palette.dim)
                            .hover(|s| s.bg(self.palette.selection).text_color(self.palette.fg))
                            .child(SharedString::from("?"))
                            .tooltip(move |window, cx| {
                                let font = tip_font.clone();
                                Tooltip::element(move |_, _| {
                                    div().font_family(font.clone()).child("Help (?)")
                                })
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.popup = Some(Popup::Dispatch(dispatch_menu_for(this)));
                                cx.notify();
                            })),
                    ),
            );
        }

        root.children(self.prefix_indicator(window))
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep keyboard focus on the status view whenever nothing else owns the
        // keyboard (the commit editor, settings, and the picker each have
        // their own focused input), so keys always land — including debug-channel
        // keystrokes while the window isn't frontmost.
        let owns_focus_elsewhere = self.editor().is_some()
            || self.settings().is_some()
            || matches!(self.popup, Some(Popup::Picker(_)));
        if !owns_focus_elsewhere && !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
        }
        self.palette = Palette::from_theme(cx);

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .key_context(STATUS_CONTEXT)
            // A click that activates the window (macOS first-mouse) should only
            // focus it, not fire the row/button under the cursor. Swallow that
            // one click in the capture phase, before any element arms its click.
            .capture_any_mouse_down(cx.listener(|this, ev: &gpui::MouseDownEvent, _window, cx| {
                // Any click dismisses an open chrome Copy menu; clear the flag
                // here in the capture phase (root-first), so the opening
                // right-click's bubble-phase handler can re-set it afterward.
                this.ctx_menu_open = false;
                // Reset per-press: a selectable row's own mouse-down sets this,
                // so the root's bubble handler can tell a click landed on text.
                this.click_hit_selectable = false;
                if ev.first_mouse {
                    cx.stop_propagation();
                    return;
                }
                // A click ends any pending key sequence and dismisses which-key
                // (like pressing Esc), then proceeds to whatever it clicked on.
                if this.pending_prefix.take().is_some() {
                    cx.notify();
                }
            }))
            // Bubble phase (fires after any row's own mouse-down): a left click
            // that didn't land on selectable text dismisses the active selection,
            // so clicking empty space, chrome, or a section header clears it too.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    if !this.click_hit_selectable && this.clear_point_selection() {
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleFold, window, cx| {
                // Tab is delivered as an action (gpui's Root binds it for
                // focus-nav, which we override here), but its *effect* routes
                // through the keymap like any key, so rebinding/unbinding `tab`
                // in `[keymap]` takes effect.
                if this.settings().is_some() {
                    this.cycle_settings_focus(true, window, cx);
                } else if this.editor().is_none()
                    && matches!(this.popup, None | Some(Popup::Dispatch(_)))
                {
                    this.run_dispatch("tab", window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &BackTab, window, cx| {
                // Shift-Tab, likewise overridden from gpui's reverse focus-nav so
                // a `[keymap]` binding for it (or reverse settings-field cycling)
                // works instead of being swallowed.
                if this.settings().is_some() {
                    this.cycle_settings_focus(false, window, cx);
                } else if this.editor().is_none()
                    && matches!(this.popup, None | Some(Popup::Dispatch(_)))
                {
                    this.run_dispatch("shift-tab", window, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, cx| {
                // Quit when closing the last window (no windowless lingering).
                let last = cx.windows().len() <= 1;
                window.remove_window();
                if last {
                    cx.quit();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                if this.editor().is_none() && this.popup.is_none() && this.settings().is_none() {
                    this.open_settings(window, cx);
                }
            }))
            // Menu-bar items that act on this window's view, routed through the
            // command registry like their keyboard equivalents.
            .on_action(cx.listener(|this, _: &menus::CheckForUpdates, window, cx| {
                this.invoke_command("check-updates", window, cx)
            }))
            .on_action(cx.listener(|this, _: &menus::HelpMenu, window, cx| {
                this.invoke_command("help", window, cx)
            }))
            // Edit > Copy reaches here only when no text input is focused (an
            // input handles it itself): copy the selection/row at point.
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Copy, _window, cx| {
                    this.copy_at_point(cx)
                }),
            )
            // Right-click menu actions, applied to the row at point / selection.
            .on_action(cx.listener(|this, _: &CtxStage, _window, cx| this.act(Op::Stage, cx)))
            .on_action(cx.listener(|this, _: &CtxUnstage, _window, cx| this.act(Op::Unstage, cx)))
            .on_action(cx.listener(|this, _: &CtxDiscard, _window, cx| this.act(Op::Discard, cx)))
            .on_action(cx.listener(|this, _: &CtxTakeOurs, _window, cx| {
                this.resolve_at_point(ConflictSide::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxTakeTheirs, _window, cx| {
                this.resolve_at_point(ConflictSide::Theirs, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxCopy, _window, cx| {
                // A right-clicked chrome value (title-bar ref, detail hash) wins;
                // otherwise copy the row selection at point.
                match this.pending_copy.take() {
                    Some(value) => this.copy_to_clipboard(value, cx),
                    None => this.copy_at_point(cx),
                }
            }))
            // Settings "Open config file" dropdown actions.
            .on_action(
                cx.listener(|this, _: &CopyConfigPath, _window, cx| this.copy_config_path(cx)),
            )
            .on_action(cx.listener(|this, _: &CopyRepoConfigPath, _window, cx| {
                this.copy_repo_config_path(cx)
            }))
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(self.palette.bg)
            .text_color(self.palette.fg)
            .text_size(px(13.0))
            // Proportional UI font is the base for prose chrome; code/diff/
            // tabular rows and the code views override back to monospace. When
            // no UI font is configured, `ui_font` equals `font`, so this is the
            // old all-monospace behavior.
            .font_family(self.ui_font.clone())
            .flex()
            .flex_col();

        // The title bar sits above every view (status, settings, editor, …).
        root = root.child(self.render_title_bar(&view));

        // Each non-Status screen takes over the window. One match defines the
        // active screen (no re-derived priority cascade); Status falls through to
        // the status list below.
        let screen_el: Option<AnyElement> = match &self.screen {
            Screen::Settings(s) => Some(self.render_settings(s, &view).into_any_element()),
            Screen::Editor(ed) => Some(self.render_editor(ed, &view).into_any_element()),
            Screen::GitLog { view: scroll, .. } => {
                Some(self.render_git_log(scroll, &view).into_any_element())
            }
            Screen::RebaseTodo(rt) => Some(self.render_rebase_todo(rt, &view).into_any_element()),
            Screen::Commit { view: cv, .. } => {
                Some(self.render_commit_view(cv, &view).into_any_element())
            }
            Screen::Diff { view: dv, .. } => {
                Some(self.render_diff_view(dv, &view).into_any_element())
            }
            Screen::Log(log) => Some(self.render_log(log, &view).into_any_element()),
            Screen::Refs(refs) => Some(self.render_refs(refs, &view).into_any_element()),
            Screen::Worktree(wt) => Some(self.render_worktrees(wt, &view).into_any_element()),
            Screen::Blame {
                view: scroll,
                path,
                rows,
            } => Some(
                self.render_blame(scroll, path, rows, &view)
                    .into_any_element(),
            ),
            Screen::Status => None,
        };
        if let Some(screen_el) = screen_el {
            return self.render_overlays(root.child(screen_el), &view, window, cx);
        }

        // An in-progress merge/rebase/cherry-pick/revert sits above the list,
        // visible while the user resolves it.
        if let Some(seq) = &self.sequence {
            root = root.child(self.render_sequence_banner(seq, &view));
        }
        if let Some(bisect) = &self.bisect {
            root = root.child(self.render_bisect_banner(bisect, &view));
        }

        // The list takes the flexible space; the status bar (added below)
        // sits beneath it, so showing the bar never shifts content down.
        // Clicking the list area dismisses an open popup or an active visual
        // selection — including clicks on empty space, not just on rows. (A
        // bottom popup panel is a sibling, so clicks on it don't reach here.)
        let dismissable = self.popup.is_some() || self.selection.visual.is_some();
        root = root.child(
            div()
                .id("list-area")
                .relative()
                .w_full()
                .flex_grow(1.0)
                .when(dismissable, |el| {
                    el.on_click(cx.listener(|this, _, _window, cx| {
                        if this.popup.is_some() {
                            this.popup = None;
                        } else {
                            this.selection.visual = None;
                        }
                        cx.notify();
                    }))
                })
                .child(
                    uniform_list("rows", count, {
                        let view = view.clone();
                        move |range, _window, cx| {
                            let this = view.read(cx);
                            range
                                .map(|ix| this.render_row(ix, &view))
                                .collect::<Vec<_>>()
                        }
                    })
                    .track_scroll(&self.scroll)
                    .size_full()
                    .py_2()
                    .px_2()
                    // A drag that overshoots the list's ends clamps to the
                    // first/last selectable row instead of freezing wherever
                    // the pointer last crossed a row (see drag_row_beyond_list).
                    .on_mouse_move({
                        let view = view.clone();
                        move |ev: &gpui::MouseMoveEvent, _window, cx| {
                            if ev.pressed_button != Some(MouseButton::Left) {
                                return;
                            }
                            view.update(cx, |v, vcx| {
                                let Some(anchor) = v.selection.drag_anchor else {
                                    return;
                                };
                                let Some(ix) =
                                    drag_row_beyond_list(&v.scroll, v.rows.len(), ev.position)
                                else {
                                    return;
                                };
                                // Snap to a selectable row (headers/spacers pad
                                // the list's ends), and leave the anchor row's
                                // precise char state alone.
                                let ix = if ix >= anchor {
                                    (0..=ix).rev().find(|&i| v.rows[i].selectable)
                                } else {
                                    (ix..v.rows.len()).find(|&i| v.rows[i].selectable)
                                };
                                let Some(ix) = ix.filter(|&i| i != anchor) else {
                                    return;
                                };
                                if v.status_drag().mouse_move(ix, None) {
                                    vcx.notify();
                                }
                            });
                        }
                    }),
                )
                .vertical_scrollbar(&self.scroll),
        );

        self.render_overlays(root, &view, window, cx)
    }
}

//! Cursor navigation, selection, fold toggling, and selection-anchor
//! preservation for [`StatusView`]. Split out of `main.rs`; these are
//! `impl StatusView` methods over the row list and fold state.

use gpui::{Context, Window};

use crate::*;

/// The line-range selection gesture, in its three entry forms: `v` (or a drag)
/// anchors a visual selection; a shift-click extends from the previous cursor
/// row. One state machine spread over keyboard and mouse handlers.
#[derive(Default)]
pub(crate) struct Selection {
    /// Anchor row of an active visual (region) selection; `None` when off.
    /// The selection spans `min(anchor, selected)..=max(anchor, selected)`.
    pub(crate) visual: Option<usize>,
    /// Row where a left-button drag began, while the button is held. Dragging
    /// across rows turns into a visual selection (mouse equivalent of `v`).
    pub(crate) drag_anchor: Option<usize>,
    /// Byte offset a left-drag anchored at within the anchor row's text (only
    /// for a text row that can char-select). Paired with [`drag_anchor`] so a
    /// same-row drag builds a [`CharSelection`]; `None` on a non-text row.
    pub(crate) char_anchor: Option<usize>,
    /// Set by a shift-click mouse-down so the following click extends the
    /// selection (and doesn't toggle the row's fold).
    pub(crate) shift_click: bool,
    /// Set by a mouse-down on a row that had an active char selection, so the
    /// following click just clears the selection (rather than firing Enter); the
    /// click after that — with no selection — fires Enter as usual.
    pub(crate) char_click: bool,
}

/// A character-range selection over a view's rows, its endpoints as
/// `(row index, byte offset)` into each row's rendered text. Drives sub-line
/// mouse selection in the read-only views; a drag may span rows — the rows
/// between the endpoints select whole. At most one is active per view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CharSelection {
    /// Where the drag anchored.
    pub(crate) anchor: (usize, usize),
    /// Where the drag currently reaches.
    pub(crate) cursor: (usize, usize),
}

impl CharSelection {
    /// A selection within one row's text (a word select, the header line).
    pub(crate) fn on_row(row: usize, anchor: usize, cursor: usize) -> Self {
        CharSelection {
            anchor: (row, anchor),
            cursor: (row, cursor),
        }
    }

    /// The earlier endpoint (row-major order).
    fn start(&self) -> (usize, usize) {
        self.anchor.min(self.cursor)
    }

    /// The later endpoint (row-major order).
    fn end(&self) -> (usize, usize) {
        self.anchor.max(self.cursor)
    }

    /// Whether nothing is actually selected (anchor == cursor).
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// The rows the selection touches, first..=last.
    pub(crate) fn rows(&self) -> std::ops::RangeInclusive<usize> {
        self.start().0..=self.end().0
    }

    /// The selected byte range on row `ix`: partial on an endpoint row, the
    /// whole line between (`usize::MAX`, which the render/copy paths clamp to
    /// the row's length). `None` when the row isn't covered or the selection
    /// is empty.
    pub(crate) fn range_on(&self, ix: usize) -> Option<std::ops::Range<usize>> {
        if self.is_empty() {
            return None;
        }
        let (start, end) = (self.start(), self.end());
        if ix < start.0 || ix > end.0 {
            return None;
        }
        let lo = if ix == start.0 { start.1 } else { 0 };
        let hi = if ix == end.0 { end.1 } else { usize::MAX };
        Some(lo..hi)
    }

    /// The selected slice of row `ix`'s text, clamped to char boundaries
    /// within bounds. `None` when the row isn't covered.
    pub(crate) fn slice_on<'a>(&self, ix: usize, text: &'a str) -> Option<&'a str> {
        let range = self.range_on(ix)?;
        let start = clamp_boundary(text, range.start);
        let end = clamp_boundary(text, range.end.max(start));
        Some(&text[start..end])
    }
}

/// Clamp `offset` down to the nearest char boundary at or before it, within
/// `text` (so a byte offset from hit-testing can safely slice the string).
pub(crate) fn clamp_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// A mutable view over one surface's drag-selection state — the char/line
/// mouse-selection machine shared by the status rows, the flattened diff
/// views, and the log. Each surface packs its own fields into this (see
/// `StatusView::status_drag`, `FlatDiff::drag`, `LogState::drag`), so the
/// transitions — press arms a drag; movement on the anchor row selects
/// char-wise, spanning rows goes line-wise, returning collapses back;
/// release disarms — are written once and can't drift apart per view.
pub(crate) struct DragState<'a> {
    /// Anchor row of an active line-wise (visual) region.
    pub(crate) visual: &'a mut Option<usize>,
    /// The active same-row character selection.
    pub(crate) char_sel: &'a mut Option<CharSelection>,
    /// Row a held left-drag began on.
    pub(crate) drag_anchor: &'a mut Option<usize>,
    /// Byte offset the drag anchored at (only on selectable text).
    pub(crate) char_anchor: &'a mut Option<usize>,
    /// Set when the press landed on a live char selection: the coming click
    /// only clears it.
    pub(crate) char_click: &'a mut bool,
    /// The surface's cursor row.
    pub(crate) selected: &'a mut usize,
}

impl DragState<'_> {
    /// A left press at row `ix` (`offset` = byte under the pointer when the
    /// row is selectable text): arm a drag there, clearing any prior
    /// selection. The caller repaints.
    pub(crate) fn mouse_down(&mut self, ix: usize, offset: Option<usize>) {
        *self.char_click = self.char_sel.is_some_and(|c| c.range_on(ix).is_some());
        *self.char_sel = None;
        *self.visual = None;
        *self.drag_anchor = Some(ix);
        *self.char_anchor = offset;
        *self.selected = ix;
    }

    /// A held drag reaching row `ix` / byte `offset`. A drag anchored on
    /// selectable text selects char-wise — across rows too, the endpoints as
    /// (row, offset); a row without text along the way pins to its start. A
    /// drag anchored on a non-text row selects line-wise. Returns whether
    /// anything changed (repaint).
    pub(crate) fn mouse_move(&mut self, ix: usize, offset: Option<usize>) -> bool {
        let Some(anchor) = *self.drag_anchor else {
            return false;
        };
        if let Some(a) = *self.char_anchor {
            let sel = CharSelection {
                anchor: (anchor, a),
                cursor: (ix, offset.unwrap_or(0)),
            };
            // The line-wise region mirrors the spanned rows, so acting on the
            // region (stage/unstage/discard the dragged rows) works from a
            // char drag too; rendering shows only the char highlight.
            let visual = (ix != anchor).then_some(anchor);
            if *self.char_sel == Some(sel) && *self.visual == visual && *self.selected == ix {
                return false;
            }
            *self.char_sel = Some(sel);
            *self.visual = visual;
            *self.selected = ix;
            return true;
        }
        if ix == anchor {
            if self.visual.is_some() || *self.selected != anchor {
                *self.visual = None;
                *self.char_sel = None;
                *self.selected = anchor;
                true
            } else {
                false
            }
        } else {
            // Spanned rows → line-wise region.
            if *self.selected == ix && *self.visual == Some(anchor) && self.char_sel.is_none() {
                return false;
            }
            *self.char_sel = None;
            *self.visual = Some(anchor);
            *self.selected = ix;
            true
        }
    }

    /// Button release: disarm the drag (the selection itself stays). Returns
    /// whether anything changed (repaint).
    pub(crate) fn mouse_up(&mut self) -> bool {
        if self.drag_anchor.take().is_some() {
            *self.char_anchor = None;
            true
        } else {
            false
        }
    }
}

impl StatusView {
    /// The status surface's drag-selection state, packed for [`DragState`].
    pub(crate) fn status_drag(&mut self) -> DragState<'_> {
        DragState {
            visual: &mut self.selection.visual,
            char_sel: &mut self.char_sel,
            drag_anchor: &mut self.selection.drag_anchor,
            char_anchor: &mut self.selection.char_anchor,
            char_click: &mut self.selection.char_click,
            selected: &mut self.selected,
        }
    }
}

impl StatusView {
    // --- Selection & folding ---------------------------------------------

    pub(crate) fn move_selection(&mut self, delta: isize) {
        // Keyboard motion drops a mouse char selection (it belongs to the row it
        // was dragged on, not wherever the cursor moves next).
        self.char_sel = None;
        if self.rows.is_empty() {
            return;
        }
        let mut i = self.selected as isize;
        loop {
            i += delta;
            if i < 0 || i >= self.rows.len() as isize {
                return;
            }
            if self.rows[i as usize].selectable {
                self.selected = i as usize;
                return;
            }
        }
    }

    /// Move the cursor by ~`delta` rows for paging (Ctrl-d/u/f/b): clamp the
    /// target into range, then snap to the nearest selectable row (so paging at
    /// the ends lands on the last/first selectable row rather than stalling).
    pub(crate) fn page_selection(&mut self, delta: isize) {
        self.char_sel = None;
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let target = (self.selected as isize + delta).clamp(0, last);
        for d in 0..=last {
            for cand in [target + d, target - d] {
                if (0..=last).contains(&cand) && self.rows[cand as usize].selectable {
                    self.selected = cand as usize;
                    return;
                }
            }
        }
    }

    pub(crate) fn select_edge(&mut self, last: bool) {
        self.char_sel = None;
        let found = if last {
            (0..self.rows.len())
                .rev()
                .find(|&i| self.rows[i].selectable)
        } else {
            (0..self.rows.len()).find(|&i| self.rows[i].selectable)
        };
        if let Some(i) = found {
            self.selected = i;
        }
    }

    /// Move to the next/previous visible section start at any depth — headers,
    /// files, commits, stashes, hunk headers — magit's `magit-section-forward`
    /// / `-backward`. Backward from inside a section's content lands on the
    /// section's own start first (magit's "beginning of the current section"),
    /// which falls out of scanning upward for the nearest start.
    pub(crate) fn select_section(&mut self, forward: bool) {
        self.char_sel = None;
        let next = if forward {
            (self.selected + 1..self.rows.len()).find(|&i| section_depth(&self.rows[i]).is_some())
        } else {
            (0..self.selected)
                .rev()
                .find(|&i| section_depth(&self.rows[i]).is_some())
        };
        if let Some(i) = next {
            self.selected = i;
        }
    }

    /// Move to the next/previous *sibling* section — the closest section start
    /// at the same depth, stopping at the enclosing section's boundary
    /// (magit's `magit-section-forward-sibling` / `-backward-sibling`). With no
    /// sibling in that direction, fall back to the fine-grained motion, as
    /// magit does.
    pub(crate) fn select_section_sibling(&mut self, forward: bool) {
        self.char_sel = None;
        // The current section: this row if it starts one, else the nearest
        // start above (the section the row is inside).
        let Some(cur) = (0..=self.selected)
            .rev()
            .find(|&i| section_depth(&self.rows[i]).is_some())
        else {
            return self.select_section(forward);
        };
        let depth = section_depth(&self.rows[cur]).unwrap();
        let sibling = if forward {
            (cur + 1..self.rows.len())
                .map(|i| (i, section_depth(&self.rows[i])))
                .filter_map(|(i, d)| d.map(|d| (i, d)))
                // A shallower start means we left the parent: no more siblings.
                .take_while(|&(_, d)| d >= depth)
                .find(|&(_, d)| d == depth)
        } else {
            (0..cur)
                .rev()
                .map(|i| (i, section_depth(&self.rows[i])))
                .filter_map(|(i, d)| d.map(|d| (i, d)))
                .take_while(|&(_, d)| d >= depth)
                .find(|&(_, d)| d == depth)
        };
        match sibling {
            Some((i, _)) => self.selected = i,
            None => self.select_section(forward),
        }
    }

    // --- Unified, screen-aware navigation ---------------------------------
    // One [keymap] drives motion in every cursor view: the registry's
    // Navigation commands resolve to these, dispatched to the active screen.

    /// Move the cursor/selection by `delta` rows in the active view.
    pub(crate) fn nav_line(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } | Screen::Diff { .. } => self.flat_diff_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            Screen::Refs(_) => self.refs_move(delta, cx),
            Screen::Worktree(_) => self.worktrees_move(delta, cx),
            _ => {
                self.move_selection(delta);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Page the cursor by a half- or full-screen in the active view.
    pub(crate) fn nav_page(
        &mut self,
        down: bool,
        full: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = page_rows(window, self.row_h()) as isize;
        let amount = if full { page } else { (page / 2).max(1) };
        let delta = if down { amount } else { -amount };
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } | Screen::Diff { .. } => self.flat_diff_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            Screen::Refs(_) => self.refs_move(delta, cx),
            Screen::Worktree(_) => self.worktrees_move(delta, cx),
            _ => {
                self.page_selection(delta);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Jump to the first/last row of the active view.
    pub(crate) fn nav_edge(&mut self, to_bottom: bool, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Log(_)
            | Screen::Commit { .. }
            | Screen::Diff { .. }
            | Screen::RebaseTodo(_)
            | Screen::Refs(_)
            | Screen::Worktree(_) => self.nav_line(
                if to_bottom {
                    isize::MAX / 2
                } else {
                    isize::MIN / 2
                },
                cx,
            ),
            _ => {
                self.select_edge(to_bottom);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Move to the next/previous section start. Only the status view has
    /// sections; a no-op elsewhere.
    pub(crate) fn nav_section(&mut self, forward: bool, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Status) {
            self.select_section(forward);
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Move to the next/previous sibling section. Status view only.
    pub(crate) fn nav_section_sibling(&mut self, forward: bool, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Status) {
            self.select_section_sibling(forward);
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Move to the parent section's start (magit-section-up). The current
    /// section of a content row is the one it's inside, so a diff line's
    /// parent is its file — as in magit.
    pub(crate) fn nav_section_up(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Status) {
            return;
        }
        let Some(cur) = (0..=self.selected)
            .rev()
            .find(|&i| section_depth(&self.rows[i]).is_some())
        else {
            return;
        };
        let depth = section_depth(&self.rows[cur]).unwrap();
        let parent = (0..cur)
            .rev()
            .find(|&i| section_depth(&self.rows[i]).is_some_and(|d| d < depth));
        if let Some(i) = parent {
            self.selected = i;
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Set the fold depth buffer-wide (magit's `magit-section-show-level-N`):
    /// 1 = sections collapsed, 2 = files visible, 3 = hunks visible (closed),
    /// 4 = everything open. Levels 3/4 expand every file, which loads any
    /// diffs not yet fetched; level 3 marks those so their hunks arrive
    /// collapsed too.
    pub(crate) fn nav_show_level(&mut self, level: u8, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Status) {
            return;
        }
        self.selection.visual = None;
        self.collapse_new_hunks = false;
        self.collapsed_hunks.clear();
        match level {
            1 => self.expanded.clear(),
            2 => {
                self.expanded = SectionId::ALL
                    .iter()
                    .map(|s| FoldKey::Section(*s))
                    .collect();
            }
            3 | 4 => {
                self.expanded = SectionId::ALL
                    .iter()
                    .map(|s| FoldKey::Section(*s))
                    .collect();
                // Expand every file in the diff-backed sections, loading any
                // diff not yet fetched (the same path a manual expand takes).
                let files: Vec<(DiffSource, String)> = self
                    .status
                    .as_ref()
                    .map(|status| {
                        status
                            .unstaged()
                            .map(|e| (DiffSource::Unstaged, e.path.clone()))
                            .chain(
                                status
                                    .staged()
                                    .map(|e| (DiffSource::Staged, e.path.clone())),
                            )
                            .collect()
                    })
                    .unwrap_or_default();
                for (source, path) in files {
                    self.expanded.insert(FoldKey::File(source, path.clone()));
                    self.ensure_diff(source, path, cx);
                }
                if level == 3 {
                    let loaded: Vec<(DiffSource, String, usize)> = self
                        .diff_cache
                        .loaded()
                        .map(|((source, path), diff)| (*source, path.clone(), diff.hunks.len()))
                        .collect();
                    for (source, path, hunks) in loaded {
                        for ix in 0..hunks {
                            self.collapsed_hunks
                                .insert(FoldKey::Hunk(source, path.clone(), ix));
                        }
                    }
                    self.collapse_new_hunks = true;
                }
            }
            _ => return,
        }
        self.persist_fold_state();
        self.rebuild_preserving_selection();
        cx.notify();
    }

    /// Cycle every section's visibility (magit's `magit-section-cycle-global`,
    /// `S-TAB`): with any section collapsed, show all section headings (files
    /// closed); with the sections open but something inside collapsed, open
    /// everything; with everything open, collapse all sections.
    pub(crate) fn nav_cycle_global(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Status) {
            return;
        }
        let (mut section_closed, mut lower_closed) = (false, false);
        for row in &self.rows {
            if let Some(key) = &row.fold {
                let closed = !self.is_fold_open(key);
                match key {
                    FoldKey::Section(_) => section_closed |= closed,
                    _ => lower_closed |= closed,
                }
            }
        }
        let level = if section_closed {
            2
        } else if lower_closed {
            4
        } else {
            1
        };
        self.nav_show_level(level, cx);
    }

    /// Jump to a status section's header (magit-status-jump). A section with
    /// nothing in it has no rows, so the miss reports rather than moving.
    pub(crate) fn jump_to_section(&mut self, id: SectionId, cx: &mut Context<Self>) {
        if !matches!(self.screen, Screen::Status) {
            return;
        }
        let header = self
            .rows
            .iter()
            .position(|r| matches!(&r.fold, Some(FoldKey::Section(s)) if *s == id));
        match header {
            Some(i) => {
                self.selected = i;
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            }
            None => {
                let label = match id {
                    SectionId::Untracked => "untracked files",
                    SectionId::Unstaged => "unstaged changes",
                    SectionId::Staged => "staged changes",
                    SectionId::Stashes => "stashes",
                    SectionId::Unpushed => "unpushed commits",
                    SectionId::Unpulled => "unpulled commits",
                    SectionId::UnpushedPushremote => "unpushed (push remote) commits",
                    SectionId::UnpulledPushremote => "unpulled (push remote) commits",
                    SectionId::Recent => "recent commits",
                    SectionId::Ignored => "ignored files",
                };
                self.set_status(format!("No {label} section"), true, cx);
            }
        }
        cx.notify();
    }

    /// Shared key handling for the cursor views (status / log / commit / rebase
    /// todo): the `g` prefix, the fixed motion aliases (arrows, Ctrl-paging,
    /// `]`/`[`), and the remappable motion keys resolved through the effective
    /// keymap. Returns whether it consumed the key.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_nav(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
        cmd: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // All motions (arrows, `C-d`, Space, `]`, the `g` prefix, …) resolve
        // through the effective keymap — there are no hardcoded aliases. The
        // full chord (cmd included), so an unhandled cmd-chord falls through to
        // the OS/app layer instead of being consumed as its bare key.
        let chord = chord(key, shift, ctrl, alt, cmd);
        // A prefix key begins a sequence.
        if self.is_prefix(&chord) {
            self.enter_prefix(chord, window, cx);
            return true;
        }
        // Run only if a candidate is a motion, so a command key (e.g. `s`) isn't
        // fired in a non-status view. Motions don't share keys with other
        // commands, so at most one candidate qualifies.
        let motion = self.screen_bindings().get(&chord).and_then(|cands| {
            cands
                .iter()
                .find(|id| {
                    commands()
                        .iter()
                        .any(|c| c.id == id.as_str() && c.category == Category::Navigation)
                })
                .cloned()
        });
        if let Some(id) = motion {
            self.invoke_command(&id, window, cx);
            true
        } else {
            false
        }
    }

    /// One keystroke on a pager screen (the `$` command log or blame — no
    /// cursor): a registry verb bound at the full chord dispatches (`close`,
    /// the log's toggle-queries); anything else scrolls less-style, with a
    /// keymap-resolved motion translated to the key [`apply_scroll_key`]
    /// understands. Resolving the full chord keeps modifier bindings (the
    /// default `ctrl-n`/`ctrl-p`, or a user remap) driving the pager too.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pager_key(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
        cmd: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let chorded = chord(key, shift, ctrl, alt, cmd);
        let bound = self
            .screen_bindings()
            .get(&chorded)
            .and_then(|v| v.first())
            .map(String::as_str);
        let (skey, sshift) = match bound {
            Some("close") => {
                // A first Esc/q clears an active mouse selection; the next
                // closes (like the flat-diff views).
                if self.pager_sel.char_sel.take().is_some() {
                    cx.notify();
                    return;
                }
                return self.close_screen(window, cx);
            }
            Some("git-log-toggle-queries") => return self.toggle_git_log_all(window, cx),
            Some("yank") => return self.copy_pager_selection(cx),
            Some("move-down") => ("j", false),
            Some("move-up") => ("k", false),
            Some("goto-bottom") => ("g", true),
            Some("goto-top") => ("g", false),
            _ => (key, shift),
        };
        let page = page_rows(window, self.row_h());
        let len = match &self.screen {
            Screen::GitLog { .. } => self.git_log_rows().len(),
            Screen::Blame { rows, .. } => rows.len(),
            _ => return,
        };
        if let Screen::GitLog { view, .. } | Screen::Blame { view, .. } = &mut self.screen {
            apply_scroll_key(&view.scroll, &mut view.top, len, skey, sshift, ctrl, page);
        }
        cx.notify();
    }

    pub(crate) fn toggle_fold(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.point_fold_key() else {
            return;
        };
        let open = self.is_fold_open(&key);
        self.apply_fold(&key, !open, cx);
    }

    /// Explicitly show (`zo`) or hide (`zc`) the fold at point, rather than
    /// toggling — evil-collection's `magit-section-show`/`hide`.
    pub(crate) fn fold_at_point(&mut self, show: bool, cx: &mut Context<Self>) {
        let Some(key) = self.point_fold_key() else {
            return;
        };
        self.apply_fold(&key, show, cx);
    }

    /// Show (`zO`) or hide (`zC`) the *children* of the node at point —
    /// evil-collection's `magit-section-show`/`hide-children`. On a section that
    /// means its files (showing first opens the section so they materialize);
    /// on a file, its hunks. A hunk has no children, so this shows/hides itself.
    pub(crate) fn fold_children_at_point(&mut self, show: bool, cx: &mut Context<Self>) {
        self.selection.visual = None;
        self.collapse_new_hunks = false;
        let Some(key) = self.point_fold_key() else {
            return;
        };
        if matches!(key, FoldKey::Hunk(..)) {
            return self.apply_fold(&key, show, cx);
        }
        let node_indent = self.rows.get(self.selected).map(|r| r.indent).unwrap_or(0);
        // Showing children first opens the node itself so its child rows exist.
        if show {
            self.set_fold(&key, true, cx);
            self.rebuild_preserving_selection();
        }
        // The descendant rows are those deeper than the node, up to its next
        // sibling. `rebuild_preserving_selection` kept the cursor on the node.
        let node_ix = self.selected;
        let children: Vec<FoldKey> = self
            .rows
            .iter()
            .skip(node_ix + 1)
            .take_while(|r| r.indent > node_indent)
            .filter_map(|r| r.fold.clone())
            .collect();
        for child in &children {
            self.set_fold(child, show, cx);
        }
        if matches!(key, FoldKey::Section(_)) {
            self.persist_fold_state();
        }
        self.rebuild_preserving_selection();
        cx.notify();
    }

    /// The fold key the cursor acts on: the row's own key, or — for a diff line
    /// — its enclosing hunk, so a fold command anywhere inside a hunk hits it
    /// (like magit).
    fn point_fold_key(&self) -> Option<FoldKey> {
        let row = self.rows.get(self.selected);
        row.and_then(|r| r.fold.clone())
            .or_else(|| match row.map(|r| &r.target) {
                Some(Some(Target::Line { file, hunk, .. })) => section_source(file.section)
                    .map(|src| FoldKey::Hunk(src, file.path.clone(), *hunk)),
                _ => None,
            })
    }

    /// Whether `key` is currently expanded. Hunks default to expanded (state in
    /// `collapsed_hunks`, present = collapsed); sections/files use `expanded`.
    fn is_fold_open(&self, key: &FoldKey) -> bool {
        if matches!(key, FoldKey::Hunk(..)) {
            !self.collapsed_hunks.contains(key)
        } else {
            self.expanded.contains(key)
        }
    }

    /// Set `key`'s fold state without rebuilding — the shared primitive behind
    /// toggle/show/hide. Expanding a file loads its diff.
    fn set_fold(&mut self, key: &FoldKey, expand: bool, cx: &mut Context<Self>) {
        if matches!(key, FoldKey::Hunk(..)) {
            if expand {
                self.collapsed_hunks.remove(key);
            } else {
                self.collapsed_hunks.insert(key.clone());
            }
        } else if expand {
            self.expanded.insert(key.clone());
            if let FoldKey::File(source, path) = key {
                self.ensure_diff(*source, path.clone(), cx);
            }
        } else {
            self.expanded.remove(key);
        }
    }

    /// Set one fold and rebuild — the single-node path (toggle/show/hide),
    /// clearing the visual anchor (row indices shift) and persisting sections.
    fn apply_fold(&mut self, key: &FoldKey, expand: bool, cx: &mut Context<Self>) {
        // Folding changes row indices, which would invalidate a visual anchor.
        self.selection.visual = None;
        // A manual fold ends fold level 3's claim on newly loaded diffs.
        self.collapse_new_hunks = false;
        self.set_fold(key, expand, cx);
        // Section fold state persists per repo (files/hunks stay ephemeral).
        if matches!(key, FoldKey::Section(_)) {
            self.persist_fold_state();
        }
        // Restore the cursor to the same node: collapsing a hunk from one of its
        // lines lands on the hunk header (the line is gone, so the anchor
        // degrades to it); folding/unfolding otherwise keeps the header.
        self.rebuild_preserving_selection();
        cx.notify();
    }

    /// Persist which status sections are collapsed to the repo scope, so the
    /// fold layout survives a restart. Sections are expanded by default, so we
    /// store only the collapsed ones. No-op without a repo scope.
    pub(crate) fn persist_fold_state(&self) {
        let Some(dir) = &self.worktree_scope_dir else {
            return;
        };
        let collapsed = SectionId::ALL
            .iter()
            .filter(|s| !self.expanded.contains(&FoldKey::Section(**s)))
            .map(|s| s.config_id().to_string())
            .collect();
        state::save_toml(
            &state::scoped_path(dir, state::FOLDS_FILE),
            &state::FoldState {
                collapsed,
                commit_details_expanded: self.commit_details_expanded,
                commit_editor_height: (self.editor_message_height != EDITOR_MESSAGE_HEIGHT_DEFAULT)
                    .then_some(self.editor_message_height),
            },
        );
    }

    /// The commit at point in a status section, as `(hash, short_hash, subject)`.
    pub(crate) fn point_commit(&self) -> Option<(String, String, String)> {
        match self.rows.get(self.selected).map(|r| &r.kind) {
            Some(RowKind::Commit {
                hash,
                short_hash,
                subject,
                ..
            }) => Some((hash.clone(), short_hash.clone(), subject.clone())),
            _ => None,
        }
    }

    /// The stash at point in the Stashes section, as `(reference, message)`.
    pub(crate) fn point_stash(&self) -> Option<(String, String)> {
        match self.rows.get(self.selected).map(|r| &r.kind) {
            Some(RowKind::Stash { reference, message }) => {
                Some((reference.clone(), message.clone()))
            }
            _ => None,
        }
    }

    /// Capture the current selection for restoration after a rebuild.
    pub(crate) fn capture_anchor(&self) -> Option<SelAnchor> {
        capture_row_anchor(&self.rows, self.selected)
    }

    /// Restore the selection captured by [`capture_anchor`] after a rebuild.
    pub(crate) fn restore_anchor(&mut self, anchor: Option<SelAnchor>) {
        self.selected = restored_row(&self.rows, self.selected, anchor.as_ref());
    }

    /// Rebuild rows while keeping the cursor on the same logical row.
    pub(crate) fn rebuild_preserving_selection(&mut self) {
        let anchor = self.capture_anchor();
        self.rebuild_rows();
        self.restore_anchor(anchor);
    }
}

// --- Selection restoration across rebuilds ---------------------------------
//
// Rather than keep the cursor at the same numeric row index (which may mean
// something unrelated after staging/folding), we capture the selected row's
// logical identity before a rebuild and restore it to the same place — or, if
// that's gone, to a sensible nearby row within the same section. Pure over the
// row list so the degradation ladder and fallbacks are unit-testable.

/// Clamp `selected` into `rows`, snapping to the nearest selectable row
/// (downward first, then upward).
fn clamp_row(rows: &[Row], selected: usize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let ix = selected.min(rows.len() - 1);
    if rows[ix].selectable {
        return ix;
    }
    let down = (ix..rows.len()).find(|&i| rows[i].selectable);
    let up = || (0..ix).rev().find(|&i| rows[i].selectable);
    down.or_else(up).unwrap_or(ix)
}

/// The logical identity of the row at `ix`.
fn row_ident(rows: &[Row], ix: usize) -> AnchorIdent {
    match rows.get(ix) {
        Some(Row {
            target: Some(t), ..
        }) => match t {
            Target::File(f) => AnchorIdent::File(f.section, f.path.clone()),
            Target::Hunk { file, hunk } => {
                AnchorIdent::Hunk(file.section, file.path.clone(), *hunk)
            }
            Target::Line { file, hunk, line } => {
                AnchorIdent::Line(file.section, file.path.clone(), *hunk, *line)
            }
        },
        Some(Row {
            fold: Some(FoldKey::Section(s)),
            ..
        }) => AnchorIdent::Section(*s),
        // Commit/stash rows carry no Target/fold; anchor by content, finding
        // the enclosing section header for the commit case.
        Some(Row {
            kind: RowKind::Commit { hash, .. },
            ..
        }) => match enclosing_section(rows, ix) {
            Some(s) => AnchorIdent::Commit(s, hash.clone()),
            None => AnchorIdent::Top,
        },
        Some(Row {
            kind: RowKind::Stash { reference, .. },
            ..
        }) => AnchorIdent::Stash(reference.clone()),
        _ => AnchorIdent::Top,
    }
}

/// The section a row belongs to, by scanning back to the nearest section
/// header at or above it.
fn enclosing_section(rows: &[Row], ix: usize) -> Option<SectionId> {
    (0..=ix)
        .rev()
        .find_map(|i| match rows.get(i).map(|r| &r.fold) {
            Some(Some(FoldKey::Section(s))) => Some(*s),
            _ => None,
        })
}

/// The row indices belonging to a section: its header through the row before
/// the next section header (or end).
fn section_rows(rows: &[Row], section: SectionId) -> Vec<usize> {
    let Some(start) = (0..rows.len()).find(|&i| rows[i].fold == Some(FoldKey::Section(section)))
    else {
        return Vec::new();
    };
    let mut out = vec![start];
    for (i, row) in rows.iter().enumerate().skip(start + 1) {
        if matches!(row.kind, RowKind::Section { .. }) {
            break;
        }
        out.push(i);
    }
    out
}

/// Capture the selection at `selected` for restoration after a rebuild.
fn capture_row_anchor(rows: &[Row], selected: usize) -> Option<SelAnchor> {
    if rows.is_empty() {
        return None;
    }
    let ident = row_ident(rows, selected);
    let scope: Vec<usize> = match ident.section() {
        Some(s) => section_rows(rows, s),
        None => (0..rows.len()).collect(),
    };
    let ordinal = scope
        .iter()
        .filter(|&&i| rows[i].selectable)
        .position(|&i| i == selected)
        .unwrap_or(0);
    Some(SelAnchor { ident, ordinal })
}

/// Find the best row for `ident`: exact, else progressively less specific
/// (a missing line falls back to its hunk header, then its file row).
fn locate_ident(rows: &[Row], ident: &AnchorIdent) -> Option<usize> {
    let ladder = match ident {
        AnchorIdent::Line(s, p, h, _) => vec![
            ident.clone(),
            AnchorIdent::Hunk(*s, p.clone(), *h),
            AnchorIdent::File(*s, p.clone()),
        ],
        AnchorIdent::Hunk(s, p, _) => vec![ident.clone(), AnchorIdent::File(*s, p.clone())],
        other => vec![other.clone()],
    };
    ladder
        .iter()
        .find_map(|id| (0..rows.len()).find(|&i| row_ident(rows, i) == *id))
}

/// The row a captured anchor lands on in the rebuilt `rows` (with `selected`
/// as the pre-rebuild cursor, for the anchorless clamp).
fn restored_row(rows: &[Row], selected: usize, anchor: Option<&SelAnchor>) -> usize {
    let Some(anchor) = anchor else {
        return clamp_row(rows, selected);
    };
    if let Some(ix) = locate_ident(rows, &anchor.ident) {
        return clamp_row(rows, ix);
    }
    // The anchored row is gone (e.g. staged away). Stay within the same
    // section at roughly the same ordinal, else fall back to nearest.
    if let Some(section) = anchor.ident.section() {
        let selectable: Vec<usize> = section_rows(rows, section)
            .into_iter()
            .filter(|&i| rows[i].selectable)
            .collect();
        if !selectable.is_empty() {
            return selectable[anchor.ordinal.min(selectable.len() - 1)];
        }
    }
    clamp_row(rows, selected)
}

/// The magit section depth of a row that *starts* a section, or `None` for
/// content/chrome rows (diff lines, messages, spacers). Top-level headers are
/// 0; files, commits, and stashes are 1; hunk headers are 2 — mirroring the
/// status buffer's section tree, flattened.
fn section_depth(row: &Row) -> Option<u8> {
    match &row.kind {
        RowKind::Section { .. } => Some(0),
        RowKind::File { .. } | RowKind::Commit { .. } | RowKind::Stash { .. } => Some(1),
        RowKind::HunkHeader { .. } => Some(2),
        RowKind::Plain { .. } | RowKind::Diff { .. } => None,
    }
}

/// Clamped cursor movement for the simple list screens (log, refs, worktrees,
/// rebase todo): step `selected` by `delta` within `len`, skipping rows
/// `selectable` rejects — past them in the travel direction first, then back
/// the other way (how the refs list hops section headers). `None` when the
/// list is empty or no selectable row is reachable; each screen applies the
/// result to its own cursor + scroll handle.
pub(crate) fn list_move(
    selected: usize,
    len: usize,
    delta: isize,
    selectable: impl Fn(usize) -> bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len as isize - 1;
    let target = (selected as isize + delta).clamp(0, last);
    let step = if delta >= 0 { 1 } else { -1 };
    let mut ix = target;
    while (0..=last).contains(&ix) && !selectable(ix as usize) {
        ix += step;
    }
    if !(0..=last).contains(&ix) || !selectable(ix as usize) {
        ix = target;
        while (0..=last).contains(&ix) && !selectable(ix as usize) {
            ix -= step;
        }
    }
    ((0..=last).contains(&ix) && selectable(ix as usize)).then_some(ix as usize)
}

// --- Scroll math for the read-only list views ------------------------------

/// The viewport height in rows (at `row_h` px per row) — a "page" for the
/// scroll/paging keys.
pub(crate) fn page_rows(window: &Window, row_h: f32) -> usize {
    let height = window.viewport_size().height.as_f32();
    // Leave a few rows for the header/padding so paging keeps a little overlap.
    ((height / row_h) as usize).saturating_sub(3).max(1)
}

/// Apply a vi-style scroll key to a `uniform_list`, updating the caller-tracked
/// top-row index (`top`) and scrolling the handle to it. Returns whether
/// `key` was a recognized scroll command: `j`/`k` line, `Ctrl-d`/`Ctrl-u`
/// half-page, `Ctrl-f`/`Ctrl-b`/`Space` full-page, and `g`/`G` to the ends.
/// Half-page requires Ctrl so plain `d`/`u` stay free for future commands
/// (`d` diff, `u` unstage).
/// The new top-row index a scroll key moves to, or `None` if `key` isn't a
/// scroll command. Clamped so the last page stays on screen. Pure (no handle)
/// so the motion/clamp math is unit-testable; [`apply_scroll_key`] adds the
/// actual scroll. `j`/`k` line, `Ctrl-d`/`Ctrl-u` half-page, `Ctrl-f`/`Ctrl-b`/
/// `Space` full-page, `g`/`G` to the ends.
pub(crate) fn scroll_target(
    top: usize,
    len: usize,
    key: &str,
    shift: bool,
    ctrl: bool,
    page: usize,
) -> Option<usize> {
    let page = (page as isize).max(1);
    let half = (page / 2).max(1);
    let cur = top as isize;
    // The furthest the top can scroll: keep a full last page on screen rather
    // than scrolling content off the bottom.
    let max_top = (len as isize - page).max(0);
    let target = match key {
        "j" => cur + 1,
        "k" => cur - 1,
        "d" if ctrl => cur + half,
        "u" if ctrl => cur - half,
        "space" => cur + page,
        "f" if ctrl => cur + page,
        "b" if ctrl => cur - page,
        "g" if shift => max_top, // G → bottom (last page)
        "g" => 0,                // g → top
        _ => return None,
    };
    Some(target.clamp(0, max_top) as usize)
}

/// The topmost visible row of a `uniform_list` — the handle's test-only
/// `logical_scroll_top_index`, re-derived over its public state. A pending
/// (not-yet-painted) scroll-to-top is honored so rapid keys within one frame
/// compound; a pending Bottom pin is not (its index is the *last* row).
fn scroll_top_index(handle: &UniformListScrollHandle) -> usize {
    let state = handle.0.borrow();
    match state.deferred_scroll_to_item.as_ref() {
        Some(d) if matches!(d.strategy, gpui::ScrollStrategy::Top) => d.item_index,
        _ => state.base_handle.logical_scroll_top().0,
    }
}

/// Where a held drag has gone once it leaves a `uniform_list`'s row area —
/// clamped to the first/last row — or `None` while it's still over rows
/// (whose own handlers track it precisely, including char offsets). The
/// same overshoot problem the commit header fixes: gpui delivers
/// `on_mouse_move` only inside an element's hitbox, so a fast drag past the
/// list's ends would otherwise freeze the selection at the last row the
/// pointer actually crossed. Attach at the list's container, feeding the
/// result to the surface's [`DragState`] with no char offset.
pub(crate) fn drag_row_beyond_list(
    handle: &UniformListScrollHandle,
    len: usize,
    position: gpui::Point<gpui::Pixels>,
    row_h: f32,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let (bounds, top_ix, top_offset) = {
        let state = handle.0.borrow();
        let (ix, offset) = state.base_handle.logical_scroll_top();
        (state.base_handle.bounds(), ix, offset)
    };
    let y = f32::from(position.y - bounds.top()) + f32::from(top_offset);
    let raw = top_ix as isize + (y / row_h).floor() as isize;
    let inside_rows = position.y >= bounds.top()
        && position.y < bounds.bottom()
        && (0..len as isize).contains(&raw);
    if inside_rows {
        return None;
    }
    Some(raw.clamp(0, len as isize - 1) as usize)
}

pub(crate) fn apply_scroll_key(
    handle: &UniformListScrollHandle,
    top: &mut usize,
    len: usize,
    key: &str,
    shift: bool,
    ctrl: bool,
    page: usize,
) -> bool {
    // The user may have wheel-scrolled since the last key: resync the tracked
    // top from the handle first, so a key motion continues from what's on
    // screen instead of snapping back to where the keyboard last left it.
    *top = scroll_top_index(handle);
    let Some(new_top) = scroll_target(*top, len, key, shift, ctrl, page) else {
        return false;
    };
    *top = new_top;
    let max_top = len.saturating_sub(page.max(1));
    // Strict scrolling positions the row even when it's already visible, so line
    // and half-page motions actually move. On the last page, pin the final row
    // to the *bottom* instead — the page-size estimate (header/padding overhead)
    // is slightly off, and pinning guarantees the very last row is reachable.
    if *top >= max_top && len > 0 {
        handle.scroll_to_item_strict(len - 1, gpui::ScrollStrategy::Bottom);
    } else {
        handle.scroll_to_item_strict(*top, gpui::ScrollStrategy::Top);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Anchor-restoration fixtures ------------------------------------
    // Rows built by hand (the shapes rebuild_rows emits): ident/restore read
    // only `fold`, `target`, `selectable`, and the commit/stash kinds.

    fn row(target: Option<Target>, fold: Option<FoldKey>, kind: RowKind) -> Row {
        Row {
            indent: 0,
            selectable: true,
            fold,
            target,
            kind,
        }
    }

    fn section(id: SectionId) -> Row {
        row(
            None,
            Some(FoldKey::Section(id)),
            RowKind::Section {
                title: String::new(),
                count: None,
                expanded: true,
                refreshing: false,
            },
        )
    }

    fn text(label: &str) -> RowKind {
        RowKind::Plain {
            text: label.to_string(),
            color: gpui::hsla(0.0, 0.0, 0.0, 1.0),
        }
    }

    fn file_ref(section: SectionId, path: &str) -> FileRef {
        FileRef {
            section,
            path: path.to_string(),
        }
    }

    fn file(section: SectionId, path: &str) -> Row {
        row(
            Some(Target::File(file_ref(section, path))),
            None,
            text(path),
        )
    }

    fn hunk(section: SectionId, path: &str, hunk: usize) -> Row {
        let target = Target::Hunk {
            file: file_ref(section, path),
            hunk,
        };
        row(Some(target), None, text("@@"))
    }

    fn line(section: SectionId, path: &str, hunk: usize, line: usize) -> Row {
        let target = Target::Line {
            file: file_ref(section, path),
            hunk,
            line,
        };
        row(Some(target), None, text("+x"))
    }

    // A commit row carries no section itself — the enclosing header does.
    fn commit(hash: &str) -> Row {
        row(
            None,
            None,
            RowKind::Commit {
                hash: hash.to_string(),
                short_hash: hash.to_string(),
                subject: String::new(),
                refs: Vec::new(),
            },
        )
    }

    #[test]
    fn anchor_restores_exactly_then_degrades_line_to_hunk_to_file() {
        let unstaged = SectionId::Unstaged;
        let rows = vec![
            section(unstaged),
            file(unstaged, "a.txt"),
            hunk(unstaged, "a.txt", 0),
            line(unstaged, "a.txt", 0, 0),
            line(unstaged, "a.txt", 0, 1),
        ];
        let anchor = capture_row_anchor(&rows, 4);
        // Unchanged rows: the exact line is found again.
        assert_eq!(restored_row(&rows, 0, anchor.as_ref()), 4);
        // The line is gone (e.g. partially staged): degrade to its hunk header.
        let no_line = vec![
            section(unstaged),
            file(unstaged, "a.txt"),
            hunk(unstaged, "a.txt", 0),
            line(unstaged, "a.txt", 0, 0),
        ];
        assert_eq!(restored_row(&no_line, 0, anchor.as_ref()), 2);
        // The hunk is gone too (collapsed file): land on the file row.
        let no_hunk = vec![section(unstaged), file(unstaged, "a.txt")];
        assert_eq!(restored_row(&no_hunk, 0, anchor.as_ref()), 1);
    }

    #[test]
    fn missing_anchor_falls_back_to_the_ordinal_within_its_section() {
        let (untracked, unstaged) = (SectionId::Untracked, SectionId::Unstaged);
        let rows = vec![
            section(untracked),
            file(untracked, "u.txt"),
            section(unstaged),
            file(unstaged, "a.txt"),
            file(unstaged, "b.txt"),
            file(unstaged, "c.txt"),
        ];
        // Cursor on b.txt — ordinal 2 among the section's selectable rows.
        let anchor = capture_row_anchor(&rows, 4);
        // b.txt staged away: land on the row now at that ordinal (c.txt),
        // staying inside the Unstaged section (not row 4 of the buffer).
        let after = vec![
            section(untracked),
            file(untracked, "u.txt"),
            section(unstaged),
            file(unstaged, "a.txt"),
            file(unstaged, "c.txt"),
        ];
        assert_eq!(restored_row(&after, 0, anchor.as_ref()), 4);
        // The ordinal clamps to the section's end when it shrank past it.
        let shrunk = vec![
            section(untracked),
            file(untracked, "u.txt"),
            section(unstaged),
            file(unstaged, "a.txt"),
        ];
        assert_eq!(restored_row(&shrunk, 0, anchor.as_ref()), 3);
        // The whole section is gone: clamp to the nearest selectable row.
        let gone = vec![section(untracked), file(untracked, "u.txt")];
        assert_eq!(restored_row(&gone, 5, anchor.as_ref()), 1);
    }

    #[test]
    fn commit_anchors_follow_the_hash_within_their_section() {
        let recent = SectionId::Recent;
        let rows = vec![section(recent), commit("aaa"), commit("bbb")];
        let anchor = capture_row_anchor(&rows, 2);
        // The commit moved (a new one landed above it): follow the hash.
        let after = vec![section(recent), commit("ccc"), commit("aaa"), commit("bbb")];
        assert_eq!(restored_row(&after, 0, anchor.as_ref()), 3);
    }

    #[test]
    fn anchorless_restore_clamps_to_a_selectable_row() {
        assert_eq!(restored_row(&[], 3, None), 0);
        let mut rows = vec![section(SectionId::Unstaged), file(SectionId::Unstaged, "a")];
        assert_eq!(restored_row(&rows, 9, None), 1);
        // An unselectable landing row snaps to the nearest selectable one.
        rows[1].selectable = false;
        assert_eq!(restored_row(&rows, 1, None), 0);
    }

    #[test]
    fn char_range_normalizes_regardless_of_drag_direction() {
        // Forward and backward drags over the same span select the same range.
        let forward = CharSelection::on_row(3, 2, 7);
        let backward = CharSelection::on_row(3, 7, 2);
        assert_eq!(forward.range_on(3), Some(2..7));
        assert_eq!(backward.range_on(3), Some(2..7));
        assert!(!forward.is_empty());
        assert_eq!(forward.range_on(2), None);
    }

    #[test]
    fn empty_selection_covers_nothing() {
        let sel = CharSelection::on_row(1, 5, 5);
        assert!(sel.is_empty());
        assert_eq!(sel.range_on(1), None);
    }

    #[test]
    fn slice_extracts_the_selected_text_and_clamps_bounds() {
        let sel = CharSelection::on_row(0, 6, 2);
        assert_eq!(sel.slice_on(0, "hello world"), Some("llo "));
        // A cursor past the end clamps to the string's length.
        let past_end = CharSelection::on_row(0, 0, 999);
        assert_eq!(past_end.slice_on(0, "hi"), Some("hi"));
    }

    #[test]
    fn slice_snaps_to_char_boundaries_inside_multibyte_text() {
        // "café" — the 'é' is two bytes (3..5). An offset landing mid-char snaps
        // back to a boundary rather than panicking.
        let text = "café";
        let sel = CharSelection::on_row(0, 0, 4);
        assert_eq!(sel.slice_on(0, text), Some("caf"));
    }

    #[test]
    fn cross_row_selection_covers_partial_ends_and_whole_middles() {
        // Drag from (2, 4) to (5, 3), either direction.
        for sel in [
            CharSelection {
                anchor: (2, 4),
                cursor: (5, 3),
            },
            CharSelection {
                anchor: (5, 3),
                cursor: (2, 4),
            },
        ] {
            assert_eq!(sel.rows(), 2..=5);
            assert_eq!(sel.range_on(1), None);
            assert_eq!(sel.range_on(2), Some(4..usize::MAX));
            assert_eq!(sel.range_on(3), Some(0..usize::MAX));
            assert_eq!(sel.range_on(5), Some(0..3));
            assert_eq!(sel.range_on(6), None);
            // A middle row slices whole; the endpoint rows partially.
            assert_eq!(sel.slice_on(3, "whole line"), Some("whole line"));
            assert_eq!(sel.slice_on(2, "tail selected"), Some(" selected"));
            assert_eq!(sel.slice_on(5, "head only"), Some("hea"));
        }
    }

    #[test]
    fn cross_row_drag_builds_a_char_selection_and_mirrors_the_region() {
        let (mut visual, mut char_sel, mut drag_anchor, mut char_anchor) = (None, None, None, None);
        let (mut char_click, mut selected) = (false, 0usize);
        let mut drag = DragState {
            visual: &mut visual,
            char_sel: &mut char_sel,
            drag_anchor: &mut drag_anchor,
            char_anchor: &mut char_anchor,
            char_click: &mut char_click,
            selected: &mut selected,
        };
        drag.mouse_down(1, Some(4));
        assert!(drag.mouse_move(3, Some(2)));
        assert_eq!(
            *drag.char_sel,
            Some(CharSelection {
                anchor: (1, 4),
                cursor: (3, 2),
            })
        );
        // The line-wise region mirrors the rows so act-on-region works.
        assert_eq!(*drag.visual, Some(1));
        assert_eq!(*drag.selected, 3);
        // Returning to the anchor row collapses back to a single-row range.
        assert!(drag.mouse_move(1, Some(9)));
        assert_eq!(*drag.char_sel, Some(CharSelection::on_row(1, 4, 9)));
        assert_eq!(*drag.visual, None);
        assert!(drag.mouse_up());
        assert_eq!(char_sel, Some(CharSelection::on_row(1, 4, 9)));
    }
}

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
    /// Set by a shift-click mouse-down so the following click extends the
    /// selection (and doesn't toggle the row's fold).
    pub(crate) shift_click: bool,
}

impl StatusView {
    // --- Selection & folding ---------------------------------------------

    pub(crate) fn move_selection(&mut self, delta: isize) {
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
        let page = page_rows(window) as isize;
        let amount = if full { page } else { (page / 2).max(1) };
        let delta = if down { amount } else { -amount };
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } | Screen::Diff { .. } => self.flat_diff_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            Screen::Refs(_) => self.refs_move(delta, cx),
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
            | Screen::Refs(_) => self.nav_line(
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
                        .diffs
                        .iter()
                        .filter_map(|((source, path), state)| match state {
                            DiffState::Loaded(diff) => {
                                Some((*source, path.clone(), diff.hunks.len()))
                            }
                            _ => None,
                        })
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
    pub(crate) fn try_nav(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // All motions (arrows, `C-d`, Space, `]`, the `g` prefix, …) resolve
        // through the effective keymap — there are no hardcoded aliases.
        let chord = chord(key, shift, ctrl, alt, false);
        // A prefix key begins a sequence.
        if self.is_prefix(&chord) {
            self.enter_prefix(chord, window, cx);
            return true;
        }
        // Run only if it's a motion, so a command key (e.g. `s`) isn't fired in
        // a non-status view.
        let Some(id) = self.keymap.get(&chord).cloned() else {
            return false;
        };
        if commands()
            .iter()
            .any(|c| c.id == id && c.category == Category::Navigation)
        {
            self.invoke_command(&id, window, cx);
            true
        } else {
            false
        }
    }

    pub(crate) fn toggle_fold(&mut self, cx: &mut Context<Self>) {
        // Folding changes row indices, which would invalidate a visual anchor.
        self.selection.visual = None;
        let row = self.rows.get(self.selected);
        // Use the row's own fold key, or — for a diff line — the enclosing hunk,
        // so `Tab` anywhere inside a hunk collapses/expands it (like magit).
        let key = row
            .and_then(|r| r.fold.clone())
            .or_else(|| match row.map(|r| &r.target) {
                Some(Some(Target::Line { file, hunk, .. })) => section_source(file.section)
                    .map(|src| FoldKey::Hunk(src, file.path.clone(), *hunk)),
                _ => None,
            });
        let Some(key) = key else {
            return;
        };
        // A manual toggle ends fold level 3's claim on newly loaded diffs.
        self.collapse_new_hunks = false;
        let is_section = matches!(key, FoldKey::Section(_));
        // Hunks default to expanded, so their state lives in `collapsed_hunks`
        // (present = collapsed); sections/files use `expanded` (present = open).
        if matches!(key, FoldKey::Hunk(..)) {
            if !self.collapsed_hunks.remove(&key) {
                self.collapsed_hunks.insert(key);
            }
        } else if self.expanded.contains(&key) {
            self.expanded.remove(&key);
        } else {
            self.expanded.insert(key.clone());
            if let FoldKey::File(source, path) = &key {
                self.ensure_diff(*source, path.clone(), cx);
            }
        }
        // Section fold state persists per repo (files/hunks stay ephemeral).
        if is_section {
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
    fn persist_fold_state(&self) {
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
            &state::FoldState { collapsed },
        );
    }

    pub(crate) fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
        if !self.rows[self.selected].selectable {
            let down = (self.selected..self.rows.len()).find(|&i| self.rows[i].selectable);
            let up = || (0..self.selected).rev().find(|&i| self.rows[i].selectable);
            if let Some(i) = down.or_else(up) {
                self.selected = i;
            }
        }
    }

    // --- Selection restoration across rebuilds ---------------------------
    //
    // Rather than keep the cursor at the same numeric row index (which may mean
    // something unrelated after staging/folding), we capture the selected row's
    // logical identity before a rebuild and restore it to the same place — or,
    // if that's gone, to a sensible nearby row within the same section.

    /// The logical identity of the row at `ix`.
    pub(crate) fn ident_of(&self, ix: usize) -> AnchorIdent {
        match self.rows.get(ix) {
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
            }) => match self.enclosing_section(ix) {
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

    /// The section a row belongs to, by scanning back to the nearest section
    /// header at or above it.
    pub(crate) fn enclosing_section(&self, ix: usize) -> Option<SectionId> {
        (0..=ix)
            .rev()
            .find_map(|i| match self.rows.get(i).map(|r| &r.fold) {
                Some(Some(FoldKey::Section(s))) => Some(*s),
                _ => None,
            })
    }

    /// The row indices belonging to a section: its header through the row before
    /// the next section header (or end).
    pub(crate) fn section_rows(&self, section: SectionId) -> Vec<usize> {
        let Some(start) =
            (0..self.rows.len()).find(|&i| self.rows[i].fold == Some(FoldKey::Section(section)))
        else {
            return Vec::new();
        };
        let mut out = vec![start];
        for i in (start + 1)..self.rows.len() {
            if matches!(self.rows[i].kind, RowKind::Section { .. }) {
                break;
            }
            out.push(i);
        }
        out
    }

    /// Capture the current selection for restoration after a rebuild.
    pub(crate) fn capture_anchor(&self) -> Option<SelAnchor> {
        if self.rows.is_empty() {
            return None;
        }
        let ident = self.ident_of(self.selected);
        let scope: Vec<usize> = match ident.section() {
            Some(s) => self.section_rows(s),
            None => (0..self.rows.len()).collect(),
        };
        let ordinal = scope
            .iter()
            .filter(|&&i| self.rows[i].selectable)
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        Some(SelAnchor { ident, ordinal })
    }

    /// Whether the row at `ix` matches `ident` exactly.
    pub(crate) fn row_matches(&self, ix: usize, ident: &AnchorIdent) -> bool {
        self.ident_of(ix) == *ident
    }

    /// Find the best row for `ident`: exact, else progressively less specific
    /// (a missing line falls back to its hunk header, then its file row).
    pub(crate) fn locate_ident(&self, ident: &AnchorIdent) -> Option<usize> {
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
            .find_map(|id| (0..self.rows.len()).find(|&i| self.row_matches(i, id)))
    }

    /// Restore the selection captured by [`capture_anchor`] after a rebuild.
    pub(crate) fn restore_anchor(&mut self, anchor: Option<SelAnchor>) {
        let Some(anchor) = anchor else {
            self.clamp_selection();
            return;
        };
        if let Some(ix) = self.locate_ident(&anchor.ident) {
            self.selected = ix;
            self.clamp_selection();
            return;
        }
        // The anchored row is gone (e.g. staged away). Stay within the same
        // section at roughly the same ordinal, else fall back to nearest.
        if let Some(section) = anchor.ident.section() {
            let selectable: Vec<usize> = self
                .section_rows(section)
                .into_iter()
                .filter(|&i| self.rows[i].selectable)
                .collect();
            if !selectable.is_empty() {
                let pick = anchor.ordinal.min(selectable.len() - 1);
                self.selected = selectable[pick];
                return;
            }
        }
        self.clamp_selection();
    }

    /// Rebuild rows while keeping the cursor on the same logical row.
    pub(crate) fn rebuild_preserving_selection(&mut self) {
        let anchor = self.capture_anchor();
        self.rebuild_rows();
        self.restore_anchor(anchor);
    }
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

// --- Scroll math for the read-only list views ------------------------------

/// The viewport height in rows — a "page" for the scroll/paging keys.
pub(crate) fn page_rows(window: &Window) -> usize {
    let height = window.viewport_size().height.as_f32();
    // Leave a few rows for the header/padding so paging keeps a little overlap.
    ((height / ROW_HEIGHT) as usize).saturating_sub(3).max(1)
}

/// Apply a vi-style scroll key to a `uniform_list`, updating the caller-tracked
/// top-row index (`top`) and scrolling the handle to it. We track `top`
/// ourselves because the handle's index getter is test-only. Returns whether
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

pub(crate) fn apply_scroll_key(
    handle: &UniformListScrollHandle,
    top: &mut usize,
    len: usize,
    key: &str,
    shift: bool,
    ctrl: bool,
    page: usize,
) -> bool {
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

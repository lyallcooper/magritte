//! Cursor navigation, selection, fold toggling, and selection-anchor
//! preservation for [`StatusView`]. Split out of `main.rs`; these are
//! `impl StatusView` methods over the row list and fold state.

use gpui::{Context, Window};

use crate::*;

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

    /// Move to the next/previous top-level section header.
    pub(crate) fn select_section(&mut self, forward: bool) {
        let is_section = |r: &Row| matches!(r.kind, RowKind::Section { .. });
        let next = if forward {
            (self.selected + 1..self.rows.len()).find(|&i| is_section(&self.rows[i]))
        } else {
            (0..self.selected)
                .rev()
                .find(|&i| is_section(&self.rows[i]))
        };
        if let Some(i) = next {
            self.selected = i;
        }
    }

    // --- Unified, screen-aware navigation ---------------------------------
    // One [keymap] drives motion in every cursor view: the registry's
    // Navigation commands resolve to these, dispatched to the active screen.

    /// Move the cursor/selection by `delta` rows in the active view.
    pub(crate) fn nav_line(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } => self.commit_view_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            _ => {
                self.move_selection(delta);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Page the cursor by a half- or full-screen in the active view.
    pub(crate) fn nav_page(&mut self, down: bool, full: bool, window: &mut Window, cx: &mut Context<Self>) {
        let page = page_rows(window) as isize;
        let amount = if full { page } else { (page / 2).max(1) };
        let delta = if down { amount } else { -amount };
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } => self.commit_view_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
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
            Screen::Log(_) | Screen::Commit { .. } | Screen::RebaseTodo(_) => self.nav_line(
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

    /// Move to the next/previous section. Only the status view has sections; a
    /// no-op elsewhere.
    pub(crate) fn nav_section(&mut self, forward: bool, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Status) {
            self.select_section(forward);
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // All motions (arrows, `C-d`, Space, `]`, the `g` prefix, …) resolve
        // through the effective keymap — there are no hardcoded aliases.
        let chord = chord(key, shift, ctrl, false, false);
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
        self.visual = None;
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
        config::save_fold_state(&dir.join("folds.toml"), &config::FoldState { collapsed });
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
        (0..=ix).rev().find_map(|i| match self.rows.get(i).map(|r| &r.fold) {
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

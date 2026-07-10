//! The conflict-resolution view (smerge's standalone analog): a conflicted
//! file rendered as a scrollable list of its lines with each conflict's
//! ours/base/theirs blocks tinted, a conflict cursor (`n`/`p`), and
//! per-conflict keep-ours/theirs/both/base verbs that rewrite the file on
//! disk as choices are made. `impl StatusView` like the other view slices.

use gpui::{Context, InteractiveElement, ParentElement, StatefulInteractiveElement, Window};
use magritte_core::conflict::{parse_conflicts, resolve, Conflict, Resolution, Segment};

use crate::*;

/// The Resolve screen's state: the parsed file, one choice slot per conflict,
/// and the derived row list the renderer virtualizes over.
pub(crate) struct ResolveView {
    /// Repo-relative path of the conflicted file.
    pub(crate) path: String,
    pub(crate) segments: Vec<Segment>,
    /// Per-conflict resolution, indexed by conflict order; `None` = unresolved.
    pub(crate) choices: Vec<Option<Resolution>>,
    /// The file as it was opened. When every choice is undone the rewrite
    /// emits these pristine bytes, so a full undo restores the exact original.
    pub(crate) original: Vec<u8>,
    /// The cursor row: `j`/`k`/`n`/`p` land it on a conflict's first row (and
    /// clicking a conflict moves it there). It has no highlight of its own —
    /// the accent border on the conflict it's in is the visible cursor. The
    /// conflict verbs act on that conflict.
    pub(crate) selected: usize,
    /// Conflicts in the order their choices were applied — `u` off a conflict
    /// undoes the most recent one.
    pub(crate) applied: Vec<usize>,
    /// Derived from `segments` + `choices`; rebuilt whenever a choice changes.
    pub(crate) rows: Vec<ResolveRow>,
    pub(crate) scroll: UniformListScrollHandle,
    /// Tracked top row for the viewport scroll keys (`C-d`/`C-u`/…).
    pub(crate) top: usize,
}

/// What a resolve row shows, driving its tint and text color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveRowKind {
    /// A file line outside any conflict.
    Text,
    /// `<<<<<<< <ours_label>`.
    OursMarker,
    /// A line of our side.
    Ours,
    /// `||||||| <base_label>` (diff3).
    BaseMarker,
    /// A line of the merge base (diff3).
    Base,
    /// The `=======` separator.
    Separator,
    /// A line of their side.
    Theirs,
    /// `>>>>>>> <theirs_label>`.
    TheirsMarker,
    /// A line of a resolved conflict's chosen content (markers gone).
    Resolved,
}

/// One row of the resolve list. Conflict rows carry their conflict's index so
/// the cursor accent and click-to-select know what they belong to.
pub(crate) struct ResolveRow {
    pub(crate) text: String,
    pub(crate) kind: ResolveRowKind,
    pub(crate) conflict: Option<usize>,
}

/// Split `bytes` into display lines: one `String` per line, line endings
/// stripped (the raw bytes keep them; rows only render).
fn display_lines(bytes: &[u8]) -> Vec<String> {
    bytes
        .split_inclusive(|&b| b == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            String::from_utf8_lossy(line).into_owned()
        })
        .collect()
}

/// Derive the row list from the segments and the current choices: plain text
/// lines pass through; an unresolved conflict renders its markers and tinted
/// blocks; a resolved one renders its chosen content inline (no markers).
pub(crate) fn build_resolve_rows(
    segments: &[Segment],
    choices: &[Option<Resolution>],
) -> Vec<ResolveRow> {
    let row = |text: String, kind: ResolveRowKind, conflict: Option<usize>| ResolveRow {
        text,
        kind,
        conflict,
    };
    let mut rows = Vec::new();
    let mut ix = 0;
    for segment in segments {
        match segment {
            Segment::Text(bytes) => {
                for line in display_lines(bytes) {
                    rows.push(row(line, ResolveRowKind::Text, None));
                }
            }
            Segment::Conflict(c) => {
                let choice = choices.get(ix).copied().flatten();
                match choice {
                    None => {
                        rows.push(row(
                            format!("<<<<<<< {}", c.ours_label),
                            ResolveRowKind::OursMarker,
                            Some(ix),
                        ));
                        for line in display_lines(&c.ours) {
                            rows.push(row(line, ResolveRowKind::Ours, Some(ix)));
                        }
                        if let Some(base) = &c.base {
                            rows.push(row(
                                format!("||||||| {}", c.base_label.as_deref().unwrap_or_default()),
                                ResolveRowKind::BaseMarker,
                                Some(ix),
                            ));
                            for line in display_lines(base) {
                                rows.push(row(line, ResolveRowKind::Base, Some(ix)));
                            }
                        }
                        rows.push(row(
                            "=======".to_string(),
                            ResolveRowKind::Separator,
                            Some(ix),
                        ));
                        for line in display_lines(&c.theirs) {
                            rows.push(row(line, ResolveRowKind::Theirs, Some(ix)));
                        }
                        rows.push(row(
                            format!(">>>>>>> {}", c.theirs_label),
                            ResolveRowKind::TheirsMarker,
                            Some(ix),
                        ));
                    }
                    Some(res) => {
                        let content = resolve(
                            &[Segment::Conflict(c.clone())],
                            std::slice::from_ref(&Some(res)),
                        );
                        let lines = display_lines(&content);
                        if lines.is_empty() {
                            // Keep an (empty) row so the conflict stays
                            // addressable — the cursor and undo need a place.
                            rows.push(row(String::new(), ResolveRowKind::Resolved, Some(ix)));
                        }
                        for line in lines {
                            rows.push(row(line, ResolveRowKind::Resolved, Some(ix)));
                        }
                    }
                }
                ix += 1;
            }
        }
    }
    rows
}

/// The next unresolved conflict after `from`, scanning forward and wrapping —
/// where the cursor lands after resolving one. `None` when all are resolved.
pub(crate) fn next_unresolved(choices: &[Option<Resolution>], from: usize) -> Option<usize> {
    let n = choices.len();
    (1..=n)
        .map(|d| (from + d) % n.max(1))
        .find(|&ix| choices.get(ix).is_some_and(Option::is_none))
}

/// The first row of conflict `ix` in the derived row list.
pub(crate) fn conflict_first_row(rows: &[ResolveRow], ix: usize) -> Option<usize> {
    rows.iter().position(|r| r.conflict == Some(ix))
}

/// The first row of the neighboring conflict block relative to the cursor
/// (`delta` = ±1, smerge-next/prev): the nearest conflict past `selected` in
/// that direction, skipping the rest of the block the cursor is already in.
/// Resolved conflicts count — that's how the cursor reaches one to undo it.
pub(crate) fn neighbor_conflict_row(
    rows: &[ResolveRow],
    selected: usize,
    delta: isize,
) -> Option<usize> {
    let at = rows.get(selected).and_then(|r| r.conflict);
    if delta > 0 {
        (selected + 1..rows.len())
            .find(|&ix| rows[ix].conflict.is_some() && rows[ix].conflict != at)
    } else {
        let prev = (0..selected)
            .rev()
            .find(|&ix| rows[ix].conflict.is_some() && rows[ix].conflict != at)?;
        conflict_first_row(rows, rows[prev].conflict?)
    }
}

impl StatusView {
    /// The conflicted file at point (its row, or the file a hunk/line belongs
    /// to) — the gate for the `resolve-conflicts` verb.
    pub(crate) fn point_conflicted_path(&self) -> Option<String> {
        self.path_at_point().filter(|p| self.is_conflicted(p))
    }

    fn resolve_state(&self) -> Option<&ResolveView> {
        match &self.screen {
            Screen::Resolve(rv) => Some(rv),
            _ => None,
        }
    }

    fn resolve_state_mut(&mut self) -> Option<&mut ResolveView> {
        match &mut self.screen {
            Screen::Resolve(rv) => Some(rv),
            _ => None,
        }
    }

    /// The conflict the cursor row belongs to, if any — what the keep/undo
    /// verbs act on.
    fn resolve_conflict_at_point(&self) -> Option<usize> {
        let rv = self.resolve_state()?;
        rv.rows.get(rv.selected)?.conflict
    }

    /// Whether the conflict at point has a diff3 base — the gate for the `B`
    /// keep-base verb.
    pub(crate) fn resolve_current_has_base(&self) -> bool {
        let Some(ix) = self.resolve_conflict_at_point() else {
            return false;
        };
        let Some(rv) = self.resolve_state() else {
            return false;
        };
        rv.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Conflict(c) => Some(c),
                _ => None,
            })
            .nth(ix)
            .is_some_and(|c: &Conflict| c.base.is_some())
    }

    /// Open the resolve view on the conflicted file at point, reading and
    /// parsing it off the UI thread. A file with no conflict markers shows a
    /// notice instead of the screen.
    pub(crate) fn open_resolve_conflicts(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.point_conflicted_path() else {
            self.set_status("No conflicted file at point".to_string(), true, cx);
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.clear_status(cx);
        let load_path = path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    repo.read_worktree_file(&load_path)
                        .map(|bytes| (parse_conflicts(&bytes), bytes))
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok((segments, original)) => {
                    let conflicts = segments
                        .iter()
                        .filter(|s| matches!(s, Segment::Conflict(_)))
                        .count();
                    if conflicts == 0 {
                        this.set_status(format!("No conflict markers in {path}"), true, cx);
                        return;
                    }
                    let choices = vec![None; conflicts];
                    let rows = build_resolve_rows(&segments, &choices);
                    // Open with the cursor on the first conflict.
                    let selected = conflict_first_row(&rows, 0).unwrap_or(0);
                    this.screen = Screen::Resolve(ResolveView {
                        path,
                        segments,
                        choices,
                        original,
                        selected,
                        applied: Vec::new(),
                        rows,
                        scroll: UniformListScrollHandle::new(),
                        top: 0,
                    });
                    this.pager_sel = PagerSelection::default();
                    cx.notify();
                }
                Err(e) => this.set_status(format!("resolve failed: {e}"), false, cx),
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn close_resolve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Jump the cursor to the neighboring conflict block (smerge-next/prev),
    /// resolved ones included — that's how the cursor reaches one to undo.
    pub(crate) fn resolve_move(&mut self, delta: isize, window: &Window, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        if let Some(row) = neighbor_conflict_row(&rv.rows, rv.selected, delta) {
            rv.selected = row;
            self.scroll_resolve_conflict_into_view(window);
            cx.notify();
        }
    }

    /// The pager motions in the resolve view: `j`/`k` step between conflicts
    /// (same as `n`/`p`), `g`/`G` jump to the first/last conflict, and the
    /// paging keys (`C-d`/`C-u`/`C-f`/`C-b`/space) scroll the viewport.
    pub(crate) fn resolve_cursor_key(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        match (key, shift, ctrl) {
            ("j", _, false) => return self.resolve_move(1, window, cx),
            ("k", _, false) => return self.resolve_move(-1, window, cx),
            ("g", shift, false) => {
                let Some(rv) = self.resolve_state_mut() else {
                    return;
                };
                let target = if shift {
                    rv.choices.len().saturating_sub(1)
                } else {
                    0
                };
                if let Some(row) = conflict_first_row(&rv.rows, target) {
                    rv.selected = row;
                    self.scroll_resolve_conflict_into_view(window);
                    cx.notify();
                }
            }
            _ => {
                let page = page_rows(window, self.row_h());
                let Some(rv) = self.resolve_state_mut() else {
                    return;
                };
                let len = rv.rows.len();
                apply_scroll_key(&rv.scroll, &mut rv.top, len, key, shift, ctrl, page);
                cx.notify();
            }
        }
    }

    /// Scroll the conflict at the cursor fully into view: nothing when the
    /// whole block is already visible, its end pulled up when it runs off the
    /// bottom, its start pinned to the top when it starts above the viewport
    /// or is taller than it.
    fn scroll_resolve_conflict_into_view(&mut self, window: &Window) {
        let page = page_rows(window, self.row_h());
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        let (first, last) = match rv.rows.get(rv.selected).and_then(|r| r.conflict) {
            Some(c) => {
                let first = conflict_first_row(&rv.rows, c).unwrap_or(rv.selected);
                let last = rv
                    .rows
                    .iter()
                    .rposition(|r| r.conflict == Some(c))
                    .unwrap_or(rv.selected);
                (first, last)
            }
            None => (rv.selected, rv.selected),
        };
        let top = scroll_top_index(&rv.scroll);
        if last - first >= page || first < top {
            rv.scroll.scroll_to_item(first, gpui::ScrollStrategy::Top);
        } else if last >= top + page {
            rv.scroll.scroll_to_item(last, gpui::ScrollStrategy::Bottom);
        }
    }

    /// Apply `res` to the conflict at point: record the choice, rewrite the
    /// file on disk, and advance to the next unresolved conflict. When it was
    /// the last one, offer to stage the file (magit's stage-to-resolve).
    pub(crate) fn resolve_choose(
        &mut self,
        res: Resolution,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.resolve_conflict_at_point() else {
            self.set_status("No conflict at point".to_string(), true, cx);
            return;
        };
        if res == Resolution::Base && !self.resolve_current_has_base() {
            self.set_status("This conflict has no base version".to_string(), true, cx);
            return;
        }
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        rv.choices[current] = Some(res);
        rv.applied.retain(|&ix| ix != current);
        rv.applied.push(current);
        let next = next_unresolved(&rv.choices, current);
        let all_resolved = rv.choices.iter().all(Option::is_some);
        let path = rv.path.clone();
        self.rewrite_resolved_file(cx);
        // Land the cursor on the next unresolved conflict (the rewrite just
        // shifted the rows), or keep it on this one's resolved content.
        if let Some(rv) = self.resolve_state_mut() {
            let target = next
                .or(Some(current))
                .and_then(|ix| conflict_first_row(&rv.rows, ix));
            rv.selected =
                target.unwrap_or_else(|| rv.selected.min(rv.rows.len().saturating_sub(1)));
        }
        self.scroll_resolve_conflict_into_view(window);
        if all_resolved {
            self.confirm = Some((
                format!("All conflicts resolved — stage {path}?"),
                Confirm::StageResolved(path),
            ));
        }
        cx.notify();
    }

    /// Undo a choice: the conflict at point when the cursor is on a resolved
    /// one, else the most recently applied choice — so `u` right after a keep
    /// always takes it back. Restores the markers and rewrites the file.
    pub(crate) fn resolve_undo(&mut self, window: &Window, cx: &mut Context<Self>) {
        let at_point = self.resolve_conflict_at_point();
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        let target = at_point
            .filter(|&ix| rv.choices.get(ix).copied().flatten().is_some())
            .or_else(|| rv.applied.last().copied());
        let Some(target) = target else {
            self.set_status("Nothing to undo".to_string(), true, cx);
            return;
        };
        rv.choices[target] = None;
        rv.applied.retain(|&ix| ix != target);
        self.rewrite_resolved_file(cx);
        // Put the cursor on the restored conflict's markers.
        if let Some(rv) = self.resolve_state_mut() {
            if let Some(row) = conflict_first_row(&rv.rows, target) {
                rv.selected = row;
            }
        }
        self.scroll_resolve_conflict_into_view(window);
        cx.notify();
    }

    /// Re-derive the rows and write the current resolution state to disk
    /// (atomic replace). With every choice undone, the pristine original bytes
    /// are written back. The status refresh picks up the on-disk change.
    fn rewrite_resolved_file(&mut self, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        rv.rows = build_resolve_rows(&rv.segments, &rv.choices);
        let bytes = if rv.choices.iter().all(Option::is_none) {
            rv.original.clone()
        } else {
            resolve(&rv.segments, &rv.choices)
        };
        let path = rv.path.clone();
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if let Err(e) = repo.write_worktree_file(&path, &bytes) {
            self.set_status(format!("write failed: {e}"), false, cx);
            return;
        }
        self.refresh(cx);
    }

    /// Open the file in the external editor at the cursor's line. The row list
    /// mirrors the file as written to disk line-for-line, so the cursor row
    /// index is the file line.
    pub(crate) fn resolve_open_editor(&mut self, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state() else {
            return;
        };
        let line = rv.selected as u32 + 1;
        let path = rv.path.clone();
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let full = repo.workdir().join(&path);
        self.launch_editor(&full, Some(line));
        self.set_status(format!("Opening {path}"), true, cx);
    }

    /// Render the resolve view: a header with the remaining-conflict count, the
    /// virtualized line list, and a key-hint footer.
    pub(crate) fn render_resolve(&self, rv: &ResolveView, view: &Entity<Self>) -> gpui::Div {
        let count = rv.rows.len();
        let unresolved = rv.choices.iter().filter(|c| c.is_none()).count();
        let body = uniform_list("resolve-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.resolve_state() {
                    Some(rv) => {
                        let at_point = rv.rows.get(rv.selected).and_then(|r| r.conflict);
                        range
                            .filter_map(|ix| rv.rows.get(ix).map(|r| (ix, r)))
                            .map(|(ix, row)| this.render_resolve_row(ix, row, at_point, &view))
                            .collect::<Vec<_>>()
                    }
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&rv.scroll)
        .flex_grow(1.0);

        let counter = if unresolved > 0 {
            format!("{unresolved} of {} unresolved", rv.choices.len())
        } else {
            "all resolved".to_string()
        };
        let left = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.palette.section)
                    .child(SharedString::from(format!("Resolve: {}", rv.path))),
            )
            .child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(counter)),
            );

        self.screen_scaffold()
            .child(self.view_header(left, "close", view))
            .child(body)
            .child(self.hint_footer(
                view,
                &[
                    ("resolve-ours", "ours"),
                    ("resolve-theirs", "theirs"),
                    ("resolve-both", "both"),
                    ("resolve-base", "base"),
                    ("resolve-undo", "undo"),
                    ("resolve-next", "next"),
                    ("resolve-prev", "previous"),
                    ("resolve-open-editor", "open in editor"),
                ],
            ))
    }

    /// One resolve row: the line, tinted by its region (ours like added lines,
    /// theirs like removed, base dim on a subtle wash). The conflict the
    /// cursor is in gets a left accent border on every row — that accent *is*
    /// the cursor; rows have no highlight of their own. Text drag-selects
    /// (the shared pager selection); a plain click on a conflict makes it
    /// current.
    fn render_resolve_row(
        &self,
        ix: usize,
        row: &ResolveRow,
        at_point: Option<usize>,
        view: &Entity<Self>,
    ) -> AnyElement {
        let in_current = row.conflict.is_some() && row.conflict == at_point;
        let (bg, fg) = match row.kind {
            ResolveRowKind::Text => (None, self.palette.dim),
            ResolveRowKind::Ours => (Some(self.palette.added_bg), self.palette.fg),
            ResolveRowKind::Base => (Some(self.palette.banner), self.palette.dim),
            ResolveRowKind::Theirs => (Some(self.palette.removed_bg), self.palette.fg),
            ResolveRowKind::Resolved => (None, self.palette.fg),
            ResolveRowKind::OursMarker
            | ResolveRowKind::BaseMarker
            | ResolveRowKind::Separator
            | ResolveRowKind::TheirsMarker => (None, self.palette.dim),
        };
        let sel = self.pager_sel.char_sel.and_then(|c| c.range_on(ix));
        let (styled, layout) = self.selectable_text(row.text.clone(), Vec::new(), sel);
        let mut el = div()
            .id(("resolve-row", ix))
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .text_color(fg)
            // A fixed-width accent slot, colored on the conflict the cursor is
            // in, so moving between conflicts doesn't shift the text.
            .border_l_2()
            .border_color(if in_current {
                self.palette.section
            } else {
                gpui::transparent_black()
            });
        if let Some(bg) = bg {
            el = el.bg(bg);
        }
        let conflict = row.conflict;
        let v_click = view.clone();
        // Registered before pager_selectable's click handler, which clears
        // `char_click` — this one must still see it to know the click only
        // dismissed a selection.
        el = el.on_click(move |ev: &gpui::ClickEvent, _window, cx: &mut App| {
            // A drag already selected text; only a stationary click on a
            // conflict moves the cursor there.
            if let gpui::ClickEvent::Mouse(e) = ev {
                if (e.up.position.x - e.down.position.x).abs() > px(4.0)
                    || (e.up.position.y - e.down.position.y).abs() > px(4.0)
                {
                    return;
                }
            }
            let Some(conflict) = conflict else { return };
            v_click.update(cx, |this, vcx| {
                if this.pager_sel.char_click {
                    return;
                }
                if let Some(rv) = this.resolve_state_mut() {
                    if let Some(first) = conflict_first_row(&rv.rows, conflict) {
                        rv.selected = first;
                        vcx.notify();
                    }
                }
            });
        });
        let el = self.pager_selectable(el, ix, layout, view);
        el.child(styled).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(ours: &str, base: Option<&str>, theirs: &str) -> Segment {
        Segment::Conflict(Conflict {
            ours: ours.as_bytes().to_vec(),
            base: base.map(|b| b.as_bytes().to_vec()),
            theirs: theirs.as_bytes().to_vec(),
            ours_label: "HEAD".to_string(),
            theirs_label: "other".to_string(),
            base_label: base.map(|_| "base".to_string()),
            raw: format!(
                "<<<<<<< HEAD\n{ours}{}=======\n{theirs}>>>>>>> other\n",
                base.map(|b| format!("||||||| base\n{b}"))
                    .unwrap_or_default()
            )
            .into_bytes(),
        })
    }

    fn text(s: &str) -> Segment {
        Segment::Text(s.as_bytes().to_vec())
    }

    fn kinds(rows: &[ResolveRow]) -> Vec<ResolveRowKind> {
        rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn rows_for_an_unresolved_conflict_show_markers_and_blocks() {
        use ResolveRowKind::*;
        let segments = vec![text("a\n"), conflict("o\n", None, "t\n"), text("z\n")];
        let rows = build_resolve_rows(&segments, &[None]);
        assert_eq!(
            kinds(&rows),
            vec![
                Text,
                OursMarker,
                Ours,
                Separator,
                Theirs,
                TheirsMarker,
                Text
            ]
        );
        assert_eq!(rows[1].text, "<<<<<<< HEAD");
        assert_eq!(rows[5].text, ">>>>>>> other");
        // Conflict rows carry their index; plain lines carry none.
        assert_eq!(rows[0].conflict, None);
        assert!(rows[1..6].iter().all(|r| r.conflict == Some(0)));
    }

    #[test]
    fn rows_for_a_diff3_conflict_include_the_base_block() {
        use ResolveRowKind::*;
        let segments = vec![conflict("o\n", Some("b\n"), "t\n")];
        let rows = build_resolve_rows(&segments, &[None]);
        assert_eq!(
            kinds(&rows),
            vec![
                OursMarker,
                Ours,
                BaseMarker,
                Base,
                Separator,
                Theirs,
                TheirsMarker
            ]
        );
        assert_eq!(rows[2].text, "||||||| base");
    }

    #[test]
    fn rows_for_a_resolved_conflict_inline_the_chosen_content() {
        use ResolveRowKind::*;
        let segments = vec![text("a\n"), conflict("o\n", None, "t\n"), text("z\n")];
        let rows = build_resolve_rows(&segments, &[Some(Resolution::Both)]);
        assert_eq!(kinds(&rows), vec![Text, Resolved, Resolved, Text]);
        assert_eq!(rows[1].text, "o");
        assert_eq!(rows[2].text, "t");
        assert_eq!(rows[1].conflict, Some(0));
        // Resolving to empty content keeps one addressable placeholder row.
        let empty = build_resolve_rows(&[conflict("", None, "t\n")], &[Some(Resolution::Ours)]);
        assert_eq!(kinds(&empty), vec![Resolved]);
        assert_eq!(empty[0].text, "");
    }

    #[test]
    fn next_unresolved_scans_forward_and_wraps() {
        let choices = vec![None, Some(Resolution::Ours), None];
        assert_eq!(next_unresolved(&choices, 0), Some(2));
        // Wraps past the end back to the first unresolved.
        assert_eq!(next_unresolved(&choices, 2), Some(0));
        let done = vec![Some(Resolution::Ours), Some(Resolution::Theirs)];
        assert_eq!(next_unresolved(&done, 0), None);
        assert_eq!(next_unresolved(&[], 0), None);
    }

    #[test]
    fn neighbor_conflict_navigation_visits_resolved_blocks_too() {
        let segments = vec![
            text("a\n"),
            conflict("o\n", None, "t\n"),
            text("mid\n"),
            conflict("x\n", None, "y\n"),
            text("z\n"),
        ];
        // First conflict resolved: rows are Text, Resolved, Text, markers…, Text.
        let rows = build_resolve_rows(&segments, &[Some(Resolution::Ours), None]);
        // From the top text row, `n` reaches the resolved block, `n` again the
        // unresolved one; `p` from there returns to the resolved block's start.
        let first = neighbor_conflict_row(&rows, 0, 1).unwrap();
        assert_eq!(rows[first].conflict, Some(0));
        assert_eq!(rows[first].kind, ResolveRowKind::Resolved);
        let second = neighbor_conflict_row(&rows, first, 1).unwrap();
        assert_eq!(rows[second].conflict, Some(1));
        assert_eq!(neighbor_conflict_row(&rows, second, -1), Some(first));
        // Mid-block: `n` skips the rest of the current conflict.
        assert_eq!(neighbor_conflict_row(&rows, second + 1, 1), None);
        assert_eq!(conflict_first_row(&rows, 1), Some(second));
    }
}

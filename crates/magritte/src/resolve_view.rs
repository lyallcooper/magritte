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
    /// The conflict the cursor is on.
    pub(crate) current: usize,
    /// Derived from `segments` + `choices`; rebuilt whenever a choice changes.
    pub(crate) rows: Vec<ResolveRow>,
    pub(crate) scroll: UniformListScrollHandle,
    /// Tracked top row for the pager scroll keys.
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

impl ResolveRowKind {
    fn is_marker(self) -> bool {
        matches!(
            self,
            ResolveRowKind::OursMarker
                | ResolveRowKind::BaseMarker
                | ResolveRowKind::Separator
                | ResolveRowKind::TheirsMarker
        )
    }
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

/// The 1-based line number (in the file as currently written to disk) where
/// conflict `ix` starts: its `<<<<<<<` marker while unresolved, or the first
/// line of its chosen content once resolved.
pub(crate) fn conflict_first_line(
    segments: &[Segment],
    choices: &[Option<Resolution>],
    ix: usize,
) -> u32 {
    let mut newlines = 0u32;
    let mut seen = 0;
    for segment in segments {
        match segment {
            Segment::Text(bytes) => {
                newlines += bytes.iter().filter(|&&b| b == b'\n').count() as u32;
            }
            Segment::Conflict(_) => {
                if seen == ix {
                    return newlines + 1;
                }
                let emitted = resolve(
                    std::slice::from_ref(segment),
                    std::slice::from_ref(choices.get(seen).unwrap_or(&None)),
                );
                newlines += emitted.iter().filter(|&&b| b == b'\n').count() as u32;
                seen += 1;
            }
        }
    }
    newlines + 1
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

    /// Whether the conflict at the resolve cursor has a diff3 base — the gate
    /// for the `B` keep-base verb.
    pub(crate) fn resolve_current_has_base(&self) -> bool {
        let Some(rv) = self.resolve_state() else {
            return false;
        };
        rv.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Conflict(c) => Some(c),
                _ => None,
            })
            .nth(rv.current)
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
                    this.screen = Screen::Resolve(ResolveView {
                        path,
                        segments,
                        choices,
                        original,
                        current: 0,
                        rows,
                        scroll: UniformListScrollHandle::new(),
                        top: 0,
                    });
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

    /// Move the conflict cursor by `delta` conflicts (smerge-next/prev),
    /// clamped, scrolling it into view.
    pub(crate) fn resolve_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        if rv.choices.is_empty() {
            return;
        }
        let last = rv.choices.len() as isize - 1;
        rv.current = (rv.current as isize + delta).clamp(0, last) as usize;
        self.scroll_resolve_cursor_into_view();
        cx.notify();
    }

    fn scroll_resolve_cursor_into_view(&mut self) {
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        if let Some(ix) = rv.rows.iter().position(|r| r.conflict == Some(rv.current)) {
            rv.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Top);
        }
    }

    /// Apply `res` to the conflict at the cursor: record the choice, rewrite
    /// the file on disk, and advance to the next unresolved conflict. When it
    /// was the last one, offer to stage the file (magit's stage-to-resolve).
    pub(crate) fn resolve_choose(&mut self, res: Resolution, cx: &mut Context<Self>) {
        if res == Resolution::Base && !self.resolve_current_has_base() {
            self.set_status("This conflict has no base version".to_string(), true, cx);
            return;
        }
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        if rv.choices.is_empty() {
            return;
        }
        let current = rv.current;
        rv.choices[current] = Some(res);
        if let Some(next) = next_unresolved(&rv.choices, current) {
            rv.current = next;
        }
        let all_resolved = rv.choices.iter().all(Option::is_some);
        let path = rv.path.clone();
        self.rewrite_resolved_file(cx);
        self.scroll_resolve_cursor_into_view();
        if all_resolved {
            self.confirm = Some((
                format!("All conflicts resolved — stage {path}?"),
                Confirm::StageResolved(path),
            ));
        }
        cx.notify();
    }

    /// Undo the choice at the cursor: restore that conflict's markers and
    /// rewrite the file.
    pub(crate) fn resolve_undo(&mut self, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state_mut() else {
            return;
        };
        let current = rv.current;
        if rv.choices.get(current).copied().flatten().is_none() {
            self.set_status("Conflict is not resolved".to_string(), true, cx);
            return;
        }
        rv.choices[current] = None;
        self.rewrite_resolved_file(cx);
        self.scroll_resolve_cursor_into_view();
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

    /// Open the file in the external editor at the current conflict's first
    /// line (as the file sits on disk right now).
    pub(crate) fn resolve_open_editor(&mut self, cx: &mut Context<Self>) {
        let Some(rv) = self.resolve_state() else {
            return;
        };
        let line = conflict_first_line(&rv.segments, &rv.choices, rv.current);
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
                    Some(rv) => range
                        .filter_map(|ix| rv.rows.get(ix).map(|r| (ix, r)))
                        .map(|(ix, row)| this.render_resolve_row(ix, row, rv.current, &view))
                        .collect::<Vec<_>>(),
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
            .child(
                div()
                    .text_size(px(self.font_px() - 1.0))
                    .text_color(self.palette.dim)
                    .child(SharedString::from(
                        "o ours · t theirs · b both · B base · u undo · n/p conflict · \
                         j/k scroll · ⏎ open in editor",
                    )),
            )
    }

    /// One resolve row: the line, tinted by its region (ours like added lines,
    /// theirs like removed, base dim on a subtle wash), with the current
    /// conflict marked by a left accent border and a marker-row highlight.
    fn render_resolve_row(
        &self,
        ix: usize,
        row: &ResolveRow,
        current: usize,
        view: &Entity<Self>,
    ) -> AnyElement {
        let is_current = row.conflict == Some(current);
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
        // The cursor's marker rows wear the selection wash so the current
        // conflict reads at a glance; every row of it gets the accent border.
        let bg = if is_current && row.kind.is_marker() {
            Some(self.palette.selection)
        } else {
            bg
        };
        let mut el = div()
            .id(("resolve-row", ix))
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .text_color(fg);
        if row.conflict.is_some() {
            // A fixed-width accent slot, colored only on the current conflict,
            // so switching conflicts doesn't shift the text.
            el = el.border_l_2().border_color(if is_current {
                self.palette.section
            } else {
                gpui::transparent_black()
            });
        }
        if let Some(bg) = bg {
            el = el.bg(bg);
        }
        if let Some(conflict) = row.conflict {
            let view = view.clone();
            el = el
                .cursor_pointer()
                .on_click(move |_, _window, cx: &mut App| {
                    view.update(cx, |this, vcx| {
                        if let Some(rv) = this.resolve_state_mut() {
                            rv.current = conflict;
                            vcx.notify();
                        }
                    });
                });
        }
        el.child(SharedString::from(row.text.clone()))
            .into_any_element()
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
    fn conflict_first_line_counts_the_file_as_written() {
        let segments = vec![
            text("a\nb\n"),
            conflict("o\n", None, "t\n"),
            text("mid\n"),
            conflict("x\n", None, "y\n"),
        ];
        // Unresolved: the first conflict's `<<<<<<<` marker sits on line 3.
        assert_eq!(conflict_first_line(&segments, &[None, None], 0), 3);
        // The second starts after the first's 5-line marker block plus "mid".
        assert_eq!(conflict_first_line(&segments, &[None, None], 1), 9);
        // Resolving the first (1 content line, markers gone) pulls it up.
        let choices = [Some(Resolution::Ours), None];
        assert_eq!(conflict_first_line(&segments, &choices, 0), 3);
        assert_eq!(conflict_first_line(&segments, &choices, 1), 5);
    }
}

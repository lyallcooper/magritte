//! The read-only diff screens: a commit's detail (header + message + diff,
//! opened from the log or a status commit row, with an LRU cache of loaded
//! commits) and the standalone diff buffer (the `d` diff transient's output).
//! Both render the same flattened [`CommitDiffRow`] list; their shared body —
//! rows, scroll, cursor, visual selection — is the [`FlatDiff`] each screen
//! embeds. `impl StatusView` like the other view slices.

use gpui::{Context, SharedString, UniformListScrollHandle, Window};
use magritte_core::{ApplyTarget, CommitMetadata, FileDiff, LineKind};

use crate::*;

/// The shared body of a read-only flattened diff screen: its rows, scroll
/// handle, cursor row (drives scrolling), and visual-selection anchor — so
/// lines can be selected and yanked in these views too.
pub(crate) struct FlatDiff {
    pub(crate) rows: Vec<CommitDiffRow>,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) selected: usize,
    pub(crate) visual: Option<usize>,
    /// Row indices of collapsed File/Hunk headers — their contents are hidden.
    /// Indices into `rows` (the full model), so the apply engine is unaffected.
    pub(crate) collapsed: std::collections::HashSet<usize>,
    /// The active character-range selection within one row (a plain drag that
    /// stayed on its anchor row). Mutually exclusive with a spanning [`visual`]:
    /// a drag off the anchor row clears this and sets `visual`.
    ///
    /// [`visual`]: Self::visual
    pub(crate) char_sel: Option<CharSelection>,
    /// Row a left-drag began on, while the button is held (`None` otherwise), so
    /// a move can tell whether the drag has left its anchor row.
    pub(crate) drag_anchor: Option<usize>,
    /// Byte offset the drag anchored at within the anchor row (only on a text
    /// row). Cleared once the drag leaves the anchor row, so a return to it
    /// collapses the line region rather than re-entering char selection.
    pub(crate) char_anchor: Option<usize>,
    /// Set by a mouse-down on a row that had an active char selection: the
    /// following click only clears the selection (see [`Selection::char_click`]).
    pub(crate) char_click: bool,
}

impl FlatDiff {
    pub(crate) fn loading() -> Self {
        FlatDiff {
            rows: vec![CommitDiffRow::Note("Loading…".to_string())],
            scroll: UniformListScrollHandle::new(),
            selected: 0,
            visual: None,
            collapsed: std::collections::HashSet::new(),
            char_sel: None,
            drag_anchor: None,
            char_anchor: None,
            char_click: false,
        }
    }

    /// The full-row indices currently visible: everything except lines under a
    /// collapsed hunk and hunks/lines under a collapsed file.
    pub(crate) fn visible_rows(&self) -> Vec<usize> {
        visible_diff_rows(&self.rows, &self.collapsed)
    }

    /// The fold header governing `ix`: the row itself if it's a File/Hunk header,
    /// else the enclosing hunk header for a line. `None` for anything unfoldable.
    fn fold_header_for(&self, ix: usize) -> Option<usize> {
        fold_header_for(&self.rows, ix)
    }

    /// Toggle the fold of the header at (or enclosing) `ix`. Returns whether a
    /// header was toggled. Keeps the cursor visible by snapping it to the header.
    pub(crate) fn toggle_fold(&mut self, ix: usize) -> bool {
        let Some(header) = self.fold_header_for(ix) else {
            return false;
        };
        if !self.collapsed.remove(&header) {
            self.collapsed.insert(header);
        }
        if !self.visible_rows().contains(&self.selected) {
            self.selected = header;
        }
        true
    }

    /// Move the cursor by `delta` visible rows, keeping it in view.
    fn move_by(&mut self, delta: isize) {
        // Keyboard motion drops a mouse char selection (it belongs to the cursor
        // row it was dragged on, not wherever the cursor moves next).
        self.char_sel = None;
        let vis = self.visible_rows();
        if vis.is_empty() {
            return;
        }
        let cur = vis.iter().position(|&i| i == self.selected).unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, vis.len() as isize - 1) as usize;
        self.selected = vis[next];
        self.scroll.scroll_to_item(next, gpui::ScrollStrategy::Top);
    }

    /// Toggle a visual selection anchored at the cursor.
    fn toggle_visual(&mut self) {
        self.char_sel = None;
        self.visual = if self.visual.is_some() {
            None
        } else {
            Some(self.selected)
        };
    }

    /// The visually-selected rows (or the line at point) as text.
    fn selection_text(&self) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        let (lo, hi) = match self.visual {
            Some(a) => (a.min(self.selected), a.max(self.selected)),
            None => (self.selected, self.selected),
        };
        let hi = hi.min(self.rows.len() - 1);
        self.rows[lo..=hi]
            .iter()
            .map(commit_row_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the apply engine acts on. Indices are into the active view's
/// `files` (and each file's `hunks`); `Lines` carries hunk-line indices.
enum ApplyScope {
    File(usize),
    Hunk(usize, usize),
    Lines(usize, usize, Vec<usize>),
}

/// If the row range `lo..=hi` covers only changed line rows within a single
/// hunk (no file/hunk header selected), the region scope — `(file, hunk,
/// hunk-line indices)`. Returns `None` when a header is in range or the
/// selection spans hunks, so the caller falls back to the file/hunk at point.
fn line_region_scope(rows: &[CommitDiffRow], lo: usize, hi: usize) -> Option<ApplyScope> {
    let mut file_ix: Option<usize> = None;
    let mut hunk_ix: Option<usize> = None;
    let mut line_ix = 0usize;
    let mut picked: Vec<(usize, usize, usize)> = Vec::new();
    for (ix, row) in rows.iter().enumerate() {
        let selected = (lo..=hi).contains(&ix);
        match row {
            CommitDiffRow::File { .. } => {
                if selected {
                    return None;
                }
                file_ix = Some(file_ix.map_or(0, |f| f + 1));
                hunk_ix = None;
            }
            CommitDiffRow::Hunk(_) => {
                if selected {
                    return None;
                }
                hunk_ix = Some(hunk_ix.map_or(0, |h| h + 1));
                line_ix = 0;
            }
            CommitDiffRow::Line { kind, .. } => {
                // Only changed lines are patch content; context lines in the
                // selection are ignored (they stay context either way).
                if selected && matches!(kind, LineKind::Added | LineKind::Removed) {
                    picked.push((file_ix?, hunk_ix?, line_ix));
                }
                line_ix += 1;
            }
            _ => {}
        }
    }
    let (f0, h0, _) = *picked.first()?;
    picked
        .iter()
        .all(|(f, h, _)| *f == f0 && *h == h0)
        .then(|| ApplyScope::Lines(f0, h0, picked.iter().map(|(_, _, l)| *l).collect()))
}

/// A single commit's detail (opened from the log): its header and diff, as the
/// same flattened rows the commit editor renders.
pub(crate) struct CommitView {
    /// The commit's full hash — passed to `diff_commit` and copied by the
    /// header's copy button.
    pub(crate) rev: String,
    /// The abbreviated hash, shown in the header next to the copy button.
    pub(crate) short: SharedString,
    pub(crate) details: Vec<String>,
    pub(crate) show_details: bool,
    pub(crate) body: FlatDiff,
    /// The commit's structured per-file diffs (same order as the rendered file
    /// sections), so the apply engine (`a`/`v`/`u`) can rebuild a patch for the
    /// file/hunk at point. Empty until the diff loads.
    pub(crate) files: Vec<FileDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CommitCacheKey {
    pub(crate) rev: String,
    pub(crate) args: Vec<String>,
    pub(crate) paths: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct CommitCacheEntry {
    pub(crate) metadata: CommitMetadata,
    pub(crate) message: String,
    pub(crate) files: Vec<(FileDiff, Option<&'static str>)>,
}

pub(crate) const COMMIT_CACHE_CAPACITY: usize = 64;

/// An LRU of immutable commit-detail loads. Bundles the entry map with its
/// insertion-order queue so the eviction invariant (the queue mirrors the map's
/// live keys, oldest first, bounded by [`COMMIT_CACHE_CAPACITY`]) is enforced in
/// one place rather than across two raw fields kept in sync by hand.
#[derive(Default)]
pub(crate) struct CommitCache {
    entries: std::collections::HashMap<CommitCacheKey, CommitCacheEntry>,
    order: std::collections::VecDeque<CommitCacheKey>,
}

impl CommitCache {
    pub(crate) fn get(&self, key: &CommitCacheKey) -> Option<&CommitCacheEntry> {
        self.entries.get(key)
    }

    pub(crate) fn insert(&mut self, key: CommitCacheKey, entry: CommitCacheEntry) {
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, entry);
        while self.order.len() > COMMIT_CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

/// A standalone diff buffer (`d` / Magit's `magit-diff`): a title plus a
/// flattened, read-only list of file/hunk/line rows.
pub(crate) struct DiffView {
    pub(crate) title: SharedString,
    pub(crate) body: FlatDiff,
    /// Structured per-file diffs (rendered order), for the apply engine
    /// (`a`/`v`/`u`) — same role as [`CommitView::files`].
    pub(crate) files: Vec<FileDiff>,
}

#[derive(Clone)]
pub(crate) enum DiffRequest {
    Unstaged {
        args: Vec<String>,
        paths: Vec<String>,
    },
    Staged {
        args: Vec<String>,
        paths: Vec<String>,
    },
    Worktree {
        rev: String,
        args: Vec<String>,
        paths: Vec<String>,
    },
    Range {
        range: String,
        args: Vec<String>,
        paths: Vec<String>,
    },
}

impl DiffRequest {
    pub(crate) fn title(&self) -> String {
        match self {
            DiffRequest::Unstaged { paths, .. } => diff_title("Unstaged changes", paths),
            DiffRequest::Staged { paths, .. } => diff_title("Staged changes", paths),
            DiffRequest::Worktree { rev, paths, .. } => {
                diff_title(&format!("Working tree vs {rev}"), paths)
            }
            DiffRequest::Range { range, paths, .. } => diff_title(range, paths),
        }
    }
}

impl StatusView {
    /// Open the commit selected in the log (Enter in the log view).
    pub(crate) fn open_commit_view(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.log().and_then(|l| l.entries.get(l.selected).cloned()) else {
            return;
        };
        self.open_commit(entry.hash, entry.short_hash, entry.subject, cx);
    }

    pub(crate) fn open_commit_with_args(
        &mut self,
        hash: String,
        short: String,
        subject: String,
        args: Vec<String>,
        paths: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.open_commit_inner(hash, short, subject, args, paths, cx);
    }

    /// Open a commit's diff detail, overlaying the current screen (restored on
    /// close). Shared by the log view and status commit rows.
    pub(crate) fn open_commit(
        &mut self,
        hash: String,
        short: String,
        subject: String,
        cx: &mut Context<Self>,
    ) {
        self.open_commit_inner(hash, short, subject, Vec::new(), Vec::new(), cx);
    }

    pub(crate) fn open_commit_inner(
        &mut self,
        hash: String,
        short: String,
        subject: String,
        args: Vec<String>,
        paths: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let key = CommitCacheKey {
            rev: hash.clone(),
            args: args.clone(),
            paths: paths.clone(),
        };
        let gen = self.next_screen_gen();
        // Carry the screen we came from so closing returns there (log or status).
        let back = Box::new(std::mem::take(&mut self.screen));
        let rev = hash.clone();
        // Seed the body with the subject (summary) so it's visible immediately —
        // the async load replaces it with the full message once it lands.
        let body = {
            let mut rows = Vec::new();
            if !subject.is_empty() {
                rows.push(CommitDiffRow::Message(subject));
                rows.push(CommitDiffRow::Note(String::new()));
            }
            rows.push(CommitDiffRow::Note("Loading…".to_string()));
            FlatDiff {
                rows,
                ..FlatDiff::loading()
            }
        };
        self.screen = Screen::Commit {
            view: CommitView {
                rev: rev.clone(),
                short: SharedString::from(short),
                details: Vec::new(),
                show_details: false,
                body,
                files: Vec::new(),
            },
            back,
        };
        if let Some(cached) = self.commit_cache.get(&key).cloned() {
            self.populate_commit_view(&cached, cx);
            cx.notify();
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let metadata = repo.commit_metadata(&rev)?;
                    let message = repo.commit_message(&rev)?;
                    let files = repo.diff_commit_with(&rev, &args, &paths).map(|diffs| {
                        diffs
                            .into_iter()
                            .map(|d| {
                                let (head, tail) =
                                    file_head_tail(&repo.workdir().join(d.display_path()));
                                let lang =
                                    highlight::detect_language(d.display_path(), &head, &tail);
                                (d, lang)
                            })
                            .collect::<Vec<_>>()
                    })?;
                    Ok::<_, magritte_core::Error>(CommitCacheEntry {
                        metadata,
                        message,
                        files,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                // Bail if a newer screen load superseded this one, or the view
                // was closed before the diff arrived.
                if !this.screen_gen.is_current(gen) || this.commit_view().is_none() {
                    return;
                }
                let loaded = match loaded {
                    Ok(loaded) => loaded,
                    Err(e) => {
                        if let Some(cv) = this.commit_view_mut() {
                            cv.body.rows =
                                vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))];
                        }
                        cx.notify();
                        return;
                    }
                };
                this.commit_cache.insert(key, loaded.clone());
                this.populate_commit_view(&loaded, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn populate_commit_view(
        &mut self,
        entry: &CommitCacheEntry,
        cx: &mut Context<Self>,
    ) {
        let details = commit_metadata_lines(&entry.metadata);
        let show_details = self.commit_view().is_some_and(|cv| cv.show_details);
        let mut rows = self.commit_detail_rows(&entry.message, &entry.files, cx);
        if show_details {
            prepend_commit_details(&mut rows, &details);
        }
        let files: Vec<FileDiff> = entry.files.iter().map(|(f, _)| f.clone()).collect();
        if let Some(cv) = self.commit_view_mut() {
            cv.details = details;
            cv.body.rows = rows;
            cv.body.collapsed.clear();
            cv.files = files;
        }
    }

    pub(crate) fn open_diff(&mut self, request: DiffRequest, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        let title = request.title();
        let back = Box::new(std::mem::take(&mut self.screen));
        self.screen = Screen::Diff {
            view: DiffView {
                title: SharedString::from(title.clone()),
                body: FlatDiff::loading(),
                files: Vec::new(),
            },
            back,
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let diffs = match request {
                        DiffRequest::Unstaged { args, paths } => repo.diff_unstaged(&args, &paths),
                        DiffRequest::Staged { args, paths } => repo.diff_staged(&args, &paths),
                        DiffRequest::Worktree { rev, args, paths } => {
                            repo.diff_worktree(&rev, &args, &paths)
                        }
                        DiffRequest::Range { range, args, paths } => {
                            repo.diff_range(&range, &args, &paths)
                        }
                    }?;
                    Ok::<_, magritte_core::Error>(
                        diffs
                            .into_iter()
                            .map(|d| {
                                let (head, tail) =
                                    file_head_tail(&repo.workdir().join(d.display_path()));
                                let lang =
                                    highlight::detect_language(d.display_path(), &head, &tail);
                                (d, lang)
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) || this.diff_view().is_none() {
                    return;
                }
                let files: Vec<FileDiff> = match &loaded {
                    Ok(fs) => fs.iter().map(|(f, _)| f.clone()).collect(),
                    Err(_) => Vec::new(),
                };
                let rows = match loaded {
                    Ok(files) if files.is_empty() => {
                        vec![CommitDiffRow::Note("No changes".to_string())]
                    }
                    Ok(files) => {
                        // Lead with the diffstat overview, like the commit view.
                        let mut rows = Vec::new();
                        let stat = diffstat_block(&files);
                        if !stat.is_empty() {
                            rows.extend(stat);
                            rows.push(CommitDiffRow::Note(String::new()));
                        }
                        rows.extend(this.diff_rows(&files, cx));
                        rows
                    }
                    Err(e) => vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))],
                };
                if let Some(dv) = this.diff_view_mut() {
                    dv.body.rows = rows;
                    dv.body.collapsed.clear();
                    dv.files = files;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn commit_detail_rows(
        &self,
        message: &str,
        files: &[(FileDiff, Option<&'static str>)],
        cx: &mut Context<Self>,
    ) -> Vec<CommitDiffRow> {
        let mut rows = Vec::new();
        let mut lines = message.lines();
        // The subject (summary) as the first selectable line, so it can be
        // selected/copied like the rest of the message (it's the header title
        // too, but that's chrome — the buffer text is what you select).
        if let Some(subject) = lines.next() {
            rows.push(CommitDiffRow::Message(subject.to_string()));
        }
        // The body, after an optional blank line separating it from the subject.
        let mut body = lines.peekable();
        if matches!(body.peek(), Some(&"")) {
            body.next();
        }
        if body.peek().is_some() {
            rows.push(CommitDiffRow::Note(String::new()));
            for line in body {
                rows.push(CommitDiffRow::Message(line.to_string()));
            }
        }
        if !rows.is_empty() {
            rows.push(CommitDiffRow::Note(String::new()));
        }
        // The diffstat block above the files (magit's overview).
        let stat = diffstat_block(files);
        if !stat.is_empty() {
            rows.extend(stat);
            rows.push(CommitDiffRow::Note(String::new()));
        }
        rows.extend(self.diff_rows(files, cx));
        rows
    }

    /// The structured per-file diffs of the active flattened-diff screen (commit
    /// or standalone diff), in rendered order — the apply engine's patch source.
    fn active_diff_files(&self) -> Option<&[FileDiff]> {
        match &self.screen {
            Screen::Commit { view, .. } => Some(&view.files),
            Screen::Diff { view, .. } => Some(&view.files),
            _ => None,
        }
    }

    /// The (file, hunk) the active flattened-diff cursor is on, for the apply
    /// engine. `hunk` is `None` when the cursor sits on a file header (act on
    /// the whole file) or above the diff (no target → `None`). Indices match
    /// `active_diff_files` and each file's `hunks`, since the rows are built in
    /// that order.
    fn flat_diff_apply_target(&self) -> Option<(usize, Option<usize>)> {
        let fd = self.flat_diff()?;
        let cursor = fd.selected;
        let mut file_ix: Option<usize> = None;
        let mut hunk_ix: Option<usize> = None;
        for (ix, row) in fd.rows.iter().enumerate() {
            match row {
                CommitDiffRow::File { .. } => {
                    file_ix = Some(file_ix.map_or(0, |f| f + 1));
                    hunk_ix = None;
                }
                CommitDiffRow::Hunk(_) => {
                    hunk_ix = Some(hunk_ix.map_or(0, |h| h + 1));
                }
                _ => {}
            }
            if ix == cursor {
                break;
            }
        }
        file_ix.map(|f| (f, hunk_ix))
    }

    /// What the apply engine acts on: a whole file, a whole hunk, or a set of
    /// changed lines within a hunk (an active region). A region is only used
    /// when a visual selection covers line rows inside a single hunk; otherwise
    /// it's the file/hunk at the cursor.
    fn flat_diff_apply_scope(&self) -> Option<ApplyScope> {
        let fd = self.flat_diff()?;
        if let Some(anchor) = fd.visual {
            let (lo, hi) = (anchor.min(fd.selected), anchor.max(fd.selected));
            if let Some(scope) = line_region_scope(&fd.rows, lo, hi) {
                return Some(scope);
            }
        }
        let (f, h) = self.flat_diff_apply_target()?;
        Some(match h {
            Some(h) => ApplyScope::Hunk(f, h),
            None => ApplyScope::File(f),
        })
    }

    /// Apply (`a`), reverse in the worktree (`v`/`-`), or reverse into the index
    /// (`u`) the change at point of the active commit/diff view, using its diff
    /// as the patch — magit's apply engine. Acts on the region when a selection
    /// covers lines in one hunk, else the hunk at point, else the whole file
    /// (cursor on a file header). `git apply` is atomic and reports an
    /// inapplicable patch (e.g. already in the worktree) as an error.
    fn apply_at_point(
        &mut self,
        target: ApplyTarget,
        reverse: bool,
        progress: &str,
        done: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.flat_diff_apply_scope() else {
            self.set_status("No change at point".to_string(), false, cx);
            return;
        };
        let (file_ix, hunk_ix, selected) = match scope {
            ApplyScope::File(f) => (f, None, None),
            ApplyScope::Hunk(f, h) => (f, Some(h), None),
            ApplyScope::Lines(f, h, idx) => (f, Some(h), Some(idx)),
        };
        let Some(file) = self
            .active_diff_files()
            .and_then(|fs| fs.get(file_ix).cloned())
        else {
            self.set_status("No change at point".to_string(), false, cx);
            return;
        };
        let hunk = hunk_ix.and_then(|h| file.hunks.get(h).cloned());
        // Deactivate the region: the action consumes it (and the shown diff no
        // longer reflects the repo once applied).
        if let Some(fd) = self.flat_diff_mut() {
            fd.visual = None;
        }
        self.run_job(
            progress,
            done,
            move |repo| {
                match (&hunk, &selected) {
                    (Some(h), Some(sel)) => repo.apply_lines_to(&file, h, sel, target, reverse),
                    (Some(h), None) => repo.apply_hunk_to(&file, h, target, reverse),
                    (None, _) => repo.apply_file_to(&file, target, reverse),
                }
                .map(|()| String::new())
            },
            cx,
        );
    }

    pub(crate) fn apply_at_point_to_worktree(&mut self, cx: &mut Context<Self>) {
        self.apply_at_point(
            ApplyTarget::Worktree,
            false,
            "Applying…",
            "Applied to worktree",
            cx,
        );
    }

    pub(crate) fn reverse_at_point_in_worktree(&mut self, cx: &mut Context<Self>) {
        self.apply_at_point(
            ApplyTarget::Worktree,
            true,
            "Reversing…",
            "Reversed in worktree",
            cx,
        );
    }

    pub(crate) fn reverse_at_point_in_index(&mut self, cx: &mut Context<Self>) {
        self.apply_at_point(
            ApplyTarget::Index,
            true,
            "Reverse-staging…",
            "Reverse-staged into index",
            cx,
        );
    }

    pub(crate) fn close_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Return to the screen the commit view was opened from (log or status).
        if let Screen::Commit { back, .. } = std::mem::take(&mut self.screen) {
            self.screen = *back;
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_diff_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Diff { back, .. } = std::mem::take(&mut self.screen) {
            self.screen = *back;
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// The active screen's flattened-diff body, if a commit or diff view is
    /// open — the one target the shared cursor/visual/copy handlers act on.
    pub(crate) fn flat_diff(&self) -> Option<&FlatDiff> {
        match &self.screen {
            Screen::Commit { view, .. } => Some(&view.body),
            Screen::Diff { view, .. } => Some(&view.body),
            _ => None,
        }
    }

    pub(crate) fn flat_diff_mut(&mut self) -> Option<&mut FlatDiff> {
        match &mut self.screen {
            Screen::Commit { view, .. } => Some(&mut view.body),
            Screen::Diff { view, .. } => Some(&mut view.body),
            _ => None,
        }
    }

    /// Move the open diff screen's cursor by `delta`, keeping it in view.
    pub(crate) fn flat_diff_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(fd) = self.flat_diff_mut() {
            fd.move_by(delta);
            cx.notify();
        }
    }

    /// Fold or unfold the File/Hunk section at the open diff screen's cursor.
    pub(crate) fn flat_diff_toggle_fold(&mut self, cx: &mut Context<Self>) {
        if let Some(fd) = self.flat_diff_mut() {
            let ix = fd.selected;
            if fd.toggle_fold(ix) {
                cx.notify();
            }
        }
    }

    /// Toggle a visual selection in the open diff screen.
    pub(crate) fn flat_diff_toggle_visual(&mut self, cx: &mut Context<Self>) {
        if let Some(fd) = self.flat_diff_mut() {
            fd.toggle_visual();
            cx.notify();
        }
    }

    /// Copy the open diff screen's visual selection (or the line at point),
    /// then exit visual mode — the counterpart to [`Self::copy_selection`].
    pub(crate) fn copy_flat_diff_selection(&mut self, cx: &mut Context<Self>) {
        let Some(fd) = self.flat_diff_mut() else {
            return;
        };
        // A mouse char selection takes precedence over the line-wise selection.
        let text = if let Some(sel) = fd.char_sel.filter(|c| !c.is_empty()) {
            let row_text = fd
                .rows
                .get(sel.row)
                .map(commit_row_text)
                .unwrap_or_default();
            fd.char_sel = None;
            sel.slice(&row_text).to_string()
        } else {
            let text = fd.selection_text();
            fd.visual = None;
            text
        };
        self.copy_to_clipboard(text, cx);
    }

    pub(crate) fn toggle_commit_details(&mut self, cx: &mut Context<Self>) {
        if let Some(cv) = self.commit_view_mut() {
            cv.show_details = !cv.show_details;
            // Prepending/removing the detail rows shifts every header index.
            cv.body.collapsed.clear();
            if cv.show_details {
                prepend_commit_details(&mut cv.body.rows, &cv.details);
            } else {
                cv.body
                    .rows
                    .retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
                cv.body.selected = cv.body.selected.min(cv.body.rows.len().saturating_sub(1));
                // Re-anchor the viewport too: removing rows can leave it
                // scrolled past the new end.
                cv.body
                    .scroll
                    .scroll_to_item(cv.body.selected, gpui::ScrollStrategy::Top);
            }
            cx.notify();
        }
    }
}

/// Added/removed line counts for one file diff.
/// The full-row indices visible given a set of collapsed File/Hunk headers:
/// lines under a collapsed hunk, and hunks/lines under a collapsed file, are
/// hidden. Shared by the flat-diff views and the commit editor's preview.
pub(crate) fn visible_diff_rows(
    rows: &[CommitDiffRow],
    collapsed: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    let mut vis = Vec::with_capacity(rows.len());
    let (mut file_collapsed, mut hunk_collapsed, mut stats_collapsed) = (false, false, false);
    for (ix, row) in rows.iter().enumerate() {
        match row {
            CommitDiffRow::Stats { .. } => {
                stats_collapsed = collapsed.contains(&ix);
                vis.push(ix);
            }
            // The per-file lines are the diffstat summary's foldable content.
            CommitDiffRow::StatLine { .. } => {
                if !stats_collapsed {
                    vis.push(ix);
                }
            }
            CommitDiffRow::File { .. } => {
                file_collapsed = collapsed.contains(&ix);
                hunk_collapsed = false;
                vis.push(ix);
            }
            CommitDiffRow::Hunk(_) => {
                if file_collapsed {
                    continue;
                }
                hunk_collapsed = collapsed.contains(&ix);
                vis.push(ix);
            }
            CommitDiffRow::Line { .. } => {
                if !file_collapsed && !hunk_collapsed {
                    vis.push(ix);
                }
            }
            _ => vis.push(ix),
        }
    }
    vis
}

/// The fold header governing `ix`: the row itself if it's a File/Hunk/Stats
/// header, the enclosing hunk header for a diff line, or the diffstat summary for
/// a per-file line. `None` for anything unfoldable.
pub(crate) fn fold_header_for(rows: &[CommitDiffRow], ix: usize) -> Option<usize> {
    match rows.get(ix)? {
        CommitDiffRow::File { .. } | CommitDiffRow::Hunk(_) | CommitDiffRow::Stats { .. } => {
            Some(ix)
        }
        CommitDiffRow::Line { .. } => rows[..ix]
            .iter()
            .rposition(|r| matches!(r, CommitDiffRow::Hunk(_))),
        CommitDiffRow::StatLine { .. } => rows[..ix]
            .iter()
            .rposition(|r| matches!(r, CommitDiffRow::Stats { .. })),
        _ => None,
    }
}

pub(crate) fn file_line_counts(diff: &FileDiff) -> (usize, usize) {
    let (mut added, mut removed) = (0usize, 0usize);
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                _ => {}
            }
        }
    }
    (added, removed)
}

/// The diffstat block above the diffs (magit's overview): the "N files changed …"
/// summary, then a per-file `path N +++---` line for each file. The summary is a
/// collapsible header over the per-file lines. Empty when there are no files.
pub(crate) fn diffstat_block(files: &[(FileDiff, Option<&'static str>)]) -> Vec<CommitDiffRow> {
    if files.is_empty() {
        return Vec::new();
    }
    let (mut insertions, mut deletions) = (0usize, 0usize);
    let mut stat_lines = Vec::with_capacity(files.len());
    for (diff, _) in files {
        let (a, r) = file_line_counts(diff);
        insertions += a;
        deletions += r;
        stat_lines.push(CommitDiffRow::StatLine {
            path: diff.display_path().to_string(),
            added: a,
            removed: r,
        });
    }
    let mut rows = Vec::with_capacity(files.len() + 1);
    rows.push(CommitDiffRow::Stats {
        files: files.len(),
        insertions,
        deletions,
    });
    rows.extend(stat_lines);
    rows
}

pub(crate) fn diff_title(base: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        base.to_string()
    } else if paths.len() == 1 {
        format!("{base} -- {}", paths[0])
    } else {
        format!("{base} -- {} paths", paths.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{fold_header_for, visible_diff_rows};
    use crate::commit_editor::CommitDiffRow;
    use magritte_core::{Change, LineKind};
    use std::collections::HashSet;
    use std::rc::Rc;

    fn line() -> CommitDiffRow {
        CommitDiffRow::Line {
            kind: LineKind::Context,
            spans: Rc::from(Vec::new()),
        }
    }

    // Stats(0) → StatLine a(1), StatLine b(2); Note(3); File a(4) → Hunk(5) →
    // Line(6), Line(7); File b(8) → Hunk(9) → Line(10).
    fn sample() -> Vec<CommitDiffRow> {
        vec![
            CommitDiffRow::Stats {
                files: 2,
                insertions: 3,
                deletions: 1,
            },
            CommitDiffRow::StatLine {
                path: "a".into(),
                added: 2,
                removed: 0,
            },
            CommitDiffRow::StatLine {
                path: "b".into(),
                added: 1,
                removed: 1,
            },
            CommitDiffRow::Note(String::new()),
            CommitDiffRow::File {
                change: Change::Modified,
                path: "a".into(),
            },
            CommitDiffRow::Hunk("@@ a".into()),
            line(),
            line(),
            CommitDiffRow::File {
                change: Change::Modified,
                path: "b".into(),
            },
            CommitDiffRow::Hunk("@@ b".into()),
            line(),
        ]
    }

    fn collapsed(ixs: &[usize]) -> HashSet<usize> {
        ixs.iter().copied().collect()
    }

    #[test]
    fn visible_rows_respects_collapsed_headers() {
        let rows = sample();
        // Nothing collapsed: every row visible.
        assert_eq!(
            visible_diff_rows(&rows, &collapsed(&[])),
            (0..rows.len()).collect::<Vec<_>>()
        );
        // Collapsed diffstat summary hides its per-file lines (1, 2) only.
        assert_eq!(
            visible_diff_rows(&rows, &collapsed(&[0])),
            vec![0, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        // Collapsed file a (4) hides its hunk (5) and lines (6, 7).
        assert_eq!(
            visible_diff_rows(&rows, &collapsed(&[4])),
            vec![0, 1, 2, 3, 4, 8, 9, 10]
        );
        // Collapsed hunk (5) hides only its lines (6, 7), not the file.
        assert_eq!(
            visible_diff_rows(&rows, &collapsed(&[5])),
            vec![0, 1, 2, 3, 4, 5, 8, 9, 10]
        );
        // Independent folds compose.
        assert_eq!(
            visible_diff_rows(&rows, &collapsed(&[0, 8])),
            vec![0, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn fold_header_maps_rows_to_their_header() {
        let rows = sample();
        assert_eq!(fold_header_for(&rows, 0), Some(0)); // Stats → itself
        assert_eq!(fold_header_for(&rows, 1), Some(0)); // StatLine → its Stats
        assert_eq!(fold_header_for(&rows, 4), Some(4)); // File → itself
        assert_eq!(fold_header_for(&rows, 5), Some(5)); // Hunk → itself
        assert_eq!(fold_header_for(&rows, 6), Some(5)); // Line → enclosing hunk
        assert_eq!(fold_header_for(&rows, 3), None); // Note → unfoldable
    }
}

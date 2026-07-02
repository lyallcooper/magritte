//! The read-only diff screens: a commit's detail (header + message + diff,
//! opened from the log or a status commit row, with an LRU cache of loaded
//! commits) and the standalone diff buffer (the `d` diff transient's output).
//! Both render the same flattened [`CommitDiffRow`] list; their shared body —
//! rows, scroll, cursor, visual selection — is the [`FlatDiff`] each screen
//! embeds. `impl StatusView` like the other view slices.

use gpui::{Context, SharedString, UniformListScrollHandle, Window};
use magritte_core::{CommitMetadata, FileDiff};

use crate::*;

/// The shared body of a read-only flattened diff screen: its rows, scroll
/// handle, cursor row (drives scrolling), and visual-selection anchor — so
/// lines can be selected and yanked in these views too.
pub(crate) struct FlatDiff {
    pub(crate) rows: Vec<CommitDiffRow>,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) selected: usize,
    pub(crate) visual: Option<usize>,
}

impl FlatDiff {
    pub(crate) fn loading() -> Self {
        FlatDiff {
            rows: vec![CommitDiffRow::Note("Loading…".to_string())],
            scroll: UniformListScrollHandle::new(),
            selected: 0,
            visual: None,
        }
    }

    /// Move the cursor by `delta`, keeping it in view.
    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
    }

    /// Toggle a visual selection anchored at the cursor.
    fn toggle_visual(&mut self) {
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

/// A single commit's detail (opened from the log): its header and diff, as the
/// same flattened rows the commit editor renders.
pub(crate) struct CommitView {
    /// The commit's full hash — passed to `diff_commit` and copied by the
    /// header's copy button.
    pub(crate) rev: String,
    /// The abbreviated hash, shown in the header next to the copy button.
    pub(crate) short: SharedString,
    /// The commit subject, shown after the hash in the header.
    pub(crate) subject: SharedString,
    pub(crate) details: Vec<String>,
    pub(crate) show_details: bool,
    pub(crate) body: FlatDiff,
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

/// A standalone diff buffer (`d` / Magit's `magit-diff`): a title plus a
/// flattened, read-only list of file/hunk/line rows.
pub(crate) struct DiffView {
    pub(crate) title: SharedString,
    pub(crate) body: FlatDiff,
}

#[derive(Clone)]
pub(crate) enum DiffRequest {
    Unstaged { args: Vec<String>, paths: Vec<String> },
    Staged { args: Vec<String>, paths: Vec<String> },
    Worktree { rev: String, args: Vec<String>, paths: Vec<String> },
    Range { range: String, args: Vec<String>, paths: Vec<String> },
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
    pub(crate) fn open_commit(&mut self, hash: String, short: String, subject: String, cx: &mut Context<Self>) {
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
        self.screen = Screen::Commit {
            view: CommitView {
                rev: rev.clone(),
                short: SharedString::from(short),
                subject: SharedString::from(subject),
                details: Vec::new(),
                show_details: false,
                body: FlatDiff::loading(),
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
                this.insert_commit_cache(key, loaded.clone());
                this.populate_commit_view(&loaded, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn insert_commit_cache(&mut self, key: CommitCacheKey, entry: CommitCacheEntry) {
        if !self.commit_cache.contains_key(&key) {
            self.commit_cache_order.push_back(key.clone());
        }
        self.commit_cache.insert(key, entry);
        while self.commit_cache_order.len() > COMMIT_CACHE_CAPACITY {
            if let Some(old) = self.commit_cache_order.pop_front() {
                self.commit_cache.remove(&old);
            }
        }
    }

    pub(crate) fn populate_commit_view(&mut self, entry: &CommitCacheEntry, cx: &mut Context<Self>) {
        let details = commit_metadata_lines(&entry.metadata);
        let show_details = self.commit_view().is_some_and(|cv| cv.show_details);
        let mut rows = self.commit_detail_rows(&entry.message, &entry.files, cx);
        if show_details {
            prepend_commit_details(&mut rows, &details);
        }
        if let Some(cv) = self.commit_view_mut() {
            cv.details = details;
            cv.body.rows = rows;
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
                let rows = match loaded {
                    Ok(files) if files.is_empty() => {
                        vec![CommitDiffRow::Note("No changes".to_string())]
                    }
                    Ok(files) => this.diff_rows(&files, cx),
                    Err(e) => vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))],
                };
                if let Some(dv) = this.diff_view_mut() {
                    dv.body.rows = rows;
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
        let mut body = message.lines().skip(1);
        if matches!(body.clone().next(), Some("")) {
            body.next();
        }
        let mut body = body.peekable();
        if body.peek().is_some() {
            rows.push(CommitDiffRow::Note(String::new()));
        }
        for line in body {
            rows.push(CommitDiffRow::Message(line.to_string()));
        }
        if !rows.is_empty() {
            rows.push(CommitDiffRow::Note(String::new()));
        }
        rows.extend(self.diff_rows(files, cx));
        rows
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
        let text = fd.selection_text();
        fd.visual = None;
        self.copy_to_clipboard(text, cx);
    }

    pub(crate) fn toggle_commit_details(&mut self, cx: &mut Context<Self>) {
        if let Some(cv) = self.commit_view_mut() {
            cv.show_details = !cv.show_details;
            if cv.show_details {
                prepend_commit_details(&mut cv.body.rows, &cv.details);
            } else {
                cv.body.rows.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
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

pub(crate) fn diff_title(base: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        base.to_string()
    } else if paths.len() == 1 {
        format!("{base} -- {}", paths[0])
    } else {
        format!("{base} -- {} paths", paths.len())
    }
}

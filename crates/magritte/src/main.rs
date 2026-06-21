//! Magritte — M2: the status view as a foldable, virtualized section tree with
//! evil-style navigation.
//!
//! The view holds a flattened list of [`Row`]s rebuilt from the parsed status,
//! the fold state, and any lazily-loaded diffs. Rendering goes through
//! `uniform_list`, so only on-screen rows become elements — a 50k-line diff
//! costs the same as a short one. All git work (status + per-file diffs) runs
//! on the background executor; a generation counter drops stale results.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    div, px, uniform_list, AnyElement, App, AppContext, Application, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled,
    TitlebarOptions, UniformListScrollHandle, Window, WindowOptions,
};
use magritte_core::transient::{self, Suffix, Transient};
use magritte_core::{Change, DiffSource, EntryKind, FileDiff, FileEntry, LineKind, Repo, Status};

/// An open transient popup and the switches toggled on within it.
struct TransientState {
    def: Transient,
    active: std::collections::HashSet<String>,
}

impl TransientState {
    fn new(def: Transient) -> Self {
        TransientState {
            def,
            active: std::collections::HashSet::new(),
        }
    }
}

mod theme {
    use gpui::{rgb, Rgba};
    pub fn bg() -> Rgba {
        rgb(0x1e2025)
    }
    pub fn fg() -> Rgba {
        rgb(0xced2da)
    }
    pub fn dim() -> Rgba {
        rgb(0x7f8694)
    }
    pub fn selection() -> Rgba {
        rgb(0x2f3340)
    }
    pub fn visual() -> Rgba {
        rgb(0x2b3650)
    }
    pub fn section() -> Rgba {
        rgb(0x7aa2f7)
    }
    pub fn hunk() -> Rgba {
        rgb(0xbb9af7)
    }
    pub fn added() -> Rgba {
        rgb(0x9ece6a)
    }
    pub fn removed() -> Rgba {
        rgb(0xf7768e)
    }
    pub fn modified() -> Rgba {
        rgb(0xe0af68)
    }
    pub fn banner() -> Rgba {
        rgb(0x3a2f1a)
    }
    pub fn panel() -> Rgba {
        rgb(0x252830)
    }
    pub fn border() -> Rgba {
        rgb(0x3a3f4b)
    }
}

/// After a refresh, warm at most this many file diffs in the background...
const PREFETCH_FILE_CAP: usize = 16;
/// ...skipping any whose changed-line count exceeds this, so massive diffs are
/// only computed when the user actually expands them.
const PREFETCH_LINE_CAP: u32 = 2000;

/// Which top-level section a row belongs to. Used as a stable fold key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SectionId {
    Untracked,
    Unstaged,
    Staged,
}

/// Identity of a foldable node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FoldKey {
    Section(SectionId),
    File(DiffSource, String),
}

/// The staging verb a keypress requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Stage,
    Unstage,
    Discard,
}

/// A file identified by its path and which section it appears in.
#[derive(Debug, Clone)]
struct FileRef {
    section: SectionId,
    path: String,
}

/// What the row at point represents, for "act on point" staging.
#[derive(Debug, Clone)]
enum Target {
    File(FileRef),
    Hunk { file: FileRef, hunk: usize },
    Line { file: FileRef, hunk: usize, line: usize },
}

/// How a multi-hunk region selection should be applied.
#[derive(Debug, Clone, Copy)]
enum RegionKind {
    Stage,
    Unstage,
    Discard,
    DiscardStaged,
}

/// A resolved git mutation, runnable on the background executor.
enum Action {
    StageFile(String),
    UnstageFile(String),
    DiscardTracked(String),
    DiscardUntracked(String),
    StageAll,
    UnstageAll,
    StageHunk(FileDiff, usize),
    UnstageHunk(FileDiff, usize),
    DiscardHunk(FileDiff, usize),
    StageLines(FileDiff, usize, Vec<usize>),
    UnstageLines(FileDiff, usize, Vec<usize>),
    DiscardLines(FileDiff, usize, Vec<usize>),
    DiscardStagedFile(String),
    DiscardStagedHunk(FileDiff, usize),
    DiscardStagedLines(FileDiff, usize, Vec<usize>),
    /// A region selection spanning one file's hunks: hunk index -> line indices.
    ApplyRegion {
        kind: RegionKind,
        file: FileDiff,
        selections: Vec<(usize, Vec<usize>)>,
    },
    /// Several actions applied in sequence (a region spanning multiple files).
    Batch(Vec<Action>),
}

impl Action {
    fn run(self, repo: &Repo) -> Result<(), String> {
        let hunk = |file: &FileDiff, ix: usize| -> Result<(), String> {
            file.hunks
                .get(ix)
                .ok_or_else(|| "hunk no longer present".to_string())
                .map(|_| ())
        };
        let to_err = |r: magritte_core::Result<()>| r.map_err(|e| e.to_string());
        match self {
            Action::StageFile(p) => to_err(repo.stage_file(&p)),
            Action::UnstageFile(p) => to_err(repo.unstage_file(&p)),
            Action::DiscardTracked(p) => to_err(repo.discard_tracked_file(&p)),
            Action::DiscardUntracked(p) => to_err(repo.discard_untracked_file(&p)),
            Action::StageAll => to_err(repo.stage_all()),
            Action::UnstageAll => to_err(repo.unstage_all()),
            Action::StageHunk(f, h) => hunk(&f, h).and_then(|_| to_err(repo.stage_hunk(&f, &f.hunks[h]))),
            Action::UnstageHunk(f, h) => hunk(&f, h).and_then(|_| to_err(repo.unstage_hunk(&f, &f.hunks[h]))),
            Action::DiscardHunk(f, h) => hunk(&f, h).and_then(|_| to_err(repo.discard_hunk(&f, &f.hunks[h]))),
            Action::StageLines(f, h, l) => hunk(&f, h).and_then(|_| to_err(repo.stage_lines(&f, &f.hunks[h], &l))),
            Action::UnstageLines(f, h, l) => hunk(&f, h).and_then(|_| to_err(repo.unstage_lines(&f, &f.hunks[h], &l))),
            Action::DiscardLines(f, h, l) => hunk(&f, h).and_then(|_| to_err(repo.discard_lines(&f, &f.hunks[h], &l))),
            Action::DiscardStagedFile(p) => to_err(repo.discard_staged_file(&p)),
            Action::DiscardStagedHunk(f, h) => hunk(&f, h).and_then(|_| to_err(repo.discard_staged_hunk(&f, &f.hunks[h]))),
            Action::DiscardStagedLines(f, h, l) => hunk(&f, h).and_then(|_| to_err(repo.discard_staged_lines(&f, &f.hunks[h], &l))),
            Action::ApplyRegion { kind, file, selections } => to_err(match kind {
                RegionKind::Stage => repo.stage_file_lines(&file, &selections),
                RegionKind::Unstage => repo.unstage_file_lines(&file, &selections),
                RegionKind::Discard => repo.discard_file_lines(&file, &selections),
                RegionKind::DiscardStaged => repo.discard_staged_file_lines(&file, &selections),
            }),
            Action::Batch(actions) => {
                for action in actions {
                    action.run(repo)?;
                }
                Ok(())
            }
        }
    }
}

fn section_source(section: SectionId) -> Option<DiffSource> {
    match section {
        SectionId::Untracked => None,
        SectionId::Unstaged => Some(DiffSource::Unstaged),
        SectionId::Staged => Some(DiffSource::Staged),
    }
}

/// Async state of a single file's diff.
enum DiffState {
    Loading,
    Loaded(FileDiff),
    Empty,
    Failed(String),
}

/// One rendered line. Every row is the same height so `uniform_list` can
/// virtualize them.
struct Row {
    indent: usize,
    selectable: bool,
    /// Present on foldable rows (sections, files); `TAB` toggles this key.
    fold: Option<FoldKey>,
    /// What this row represents for staging "at point" (s/u/x).
    target: Option<Target>,
    kind: RowKind,
}

enum RowKind {
    Plain {
        text: String,
        color: gpui::Rgba,
    },
    Section {
        title: String,
        count: usize,
        expanded: bool,
    },
    File {
        code: String,
        code_color: gpui::Rgba,
        label: String,
        expanded: Option<bool>,
    },
    HunkHeader {
        text: String,
    },
    Diff {
        text: String,
        color: gpui::Rgba,
    },
}

struct StatusView {
    /// The directory we tried to open (for error messages).
    root: PathBuf,
    repo: Option<Repo>,
    status: Option<Status>,
    error: Option<String>,
    expanded: HashSet<FoldKey>,
    diffs: HashMap<(DiffSource, String), DiffState>,
    rows: Vec<Row>,
    selected: usize,
    /// Anchor row of an active visual (region) selection; `None` when off.
    /// The selection spans `min(anchor, selected)..=max(anchor, selected)`.
    visual: Option<usize>,
    generation: u64,
    pending_g: bool,
    /// An open transient popup (push/pull/fetch/…), or `None`.
    transient: Option<TransientState>,
    /// Last operation result / progress, shown in the bottom bar.
    status_message: Option<String>,
    /// A pending destructive confirmation: (prompt, action awaiting `y`).
    confirm: Option<(String, Action)>,
    focus: FocusHandle,
    focused_once: bool,
    scroll: UniformListScrollHandle,
}

impl StatusView {
    fn new(start_dir: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let root = start_dir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo = Repo::discover(&root).ok();

        // Sections are expanded by default; individual files start collapsed,
        // so opening a large repo loads no diffs until a file is expanded.
        let mut expanded = HashSet::new();
        expanded.insert(FoldKey::Section(SectionId::Untracked));
        expanded.insert(FoldKey::Section(SectionId::Unstaged));
        expanded.insert(FoldKey::Section(SectionId::Staged));

        let mut view = StatusView {
            root,
            repo,
            status: None,
            error: None,
            expanded,
            diffs: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            visual: None,
            generation: 0,
            pending_g: false,
            transient: None,
            status_message: None,
            confirm: None,
            focus: cx.focus_handle(),
            focused_once: false,
            scroll: UniformListScrollHandle::new(),
        };
        view.refresh(cx);
        view
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.diffs.clear();
        self.error = None;

        let Some(repo) = self.repo.clone() else {
            self.error = Some(format!("Not a git repository: {}", self.root.display()));
            self.rebuild_rows();
            return;
        };

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.status() })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(status) => {
                        this.status = Some(status);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                this.rebuild_rows();
                this.clamp_selection();
                // Re-load diffs for any files that were expanded before the
                // refresh cleared them, so they don't get stuck on "Loading…".
                this.reload_expanded_diffs(cx);
                // Warm a bounded set of small diffs so first expand feels instant.
                this.start_prefetch(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-trigger diff loads for every currently-expanded file.
    fn reload_expanded_diffs(&mut self, cx: &mut Context<Self>) {
        let files: Vec<(DiffSource, String)> = self
            .expanded
            .iter()
            .filter_map(|k| match k {
                FoldKey::File(source, path) => Some((*source, path.clone())),
                FoldKey::Section(_) => None,
            })
            .collect();
        for (source, path) in files {
            self.ensure_diff(source, path, cx);
        }
    }

    /// After a refresh, probe changed-line counts (cheap `git diff --numstat`)
    /// off the UI thread, then warm the diffs for a bounded number of small
    /// files so expanding them feels instant. Massive diffs are skipped and
    /// load lazily on explicit expand.
    fn start_prefetch(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let generation = self.generation;

        cx.spawn(async move |this, cx| {
            let counts = cx
                .background_executor()
                .spawn(async move {
                    let mut all = Vec::new();
                    for source in [DiffSource::Unstaged, DiffSource::Staged] {
                        if let Ok(list) = repo.diff_line_counts(source) {
                            for (path, lines) in list {
                                all.push((source, path, lines));
                            }
                        }
                    }
                    all
                })
                .await;

            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let mut warmed = 0;
                for (source, path, lines) in counts {
                    if warmed >= PREFETCH_FILE_CAP {
                        break;
                    }
                    if lines > PREFETCH_LINE_CAP {
                        continue;
                    }
                    if this.diffs.contains_key(&(source, path.clone())) {
                        continue;
                    }
                    this.ensure_diff(source, path, cx);
                    warmed += 1;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Kick off a background diff load for a file if not already present.
    fn ensure_diff(&mut self, source: DiffSource, path: String, cx: &mut Context<Self>) {
        let key = (source, path.clone());
        if self.diffs.contains_key(&key) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.diffs.insert(key.clone(), DiffState::Loading);
        let generation = self.generation;

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { repo.diff_path(source, &path) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let state = match loaded {
                    Ok(Some(diff)) => DiffState::Loaded(diff),
                    Ok(None) => DiffState::Empty,
                    Err(e) => DiffState::Failed(e.to_string()),
                };
                this.diffs.insert(key, state);
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- Row construction -------------------------------------------------

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();

        if let Some(error) = &self.error {
            rows.push(plain(format!("Error: {error}"), theme::removed()));
            self.rows = rows;
            return;
        }
        let Some(status) = &self.status else {
            rows.push(plain("Loading…", theme::dim()));
            self.rows = rows;
            return;
        };

        let head = &status.head;
        let branch = head
            .branch
            .clone()
            .unwrap_or_else(|| "HEAD (detached)".to_string());
        rows.push(plain(format!("Head:    {branch}"), theme::fg()));
        if let Some(upstream) = &head.upstream {
            rows.push(plain(
                format!("Push:    {upstream}  (+{} -{})", head.ahead, head.behind),
                theme::dim(),
            ));
        }

        self.push_section(
            &mut rows,
            SectionId::Untracked,
            "Untracked files",
            status.untracked().collect(),
            None,
        );
        self.push_section(
            &mut rows,
            SectionId::Unstaged,
            "Unstaged changes",
            status.unstaged().collect(),
            Some(DiffSource::Unstaged),
        );
        self.push_section(
            &mut rows,
            SectionId::Staged,
            "Staged changes",
            status.staged().collect(),
            Some(DiffSource::Staged),
        );

        if rows.len() <= 2 {
            rows.push(spacer());
            rows.push(plain("Nothing to commit, working tree clean", theme::dim()));
        }

        self.rows = rows;
    }

    fn push_section(
        &self,
        rows: &mut Vec<Row>,
        id: SectionId,
        title: &str,
        entries: Vec<&FileEntry>,
        source: Option<DiffSource>,
    ) {
        if entries.is_empty() {
            return;
        }
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: title.to_string(),
                count: entries.len(),
                expanded,
            },
        });
        if !expanded {
            return;
        }

        for entry in entries {
            let path = entry.path.clone();
            let label = match &entry.orig_path {
                Some(orig) => format!("{orig} → {}", entry.path),
                None => entry.path.clone(),
            };
            let file_ref = FileRef {
                section: id,
                path: path.clone(),
            };
            let file_expanded = source.map(|s| self.expanded.contains(&FoldKey::File(s, path.clone())));
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: source.map(|s| FoldKey::File(s, path.clone())),
                target: Some(Target::File(file_ref.clone())),
                kind: RowKind::File {
                    code: status_code(entry),
                    code_color: code_color(entry),
                    label,
                    expanded: file_expanded,
                },
            });

            if let (Some(src), Some(true)) = (source, file_expanded) {
                self.push_file_body(rows, src, &file_ref);
            }
        }
    }

    fn push_file_body(&self, rows: &mut Vec<Row>, source: DiffSource, file: &FileRef) {
        match self.diffs.get(&(source, file.path.clone())) {
            Some(DiffState::Loaded(diff)) => {
                if diff.is_binary {
                    rows.push(message("Binary file"));
                } else if diff.hunks.is_empty() {
                    rows.push(message("(no textual changes)"));
                }
                for (hunk_ix, hunk) in diff.hunks.iter().enumerate() {
                    rows.push(Row {
                        indent: 2,
                        selectable: true,
                        fold: None,
                        target: Some(Target::Hunk {
                            file: file.clone(),
                            hunk: hunk_ix,
                        }),
                        kind: RowKind::HunkHeader {
                            text: hunk_header_text(hunk),
                        },
                    });
                    for (line_ix, line) in hunk.lines.iter().enumerate() {
                        let (sign, color) = match line.kind {
                            LineKind::Added => ('+', theme::added()),
                            LineKind::Removed => ('-', theme::removed()),
                            LineKind::Context => (' ', theme::fg()),
                            LineKind::NoNewline => (' ', theme::dim()),
                        };
                        let text = if line.kind == LineKind::NoNewline {
                            line.content.clone()
                        } else {
                            format!("{sign}{}", line.content)
                        };
                        rows.push(Row {
                            indent: 2,
                            selectable: true,
                            fold: None,
                            target: Some(Target::Line {
                                file: file.clone(),
                                hunk: hunk_ix,
                                line: line_ix,
                            }),
                            kind: RowKind::Diff { text, color },
                        });
                    }
                }
            }
            Some(DiffState::Loading) | None => rows.push(message("Loading diff…")),
            Some(DiffState::Empty) => rows.push(message("(no changes)")),
            Some(DiffState::Failed(e)) => rows.push(message(&format!("diff failed: {e}"))),
        }
    }

    // --- Selection & folding ---------------------------------------------

    fn move_selection(&mut self, delta: isize) {
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

    fn select_edge(&mut self, last: bool) {
        let found = if last {
            (0..self.rows.len()).rev().find(|&i| self.rows[i].selectable)
        } else {
            (0..self.rows.len()).find(|&i| self.rows[i].selectable)
        };
        if let Some(i) = found {
            self.selected = i;
        }
    }

    /// Move to the next/previous top-level section header.
    fn select_section(&mut self, forward: bool) {
        let is_section = |r: &Row| matches!(r.kind, RowKind::Section { .. });
        let next = if forward {
            (self.selected + 1..self.rows.len()).find(|&i| is_section(&self.rows[i]))
        } else {
            (0..self.selected).rev().find(|&i| is_section(&self.rows[i]))
        };
        if let Some(i) = next {
            self.selected = i;
        }
    }

    fn toggle_fold(&mut self, cx: &mut Context<Self>) {
        // Folding changes row indices, which would invalidate a visual anchor.
        self.visual = None;
        let Some(key) = self.rows.get(self.selected).and_then(|r| r.fold.clone()) else {
            return;
        };
        if self.expanded.contains(&key) {
            self.expanded.remove(&key);
        } else {
            self.expanded.insert(key.clone());
            if let FoldKey::File(source, path) = &key {
                self.ensure_diff(*source, path.clone(), cx);
            }
        }
        self.rebuild_rows();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
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

    // --- Staging ----------------------------------------------------------

    /// The loaded diff for a file in a given section, if available.
    fn diff_for(&self, file: &FileRef) -> Option<FileDiff> {
        let source = section_source(file.section)?;
        match self.diffs.get(&(source, file.path.clone()))? {
            DiffState::Loaded(diff) => Some(diff.clone()),
            _ => None,
        }
    }

    /// Resolve the row at point + verb into a concrete git action, if the verb
    /// is meaningful there (e.g. you cannot stage something already staged).
    fn resolve_action(&self, op: Op) -> Option<Action> {
        let target = self.rows.get(self.selected)?.target.clone()?;
        match (op, target) {
            // Stage: from the untracked or unstaged side.
            (Op::Stage, Target::File(f)) => match f.section {
                SectionId::Untracked | SectionId::Unstaged => Some(Action::StageFile(f.path)),
                SectionId::Staged => None,
            },
            (Op::Stage, Target::Hunk { file, hunk }) if file.section == SectionId::Unstaged => {
                Some(Action::StageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Stage, Target::Line { file, hunk, line }) if file.section == SectionId::Unstaged => {
                Some(Action::StageLines(self.diff_for(&file)?, hunk, vec![line]))
            }

            // Unstage: from the staged side.
            (Op::Unstage, Target::File(f)) if f.section == SectionId::Staged => {
                Some(Action::UnstageFile(f.path))
            }
            (Op::Unstage, Target::Hunk { file, hunk }) if file.section == SectionId::Staged => {
                Some(Action::UnstageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Unstage, Target::Line { file, hunk, line }) if file.section == SectionId::Staged => {
                Some(Action::UnstageLines(self.diff_for(&file)?, hunk, vec![line]))
            }

            // Discard: untracked removes the file; unstaged reverts to the
            // index; staged reverts both index and worktree to HEAD.
            (Op::Discard, Target::File(f)) => match f.section {
                SectionId::Untracked => Some(Action::DiscardUntracked(f.path)),
                SectionId::Unstaged => Some(Action::DiscardTracked(f.path)),
                SectionId::Staged => Some(Action::DiscardStagedFile(f.path)),
            },
            (Op::Discard, Target::Hunk { file, hunk }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardHunk(self.diff_for(&file)?, hunk)),
                SectionId::Staged => Some(Action::DiscardStagedHunk(self.diff_for(&file)?, hunk)),
                SectionId::Untracked => None,
            },
            (Op::Discard, Target::Line { file, hunk, line }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardLines(self.diff_for(&file)?, hunk, vec![line])),
                SectionId::Staged => Some(Action::DiscardStagedLines(self.diff_for(&file)?, hunk, vec![line])),
                SectionId::Untracked => None,
            },

            _ => None,
        }
    }

    /// The inclusive row range of the active visual selection, if any.
    fn visual_range(&self) -> Option<(usize, usize)> {
        self.visual
            .map(|anchor| (anchor.min(self.selected), anchor.max(self.selected)))
    }

    /// Resolve a region (visual) selection into actions. Selected lines are
    /// grouped by file and hunk, so a selection spanning multiple hunks (or
    /// files) acts on *all* of them. Groups whose section doesn't match the
    /// verb (e.g. a staged file when staging) are skipped.
    fn resolve_region_action(&self, op: Op) -> Option<Action> {
        let (lo, hi) = self.visual_range()?;

        // Group selected diff lines: file (section+path) -> hunk -> line indices,
        // preserving encounter order.
        let mut groups: Vec<(FileRef, Vec<(usize, Vec<usize>)>)> = Vec::new();
        for ix in lo..=hi {
            let Some(Target::Line { file, hunk, line }) =
                self.rows.get(ix).and_then(|r| r.target.as_ref())
            else {
                continue;
            };
            let gi = match groups
                .iter()
                .position(|(f, _)| f.section == file.section && f.path == file.path)
            {
                Some(i) => i,
                None => {
                    groups.push((file.clone(), Vec::new()));
                    groups.len() - 1
                }
            };
            let hunks = &mut groups[gi].1;
            match hunks.iter_mut().find(|(h, _)| *h == *hunk) {
                Some((_, lines)) => lines.push(*line),
                None => hunks.push((*hunk, vec![*line])),
            }
        }

        let mut actions = Vec::new();
        for (file, selections) in groups {
            let kind = match (op, file.section) {
                (Op::Stage, SectionId::Unstaged) => RegionKind::Stage,
                (Op::Unstage, SectionId::Staged) => RegionKind::Unstage,
                (Op::Discard, SectionId::Unstaged) => RegionKind::Discard,
                (Op::Discard, SectionId::Staged) => RegionKind::DiscardStaged,
                _ => continue, // section doesn't match the verb
            };
            let Some(diff) = self.diff_for(&file) else {
                continue;
            };
            actions.push(Action::ApplyRegion {
                kind,
                file: diff,
                selections,
            });
        }

        match actions.len() {
            0 => None,
            1 => actions.pop(),
            _ => Some(Action::Batch(actions)),
        }
    }

    /// `s`/`u`/`x`: resolve and either run, or (for discard) ask to confirm.
    fn act(&mut self, op: Op, cx: &mut Context<Self>) {
        let resolved = if self.visual.is_some() {
            self.resolve_region_action(op)
        } else {
            self.resolve_action(op)
        };
        let Some(action) = resolved else {
            return;
        };
        if op == Op::Discard {
            self.confirm = Some((describe_discard(&action), action));
        } else {
            self.run_action(action, cx);
        }
        cx.notify();
    }

    /// Run a git mutation on the background executor, then refresh.
    fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        self.confirm = None;
        self.visual = None;
        let Some(repo) = self.repo.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action.run(&repo) })
                .await;
            this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.error = Some(e);
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- Transients (push/pull/fetch popups) ------------------------------

    fn open_transient(&mut self, def: Transient, cx: &mut Context<Self>) {
        self.transient = Some(TransientState::new(def));
        cx.notify();
    }

    fn handle_transient_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if key == "escape" || key == "q" {
            self.transient = None;
            cx.notify();
            return;
        }
        let Some(state) = self.transient.as_mut() else {
            return;
        };

        // Toggle a switch? Switch keys are magit-style ("-f"); a single
        // keypress of the letter toggles it.
        let switch_key = state
            .def
            .switches()
            .find(|s| s.key.trim_start_matches('-') == key)
            .map(|s| s.key.to_string());
        if let Some(sw) = switch_key {
            if !state.active.remove(&sw) {
                state.active.insert(sw);
            }
            cx.notify();
            return;
        }

        // Invoke an action?
        let action = state.def.action_for(key).copied();
        let switches: Vec<String> = state
            .def
            .switches()
            .filter(|s| state.active.contains(s.key))
            .map(|s| s.arg.to_string())
            .collect();
        if let Some(action) = action {
            self.transient = None;
            self.run_command(action.command, switches, cx);
        }
    }

    /// Run a transient command on the background executor, showing progress in
    /// the bottom bar, then refresh.
    fn run_command(&mut self, command: transient::Command, switches: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.status_message = Some(format!("{}…", describe_command(command)));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.execute(command, &switches) })
                .await;
            this.update(cx, |this, cx| {
                this.status_message = Some(match result {
                    Ok(msg) if msg.trim().is_empty() => "Done".to_string(),
                    Ok(msg) => last_line(&msg),
                    Err(e) => format!("error: {e}"),
                });
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        let shift = event.keystroke.modifiers.shift;

        // An open transient popup captures all keys.
        if self.transient.is_some() {
            // Distinguish F (pull action) from f (fetch); transient keys are
            // case-sensitive, so reconstruct the cased key.
            let cased = if shift { key.to_uppercase() } else { key.clone() };
            self.handle_transient_key(&cased, cx);
            return;
        }

        // A pending discard confirmation captures the next key.
        if self.confirm.is_some() {
            if key == "y" {
                let action = self.confirm.take().unwrap().1;
                self.run_action(action, cx);
            } else {
                self.confirm = None;
            }
            cx.notify();
            return;
        }

        if self.pending_g {
            self.pending_g = false;
            match key.as_str() {
                "g" => self.select_edge(false),
                "j" => self.select_section(true),
                "k" => self.select_section(false),
                "r" => {
                    self.refresh(cx);
                    cx.notify();
                    return;
                }
                _ => {}
            }
            self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
            return;
        }

        match key.as_str() {
            "j" => self.move_selection(1),
            "k" => self.move_selection(-1),
            "g" if shift => self.select_edge(true), // G
            "g" => {
                self.pending_g = true;
                return;
            }
            "tab" => self.toggle_fold(cx),
            // Visual (region) selection. `v`/`V` toggle; Escape cancels.
            "v" => {
                self.visual = if self.visual.is_some() {
                    None
                } else {
                    Some(self.selected)
                };
                cx.notify();
                return;
            }
            "escape" => {
                if self.visual.take().is_some() {
                    cx.notify();
                }
                return;
            }
            // Staging. Shifted variants act on the whole working tree.
            "s" if shift => return self.run_action(Action::StageAll, cx),
            "s" => return self.act(Op::Stage, cx),
            "u" if shift => return self.run_action(Action::UnstageAll, cx),
            "u" => return self.act(Op::Unstage, cx),
            "x" => return self.act(Op::Discard, cx),
            // Sync transients: P push, F pull, f fetch.
            "p" if shift => return self.open_transient(transient::push_transient(), cx),
            "f" if shift => return self.open_transient(transient::pull_transient(), cx),
            "f" => return self.open_transient(transient::fetch_transient(), cx),
            _ => return,
        }
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Render the open transient popup as a bottom panel.
    fn render_transient(&self, state: &TransientState) -> gpui::Div {
        let mut panel = div()
            .w_full()
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::panel())
            .py_1()
            .px_2()
            .flex()
            .flex_col()
            .child(
                div()
                    .text_color(theme::section())
                    .child(SharedString::from(state.def.title)),
            );

        for group in &state.def.groups {
            panel = panel.child(
                div()
                    .mt_1()
                    .text_color(theme::dim())
                    .child(SharedString::from(group.title)),
            );
            for suffix in &group.suffixes {
                let row = match suffix {
                    Suffix::Switch(sw) => {
                        let on = state.active.contains(sw.key);
                        let color = if on { theme::added() } else { theme::dim() };
                        div()
                            .flex()
                            .gap_2()
                            .pl_2()
                            .child(key_chip(sw.key))
                            .child(
                                div()
                                    .text_color(color)
                                    .child(SharedString::from(format!(
                                        "{}  {}",
                                        sw.arg, sw.description
                                    ))),
                            )
                    }
                    Suffix::Action(a) => div()
                        .flex()
                        .gap_2()
                        .pl_2()
                        .child(key_chip(a.key))
                        .child(SharedString::from(a.description)),
                };
                panel = panel.child(row);
            }
        }
        panel
    }

    fn render_row(&self, ix: usize) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected && row.selectable;
        let in_region = self
            .visual_range()
            .is_some_and(|(lo, hi)| ix >= lo && ix <= hi);

        let mut el = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(18.0))
            .w_full()
            .pl(px(8.0 + row.indent as f32 * 16.0));
        if in_region {
            el = el.bg(theme::visual());
        }
        if selected {
            el = el.bg(theme::selection());
        }

        match &row.kind {
            RowKind::Plain { text, color } => {
                el.text_color(*color).child(SharedString::from(text.clone()))
            }
            RowKind::Section {
                title,
                count,
                expanded,
            } => el.text_color(theme::section()).child(SharedString::from(format!(
                "{} {title} ({count})",
                triangle(*expanded)
            ))),
            RowKind::File {
                code,
                code_color,
                label,
                expanded,
            } => {
                let tri = match expanded {
                    Some(e) => triangle(*e),
                    None => " ",
                };
                el.child(SharedString::from(tri))
                    .child(
                        div()
                            .w(px(20.0))
                            .text_color(*code_color)
                            .child(SharedString::from(code.clone())),
                    )
                    .child(SharedString::from(label.clone()))
            }
            RowKind::HunkHeader { text } => {
                el.text_color(theme::hunk()).child(SharedString::from(text.clone()))
            }
            RowKind::Diff { text, color } => {
                el.text_color(*color).child(SharedString::from(text.clone()))
            }
        }
        .into_any_element()
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focus.focus(window);
            self.focused_once = true;
        }

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(px(13.0))
            .font_family("Menlo")
            .flex()
            .flex_col()
            // The list takes the flexible space; the status bar (added below)
            // sits beneath it, so showing the bar never shifts content down.
            .child(
                uniform_list("rows", count, move |range, _window, cx| {
                    let this = view.read(cx);
                    range.map(|ix| this.render_row(ix)).collect::<Vec<_>>()
                })
                .track_scroll(self.scroll.clone())
                .w_full()
                .flex_grow()
                .py_2()
                .px_2(),
            );

        if let Some(state) = &self.transient {
            root = root.child(self.render_transient(state));
        } else if let Some((prompt, _)) = &self.confirm {
            root = root.child(status_bar(prompt.clone(), theme::banner(), theme::modified()));
        } else if self.visual.is_some() {
            root = root.child(status_bar(
                "-- VISUAL --   s stage · u unstage · x discard · v/esc cancel".to_string(),
                theme::visual(),
                theme::fg(),
            ));
        } else if let Some(msg) = &self.status_message {
            root = root.child(status_bar(msg.clone(), theme::panel(), theme::fg()));
        }

        root
    }
}

// --- Small row/value helpers ---------------------------------------------

fn plain(text: impl Into<String>, color: gpui::Rgba) -> Row {
    Row {
        indent: 0,
        selectable: true,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: text.into(),
            color,
        },
    }
}

fn message(text: &str) -> Row {
    Row {
        indent: 2,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: text.to_string(),
            color: theme::dim(),
        },
    }
}

fn spacer() -> Row {
    Row {
        indent: 0,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: String::new(),
            color: theme::fg(),
        },
    }
}

fn triangle(expanded: bool) -> &'static str {
    if expanded {
        "▾"
    } else {
        "▸"
    }
}

fn describe_command(command: transient::Command) -> &'static str {
    use transient::Command::*;
    match command {
        Push | PushSetUpstream => "Pushing",
        Pull => "Pulling",
        Fetch | FetchAll => "Fetching",
    }
}

/// The last non-empty line of git output, for a concise status summary.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A small colored key label for transient rows.
fn key_chip(key: &str) -> gpui::Div {
    div()
        .min_w(px(20.0))
        .text_color(theme::modified())
        .child(SharedString::from(key.to_string()))
}

/// A bottom-pinned status bar row (confirm prompt or mode indicator).
fn status_bar(text: String, bg: gpui::Rgba, fg: gpui::Rgba) -> gpui::Div {
    div()
        .w_full()
        .px_2()
        .py_1()
        .border_t_1()
        .border_color(theme::bg())
        .bg(bg)
        .text_color(fg)
        .child(SharedString::from(text))
}

fn describe_discard(action: &Action) -> String {
    match action {
        Action::DiscardUntracked(p) => format!("Delete untracked {p}?  (y/n)"),
        Action::DiscardTracked(p) => format!("Discard unstaged changes to {p}?  (y/n)"),
        Action::DiscardHunk(f, _) => format!("Discard hunk in {}?  (y/n)", f.display_path()),
        Action::DiscardLines(f, _, l) => {
            format!("Discard {} line(s) in {}?  (y/n)", l.len(), f.display_path())
        }
        Action::DiscardStagedFile(p) => {
            format!("Discard staged {p} (reverts index and worktree to HEAD)?  (y/n)")
        }
        Action::DiscardStagedHunk(f, _) => {
            format!("Discard staged hunk in {} (index + worktree)?  (y/n)", f.display_path())
        }
        Action::DiscardStagedLines(f, _, l) => format!(
            "Discard {} staged line(s) in {} (index + worktree)?  (y/n)",
            l.len(),
            f.display_path()
        ),
        Action::ApplyRegion { kind, file, selections } => {
            let n: usize = selections.iter().map(|(_, l)| l.len()).sum();
            let staged = matches!(kind, RegionKind::DiscardStaged);
            format!(
                "Discard {n} line(s) in {}{}?  (y/n)",
                file.display_path(),
                if staged { " (index + worktree)" } else { "" }
            )
        }
        Action::Batch(actions) => {
            format!("Discard selection across {} files?  (y/n)", actions.len())
        }
        _ => "Discard?  (y/n)".to_string(),
    }
}

fn hunk_header_text(hunk: &magritte_core::Hunk) -> String {
    let mut text = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    );
    if !hunk.section_heading.is_empty() {
        text.push(' ');
        text.push_str(&hunk.section_heading);
    }
    text
}

fn status_code(entry: &FileEntry) -> String {
    if entry.kind == EntryKind::Untracked {
        return "??".to_string();
    }
    let glyph = |c: Change| match c {
        Change::Unmodified => ' ',
        Change::Modified => 'M',
        Change::TypeChanged => 'T',
        Change::Added => 'A',
        Change::Deleted => 'D',
        Change::Renamed => 'R',
        Change::Copied => 'C',
        Change::Unmerged => 'U',
    };
    format!("{}{}", glyph(entry.index), glyph(entry.worktree))
}

fn code_color(entry: &FileEntry) -> gpui::Rgba {
    if entry.kind == EntryKind::Untracked {
        return theme::dim();
    }
    let dominant = if entry.index != Change::Unmodified {
        entry.index
    } else {
        entry.worktree
    };
    match dominant {
        Change::Added | Change::Copied => theme::added(),
        Change::Deleted => theme::removed(),
        _ => theme::modified(),
    }
}

fn main() {
    // Optional positional arg: a path inside the repo to open (defaults to cwd).
    let arg = std::env::args().nth(1);
    if matches!(arg.as_deref(), Some("-h") | Some("--help")) {
        println!("Usage: magritte [PATH]\n\nOpen the git repository containing PATH (default: current directory).");
        return;
    }
    let start_dir = arg.map(PathBuf::from);

    Application::new().run(move |cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Magritte")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| StatusView::new(start_dir.clone(), cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}

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
    actions, div, px, size, uniform_list, AnyElement, App, AppContext, Bounds, Context, Entity,
    FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    ParentElement, Render, SharedString, Styled, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};

mod highlight;
use highlight::{FileHighlights, Span};

/// Key context for our status view, used so our `tab` binding takes precedence
/// over gpui-component Root's focus-navigation `tab`.
const STATUS_CONTEXT: &str = "MagritteStatus";

// Tab is bound by gpui-component's Root (focus nav) and so never reaches an
// on_key_down listener; we override it with an action in our key context.
actions!(magritte, [ToggleFold]);
use gpui::Subscription;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::ActiveTheme;
use magritte_core::transient::{self, Group, Suffix, Transient};
use magritte_core::{
    Change, CommitMode, DiffSource, EntryKind, FileDiff, FileEntry, LineKind, Repo, Status,
};

/// The in-app commit message editor, backed by gpui-component's multi-line
/// Input. We keep the commit context (mode + switches) alongside it.
struct CommitEditor {
    state: Entity<InputState>,
    mode: CommitMode,
    args: Vec<String>,
    /// Kept alive so the PressEnter subscription stays active.
    _sub: Subscription,
}

/// An open transient popup and the switches toggled on within it.
struct TransientState {
    def: Transient,
    active: std::collections::HashSet<String>,
    /// True after `-` is pressed, awaiting the switch letter (magit `-f`).
    pending_dash: bool,
}

impl TransientState {
    fn new(def: Transient) -> Self {
        TransientState {
            def,
            active: std::collections::HashSet::new(),
            pending_dash: false,
        }
    }
}

/// A bottom popup overlay. Both the command transients and the `?` help menu
/// are [`Transient`]s — the help just carries informational rows and dismisses
/// on any key (its keys fall through to normal handling) rather than being
/// modal. Sharing the type means they share `render_transient`.
enum Popup {
    Transient(TransientState),
    Help(Transient),
}

/// The `?` dispatch/help menu, built as a [`Transient`] of informational rows
/// so it renders through the same multi-column path as the command popups.
fn dispatch_help() -> Transient {
    let info = |keys, description| Suffix::Info(transient::Info { keys, description });
    Transient {
        title: "Help",
        groups: vec![
            Group {
                title: "Navigation",
                suffixes: vec![
                    info("j / k", "move up / down"),
                    info("gj / gk", "next / previous section"),
                    info("gg / G", "top / bottom"),
                    info("TAB", "fold / unfold"),
                    info("gr", "refresh"),
                ],
            },
            Group {
                title: "Selecting",
                suffixes: vec![
                    info("v / V", "visual line selection"),
                    info("esc", "cancel selection"),
                ],
            },
            Group {
                title: "Staging",
                suffixes: vec![
                    info("s / u", "stage / unstage at point"),
                    info("S / U", "stage / unstage all"),
                    info("x", "discard (with confirm)"),
                ],
            },
            Group {
                title: "Commands",
                suffixes: vec![
                    info("c", "commit"),
                    info("p", "push"),
                    info("F", "pull"),
                    info("f", "fetch"),
                    info("?", "this help"),
                ],
            },
        ],
    }
}

/// Resolved colors for one render, derived from gpui-component's active theme
/// so the chrome matches the Input/Kbd/Icon widgets (light or dark).
#[derive(Clone, Copy)]
struct Palette {
    bg: Hsla,
    fg: Hsla,
    dim: Hsla,
    border: Hsla,
    selection: Hsla,
    visual: Hsla,
    section: Hsla,
    hunk: Hsla,
    panel: Hsla,
    modified: Hsla,
    added: Hsla,
    removed: Hsla,
    added_bg: Hsla,
    removed_bg: Hsla,
    banner: Hsla,
}

fn with_alpha(mut color: Hsla, alpha: f32) -> Hsla {
    color.a = alpha;
    color
}


impl Palette {
    fn from_theme(cx: &App) -> Self {
        let t = cx.theme();
        Palette {
            bg: t.background,
            fg: t.foreground,
            dim: t.muted_foreground,
            border: t.border,
            selection: t.accent,
            visual: with_alpha(t.selection, 0.32),
            section: t.primary,
            hunk: t.info,
            panel: t.popover,
            modified: t.warning,
            added: t.success,
            removed: t.danger,
            added_bg: with_alpha(t.success, 0.12),
            removed_bg: with_alpha(t.danger, 0.12),
            banner: with_alpha(t.warning, 0.18),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        let g = |v: u32| gpui::rgb(v).into();
        Palette {
            bg: g(0xffffff),
            fg: g(0x1a1a1a),
            dim: g(0x8a8a8a),
            border: g(0xe2e2e2),
            selection: g(0xeaeaea),
            visual: g(0xdbe7ff),
            section: g(0x2f6feb),
            hunk: g(0x6f42c1),
            panel: g(0xf6f6f6),
            modified: g(0xb08800),
            added: g(0x1a7f37),
            removed: g(0xcf222e),
            added_bg: with_alpha(g(0x1a7f37), 0.12),
            removed_bg: with_alpha(g(0xcf222e), 0.12),
            banner: with_alpha(g(0xb08800), 0.18),
        }
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
        color: Hsla,
    },
    Section {
        title: String,
        count: usize,
        expanded: bool,
    },
    File {
        code: String,
        code_color: Hsla,
        label: String,
        expanded: Option<bool>,
    },
    HunkHeader {
        text: String,
    },
    Diff {
        kind: LineKind,
        /// Syntax-highlighted (or fallback) content runs.
        spans: Vec<Span>,
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
    /// Cached syntax highlighting per file diff, keyed like `diffs`.
    highlights: HashMap<(DiffSource, String), FileHighlights>,
    rows: Vec<Row>,
    selected: usize,
    /// Anchor row of an active visual (region) selection; `None` when off.
    /// The selection spans `min(anchor, selected)..=max(anchor, selected)`.
    visual: Option<usize>,
    generation: u64,
    pending_g: bool,
    /// An open bottom popup (command transient or help menu), or `None`.
    popup: Option<Popup>,
    /// The commit message editor, when open (takes over the window).
    editor: Option<CommitEditor>,
    /// Last operation result / progress, shown in the bottom bar.
    status_message: Option<String>,
    /// A pending destructive confirmation: (prompt, action awaiting `y`).
    confirm: Option<(String, Action)>,
    focus: FocusHandle,
    focused_once: bool,
    scroll: UniformListScrollHandle,
    /// Colors for the current theme, refreshed at the top of each render.
    palette: Palette,
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
            highlights: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            visual: None,
            generation: 0,
            pending_g: false,
            popup: None,
            editor: None,
            status_message: None,
            confirm: None,
            focus: cx.focus_handle(),
            focused_once: false,
            scroll: UniformListScrollHandle::new(),
            palette: Palette::default(),
        };
        view.refresh(cx);
        view
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.diffs.clear();
        self.highlights.clear();
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
            // Off the UI thread: load the diff and resolve the language
            // (extension/filename, falling back to a shebang sniff of the file).
            let (loaded, lang) = cx
                .background_executor()
                .spawn(async move {
                    let diff = repo.diff_path(source, &path);
                    let (head, tail) = file_head_tail(&repo.workdir().join(&path));
                    let lang = highlight::detect_language(&path, &head, &tail);
                    (diff, lang)
                })
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
                // Precompute syntax highlighting for the loaded diff.
                if let DiffState::Loaded(diff) = &state {
                    if !diff.is_binary {
                        if let Some(lang) = lang {
                            let default = cx.theme().foreground;
                            let hl = highlight::highlight_diff(diff, lang, cx, default);
                            this.highlights.insert(key.clone(), hl);
                        }
                    }
                }
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
            rows.push(plain(format!("Error: {error}"), self.palette.removed));
            self.rows = rows;
            return;
        }
        let Some(status) = &self.status else {
            rows.push(plain("Loading…", self.palette.dim));
            self.rows = rows;
            return;
        };

        let head = &status.head;
        let branch = head
            .branch
            .clone()
            .unwrap_or_else(|| "HEAD (detached)".to_string());
        rows.push(plain(format!("Head:    {branch}"), self.palette.fg));
        if let Some(upstream) = &head.upstream {
            rows.push(plain(
                format!("Push:    {upstream}  (+{} -{})", head.ahead, head.behind),
                self.palette.dim,
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
            rows.push(plain("Nothing to commit, working tree clean", self.palette.dim));
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
                    code_color: code_color(entry, &self.palette),
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
                    rows.push(message("Binary file", self.palette.dim));
                } else if diff.hunks.is_empty() {
                    rows.push(message("(no textual changes)", self.palette.dim));
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
                    let file_hl = self.highlights.get(&(source, file.path.clone()));
                    for (line_ix, line) in hunk.lines.iter().enumerate() {
                        // Use cached highlight spans if present, else a single
                        // fallback span in the default color.
                        let spans: Vec<Span> = file_hl
                            .and_then(|h| h.get(&(hunk_ix, line_ix)))
                            .cloned()
                            .unwrap_or_else(|| {
                                let color = if line.kind == LineKind::NoNewline {
                                    self.palette.dim.into()
                                } else {
                                    self.palette.fg.into()
                                };
                                vec![(line.content.clone(), color)]
                            });
                        rows.push(Row {
                            indent: 2,
                            selectable: true,
                            fold: None,
                            target: Some(Target::Line {
                                file: file.clone(),
                                hunk: hunk_ix,
                                line: line_ix,
                            }),
                            kind: RowKind::Diff {
                                kind: line.kind,
                                spans,
                            },
                        });
                    }
                }
            }
            Some(DiffState::Loading) | None => rows.push(message("Loading diff…", self.palette.dim)),
            Some(DiffState::Empty) => rows.push(message("(no changes)", self.palette.dim)),
            Some(DiffState::Failed(e)) => rows.push(message(&format!("diff failed: {e}"), self.palette.dim)),
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
        cx.notify();
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

    // --- Popups (transients + help) --------------------------------------

    fn open_transient(&mut self, def: Transient, cx: &mut Context<Self>) {
        self.popup = Some(Popup::Transient(TransientState::new(def)));
        cx.notify();
    }

    fn handle_transient_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if key == "escape" || key == "q" {
            self.popup = None;
            cx.notify();
            return;
        }
        let Some(Popup::Transient(state)) = self.popup.as_mut() else {
            return;
        };

        // Switches are toggled magit-style: `-` then the letter (e.g. -f).
        if state.pending_dash {
            state.pending_dash = false;
            let full = format!("-{key}");
            if let Some(sw) = state.def.switches().find(|s| s.key == full).map(|s| s.key.to_string()) {
                if !state.active.remove(&sw) {
                    state.active.insert(sw);
                }
            }
            cx.notify();
            return;
        }
        if key == "-" {
            state.pending_dash = true;
            cx.notify();
            return;
        }

        // Invoke an action.
        let action = state.def.action_for(key).copied();
        let switches: Vec<String> = state
            .def
            .switches()
            .filter(|s| state.active.contains(s.key))
            .map(|s| s.arg.to_string())
            .collect();
        if let Some(action) = action {
            self.popup = None;
            match action.command {
                transient::Command::CommitCreate => {
                    self.open_editor(CommitMode::Create, switches, String::new(), window, cx)
                }
                transient::Command::CommitAmend => {
                    let initial = self.head_message();
                    self.open_editor(CommitMode::Amend, switches, initial, window, cx)
                }
                transient::Command::CommitReword => {
                    let initial = self.head_message();
                    self.open_editor(CommitMode::Reword, switches, initial, window, cx)
                }
                _ => self.run_command(action.command, switches, cx),
            }
        }
    }

    fn head_message(&self) -> String {
        self.repo
            .as_ref()
            .and_then(|r| r.head_message().ok())
            .unwrap_or_default()
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

    // --- Commit message editor -------------------------------------------

    fn open_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Return inserts a newline; Cmd/Ctrl+Return submits (reported as a
        // PressEnter with secondary=true).
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .submit_on_enter(false)
                .default_value(initial)
        });
        let sub = cx.subscribe_in(&state, window, |this, _state, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: true, .. } = ev {
                this.submit_editor(window, cx);
            }
        });
        // Focus the input so typing goes straight into it.
        state.read(cx).focus_handle(cx).focus(window, cx);
        self.editor = Some(CommitEditor {
            state,
            mode,
            args,
            _sub: sub,
        });
        cx.notify();
    }

    /// Capture-phase handler: Escape cancels the editor. (Enter is consumed by
    /// the Input as a bound action and never reaches here — commit is driven by
    /// the PressEnter subscription instead.)
    fn on_capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_none() {
            return;
        }
        if event.keystroke.key == "escape" {
            cx.stop_propagation();
            self.cancel_editor(window, cx);
        }
    }

    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.as_ref() else {
            return;
        };
        let text = ed.state.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.status_message = Some("Commit message is empty".to_string());
            cx.notify();
            return;
        }
        let ed = self.editor.take().unwrap();
        self.focus.focus(window, cx);
        // Drop the trailing newline the submit keystroke inserted.
        self.run_commit(text.trim_end().to_string(), ed.mode, ed.args, cx);
    }

    fn run_commit(&mut self, message: String, mode: CommitMode, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.status_message = Some("Committing…".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.commit(&message, mode, &args) })
                .await;
            this.update(cx, |this, cx| {
                this.status_message = Some(match result {
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

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // While the editor is open the focused Input handles keys; commit/cancel
        // are caught in the capture phase (on_capture_key).
        if self.editor.is_some() {
            return;
        }

        let key = event.keystroke.key.to_lowercase();
        let shift = event.keystroke.modifiers.shift;

        // Popup keys are case-sensitive (e.g. F pull vs f fetch), so
        // reconstruct the cased key from the shift modifier.
        let cased = if shift { key.to_uppercase() } else { key.clone() };

        // A command transient is modal — it captures every key.
        if matches!(self.popup, Some(Popup::Transient(_))) {
            self.handle_transient_key(&cased, window, cx);
            return;
        }

        // The help/dispatch popup is a transparent cheatsheet: the sub-transient
        // keys open their popups, esc/q/? close it, and any other key dismisses
        // it and then performs its normal action (falls through below).
        if matches!(self.popup, Some(Popup::Help(_))) {
            match cased.as_str() {
                "p" => return self.open_transient(transient::push_transient(), cx),
                "F" => return self.open_transient(transient::pull_transient(), cx),
                "f" => return self.open_transient(transient::fetch_transient(), cx),
                "escape" | "q" | "?" | "/" => {
                    self.popup = None;
                    cx.notify();
                    return;
                }
                _ => {
                    self.popup = None;
                    cx.notify();
                    // fall through to normal handling of this key
                }
            }
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
            // Tab is delivered via the ToggleFold action (Root binds tab), but
            // keep this as a fallback for any path that reaches on_key.
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
            // Commit transient.
            "c" => return self.open_transient(transient::commit_transient(), cx),
            // Sync transients (evil-collection magit): p push, F pull, f fetch.
            "p" => return self.open_transient(transient::push_transient(), cx),
            "f" if shift => return self.open_transient(transient::pull_transient(), cx),
            "f" => return self.open_transient(transient::fetch_transient(), cx),
            // Help / dispatch menu. "?" may arrive as "/" + shift.
            "?" => {
                self.popup = Some(Popup::Help(dispatch_help()));
                cx.notify();
                return;
            }
            "/" if shift => {
                self.popup = Some(Popup::Help(dispatch_help()));
                cx.notify();
                return;
            }
            _ => return,
        }
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Render a popup (command transient or the `?` help menu) as a bottom
    /// panel. `state` is `None` for the help menu, which has no toggled
    /// switches and no pending-dash prefix.
    fn render_transient(&self, def: &Transient, state: Option<&TransientState>) -> gpui::Div {
        let pending_dash = state.is_some_and(|s| s.pending_dash);

        // Lay the groups out as columns so we spread across horizontal space
        // instead of growing tall; columns wrap if the window is narrow.
        let mut columns = div().flex().flex_row().flex_wrap().gap_x_8().gap_y_2();
        for group in &def.groups {
            let mut col = div().flex().flex_col().gap_1().child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(group.title)),
            );
            for suffix in &group.suffixes {
                let row = match suffix {
                    Suffix::Switch(sw) => {
                        let on = state.is_some_and(|s| s.active.contains(sw.key));
                        // magit layout: key, description, then the literal git
                        // flag in parens. Only the flag itself dims (off) or
                        // highlights in cyan + bold (on) — the parens stay a
                        // constant neutral color.
                        let flag_color = if on { self.palette.modified } else { self.palette.dim };
                        let flag = if on {
                            div().text_color(flag_color).font_weight(FontWeight::BOLD)
                        } else {
                            div().text_color(flag_color)
                        };
                        let paren = || div().text_color(self.palette.fg);
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(switch_chip(
                                sw.key,
                                self.palette.dim,
                                self.palette.removed,
                                pending_dash,
                            ))
                            .child(
                                div()
                                    .text_color(self.palette.fg)
                                    .child(SharedString::from(sw.description)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(paren().child(SharedString::from("(")))
                                    .child(flag.child(SharedString::from(sw.arg)))
                                    .child(paren().child(SharedString::from(")"))),
                            )
                    }
                    Suffix::Action(a) => div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(key_chip(a.key, self.palette.dim))
                        .child(SharedString::from(a.description)),
                    // A reference row: one or more keycaps then a description.
                    Suffix::Info(i) => div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.key_tokens(i.keys))
                        .child(
                            div()
                                .text_color(self.palette.fg)
                                .child(SharedString::from(i.description)),
                        ),
                };
                col = col.child(row);
            }
            columns = columns.child(col);
        }

        div()
            .w_full()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .py_2()
            .px_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(self.palette.section)
                    .child(SharedString::from(def.title)),
            )
            .child(columns)
    }

    /// Render a whitespace-separated key spec (e.g. `gg / G`) as keycaps with
    /// any `/` separators kept as plain text between them.
    fn key_tokens(&self, keys: &str) -> gpui::Div {
        let mut row = div().flex().items_center().gap_1();
        for token in keys.split_whitespace() {
            row = if token == "/" {
                row.child(div().text_color(self.palette.dim).child(SharedString::from("/")))
            } else {
                row.child(key_chip(token, self.palette.dim))
            };
        }
        row
    }

    /// Render the commit message editor: a header, the editable text with a
    /// caret, all filling the window.
    fn render_editor(&self, ed: &CommitEditor) -> gpui::Div {
        let title = match ed.mode {
            CommitMode::Create => "Commit message",
            CommitMode::Amend => "Amend commit",
            CommitMode::Reword => "Reword commit",
        };

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .w_full()
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from(title)),
                    )
                    .child(key_chip("cmd-enter", self.palette.dim))
                    .child(div().text_color(self.palette.dim).child(SharedString::from("commit")))
                    .child(key_chip("enter", self.palette.dim))
                    .child(div().text_color(self.palette.dim).child(SharedString::from("newline")))
                    .child(key_chip("esc", self.palette.dim))
                    .child(div().text_color(self.palette.dim).child(SharedString::from("cancel"))),
            )
            .child(div().flex_grow(1.0).w_full().child(Input::new(&ed.state).h_full()))
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
            el = el.bg(self.palette.visual);
        }
        if selected {
            el = el.bg(self.palette.selection);
        }

        match &row.kind {
            RowKind::Plain { text, color } => {
                el.text_color(*color).child(SharedString::from(text.clone()))
            }
            RowKind::Section {
                title,
                count,
                expanded,
            } => el.child(chevron(*expanded, self.palette.dim)).child(
                div()
                    .text_color(self.palette.section)
                    .child(SharedString::from(format!("{title} ({count})"))),
            ),
            RowKind::File {
                code,
                code_color,
                label,
                expanded,
            } => {
                let lead = match expanded {
                    Some(e) => chevron(*e, self.palette.dim).into_any_element(),
                    None => div().w(px(14.0)).into_any_element(),
                };
                el.child(lead)
                    .child(
                        div()
                            .w(px(20.0))
                            .text_color(*code_color)
                            .child(SharedString::from(code.clone())),
                    )
                    .child(SharedString::from(label.clone()))
            }
            RowKind::HunkHeader { text } => {
                el.text_color(self.palette.hunk).child(SharedString::from(text.clone()))
            }
            RowKind::Diff { kind, spans } => {
                let (sign, sign_color, tint) = match kind {
                    LineKind::Added => ('+', self.palette.added, Some(self.palette.added_bg)),
                    LineKind::Removed => ('-', self.palette.removed, Some(self.palette.removed_bg)),
                    _ => (' ', self.palette.dim, None),
                };
                // Add/remove background tint, unless the row is selected/in-region.
                if let Some(t) = tint {
                    if !selected && !in_region {
                        el = el.bg(t);
                    }
                }
                // Sign + syntax-highlighted content as adjacent runs (no gap).
                let mut line = div()
                    .flex()
                    .child(div().text_color(sign_color).child(SharedString::from(sign.to_string())));
                for (text, color) in spans {
                    line = line.child(div().text_color(*color).child(SharedString::from(text.clone())));
                }
                el.child(line)
            }
        }
        .into_any_element()
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focus.focus(window, cx);
            self.focused_once = true;
        }
        self.palette = Palette::from_theme(cx);

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .key_context(STATUS_CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleFold, _window, cx| {
                if this.popup.is_none() && this.editor.is_none() {
                    this.toggle_fold(cx);
                }
            }))
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(self.palette.bg)
            .text_color(self.palette.fg)
            .text_size(px(13.0))
            .font_family("Menlo")
            .flex()
            .flex_col();

        // The commit editor takes over the whole window when open.
        if let Some(ed) = &self.editor {
            return root.child(self.render_editor(ed));
        }

        // The list takes the flexible space; the status bar (added below)
        // sits beneath it, so showing the bar never shifts content down.
        root = root.child(
            div()
                .relative()
                .w_full()
                .flex_grow(1.0)
                .child(
                    uniform_list("rows", count, move |range, _window, cx| {
                        let this = view.read(cx);
                        range.map(|ix| this.render_row(ix)).collect::<Vec<_>>()
                    })
                    .track_scroll(&self.scroll)
                    .size_full()
                    .py_2()
                    .px_2(),
                )
                .vertical_scrollbar(&self.scroll),
        );

        if let Some(popup) = &self.popup {
            root = root.child(match popup {
                Popup::Transient(state) => self.render_transient(&state.def, Some(state)),
                Popup::Help(def) => self.render_transient(def, None),
            });
        } else if let Some((prompt, _)) = &self.confirm {
            root = root.child(status_bar(
                prompt.clone(),
                self.palette.banner,
                self.palette.fg,
                self.palette.border,
            ));
        } else if self.visual.is_some() {
            root = root.child(status_bar(
                "-- VISUAL --   s stage · u unstage · x discard · v/esc cancel".to_string(),
                self.palette.visual,
                self.palette.fg,
                self.palette.border,
            ));
        } else if let Some(msg) = &self.status_message {
            root = root.child(status_bar(
                msg.clone(),
                self.palette.panel,
                self.palette.fg,
                self.palette.border,
            ));
        }

        root
    }
}

// --- Small row/value helpers ---------------------------------------------

fn plain(text: impl Into<String>, color: Hsla) -> Row {
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

fn message(text: &str, color: Hsla) -> Row {
    Row {
        indent: 2,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: text.to_string(),
            color,
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
            color: gpui::black(),
        },
    }
}

fn chevron(expanded: bool, color: Hsla) -> gpui_component::Icon {
    let name = if expanded {
        gpui_component::IconName::ChevronDown
    } else {
        gpui_component::IconName::ChevronRight
    };
    gpui_component::Icon::new(name)
        .size(px(14.0))
        .text_color(color)
}

fn describe_command(command: transient::Command) -> &'static str {
    use transient::Command::*;
    match command {
        Push | PushSetUpstream => "Pushing",
        Pull => "Pulling",
        Fetch | FetchAll => "Fetching",
        CommitCreate | CommitAmend | CommitReword | CommitExtend => "Committing",
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

/// Read the first and last ~1 KB of a file (lossy UTF-8) for modeline/shebang
/// detection. Returns empty strings on error.
fn file_head_tail(path: &std::path::Path) -> (String, String) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), String::new());
    };
    let mut head = [0u8; 1024];
    let hn = file.read(&mut head).unwrap_or(0);
    // Tail: only when the file is larger than the head we already read.
    let mut tail = [0u8; 1024];
    let tn = match file.seek(SeekFrom::End(-1024)) {
        Ok(_) => file.read(&mut tail).unwrap_or(0),
        Err(_) => 0,
    };
    (
        String::from_utf8_lossy(&head[..hn]).into_owned(),
        String::from_utf8_lossy(&tail[..tn]).into_owned(),
    )
}

/// Spell out one keystroke token as a word label. Modifier and named keys
/// become words (`Cmd`, `Enter`, `Esc`, `Tab`) rather than the macOS glyphs,
/// which render poorly in our monospace chrome. Plain letters keep their case
/// (`F` vs `f`) so case alone distinguishes the shifted key — no `Shift` shown.
fn key_word(token: &str) -> String {
    match token {
        "cmd" | "super" | "meta" => "Cmd".into(),
        "ctrl" | "control" => "Ctrl".into(),
        "alt" | "opt" | "option" => "Opt".into(),
        "shift" => "Shift".into(),
        "enter" | "return" => "Enter".into(),
        "esc" | "ESC" | "escape" => "Esc".into(),
        "tab" | "TAB" => "Tab".into(),
        "space" => "Space".into(),
        _ => token.to_string(),
    }
}

fn is_modifier(token: &str) -> bool {
    matches!(
        token,
        "cmd" | "super" | "meta" | "ctrl" | "control" | "alt" | "opt" | "option" | "shift"
    )
}

/// The keycap chip shell: a bordered, tinted rounded box. Callers fill in the
/// label (or, for switches, a multi-span label). The border makes adjacent
/// chips read as distinct keys rather than blending together.
fn chip_box(color: Hsla) -> gpui::Div {
    div()
        .px(px(5.0))
        .min_w(px(18.0))
        .flex()
        .justify_center()
        .text_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(with_alpha(color, 0.45))
        .text_color(color)
        .bg(with_alpha(color, 0.12))
}

/// A keyboard key badge: a keycap chip with a word-style label. Chords like
/// `cmd-enter` render as `Cmd+Enter`. A leading `-` (transient switch keys
/// such as `-a`) is kept verbatim, not treated as a chord.
fn key_chip(key: &str, color: Hsla) -> AnyElement {
    let parts: Vec<&str> = key.split('-').collect();
    let is_chord = parts.len() >= 2 && parts[..parts.len() - 1].iter().all(|p| is_modifier(p));
    let label = if is_chord {
        parts.iter().map(|p| key_word(p)).collect::<Vec<_>>().join("+")
    } else {
        key_word(key)
    };
    chip_box(color).child(SharedString::from(label)).into_any_element()
}

/// A switch keycap (`-a`). When a `-` prefix is pending (we're awaiting the
/// switch letter), only the dash *inside* the keycap changes color to the
/// accent, while the keycap itself stays neutral (magit's prefix feedback).
fn switch_chip(key: &str, color: Hsla, accent: Hsla, pending: bool) -> AnyElement {
    let rest = key.strip_prefix('-').unwrap_or(key);
    let dash_color = if pending { accent } else { color };
    chip_box(color)
        .child(div().text_color(dash_color).child(SharedString::from("-")))
        .child(div().text_color(color).child(SharedString::from(rest.to_string())))
        .into_any_element()
}

/// A bottom-pinned status bar row (confirm prompt or mode indicator).
fn status_bar(text: String, bg: Hsla, fg: Hsla, border: Hsla) -> gpui::Div {
    div()
        .w_full()
        .px_2()
        .py_1()
        .border_t_1()
        .border_color(border)
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

fn code_color(entry: &FileEntry, p: &Palette) -> Hsla {
    if entry.kind == EntryKind::Untracked {
        return p.dim;
    }
    let dominant = if entry.index != Change::Unmodified {
        entry.index
    } else {
        entry.worktree
    };
    match dominant {
        Change::Added | Change::Copied => p.added,
        Change::Deleted => p.removed,
        _ => p.modified,
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

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    app.run(move |cx: &mut App| {
        // Required before using any gpui-component widgets/themes.
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
        // Our tab binding, in our context, outranks Root's focus-nav tab.
        cx.bind_keys([KeyBinding::new("tab", ToggleFold, Some(STATUS_CONTEXT))]);
        cx.activate(true);

        // A reasonable default window instead of filling the whole screen;
        // centered on the active display. The user can resize freely.
        let bounds = Bounds::centered(None, size(px(1000.0), px(720.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("Magritte")),
                ..Default::default()
            }),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| StatusView::new(start_dir.clone(), cx));
                // The window's root must be a gpui-component Root (provides
                // theming, overlays, and the component context).
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}

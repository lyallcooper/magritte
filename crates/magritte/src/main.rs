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
use magritte_core::{Change, DiffSource, EntryKind, FileDiff, FileEntry, LineKind, Repo, Status};

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
}

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
    generation: u64,
    pending_g: bool,
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
            generation: 0,
            pending_g: false,
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
            let file_expanded = source.map(|s| self.expanded.contains(&FoldKey::File(s, path.clone())));
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: source.map(|s| FoldKey::File(s, path.clone())),
                kind: RowKind::File {
                    code: status_code(entry),
                    code_color: code_color(entry),
                    label,
                    expanded: file_expanded,
                },
            });

            if let (Some(src), Some(true)) = (source, file_expanded) {
                self.push_file_body(rows, src, &path);
            }
        }
    }

    fn push_file_body(&self, rows: &mut Vec<Row>, source: DiffSource, path: &str) {
        match self.diffs.get(&(source, path.to_string())) {
            Some(DiffState::Loaded(diff)) => {
                if diff.is_binary {
                    rows.push(message("Binary file"));
                } else if diff.hunks.is_empty() {
                    rows.push(message("(no textual changes)"));
                }
                for hunk in &diff.hunks {
                    rows.push(Row {
                        indent: 2,
                        selectable: true,
                        fold: None,
                        kind: RowKind::HunkHeader {
                            text: hunk_header_text(hunk),
                        },
                    });
                    for line in &hunk.lines {
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

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.to_lowercase();
        let shift = event.keystroke.modifiers.shift;

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
            _ => return,
        }
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn render_row(&self, ix: usize) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected && row.selectable;

        let mut el = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(18.0))
            .w_full()
            .pl(px(8.0 + row.indent as f32 * 16.0));
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

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(px(13.0))
            .font_family("Menlo")
            .child(
                uniform_list("rows", count, move |range, _window, cx| {
                    let this = view.read(cx);
                    range.map(|ix| this.render_row(ix)).collect::<Vec<_>>()
                })
                .track_scroll(self.scroll.clone())
                .size_full()
                .py_2()
                .px_2(),
            )
    }
}

// --- Small row/value helpers ---------------------------------------------

fn plain(text: impl Into<String>, color: gpui::Rgba) -> Row {
    Row {
        indent: 0,
        selectable: true,
        fold: None,
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

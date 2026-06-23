//! Magritte's status view: a foldable section tree with evil-style navigation,
//! act-at-point staging, transient command popups, a commit editor, and a live
//! settings screen.
//!
//! The view holds a flattened list of [`Row`]s rebuilt from the parsed status,
//! the fold state, and any lazily-loaded diffs. Rendering goes through
//! `uniform_list`, so only on-screen rows become elements — scrolling a long
//! diff stays cheap regardless of its length. Note the `Row` *model* is still
//! materialized eagerly for everything currently expanded, so the cost of
//! expanding one huge file is paid up front (magit-style collapsed defaults
//! keep that off the opening render). All git work (status + per-file diffs)
//! runs on the background executor; a generation counter drops stale results.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    actions, div, px, size, uniform_list, AnyElement, App, AppContext, Bounds, Context, Entity,
    FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent,
    Menu, MenuItem, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, UniformListScrollHandle, Window, WindowAppearance, WindowBounds, WindowOptions,
};

use gpui::prelude::FluentBuilder;

mod config;
mod debug;
mod highlight;
use highlight::{FileHighlights, Span};

/// Key context for our status view, used so our `tab` binding takes precedence
/// over gpui-component Root's focus-navigation `tab`.
const STATUS_CONTEXT: &str = "MagritteStatus";

// Tab is bound by gpui-component's Root (focus nav) and so never reaches an
// on_key_down listener; we override it with an action in our key context.
actions!(magritte, [ToggleFold, Quit, CloseWindow, OpenSettings]);
use gpui::Subscription;
use gpui_component::button::{Button, ButtonRounded, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{Select, SearchableVec, SelectEvent, SelectState};
use gpui_component::tag::Tag;
use gpui_component::{ActiveTheme, IndexPath, Sizable};
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

/// A bottom popup overlay. Both the command transients (push/commit/…) and the
/// `?` dispatch menu are [`Transient`]s rendered by `render_transient`. The
/// difference is dispatch (`Dispatch`) has no toggleable switches and its rows
/// invoke view-level commands via [`StatusView::run_dispatch`] rather than
/// `Repo::execute`, so it's a separate variant.
enum Popup {
    Transient(TransientState),
    Dispatch(Transient),
}

/// The `?` dispatch menu: a modal command transient (magit's dispatch). Each
/// row is a command invoked by its key or a click; navigation keys aren't
/// listed (they're always available, not dispatched).
fn dispatch_menu() -> Transient {
    let info = |keys, description| Suffix::Info(transient::Info { keys, description });
    Transient {
        title: "Dispatch",
        groups: vec![
            Group {
                title: "Commands",
                suffixes: vec![
                    info("c", "Commit"),
                    info("p", "Push"),
                    info("F", "Pull"),
                    info("f", "Fetch"),
                    info(",", "Settings"),
                ],
            },
            Group {
                title: "Applying changes",
                suffixes: vec![
                    info("s", "Stage"),
                    info("u", "Unstage"),
                    info("S", "Stage all"),
                    info("U", "Unstage all"),
                    info("x", "Discard"),
                ],
            },
            Group {
                title: "Navigation",
                suffixes: vec![
                    info("j", "Move down"),
                    info("k", "Move up"),
                    info("gg", "Top"),
                    info("G", "Bottom"),
                    info("gj", "Next section"),
                    info("gk", "Previous section"),
                ],
            },
            Group {
                title: "Essential",
                suffixes: vec![
                    info("tab", "Fold / unfold"),
                    info("gr", "Refresh"),
                    info("v", "Visual selection"),
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
        // Diff/status colors come from the highlight theme's git status colors
        // (created/deleted/modified → success/error/warning), not the base
        // semantic tokens: many themes (e.g. Solarized) leave the base tokens
        // muted and put the vivid git colors in the highlight block. These
        // accessors fall back to the base tokens when a theme omits them.
        let status = &t.highlight_theme.style.status;
        let added = status.success(cx);
        let removed = status.error(cx);
        let modified = status.warning(cx);
        let hunk = status.info(cx);
        Palette {
            bg: t.background,
            fg: t.foreground,
            dim: t.muted_foreground,
            border: t.border,
            selection: t.accent,
            visual: with_alpha(t.selection, 0.32),
            section: t.primary,
            hunk,
            panel: t.popover,
            modified,
            added,
            removed,
            added_bg: with_alpha(added, 0.12),
            removed_bg: with_alpha(removed, 0.12),
            banner: with_alpha(modified, 0.18),
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

/// Fixed row height (points) so `uniform_list` can virtualize every row.
const ROW_HEIGHT: f32 = 18.0;
/// Left padding (points) added per indent level.
const INDENT_STEP: f32 = 16.0;
/// Base left padding (points) before any indent.
const ROW_PAD_LEFT: f32 = 8.0;
/// Fixed width (points) of the status-word column on file rows.
const STATUS_COL_WIDTH: f32 = 84.0;
/// Group name shared by keycap+label button rows so hovering a row highlights
/// only its label (via `group_hover`), not its keycap.
const KBD_ROW_GROUP: &str = "kbd-row";

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

/// Selected changed lines within one file, grouped by hunk: each entry is
/// `(hunk index, line indices within that hunk)`.
type HunkSelections = Vec<(usize, Vec<usize>)>;

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
        selections: HunkSelections,
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
        /// Humanized status word ("modified", "new file", …); empty for untracked.
        status: String,
        status_color: Hsla,
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

/// The appearance options, in display order. Label paired with config value.
const APPEARANCE_OPTIONS: [(&str, &str); 3] =
    [("Auto (system)", "auto"), ("Light", "light"), ("Dark", "dark")];

/// The live settings screen, built from gpui-component `Select` dropdowns (each
/// with built-in mouse + keyboard handling). Tab cycles focus between them;
/// confirming a selection applies it live.
struct SettingsState {
    appearance: Entity<SelectState<Vec<SharedString>>>,
    light_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    dark_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    font: Entity<SelectState<SearchableVec<SharedString>>>,
    /// Which dropdown Tab focuses next (0=appearance,1=light,2=dark,3=font).
    focus_ix: usize,
    /// Kept alive so the Confirm subscriptions stay active.
    _subs: Vec<Subscription>,
}

/// All monospace font families available to the text system, sorted.
/// Membership is decided by the font's own monospace trait as reported by the
/// OS (CoreText's `kCTFontMonoSpaceTrait`) rather than by measuring glyph
/// widths — the trait reliably excludes symbol fonts (e.g. Webdings) and
/// proportional CJK fonts whose Latin glyphs happen to be equal-width, both of
/// which fooled the old width heuristic.
fn monospace_font_names(cx: &App) -> Vec<SharedString> {
    let mut names: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| is_monospace_font(name))
        .map(SharedString::from)
        .collect();
    names.sort_by_key(|f| f.to_lowercase());
    names.dedup();
    names
}

/// Whether a font family declares the monospace trait to the OS font system.
#[cfg(target_os = "macos")]
fn is_monospace_font(name: &str) -> bool {
    use core_text::font::new_from_name;
    use core_text::font_descriptor::SymbolicTraitAccessors;
    new_from_name(name, 12.0)
        .map(|font| font.symbolic_traits().is_monospace())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn is_monospace_font(_name: &str) -> bool {
    // No OS trait query wired up off macOS (not a current target).
    true
}

/// Whether the system appearance is currently dark.
fn system_is_dark(cx: &App) -> bool {
    matches!(
        cx.window_appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// The effective theme mode for a config: forced light/dark, or the system's
/// appearance when set to "auto".
fn effective_mode(cfg: &config::Config, cx: &App) -> gpui_component::ThemeMode {
    match cfg.appearance.as_str() {
        "light" => gpui_component::ThemeMode::Light,
        "dark" => gpui_component::ThemeMode::Dark,
        _ if system_is_dark(cx) => gpui_component::ThemeMode::Dark,
        _ => gpui_component::ThemeMode::Light,
    }
}

/// Point the theme's light/dark slots at the config's chosen themes and switch
/// to the effective mode (following the system when appearance is "auto").
fn apply_appearance(cfg: &config::Config, cx: &mut App) {
    let registry = gpui_component::ThemeRegistry::global(cx);
    let light = registry.themes().get(cfg.light_theme()).cloned();
    let dark = registry.themes().get(cfg.dark_theme()).cloned();
    {
        let theme = gpui_component::Theme::global_mut(cx);
        if let Some(t) = light {
            theme.light_theme = t;
        }
        if let Some(t) = dark {
            theme.dark_theme = t;
        }
    }
    gpui_component::Theme::change(effective_mode(cfg, cx), None, cx);
}

/// Label for the font-picker entry that follows the OS default monospace.
const SYSTEM_FONT_LABEL: &str = "System Default";

/// The platform's system monospace UI font. On macOS this is the SF Mono-based
/// `.AppleSystemUIFontMonospaced` (what `NSFont.monospacedSystemFont` returns),
/// which Apple does not expose as a normal selectable font family.
#[cfg(target_os = "macos")]
fn system_mono_font(_cx: &App) -> SharedString {
    SharedString::from(".AppleSystemUIFontMonospaced")
}
#[cfg(not(target_os = "macos"))]
fn system_mono_font(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

/// The monospace font family to render with: the user's configured choice, or
/// the platform's system monospace UI font when unset (the "System Default"
/// font-picker entry, stored as an empty config value so it stays adaptive).
fn resolve_font(cfg: &config::Config, cx: &App) -> SharedString {
    if cfg.font.is_empty() {
        system_mono_font(cx)
    } else {
        SharedString::from(cfg.font.clone())
    }
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
    /// Detected highlight language per file diff, kept so highlighting can be
    /// recomputed on a theme change without re-reading files off the UI thread.
    diff_langs: HashMap<(DiffSource, String), &'static str>,
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
    /// The live settings screen, when open (takes over the window).
    settings: Option<SettingsState>,
    /// The monospace font family used for all chrome, set via settings.
    font: SharedString,
    /// The loaded user config (theme/appearance/font), kept so we can re-apply
    /// on config-file edits or system appearance changes.
    config: config::Config,
    /// Cached list of monospace font families (computed on first settings open).
    mono_fonts: Vec<SharedString>,
    /// Last operation result / progress, shown in the bottom bar.
    status_message: Option<String>,
    /// A pending destructive confirmation: (prompt, action awaiting `y`).
    confirm: Option<(String, Action)>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Colors for the current theme, refreshed at the top of each render.
    palette: Palette,
}

impl StatusView {
    fn new(start_dir: Option<PathBuf>, config: config::Config, cx: &mut Context<Self>) -> Self {
        let root = start_dir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo = Repo::discover(&root).ok();
        let font = resolve_font(&config, cx);

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
            diff_langs: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            visual: None,
            generation: 0,
            pending_g: false,
            popup: None,
            editor: None,
            settings: None,
            font,
            config,
            mono_fonts: Vec::new(),
            status_message: None,
            confirm: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            palette: Palette::default(),
        };
        view.refresh(cx);
        view.watch_config(cx);
        view
    }

    /// Poll for external config-file edits and system light/dark changes, and
    /// re-apply live. Cheap (a stat + an appearance read once a second) and
    /// dependency-free; the in-app settings screen is the other path.
    fn watch_config(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut last_mtime = config::mtime();
            let mut last_dark = cx.update(|cx| system_is_dark(cx));
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let now_mtime = config::mtime();
                let config_changed = now_mtime != last_mtime;
                if config_changed {
                    last_mtime = now_mtime;
                }
                let now_dark = cx.update(|cx| system_is_dark(cx));
                let appearance_changed = now_dark != last_dark;
                last_dark = now_dark;
                if !config_changed && !appearance_changed {
                    continue;
                }
                let cfg = config_changed.then(config::load);
                let updated = this.update(cx, |view, cx| {
                    match cfg {
                        // Skip a re-apply when the file's contents are unchanged
                        // (e.g. our own in-app save, or a no-op external edit).
                        Some(cfg) if cfg != view.config => view.apply_config(cfg, cx),
                        Some(_) => {}
                        // System appearance flipped; re-apply with the same config.
                        None => view.reapply_theme(cx),
                    }
                });
                if updated.is_err() {
                    break; // window closed
                }
            }
        })
        .detach();
    }

    /// Adopt a freshly-loaded config: store it, re-apply theme/appearance, and
    /// update the font.
    fn apply_config(&mut self, cfg: config::Config, cx: &mut Context<Self>) {
        self.config = cfg;
        self.font = resolve_font(&self.config, cx);
        self.reapply_theme(cx);
    }

    /// Re-apply the current config's theme and refresh everything that bakes in
    /// theme colors. Diff/status/plain row colors are stored in the `Row` model
    /// and the syntax-highlight cache is theme-derived, so a live theme switch
    /// must rebuild both — otherwise the screen keeps the old theme's colors.
    fn reapply_theme(&mut self, cx: &mut Context<Self>) {
        apply_appearance(&self.config, cx);
        self.palette = Palette::from_theme(cx);
        self.recompute_highlights(cx);
        self.rebuild_rows();
        cx.notify();
    }

    /// Recompute the syntax-highlight cache for every loaded diff against the
    /// current theme. Reuses the languages detected at load time, so no files
    /// are re-read.
    fn recompute_highlights(&mut self, cx: &mut Context<Self>) {
        if self.highlights.is_empty() && self.diff_langs.is_empty() {
            return;
        }
        let default = cx.theme().foreground;
        let mut next = HashMap::new();
        for (key, state) in &self.diffs {
            let DiffState::Loaded(diff) = state else { continue };
            if diff.is_binary {
                continue;
            }
            if let Some(&lang) = self.diff_langs.get(key) {
                next.insert(key.clone(), highlight::highlight_diff(diff, lang, cx, default));
            }
        }
        self.highlights = next;
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.diffs.clear();
        self.highlights.clear();
        self.diff_langs.clear();
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
                if let Some(lang) = lang {
                    this.diff_langs.insert(key.clone(), lang);
                }
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

        if status.is_clean() {
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
                    status: status_label(entry, id),
                    status_color: status_color(entry, id, &self.palette),
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
                                    self.palette.dim
                                } else {
                                    self.palette.fg
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
        let mut groups: Vec<(FileRef, HunkSelections)> = Vec::new();
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
                // Use status_message, not `error`: refresh() clears `error` at
                // its top, so a failure stored there would never be shown.
                match result {
                    Ok(()) => this.status_message = None,
                    Err(e) => this.status_message = Some(format!("error: {e}")),
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

    /// Open the live settings screen: four `Select` dropdowns (appearance,
    /// light theme, dark theme, font), each applying its selection immediately.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut theme_names: Vec<SharedString> = gpui_component::ThemeRegistry::global(cx)
            .sorted_themes()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        theme_names.sort_by_key(|n| n.to_lowercase());

        let row = |ix: usize| Some(IndexPath::default().row(ix));
        let appearance_ix = APPEARANCE_OPTIONS
            .iter()
            .position(|(_, v)| *v == self.config.appearance)
            .unwrap_or(0);
        let pos = |list: &[SharedString], want: &str| {
            list.iter().position(|n| n.as_ref() == want).unwrap_or(0)
        };
        let light_ix = pos(&theme_names, self.config.light_theme());
        let dark_ix = pos(&theme_names, self.config.dark_theme());

        if self.mono_fonts.is_empty() {
            self.mono_fonts = monospace_font_names(cx);
        }
        // Lead with a "System Default" entry (maps to an empty config value, so
        // it follows the OS monospace); the rest are concrete families.
        let mut font_items: Vec<SharedString> = vec![SharedString::from(SYSTEM_FONT_LABEL)];
        font_items.extend(self.mono_fonts.iter().cloned());
        let font_ix = if self.config.font.is_empty() {
            0
        } else {
            pos(&font_items, self.config.font.as_str())
        };

        let appearance_items: Vec<SharedString> = APPEARANCE_OPTIONS
            .iter()
            .map(|(label, _)| SharedString::from(*label))
            .collect();

        let appearance = cx.new(|cx| {
            SelectState::new(appearance_items, row(appearance_ix), &mut *window, cx)
        });
        let light_theme = cx.new(|cx| {
            SelectState::new(SearchableVec::new(theme_names.clone()), row(light_ix), &mut *window, cx)
                .searchable(true)
        });
        let dark_theme = cx.new(|cx| {
            SelectState::new(SearchableVec::new(theme_names), row(dark_ix), &mut *window, cx)
                .searchable(true)
        });
        let font = cx.new(|cx| {
            SelectState::new(SearchableVec::new(font_items), row(font_ix), &mut *window, cx)
                .searchable(true)
        });

        let subs = vec![
            cx.subscribe_in(&appearance, window, |this, _, ev: &SelectEvent<Vec<SharedString>>, _w, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    let value = APPEARANCE_OPTIONS
                        .iter()
                        .find(|(l, _)| *l == label.as_ref())
                        .map_or("auto", |(_, v)| v);
                    this.config.appearance = value.to_string();
                    this.apply_and_save(cx);
                }
            }),
            cx.subscribe_in(&light_theme, window, |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    this.config.light_theme = name.to_string();
                    this.apply_and_save(cx);
                }
            }),
            cx.subscribe_in(&dark_theme, window, |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    this.config.dark_theme = name.to_string();
                    this.apply_and_save(cx);
                }
            }),
            cx.subscribe_in(&font, window, |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    // "System Default" → empty config (adaptive system mono).
                    this.config.font = if name.as_ref() == SYSTEM_FONT_LABEL {
                        String::new()
                    } else {
                        name.to_string()
                    };
                    this.font = resolve_font(&this.config, cx);
                    this.apply_and_save(cx);
                }
            }),
        ];

        appearance.update(cx, |st, cx| st.focus(window, cx));
        self.settings = Some(SettingsState {
            appearance,
            light_theme,
            dark_theme,
            font,
            focus_ix: 0,
            _subs: subs,
        });
        cx.notify();
    }

    /// Re-apply the theme for the current config and persist it.
    fn apply_and_save(&mut self, cx: &mut Context<Self>) {
        self.reapply_theme(cx);
        config::save(&self.config);
    }

    /// Tab moves focus to the next settings dropdown. (The four dropdowns have
    /// distinct `SelectState` types, so each arm focuses its own entity.)
    fn cycle_settings_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.settings.as_mut() else {
            return;
        };
        s.focus_ix = (s.focus_ix + 1) % 4;
        match s.focus_ix {
            0 => s.appearance.clone().update(cx, |st, cx| st.focus(window, cx)),
            1 => s.light_theme.clone().update(cx, |st, cx| st.focus(window, cx)),
            2 => s.dark_theme.clone().update(cx, |st, cx| st.focus(window, cx)),
            _ => s.font.clone().update(cx, |st, cx| st.focus(window, cx)),
        }
    }

    /// Close the settings screen, persisting and returning focus to the list.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        config::save(&self.config);
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

        // While settings is open the focused Select handles keys; we only watch
        // for Esc (when no dropdown menu is open) to close the screen. Tab is
        // delivered via the ToggleFold action.
        if self.settings.is_some() {
            if key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }

        // Popup keys are case-sensitive (e.g. F pull vs f fetch), so
        // reconstruct the cased key from the shift modifier.
        let cased = if shift { key.to_uppercase() } else { key.clone() };

        // A command transient is modal — it captures every key.
        if matches!(self.popup, Some(Popup::Transient(_))) {
            self.handle_transient_key(&cased, window, cx);
            return;
        }

        // The `?` dispatch popup is modal (like magit's dispatch): a command
        // key runs that command, esc/q/? close it, other keys are ignored.
        if matches!(self.popup, Some(Popup::Dispatch(_))) {
            if self.pending_g {
                self.pending_g = false;
                match key.as_str() {
                    "r" => self.run_dispatch("gr", window, cx),
                    "g" => self.run_dispatch("gg", window, cx),
                    "j" => self.run_dispatch("gj", window, cx),
                    "k" => self.run_dispatch("gk", window, cx),
                    _ => {}
                }
                return;
            }
            match cased.as_str() {
                "escape" | "q" | "?" | "/" => {
                    self.popup = None;
                    cx.notify();
                }
                "g" => self.pending_g = true,
                k if Self::is_dispatch_key(k) => self.run_dispatch(&cased, window, cx),
                _ => {}
            }
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
            // Settings (theme + font), applied live.
            "," => {
                self.open_settings(window, cx);
                return;
            }
            // Help / dispatch menu. "?" may arrive as "/" + shift.
            "?" => {
                self.popup = Some(Popup::Dispatch(dispatch_menu()));
                cx.notify();
                return;
            }
            "/" if shift => {
                self.popup = Some(Popup::Dispatch(dispatch_menu()));
                cx.notify();
                return;
            }
            _ => return,
        }
        self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Mouse click on a transient suffix: toggle a switch, or invoke an action.
    fn click_suffix(
        &mut self,
        key: SharedString,
        is_switch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_switch {
            if let Some(Popup::Transient(state)) = self.popup.as_mut() {
                let k = key.to_string();
                if !state.active.remove(&k) {
                    state.active.insert(k);
                }
                cx.notify();
            }
        } else {
            self.handle_transient_key(&key, window, cx);
        }
    }

    /// Invoke a `?`-dispatch command (by key press or row click): close the
    /// dispatch menu and run the command, like magit's dispatch transient.
    fn run_dispatch(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        match key {
            "c" => self.open_transient(transient::commit_transient(), cx),
            "p" => self.open_transient(transient::push_transient(), cx),
            "F" => self.open_transient(transient::pull_transient(), cx),
            "f" => self.open_transient(transient::fetch_transient(), cx),
            "," => self.open_settings(window, cx),
            "s" => self.act(Op::Stage, cx),
            "S" => self.run_action(Action::StageAll, cx),
            "u" => self.act(Op::Unstage, cx),
            "U" => self.run_action(Action::UnstageAll, cx),
            "x" => self.act(Op::Discard, cx),
            "v" => {
                self.visual = if self.visual.is_some() {
                    None
                } else {
                    Some(self.selected)
                };
                cx.notify();
            }
            "tab" => self.toggle_fold(cx),
            "gr" => {
                self.refresh(cx);
                cx.notify();
            }
            // Motions: move the selection, then settle the scroll.
            motion => {
                match motion {
                    "j" => self.move_selection(1),
                    "k" => self.move_selection(-1),
                    "gg" => self.select_edge(false),
                    "G" => self.select_edge(true),
                    "gj" => self.select_section(true),
                    "gk" => self.select_section(false),
                    _ => {}
                }
                self.scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Whether `key` is a single-key `?`-dispatch command. Derived from
    /// `dispatch_menu` so the menu is the single source of truth: any row added
    /// there becomes routable here (as long as `run_dispatch` handles it). The
    /// multi-stroke entries are handled elsewhere — Tab via the ToggleFold
    /// action, and `gr`/`gg`/`gj`/`gk` via the g-prefix — so they're excluded.
    fn is_dispatch_key(key: &str) -> bool {
        if matches!(key, "tab" | "gr" | "gg" | "gj" | "gk") {
            return false;
        }
        dispatch_menu()
            .groups
            .iter()
            .flat_map(|g| &g.suffixes)
            .any(|s| matches!(s, Suffix::Info(i) if i.keys == key))
    }

    /// Render a popup (command transient or the `?` help menu) as a bottom
    /// panel. `state` is `None` for the help menu, which has no toggled
    /// switches and no pending-dash prefix.
    /// A button label that gets a background highlight only when its containing
    /// [`KBD_ROW_GROUP`] row is hovered — so mousing over a keycap+label button
    /// highlights the text, not the keycap.
    fn hover_label(&self, text: &str, color: Hsla) -> gpui::Div {
        div()
            .px_1()
            .rounded(px(3.0))
            .text_color(color)
            .group_hover(KBD_ROW_GROUP, |s| s.bg(self.palette.visual))
            .child(SharedString::from(text.to_string()))
    }

    fn render_transient(
        &self,
        def: &Transient,
        state: Option<&TransientState>,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let pending_dash = state.is_some_and(|s| s.pending_dash);

        // Lay the groups out as columns so we spread across horizontal space
        // instead of growing tall; columns wrap if the window is narrow.
        let mut columns = div().flex().flex_row().flex_wrap().gap_x_8().gap_y_2();
        for group in &def.groups {
            // items_start so each row's clickable hitbox hugs its content width
            // rather than stretching across the column (which makes clicks land
            // on the wrong row).
            let mut col = div().flex().flex_col().items_start().gap_1().child(
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
                        // highlights bold in the `modified` accent (on) — the
                        // parens stay a constant neutral color.
                        let flag_color = if on { self.palette.modified } else { self.palette.dim };
                        let flag = if on {
                            div().text_color(flag_color).font_weight(FontWeight::BOLD)
                        } else {
                            div().text_color(flag_color)
                        };
                        let paren = || div().text_color(self.palette.fg);
                        let view = view.clone();
                        let key = SharedString::from(sw.key);
                        div()
                            .id(sw.key)
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_1()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .group(KBD_ROW_GROUP)
                            .child(track_target(sw.key))
                            .child(switch_chip(
                                sw.key,
                                self.palette.dim,
                                self.palette.removed,
                                pending_dash,
                            ))
                            .child(self.hover_label(sw.description, self.palette.fg))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(paren().child(SharedString::from("(")))
                                    .child(flag.child(SharedString::from(sw.arg)))
                                    .child(paren().child(SharedString::from(")"))),
                            )
                            .on_click(move |_, window, cx: &mut App| {
                                view.update(cx, |v, vcx| {
                                    v.click_suffix(key.clone(), true, window, vcx)
                                });
                            })
                            .into_any_element()
                    }
                    Suffix::Action(a) => {
                        let view = view.clone();
                        let key = SharedString::from(a.key);
                        div()
                            .id(a.key)
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_1()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .group(KBD_ROW_GROUP)
                            .child(track_target(a.key))
                            .child(key_chip(a.key, self.palette.dim))
                            .child(self.hover_label(a.description, self.palette.fg))
                            .on_click(move |_, window, cx: &mut App| {
                                view.update(cx, |v, vcx| {
                                    v.click_suffix(key.clone(), false, window, vcx)
                                });
                            })
                            .into_any_element()
                    }
                    // A dispatch command row: keycap + label, clickable to run.
                    Suffix::Info(i) => {
                        let view = view.clone();
                        let key = SharedString::from(i.keys);
                        div()
                            .id(i.keys)
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_1()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .group(KBD_ROW_GROUP)
                            .child(track_target(i.keys))
                            .child(self.key_tokens(i.keys))
                            .child(self.hover_label(i.description, self.palette.fg))
                            .on_click(move |_, window, cx: &mut App| {
                                view.update(cx, |v, vcx| v.run_dispatch(&key, window, vcx));
                            })
                            .into_any_element()
                    }
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

    /// A clickable key hint: a keycap + label that runs `action` (the same
    /// behavior its key triggers). Lets shown keys double as mouse buttons —
    /// used by the commit editor and settings screen.
    fn key_action(
        &self,
        id: &'static str,
        key: &'static str,
        label: &'static str,
        view: &Entity<Self>,
        action: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let view = view.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id))
            .child(key_chip(key, self.palette.dim))
            .child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// Render the commit message editor: a header, the editable text with a
    /// caret, all filling the window.
    fn render_editor(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
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
                    .child(self.key_action("editor-commit", "cmd-enter", "commit", view, Self::submit_editor))
                    .child(self.key_action("editor-cancel", "esc", "cancel", view, Self::cancel_editor)),
            )
            .child(div().flex_grow(1.0).w_full().child(Input::new(&ed.state).h_full()))
    }

    /// Render the live settings screen as a form of dropdowns. The `Select`
    /// components carry their own mouse + keyboard handling; Tab moves between
    /// them, Esc closes.
    fn render_settings(&self, s: &SettingsState, view: &Entity<Self>) -> gpui::Div {
        let field = |id: &'static str, label: &str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(110.0))
                        .text_color(self.palette.dim)
                        .child(SharedString::from(label.to_string())),
                )
                .child(
                    div()
                        .relative()
                        .w(px(320.0))
                        .child(track_target(id))
                        .child(control),
                )
        };

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .w_full()
            .p_4()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_color(self.palette.section).child(SharedString::from("Settings")))
                    .child(self.key_action("settings-switch", "tab", "switch", view, Self::cycle_settings_focus))
                    .child(self.key_action("settings-close", "esc", "close", view, Self::close_settings)),
            )
            .child(field(
                "appearance",
                "Appearance",
                Select::new(&s.appearance).into_any_element(),
            ))
            .child(field(
                "light-theme",
                "Light theme",
                Select::new(&s.light_theme)
                    .search_placeholder("Search themes")
                    .into_any_element(),
            ))
            .child(field(
                "dark-theme",
                "Dark theme",
                Select::new(&s.dark_theme)
                    .search_placeholder("Search themes")
                    .into_any_element(),
            ))
            .child(field(
                "font",
                "Font",
                Select::new(&s.font)
                    .search_placeholder("Search fonts")
                    .into_any_element(),
            ))
    }

    fn render_row(&self, ix: usize, view: &Entity<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected && row.selectable;
        let clickable = row.selectable || row.fold.is_some();
        let in_region = self
            .visual_range()
            .is_some_and(|(lo, hi)| ix >= lo && ix <= hi);

        let mut el = div()
            .id(SharedString::from(format!("status-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .when(clickable, |el| el.cursor_pointer())
            .pl(px(ROW_PAD_LEFT + row.indent as f32 * INDENT_STEP));
        // In visual mode the whole region — including the current line — uses
        // the region color, so the cursor line doesn't stand out from it.
        // Otherwise the current line gets the selection accent.
        if in_region {
            el = el.bg(self.palette.visual);
        } else if selected {
            el = el.bg(self.palette.selection);
        }

        let content = match &row.kind {
            RowKind::Plain { text, color } => {
                el.text_color(*color).child(SharedString::from(text.clone()))
            }
            RowKind::Section {
                title,
                count,
                expanded,
            } => el
                .child(chevron(*expanded, self.palette.dim))
                .child(
                    div()
                        .text_color(self.palette.section)
                        .child(SharedString::from(title.clone())),
                )
                .child(
                    Tag::secondary()
                        .small()
                        .outline()
                        .child(SharedString::from(count.to_string())),
                ),
            RowKind::File {
                status,
                status_color,
                label,
                expanded,
            } => {
                let lead = match expanded {
                    Some(e) => chevron(*e, self.palette.dim).into_any_element(),
                    None => div().w(px(14.0)).into_any_element(),
                };
                let mut el = el.child(lead);
                // Only files with a status word get the fixed-width status
                // column; untracked files (no word) sit flush after the lead.
                if !status.is_empty() {
                    el = el.child(
                        div()
                            .w(px(STATUS_COL_WIDTH))
                            .text_color(*status_color)
                            .child(SharedString::from(status.clone())),
                    );
                }
                el.child(SharedString::from(label.clone()))
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
        };
        if clickable {
            let view = view.clone();
            content
                .relative()
                .child(track_target(format!("status-row-{ix}")))
                .on_click(move |_, _window, cx: &mut App| {
                    view.update(cx, |v, cx| v.click_row(ix, cx));
                })
                .into_any_element()
        } else {
            content.into_any_element()
        }
    }

    /// Mouse click on a status row: select it, and toggle its fold if foldable.
    fn click_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let foldable = row.fold.is_some();
        if row.selectable {
            self.selected = ix;
        }
        if foldable {
            self.toggle_fold(cx);
        } else {
            cx.notify();
        }
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep keyboard focus on the status view whenever the commit editor
        // (which owns its own input focus) isn't open, so keys always land —
        // including debug-channel keystrokes while the window isn't frontmost.
        if self.editor.is_none() && self.settings.is_none() && !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
        }
        self.palette = Palette::from_theme(cx);

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .key_context(STATUS_CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleFold, window, cx| {
                if this.settings.is_some() {
                    this.cycle_settings_focus(window, cx);
                } else if matches!(this.popup, Some(Popup::Dispatch(_))) {
                    this.run_dispatch("tab", window, cx);
                } else if this.popup.is_none() && this.editor.is_none() {
                    this.toggle_fold(cx);
                }
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, cx| {
                // Quit when closing the last window (no windowless lingering).
                let last = cx.windows().len() <= 1;
                window.remove_window();
                if last {
                    cx.quit();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                if this.editor.is_none() && this.popup.is_none() && this.settings.is_none() {
                    this.open_settings(window, cx);
                }
            }))
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(self.palette.bg)
            .text_color(self.palette.fg)
            .text_size(px(13.0))
            .font_family(self.font.clone())
            .flex()
            .flex_col();

        // The settings screen and commit editor each take over the window.
        if let Some(s) = &self.settings {
            return root.child(self.render_settings(s, &view));
        }
        if let Some(ed) = &self.editor {
            return root.child(self.render_editor(ed, &view));
        }

        // The list takes the flexible space; the status bar (added below)
        // sits beneath it, so showing the bar never shifts content down.
        // While a popup is open, clicking the list area (anywhere outside the
        // bottom panel) dismisses it. The panel is a sibling, so a click on it
        // never reaches this handler.
        let popup_open = self.popup.is_some();
        root = root.child(
            div()
                .id("list-area")
                .relative()
                .w_full()
                .flex_grow(1.0)
                .when(popup_open, |el| {
                    el.on_click(cx.listener(|this, _, _window, cx| {
                        this.popup = None;
                        cx.notify();
                    }))
                })
                .child(
                    uniform_list("rows", count, {
                        let view = view.clone();
                        move |range, _window, cx| {
                            let this = view.read(cx);
                            range.map(|ix| this.render_row(ix, &view)).collect::<Vec<_>>()
                        }
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
                Popup::Transient(state) => self.render_transient(&state.def, Some(state), &view),
                Popup::Dispatch(def) => self.render_transient(def, None, &view),
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

        // A floating "?" button (bottom-right) opens the dispatch menu — a
        // mouse affordance for discovering commands. Hidden while a popup is up.
        if self.popup.is_none() {
            root = root.child(
                div()
                    .absolute()
                    .bottom_3()
                    .right_4()
                    .child(track_target("dispatch-help"))
                    .child(
                        Button::new("dispatch-help")
                            .label("?")
                            .ghost()
                            .rounded(ButtonRounded::Size(px(14.0)))
                            .w(px(28.0))
                            .h(px(28.0))
                            .tooltip("Dispatch (?)")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.popup = Some(Popup::Dispatch(dispatch_menu()));
                                cx.notify();
                            })),
                    ),
            );
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
        "enter" | "return" => "Return".into(),
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

/// A transparent overlay that records its element's on-screen center for the
/// debug `click-id` command (no-op unless debug mode is on). Add as a child of
/// a `.relative()` clickable element so synthetic tests can click it by id.
fn track_target(id: impl Into<SharedString>) -> impl IntoElement {
    let id = id.into();
    gpui::canvas(
        move |bounds, _, _| {
            debug::record_target(
                &id,
                bounds.origin.x.as_f32() + bounds.size.width.as_f32() / 2.0,
                bounds.origin.y.as_f32() + bounds.size.height.as_f32() / 2.0,
            );
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
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

/// The change relevant to a file within a given section: a staged row reflects
/// the index status, everything else the worktree status.
fn section_change(entry: &FileEntry, section: SectionId) -> Change {
    match section {
        SectionId::Staged => entry.index,
        _ => entry.worktree,
    }
}

/// A human-readable status word (magit-style) for a file in a section, e.g.
/// "modified", "new file", "deleted". Untracked files carry no word — the
/// section header already says "Untracked files".
fn status_label(entry: &FileEntry, section: SectionId) -> String {
    if entry.kind == EntryKind::Untracked {
        // No status word — the "Untracked files" header already says it, and
        // the filename sits flush rather than tabbed past an empty column.
        return String::new();
    }
    match section_change(entry, section) {
        Change::Unmodified => "",
        Change::Modified => "modified",
        Change::TypeChanged => "typechange",
        Change::Added => "new file",
        Change::Deleted => "deleted",
        Change::Renamed => "renamed",
        Change::Copied => "copied",
        Change::Unmerged => "conflicted",
    }
    .to_string()
}

fn status_color(entry: &FileEntry, section: SectionId, p: &Palette) -> Hsla {
    if entry.kind == EntryKind::Untracked {
        return p.added;
    }
    match section_change(entry, section) {
        Change::Added | Change::Copied => p.added,
        Change::Deleted => p.removed,
        _ => p.modified,
    }
}

/// gpui-component's bundled theme sets, embedded at compile time. Each file is
/// a `ThemeSet` containing one or more light/dark `ThemeConfig`s; loading them
/// makes every theme selectable from the registry by name.
const BUNDLED_THEMES: &[&str] = &[
    include_str!("../themes/adventure.json"),
    include_str!("../themes/alduin.json"),
    include_str!("../themes/asciinema.json"),
    include_str!("../themes/aurora.json"),
    include_str!("../themes/ayu.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/everforest.json"),
    include_str!("../themes/fahrenheit.json"),
    include_str!("../themes/flexoki.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/harper.json"),
    include_str!("../themes/hybrid.json"),
    include_str!("../themes/jellybeans.json"),
    include_str!("../themes/kibble.json"),
    include_str!("../themes/macos-classic.json"),
    include_str!("../themes/matrix.json"),
    include_str!("../themes/mellifluous.json"),
    include_str!("../themes/molokai.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/spaceduck.json"),
    include_str!("../themes/tokyonight.json"),
    include_str!("../themes/twilight.json"),
];

/// Load every bundled theme set into the registry so all themes are available.
fn register_bundled_themes(cx: &mut App) {
    let registry = gpui_component::ThemeRegistry::global_mut(cx);
    for set in BUNDLED_THEMES {
        if let Err(e) = registry.load_themes_from_str(set) {
            eprintln!("magritte: failed to load a bundled theme set: {e}");
        }
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
        register_bundled_themes(cx);
        // Apply the saved appearance/themes. Theme::change first ensures the
        // Theme global exists so apply_appearance can set its slots.
        let cfg = config::load();
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
        apply_appearance(&cfg, cx);
        // Standard macOS app shortcuts. Quit is global; Close Window runs on
        // the focused view (so it has a Window to remove).
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.bind_keys([
            // Our tab binding, in our context, outranks Root's focus-nav tab.
            KeyBinding::new("tab", ToggleFold, Some(STATUS_CONTEXT)),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, Some(STATUS_CONTEXT)),
            KeyBinding::new("cmd-,", OpenSettings, Some(STATUS_CONTEXT)),
        ]);
        cx.set_menus(vec![
            Menu::new("Magritte").items([
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit Magritte", Quit),
            ]),
            Menu::new("File").items([MenuItem::action("Close Window", CloseWindow)]),
        ]);
        // Closing the last window (red traffic light included) quits the app.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
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
            let window = cx
                .open_window(options, |window, cx| {
                    let view = cx.new(|cx| StatusView::new(start_dir.clone(), cfg.clone(), cx));
                    // The window's root must be a gpui-component Root (provides
                    // theming, overlays, and the component context).
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
                .expect("failed to open window");
            // Start the debug control channel (no-op unless MAGRITTE_DEBUG_DIR is set).
            cx.update(|cx| debug::init(window.into(), cx));
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind, index: Change, worktree: Change) -> FileEntry {
        FileEntry {
            path: "f".into(),
            orig_path: None,
            kind,
            index,
            worktree,
        }
    }

    #[test]
    fn status_label_humanizes_per_section() {
        // A staged row reflects the index status; unstaged reflects the worktree.
        let staged_add = entry(EntryKind::Tracked, Change::Added, Change::Unmodified);
        assert_eq!(status_label(&staged_add, SectionId::Staged), "new file");

        let modified = entry(EntryKind::Tracked, Change::Unmodified, Change::Modified);
        assert_eq!(status_label(&modified, SectionId::Unstaged), "modified");

        let deleted = entry(EntryKind::Tracked, Change::Unmodified, Change::Deleted);
        assert_eq!(status_label(&deleted, SectionId::Unstaged), "deleted");

        let conflicted = entry(EntryKind::Unmerged, Change::Unmodified, Change::Unmerged);
        assert_eq!(status_label(&conflicted, SectionId::Unstaged), "conflicted");
    }

    #[test]
    fn untracked_files_carry_no_status_word() {
        let untracked = entry(EntryKind::Untracked, Change::Unmodified, Change::Modified);
        assert_eq!(status_label(&untracked, SectionId::Untracked), "");
    }

    #[test]
    fn is_dispatch_key_matches_single_key_menu_rows() {
        // Single-key commands route; multi-stroke / g-prefix entries don't.
        assert!(StatusView::is_dispatch_key("c"));
        assert!(StatusView::is_dispatch_key("s"));
        assert!(StatusView::is_dispatch_key("G"));
        assert!(!StatusView::is_dispatch_key("tab"));
        assert!(!StatusView::is_dispatch_key("gg"));
        assert!(!StatusView::is_dispatch_key("gr"));
        assert!(!StatusView::is_dispatch_key("z")); // not in the menu
    }
}

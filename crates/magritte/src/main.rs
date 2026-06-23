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
    FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding,
    KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, TitlebarOptions, UniformListScrollHandle, Window,
    WindowAppearance, WindowBounds, WindowOptions,
};

use gpui::prelude::FluentBuilder;

mod config;
#[cfg(feature = "debug")]
mod debug;
mod git_action;
mod highlight;
use git_action::{describe_discard, Action, HunkSelections, Op, RegionKind};
use highlight::{FileHighlights, Span};

/// Key context for our status view, used so our `tab` binding takes precedence
/// over gpui-component Root's focus-navigation `tab`.
const STATUS_CONTEXT: &str = "MagritteStatus";

// Tab is bound by gpui-component's Root (focus nav) and so never reaches an
// on_key_down listener; we override it with an action in our key context.
actions!(magritte, [ToggleFold, Quit, CloseWindow, OpenSettings]);
// Right-click context-menu actions; dispatched by the PopupMenu and handled on
// the status view, which applies them to the row at point (selected on
// right-click) or the active visual selection.
actions!(magritte, [CtxStage, CtxUnstage, CtxDiscard]);
use gpui::Subscription;
use gpui_component::button::{Button, ButtonRounded, ButtonVariants};
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::input::{Input, InputEvent, InputState, Position};
use gpui_component::menu::ContextMenuExt;
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::switch::Switch;
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
    /// The staged diff being committed, flattened for read-only display below
    /// the message (magit's commit buffer). Empty until loaded, and left empty
    /// for reword (which commits no tree change).
    diff: Vec<CommitDiffRow>,
    diff_scroll: UniformListScrollHandle,
    /// Kept alive so the PressEnter subscription stays active.
    _sub: Subscription,
}

/// One flattened row of the commit editor's staged-diff preview.
enum CommitDiffRow {
    /// A file header (the path).
    File(String),
    /// A hunk header (`@@ … @@`).
    Hunk(String),
    /// A diff line: its kind plus syntax-highlighted (or fallback) content.
    Line { kind: LineKind, spans: Vec<Span> },
    /// A dim status note (e.g. when the staged diff couldn't be loaded).
    Note(String),
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
    /// A hunk within a file's diff: (source, path, hunk index). Unlike sections
    /// and files, hunks are expanded by default; see `collapsed_hunks`.
    Hunk(DiffSource, String, usize),
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
    Hunk {
        file: FileRef,
        hunk: usize,
    },
    Line {
        file: FileRef,
        hunk: usize,
        line: usize,
    },
}

/// A stable identity of a selected row, so the cursor can be restored to the
/// same logical place after the row list is rebuilt — rather than left at the
/// same numeric index (which may now mean something unrelated).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AnchorIdent {
    /// A top header row (Head / Push) — outside any section.
    Top,
    Section(SectionId),
    File(SectionId, String),
    Hunk(SectionId, String, usize),
    Line(SectionId, String, usize, usize),
}

impl AnchorIdent {
    fn section(&self) -> Option<SectionId> {
        match self {
            AnchorIdent::Top => None,
            AnchorIdent::Section(s)
            | AnchorIdent::File(s, _)
            | AnchorIdent::Hunk(s, _, _)
            | AnchorIdent::Line(s, _, _, _) => Some(*s),
        }
    }
}

/// A captured selection: its logical identity plus its ordinal among the
/// selectable rows of its section, used as a fallback when the identity is gone.
#[derive(Debug, Clone)]
struct SelAnchor {
    ident: AnchorIdent,
    ordinal: usize,
}

fn section_source(section: SectionId) -> Option<DiffSource> {
    match section {
        SectionId::Untracked => None,
        SectionId::Unstaged => Some(DiffSource::Unstaged),
        SectionId::Staged => Some(DiffSource::Staged),
    }
}

/// git convention: keep the commit summary within 50 columns, and wrap the
/// body at 72.
const COMMIT_TITLE_LIMIT: usize = 50;
const COMMIT_BODY_WIDTH: usize = 72;

/// Break a single line into pieces no longer than `width` characters, splitting
/// at the last space at or before the limit. A word longer than `width` (no
/// usable space) is left intact on its own piece rather than chopped.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = line;
    loop {
        if rest.chars().count() <= width {
            pieces.push(rest.to_string());
            break;
        }
        // Last space whose preceding text fits in `width` columns.
        let split = rest
            .char_indices()
            .enumerate()
            .take_while(|&(ci, _)| ci <= width)
            .filter(|&(ci, (_, ch))| ch == ' ' && ci > 0)
            .last()
            .map(|(_, (bi, _))| bi);
        match split {
            Some(bi) => {
                pieces.push(rest[..bi].to_string());
                rest = &rest[bi + 1..]; // drop the space we broke on
            }
            None => {
                pieces.push(rest.to_string()); // unbreakable long word
                break;
            }
        }
    }
    pieces
}

/// Auto-fill the commit *body* to `width`: any over-long body line is broken at
/// word boundaries. The summary (line 0) is never wrapped, and existing line
/// breaks are preserved (lines are only split, never joined) — so this is safe
/// to run on every keystroke. Because it only ever turns a space into a
/// newline, total length and character offsets are preserved.
fn auto_fill_body(text: &str, width: usize) -> String {
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if i == 0 || line.chars().count() <= width {
            out.push(line.to_string());
        } else {
            out.extend(wrap_line(line, width));
        }
    }
    out.join("\n")
}

/// Reflow the commit *body* to `width`: each blank-line-separated paragraph is
/// joined into one line then re-wrapped, collapsing runs of whitespace. The
/// summary (line 0) and blank separator lines are left untouched. Unlike
/// [`auto_fill_body`], this *re-joins* manually-broken lines, so it's an
/// explicit action rather than something to run while typing.
fn reflow_body(text: &str, width: usize) -> String {
    let mut iter = text.split('\n');
    let mut out = vec![iter.next().unwrap_or("").to_string()];
    let body: Vec<&str> = iter.collect();
    let mut i = 0;
    while i < body.len() {
        if body[i].trim().is_empty() {
            out.push(String::new());
            i += 1;
        } else {
            let start = i;
            while i < body.len() && !body[i].trim().is_empty() {
                i += 1;
            }
            let collapsed = body[start..i].join(" ");
            let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
            out.extend(wrap_line(&collapsed, width));
        }
    }
    out.join("\n")
}

/// The UTF-16 column range of the part of the summary (line 0) that overruns
/// `limit` columns, as `(start, end)` for a diagnostic `Position`. `None` when
/// the summary fits. Columns are counted in characters; the start is the
/// UTF-16 offset of the `limit`-th character.
fn title_overflow(text: &str, limit: usize) -> Option<(u32, u32)> {
    let title = text.split('\n').next().unwrap_or("");
    if title.chars().count() <= limit {
        return None;
    }
    let start: usize = title.chars().take(limit).map(char::len_utf16).sum();
    let end: usize = title.chars().map(char::len_utf16).sum();
    Some((start as u32, end as u32))
}

/// Convert a UTF-16 offset into `text` to a 0-based line/column [`Position`],
/// for restoring the cursor after a programmatic edit.
fn utf16_offset_to_position(text: &str, offset: usize) -> Position {
    let (mut line, mut col, mut seen) = (0u32, 0u32, 0usize);
    for ch in text.chars() {
        if seen >= offset {
            break;
        }
        let w = ch.len_utf16();
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += w as u32;
        }
        seen += w;
    }
    Position::new(line, col)
}

/// The repo-relative path of the file a target belongs to.
fn target_path(target: &Target) -> &str {
    match target {
        Target::File(f) => &f.path,
        Target::Hunk { file, .. } | Target::Line { file, .. } => &file.path,
    }
}

/// Which staging verbs apply to a target, by section: `(stage, unstage,
/// discard)`. Populates the right-click menu with only meaningful actions.
fn target_ops(target: &Target) -> (bool, bool, bool) {
    let section = match target {
        Target::File(f) => f.section,
        Target::Hunk { file, .. } | Target::Line { file, .. } => file.section,
    };
    match section {
        // Untracked/unstaged content can be staged or discarded.
        SectionId::Untracked | SectionId::Unstaged => (true, false, true),
        // Staged content can be unstaged or discarded.
        SectionId::Staged => (false, true, true),
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
        expanded: bool,
    },
    Diff {
        kind: LineKind,
        /// Syntax-highlighted (or fallback) content runs.
        spans: Vec<Span>,
    },
}

/// The appearance options, in display order. Label paired with config value.
const APPEARANCE_OPTIONS: [(&str, &str); 3] = [
    ("Auto (system)", "auto"),
    ("Light", "light"),
    ("Dark", "dark"),
];

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
        // Skip dot-prefixed system/fallback tokens (".SystemUIFont", ".ZedSans",
        // ".ZedMono", …). They aren't user-selectable families, and probing them
        // by name makes CoreText log "should use CTFontCreateUIFontForLanguage".
        .filter(|name| !name.starts_with('.') && is_monospace_font(name))
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

/// A pending yes/no confirmation shown in the bottom bar.
enum Confirm {
    /// A destructive staging action awaiting `y`.
    Action(Action),
    /// `c c` with nothing staged: on `y`, commit all tracked changes by
    /// opening the message editor with `--all` (the carried switches) appended.
    CommitAll(Vec<String>),
}

struct StatusView {
    /// The directory we tried to open (for error messages).
    root: PathBuf,
    repo: Option<Repo>,
    status: Option<Status>,
    error: Option<String>,
    expanded: HashSet<FoldKey>,
    /// Hunks the user has explicitly collapsed (`FoldKey::Hunk`). Hunks default
    /// to expanded, so this tracks the exceptions rather than `expanded` does.
    collapsed_hunks: HashSet<FoldKey>,
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
    /// Row where a left-button drag began, while the button is held. Dragging
    /// across rows turns into a visual selection (mouse equivalent of `v`).
    drag_anchor: Option<usize>,
    /// Set by a shift-click mouse-down so the following click extends the
    /// selection (and doesn't toggle the row's fold).
    shift_click: bool,
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
    /// A pending confirmation: (prompt, what to do on `y`).
    confirm: Option<(String, Confirm)>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Colors for the current theme, refreshed at the top of each render.
    palette: Palette,
}

impl StatusView {
    fn new(
        start_dir: Option<PathBuf>,
        config: config::Config,
        startup_warning: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            collapsed_hunks: HashSet::new(),
            diffs: HashMap::new(),
            highlights: HashMap::new(),
            diff_langs: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            visual: None,
            drag_anchor: None,
            shift_click: false,
            generation: 0,
            pending_g: false,
            popup: None,
            editor: None,
            settings: None,
            font,
            config,
            mono_fonts: Vec::new(),
            status_message: startup_warning,
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
            let DiffState::Loaded(diff) = state else {
                continue;
            };
            if diff.is_binary {
                continue;
            }
            if let Some(&lang) = self.diff_langs.get(key) {
                next.insert(
                    key.clone(),
                    highlight::highlight_diff(diff, lang, cx, default),
                );
            }
        }
        self.highlights = next;
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Capture the cursor's logical position so we can restore it after the
        // rebuild rather than leaving it at the same numeric index.
        let anchor = self.capture_anchor();
        self.generation += 1;
        let generation = self.generation;
        self.diffs.clear();
        self.highlights.clear();
        self.diff_langs.clear();
        // Hunk indices shift when the diff changes, so don't carry collapse
        // state across a refresh.
        self.collapsed_hunks.clear();
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
                this.restore_anchor(anchor);
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
                FoldKey::Section(_) | FoldKey::Hunk(..) => None,
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
                // A diff finishing load inserts rows; keep the cursor put.
                this.rebuild_preserving_selection();
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
            rows.push(plain(
                "Nothing to commit, working tree clean",
                self.palette.dim,
            ));
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
            let file_expanded =
                source.map(|s| self.expanded.contains(&FoldKey::File(s, path.clone())));
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
                    let hunk_key = FoldKey::Hunk(source, file.path.clone(), hunk_ix);
                    let hunk_expanded = !self.collapsed_hunks.contains(&hunk_key);
                    rows.push(Row {
                        indent: 2,
                        selectable: true,
                        fold: Some(hunk_key),
                        target: Some(Target::Hunk {
                            file: file.clone(),
                            hunk: hunk_ix,
                        }),
                        kind: RowKind::HunkHeader {
                            text: hunk_header_text(hunk),
                            expanded: hunk_expanded,
                        },
                    });
                    if !hunk_expanded {
                        continue;
                    }
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
            Some(DiffState::Loading) | None => {
                rows.push(message("Loading diff…", self.palette.dim))
            }
            Some(DiffState::Empty) => rows.push(message("(no changes)", self.palette.dim)),
            Some(DiffState::Failed(e)) => {
                rows.push(message(&format!("diff failed: {e}"), self.palette.dim))
            }
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
    fn select_section(&mut self, forward: bool) {
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

    fn toggle_fold(&mut self, cx: &mut Context<Self>) {
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
        // Restore the cursor to the same node: collapsing a hunk from one of its
        // lines lands on the hunk header (the line is gone, so the anchor
        // degrades to it); folding/unfolding otherwise keeps the header.
        self.rebuild_preserving_selection();
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

    // --- Selection restoration across rebuilds ---------------------------
    //
    // Rather than keep the cursor at the same numeric row index (which may mean
    // something unrelated after staging/folding), we capture the selected row's
    // logical identity before a rebuild and restore it to the same place — or,
    // if that's gone, to a sensible nearby row within the same section.

    /// The logical identity of the row at `ix`.
    fn ident_of(&self, ix: usize) -> AnchorIdent {
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
            _ => AnchorIdent::Top,
        }
    }

    /// The row indices belonging to a section: its header through the row before
    /// the next section header (or end).
    fn section_rows(&self, section: SectionId) -> Vec<usize> {
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
    fn capture_anchor(&self) -> Option<SelAnchor> {
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
    fn row_matches(&self, ix: usize, ident: &AnchorIdent) -> bool {
        self.ident_of(ix) == *ident
    }

    /// Find the best row for `ident`: exact, else progressively less specific
    /// (a missing line falls back to its hunk header, then its file row).
    fn locate_ident(&self, ident: &AnchorIdent) -> Option<usize> {
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
    fn restore_anchor(&mut self, anchor: Option<SelAnchor>) {
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
    fn rebuild_preserving_selection(&mut self) {
        let anchor = self.capture_anchor();
        self.rebuild_rows();
        self.restore_anchor(anchor);
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

    /// Whether `path` is an unmerged (conflicted) entry. Conflict resolution
    /// isn't supported in-app yet, so ordinary stage/unstage/discard is refused
    /// on these — `git add` would silently mark a conflict resolved (markers and
    /// all), and a discard could lose work.
    fn is_conflicted(&self, path: &str) -> bool {
        self.status.as_ref().is_some_and(|s| {
            s.entries
                .iter()
                .any(|e| e.path == path && e.kind == EntryKind::Unmerged)
        })
    }

    /// The first conflicted file in the current selection — the row at point, or
    /// any file touched by the visual region. Used to refuse the *whole* action
    /// (point or region) rather than silently acting on a subset.
    fn conflicted_in_selection(&self) -> Option<String> {
        let path_at = |ix: usize| {
            self.rows
                .get(ix)
                .and_then(|r| r.target.as_ref())
                .map(target_path)
        };
        match self.visual_range() {
            Some((lo, hi)) => (lo..=hi)
                .filter_map(path_at)
                .find(|p| self.is_conflicted(p))
                .map(str::to_string),
            None => path_at(self.selected)
                .filter(|p| self.is_conflicted(p))
                .map(str::to_string),
        }
    }

    /// Resolve a whole-file staging action for `op` on `f`, honoring its
    /// section (e.g. you cannot stage a file that's already staged; discard
    /// means delete for untracked, revert-to-index for unstaged, and
    /// revert-the-index for staged). Shared by point resolution and by region
    /// selections that include a file-name row.
    fn file_action(&self, f: &FileRef, op: Op) -> Option<Action> {
        Some(match (op, f.section) {
            (Op::Stage, SectionId::Untracked | SectionId::Unstaged) => {
                Action::StageFile(f.path.clone())
            }
            (Op::Unstage, SectionId::Staged) => Action::UnstageFile(f.path.clone()),
            (Op::Discard, SectionId::Untracked) => Action::DiscardUntracked(f.path.clone()),
            (Op::Discard, SectionId::Unstaged) => Action::DiscardTracked(f.path.clone()),
            (Op::Discard, SectionId::Staged) => Action::DiscardStagedFile(f.path.clone()),
            _ => return None,
        })
    }

    /// Resolve the row at point + verb into a concrete git action, if the verb
    /// is meaningful there (e.g. you cannot stage something already staged).
    fn resolve_action(&self, op: Op) -> Option<Action> {
        let target = self.rows.get(self.selected)?.target.clone()?;
        if self.is_conflicted(target_path(&target)) {
            return None;
        }
        match (op, target) {
            // Whole-file staging (any verb) — shared with region selections.
            (op, Target::File(f)) => self.file_action(&f, op),

            // Stage: from the unstaged side.
            (Op::Stage, Target::Hunk { file, hunk }) if file.section == SectionId::Unstaged => {
                Some(Action::StageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Stage, Target::Line { file, hunk, line })
                if file.section == SectionId::Unstaged =>
            {
                Some(Action::StageLines(self.diff_for(&file)?, hunk, vec![line]))
            }

            // Unstage: from the staged side.
            (Op::Unstage, Target::Hunk { file, hunk }) if file.section == SectionId::Staged => {
                Some(Action::UnstageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Unstage, Target::Line { file, hunk, line })
                if file.section == SectionId::Staged =>
            {
                Some(Action::UnstageLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                ))
            }

            // Discard hunks/lines: unstaged reverts to the index, staged
            // reverts the index (whole-file discard is handled above).
            (Op::Discard, Target::Hunk { file, hunk }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardHunk(self.diff_for(&file)?, hunk)),
                SectionId::Staged => Some(Action::DiscardStagedHunk(self.diff_for(&file)?, hunk)),
                SectionId::Untracked => None,
            },
            (Op::Discard, Target::Line { file, hunk, line }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                )),
                SectionId::Staged => Some(Action::DiscardStagedLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                )),
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

    /// Resolve a region (visual) selection into actions. Each file in the
    /// selection acts at the coarsest granularity it was selected with: a
    /// file-name row stages the whole file (even when its diff is collapsed),
    /// while selected hunks/lines act on just those. A selection spanning
    /// multiple files acts on *all* of them; parts whose section doesn't match
    /// the verb (e.g. a staged file when staging) are skipped.
    fn resolve_region_action(&self, op: Op) -> Option<Action> {
        let (lo, hi) = self.visual_range()?;

        /// The granularity at which a file in the selection should be acted on.
        /// A whole-file row wins over individual hunks/lines of the same file.
        enum Gran {
            Whole,
            Lines(HunkSelections),
        }
        fn add_lines(
            sels: &mut HunkSelections,
            hunk: usize,
            lines: impl IntoIterator<Item = usize>,
        ) {
            match sels.iter_mut().find(|(h, _)| *h == hunk) {
                Some((_, existing)) => existing.extend(lines),
                None => sels.push((hunk, lines.into_iter().collect())),
            }
        }

        // Collect per file (section+path), preserving encounter order.
        let mut files: Vec<(FileRef, Gran)> = Vec::new();
        let slot = |files: &mut Vec<(FileRef, Gran)>, f: &FileRef| -> usize {
            match files
                .iter()
                .position(|(g, _)| g.section == f.section && g.path == f.path)
            {
                Some(i) => i,
                None => {
                    files.push((f.clone(), Gran::Lines(Vec::new())));
                    files.len() - 1
                }
            }
        };
        for ix in lo..=hi {
            match self.rows.get(ix).and_then(|r| r.target.as_ref()) {
                Some(Target::File(f)) => {
                    let i = slot(&mut files, f);
                    files[i].1 = Gran::Whole;
                }
                Some(Target::Hunk { file, hunk }) => {
                    let i = slot(&mut files, file);
                    // Selecting a hunk header acts on the whole hunk: pull in
                    // every line index (the core ignores context lines).
                    if let Gran::Lines(sels) = &mut files[i].1 {
                        if let Some(h) = self
                            .diff_for(file)
                            .and_then(|d| d.hunks.into_iter().nth(*hunk))
                        {
                            add_lines(sels, *hunk, 0..h.lines.len());
                        }
                    }
                }
                Some(Target::Line { file, hunk, line }) => {
                    let i = slot(&mut files, file);
                    if let Gran::Lines(sels) = &mut files[i].1 {
                        add_lines(sels, *hunk, [*line]);
                    }
                }
                None => {}
            }
        }

        // Conflicted files in the region are handled up-front in `act` (the
        // whole action is refused), so none reach here.
        let mut actions = Vec::new();
        for (file, gran) in files {
            match gran {
                Gran::Whole => {
                    if let Some(a) = self.file_action(&file, op) {
                        actions.push(a);
                    }
                }
                Gran::Lines(mut selections) => {
                    let kind = match (op, file.section) {
                        (Op::Stage, SectionId::Unstaged) => RegionKind::Stage,
                        (Op::Unstage, SectionId::Staged) => RegionKind::Unstage,
                        (Op::Discard, SectionId::Unstaged) => RegionKind::Discard,
                        (Op::Discard, SectionId::Staged) => RegionKind::DiscardStaged,
                        _ => continue, // section doesn't match the verb
                    };
                    // A hunk header and its lines can both land in the range;
                    // collapse the duplicates.
                    for (_, lines) in &mut selections {
                        lines.sort_unstable();
                        lines.dedup();
                    }
                    if selections.iter().all(|(_, l)| l.is_empty()) {
                        continue;
                    }
                    let Some(diff) = self.diff_for(&file) else {
                        continue;
                    };
                    actions.push(Action::ApplyRegion {
                        kind,
                        file: diff,
                        selections,
                    });
                }
            }
        }

        match actions.len() {
            0 => None,
            1 => actions.pop(),
            _ => Some(Action::Batch(actions)),
        }
    }

    /// `s`/`u`/`x`: resolve and either run, or (for discard) ask to confirm.
    fn act(&mut self, op: Op, cx: &mut Context<Self>) {
        // Refuse the whole action if the selection (point or region) touches a
        // conflicted file — rather than silently acting on a subset — and say
        // why. Conflict resolution isn't supported in-app yet.
        if let Some(path) = self.conflicted_in_selection() {
            self.status_message = Some(format!("{path} is conflicted — resolve it before staging"));
            cx.notify();
            return;
        }
        let resolved = if self.visual.is_some() {
            self.resolve_region_action(op)
        } else {
            self.resolve_action(op)
        };
        let Some(action) = resolved else {
            return;
        };
        if op == Op::Discard {
            self.confirm = Some((describe_discard(&action), Confirm::Action(action)));
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

    /// Confirm a pending action (the `y` key or the confirm bar's "yes"
    /// button): run the destructive action, or proceed with a commit-all.
    fn confirm_yes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.confirm.take() {
            Some((_, Confirm::Action(action))) => self.run_action(action, cx),
            Some((_, Confirm::CommitAll(mut switches))) => {
                if !switches.iter().any(|s| s == "--all") {
                    switches.push("--all".into());
                }
                self.open_editor(CommitMode::Create, switches, window, cx);
            }
            None => {}
        }
        cx.notify();
    }

    /// Cancel a pending destructive action (any other key, or the "no" button).
    fn confirm_no(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    // Visual-mode bar buttons (mirror the s/u/x/esc keys on the region).
    fn visual_stage(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Stage, cx);
    }
    fn visual_unstage(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Unstage, cx);
    }
    fn visual_discard(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Discard, cx);
    }
    fn visual_cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.visual = None;
        cx.notify();
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
            if let Some(sw) = state
                .def
                .switches()
                .find(|s| s.key == full)
                .map(|s| s.key.to_string())
            {
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
                transient::Command::CommitCreate => self.start_commit(switches, window, cx),
                transient::Command::CommitAmend => {
                    self.open_editor(CommitMode::Amend, switches, window, cx)
                }
                transient::Command::CommitReword => {
                    self.open_editor(CommitMode::Reword, switches, window, cx)
                }
                _ => self.run_command(action.command, switches, cx),
            }
        }
    }

    /// Run a transient command on the background executor, showing progress in
    /// the bottom bar, then refresh.
    fn run_command(
        &mut self,
        command: transient::Command,
        switches: Vec<String>,
        cx: &mut Context<Self>,
    ) {
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

    /// Begin a new commit (`c c`). Mirrors magit's `magit-commit-assert`: a
    /// commit only takes the *staged* changes, so with nothing staged we either
    /// refuse (nothing to commit at all) or offer to commit everything (`--all`,
    /// like `git commit -a`). An explicit `--all`/`--allow-empty` switch means
    /// the user already decided, so we skip straight to the editor.
    fn start_commit(&mut self, switches: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let has_staged = self
            .status
            .as_ref()
            .is_some_and(|s| s.staged().next().is_some());
        let preempted = switches
            .iter()
            .any(|s| s == "--all" || s == "--allow-empty");
        if has_staged || preempted {
            self.open_editor(CommitMode::Create, switches, window, cx);
            return;
        }
        // Nothing staged. `--all` only stages *tracked* modifications (so does
        // `Status::unstaged`, which excludes untracked) — if there's nothing
        // there either, there is genuinely nothing to commit.
        let has_unstaged = self
            .status
            .as_ref()
            .is_some_and(|s| s.unstaged().next().is_some());
        if !has_unstaged {
            self.status_message = Some("Nothing staged (or unstaged)".to_string());
            cx.notify();
            return;
        }
        self.confirm = Some((
            "Nothing staged. Commit all uncommitted changes?".to_string(),
            Confirm::CommitAll(switches),
        ));
        cx.notify();
    }

    /// React to an edit in the commit message: auto-wrap the body (if enabled)
    /// and refresh the over-50 summary warning (if enabled). Reads the toggles
    /// live from config so the settings screen takes effect without reopening.
    fn on_editor_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor.as_ref().map(|e| e.state.clone()) else {
            return;
        };
        let wrap = self.config.commit_body_wrap;
        let ruler = self.config.commit_title_ruler;
        state.update(cx, |s, cx| {
            if wrap {
                let value = s.value().to_string();
                let wrapped = auto_fill_body(&value, COMMIT_BODY_WIDTH);
                if wrapped != value {
                    // Auto-fill only turns spaces into newlines, so the cursor's
                    // UTF-16 offset is unchanged — recompute its line/column in
                    // the rewrapped text and restore it.
                    let offset = s.cursor();
                    s.set_value(wrapped.clone(), window, cx);
                    s.set_cursor_position(utf16_offset_to_position(&wrapped, offset), window, cx);
                }
            }
            // Diagnostics carry their own copy of the text for position math;
            // reset it to the current value, then flag any summary overflow.
            let rope = s.text().clone();
            if let Some(diags) = s.diagnostics_mut() {
                diags.reset(&rope);
                if ruler {
                    if let Some((start, end)) =
                        title_overflow(&rope.to_string(), COMMIT_TITLE_LIMIT)
                    {
                        diags.push(
                            Diagnostic::new(
                                Position::new(0, start)..Position::new(0, end),
                                "Summary longer than 50 characters",
                            )
                            .with_severity(DiagnosticSeverity::Warning),
                        );
                    }
                }
            }
        });
        cx.notify();
    }

    /// Reflow the commit body to 72 columns (the `alt-q` key / "reflow" button).
    /// Unlike auto-wrap, this rejoins manually-broken lines before re-wrapping,
    /// so it tidies a paragraph you've been editing.
    fn reflow_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor.as_ref().map(|e| e.state.clone()) else {
            return;
        };
        state.update(cx, |s, cx| {
            let value = s.value().to_string();
            let reflowed = reflow_body(&value, COMMIT_BODY_WIDTH);
            if reflowed != value {
                let end = reflowed.encode_utf16().count();
                s.set_value(reflowed.clone(), window, cx);
                s.set_cursor_position(utf16_offset_to_position(&reflowed, end), window, cx);
            }
        });
        // Refresh the summary warning against the reflowed text.
        self.on_editor_changed(window, cx);
    }

    fn open_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Return inserts a newline; Cmd/Ctrl+Return submits (reported as a
        // PressEnter with secondary=true). We use code-editor mode (with the
        // grammar-less "text" language, so no syntax coloring) purely to get its
        // diagnostics layer, which we use to flag the over-50 summary; gutter,
        // line numbers, and folding are turned off so it reads as a plain box.
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("text")
                .submit_on_enter(false)
                .line_number(false)
                .folding(false)
        });
        let sub = cx.subscribe_in(
            &state,
            window,
            |this, _state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.submit_editor(window, cx),
                // Re-wrap the body and refresh the summary-length warning as the
                // message is edited.
                InputEvent::Change => this.on_editor_changed(window, cx),
                _ => {}
            },
        );
        // Focus the input so typing goes straight into it.
        state.read(cx).focus_handle(cx).focus(window, cx);
        self.editor = Some(CommitEditor {
            state: state.clone(),
            mode,
            args,
            diff: Vec::new(),
            diff_scroll: UniformListScrollHandle::new(),
            _sub: sub,
        });
        // Amend/reword pre-fill HEAD's message — loaded off the UI thread (the
        // git call must not block the UI), then set into the input if the user
        // hasn't started typing.
        if matches!(mode, CommitMode::Amend | CommitMode::Reword) {
            if let Some(repo) = self.repo.clone() {
                cx.spawn_in(window, async move |this, cx| {
                    let msg = cx
                        .background_executor()
                        .spawn(async move { repo.head_message().unwrap_or_default() })
                        .await;
                    let _ = cx.update(|window, app| {
                        state.update(app, |s, cx| {
                            if s.value().is_empty() {
                                s.set_value(msg, window, cx);
                            }
                        });
                    });
                    // set_value doesn't emit Change, so update the summary
                    // warning for the pre-filled message ourselves.
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.on_editor_changed(window, cx);
                    });
                })
                .detach();
            }
        }
        // Show the staged diff being committed. Reword commits no tree change,
        // so its diff stays empty.
        if matches!(mode, CommitMode::Create | CommitMode::Amend) {
            self.load_commit_diff(cx);
        }
        cx.notify();
    }

    /// Load the staged diff in the background and flatten it (with syntax
    /// highlighting) into the open editor's `diff` for read-only preview. When
    /// committing with `--all` and nothing is staged, preview the unstaged
    /// (tracked) changes instead — that's what the commit will actually take.
    fn load_commit_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let also_unstaged = self
            .editor
            .as_ref()
            .is_some_and(|e| e.args.iter().any(|a| a == "--all"));
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    let loaded = repo.diff_all(DiffSource::Staged).and_then(|staged| {
                        if staged.is_empty() && also_unstaged {
                            repo.diff_all(DiffSource::Unstaged)
                        } else {
                            Ok(staged)
                        }
                    });
                    match loaded {
                        Ok(diffs) => {
                            let mapped = diffs
                                .into_iter()
                                .map(|d| {
                                    let (head, tail) =
                                        file_head_tail(&repo.workdir().join(d.display_path()));
                                    let lang =
                                        highlight::detect_language(d.display_path(), &head, &tail);
                                    (d, lang)
                                })
                                .collect::<Vec<_>>();
                            (mapped, None)
                        }
                        Err(e) => (Vec::new(), Some(e.to_string())),
                    }
                })
                .await;
            let (files, error) = files;
            this.update(cx, |this, cx| {
                if this.editor.is_none() {
                    return; // editor closed before the diff loaded
                }
                if let Some(err) = error {
                    if let Some(ed) = this.editor.as_mut() {
                        ed.diff = vec![CommitDiffRow::Note(format!(
                            "staged diff unavailable: {err}"
                        ))];
                    }
                    cx.notify();
                    return;
                }
                let default = cx.theme().foreground;
                let (fg, dim) = (this.palette.fg, this.palette.dim);
                let mut rows = Vec::new();
                for (diff, lang) in &files {
                    rows.push(CommitDiffRow::File(diff.display_path().to_string()));
                    let hl = match lang {
                        Some(l) if !diff.is_binary => {
                            Some(highlight::highlight_diff(diff, l, cx, default))
                        }
                        _ => None,
                    };
                    for (hi, hunk) in diff.hunks.iter().enumerate() {
                        rows.push(CommitDiffRow::Hunk(hunk_header_text(hunk)));
                        for (li, line) in hunk.lines.iter().enumerate() {
                            let spans = hl
                                .as_ref()
                                .and_then(|h| h.get(&(hi, li)))
                                .cloned()
                                .unwrap_or_else(|| {
                                    let color = if line.kind == LineKind::NoNewline {
                                        dim
                                    } else {
                                        fg
                                    };
                                    vec![(line.content.clone(), color)]
                                });
                            rows.push(CommitDiffRow::Line {
                                kind: line.kind,
                                spans,
                            });
                        }
                    }
                }
                if let Some(ed) = this.editor.as_mut() {
                    ed.diff = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Capture-phase handler: Escape cancels the editor. (Enter is consumed by
    /// the Input as a bound action and never reaches here — commit is driven by
    /// the PressEnter subscription instead.)
    fn on_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.is_none() {
            return;
        }
        if event.keystroke.key == "escape" {
            cx.stop_propagation();
            self.cancel_editor(window, cx);
        } else if event.keystroke.key == "q" && event.keystroke.modifiers.alt {
            // alt-q reflows the body (Emacs fill-paragraph heritage); capture it
            // so the Input doesn't insert the character.
            cx.stop_propagation();
            self.reflow_editor(window, cx);
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

        let appearance =
            cx.new(|cx| SelectState::new(appearance_items, row(appearance_ix), &mut *window, cx));
        let light_theme = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(theme_names.clone()),
                row(light_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        let dark_theme = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(theme_names),
                row(dark_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        let font = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(font_items),
                row(font_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });

        let subs = vec![
            cx.subscribe_in(
                &appearance,
                window,
                |this, _, ev: &SelectEvent<Vec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(label)) = ev {
                        let value = APPEARANCE_OPTIONS
                            .iter()
                            .find(|(l, _)| *l == label.as_ref())
                            .map_or("auto", |(_, v)| v);
                        this.config.appearance = value.to_string();
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &light_theme,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.config.light_theme = name.to_string();
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &dark_theme,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.config.dark_theme = name.to_string();
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &font,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
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
                },
            ),
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
            0 => s
                .appearance
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            1 => s
                .light_theme
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            2 => s
                .dark_theme
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
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

    fn run_commit(
        &mut self,
        message: String,
        mode: CommitMode,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
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
        let cased = if shift {
            key.to_uppercase()
        } else {
            key.clone()
        };

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
                self.confirm_yes(window, cx);
            } else {
                self.confirm_no(window, cx);
            }
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
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
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
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
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
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
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
                        let flag_color = if on {
                            self.palette.modified
                        } else {
                            self.palette.dim
                        };
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
                row.child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("/")),
                )
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

        let root = div()
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
                    .child(self.key_action(
                        "editor-commit",
                        "cmd-enter",
                        "commit",
                        view,
                        Self::submit_editor,
                    ))
                    .child(self.key_action(
                        "editor-reflow",
                        "alt-q",
                        "reflow",
                        view,
                        Self::reflow_editor,
                    ))
                    .child(self.key_action(
                        "editor-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::cancel_editor,
                    )),
            );

        // With a staged diff to review, the message takes a fixed band at the
        // top and the diff fills the rest (scrollable); otherwise the message
        // fills the window.
        if ed.diff.is_empty() {
            root.child(
                div()
                    .flex_grow(1.0)
                    .w_full()
                    .child(Input::new(&ed.state).h_full()),
            )
        } else {
            root.child(
                div()
                    .h(px(176.0))
                    .w_full()
                    .child(Input::new(&ed.state).h_full()),
            )
            .child(self.render_commit_diff(ed, view))
        }
    }

    /// The read-only, scrollable staged-diff preview shown below the message.
    fn render_commit_diff(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        let count = ed.diff.len();
        div()
            .relative()
            .w_full()
            .flex_grow(1.0)
            .border_t_1()
            .border_color(self.palette.border)
            .child(
                uniform_list("commit-diff", count, {
                    let view = view.clone();
                    move |range, _window, cx| {
                        let this = view.read(cx);
                        match this.editor.as_ref() {
                            Some(ed) => range
                                .map(|ix| this.render_commit_diff_row(&ed.diff[ix]))
                                .collect::<Vec<_>>(),
                            None => Vec::new(),
                        }
                    }
                })
                .track_scroll(&ed.diff_scroll)
                .size_full()
                .py_1(),
            )
            .vertical_scrollbar(&ed.diff_scroll)
    }

    fn render_commit_diff_row(&self, row: &CommitDiffRow) -> AnyElement {
        let base = div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .flex()
            .items_center();
        match row {
            CommitDiffRow::File(path) => base
                .child(
                    div()
                        .text_color(self.palette.section)
                        .child(SharedString::from(path.clone())),
                )
                .into_any_element(),
            CommitDiffRow::Hunk(text) => base
                .text_color(self.palette.hunk)
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            CommitDiffRow::Note(text) => base
                .text_color(self.palette.dim)
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            CommitDiffRow::Line { kind, spans } => {
                let (sign, sign_color, tint) = match kind {
                    LineKind::Added => ('+', self.palette.added, Some(self.palette.added_bg)),
                    LineKind::Removed => ('-', self.palette.removed, Some(self.palette.removed_bg)),
                    _ => (' ', self.palette.dim, None),
                };
                let mut el = base;
                if let Some(t) = tint {
                    el = el.bg(t);
                }
                let mut line = div().flex().child(
                    div()
                        .text_color(sign_color)
                        .child(SharedString::from(sign.to_string())),
                );
                for (text, color) in spans {
                    line = line.child(
                        div()
                            .text_color(*color)
                            .child(SharedString::from(text.clone())),
                    );
                }
                el.child(line).into_any_element()
            }
        }
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
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from("Settings")),
                    )
                    .child(self.key_action(
                        "settings-switch",
                        "tab",
                        "switch",
                        view,
                        Self::cycle_settings_focus,
                    ))
                    .child(self.key_action(
                        "settings-close",
                        "esc",
                        "close",
                        view,
                        Self::close_settings,
                    )),
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
            .child(field(
                "commit-title-ruler",
                "Commit summary ruler",
                self.config_switch(
                    "commit-title-ruler",
                    self.config.commit_title_ruler,
                    view,
                    |cfg, on| cfg.commit_title_ruler = on,
                ),
            ))
            .child(field(
                "commit-body-wrap",
                "Commit body auto-wrap",
                self.config_switch(
                    "commit-body-wrap",
                    self.config.commit_body_wrap,
                    view,
                    |cfg, on| cfg.commit_body_wrap = on,
                ),
            ))
    }

    /// A settings toggle bound to a `bool` field of the config: flips the field
    /// and persists on click. (Mouse-driven, like the rest of the settings
    /// screen; not part of the Tab focus ring, which cycles the dropdowns.)
    fn config_switch(
        &self,
        id: &'static str,
        checked: bool,
        view: &Entity<Self>,
        set: fn(&mut config::Config, bool),
    ) -> AnyElement {
        let view = view.clone();
        Switch::new(id)
            .checked(checked)
            .on_click(move |on, _window, cx| {
                let on = *on;
                view.update(cx, |this, cx| {
                    set(&mut this.config, on);
                    config::save(&this.config);
                    cx.notify();
                });
            })
            .into_any_element()
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
            RowKind::Plain { text, color } => el
                .text_color(*color)
                .child(SharedString::from(text.clone())),
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
            RowKind::HunkHeader { text, expanded } => {
                el.child(chevron(*expanded, self.palette.dim)).child(
                    div()
                        .text_color(self.palette.hunk)
                        .child(SharedString::from(text.clone())),
                )
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
                let mut line = div().flex().child(
                    div()
                        .text_color(sign_color)
                        .child(SharedString::from(sign.to_string())),
                );
                for (text, color) in spans {
                    line = line.child(
                        div()
                            .text_color(*color)
                            .child(SharedString::from(text.clone())),
                    );
                }
                el.child(line)
            }
        };
        if clickable {
            let el = content
                .relative()
                .child(track_target(format!("status-row-{ix}")))
                .on_click({
                    let view = view.clone();
                    move |_, _window, cx: &mut App| {
                        view.update(cx, |v, cx| v.click_row(ix, cx));
                    }
                })
                // Click-and-drag selects a range, like pressing `v` and moving.
                // Shift-click extends a selection from the current cursor (or
                // the existing anchor) to the clicked row, like a list widget.
                .on_mouse_down(MouseButton::Left, {
                    let view = view.clone();
                    move |ev: &MouseDownEvent, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            if ev.modifiers.shift {
                                let anchor = v.visual.unwrap_or(v.selected);
                                v.visual = (ix != anchor).then_some(anchor);
                                v.selected = ix;
                                v.drag_anchor = None;
                                v.shift_click = true;
                            } else {
                                v.drag_anchor = Some(ix);
                                v.visual = None;
                                v.selected = ix;
                                v.shift_click = false;
                            }
                            vcx.notify();
                        });
                    }
                })
                .on_mouse_move({
                    let view = view.clone();
                    move |ev: &gpui::MouseMoveEvent, _window, cx: &mut App| {
                        if ev.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        view.update(cx, |v, vcx| {
                            let Some(anchor) = v.drag_anchor else { return };
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            // Skip redundant work while the cursor stays on one row.
                            if v.selected == ix && (ix == anchor || v.visual == Some(anchor)) {
                                return;
                            }
                            if ix != anchor {
                                v.visual = Some(anchor);
                            }
                            v.selected = ix;
                            vcx.notify();
                        });
                    }
                })
                .on_mouse_up(MouseButton::Left, {
                    let view = view.clone();
                    move |_, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if v.drag_anchor.take().is_some() {
                                vcx.notify();
                            }
                        });
                    }
                });
            // Right-click on a stageable row: select it (unless a visual
            // selection is in progress) and show a menu of the staging verbs
            // that apply. The actions act on the row at point / the selection.
            match &row.target {
                Some(target) => {
                    let (can_stage, can_unstage, can_discard) = target_ops(target);
                    let view = view.clone();
                    el.on_mouse_down(MouseButton::Right, move |_, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if v.visual.is_none() && v.rows.get(ix).is_some_and(|r| r.selectable) {
                                v.selected = ix;
                                vcx.notify();
                            }
                        });
                    })
                    .context_menu(move |mut menu, _window, _cx| {
                        if can_stage {
                            menu = menu.menu("Stage", Box::new(CtxStage));
                        }
                        if can_unstage {
                            menu = menu.menu("Unstage", Box::new(CtxUnstage));
                        }
                        if can_discard {
                            menu = menu.menu("Discard", Box::new(CtxDiscard));
                        }
                        menu
                    })
                    .into_any_element()
                }
                None => el.into_any_element(),
            }
        } else {
            content.into_any_element()
        }
    }

    /// Mouse click on a status row: select it, and toggle its fold if foldable.
    fn click_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        // A shift-click already set up the extended selection in `on_mouse_down`;
        // don't also toggle the row's fold.
        if self.shift_click {
            self.shift_click = false;
            cx.notify();
            return;
        }
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
            // Right-click menu actions, applied to the row at point / selection.
            .on_action(cx.listener(|this, _: &CtxStage, _window, cx| this.act(Op::Stage, cx)))
            .on_action(cx.listener(|this, _: &CtxUnstage, _window, cx| this.act(Op::Unstage, cx)))
            .on_action(cx.listener(|this, _: &CtxDiscard, _window, cx| this.act(Op::Discard, cx)))
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
        // Clicking the list area dismisses an open popup or an active visual
        // selection — including clicks on empty space, not just on rows. (A
        // bottom popup panel is a sibling, so clicks on it don't reach here.)
        let dismissable = self.popup.is_some() || self.visual.is_some();
        root = root.child(
            div()
                .id("list-area")
                .relative()
                .w_full()
                .flex_grow(1.0)
                .when(dismissable, |el| {
                    el.on_click(cx.listener(|this, _, _window, cx| {
                        if this.popup.is_some() {
                            this.popup = None;
                        } else {
                            this.visual = None;
                        }
                        cx.notify();
                    }))
                })
                .child(
                    uniform_list("rows", count, {
                        let view = view.clone();
                        move |range, _window, cx| {
                            let this = view.read(cx);
                            range
                                .map(|ix| this.render_row(ix, &view))
                                .collect::<Vec<_>>()
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
            root = root.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(self.palette.border)
                    .bg(self.palette.banner)
                    .text_color(self.palette.fg)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(SharedString::from(prompt.clone()))
                    .child(self.key_action("confirm-yes", "y", "yes", &view, Self::confirm_yes))
                    .child(self.key_action("confirm-no", "n", "no", &view, Self::confirm_no)),
            );
        } else if self.visual.is_some() {
            root = root.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(self.palette.border)
                    .bg(self.palette.visual)
                    .text_color(self.palette.fg)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from("VISUAL")),
                    )
                    .child(self.key_action("visual-stage", "s", "stage", &view, Self::visual_stage))
                    .child(self.key_action(
                        "visual-unstage",
                        "u",
                        "unstage",
                        &view,
                        Self::visual_unstage,
                    ))
                    .child(self.key_action(
                        "visual-discard",
                        "x",
                        "discard",
                        &view,
                        Self::visual_discard,
                    ))
                    .child(self.key_action(
                        "visual-cancel",
                        "esc",
                        "cancel",
                        &view,
                        Self::visual_cancel,
                    )),
            );
        } else if let Some(msg) = &self.status_message {
            root = root.child(status_bar(
                msg.clone(),
                self.palette.panel,
                self.palette.fg,
                self.palette.border,
            ));
        }

        // A floating "?" button (bottom-right) opens the dispatch menu — a
        // mouse affordance for discovering commands. Hidden while a popup or a
        // bottom bar (confirm / visual / status) is shown, so it never overlaps
        // them.
        let bottom_bar =
            self.confirm.is_some() || self.visual.is_some() || self.status_message.is_some();
        if self.popup.is_none() && !bottom_bar {
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
/// debug `click-id` command. Add as a child of a `.relative()` clickable
/// element so synthetic tests can click it by id. Compiled only with the
/// `debug` feature; otherwise it's an empty no-op element.
#[cfg(feature = "debug")]
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

#[cfg(not(feature = "debug"))]
fn track_target(_id: impl Into<SharedString>) -> impl IntoElement {
    gpui::Empty
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
        parts
            .iter()
            .map(|p| key_word(p))
            .collect::<Vec<_>>()
            .join("+")
    } else {
        key_word(key)
    };
    chip_box(color)
        .child(SharedString::from(label))
        .into_any_element()
}

/// A switch keycap (`-a`). When a `-` prefix is pending (we're awaiting the
/// switch letter), only the dash *inside* the keycap changes color to the
/// accent, while the keycap itself stays neutral (magit's prefix feedback).
fn switch_chip(key: &str, color: Hsla, accent: Hsla, pending: bool) -> AnyElement {
    let rest = key.strip_prefix('-').unwrap_or(key);
    let dash_color = if pending { accent } else { color };
    chip_box(color)
        .child(div().text_color(dash_color).child(SharedString::from("-")))
        .child(
            div()
                .text_color(color)
                .child(SharedString::from(rest.to_string())),
        )
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
        let (cfg, cfg_warning) = config::load_reporting();
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
                    let view = cx.new(|cx| {
                        StatusView::new(start_dir.clone(), cfg.clone(), cfg_warning.clone(), cx)
                    });
                    // The window's root must be a gpui-component Root (provides
                    // theming, overlays, and the component context).
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
                .expect("failed to open window");
            // Start the debug control channel (dev builds only; no-op unless
            // MAGRITTE_DEBUG_DIR is set).
            #[cfg(feature = "debug")]
            cx.update(|cx| debug::init(window.into(), cx));
            #[cfg(not(feature = "debug"))]
            let _ = window;
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
    fn title_overflow_flags_only_past_the_limit() {
        // Within the limit: no overflow.
        assert_eq!(title_overflow("a short summary", 50), None);
        // Exactly at the limit: still fine.
        let fifty = "x".repeat(50);
        assert_eq!(title_overflow(&fifty, 50), None);
        // One over: range covers just the overflow (col 50..51).
        let fifty_one = "x".repeat(51);
        assert_eq!(title_overflow(&fifty_one, 50), Some((50, 51)));
        // Only the first line (summary) counts; a long body doesn't trigger it.
        assert_eq!(title_overflow("ok\n\nbody line", 50), None);
    }

    #[test]
    fn auto_fill_wraps_body_but_not_the_summary() {
        // A summary well past the width is left intact.
        let long_title = "w ".repeat(60); // 120 chars, all on line 0
        let filled = auto_fill_body(long_title.trim_end(), 72);
        assert_eq!(filled.lines().count(), 1, "summary must not wrap");

        // A long body line wraps at a space, each piece within the width.
        let text = format!("summary\n\n{}", "word ".repeat(30).trim_end());
        let filled = auto_fill_body(&text, 72);
        let body_lines: Vec<&str> = filled.lines().skip(2).collect();
        assert!(body_lines.len() > 1, "long body line should wrap");
        assert!(body_lines.iter().all(|l| l.chars().count() <= 72));
        // Wrapping only splits — joining the pieces with spaces restores it.
        assert_eq!(body_lines.join(" "), "word ".repeat(30).trim_end());
    }

    #[test]
    fn auto_fill_preserves_offsets_and_is_idempotent() {
        let text = format!("summary\n\n{}", "word ".repeat(30).trim_end());
        let once = auto_fill_body(&text, 72);
        // Space->newline keeps total length identical (offset-preserving).
        assert_eq!(once.chars().count(), text.chars().count());
        // Running it again changes nothing (already wrapped).
        assert_eq!(auto_fill_body(&once, 72), once);
    }

    #[test]
    fn auto_fill_leaves_unbreakable_long_words() {
        let word = "x".repeat(100);
        let text = format!("summary\n\n{word}");
        let filled = auto_fill_body(&text, 72);
        assert_eq!(filled.lines().nth(2), Some(word.as_str()));
    }

    #[test]
    fn reflow_rejoins_then_rewraps_paragraphs() {
        // Two short manually-broken lines in one paragraph rejoin and re-wrap.
        let text = "summary\n\nthese are\nseveral short\nlines";
        let reflowed = reflow_body(text, 72);
        assert_eq!(reflowed, "summary\n\nthese are several short lines");

        // A blank line separates paragraphs, which stay separate.
        let text = "summary\n\npara one here\n\npara two here";
        let reflowed = reflow_body(text, 72);
        assert_eq!(reflowed, "summary\n\npara one here\n\npara two here");
    }

    #[test]
    fn utf16_offset_to_position_tracks_lines() {
        assert_eq!(utf16_offset_to_position("abc", 2), Position::new(0, 2));
        // Offset just past the first newline -> start of line 1.
        assert_eq!(utf16_offset_to_position("ab\ncd", 3), Position::new(1, 0));
        assert_eq!(utf16_offset_to_position("ab\ncd", 5), Position::new(1, 2));
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

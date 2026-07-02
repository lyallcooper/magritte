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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, div, point, px, size, uniform_list, AnyElement, AnyWindowHandle, App, AppContext, Bounds,
    ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, SharedString, Styled,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};

mod commands;
mod commit_diff_view;
mod commit_editor;
mod commit_text;
mod config;
mod controller;
#[cfg(feature = "debug")]
mod debug;
mod editor_launch;
mod generation;
mod editors;
mod git_action;
mod highlight;
mod input;
mod ipc;
mod kbd;
mod log_view;
mod navigation;
mod picker;
mod render;
mod settings;
mod staging;
mod state;
mod status_label;
mod targets;
mod theme;
mod transient_state;
pub(crate) use commands::*;
pub(crate) use commit_diff_view::*;
pub(crate) use commit_editor::*;
pub(crate) use log_view::*;
pub(crate) use staging::*;
pub(crate) use transient_state::*;
use generation::Generation;
use git_action::{describe_discard, Action, HunkSelections, Op, RegionKind};
use highlight::{FileHighlights, Span};
use picker::{CreateMode, PickerList};

/// Key context for our status view, used so our `tab` binding takes precedence
/// over gpui-component Root's focus-navigation `tab`.
const STATUS_CONTEXT: &str = "MagritteStatus";

// Tab is bound by gpui-component's Root (focus nav) and so never reaches an
// on_key_down listener; we override it with an action in our key context.
actions!(magritte, [ToggleFold, Quit, CloseWindow, OpenSettings]);
// Right-click context-menu actions; dispatched by the PopupMenu and handled on
// the status view, which applies them to the row at point (selected on
// right-click) or the active visual selection.
actions!(
    magritte,
    [
        CtxStage,
        CtxUnstage,
        CtxDiscard,
        CtxTakeOurs,
        CtxTakeTheirs,
        CtxCopy
    ]
);
// Settings "Open config file" dropdown actions: copy the path, or open the
// config with a specific editor (carries the editor's app path). `no_json`
// avoids the serde/schemars requirement of keymap-loadable actions.
actions!(magritte, [CopyConfigPath, CopyRepoConfigPath]);
use gpui::Subscription;
use gpui_component::input::{InputEvent, InputState};
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::{ActiveTheme, IndexPath};
use magritte_core::transient::{self, Group, Suffix, TitleSpan, Transient};
use magritte_core::{
    CommitMetadata, CommitMode, ConflictSide, DiffSource, EntryKind, FileEntry,
    IgnoreDest, LineKind, LogEntry, RebaseAction, RefreshNeeds, RemoteTargets, Repo, ResetMode,
    Sequence, SequenceKind, Stash, Status, TagsAround,
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/lyallcooper/homebrew-magritte/releases/latest";

impl Transfer {
    /// Present-tense label for the progress message.
    fn verb(&self) -> &'static str {
        match self {
            Transfer::Push { .. } | Transfer::PushRef { .. } => "Pushing",
            Transfer::Pull { .. } | Transfer::PullRef => "Pulling",
            Transfer::Fetch => "Fetching",
        }
    }

    /// The minibuffer prompt (styled spans): you push the current branch *to* a
    /// target, but pull/fetch *from* one (matching magit's "Push master to" /
    /// "Pull from" / "Fetch from"). The branch is set off as its own span.
    fn prompt(&self) -> Vec<TitleSpan> {
        match self {
            Transfer::Push { branch, .. } | Transfer::PushRef { branch } => {
                if branch.is_empty() {
                    transient::plain_title("Push to")
                } else {
                    vec![
                        TitleSpan::text("Push "),
                        TitleSpan::branch(branch.clone()),
                        TitleSpan::text(" to"),
                    ]
                }
            }
            Transfer::Pull { .. } | Transfer::PullRef => transient::plain_title("Pull from"),
            Transfer::Fetch => transient::plain_title("Fetch from"),
        }
    }
}

/// Whether the rebase-todo editor is composing a *new* interactive rebase or
/// editing the remaining plan of one already in progress.
#[derive(PartialEq, Eq)]
enum RebaseTodoMode {
    /// `r i`: build `base..HEAD`, then run `git rebase -i`.
    Start,
    /// `r e` while paused: rewrite the remaining todo via `git rebase --edit-todo`.
    Edit,
}

/// The interactive-rebase todo editor: an editable list of commits, each with an
/// action, reorderable. In `Start` mode it's `base..HEAD` and runs as one new
/// rebase; in `Edit` mode it's an in-progress rebase's remaining steps.
struct RebaseTodoView {
    /// The rebase base (`base..HEAD` is the editable range). Unused in `Edit`.
    base: String,
    /// Toggled switches carried from the rebase transient (`--autostash`, …).
    args: Vec<String>,
    /// The todo, oldest first (git's order).
    steps: Vec<magritte_core::RebaseStep>,
    /// The todo as loaded, to detect unsaved edits when cancelling.
    initial: Vec<magritte_core::RebaseStep>,
    /// Cursor row.
    selected: usize,
    scroll: UniformListScrollHandle,
    mode: RebaseTodoMode,
    /// Showing the "discard edits?" confirmation (Esc with unsaved changes).
    confirming_cancel: bool,
}

/// Scroll state for a read-only list view: its handle plus the top row we track
/// for keyboard scrolling (the handle's index getter is test-only).
struct ScrollView {
    scroll: UniformListScrollHandle,
    top: usize,
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
    hover: Hsla,
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
        // Every face is read directly from the theme — the app never blends
        // colors at runtime. Translucent overlays (the visual-mode region, the
        // diff line bands, the warning banner) carry their alpha in the theme's
        // hex (`#rrggbbaa`), so they're read verbatim too.
        let status = &t.highlight_theme.style.status;
        Palette {
            bg: t.background,
            fg: t.foreground,
            dim: t.muted_foreground,
            border: t.border,
            selection: t.accent, // accent.background — selected row
            hover: t.list_hover, // list.hover.background
            visual: t.selection, // selection.background (translucent)
            section: t.primary,
            hunk: status.info(cx),
            panel: t.secondary, // elevated surface for the panel
            modified: status.warning(cx),
            added: status.success(cx),
            removed: status.error(cx),
            added_bg: status.success_background(cx),
            removed_bg: status.error_background(cx),
            banner: status.warning_background(cx),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        let g = |v: u32| gpui::rgb(v).into();
        let ga = |v: u32| gpui::rgba(v).into();
        Palette {
            bg: g(0xffffff),
            fg: g(0x1a1a1a),
            dim: g(0x8a8a8a),
            border: g(0xe2e2e2),
            selection: g(0xeaeaea),
            hover: g(0xf5f5f5),
            visual: ga(0x007aff52),
            section: g(0x2f6feb),
            hunk: g(0x6f42c1),
            panel: g(0xf6f6f6),
            modified: g(0xb08800),
            added: g(0x1a7f37),
            removed: g(0xcf222e),
            added_bg: ga(0x1a7f371f),
            removed_bg: ga(0xcf222e1f),
            banner: ga(0xb088002e),
        }
    }
}

/// Fixed row height (points) so `uniform_list` can virtualize every row.
const ROW_HEIGHT: f32 = 18.0;
/// How long a success notice lingers before auto-dismissing (seconds).
const STATUS_FADE_SECS: u64 = 4;
/// How long background work must run before the title-bar spinner appears, so
/// quick operations never flash it.
const BUSY_SPINNER_DELAY_MS: u64 = 200;
/// Minimum gap between refresh-on-focus runs: focusing refreshes immediately,
/// but only if the last refresh (of any kind) was at least this long ago, so
/// rapid app-switching doesn't re-run a full status each time.
const FOCUS_REFRESH_COOLDOWN_MS: u64 = 5000;
/// How long the commit editor's discard-prompt flash stays lit after an
/// ignored keypress.
const CONFIRM_FLASH_MS: u64 = 400;
/// The status text for a clipboard copy. Doubles as the toast's discriminator:
/// `status_copied` is rendered (emphasized) only when the message is this.
const COPIED_LABEL: &str = "Copied";
/// Left padding (points) added per indent level.
const INDENT_STEP: f32 = 16.0;
/// Base left padding (points) before any indent.
const ROW_PAD_LEFT: f32 = 8.0;
/// Fixed width (points) of the status-word column on file rows.
const STATUS_COL_WIDTH: f32 = 84.0;
/// Group name shared by keycap+label button rows so hovering a row highlights
/// only its label (via `group_hover`), not its keycap.
const KBD_ROW_GROUP: &str = "kbd-row";

/// In a transient, save the current argument state as its defaults (magit's
/// `transient-save`, which uses `C-x C-s`).
const TRANSIENT_SAVE_KEY: &str = "ctrl-s";

/// After a refresh, warm at most this many file diffs in the background...
const PREFETCH_FILE_CAP: usize = 16;
/// ...skipping any whose changed-line count exceeds this, so massive diffs are
/// only computed when the user actually expands them.
const PREFETCH_LINE_CAP: u32 = 2000;

/// The commit/stash listings for the non-file status sections, refreshed off
/// the UI thread (cheap `git log`/`stash list`). Empty lists (e.g. no upstream)
/// simply render no section.
#[derive(Debug, Clone, Default)]
struct StatusSections {
    /// Commits on HEAD not yet on the upstream.
    unpushed: Vec<LogEntry>,
    /// Commits on the upstream not yet pulled into HEAD.
    unpulled: Vec<LogEntry>,
    /// The triangular-workflow counterparts, vs the push target (empty unless a
    /// distinct push target is configured).
    unpushed_pushremote: Vec<LogEntry>,
    unpulled_pushremote: Vec<LogEntry>,
    /// The most recent commits (count from `[status].recent_count`).
    recent: Vec<LogEntry>,
    stashes: Vec<Stash>,
    /// Ignored file paths — fetched only when the `ignored` section is enabled.
    ignored: Vec<String>,
}

/// Which top-level section a row belongs to. Used as a stable fold key. The file
/// sections (Untracked/Unstaged/Staged) carry staging; the commit/stash sections
/// are read-only listings with act-at-point (open/yank/apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SectionId {
    Untracked,
    Unstaged,
    Staged,
    Stashes,
    Unpushed,
    Unpulled,
    /// Unpushed to / unpulled from the *push* target (triangular workflows).
    UnpushedPushremote,
    UnpulledPushremote,
    Recent,
    Ignored,
}

impl SectionId {
    /// Every section, in enum order — the source of truth for "all sections",
    /// used to seed the default-expanded set and resolve config ids.
    const ALL: [SectionId; 10] = [
        SectionId::Untracked,
        SectionId::Unstaged,
        SectionId::Staged,
        SectionId::Stashes,
        SectionId::Unpushed,
        SectionId::Unpulled,
        SectionId::UnpushedPushremote,
        SectionId::UnpulledPushremote,
        SectionId::Recent,
        SectionId::Ignored,
    ];

    /// The config id (`[status].sections` entry) for this section.
    fn config_id(self) -> &'static str {
        match self {
            SectionId::Untracked => "untracked",
            SectionId::Unstaged => "unstaged",
            SectionId::Staged => "staged",
            SectionId::Stashes => "stashes",
            SectionId::Unpushed => "unpushed",
            SectionId::Unpulled => "unpulled",
            SectionId::UnpushedPushremote => "unpushed-pushremote",
            SectionId::UnpulledPushremote => "unpulled-pushremote",
            SectionId::Recent => "recent",
            SectionId::Ignored => "ignored",
        }
    }

    /// The section for a config id, or `None` if unknown.
    fn from_config_id(id: &str) -> Option<SectionId> {
        SectionId::ALL.into_iter().find(|s| s.config_id() == id)
    }
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
    /// A commit row, by its section + full hash (the same commit can appear in
    /// more than one section, e.g. recent and unpushed).
    Commit(SectionId, String),
    /// A stash row, by its reference (`stash@{N}`).
    Stash(String),
}

impl AnchorIdent {
    fn section(&self) -> Option<SectionId> {
        match self {
            AnchorIdent::Top => None,
            AnchorIdent::Section(s)
            | AnchorIdent::File(s, _)
            | AnchorIdent::Hunk(s, _, _)
            | AnchorIdent::Line(s, _, _, _)
            | AnchorIdent::Commit(s, _) => Some(*s),
            AnchorIdent::Stash(_) => Some(SectionId::Stashes),
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

/// git convention: keep the commit summary within 50 columns, and wrap the
/// body at 72.
const COMMIT_TITLE_LIMIT: usize = 50;
const COMMIT_BODY_WIDTH: usize = 72;

/// The three controls for an in-progress sequence.
#[derive(Clone, Copy)]
enum SeqOp {
    Continue,
    Skip,
    Abort,
}

/// How a keystroke sequence resolves against the effective keymap.
enum KeyMatch {
    /// A complete binding: run this command id.
    Command(String),
    /// A prefix of one or more longer bindings: wait for the next key.
    Prefix,
    /// Neither — nothing is bound to this sequence or anything extending it.
    Unbound,
}

/// The keys typed so far of an in-progress sequence, awaiting the next key.
/// Sequences nest to any depth: each key that lands on a deeper prefix extends
/// `seq` (e.g. `g` → `g r`), until one resolves to a command or to nothing.
struct PendingPrefix {
    /// The keys typed so far, space-joined (e.g. `g` or `C-x C-c`).
    seq: String,
    /// The `prefix_gen` value when entered, so a stale timer is ignored.
    gen: u64,
    /// Set once the which-key delay elapses: the bottom strip expands from just
    /// the typed keys into the list of possible continuations.
    which_key: bool,
}

/// Applying a commit selected in the log to the current branch.
#[derive(Clone, Copy)]
enum PickOp {
    CherryPick,
    CherryApply,
    Revert,
    RevertNoCommit,
}

/// The active full-window screen. `Status` is the home base; the rest take over
/// the window when open. Exactly one is active, so invalid combinations (two
/// screens at once) can't be represented, and the active screen is chosen by a
/// single `match` in render and key handling rather than a repeated cascade.
#[derive(Default)]
enum Screen {
    #[default]
    Status,
    /// The commit message editor.
    Editor(CommitEditor),
    /// The live settings screen.
    Settings(settings::SettingsState),
    /// The git command-log view (magit's `$` process buffer); holds the scroll
    /// state, with entries read live from the repo.
    GitLog(ScrollView),
    /// The commit-log view (`l`).
    Log(LogState),
    /// A commit's diff detail, opened with Enter from the log or a status
    /// commit row. It overlays the screen it came from; `back` is that screen,
    /// restored on close (the log, or the status view).
    Commit { view: CommitView, back: Box<Screen> },
    /// A standalone diff buffer opened by the diff transient.
    Diff { view: DiffView, back: Box<Screen> },
    /// The interactive-rebase todo editor (`r i`).
    RebaseTodo(RebaseTodoView),
}

struct StatusView {
    /// The directory we tried to open (for error messages).
    root: PathBuf,
    repo: Option<Repo>,
    status: Option<Status>,
    /// Commit/stash lists for the non-file status sections (unpushed/unpulled/
    /// recent/stashes), each refreshed by its own background fetch so a slow one
    /// can't hold up the rest (see [`refresh`](Self::refresh)).
    status_sections: StatusSections,
    /// Sections whose fetch is in flight for the current generation. A section
    /// that's here *and* already has data shows a small spinner by its header
    /// (it's being refreshed); one with no data yet just pops in when it lands,
    /// so a first load doesn't flash spinners on not-yet-visible sections.
    loading_sections: HashSet<SectionId>,
    /// Paths with an unmerged (conflicted) status, refreshed with `rebuild_rows`
    /// so `is_conflicted` is an O(1) lookup rather than an O(entries) scan per
    /// row per frame in `render_row`.
    conflicted: HashSet<String>,
    /// The in-progress merge/rebase/cherry-pick/revert/am, surfaced as a banner.
    sequence: Option<Sequence>,
    /// Original commit ids whose `reword` rows were intentionally written to
    /// git as `edit` stops so the in-app editor can handle their messages.
    pending_rebase_rewords: HashSet<String>,
    error: Option<String>,
    expanded: HashSet<FoldKey>,
    /// Hunks the user has explicitly collapsed (`FoldKey::Hunk`). Hunks default
    /// to expanded, so this tracks the exceptions rather than `expanded` does.
    collapsed_hunks: HashSet<FoldKey>,
    /// Set by fold level 3 (files open, hunks closed): diffs that finish
    /// loading afterwards get their hunks collapsed on arrival, so the level
    /// covers lazily loaded diffs too. Cleared by any manual fold toggle, a
    /// refresh, or another level.
    collapse_new_hunks: bool,
    diffs: HashMap<(DiffSource, String), DiffState>,
    /// Cached syntax highlighting per file diff, keyed like `diffs`.
    highlights: HashMap<(DiffSource, String), FileHighlights>,
    /// Detected highlight language per file diff, kept so highlighting can be
    /// recomputed on a theme change without re-reading files off the UI thread.
    diff_langs: HashMap<(DiffSource, String), &'static str>,
    /// Immutable commit detail loads (metadata/message/diff), keyed by full OID
    /// plus diff args/pathspecs. Rows are re-rendered from this on demand so the
    /// current theme still controls highlight colors.
    commit_cache: HashMap<CommitCacheKey, CommitCacheEntry>,
    commit_cache_order: VecDeque<CommitCacheKey>,
    /// Resolved git-config defaults for transient switches during the current
    /// repository generation. Cleared on refresh so external git config changes
    /// are picked up without re-querying on every popup open.
    transient_config_defaults: HashMap<String, bool>,
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
    generation: Generation,
    /// Cancels the in-flight read jobs (status/diff/prefetch) of the current
    /// generation. `refresh` flips this and installs a fresh flag, so the
    /// processes superseded by a newer refresh are killed, not just dropped.
    read_cancel: Arc<AtomicBool>,
    /// Cancel flag for the active mutating job (push/pull/merge/…), set while it
    /// runs so `C-g`/Esc can kill the subprocess. `None` when nothing is running.
    job_cancel: Option<Arc<AtomicBool>>,
    /// Bumped whenever a screen-changing async load starts (log, reflog, commit
    /// diff, rebase todo). A load verifies its captured value still matches
    /// before populating the screen, so a superseded load can't land in the
    /// screen a newer request opened.
    screen_gen: Generation,
    /// Scopes the background auto-fetch loop. Bumped whenever the `[fetch]`
    /// config changes (and at startup): the running loop exits once its captured
    /// value is stale, so toggling auto-fetch or its interval restarts cleanly.
    auto_fetch_gen: Generation,
    /// Scopes the background update-check loop. Bumped whenever the setting is
    /// toggled or config reloads, so disabling update checks stops the old loop.
    update_check_gen: Generation,
    /// Newer release version already announced in this session, to avoid
    /// repeating the periodic update notice every interval.
    notified_update_version: Option<String>,
    /// In-flight background operations (status reads, jobs, fetches, diff
    /// loads). The title-bar spinner shows while this is non-zero *and* the
    /// work outlasts a short delay — see [`StatusView::begin_activity`].
    activity: u32,
    /// Whether the title-bar activity spinner is currently shown. Set by the
    /// delay timer, cleared when `activity` returns to zero.
    busy: bool,
    /// Scopes the spinner's delay timer so a stale arm-timer (activity already
    /// ended) can't light the spinner after the fact.
    busy_gen: Generation,
    /// When the status was last refreshed (any refresh — manual, post-action,
    /// auto-fetch, or focus). Throttles the refresh-on-focus: it fires
    /// immediately on focus unless a refresh happened within the cooldown, so
    /// rapid app-switching doesn't re-run a full status each time.
    last_refresh: Option<std::time::Instant>,
    /// A prefix key awaiting the next key of a sequence (e.g. `g` before `g r`),
    /// with the generation that scopes its timeout. Any key that starts a
    /// multi-key binding can be a prefix; `None` when none is pending.
    pending_prefix: Option<PendingPrefix>,
    /// Bumped each time a prefix is entered, so a stale timeout (a newer prefix,
    /// or a resolved one) is ignored.
    prefix_gen: Generation,
    /// Debounces saving the window frame while the user drags/resizes it.
    window_bounds_save_gen: Generation,
    /// Scopes the timer that clears the commit editor's discard-prompt flash, so
    /// a later flash isn't cleared early by an earlier one's timer.
    confirm_flash_gen: Generation,
    /// An open bottom popup (command transient or help menu), or `None`.
    popup: Option<Popup>,
    /// The active full-window screen — exactly one at a time. Modeling these as
    /// one enum (rather than several `Option` fields) makes invalid combinations
    /// unrepresentable and lets render and key-handling pick the active screen
    /// with one `match` instead of re-deriving a priority cascade. Overlays (the
    /// `popup` above, the confirm bar) sit *over* whatever screen is active.
    screen: Screen,
    /// The monospace font family for code, diffs, and tabular columns.
    font: SharedString,
    /// The proportional UI font for prose chrome; equals `font` when unset.
    ui_font: SharedString,
    /// The effective config: the global config with this repo's `.git/magritte`
    /// overlay merged on top. Everything renders from this.
    config: config::Config,
    /// The *global* config alone (no repo overlay). The settings screen is
    /// global-only, so its saves write this — never [`config`](Self::config),
    /// which would leak the repo overlay (e.g. a repo's `[status].sections`)
    /// into the global file. Kept in sync with the global file by `new`,
    /// `apply_config`, and the settings handlers.
    config_global: config::Config,
    /// The effective keystroke → command-id map (registry defaults overlaid with
    /// the user's `[keymap]`), resolved by `on_key`/`run_dispatch`.
    keymap: HashMap<String, String>,
    /// Kept alive so the native config-file watcher keeps delivering events
    /// (dropping it stops watching). `None` if there's no config dir to watch.
    _config_watcher: Option<notify::RecommendedWatcher>,
    /// Kept alive so the system light/dark appearance observer stays active.
    _appearance_sub: Option<Subscription>,
    /// Kept alive so the window-activation observer (focus refresh) stays active.
    _activation_sub: Option<Subscription>,
    /// Kept alive so the window-frame persistence observer stays active.
    _window_bounds_sub: Option<Subscription>,
    /// Per-command usage, for ranking the `:` palette by frecency.
    usage: config::Usage,
    /// Saved per-transient argument defaults (magit's `transient-save`), global scope.
    transient_arguments: config::TransientArguments,
    /// The same, scoped to this repo (`.git/magritte/transient-arguments.toml`),
    /// overlaid on the global ones (repo wins per transient id). Empty with no repo.
    repo_transient_arguments: config::TransientArguments,
    /// This repo's settings dir (`.git/magritte`), for repo-scoped saves and the
    /// live-reload watcher. `None` with no repo.
    repo_scope_dir: Option<PathBuf>,
    /// The per-worktree git dir's `magritte` scope, for UI state local to this
    /// checkout (fold state). Equals `repo_scope_dir` in the main worktree.
    /// `None` with no repo.
    worktree_scope_dir: Option<PathBuf>,
    /// The per-worktree git dir itself, resolved once at startup. Sequence-state
    /// reads use this to avoid re-running `rev-parse --absolute-git-dir` on
    /// every refresh.
    worktree_git_dir: Option<PathBuf>,
    /// The title-bar tag display: (nearest tag behind + commits-since, nearest
    /// tag ahead + commits-until). Refreshed with status when title-bar tags are on.
    tag_info: TagsAround,
    /// Cached list of monospace font families (computed on first settings open).
    mono_fonts: Vec<SharedString>,
    /// Cached list of all font families, for the UI-font picker.
    ui_fonts: Vec<SharedString>,
    /// Installed GUI editors, as (display name, .app path), for the settings
    /// "Open config file" dropdown. Refreshed each time settings opens.
    editors: Vec<(SharedString, SharedString)>,
    /// Last operation result / progress, shown in the bottom bar.
    status_message: Option<String>,
    /// For a copy confirmation, the copied value — rendered emphasized after the
    /// `Copied` label. Set by [`copy_to_clipboard`]; shown only when the message
    /// is exactly that label, so the many direct `status_message` writes that
    /// don't clear it can't accidentally trail a stale value.
    status_copied: Option<SharedString>,
    /// A keystroke to render as keycap(s) before the message (e.g. the unbound
    /// `g x` in "g x is unbound"). Cleared by every `status` post; set right
    /// after by the few messages that lead with a key.
    status_keys: Option<String>,
    /// Bumped each time the status message changes, so an auto-dismiss timer
    /// only clears the message it was scheduled for (not a newer one).
    status_seq: Generation,
    /// Bumped per async picker open, stamped onto the picker, so a late
    /// candidate load only fills the picker it was started for.
    picker_gen: Generation,
    /// In the `$` command-log view, whether to also show the UI's own read-only
    /// queries (status/diff/ref lookups), which are hidden by default.
    git_log_show_all: bool,
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
        // The repo's settings scope (`.git/magritte`) and its saved argument sets,
        // overlaid on the global ones when a transient opens. Keyed to the
        // *common* git dir, so config/arguments are shared across worktrees.
        let repo_scope_dir = repo
            .as_ref()
            .and_then(|r| r.git_common_dir().ok())
            .map(|d| config::repo_dir(&d));
        // UI state local to this checkout (folds, window placement) lives in the
        // *per-worktree* git dir instead — `.git/magritte` for the main worktree,
        // `.git/worktrees/<name>/magritte` for a linked one.
        let worktree_git_dir = repo
            .as_ref()
            .and_then(|r| r.git_dir().ok());
        let worktree_scope_dir = worktree_git_dir.as_ref().map(|d| config::repo_dir(d));
        let repo_transient_arguments = repo_scope_dir
            .as_ref()
            .map(|d| config::load_transient_arguments_at(&config::repo_transient_arguments_path(d)))
            .unwrap_or_default();
        // The global config alone, before any repo overlay — what in-app
        // (settings-screen) saves write back, so they never persist the repo's
        // overrides into the global file. (`config` here is the global config
        // main loaded via `load_reporting`.)
        let config_global = config.clone();
        // Overlay this repo's config.toml (if any) on the global config. Done
        // here, after repo discovery, since the repo isn't known until now; the
        // re-resolved warning supersedes the global-only one from startup, and we
        // re-apply appearance so a repo theme override paints from the first frame
        // (main applied the global theme pre-window).
        let (config, startup_warning) = match repo_scope_dir.as_ref().map(|d| d.join("config.toml"))
        {
            Some(p) if p.exists() => {
                let (merged, warning) = config::load_merged(Some(&p));
                theme::apply_appearance(&merged, cx);
                (merged, warning)
            }
            _ => (config, startup_warning),
        };
        let font = theme::resolve_font(&config, cx);
        let ui_font = theme::resolve_ui_font(&config, cx);

        // Resolve the effective keymap and validate config values; fold any
        // warnings (unknown command id, theme, appearance, …) into the startup
        // notice so the user learns the setting was ignored.
        let (keymap, mut warnings) = build_keymap(&config);
        warnings.extend(theme::config_value_warnings(&config, cx));
        let startup_warning = match (startup_warning, warnings.is_empty()) {
            (warning, true) => warning,
            (Some(warning), false) => Some(format!("{warning}; {}", warnings.join("; "))),
            (None, false) => Some(warnings.join("; ")),
        };

        // Sections are expanded by default; individual files start collapsed,
        // so opening a large repo loads no diffs until a file is expanded. A
        // per-repo `folds.toml` then re-collapses whatever the user last left
        // collapsed (persisting sections only — file/hunk folds stay ephemeral
        // so reopening loads no diffs).
        let mut expanded: HashSet<FoldKey> = SectionId::ALL
            .iter()
            .map(|s| FoldKey::Section(*s))
            .collect();
        if let Some(dir) = &worktree_scope_dir {
            let folds = state::scoped_path(dir, state::FOLDS_FILE);
            for id in state::load_toml_or_default::<state::FoldState>(&folds).collapsed {
                if let Some(section) = SectionId::from_config_id(&id) {
                    expanded.remove(&FoldKey::Section(section));
                }
            }
        }

        let mut view = StatusView {
            root,
            repo,
            status: None,
            status_sections: StatusSections::default(),
            loading_sections: HashSet::new(),
            tag_info: (None, None),
            conflicted: HashSet::new(),
            sequence: None,
            pending_rebase_rewords: HashSet::new(),
            error: None,
            expanded,
            collapsed_hunks: HashSet::new(),
            collapse_new_hunks: false,
            diffs: HashMap::new(),
            highlights: HashMap::new(),
            diff_langs: HashMap::new(),
            commit_cache: HashMap::new(),
            commit_cache_order: VecDeque::new(),
            transient_config_defaults: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            visual: None,
            drag_anchor: None,
            shift_click: false,
            generation: Generation::default(),
            read_cancel: Arc::new(AtomicBool::new(false)),
            job_cancel: None,
            screen_gen: Generation::default(),
            auto_fetch_gen: Generation::default(),
            update_check_gen: Generation::default(),
            notified_update_version: None,
            activity: 0,
            busy: false,
            busy_gen: Generation::default(),
            last_refresh: None,
            pending_prefix: None,
            prefix_gen: Generation::default(),
            window_bounds_save_gen: Generation::default(),
            confirm_flash_gen: Generation::default(),
            popup: None,
            screen: Screen::Status,
            font,
            ui_font,
            config,
            config_global,
            keymap,
            _config_watcher: None,
            _appearance_sub: None,
            _activation_sub: None,
            _window_bounds_sub: None,
            usage: config::load_usage(),
            transient_arguments: config::load_transient_arguments(),
            repo_transient_arguments,
            repo_scope_dir,
            worktree_scope_dir,
            worktree_git_dir,
            mono_fonts: Vec::new(),
            ui_fonts: Vec::new(),
            editors: Vec::new(),
            status_message: startup_warning,
            status_copied: None,
            status_keys: None,
            status_seq: Generation::default(),
            picker_gen: Generation::default(),
            git_log_show_all: false,
            confirm: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            palette: Palette::default(),
        };
        view.refresh(cx);
        view
    }

    // Read accessors for the active [`Screen`]'s state — `None` unless that
    // screen is the active one. Mutating sites match `&mut self.screen` inline
    // (so the borrow stays scoped to `screen`, like the old per-field access).
    fn editor(&self) -> Option<&CommitEditor> {
        match &self.screen {
            Screen::Editor(e) => Some(e),
            _ => None,
        }
    }
    fn settings(&self) -> Option<&settings::SettingsState> {
        match &self.screen {
            Screen::Settings(s) => Some(s),
            _ => None,
        }
    }
    fn git_log(&self) -> Option<&ScrollView> {
        match &self.screen {
            Screen::GitLog(s) => Some(s),
            _ => None,
        }
    }
    fn log(&self) -> Option<&LogState> {
        match &self.screen {
            Screen::Log(l) => Some(l),
            _ => None,
        }
    }
    fn commit_view(&self) -> Option<&CommitView> {
        match &self.screen {
            Screen::Commit { view, .. } => Some(view),
            _ => None,
        }
    }
    fn diff_view(&self) -> Option<&DiffView> {
        match &self.screen {
            Screen::Diff { view, .. } => Some(view),
            _ => None,
        }
    }
    fn rebase_todo(&self) -> Option<&RebaseTodoView> {
        match &self.screen {
            Screen::RebaseTodo(r) => Some(r),
            _ => None,
        }
    }
    fn editor_mut(&mut self) -> Option<&mut CommitEditor> {
        match &mut self.screen {
            Screen::Editor(e) => Some(e),
            _ => None,
        }
    }
    fn settings_mut(&mut self) -> Option<&mut settings::SettingsState> {
        match &mut self.screen {
            Screen::Settings(s) => Some(s),
            _ => None,
        }
    }
    fn log_mut(&mut self) -> Option<&mut LogState> {
        match &mut self.screen {
            Screen::Log(l) => Some(l),
            _ => None,
        }
    }
    fn commit_view_mut(&mut self) -> Option<&mut CommitView> {
        match &mut self.screen {
            Screen::Commit { view, .. } => Some(view),
            _ => None,
        }
    }
    fn diff_view_mut(&mut self) -> Option<&mut DiffView> {
        match &mut self.screen {
            Screen::Diff { view, .. } => Some(view),
            _ => None,
        }
    }
    fn rebase_todo_mut(&mut self) -> Option<&mut RebaseTodoView> {
        match &mut self.screen {
            Screen::RebaseTodo(r) => Some(r),
            _ => None,
        }
    }
    fn git_log_mut(&mut self) -> Option<&mut ScrollView> {
        match &mut self.screen {
            Screen::GitLog(s) => Some(s),
            _ => None,
        }
    }
    /// Take the rebase-todo editor's state, leaving the home screen.
    fn take_rebase_todo(&mut self) -> Option<RebaseTodoView> {
        match std::mem::take(&mut self.screen) {
            Screen::RebaseTodo(r) => Some(r),
            other => {
                self.screen = other;
                None
            }
        }
    }
    /// Take the commit editor's state, leaving the home screen.
    fn take_editor(&mut self) -> Option<CommitEditor> {
        match std::mem::take(&mut self.screen) {
            Screen::Editor(e) => Some(e),
            other => {
                self.screen = other;
                None
            }
        }
    }

    /// Re-apply config edits and system light/dark changes live, event-driven
    /// (no polling): a native watch on the config file and GPUI's appearance
    /// observer. Needs `window` for the observer, so it runs once the window
    /// exists; the in-app settings screen is the other path. Held subscriptions
    /// keep both alive.
    fn install_watchers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // System light/dark: re-theme when the window's appearance flips (only
        // matters when the config follows the system, but `reapply_theme` is
        // cheap and idempotent).
        self._appearance_sub = Some(cx.observe_window_appearance(window, |view, _window, cx| {
            view.reapply_theme(cx);
        }));

        // Refresh when the window regains focus, so changes made outside the app
        // show up without a manual `g r` — the same cost as the `g r` you'd press
        // anyway, and opt-out via `refresh_on_focus`. We deliberately don't watch
        // the worktree (a large-repo event/refresh-storm hazard magit also
        // avoids); this is the bounded, on-demand alternative. Skipped until the
        // first status load lands so it doesn't double the startup refresh, and
        // only on the status screen (other screens have their own state).
        self._activation_sub = Some(cx.observe_window_activation(window, |view, window, cx| {
            if !(window.is_window_active()
                && view.config.refresh_on_focus
                && view.status.is_some()
                && matches!(view.screen, Screen::Status))
            {
                return;
            }
            // Refresh immediately on focus, but throttle: skip if we refreshed
            // recently (a manual `g r`, a post-action refresh, an auto-fetch, or
            // a prior focus), so rapid app-switching — or macOS firing several
            // activation events for one focus change — doesn't re-run a full
            // status each time.
            let recent = view.last_refresh.is_some_and(|t| {
                t.elapsed() < Duration::from_millis(FOCUS_REFRESH_COOLDOWN_MS)
            });
            if !recent {
                view.refresh(cx);
            }
        }));

        self._window_bounds_sub = Some(cx.observe_window_bounds(window, |view, window, cx| {
            let gen = view.window_bounds_save_gen.bump();
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                this.update_in(cx, |this, window, _cx| {
                    if this.window_bounds_save_gen.is_current(gen) {
                        save_window_state(this.worktree_scope_dir.as_deref(), window, _cx);
                    }
                })
                .ok();
            })
            .detach();
        }));
        save_window_state(self.worktree_scope_dir.as_deref(), window, cx);

        // Config file: watch its directory (so atomic save-via-rename, which
        // swaps the inode, still fires), forward matching events over a channel,
        // and re-apply on the UI thread. Watching the dir lets us pick up the
        // sibling transient-arguments.toml too, while ignoring other siblings (e.g.
        // command-usage.toml) by matching the exact paths.
        let Some(config_path) = config::path() else {
            return;
        };
        let Some(dir) = config_path.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        // Canonicalize the dir so the watch target matches the resolved paths the
        // OS reports (e.g. macOS reports `/private/tmp/…` for a `/tmp/…` watch).
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let watch_target = match config_path.file_name() {
            Some(name) => dir.join(name),
            None => return,
        };
        // Which watched file changed — kept distinct so a transient-arguments edit
        // doesn't run the config-reload path (theme rebuild, "Settings reloaded"
        // toast). All reload live, like the config always has.
        enum Changed {
            Config,
            TransientArguments,
            RepoTransientArguments,
        }
        let tv_target = config::transient_arguments_path()
            .and_then(|p| p.file_name().map(|n| dir.join(n)));
        // The repo scope's settings dir, if it exists yet (canonicalize fails
        // otherwise) — so we can watch its config.toml / transient-arguments.toml.
        // Created lazily on the first repo-scoped save, so a brand-new repo picks
        // it up next launch; an in-app save updates memory directly anyway.
        let repo_scope = self
            .repo_scope_dir
            .as_ref()
            .and_then(|d| std::fs::canonicalize(d).ok());
        let repo_tv_target = repo_scope.as_ref().map(|d| config::repo_transient_arguments_path(d));
        // For re-resolving the merged config: the plain repo config path (its
        // existence is checked at load time, so it works even if created later).
        let repo_config_load = self.repo_scope_dir.as_ref().map(|d| d.join("config.toml"));
        let cb_repo_tv = repo_tv_target.clone();
        let cb_repo_config = repo_scope.as_ref().map(|d| d.join("config.toml"));
        let (tx, rx) = async_channel::unbounded::<Changed>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                // Either config file (global or repo scope) re-resolves the merged
                // config — one path for both.
                if event.paths.contains(&watch_target)
                    || cb_repo_config.as_ref().is_some_and(|t| event.paths.contains(t))
                {
                    let _ = tx.send_blocking(Changed::Config);
                } else if tv_target.as_ref().is_some_and(|t| event.paths.contains(t)) {
                    let _ = tx.send_blocking(Changed::TransientArguments);
                } else if cb_repo_tv.as_ref().is_some_and(|t| event.paths.contains(t)) {
                    let _ = tx.send_blocking(Changed::RepoTransientArguments);
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        // A missing config dir (no config yet) just means nothing to watch.
        if notify::Watcher::watch(&mut watcher, &dir, notify::RecursiveMode::NonRecursive).is_err()
        {
            return;
        }
        // Also watch the repo's settings dir (a different directory) when present.
        if let Some(repo_scope) = &repo_scope {
            let _ = notify::Watcher::watch(
                &mut watcher,
                repo_scope,
                notify::RecursiveMode::NonRecursive,
            );
        }
        self._config_watcher = Some(watcher);

        // spawn_in so the reload has a Window: applying a config can rebuild the
        // open settings form, whose Select/Input entities need one.
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(changed) = rx.recv().await {
                let updated = match changed {
                    Changed::Config => {
                        let (cfg, warning) = config::load_merged(repo_config_load.as_deref());
                        this.update_in(cx, |view, window, cx| {
                            if let Some(warning) = warning {
                                // The file is now invalid/unreadable. Keep the
                                // live config (don't reset to defaults on a
                                // transient bad edit) and surface why it was
                                // ignored.
                                view.set_status(warning, false, cx);
                            } else if cfg != view.config {
                                // Skip an unchanged config (our own in-app save,
                                // or a no-op external edit).
                                view.apply_config(cfg, window, cx);
                            }
                        })
                    }
                    Changed::TransientArguments => {
                        let values = config::load_transient_arguments();
                        this.update_in(cx, |view, _window, cx| {
                            // Skip our own Ctrl-s save (we update in memory first,
                            // so the reload reads back identical values).
                            if values != view.transient_arguments {
                                view.transient_arguments = values;
                                view.set_status(
                                    "Argument defaults reloaded from disk".to_string(),
                                    true,
                                    cx,
                                );
                            }
                        })
                    }
                    Changed::RepoTransientArguments => {
                        let values = repo_tv_target
                            .as_ref()
                            .map(|p| config::load_transient_arguments_at(p))
                            .unwrap_or_default();
                        this.update_in(cx, |view, _window, cx| {
                            if values != view.repo_transient_arguments {
                                view.repo_transient_arguments = values;
                                view.set_status(
                                    "Argument defaults reloaded from disk".to_string(),
                                    true,
                                    cx,
                                );
                            }
                        })
                    }
                };
                if updated.is_err() {
                    break; // window closed
                }
            }
        })
        .detach();
    }

    /// Adopt a freshly-loaded config: store it, re-apply theme/appearance,
    /// update the font, and rebuild the effective keymap — so a `[keymap]` edit
    /// takes effect on save, like the other settings (any unknown id re-warns).
    fn apply_config(&mut self, cfg: config::Config, window: &mut Window, cx: &mut Context<Self>) {
        let fetch_changed = self.config.fetch != cfg.fetch;
        let update_check_changed = self.config.check_for_updates != cfg.check_for_updates;
        // Some settings change *fetched data*, not just how it's painted — the
        // title-bar tag segment (and commit ref labels), which status sections
        // are populated, and the recent-commit count. Those need a refresh to
        // take effect live; a repaint alone leaves them stale until the next one.
        let data_changed = self.config.show_tags_in_title_bar != cfg.show_tags_in_title_bar
            || self.config.status != cfg.status;
        self.config = cfg;
        // Keep the global-only copy current too (the watcher fires for both the
        // global and the repo file), so a later settings save writes back the
        // latest global config rather than a stale one.
        self.config_global = config::load_reporting().0;
        if fetch_changed {
            self.start_auto_fetch(cx);
        }
        if update_check_changed {
            self.start_update_checks(cx);
        }
        self.font = theme::resolve_font(&self.config, cx);
        self.ui_font = theme::resolve_ui_font(&self.config, cx);
        let (keymap, mut warnings) = build_keymap(&self.config);
        self.keymap = keymap;
        warnings.extend(theme::config_value_warnings(&self.config, cx));
        self.reapply_theme(cx);
        if data_changed {
            self.refresh(cx);
        }
        // The open settings form's dropdowns/inputs were built from the old
        // config, so rebuild it in place against the reloaded values rather than
        // leave stale controls. Only external edits reach here; our own in-app
        // saves are filtered out upstream by the unchanged-config guard.
        if self.settings().is_some() {
            self.open_settings(window, cx);
        }
        // Confirm every external reload, on any screen. Problems take priority
        // and stay until dismissed; a clean reload posts a fading confirmation.
        // Since each reload posts a fresh status, fixing the config and saving
        // replaces a prior warning with the confirmation — so a resolved warning
        // clears itself.
        if warnings.is_empty() {
            self.set_status("Settings reloaded from disk".to_string(), true, cx);
        } else {
            self.set_status(warnings.join("; "), false, cx);
        }
    }

    /// Re-apply the current config's theme and refresh everything that bakes in
    /// theme colors. Diff/status/plain row colors are stored in the `Row` model
    /// and the syntax-highlight cache is theme-derived, so a live theme switch
    /// must rebuild both — otherwise the screen keeps the old theme's colors.
    fn reapply_theme(&mut self, cx: &mut Context<Self>) {
        theme::apply_appearance(&self.config, cx);
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

    /// Mark the start of a background operation. The first concurrent op arms a
    /// short timer; if work is still in flight when it fires, the title-bar
    /// spinner appears — so sub-threshold operations never flash it. Pair every
    /// call with [`end_activity`](Self::end_activity) on completion.
    fn begin_activity(&mut self, cx: &mut Context<Self>) {
        self.activity += 1;
        if self.activity != 1 {
            return; // already counting; one arm-timer covers the whole busy span
        }
        let gen = self.busy_gen.bump();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BUSY_SPINNER_DELAY_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.busy_gen.is_current(gen) && this.activity > 0 && !this.busy {
                    this.busy = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Mark the end of a background operation. When the last one finishes the
    /// spinner is retired and any pending arm-timer is invalidated.
    fn end_activity(&mut self, cx: &mut Context<Self>) {
        self.activity = self.activity.saturating_sub(1);
        if self.activity == 0 {
            self.busy_gen.bump();
            if self.busy {
                self.busy = false;
                cx.notify();
            }
        }
    }

    /// Which keymap preset is active — for the handful of hardcoded
    /// act-at-point keys that differ between evil-collection and vanilla magit.
    pub(crate) fn is_evil(&self) -> bool {
        matches!(self.config.keymap_preset, config::KeymapPreset::EvilCollection)
    }

    pub(crate) fn is_vanilla(&self) -> bool {
        matches!(self.config.keymap_preset, config::KeymapPreset::Vanilla)
    }

    /// The repo cloned for a background *read* (status/diff/prefetch), tagged
    /// with the current generation's cancel flag so a later `refresh` kills it.
    fn read_repo(&self) -> Option<magritte_core::Repo> {
        self.repo
            .clone()
            .map(|r| r.with_cancel(self.read_cancel.clone()))
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Stamp the refresh so the focus-refresh throttle can tell how long it's
        // been since the status was last reloaded (by any path).
        self.last_refresh = Some(std::time::Instant::now());
        // Cancel the previous generation's in-flight reads (kill the processes,
        // not just drop their results) and start a fresh cancel scope.
        self.read_cancel.store(true, Ordering::Relaxed);
        self.read_cancel = Arc::new(AtomicBool::new(false));
        let stamp = self.generation.bump();
        let expanded_diff_keys: HashSet<(DiffSource, String)> = self
            .expanded
            .iter()
            .filter_map(|k| match k {
                FoldKey::File(source, path) => Some((*source, path.clone())),
                FoldKey::Section(_) | FoldKey::Hunk(..) => None,
            })
            .collect();
        self.diffs.retain(|key, _| expanded_diff_keys.contains(key));
        self.highlights.retain(|key, _| expanded_diff_keys.contains(key));
        self.diff_langs.retain(|key, _| expanded_diff_keys.contains(key));
        self.transient_config_defaults.clear();
        // Hunk indices shift when the diff changes, so don't carry collapse
        // state across a refresh.
        self.collapsed_hunks.clear();
        self.collapse_new_hunks = false;
        self.error = None;

        if self.read_repo().is_none() {
            self.error = Some(format!("Not a git repository: {}", self.root.display()));
            self.loading_sections.clear();
            self.rebuild_rows();
            return;
        }

        // The configured sections, so we only fetch what's actually shown.
        let configured: HashSet<SectionId> = self
            .config
            .status
            .section_ids()
            .iter()
            .filter_map(|id| SectionId::from_config_id(id))
            .collect();
        // Mark every configured section (except the conditional pushremote ones)
        // as refreshing. A section already on screen shows a spinner by its
        // header until its fetch lands; a first-load section has no data yet, so
        // it just pops in. The file sections clear when `git status` lands; each
        // auxiliary listing clears when its own fetch does.
        self.loading_sections = configured
            .iter()
            .copied()
            .filter(|s| {
                !matches!(
                    s,
                    SectionId::UnpushedPushremote | SectionId::UnpulledPushremote
                )
            })
            .collect();

        let recent_count = self.config.status.recent_count;
        let want_tags = self.config.show_tags_in_title_bar;
        let upstream_configured = configured.contains(&SectionId::Unpushed)
            || configured.contains(&SectionId::Unpulled);
        let pushremote_configured = configured.contains(&SectionId::UnpushedPushremote)
            || configured.contains(&SectionId::UnpulledPushremote);

        // PRIORITY: `git status` + the in-progress sequence. Renders the main
        // file sections (and the header) the moment it lands, before the
        // auxiliary listings — and kicks off upstream/pushremote divergence
        // afterward, since status tells us whether those targets exist.
        self.spawn_status_fetch(stamp, upstream_configured, pushremote_configured, cx);

        // Auxiliary listings, each its own fetch running concurrently with
        // status when it doesn't need status metadata, so a slow listing can't
        // hold up the main sections or the others. Each pops into place as it
        // lands; the title-bar spinner signals the work.
        if configured.contains(&SectionId::Recent) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Recent],
                cx,
                move |repo| repo.log("HEAD", recent_count).unwrap_or_default(),
                |this, recent| this.status_sections.recent = recent,
            );
        }
        if configured.contains(&SectionId::Stashes) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Stashes],
                cx,
                |repo| repo.stash_list().unwrap_or_default(),
                |this, stashes| this.status_sections.stashes = stashes,
            );
        }
        if configured.contains(&SectionId::Ignored) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Ignored],
                cx,
                |repo| repo.ignored_files().unwrap_or_default(),
                |this, ignored| this.status_sections.ignored = ignored,
            );
        }
        if want_tags {
            // Not a section (it's the title-bar tag segment), so it tracks no
            // section id — it just updates the header when it lands.
            self.spawn_fetch(
                stamp,
                &[],
                cx,
                |repo| repo.tags_around(),
                |this, tags| this.tag_info = tags,
            );
        } else {
            self.tag_info = (None, None);
        }
    }

    /// The priority fetch: `git status` and the in-progress sequence. Renders
    /// the main file sections and header as soon as it lands (restoring the
    /// cursor and re-warming diffs), then — now that the upstream/push targets
    /// are known — fetches those divergence sections only when they can exist.
    fn spawn_status_fetch(
        &mut self,
        stamp: u64,
        upstream_configured: bool,
        pushremote_configured: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        // Capture the cursor's logical position now (before the rebuild) so it
        // can be restored once status lands, rather than left at a stale index.
        let anchor = self.capture_anchor();
        let worktree_git_dir = self.worktree_git_dir.clone();
        let needs = RefreshNeeds {
            push_target: pushremote_configured,
        };
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let (result, sequence) = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = match worktree_git_dir.as_deref() {
                        Some(dir) => repo.refresh_snapshot_in_dir_with(dir, needs),
                        None => repo.refresh_snapshot_with(needs),
                    };
                    match snapshot {
                        Ok(snapshot) => (Ok(snapshot.status), snapshot.sequence),
                        Err(e) => (Err(e), None),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.end_activity(cx);
                if !this.generation.is_current(stamp) {
                    return;
                }
                this.sequence = sequence;
                match result {
                    Ok(status) => {
                        this.status = Some(status);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                // The file sections are now fresh — drop their refreshing spinner.
                for s in [SectionId::Untracked, SectionId::Unstaged, SectionId::Staged] {
                    this.loading_sections.remove(&s);
                }
                let has_upstream = this
                    .status
                    .as_ref()
                    .is_some_and(|s| s.head.upstream.is_some());
                let triangular = this
                    .status
                    .as_ref()
                    .is_some_and(|s| s.head.push.is_some());
                // Divergence sections only exist when their target exists; clear
                // any stale listings otherwise so they don't linger from a prior
                // state (do it before the rebuild so the rows reflect it).
                if upstream_configured && !has_upstream {
                    this.status_sections.unpushed.clear();
                    this.status_sections.unpulled.clear();
                    this.loading_sections.remove(&SectionId::Unpushed);
                    this.loading_sections.remove(&SectionId::Unpulled);
                }
                if pushremote_configured && triangular {
                    this.loading_sections.insert(SectionId::UnpushedPushremote);
                    this.loading_sections.insert(SectionId::UnpulledPushremote);
                } else {
                    this.status_sections.unpushed_pushremote.clear();
                    this.status_sections.unpulled_pushremote.clear();
                }
                this.rebuild_rows();
                this.restore_anchor(anchor);
                // Re-load diffs for any files that were expanded before the
                // refresh cleared them, so they don't get stuck on "Loading…".
                this.reload_expanded_diffs(cx);
                // Warm a bounded set of small diffs so first expand feels instant.
                this.start_prefetch(cx);
                // Now that status resolved the upstream/push targets, fetch the
                // divergence listings; they pop into place (or drop their
                // spinners) on land.
                if upstream_configured && has_upstream {
                    this.spawn_fetch(
                        stamp,
                        &[SectionId::Unpushed, SectionId::Unpulled],
                        cx,
                        |repo| repo.upstream_divergence().unwrap_or_default(),
                        |this, (up, down)| {
                            this.status_sections.unpushed = up;
                            this.status_sections.unpulled = down;
                        },
                    );
                }
                if pushremote_configured && triangular {
                    this.spawn_fetch(
                        stamp,
                        &[SectionId::UnpushedPushremote, SectionId::UnpulledPushremote],
                        cx,
                        |repo| repo.push_divergence().unwrap_or_default(),
                        |this, (up, down)| {
                            this.status_sections.unpushed_pushremote = up;
                            this.status_sections.unpulled_pushremote = down;
                        },
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Spawn one independent background section fetch: run `fetch` off the UI
    /// thread, then on the UI thread (if still the current generation) hand the
    /// result to `apply`, clear `sections` from the refreshing set, and rebuild
    /// — so the section pops in (or drops its spinner). Pairs
    /// `begin_activity`/`end_activity` so the busy spinner accounts for it.
    fn spawn_fetch<T: Send + 'static>(
        &mut self,
        stamp: u64,
        sections: &[SectionId],
        cx: &mut Context<Self>,
        fetch: impl FnOnce(Repo) -> T + Send + 'static,
        apply: impl FnOnce(&mut Self, T) + 'static,
    ) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        let sections = sections.to_vec();
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch(repo) })
                .await;
            this.update(cx, |this, cx| {
                this.end_activity(cx);
                if !this.generation.is_current(stamp) {
                    return;
                }
                apply(this, result);
                for s in &sections {
                    this.loading_sections.remove(s);
                }
                this.rebuild_rows();
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
            self.load_diff(source, path, true, cx);
        }
    }

    /// After a refresh, probe changed-line counts (cheap `git diff --numstat`)
    /// off the UI thread, then warm the diffs for a bounded number of small
    /// files so expanding them feels instant. Massive diffs are skipped and
    /// load lazily on explicit expand.
    fn start_prefetch(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        let generation = self.generation.current();

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
                if !this.generation.is_current(generation) {
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
        self.load_diff(source, path, false, cx);
    }

    /// Kick off a background diff load for a file. A forced reload preserves an
    /// existing loaded diff on screen until the replacement lands, so refreshing
    /// an expanded file never flashes a temporary "Loading…" body.
    fn load_diff(
        &mut self,
        source: DiffSource,
        path: String,
        replace_existing: bool,
        cx: &mut Context<Self>,
    ) {
        let key = (source, path.clone());
        if !replace_existing && self.diffs.contains_key(&key) {
            return;
        }
        let Some(repo) = self.read_repo() else {
            return;
        };
        if !self.diffs.contains_key(&key) {
            self.diffs.insert(key.clone(), DiffState::Loading);
        }
        let generation = self.generation.current();
        self.begin_activity(cx);

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
                this.end_activity(cx);
                if !this.generation.is_current(generation) {
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
                    // Fold level 3 (hunks closed) extends to diffs that were
                    // still loading when it was applied.
                    if this.collapse_new_hunks {
                        for ix in 0..diff.hunks.len() {
                            this.collapsed_hunks
                                .insert(FoldKey::Hunk(key.0, key.1.clone(), ix));
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
        // Refresh the conflicted-path set so is_conflicted (called per clickable
        // row in render) is an O(1) lookup, not an O(entries) scan per row.
        self.conflicted = self
            .status
            .as_ref()
            .map(|s| {
                s.entries
                    .iter()
                    .filter(|e| e.kind == EntryKind::Unmerged)
                    .map(|e| e.path.clone())
                    .collect()
            })
            .unwrap_or_default();

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

        // The branch and its upstream/push tracking live in the title bar (see
        // `render_title_bar`), not in header rows here. Sections render in the
        // configured order (`[status].sections`); an unknown id was warned about
        // at startup and is skipped here.
        let head = &status.head;
        let upstream = head.upstream.as_deref();
        // The distinct push target (triangular workflow), for the pushremote
        // sections; `None` when the push target is the upstream.
        let push = head.push.as_deref();
        // When there's nothing staged/unstaged/untracked, lead with the clean
        // notice — above the stashes/recent/log listings, not buried under them.
        if status.is_clean() {
            rows.push(spacer());
            rows.push(plain(
                "Nothing to commit, working tree clean",
                self.palette.dim,
            ));
        }
        for id in self.config.status.section_ids() {
            let Some(section) = SectionId::from_config_id(&id) else {
                continue;
            };
            match section {
                SectionId::Untracked => self.push_section(
                    &mut rows,
                    section,
                    "Untracked files",
                    status.untracked().collect(),
                    None,
                ),
                SectionId::Unstaged => self.push_section(
                    &mut rows,
                    section,
                    "Unstaged changes",
                    status.unstaged().collect(),
                    Some(DiffSource::Unstaged),
                ),
                SectionId::Staged => self.push_section(
                    &mut rows,
                    section,
                    "Staged changes",
                    status.staged().collect(),
                    Some(DiffSource::Staged),
                ),
                SectionId::Stashes => self.push_stash_section(&mut rows),
                SectionId::Unpushed => {
                    let title = match upstream {
                        Some(t) => format!("Unpushed to {t}"),
                        None => "Unpushed".to_string(),
                    };
                    let n = self.status_sections.unpushed.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpushed,
                        Some(n),
                    );
                }
                SectionId::Unpulled => {
                    let title = match upstream {
                        Some(t) => format!("Unpulled from {t}"),
                        None => "Unpulled".to_string(),
                    };
                    let n = self.status_sections.unpulled.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpulled,
                        Some(n),
                    );
                }
                SectionId::Recent => {
                    // Honor recent_count at render too, so lowering it takes
                    // effect on the next reload (the list is fetched at the
                    // count from the last status refresh).
                    let n = self
                        .config
                        .status
                        .recent_count
                        .min(self.status_sections.recent.len());
                    self.push_commit_section(
                        &mut rows,
                        section,
                        "Recent commits",
                        &self.status_sections.recent[..n],
                        // No count — the recent list is capped to recent_count.
                        None,
                    );
                }
                SectionId::UnpushedPushremote => {
                    let title = match push {
                        Some(t) => format!("Unpushed to {t}"),
                        None => "Unpushed to pushremote".to_string(),
                    };
                    let n = self.status_sections.unpushed_pushremote.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpushed_pushremote,
                        Some(n),
                    );
                }
                SectionId::UnpulledPushremote => {
                    let title = match push {
                        Some(t) => format!("Unpulled from {t}"),
                        None => "Unpulled from pushremote".to_string(),
                    };
                    let n = self.status_sections.unpulled_pushremote.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpulled_pushremote,
                        Some(n),
                    );
                }
                SectionId::Ignored => self.push_ignored_section(&mut rows),
            }
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
                count: Some(entries.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
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
                    status: status_label::status_label(entry, id),
                    status_color: status_label::status_color(entry, id, &self.palette),
                    label,
                    expanded: file_expanded,
                },
            });

            if let (Some(src), Some(true)) = (source, file_expanded) {
                self.push_file_body(rows, src, &file_ref);
            }
        }
    }

    /// A commit-listing section (unpushed/unpulled/recent): a foldable header
    /// over one `RowKind::Commit` per commit. Skipped when empty — a still-
    /// loading section simply isn't rendered until its fetch lands (it pops in).
    /// `count` is shown after the title when `Some` — `None` for the recent
    /// section, which is capped to a fixed number anyway.
    fn push_commit_section(
        &self,
        rows: &mut Vec<Row>,
        id: SectionId,
        title: &str,
        commits: &[LogEntry],
        count: Option<usize>,
    ) {
        if commits.is_empty() {
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
                count,
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        for c in commits {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::Commit {
                    hash: c.hash.clone(),
                    short_hash: c.short_hash.clone(),
                    subject: c.subject.clone(),
                    refs: parse_refs(&c.refs),
                },
            });
        }
    }

    /// The stashes section: a foldable header over one `RowKind::Stash` per
    /// entry. Skipped when there are no stashes.
    fn push_stash_section(&self, rows: &mut Vec<Row>) {
        let stashes = &self.status_sections.stashes;
        if stashes.is_empty() {
            return;
        }
        let id = SectionId::Stashes;
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: "Stashes".to_string(),
                count: Some(stashes.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        for s in stashes {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::Stash {
                    reference: s.reference.clone(),
                    message: s.message.clone(),
                },
            });
        }
    }

    /// The ignored-files section (opt-in): a foldable header over dim path rows
    /// (no staging — they're display-only). Skipped when there are none.
    fn push_ignored_section(&self, rows: &mut Vec<Row>) {
        let ignored = &self.status_sections.ignored;
        if ignored.is_empty() {
            return;
        }
        let id = SectionId::Ignored;
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: "Ignored files".to_string(),
                count: Some(ignored.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        for path in ignored {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::File {
                    status: String::new(),
                    status_color: self.palette.dim,
                    label: path.clone(),
                    expanded: None,
                },
            });
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
                            text: status_label::hunk_header_text(hunk),
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

    // --- Staging ----------------------------------------------------------

    // --- Popups (transients + help) --------------------------------------

    /// Advance and return the screen-load generation. A screen-changing async
    /// load captures this and re-checks it before mutating the screen.
    fn next_screen_gen(&mut self) -> u64 {
        self.screen_gen.bump()
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

/// The plain text of a row, for copying. A diff line yields its content without
/// the `+`/`-` sigil (so pasted code is clean); a file row joins its status word
/// and path.
fn row_text(row: &Row) -> String {
    match &row.kind {
        RowKind::Plain { text, .. } => text.clone(),
        RowKind::Section { title, .. } => title.clone(),
        RowKind::File { status, label, .. } => {
            if status.is_empty() {
                label.clone()
            } else {
                format!("{status}  {label}")
            }
        }
        RowKind::HunkHeader { text, .. } => text.clone(),
        RowKind::Diff { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        RowKind::Commit {
            short_hash,
            subject,
            ..
        } => format!("{short_hash}  {subject}"),
        RowKind::Stash { reference, message } => format!("{reference}  {message}"),
    }
}

/// How a `%D` ref decoration entry is classified, for coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RefKind {
    /// The current branch (`HEAD -> main`) or a detached `HEAD`.
    Head,
    Local,
    Remote,
    Tag,
}

/// Parse a commit's `%D` decoration (e.g. `HEAD -> main, origin/main, tag: v1`)
/// into labeled, classified entries for rendering.
fn parse_refs(refs: &str) -> Vec<(String, RefKind)> {
    refs.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            if let Some(tag) = entry.strip_prefix("tag: ") {
                (tag.to_string(), RefKind::Tag)
            } else if let Some(branch) = entry.strip_prefix("HEAD -> ") {
                (branch.to_string(), RefKind::Head)
            } else if entry == "HEAD" {
                ("HEAD".to_string(), RefKind::Head)
            } else if entry.contains('/') {
                (entry.to_string(), RefKind::Remote)
            } else {
                (entry.to_string(), RefKind::Local)
            }
        })
        .collect()
}

/// The plain text of a commit-view row, for copying (diff line content without
/// the `+`/`-` sigil).
fn commit_row_text(row: &CommitDiffRow) -> String {
    match row {
        CommitDiffRow::Detail(d) => d.clone(),
        CommitDiffRow::Message(m) => m.clone(),
        CommitDiffRow::File(p) => p.clone(),
        CommitDiffRow::Hunk(h) => h.clone(),
        CommitDiffRow::Line { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        CommitDiffRow::Note(n) => n.clone(),
    }
}

fn commit_metadata_lines(metadata: &CommitMetadata) -> Vec<String> {
    let mut lines = vec![
        format!("Author:    {}", metadata.author),
        format!("AuthorDate: {}", metadata.author_date),
        format!("Commit:    {}", metadata.committer),
        format!("CommitDate: {}", metadata.committer_date),
    ];
    if !metadata.refs.is_empty() {
        lines.push(format!("Refs:      {}", metadata.refs));
    }
    lines
}

fn prepend_commit_details(rows: &mut Vec<CommitDiffRow>, details: &[String]) {
    if details.is_empty() || rows.iter().any(|row| matches!(row, CommitDiffRow::Detail(_))) {
        return;
    }
    while matches!(rows.first(), Some(CommitDiffRow::Note(n)) if n.is_empty()) {
        rows.remove(0);
    }
    let mut prefix = details
        .iter()
        .cloned()
        .map(CommitDiffRow::Detail)
        .collect::<Vec<_>>();
    prefix.push(CommitDiffRow::Note(String::new()));
    rows.splice(0..0, prefix);
}

fn parse_release_version(version: &str) -> Option<(u64, u64, u64)> {
    let version = version
        .trim()
        .strip_prefix("refs/tags/")
        .unwrap_or(version.trim())
        .trim_start_matches('v');
    let stable = version.split_once('-').map_or(version, |(stable, _)| stable);
    let mut parts = stable.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn latest_release_version_from_github_json(body: &str) -> std::result::Result<String, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid GitHub response: {e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "GitHub response did not include tag_name".to_string())?;
    let version = tag.trim().trim_start_matches('v').to_string();
    if parse_release_version(&version).is_none() {
        return Err(format!("latest release tag is not a vX.Y.Z version: {tag}"));
    }
    Ok(version)
}

fn latest_release_version() -> std::result::Result<String, String> {
    let output = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "5",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--user-agent",
            concat!("magritte/", env!("CARGO_PKG_VERSION")),
            GITHUB_LATEST_RELEASE_API,
        ])
        .output()
        .map_err(|e| format!("failed to run curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("GitHub request exited with {}", output.status)
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    latest_release_version_from_github_json(&stdout)
}

fn version_status_message(current: &str, latest: &str) -> String {
    match (parse_release_version(current), parse_release_version(latest)) {
        (Some(current_version), Some(latest_version)) if current_version < latest_version => {
            format!("Magritte {current}; latest is {latest} (update available)")
        }
        (Some(current_version), Some(latest_version)) if current_version > latest_version => {
            format!("Magritte {current}; latest release is {latest}")
        }
        _ => format!("Magritte {current} is the latest version"),
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
        // A row's leading gutter must never shrink: flex items default to
        // flex-shrink 1, so on a row wide enough to overflow, a shrinking
        // chevron/spacer pulls everything after it left and breaks alignment.
        .flex_shrink_0()
        .text_color(color)
}

/// The default ignore pattern for a concrete path at point. Repo-local ignore
/// files get anchored paths (`/foo`) so ignoring a file named `foo` doesn't also
/// ignore every nested `foo`; a subdir `.gitignore` anchors the basename within
/// that subdirectory. This mirrors Magit's `magit-gitignore-read-pattern`,
/// which prefixes the current-file default with `/` for every ignore target.
fn default_ignore_pattern(command: transient::Command, file: Option<&str>) -> String {
    use transient::Command::*;
    match (command, file) {
        (IgnoreSubdir, Some(f)) => Path::new(f)
            .file_name()
            .map(|n| anchored_ignore_path(&n.to_string_lossy()))
            .unwrap_or_default(),
        (IgnoreToplevel | IgnorePrivate | IgnoreGlobal, Some(f)) => anchored_ignore_path(f),
        (_, Some(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn anchored_ignore_path(path: &str) -> String {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}

/// The revision scope for a `git log` invocation.
enum LogScope {
    /// HEAD / the current branch.
    Current,
    /// All refs (`--all`).
    All,
    /// A specific ref.
    Ref(String),
}

/// Assemble a `git log` argument list in the order git requires: flags and
/// options, a commit limit (defaulted when unset), the revision scope, then any
/// pathspecs behind a `--`.
fn build_log_args(
    mut flags: Vec<String>,
    scope: LogScope,
    paths: Vec<String>,
    limit: usize,
) -> Vec<String> {
    if !flags
        .iter()
        .any(|a| a.starts_with("-n") || a.starts_with("--max-count"))
    {
        flags.push(format!("--max-count={limit}"));
    }
    match scope {
        LogScope::Current => flags.push("HEAD".to_string()),
        LogScope::All => flags.push("--all".to_string()),
        LogScope::Ref(r) => flags.push(r),
    }
    if !paths.is_empty() {
        flags.push("--".to_string());
        flags.extend(paths);
    }
    flags
}

fn diff_title(base: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        base.to_string()
    } else if paths.len() == 1 {
        format!("{base} -- {}", paths[0])
    } else {
        format!("{base} -- {} paths", paths.len())
    }
}

/// The viewport height in rows — a "page" for the scroll/paging keys.
fn page_rows(window: &Window) -> usize {
    let height = window.viewport_size().height.as_f32();
    // Leave a few rows for the header/padding so paging keeps a little overlap.
    ((height / ROW_HEIGHT) as usize).saturating_sub(3).max(1)
}

/// Apply a vi-style scroll key to a `uniform_list`, updating the caller-tracked
/// top-row index (`top`) and scrolling the handle to it. We track `top`
/// ourselves because the handle's index getter is test-only. Returns whether
/// `key` was a recognized scroll command: `j`/`k` line, `Ctrl-d`/`Ctrl-u`
/// half-page, `Ctrl-f`/`Ctrl-b`/`Space` full-page, and `g`/`G` to the ends.
/// Half-page requires Ctrl so plain `d`/`u` stay free for future commands
/// (`d` diff, `u` unstage).
/// The new top-row index a scroll key moves to, or `None` if `key` isn't a
/// scroll command. Clamped so the last page stays on screen. Pure (no handle)
/// so the motion/clamp math is unit-testable; [`apply_scroll_key`] adds the
/// actual scroll. `j`/`k` line, `Ctrl-d`/`Ctrl-u` half-page, `Ctrl-f`/`Ctrl-b`/
/// `Space` full-page, `g`/`G` to the ends.
fn scroll_target(top: usize, len: usize, key: &str, shift: bool, ctrl: bool, page: usize) -> Option<usize> {
    let page = (page as isize).max(1);
    let half = (page / 2).max(1);
    let cur = top as isize;
    // The furthest the top can scroll: keep a full last page on screen rather
    // than scrolling content off the bottom.
    let max_top = (len as isize - page).max(0);
    let target = match key {
        "j" => cur + 1,
        "k" => cur - 1,
        "d" if ctrl => cur + half,
        "u" if ctrl => cur - half,
        "space" => cur + page,
        "f" if ctrl => cur + page,
        "b" if ctrl => cur - page,
        "g" if shift => max_top, // G → bottom (last page)
        "g" => 0,                // g → top
        _ => return None,
    };
    Some(target.clamp(0, max_top) as usize)
}

fn apply_scroll_key(
    handle: &UniformListScrollHandle,
    top: &mut usize,
    len: usize,
    key: &str,
    shift: bool,
    ctrl: bool,
    page: usize,
) -> bool {
    let Some(new_top) = scroll_target(*top, len, key, shift, ctrl, page) else {
        return false;
    };
    *top = new_top;
    let max_top = len.saturating_sub(page.max(1));
    // Strict scrolling positions the row even when it's already visible, so line
    // and half-page motions actually move. On the last page, pin the final row
    // to the *bottom* instead — the page-size estimate (header/padding overhead)
    // is slightly off, and pinning guarantees the very last row is reachable.
    if *top >= max_top && len > 0 {
        handle.scroll_to_item_strict(len - 1, gpui::ScrollStrategy::Bottom);
    } else {
        handle.scroll_to_item_strict(*top, gpui::ScrollStrategy::Top);
    }
    true
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

/// Launch a fresh copy in the background so the shell gets its prompt back
/// without continuing a forked process into AppKit. The child opts out of this
/// handoff with `MAGRITTE_FOREGROUND`, so it follows the normal app path.
fn detach_into_background(args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe)
        .args(args)
        .env("MAGRITTE_FOREGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

type RepoWindows = Rc<RefCell<HashMap<PathBuf, AnyWindowHandle>>>;

fn repo_window_key(start_dir: Option<&Path>) -> PathBuf {
    let root = start_dir
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    Repo::discover(&root)
        .map(|repo| repo.workdir().to_path_buf())
        .or_else(|_| std::fs::canonicalize(&root))
        .unwrap_or(root)
}

fn status_window_options(worktree_scope_dir: Option<&Path>, cx: &mut App) -> WindowOptions {
    // Restore the repo/worktree frame first, then the global default. On first
    // launch, avoid the stiff "exactly centered on the primary display" feel:
    // place a reasonably sized window near the top-left of the usable display
    // area, and let later windows cascade from the active Magritte window.
    let bounds = load_window_state(worktree_scope_dir)
        .and_then(|state| window_state_to_bounds(state, cx))
        .unwrap_or_else(|| WindowBounds::Windowed(default_status_window_bounds(cx)));
    WindowOptions {
        window_bounds: Some(bounds),
        // Transparent system bar so our custom `TitleBar` draws the chrome
        // (and the traffic lights sit where the component expects them).
        titlebar: Some(gpui_component::TitleBar::title_bar_options()),
        ..Default::default()
    }
}

fn load_window_state(worktree_scope_dir: Option<&Path>) -> Option<state::WindowState> {
    worktree_scope_dir
        .map(|dir| state::scoped_path(dir, state::WINDOW_FILE))
        .and_then(|path| state::load_toml_opt(&path))
        .or_else(|| {
            state::global_path(state::WINDOW_FILE)
                .as_deref()
                .and_then(state::load_toml_opt)
        })
}

fn save_window_state(worktree_scope_dir: Option<&Path>, window: &mut Window, cx: &mut App) {
    let state = window_state_from_window(window, cx);
    if let Some(dir) = worktree_scope_dir {
        state::save_toml(&state::scoped_path(dir, state::WINDOW_FILE), &state);
    }
    if let Some(path) = state::global_path(state::WINDOW_FILE) {
        state::save_toml(&path, &state);
    }
}

fn default_status_window_bounds(cx: &mut App) -> Bounds<gpui::Pixels> {
    if let Some(bounds) = cx
        .active_window()
        .and_then(|window| window.update(cx, |_, window, _| window.window_bounds().get_bounds()).ok())
    {
        return fit_window_bounds_on_display(
            Bounds::new(bounds.origin + point(px(25.0), px(25.0)), bounds.size),
            None,
            cx,
        );
    }

    let display = primary_visible_bounds(cx);
    fit_window_bounds_to_visible_bounds(
        Bounds::new(
            display.origin + point(px(80.0), px(60.0)),
            size(px(1000.0), px(720.0)),
        ),
        display,
    )
}

fn fit_window_bounds_on_display(
    bounds: Bounds<gpui::Pixels>,
    display_uuid: Option<&str>,
    cx: &mut App,
) -> Bounds<gpui::Pixels> {
    let displays = cx.displays();
    let display = display_uuid
        .and_then(|uuid| {
            displays
                .iter()
                .find(|display| display.uuid().ok().is_some_and(|id| id.to_string() == uuid))
                .cloned()
        })
        .or_else(|| {
            displays
                .iter()
                .find(|display| display.visible_bounds().intersects(&bounds))
                .cloned()
        })
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| primary_visible_bounds(cx));
    fit_window_bounds_to_visible_bounds(bounds, display)
}

fn fit_window_bounds_to_visible_bounds(
    bounds: Bounds<gpui::Pixels>,
    display: Bounds<gpui::Pixels>,
) -> Bounds<gpui::Pixels> {
    let width = bounds.size.width.max(px(640.0)).min(display.size.width);
    let height = bounds.size.height.max(px(420.0)).min(display.size.height);
    let max_x = display.origin.x + display.size.width - width;
    let max_y = display.origin.y + display.size.height - height;
    Bounds::new(
        point(
            bounds.origin.x.max(display.origin.x).min(max_x),
            bounds.origin.y.max(display.origin.y).min(max_y),
        ),
        size(width, height),
    )
}

fn primary_visible_bounds(cx: &App) -> Bounds<gpui::Pixels> {
    cx.primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(800.0))))
}

fn window_state_to_bounds(state: state::WindowState, cx: &mut App) -> Option<WindowBounds> {
    if !(state.x.is_finite()
        && state.y.is_finite()
        && state.width.is_finite()
        && state.height.is_finite())
        || state.width <= 0.0
        || state.height <= 0.0
    {
        return None;
    }
    let bounds = Bounds::new(
        point(px(state.x), px(state.y)),
        size(px(state.width), px(state.height)),
    );
    let bounds = fit_window_bounds_on_display(bounds, state.display_uuid.as_deref(), cx);
    Some(match state.mode {
        state::WindowMode::Windowed => WindowBounds::Windowed(bounds),
        state::WindowMode::Maximized => WindowBounds::Maximized(bounds),
        state::WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    })
}

fn window_state_from_window(window: &mut Window, cx: &mut App) -> state::WindowState {
    let display_uuid = window
        .display(cx)
        .and_then(|display| display.uuid().ok())
        .map(|uuid| uuid.to_string());
    let mode = match window.window_bounds() {
        WindowBounds::Windowed(_) => state::WindowMode::Windowed,
        WindowBounds::Maximized(_) => state::WindowMode::Maximized,
        WindowBounds::Fullscreen(_) => state::WindowMode::Fullscreen,
    };
    let bounds = window.window_bounds().get_bounds();
    state::WindowState {
        mode,
        display_uuid,
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
    }
}

fn discover_worktree_scope_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    let root = start_dir
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    Repo::discover(&root)
        .ok()
        .and_then(|repo| repo.git_dir().ok())
        .map(|dir| config::repo_dir(&dir))
}

fn open_repo_window(start_dir: Option<PathBuf>, cx: &mut App) -> Option<AnyWindowHandle> {
    let (cfg, cfg_warning) = config::load_reporting();
    theme::apply_appearance(&cfg, cx);
    let worktree_scope_dir = discover_worktree_scope_dir(start_dir.as_deref());
    let options = status_window_options(worktree_scope_dir.as_deref(), cx);
    let window = cx
        .open_window(options, |window, cx| {
            let view = cx.new(|cx| {
                StatusView::new(start_dir.clone(), cfg.clone(), cfg_warning.clone(), cx)
            });
            // Now that the window exists, install the live-reload watchers (the
            // appearance observer needs `&mut Window`).
            view.update(cx, |view, cx| {
                view.install_watchers(window, cx);
                view.start_auto_fetch(cx);
                view.start_update_checks(cx);
            });
            // The window's root must be a gpui-component Root (provides
            // theming, overlays, and the component context).
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        })
        .ok()?;
    Some(window.into())
}

fn open_or_focus_repo(
    start_dir: Option<PathBuf>,
    windows: &RepoWindows,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let key = repo_window_key(start_dir.as_deref());
    if let Some(handle) = windows.borrow().get(&key).copied() {
        if cx
            .update_window(handle, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return Some(handle);
        }
        windows.borrow_mut().remove(&key);
    }

    let handle = open_repo_window(start_dir, cx)?;
    windows.borrow_mut().insert(key, handle);
    cx.activate(true);
    Some(handle)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("magritte {CURRENT_VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check-version") {
        match latest_release_version() {
            Ok(latest) => println!("{}", version_status_message(CURRENT_VERSION, &latest)),
            Err(e) => {
                eprintln!("magritte: failed to check latest version: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "Usage: magritte [--foreground] [PATH]\n\n\
             Open the git repository containing PATH (default: current directory).\n\n\
             Options:\n\
               --foreground      Keep the app attached to the terminal.\n\
               --version, -V     Print the Magritte version.\n\
               --check-version   Check GitHub for the latest release.\n\n\
             Magritte detaches into the background so the shell returns immediately.\n\
             Pass --foreground (or set MAGRITTE_FOREGROUND) to keep it attached to\n\
             the terminal — handy for logs and debugging."
        );
        return;
    }
    // First non-flag argument is a path inside the repo to open (defaults to cwd).
    let start_dir = args.iter().find(|a| !a.starts_with('-')).map(PathBuf::from);
    let single_instance = ipc::enabled();
    if single_instance && ipc::try_handoff(start_dir.as_deref()) {
        return;
    }

    // Detach into the background by default, like a GUI app launched from a
    // shell. Opt out with --foreground or MAGRITTE_FOREGROUND (the debug harness
    // sets the latter so it can read the app's log and control channel).
    let foreground = args.iter().any(|a| a == "--foreground")
        || std::env::var_os("MAGRITTE_FOREGROUND").is_some();
    if !foreground && detach_into_background(&args) {
        return;
    }

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    app.run(move |cx: &mut App| {
        // Required before using any gpui-component widgets/themes.
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        // Apply the saved appearance/themes. Theme::change first ensures the
        // Theme global exists so apply_appearance can set its slots.
        let (cfg, _) = config::load_reporting();
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
        theme::apply_appearance(&cfg, cx);
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

        let windows: RepoWindows = Rc::new(RefCell::new(HashMap::new()));
        if single_instance {
            let (tx, rx) = async_channel::unbounded();
            if ipc::start_server(tx) {
                let windows_for_ipc = windows.clone();
                cx.spawn(async move |cx| {
                    while let Ok(path) = rx.recv().await {
                        let windows = windows_for_ipc.clone();
                        cx.update(|cx| {
                            open_or_focus_repo(Some(path), &windows, cx);
                        });
                    }
                })
                .detach();
            } else if ipc::try_handoff(start_dir.as_deref()) {
                cx.quit();
                return;
            }
        }

        if let Some(window) = open_or_focus_repo(start_dir.clone(), &windows, cx) {
            // Start the debug control channel (dev builds only; no-op unless
            // MAGRITTE_DEBUG_DIR is set). Debug mode opts out of single-instance
            // handoff, so this controls the isolated instance it launched.
            #[cfg(feature = "debug")]
            debug::init(window, cx);
            #[cfg(not(feature = "debug"))]
            let _ = window;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_registry_is_consistent() {
        use std::collections::HashSet;
        let (mut ids, mut keys, mut titles) = (HashSet::new(), HashSet::new(), HashSet::new());
        for c in commands() {
            assert!(ids.insert(c.id), "duplicate command id: {}", c.id);
            // Titles must be unique — the `:` palette resolves the chosen title
            // back to its command.
            assert!(
                titles.insert(c.title),
                "duplicate command title: {}",
                c.title
            );
            // Keys (when bound) must be unique; leaves carry no top-level key.
            if let Some(key) = c.key {
                assert!(keys.insert(key), "duplicate command key: {key}");
            }
            // Surface invariants: a `?`-menu command needs a key in at least one
            // preset (the menu drops keyless entries per preset); a command with
            // no key anywhere must be palette-only.
            let bound_somewhere = c.key.is_some()
                || default_key_for_command(config::KeymapPreset::Vanilla, c).is_some();
            if c.menu {
                assert!(
                    bound_somewhere,
                    "menu command {:?} has no key in any preset",
                    c.id
                );
            }
            if !bound_somewhere {
                assert!(
                    c.palette,
                    "keyless command {:?} should be in the palette",
                    c.id
                );
            }
        }
        // Every menu command is reachable from the `?` dispatch menu, in each
        // preset where it has a default key.
        for preset in [
            config::KeymapPreset::EvilCollection,
            config::KeymapPreset::Vanilla,
        ] {
            let config = config::Config {
                keymap_preset: preset,
                ..config::Config::default()
            };
            let km = build_keymap(&config).0;
            let menu: HashSet<String> = dispatch_menu(&km, &config)
                .groups
                .iter()
                .flat_map(|g| &g.suffixes)
                .filter_map(|s| match s {
                    Suffix::Info(i) => Some(i.keys.clone()),
                    _ => None,
                })
                .collect();
            for c in commands().iter().filter(|c| c.menu) {
                let Some(key) = default_key_for_command(preset, c) else {
                    continue;
                };
                assert!(
                    menu.contains(key),
                    "menu command {:?} ({key}) missing from the {} dispatch menu",
                    c.id,
                    preset.as_str()
                );
            }
        }
    }

    #[test]
    fn is_dispatch_key_matches_single_key_menu_rows() {
        // Against the default keymap: single-key commands route; multi-stroke /
        // g-prefix entries don't.
        let (km, warnings) = build_keymap(&config::Config::default());
        assert!(warnings.is_empty(), "default config has no keymap warnings");
        assert!(StatusView::is_dispatch_key(&km, "c"));
        assert!(StatusView::is_dispatch_key(&km, "s"));
        assert!(StatusView::is_dispatch_key(&km, "G"));
        assert!(!StatusView::is_dispatch_key(&km, "tab"));
        assert!(!StatusView::is_dispatch_key(&km, "g g"));
        assert!(!StatusView::is_dispatch_key(&km, "g r"));
        assert!(!StatusView::is_dispatch_key(&km, "z")); // not bound by default
    }

    #[test]
    fn negatable_switch_emits_relative_to_config_default() {
        // A negatable switch (e.g. --gpg-sign, tied to commit.gpgSign) emits a
        // flag only when its toggle differs from the configured default: the
        // positive arg when turned on, the negation when turned off.
        let args = |config_default: bool, on: bool| {
            let mut sw = transient::Switch::negatable(
                "-S",
                "--gpg-sign",
                "--no-gpg-sign",
                "commit.gpgSign",
                "Sign using gpg",
            );
            sw.default_on = config_default;
            let def = Transient {
                title: transient::plain_title("Commit"),
                groups: vec![transient::Group {
                    title: transient::plain_title("Arguments"),
                    suffixes: vec![Suffix::Switch(sw)],
                }],
            };
            let mut state = TransientState::new("commit", def, RemoteTargets::default());
            if on {
                state.active.insert("-S".into());
            } else {
                state.active.remove("-S");
            }
            state.args()
        };
        // Config off: nothing when off, --gpg-sign when the user turns it on.
        assert!(args(false, false).is_empty());
        assert_eq!(args(false, true), vec!["--gpg-sign"]);
        // Config on: nothing when left on (git signs anyway), --no-gpg-sign when
        // the user turns it off.
        assert!(args(true, true).is_empty());
        assert_eq!(args(true, false), vec!["--no-gpg-sign"]);
    }

    #[test]
    fn saved_set_reconciles_with_config_defaults() {
        // A transient with one plain switch (-a) and one negatable, config-derived
        // switch (-S, e.g. --gpg-sign). Build it with a given config default.
        let build = |gpg_default: bool, saved: &[&str]| {
            let mut gpg = transient::Switch::negatable(
                "-S",
                "--gpg-sign",
                "--no-gpg-sign",
                "commit.gpgSign",
                "Sign using gpg",
            );
            gpg.default_on = gpg_default;
            let def = Transient {
                title: transient::plain_title("Commit"),
                groups: vec![transient::Group {
                    title: transient::plain_title("Arguments"),
                    suffixes: vec![
                        Suffix::Switch(transient::Switch::new("-a", "--all", "Stage all")),
                        Suffix::Switch(gpg),
                    ],
                }],
            };
            let saved: Vec<String> = saved.iter().map(|s| s.to_string()).collect();
            let active = TransientState::apply_saved(&def, &saved);
            let mut state = TransientState::new("commit", def, RemoteTargets::default());
            state.active = active;
            state
        };
        let on = |s: &TransientState, k: &str| s.active.contains(k);

        // The reported bug: an empty saved set must NOT force a config-on switch
        // off — it keeps the config default.
        let s = build(true, &[]);
        assert!(on(&s, "-S"), "empty saved set keeps gpg-sign on when config enables it");
        assert!(!on(&s, "-a"));
        // Config off + empty: stays off.
        assert!(!on(&build(false, &[]), "-S"));

        // Explicit forms override the config default either way, and round-trip.
        let off = build(true, &["--no-gpg-sign"]); // forced off against config-on
        assert!(!on(&off, "-S"));
        assert_eq!(off.saved_overrides(), vec!["--no-gpg-sign".to_string()]);
        let forced_on = build(false, &["-S"]); // forced on against config-off
        assert!(on(&forced_on, "-S"));
        assert_eq!(forced_on.saved_overrides(), vec!["-S".to_string()]);

        // A switch matching its config default isn't persisted (config drives it);
        // a plain switch persists by its key.
        let mut s = build(true, &["-a"]);
        assert!(on(&s, "-S") && on(&s, "-a"));
        assert_eq!(s.saved_overrides(), vec!["-a".to_string()]);
        // Turn the config-on switch off → it now persists as the negation.
        s.active.remove("-S");
        assert_eq!(
            s.saved_overrides(),
            vec!["--no-gpg-sign".to_string(), "-a".to_string()]
        );
    }

    #[test]
    fn saved_set_round_trips_option_values() {
        let def = transient::log_transient();
        let saved = vec![
            "-r".to_string(),
            "-n=50".to_string(),
            "-F=fix bug".to_string(),
            "--=src/main.rs".to_string(),
        ];
        let mut state = TransientState::new("log", def, RemoteTargets::default());
        state.active = TransientState::apply_saved(&state.def, &saved);
        state.values = TransientState::apply_saved_values(&state.def, &saved);
        assert!(state.active.contains("-r"));
        assert_eq!(state.values.get("-n").map(String::as_str), Some("50"));
        assert_eq!(state.values.get("-F").map(String::as_str), Some("fix bug"));
        assert_eq!(
            state.values.get("--").map(String::as_str),
            Some("src/main.rs")
        );
        assert_eq!(
            state.args(),
            vec![
                "--reverse".to_string(),
                "-n50".to_string(),
                "--grep=fix bug".to_string(),
            ]
        );
        assert_eq!(state.pathspecs(), vec!["src/main.rs".to_string()]);
        assert_eq!(
            state.saved_overrides(),
            vec![
                "--=src/main.rs".to_string(),
                "-F=fix bug".to_string(),
                "-n=50".to_string(),
                "-r".to_string(),
            ]
        );
    }

    #[test]
    fn status_unknown_section_warns() {
        let mut config = config::Config::default();
        config.status.sections = vec!["staged".into(), "bogus".into()];
        let (_, warnings) = build_keymap(&config);
        assert!(
            warnings.iter().any(|w| w.contains("unknown section \"bogus\"")),
            "expected an unknown-section warning, got {warnings:?}"
        );
    }

    #[test]
    fn keymap_remap_unbind_and_unknown_id() {
        let mut config = config::Config::default();
        config.keymap.insert("K".into(), "branch-delete".into()); // remap
        config.keymap.insert("x".into(), "unbound".into()); // unbind
        config.keymap.insert("Q".into(), "no-such-command".into()); // unknown
        let (km, warnings) = build_keymap(&config);
        assert_eq!(km.get("K").map(String::as_str), Some("branch-delete"));
        assert!(!km.contains_key("x"), "x was unbound");
        assert!(!km.contains_key("Q"), "unknown id isn't bound");
        assert_eq!(warnings.len(), 1, "the unknown id warns: {warnings:?}");
        // Defaults the user didn't touch survive.
        assert_eq!(km.get("c").map(String::as_str), Some("commit"));
    }

    #[test]
    fn keymap_preset_switches_defaults_and_transient_suffixes() {
        let config = config::Config::default();
        let (km, warnings) = build_keymap(&config);
        assert!(warnings.is_empty(), "default keymap is clean: {warnings:?}");
        assert_eq!(km.get("p").map(String::as_str), Some("push"));
        assert_eq!(km.get("O").map(String::as_str), Some("reset"));
        assert_eq!(km.get("Z").map(String::as_str), Some("stash"));
        assert_eq!(km.get("|").map(String::as_str), Some("git-command"));
        assert_eq!(km.get("V").map(String::as_str), Some("visual"));
        assert_eq!(command_keys(&km, &config, "Delete branch").as_deref(), Some("b x"));
        assert_eq!(command_keys(&km, &config, "Delete tag").as_deref(), Some("t x"));
        assert_eq!(command_keys(&km, &config, "Remove remote").as_deref(), Some("M x"));

        let mut config = config::Config::default();
        config.keymap_preset = config::KeymapPreset::Vanilla;
        let (km, warnings) = build_keymap(&config);
        assert!(warnings.is_empty(), "vanilla keymap is clean: {warnings:?}");
        assert_eq!(km.get("P").map(String::as_str), Some("push"));
        assert_eq!(km.get("X").map(String::as_str), Some("reset"));
        assert_eq!(km.get("z").map(String::as_str), Some("stash"));
        assert_eq!(km.get("k").map(String::as_str), Some("discard"));
        assert_eq!(km.get("n").map(String::as_str), Some("next-section"));
        assert_eq!(km.get("p").map(String::as_str), Some("prev-section"));
        assert_eq!(km.get(":").map(String::as_str), Some("git-command"));
        assert_eq!(km.get("!").map(String::as_str), Some("git-command"));
        assert!(!km.contains_key("V"), "vanilla leaves V for commit-at-point revert");
        assert_eq!(command_keys(&km, &config, "Push").as_deref(), Some("P"));
        assert_eq!(command_keys(&km, &config, "Reset").as_deref(), Some("X"));
        assert_eq!(command_keys(&km, &config, "Stash").as_deref(), Some("z"));
        assert_eq!(command_keys(&km, &config, "Discard").as_deref(), Some("k"));
        assert_eq!(command_keys(&km, &config, "Delete branch").as_deref(), Some("b k"));
        assert_eq!(command_keys(&km, &config, "Delete tag").as_deref(), Some("t k"));
        assert_eq!(command_keys(&km, &config, "Remove remote").as_deref(), Some("M k"));
    }

    #[test]
    fn keymap_sequences_any_depth() {
        let mut config = config::Config::default();
        config.keymap.insert("g x".into(), "stage".into()); // 2-key sequence
        config.keymap.insert(". c".into(), "commit".into()); // a `.` prefix
        config.keymap.insert("a b c".into(), "stage".into()); // 3-key chain
        let (km, warnings) = build_keymap(&config);
        assert_eq!(km.get("g x").map(String::as_str), Some("stage"));
        assert_eq!(km.get(". c").map(String::as_str), Some("commit"));
        assert_eq!(km.get("a b c").map(String::as_str), Some("stage"));
        assert!(warnings.is_empty(), "any-depth sequence is fine: {warnings:?}");
    }

    #[test]
    fn custom_command_is_a_valid_bind_target() {
        let mut config = config::Config::default();
        config.commands.push(config::CustomCommand {
            id: "user.wip".into(),
            title: "WIP".into(),
            run: "git commit -a -m WIP".into(),
            refresh: true,
            section: None,
        });
        config.keymap.insert("X".into(), "user.wip".into());
        config.keymap.insert("Y".into(), "user.nope".into()); // unknown id
        // Injected into a transient too — a valid target there, no warning.
        config
            .transient
            .entry("commit".into())
            .or_default()
            .insert("W".into(), config::TransientSuffix::Bare("user.wip".into()));
        let (km, warnings) = build_keymap(&config);
        assert_eq!(km.get("X").map(String::as_str), Some("user.wip"));
        assert!(!km.contains_key("Y"), "unknown id isn't bound");
        assert_eq!(warnings.len(), 1, "only the unknown id warns: {warnings:?}");
    }

    /// A `[transient.<id>]` switch — a bare `-`-flag string or a `{ flag, … }`
    /// table — is valid without naming a command, but its key must be
    /// dash-prefixed to toggle.
    #[test]
    fn transient_switch_injection_validates() {
        use config::TransientSuffix as Sfx;
        let mut config = config::Config::default();
        let commit = config.transient.entry("commit".into()).or_default();
        commit.insert("-d".into(), Sfx::Bare("--depth=1".into())); // bare flag, ok
        commit.insert(
            "-n".into(),
            Sfx::Switch {
                flag: "--no-verify".into(),
                description: "Skip hooks".into(),
                group: None,
            },
        ); // table form, ok
        commit.insert("x".into(), Sfx::Bare("--depth=1".into())); // flag, non-dash key
        let (_, warnings) = build_keymap(&config);
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("\"-d\"") || w.contains("\"-n\"")),
            "dash-keyed switches are fine: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("\"x\"") && w.contains("dash-prefixed")),
            "a non-dash switch key warns: {warnings:?}"
        );
    }

    /// The palette key-hint resolves a user command's binding — a `[keymap]`
    /// entry, or a `[transient.<id>]` injection as `<prefix> <key>`.
    #[test]
    fn command_keys_for_user_command() {
        let mut config = config::Config::default();
        config.commands.push(config::CustomCommand {
            id: "user.wip".into(),
            title: "WIP commit".into(),
            run: "git commit -m WIP".into(),
            refresh: true,
            section: None,
        });
        // Injected into the commit transient at a free key → reached via `c W`.
        config
            .transient
            .entry("commit".into())
            .or_default()
            .insert("W".into(), config::TransientSuffix::Bare("user.wip".into()));
        let (km, _) = build_keymap(&config);
        assert_eq!(
            command_keys(&km, &config, "WIP commit").as_deref(),
            Some("c W")
        );

        // A direct keymap binding is shown directly.
        config.keymap.insert("g w".into(), "user.wip".into());
        let (km, _) = build_keymap(&config);
        assert_eq!(
            command_keys(&km, &config, "WIP commit").as_deref(),
            Some("g w")
        );
    }

    /// An injection whose key is shadowed by a built-in suffix (`w` = Reword in
    /// the commit transient) is dropped, so no key is shown for it.
    #[test]
    fn command_keys_skips_shadowed_injection() {
        let mut config = config::Config::default();
        config.commands.push(config::CustomCommand {
            id: "user.wip".into(),
            title: "WIP commit".into(),
            run: "git commit -m WIP".into(),
            refresh: true,
            section: None,
        });
        config
            .transient
            .entry("commit".into())
            .or_default()
            .insert("w".into(), config::TransientSuffix::Bare("user.wip".into()));
        let (km, _) = build_keymap(&config);
        assert_eq!(command_keys(&km, &config, "WIP commit"), None);
    }

    /// A `[keymap]` sequence whose prefix already runs a command is unreachable
    /// (exact match wins), so it warns — pointing at the transient when the
    /// shadower is one. Unbinding the prefix makes it reachable, no warning.
    #[test]
    fn keymap_warns_on_shadowed_sequence() {
        let mut config = config::Config::default();
        config.keymap.insert("c W".into(), "stage".into()); // `c` runs commit
        let (_, warnings) = build_keymap(&config);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("\"c W\"") && w.contains("[transient.commit]")),
            "expected a shadow warning pointing at the transient: {warnings:?}"
        );

        // Free `c` and the sequence is reachable — no shadow warning.
        config.keymap.insert("c".into(), "unbound".into());
        let (_, warnings) = build_keymap(&config);
        assert!(
            !warnings.iter().any(|w| w.contains("unreachable")),
            "unbinding the prefix clears the shadow: {warnings:?}"
        );
    }

    #[test]
    fn command_toast_caps_long_output() {
        let run = |out: &str| magritte_core::CommandRun {
            ok: true,
            stdout: out.to_string(),
            stderr: String::new(),
        };
        // Short output passes through unchanged.
        assert_eq!(command_toast(&run("a\nb\nc"), Some("$")), "a\nb\nc");

        // Long output is cut to the cap plus one hint line pointing at the log.
        let long = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let toast = command_toast(&run(&long), Some("$"));
        assert_eq!(toast.lines().count(), MAX_TOAST_LINES + 1);
        assert!(toast.contains("line 1\n") && toast.contains(&format!("line {MAX_TOAST_LINES}")));
        assert!(!toast.contains(&format!("line {}", MAX_TOAST_LINES + 1)));
        assert!(toast.contains("more lines") && toast.contains("press $"));

        // No `$` binding → directs to the command log without a key.
        assert!(command_toast(&run(&long), None).contains("open the command log"));
    }

    #[test]
    fn custom_destructive_detection() {
        assert!(command_is_destructive("git reset --hard HEAD"));
        assert!(command_is_destructive("git clean -fd"));
        assert!(command_is_destructive("git push --force"));
        assert!(!command_is_destructive("git pull --rebase && git push"));
        assert!(!command_is_destructive("git commit -a -m WIP"));
    }

    #[test]
    fn ignore_prompt_defaults_anchor_repo_local_paths() {
        use transient::Command::*;

        assert_eq!(
            default_ignore_pattern(IgnoreToplevel, Some("src/generated/file.rs")),
            "/src/generated/file.rs"
        );
        assert_eq!(
            default_ignore_pattern(IgnorePrivate, Some("build/output.log")),
            "/build/output.log"
        );
        assert_eq!(
            default_ignore_pattern(IgnoreSubdir, Some("src/generated/file.rs")),
            "/file.rs"
        );
        assert_eq!(
            default_ignore_pattern(IgnoreGlobal, Some("build/output.log")),
            "/build/output.log"
        );
    }

    /// Guards against forgetting to surface a command: the `?` dispatch menu and
    /// the set of keys `run_dispatch` handles must agree, so a command can't be
    /// invocable-but-hidden or shown-but-dead. The menu is generated from the
    /// `commands` registry; this pins the *motions* (handled inline in
    /// `run_dispatch`) plus the registry keys to one explicit list, so adding a
    /// command or motion without surfacing it fails here (or goes in `OVERRIDES`,
    /// for genuine exceptions).
    #[test]
    fn dispatch_menu_covers_every_command() {
        use std::collections::HashSet;

        // The keys `run_dispatch` handles: every registry command key, plus the
        // inline motions.
        const DISPATCH_KEYS: &[&str] = &[
            "c", "b", "t", "M", "Z", "l", "d", "p", "F", "f", "O", "m", "r", "i", "!", ",", "$", // commands
            "s", "u", "S", "U", "x", // applying changes
            "v", "y", "tab", "g r", ":", "enter", // essential + open file + palette
            "j", "k", "g g", "G", "g j", "g k", // navigation / motions
            "ctrl-d", "ctrl-u", "ctrl-f", "ctrl-b", // half/full page motions
        ];
        // Keys allowed to be on only one side of the check. Cursor motions
        // dispatch but are intentionally hidden from the `?` menu (standard
        // vim/emacs conventions — see the `nav!` block in commands.rs).
        const OVERRIDES: &[&str] = &[
            "j", "k", "g g", "G", "g j", "g k", "ctrl-d", "ctrl-u", "ctrl-f", "ctrl-b",
        ];

        let config = config::Config::default();
        let km = build_keymap(&config).0;
        let menu: HashSet<String> = dispatch_menu(&km, &config)
            .groups
            .iter()
            .flat_map(|g| &g.suffixes)
            .filter_map(|s| match s {
                Suffix::Info(i) => Some(i.keys.clone()),
                _ => None,
            })
            .collect();
        let dispatched: HashSet<String> = DISPATCH_KEYS.iter().map(|s| s.to_string()).collect();
        let overrides: HashSet<String> = OVERRIDES.iter().map(|s| s.to_string()).collect();

        let missing_from_menu: Vec<&String> = dispatched
            .difference(&menu)
            .filter(|k| !overrides.contains(*k))
            .collect();
        assert!(
            missing_from_menu.is_empty(),
            "dispatchable commands missing from the `?` menu (add them to dispatch_menu \
             or OVERRIDES): {missing_from_menu:?}"
        );

        let missing_handler: Vec<&String> = menu
            .difference(&dispatched)
            .filter(|k| !overrides.contains(*k))
            .collect();
        assert!(
            missing_handler.is_empty(),
            "`?` menu rows with no run_dispatch handler (add them to DISPATCH_KEYS \
             or OVERRIDES): {missing_handler:?}"
        );
    }

    #[test]
    fn parse_refs_classifies_decorations() {
        let got = parse_refs("HEAD -> main, origin/main, tag: v1.0, feature, HEAD");
        assert_eq!(
            got,
            vec![
                ("main".to_string(), RefKind::Head),
                ("origin/main".to_string(), RefKind::Remote),
                ("v1.0".to_string(), RefKind::Tag),
                ("feature".to_string(), RefKind::Local),
                ("HEAD".to_string(), RefKind::Head),
            ]
        );
        assert!(parse_refs("").is_empty());
    }

    #[test]
    fn release_version_parsing_and_status() {
        assert_eq!(parse_release_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_release_version("refs/tags/v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_release_version("1.2"), None);
        assert_eq!(
            latest_release_version_from_github_json(r#"{"tag_name":"v1.2.3"}"#).unwrap(),
            "1.2.3"
        );
        assert!(latest_release_version_from_github_json(r#"{"tag_name":"nightly"}"#).is_err());
        assert!(version_status_message("0.3.0", "0.4.0").contains("update available"));
        assert!(version_status_message(CURRENT_VERSION, CURRENT_VERSION).contains("latest version"));
    }

    #[test]
    fn commit_details_toggle_does_not_accumulate_blank_lines() {
        let details = vec!["Author:    A".to_string()];
        let mut rows = vec![
            CommitDiffRow::Note(String::new()),
            CommitDiffRow::File("a.txt".to_string()),
        ];

        prepend_commit_details(&mut rows, &details);
        rows.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
        prepend_commit_details(&mut rows, &details);

        assert!(matches!(rows.first(), Some(CommitDiffRow::Detail(_))));
        assert!(matches!(rows.get(1), Some(CommitDiffRow::Note(n)) if n.is_empty()));
        assert!(matches!(rows.get(2), Some(CommitDiffRow::File(_))));
    }

    #[test]
    fn scroll_target_motions_and_clamping() {
        // len 100, page 10 → max_top 90.
        assert_eq!(scroll_target(5, 100, "j", false, false, 10), Some(6));
        assert_eq!(scroll_target(5, 100, "k", false, false, 10), Some(4));
        assert_eq!(scroll_target(0, 100, "k", false, false, 10), Some(0)); // clamp low
        assert_eq!(scroll_target(5, 100, "d", false, true, 10), Some(10)); // half page
        assert_eq!(scroll_target(5, 100, "u", false, true, 10), Some(0));
        assert_eq!(scroll_target(5, 100, "space", false, false, 10), Some(15));
        assert_eq!(scroll_target(0, 100, "g", true, false, 10), Some(90)); // G → bottom
        assert_eq!(scroll_target(50, 100, "g", false, false, 10), Some(0)); // g → top
        assert_eq!(scroll_target(85, 100, "space", false, false, 10), Some(90)); // clamp high
        // Half-page needs ctrl; plain d/u (and unknown keys) aren't scrolls.
        assert_eq!(scroll_target(5, 100, "d", false, false, 10), None);
        assert_eq!(scroll_target(5, 100, "z", false, false, 10), None);
        // Fewer rows than a page → max_top 0, everything pins to top.
        assert_eq!(scroll_target(0, 3, "j", false, false, 10), Some(0));
    }

    #[test]
    fn build_log_args_orders_and_defaults() {
        // Default limit injected; scope appended; pathspecs behind `--`.
        assert_eq!(
            build_log_args(vec!["--reverse".into()], LogScope::Current, vec![], 20),
            vec!["--reverse", "--max-count=20", "HEAD"]
        );
        assert_eq!(
            build_log_args(vec![], LogScope::All, vec!["a.txt".into()], 5),
            vec!["--max-count=5", "--all", "--", "a.txt"]
        );
        // An explicit -n / --max-count suppresses the default.
        assert_eq!(
            build_log_args(vec!["-n3".into()], LogScope::Ref("dev".into()), vec![], 20),
            vec!["-n3", "dev"]
        );
    }

    #[test]
    fn row_text_strips_diff_sigils_and_joins_fields() {
        let commit = Row {
            indent: 1,
            selectable: true,
            fold: None,
            target: None,
            kind: RowKind::Commit {
                hash: "abc".into(),
                short_hash: "abc123".into(),
                subject: "Do a thing".into(),
                refs: Vec::new(),
            },
        };
        assert_eq!(row_text(&commit), "abc123  Do a thing");

        let stash = Row {
            indent: 1,
            selectable: true,
            fold: None,
            target: None,
            kind: RowKind::Stash {
                reference: "stash@{0}".into(),
                message: "WIP".into(),
            },
        };
        assert_eq!(row_text(&stash), "stash@{0}  WIP");
    }
}

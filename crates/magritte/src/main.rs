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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, div, px, uniform_list, AnyElement, AnyWindowHandle, App, ClipboardItem, Context,
    Entity, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, KeyBinding, KeyDownEvent, Menu,
    MenuItem, MouseButton, MouseDownEvent, SharedString, Styled, UniformListScrollHandle,
};

mod app_icon;
mod blame_view;
mod commands;
mod commit_diff_view;
mod commit_editor;
mod commit_text;
mod config;
mod controller;
#[cfg(feature = "debug")]
mod debug;
mod editor_launch;
mod editors;
mod generation;
mod git_action;
mod highlight;
mod input;
mod ipc;
mod kbd;
mod log_view;
mod navigation;
mod palette;
mod picker;
mod refs_view;
mod render;
mod row_build;
mod settings;
mod staging;
mod state;
mod status_label;
mod status_loader;
mod targets;
mod theme;
mod transient_state;
mod watchers;
mod window;
mod worktree_view;
pub(crate) use commands::*;
pub(crate) use commit_diff_view::*;
pub(crate) use commit_editor::*;
use controller::StatusToast;
pub(crate) use log_view::*;
pub(crate) use navigation::*;
pub(crate) use palette::*;
pub(crate) use refs_view::*;
pub(crate) use row_build::*;
pub(crate) use staging::*;
pub(crate) use transient_state::*;
pub(crate) use window::*;
pub(crate) use worktree_view::*;

/// See [`StatusView::git_log_rows`]: (command-log sequence, show-all, rows).
type GitLogCache = RefCell<Option<(u64, bool, Rc<Vec<GitLogRow>>)>>;
use generation::Generation;
use git_action::{describe_discard, Action, HunkSelections, Op, RegionKind};
use highlight::{file_head_tail, FileHighlights, Span};
use picker::{CreateMode, PickerList};
use settings::SettingsCaches;

/// Key context for our status view, used so our `tab` binding takes precedence
/// over gpui-component Root's focus-navigation `tab`.
const STATUS_CONTEXT: &str = "MagritteStatus";

// Tab and Shift-Tab are bound by gpui-component's Root (focus nav) and so never
// reach an on_key_down listener; we override them with actions in our key context.
actions!(
    magritte,
    [ToggleFold, BackTab, Quit, CloseWindow, OpenSettings]
);
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
    bisect::Bisect, BisectMark, CommitMode, ConflictSide, DiffSource, FileEntry, IgnoreDest,
    LineKind, RebaseAction, RefreshNeeds, RemoteTargets, Repo, ResetMode, Sequence, SequenceKind,
    Status, TagsAround,
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

fn with_alpha(mut color: Hsla, alpha: f32) -> Hsla {
    color.a = alpha;
    color
}

/// Fixed row height (points) so `uniform_list` can virtualize every row.
const ROW_HEIGHT: f32 = 18.0;

/// git's default diff context (`-U3`); the `+`/`-`/`0` keys adjust from here.
const DEFAULT_DIFF_CONTEXT: usize = 3;
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
/// Shown when the `git` binary can't be found — Magritte shells out to `git`,
/// so nothing works without it.
const GIT_MISSING_MESSAGE: &str =
    "git was not found. Install git or add it to your PATH, then reopen Magritte.";
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
    GitLog { view: ScrollView, show_all: bool },
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
    /// The refs browser (`y`, magit's show-refs): branches, remotes, and tags
    /// with act-at-point verbs.
    Refs(refs_view::RefsView),
    /// The worktree browser (`%`, magit's worktree): linked worktrees with
    /// visit/remove at point.
    Worktree(worktree_view::WorktreeView),
    /// A file's `git blame`: a scrollable pager (no cursor) of the file's lines
    /// with an inline commit annotation inserted above each commit run.
    Blame {
        view: ScrollView,
        path: String,
        rows: Rc<Vec<blame_view::BlameRow>>,
    },
}

/// A data-free tag for each [`Screen`] — its *keymap context*. Every command
/// declares the set of contexts it dispatches in ([`Command::contexts`]), so a
/// key can mean different things per screen (`a` = apply in a commit view,
/// cherry-apply on a status commit, toggle-queries in the command log) without
/// collision. Ordered so `as usize` indexes the [`ScreenSet`] bitset.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) enum ScreenKind {
    Status,
    Editor,
    Settings,
    GitLog,
    Log,
    Commit,
    Diff,
    RebaseTodo,
    Refs,
    Worktree,
    Blame,
}

impl ScreenKind {
    /// Every screen kind (keymap-build iteration).
    pub(crate) const ALL_KINDS: &'static [ScreenKind] = &[
        ScreenKind::Status,
        ScreenKind::Editor,
        ScreenKind::Settings,
        ScreenKind::GitLog,
        ScreenKind::Log,
        ScreenKind::Commit,
        ScreenKind::Diff,
        ScreenKind::RebaseTodo,
        ScreenKind::Refs,
        ScreenKind::Worktree,
        ScreenKind::Blame,
    ];
}

/// A set of [`ScreenKind`]s — a command's dispatch contexts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenSet(u16);

impl ScreenSet {
    /// Every screen (the default for global commands: motions, prefixes, quit…).
    pub(crate) const ALL: ScreenSet = ScreenSet(u16::MAX);

    /// The set containing exactly `kinds`.
    pub(crate) const fn of(kinds: &[ScreenKind]) -> ScreenSet {
        let mut bits = 0u16;
        let mut i = 0;
        while i < kinds.len() {
            bits |= 1 << (kinds[i] as u16);
            i += 1;
        }
        ScreenSet(bits)
    }

    pub(crate) fn contains(self, kind: ScreenKind) -> bool {
        self.0 & (1 << (kind as u16)) != 0
    }
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
    /// The in-progress `git bisect`, surfaced as a banner (its own good/bad/skip).
    bisect: Option<Bisect>,
    /// Original commit ids whose `reword` rows were intentionally written to
    /// git as `edit` stops so the in-app editor can handle their messages.
    pending_rebase_rewords: HashSet<String>,
    error: Option<String>,
    /// A message explaining why the repo couldn't be opened at startup, when the
    /// reason isn't the ordinary "not a git repository" (currently: `git` is not
    /// installed). Shown by `refresh` in place of the generic message.
    open_error: Option<String>,
    expanded: HashSet<FoldKey>,
    /// Hunks the user has explicitly collapsed (`FoldKey::Hunk`). Hunks default
    /// to expanded, so this tracks the exceptions rather than `expanded` does.
    collapsed_hunks: HashSet<FoldKey>,
    /// Set by fold level 3 (files open, hunks closed): diffs that finish
    /// loading afterwards get their hunks collapsed on arrival, so the level
    /// covers lazily loaded diffs too. Cleared by any manual fold toggle, a
    /// refresh, or another level.
    collapse_new_hunks: bool,
    /// Context lines shown around each hunk in the status view (git's default is
    /// 3); the `+`/`-`/`0` keys adjust it and reload the shown diffs.
    diff_context: usize,
    /// The lazily-loaded per-file diff cache (states + languages + highlight
    /// spans), keyed by `(source, path)`. See [`DiffCache`].
    diff_cache: DiffCache,
    /// Immutable commit detail loads (metadata/message/diff), keyed by full OID
    /// plus diff args/pathspecs. Rows are re-rendered from this on demand so the
    /// current theme still controls highlight colors.
    commit_cache: CommitCache,
    /// Resolved git-config defaults for transient switches during the current
    /// repository generation. Cleared on refresh so external git config changes
    /// are picked up without re-querying on every popup open.
    transient_config_defaults: HashMap<String, bool>,
    rows: Vec<Row>,
    selected: usize,
    /// The active visual/drag/shift-click range selection — see [`Selection`].
    selection: Selection,
    /// The active mouse char-range selection within one status diff line (a
    /// plain drag that stayed on its row) — see [`CharSelection`]. Mutually
    /// exclusive with a spanning [`Selection::visual`].
    char_sel: Option<CharSelection>,
    /// A value staged by right-clicking an atomic chrome element (a title-bar
    /// ref, the commit-detail hash): the `Copy` context-menu item copies this
    /// rather than the row selection. Set fresh on each such right-click.
    pending_copy: Option<String>,
    /// True while a chrome `Copy` context menu is open. Title-bar items suppress
    /// their hover tooltip while set, so the tooltip can't paint over the menu.
    /// Set on the opening right-click; cleared by the root's capture-phase
    /// mouse-down handler on the next click (which also dismisses the menu).
    ctx_menu_open: bool,
    /// Set by a selectable row's mouse-down (which manages its own selection);
    /// the root's bubble-phase handler reads it to decide whether a click landed
    /// on selectable text. A click that didn't (empty space, chrome, a section
    /// header) dismisses the active selection. Reset each click in the capture
    /// phase, so it reflects only the current press.
    click_hit_selectable: bool,
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
    /// Debounces persisting free-text settings edits (see
    /// `save_settings_debounced`); the flag marks an unflushed edit so closing
    /// settings can write it immediately.
    settings_save_gen: Generation,
    settings_save_pending: bool,
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
    /// The platform system UI font, for the `⏎` keycap glyph (always, so a
    /// custom `ui_font` that lacks the glyph can't turn it into tofu).
    system_ui_font: SharedString,
    /// The effective config: the global config with this repo's `.git/magritte`
    /// overlay merged on top. Everything renders from this.
    config: config::Config,
    /// The *global* config alone (no repo overlay). The settings screen is
    /// global-only, so its saves write this — never [`config`](Self::config),
    /// which would leak the repo overlay (e.g. a repo's `[status].sections`)
    /// into the global file. Kept in sync with the global file by `new`,
    /// `apply_config`, and the settings handlers.
    config_global: config::Config,
    /// The effective keystroke → command-id map, per screen (registry defaults
    /// overlaid with the user's `[keymap]`, then split by each command's
    /// `contexts`), resolved by `on_key`/`run_dispatch` via `screen_bindings()`.
    keymap: ScreenKeymaps,
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
    /// Font/editor option lists for the settings screen — see [`SettingsCaches`].
    settings_caches: SettingsCaches,
    /// The bottom status bar's toast (message / copied value / keycaps / fade
    /// stamp) — see [`StatusToast`].
    toast: StatusToast,
    /// Bumped per async picker open, stamped onto the picker, so a late
    /// candidate load only fills the picker it was started for.
    picker_gen: Generation,
    /// A pending confirmation: (prompt, what to do on `y`).
    confirm: Option<(String, Confirm)>,
    /// Memoized `$`-log rows, keyed on (command-log sequence, show-all) — see
    /// [`Self::git_log_rows`]. RefCell because render derives it with `&self`.
    git_log_cache: GitLogCache,
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
        // Keep discovery's failure reason: a missing `git` binary must read as
        // "git not found" rather than the misleading "not a git repository" that
        // any `None` repo would otherwise produce.
        let (repo, open_error) = match Repo::discover(&root) {
            Ok(repo) => (Some(repo), None),
            Err(e) if e.is_git_missing() => (None, Some(GIT_MISSING_MESSAGE.to_string())),
            Err(_) => (None, None),
        };
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
        let worktree_git_dir = repo.as_ref().and_then(|r| r.git_dir().ok());
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
        let system_ui_font = theme::resolve_system_ui_font(cx);

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
            bisect: None,
            pending_rebase_rewords: HashSet::new(),
            error: None,
            open_error,
            expanded,
            collapsed_hunks: HashSet::new(),
            collapse_new_hunks: false,
            diff_context: DEFAULT_DIFF_CONTEXT,
            diff_cache: DiffCache::default(),
            commit_cache: CommitCache::default(),
            transient_config_defaults: HashMap::new(),
            rows: Vec::new(),
            selected: 0,
            selection: Selection::default(),
            char_sel: None,
            pending_copy: None,
            ctx_menu_open: false,
            click_hit_selectable: false,
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
            settings_save_gen: Generation::default(),
            settings_save_pending: false,
            confirm_flash_gen: Generation::default(),
            popup: None,
            screen: Screen::Status,
            font,
            ui_font,
            system_ui_font,
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
            settings_caches: SettingsCaches::default(),
            toast: StatusToast {
                message: startup_warning,
                ..StatusToast::default()
            },
            picker_gen: Generation::default(),
            confirm: None,
            git_log_cache: GitLogCache::default(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            palette: Palette::default(),
        };
        view.refresh(cx);
        // Warm the settings screen's font/editor lists off-thread so the first
        // open doesn't stall on system font enumeration.
        view.prewarm_settings_caches(cx);
        view
    }

    // Read accessors for the active [`Screen`]'s state — `None` unless that
    // screen is the active one. Mutating sites match `&mut self.screen` inline
    // (so the borrow stays scoped to `screen`, like the old per-field access).
    /// The active screen's keymap context — how the dispatcher routes a key and
    /// which commands the `?` menu / headers show.
    pub(crate) fn screen_kind(&self) -> ScreenKind {
        match &self.screen {
            Screen::Status => ScreenKind::Status,
            Screen::Editor(_) => ScreenKind::Editor,
            Screen::Settings(_) => ScreenKind::Settings,
            Screen::GitLog { .. } => ScreenKind::GitLog,
            Screen::Log(_) => ScreenKind::Log,
            Screen::Commit { .. } => ScreenKind::Commit,
            Screen::Diff { .. } => ScreenKind::Diff,
            Screen::RebaseTodo(_) => ScreenKind::RebaseTodo,
            Screen::Refs(_) => ScreenKind::Refs,
            Screen::Worktree(_) => ScreenKind::Worktree,
            Screen::Blame { .. } => ScreenKind::Blame,
        }
    }

    /// A short human name for the active screen, for "… is unbound in <name>
    /// view" feedback. `None` on the status screen (the default, un-named).
    pub(crate) fn screen_name(&self) -> Option<&'static str> {
        match self.screen_kind() {
            ScreenKind::Status | ScreenKind::Editor => None,
            ScreenKind::Settings => Some("settings"),
            ScreenKind::GitLog => Some("command log"),
            ScreenKind::Log => Some("log"),
            ScreenKind::Commit => Some("commit"),
            ScreenKind::Diff => Some("diff"),
            ScreenKind::RebaseTodo => Some("rebase"),
            ScreenKind::Refs => Some("refs"),
            ScreenKind::Worktree => Some("worktree"),
            ScreenKind::Blame => Some("blame"),
        }
    }

    /// The active screen's keystroke → candidate-ids submap. Every screen has an
    /// entry (text-entry screens' is empty), so this can't miss.
    pub(crate) fn screen_bindings(&self) -> &commands::KeyBindings {
        static EMPTY: std::sync::OnceLock<commands::KeyBindings> = std::sync::OnceLock::new();
        self.keymap
            .get(&self.screen_kind())
            .unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
    }
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
            Screen::GitLog { view, .. } => Some(view),
            _ => None,
        }
    }

    /// Whether the `$` command-log view also shows the UI's own read-only
    /// queries (hidden by default). Lives on the screen so it resets naturally.
    pub(crate) fn git_log_show_all(&self) -> bool {
        matches!(self.screen, Screen::GitLog { show_all: true, .. })
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
            Screen::GitLog { view, .. } => Some(view),
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

    /// Which keymap preset is active — for the handful of hardcoded
    /// act-at-point keys that differ between evil-collection and vanilla magit.
    pub(crate) fn is_evil(&self) -> bool {
        matches!(
            self.config.keymap_preset,
            config::KeymapPreset::EvilCollection
        )
    }

    /// The repo cloned for a background *read* (status/diff/prefetch), tagged
    /// with the current generation's cancel flag so a later `refresh` kills it.
    fn read_repo(&self) -> Option<magritte_core::Repo> {
        self.repo
            .clone()
            .map(|r| r.with_cancel(self.read_cancel.clone()))
    }

    // --- Row construction -------------------------------------------------

    // --- Staging ----------------------------------------------------------

    // --- Popups (transients + help) --------------------------------------

    /// Advance and return the screen-load generation. A screen-changing async
    /// load captures this and re-checks it before mutating the screen.
    fn next_screen_gen(&mut self) -> u64 {
        self.screen_gen.bump()
    }
}

// --- Small row/value helpers ---------------------------------------------

fn parse_release_version(version: &str) -> Option<(u64, u64, u64)> {
    let version = version
        .trim()
        .strip_prefix("refs/tags/")
        .unwrap_or(version.trim())
        .trim_start_matches('v');
    let stable = version
        .split_once('-')
        .map_or(version, |(stable, _)| stable);
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
    match (
        parse_release_version(current),
        parse_release_version(latest),
    ) {
        (Some(current_version), Some(latest_version)) if current_version < latest_version => {
            format!("Magritte {current}; latest is {latest} (update available)")
        }
        (Some(current_version), Some(latest_version)) if current_version > latest_version => {
            format!("Magritte {current}; latest release is {latest}")
        }
        _ => format!("Magritte {current} is the latest version"),
    }
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

type RepoWindows = Rc<RefCell<HashMap<PathBuf, AnyWindowHandle>>>;

/// The open-repo-window registry, exposed as a GPUI global so a view (e.g. the
/// worktree browser visiting another worktree) can open-or-focus a window for a
/// path without threading the registry through every constructor.
pub(crate) struct GlobalRepoWindows(pub(crate) RepoWindows);
impl gpui::Global for GlobalRepoWindows {}

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
            KeyBinding::new("shift-tab", BackTab, Some(STATUS_CONTEXT)),
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
        // Expose the registry as a global so views can open-or-focus repo
        // windows (worktree "visit") without threading it through constructors.
        cx.set_global(GlobalRepoWindows(windows.clone()));
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

    /// The status screen's keystroke → id submap (what the keymap tests assert
    /// against), plus the build warnings.
    fn status_km_w(config: &config::Config) -> (commands::KeyBindings, Vec<String>) {
        let (mut kms, warnings) = build_keymap(config);
        (kms.remove(&ScreenKind::Status).unwrap(), warnings)
    }

    fn status_km(config: &config::Config) -> commands::KeyBindings {
        status_km_w(config).0
    }

    /// The top-precedence command a key resolves to in a submap (its first
    /// candidate) — what these tests, which don't model the at-point target,
    /// assert against.
    fn km_id<'a>(km: &'a commands::KeyBindings, key: &str) -> Option<&'a str> {
        km.get(key).and_then(|v| v.first()).map(String::as_str)
    }

    #[test]
    fn command_registry_is_consistent() {
        use std::collections::HashSet;
        let (mut ids, mut titles) = (HashSet::new(), HashSet::new());
        for c in commands() {
            assert!(ids.insert(c.id), "duplicate command id: {}", c.id);
            // Palette titles must be unique — the `:` palette resolves the chosen
            // title back to its command. Non-palette verbs (per-screen and
            // act-at-point) may share a title with their twin on another screen
            // (a status commit's "Cherry-pick" vs the log's).
            if c.palette {
                assert!(
                    titles.insert(c.title),
                    "duplicate palette command title: {}",
                    c.title
                );
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
        // A command's key must be unique within each (preset, context) it binds:
        // two commands may share a key only if their contexts are disjoint —
        // that's what lets `a`/`enter` mean different things per screen without
        // the per-context keymap silently dropping one.
        for preset in [
            config::KeymapPreset::EvilCollection,
            config::KeymapPreset::Vanilla,
        ] {
            let mut claimed: HashSet<(ScreenKind, String)> = HashSet::new();
            for c in commands() {
                // Act-at-point verbs deliberately share a key with a general
                // command in the same context (dispatch tries them first, gated
                // by the target at point); only the general layer must be unique.
                if c.at_point {
                    continue;
                }
                let Some(key) = default_key_for_command(preset, c) else {
                    continue;
                };
                for &sk in ScreenKind::ALL_KINDS {
                    if c.contexts.contains(sk) {
                        assert!(
                            claimed.insert((sk, key.to_string())),
                            "duplicate key {key:?} in {sk:?} ({preset:?}): {}",
                            c.id
                        );
                    }
                }
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
            let km = status_km(&config);
            let menu: HashSet<String> = dispatch_menu(&km, &config)
                .groups
                .iter()
                .flat_map(|g| &g.suffixes)
                .filter_map(|s| match s {
                    Suffix::Info(i) => Some(i.keys.clone()),
                    _ => None,
                })
                .collect();
            // Only status-context menu commands appear in this (status) menu;
            // screen-scoped verbs are covered by the per-screen invariant, and
            // act-at-point verbs are grafted into their own group by target.
            for c in commands()
                .iter()
                .filter(|c| c.menu && !c.at_point && c.contexts.contains(ScreenKind::Status))
            {
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
    fn is_dispatch_key_matches_bound_single_keys() {
        // Against the default keymap: bound single-key commands route, unbound
        // keys don't. Only single-keystroke chords reach the helper — multi-key
        // sequences resolve through the prefix machinery, and `tab` is handled
        // before dispatch.
        let (km, warnings) = status_km_w(&config::Config::default());
        assert!(warnings.is_empty(), "default config has no keymap warnings");
        assert!(StatusView::is_dispatch_key(&km, "c"));
        assert!(StatusView::is_dispatch_key(&km, "s"));
        assert!(StatusView::is_dispatch_key(&km, "G"));
        assert!(!StatusView::is_dispatch_key(&km, "z")); // not bound by default
    }

    #[test]
    fn exclusive_switches_deactivate_each_other() {
        // Cherry-pick declares --ff and -x incompatible (git rejects the
        // combination); toggling either must turn the other off, in both
        // directions even though only --ff carries the declaration.
        let def = transient::cherry_pick_transient();
        assert_eq!(conflicting_switch_keys(&def, "-x"), ["-F"]);
        assert_eq!(conflicting_switch_keys(&def, "-F"), ["-x"]);
        assert!(conflicting_switch_keys(&def, "-e").is_empty());

        let merge = transient::merge_transient();
        assert_eq!(conflicting_switch_keys(&merge, "-f"), ["-n"]);
        assert_eq!(conflicting_switch_keys(&merge, "-n"), ["-f"]);
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
        assert!(
            on(&s, "-S"),
            "empty saved set keeps gpg-sign on when config enables it"
        );
        assert!(!on(&s, "-a"));
        // Config off + empty: stays off.
        assert!(!on(&build(false, &[]), "-S"));

        // Explicit forms override the config default either way, and round-trip.
        // Saved entries are the git arguments, not the keystrokes.
        let off = build(true, &["--no-gpg-sign"]); // forced off against config-on
        assert!(!on(&off, "-S"));
        assert_eq!(off.saved_overrides(), vec!["--no-gpg-sign".to_string()]);
        let forced_on = build(false, &["--gpg-sign"]); // forced on against config-off
        assert!(on(&forced_on, "-S"));
        assert_eq!(forced_on.saved_overrides(), vec!["--gpg-sign".to_string()]);

        // A switch matching its config default isn't persisted (config drives it);
        // a plain switch persists by its argument.
        let mut s = build(true, &["--all"]);
        assert!(on(&s, "-S") && on(&s, "-a"));
        assert_eq!(s.saved_overrides(), vec!["--all".to_string()]);
        // Turn the config-on switch off → it now persists as the negation.
        s.active.remove("-S");
        assert_eq!(
            s.saved_overrides(),
            vec!["--all".to_string(), "--no-gpg-sign".to_string()]
        );
    }

    #[test]
    fn saved_set_round_trips_option_values() {
        // Saved entries are the git arguments the transient emits, matched back
        // to their switch/option by argument (not keystroke): a plain switch,
        // a value option by its arg prefix (`-n50`, `--grep=…`), and a
        // fixed-choice option whose value is itself the flag (`-o`, order).
        let def = transient::log_transient();
        let saved = vec![
            "--reverse".to_string(),
            "-n50".to_string(),
            "--grep=fix bug".to_string(),
            "--author-date-order".to_string(),
        ];
        let mut state = TransientState::new("log", def, RemoteTargets::default());
        state.active = TransientState::apply_saved(&state.def, &saved);
        state.values = TransientState::apply_saved_values(&state.def, &saved);
        assert!(state.active.contains("-r"));
        assert_eq!(state.values.get("-n").map(String::as_str), Some("50"));
        assert_eq!(state.values.get("-F").map(String::as_str), Some("fix bug"));
        assert_eq!(
            state.values.get("-o").map(String::as_str),
            Some("--author-date-order")
        );
        assert_eq!(
            state.args(),
            vec![
                "--reverse".to_string(),
                "--author-date-order".to_string(),
                "-n50".to_string(),
                "--grep=fix bug".to_string(),
            ]
        );
        // The set round-trips through save unchanged (sorted); pathspec file
        // limits are deliberately not persisted as defaults.
        let mut expected = saved.clone();
        expected.sort();
        assert_eq!(state.saved_overrides(), expected);
    }

    #[test]
    fn status_unknown_section_warns() {
        let mut config = config::Config::default();
        config.status.sections = vec!["staged".into(), "bogus".into()];
        let (_, warnings) = build_keymap(&config);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unknown section \"bogus\"")),
            "expected an unknown-section warning, got {warnings:?}"
        );
    }

    #[test]
    fn keymap_remap_unbind_and_unknown_id() {
        let mut config = config::Config::default();
        config.keymap.insert("K".into(), "branch-delete".into()); // remap
        config.keymap.insert("x".into(), "unbound".into()); // unbind
        config.keymap.insert("Q".into(), "no-such-command".into()); // unknown
        let (km, warnings) = status_km_w(&config);
        assert_eq!(km_id(&km, "K"), Some("branch-delete"));
        assert!(!km.contains_key("x"), "x was unbound");
        assert!(!km.contains_key("Q"), "unknown id isn't bound");
        assert_eq!(warnings.len(), 1, "the unknown id warns: {warnings:?}");
        // Defaults the user didn't touch survive.
        assert_eq!(km_id(&km, "c"), Some("commit"));
    }

    #[test]
    fn keymap_preset_switches_defaults_and_transient_suffixes() {
        let config = config::Config::default();
        let (km, warnings) = status_km_w(&config);
        assert!(warnings.is_empty(), "default keymap is clean: {warnings:?}");
        assert_eq!(km_id(&km, "p"), Some("push"));
        assert_eq!(km_id(&km, "O"), Some("reset"));
        assert_eq!(km_id(&km, "Z"), Some("stash"));
        assert_eq!(km_id(&km, "|"), Some("git-command"));
        assert_eq!(km_id(&km, "V"), Some("visual"));
        assert_eq!(
            command_keys(&km, &config, "Delete branch").as_deref(),
            Some("b x")
        );
        assert_eq!(
            command_keys(&km, &config, "Delete tag").as_deref(),
            Some("t x")
        );
        assert_eq!(
            command_keys(&km, &config, "Remove remote").as_deref(),
            Some("M x")
        );

        let mut config = config::Config::default();
        config.keymap_preset = config::KeymapPreset::Vanilla;
        let (km, warnings) = status_km_w(&config);
        assert!(warnings.is_empty(), "vanilla keymap is clean: {warnings:?}");
        assert_eq!(km_id(&km, "P"), Some("push"));
        assert_eq!(km_id(&km, "X"), Some("reset"));
        assert_eq!(km_id(&km, "z"), Some("stash"));
        // `k` drops a stash at point first (act-at-point), else discards a file —
        // both are candidates, with discard the general fallback.
        assert!(km.get("k").unwrap().iter().any(|id| id == "discard"));
        assert_eq!(km_id(&km, "n"), Some("next-section"));
        assert_eq!(km_id(&km, "p"), Some("prev-section"));
        assert_eq!(km_id(&km, ":"), Some("git-command"));
        assert_eq!(km_id(&km, "!"), Some("git-command"));
        // Vanilla's commit-at-point revert is on `V` (evil uses `_`).
        assert_eq!(km_id(&km, "V"), Some("revert-here"));
        assert_eq!(command_keys(&km, &config, "Push").as_deref(), Some("P"));
        assert_eq!(command_keys(&km, &config, "Reset").as_deref(), Some("X"));
        assert_eq!(command_keys(&km, &config, "Stash").as_deref(), Some("z"));
        assert_eq!(command_keys(&km, &config, "Discard").as_deref(), Some("k"));
        assert_eq!(
            command_keys(&km, &config, "Delete branch").as_deref(),
            Some("b k")
        );
        assert_eq!(
            command_keys(&km, &config, "Delete tag").as_deref(),
            Some("t k")
        );
        assert_eq!(
            command_keys(&km, &config, "Remove remote").as_deref(),
            Some("M k")
        );
    }

    #[test]
    fn keymap_sequences_any_depth() {
        let mut config = config::Config::default();
        config.keymap.insert("g x".into(), "stage".into()); // 2-key sequence
        config.keymap.insert(". c".into(), "commit".into()); // a `.` prefix
        config.keymap.insert("a b c".into(), "stage".into()); // 3-key chain
        let (km, warnings) = status_km_w(&config);
        assert_eq!(km_id(&km, "g x"), Some("stage"));
        assert_eq!(km_id(&km, ". c"), Some("commit"));
        assert_eq!(km_id(&km, "a b c"), Some("stage"));
        assert!(
            warnings.is_empty(),
            "any-depth sequence is fine: {warnings:?}"
        );
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
        let (km, warnings) = status_km_w(&config);
        assert_eq!(km_id(&km, "X"), Some("user.wip"));
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
        let (km, _) = status_km_w(&config);
        assert_eq!(
            command_keys(&km, &config, "WIP commit").as_deref(),
            Some("c W")
        );

        // A direct keymap binding is shown directly.
        config.keymap.insert("g w".into(), "user.wip".into());
        let (km, _) = status_km_w(&config);
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
        let (km, _) = status_km_w(&config);
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
            "c", "b", "t", "M", "Z", "l", "d", "p", "F", "f", "O", "m", "r", "i", "!", ",", "$",
            "%", "B", "W", // commands
            "s", "u", "S", "U", "x", "X", // applying changes (X = evil untrack)
            "v", "tab", "g r", ":", "enter", // essential + open file + palette
            "j", "k", "g g", "G", "g j", "g k", // navigation / motions
            "ctrl-d", "ctrl-u", "ctrl-f", "ctrl-b", // half/full page motions
        ];
        // Keys allowed to be on only one side of the check. Cursor motions
        // dispatch but are intentionally hidden from the `?` menu (standard
        // vim/emacs conventions — see the `nav!` block in commands.rs).
        const OVERRIDES: &[&str] = &[
            "j", "k", "g g", "G", "g j", "g k", "ctrl-d", "ctrl-u", "ctrl-f", "ctrl-b",
            // The evil yank family (`yy` copy, `yr` show-refs) shows in the menu
            // but resolves through the prefix machinery, not `run_dispatch`.
            "y y", "y r",
        ];

        let config = config::Config::default();
        let km = status_km(&config);
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

    /// The secondary screens derive their `?` menu and header hints from the same
    /// per-context keymap the keyboard dispatches through, so menu == dispatch by
    /// construction. This pins that: for every secondary screen and both presets,
    /// each screen-scoped verb resolves to a key in that screen's submap, and that
    /// key dispatches back to the same command (never a stale or colliding one).
    #[test]
    fn every_scoped_verb_dispatches_to_itself() {
        use commands::{current_key, default_key_for_command};
        for preset in [
            config::KeymapPreset::EvilCollection,
            config::KeymapPreset::Vanilla,
        ] {
            let config = config::Config {
                keymap_preset: preset,
                ..config::Config::default()
            };
            let keymap = build_keymap(&config).0;
            for &kind in ScreenKind::ALL_KINDS {
                let submap = keymap.get(&kind).cloned().unwrap_or_default();
                for cmd in commands::commands() {
                    // Only the screen-scoped verbs (not the global `ALL` commands,
                    // which are keyed for the status screen).
                    if cmd.contexts == ScreenSet::ALL || !cmd.contexts.contains(kind) {
                        continue;
                    }
                    // A verb the preset intentionally leaves unbound (e.g. the
                    // evil-only visual toggle in vanilla) declares no key.
                    if default_key_for_command(preset, cmd).is_none() {
                        continue;
                    }
                    let key = current_key(&submap, cmd.id, cmd.key).unwrap_or_else(|| {
                        panic!(
                            "{preset:?}: verb `{}` scoped to {kind:?} declares a key but none \
                             is bound in that screen's keymap",
                            cmd.id
                        )
                    });
                    // The verb is reachable at that key: it's among the key's
                    // candidates (an at-point verb sharing the key sits ahead of
                    // it, resolved by target at dispatch time).
                    assert!(
                        submap
                            .get(&key)
                            .is_some_and(|cands| cands.iter().any(|id| id == cmd.id)),
                        "{preset:?}/{kind:?}: key `{key}` (shown for `{}`) dispatches elsewhere",
                        cmd.id
                    );
                }
            }
        }
    }

    #[test]
    fn parse_refs_classifies_decorations() {
        // No upstream context → no folding; every ref classified on its own.
        let got = parse_refs("HEAD -> main, origin/main, tag: v1.0, feature, HEAD", None);
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
        assert!(parse_refs("", None).is_empty());
    }

    #[test]
    fn parse_refs_drops_remote_head_pointer() {
        // `origin/HEAD` is a symbolic pointer, not a real branch — magit hides it.
        let got = parse_refs("HEAD -> main, origin/main, origin/HEAD", None);
        assert_eq!(
            got,
            vec![
                ("main".to_string(), RefKind::Head),
                ("origin/main".to_string(), RefKind::Remote),
            ]
        );
    }

    #[test]
    fn parse_refs_folds_current_branch_with_upstream() {
        // With the branch's upstream known, `main` + `origin/main` collapse into
        // one synced entry; `origin/HEAD` is still dropped; the tag survives.
        let got = parse_refs(
            "HEAD -> main, origin/main, origin/HEAD, tag: v1.0",
            Some("origin/main"),
        );
        assert_eq!(
            got,
            vec![
                ("origin/main".to_string(), RefKind::SyncedHead),
                ("v1.0".to_string(), RefKind::Tag),
            ]
        );
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
            CommitDiffRow::File {
                change: magritte_core::Change::Modified,
                path: "a.txt".to_string(),
            },
        ];

        prepend_commit_details(&mut rows, &details);
        rows.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
        prepend_commit_details(&mut rows, &details);

        assert!(matches!(rows.first(), Some(CommitDiffRow::Detail(_))));
        assert!(matches!(rows.get(1), Some(CommitDiffRow::Note(n)) if n.is_empty()));
        assert!(matches!(rows.get(2), Some(CommitDiffRow::File { .. })));

        // A bodyless commit (no leading blank): showing then hiding details must
        // restore exactly `[File]`, not leave a stray blank line at the top.
        let mut bodyless = vec![CommitDiffRow::File {
            change: magritte_core::Change::Modified,
            path: "a.txt".to_string(),
        }];
        prepend_commit_details(&mut bodyless, &details);
        bodyless.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
        assert!(
            matches!(bodyless.as_slice(), [CommitDiffRow::File { .. }]),
            "hiding details on a bodyless commit should leave just the file"
        );
    }

    #[test]
    fn commit_details_slot_below_the_head_line() {
        // Details must appear under the "Commit <sha>" head line, not above it.
        let details = vec!["Author:    A".to_string()];
        let mut rows = vec![
            CommitDiffRow::Head("deadbeef".to_string()),
            CommitDiffRow::Note(String::new()),
            CommitDiffRow::Message("subject".to_string()),
        ];
        prepend_commit_details(&mut rows, &details);
        assert!(matches!(rows.first(), Some(CommitDiffRow::Head(_))));
        assert!(matches!(rows.get(1), Some(CommitDiffRow::Detail(_))));
        // Hiding restores the exact original order (head line still first).
        rows.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
        assert!(matches!(
            rows.as_slice(),
            [
                CommitDiffRow::Head(_),
                CommitDiffRow::Note(_),
                CommitDiffRow::Message(_)
            ]
        ));
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

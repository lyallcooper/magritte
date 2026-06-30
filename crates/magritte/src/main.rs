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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, div, px, size, uniform_list, AnyElement, App, AppContext, Bounds, ClipboardItem,
    Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, KeyBinding,
    KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, SharedString, Styled,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};

mod commands;
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
mod kbd;
mod navigation;
mod picker;
mod render;
mod settings;
mod status_label;
mod targets;
mod theme;
pub(crate) use commands::*;
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
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::input::{InputEvent, InputState, Position};
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::{ActiveTheme, IndexPath};
use magritte_core::transient::{self, Group, Suffix, TitleSpan, Transient};
use magritte_core::{
    CommitMode, ConflictSide, DiffSource, EntryKind, FileDiff, FileEntry, IgnoreDest, LineKind,
    LogEntry, RebaseAction, RemoteTargets, Repo, ResetMode, Sequence, SequenceKind, Stash, Status,
};

/// The in-app commit message editor, backed by gpui-component's multi-line
/// Input. We keep the commit context (mode + switches) alongside it.
struct CommitEditor {
    state: Entity<InputState>,
    mode: CommitMode,
    args: Vec<String>,
    /// The baseline message we'd discard back to: empty for a new commit, or
    /// HEAD's message for amend/reword. Canceling only prompts when the current
    /// text differs from this.
    initial: String,
    /// Whether a "discard message?" confirmation is showing (cancel was pressed
    /// with unsaved edits).
    confirming_cancel: bool,
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

/// An open transient popup with the switches toggled on and the option values
/// set within it.
struct TransientState {
    /// The transient's command id (`commit`, `push`, …), for saving its switch
    /// defaults. Empty for ad-hoc transients (e.g. an in-progress sequence).
    id: String,
    def: Transient,
    active: std::collections::HashSet<String>,
    /// The active set as opened (its saved/built-in defaults), so the UI can
    /// tell when switches have been *modified* (to offer saving them).
    baseline: std::collections::HashSet<String>,
    /// Value-reading option values, keyed by the option's key (e.g. `-F` →
    /// `fix bug`). Combined with `active` to build the git argument list.
    values: std::collections::HashMap<String, String>,
    /// True after `-` is pressed, awaiting the switch/option letter (magit `-f`).
    pending_dash: bool,
    /// True after the save key is pressed, awaiting the scope letter (`g`lobal /
    /// `l`ocal) — magit-style two-step save.
    pending_save: bool,
    /// Resolved push/pull/fetch targets, so dispatch can route to the right
    /// remote without recomputing (empty for non-remote transients).
    targets: RemoteTargets,
}

impl TransientState {
    fn new(id: impl Into<String>, def: Transient, targets: RemoteTargets) -> Self {
        // Switches flagged default-on start toggled on (the user can turn them
        // off); the rest start off.
        let active: std::collections::HashSet<String> = def
            .switches()
            .filter(|s| s.default_on)
            .map(|s| s.key.to_string())
            .collect();
        TransientState {
            id: id.into(),
            def,
            baseline: active.clone(),
            active,
            values: std::collections::HashMap::new(),
            pending_dash: false,
            pending_save: false,
            targets,
        }
    }

    /// The git flag arguments from the toggled switches and set options, in
    /// definition order (switches first, then options as `{arg}{value}`).
    /// Pathspec options are excluded — see [`Self::pathspecs`] — since they must
    /// trail the revision behind a `--`.
    fn args(&self) -> Vec<String> {
        let switches = self.def.switches().filter_map(|s| {
            let on = self.active.contains(s.key.as_str());
            match &s.negation {
                // A negatable switch reflects a git-config default: emit a flag
                // only when the toggle differs from that default — the positive
                // arg when turned on, the negation (e.g. --no-gpg-sign) when off.
                Some(neg) => (on != s.default_on).then(|| if on { s.arg.clone() } else { neg.clone() }),
                None => on.then(|| s.arg.clone()),
            }
        });
        let options = self
            .def
            .options()
            .filter(|o| !o.pathspec)
            .filter_map(|o| self.values.get(o.key).map(|v| format!("{}{}", o.arg, v)));
        switches.chain(options).collect()
    }

    /// The values of any set pathspec options (e.g. the log file limit), to be
    /// placed after the revision behind a `--`.
    fn pathspecs(&self) -> Vec<String> {
        self.def
            .options()
            .filter(|o| o.pathspec)
            .filter_map(|o| self.values.get(o.key).cloned())
            .collect()
    }

    /// The active switch set from a saved set (magit's `transient-save`),
    /// reconciled against the transient's switches. A plain switch is on iff the
    /// set names its key. A *negatable* (config-derived) switch is forced on or
    /// off only when the set names its key or its negation flag explicitly —
    /// otherwise it keeps its config default, so an old or empty saved set can't
    /// silently flip e.g. gpg-signing off by mere omission.
    fn apply_saved(def: &Transient, saved: &[String]) -> std::collections::HashSet<String> {
        let saved: std::collections::HashSet<&str> = saved.iter().map(String::as_str).collect();
        let mut active = std::collections::HashSet::new();
        for sw in def.switches() {
            let on = match &sw.negation {
                Some(_) if saved.contains(sw.key.as_str()) => true,
                Some(neg) if saved.contains(neg.as_str()) => false,
                Some(_) => sw.default_on,
                None => saved.contains(sw.key.as_str()),
            };
            if on {
                active.insert(sw.key.clone());
            }
        }
        active
    }

    /// The switch overrides to persist: a plain switch when on, and a negatable
    /// switch only when it differs from its config default — recorded as the key
    /// (forced on) or the negation flag (forced off), so omission round-trips as
    /// "follow config". The inverse of [`apply_saved`](Self::apply_saved).
    fn saved_overrides(&self) -> Vec<String> {
        let mut out = Vec::new();
        for sw in self.def.switches() {
            let on = self.active.contains(&sw.key);
            match &sw.negation {
                Some(neg) if on != sw.default_on => {
                    out.push(if on { sw.key.clone() } else { neg.clone() })
                }
                Some(_) => {}
                None if on => out.push(sw.key.clone()),
                None => {}
            }
        }
        out.sort();
        out
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
    /// A vertico-style minibuffer picker (the general one): selecting or typing
    /// a value — remotes, branches, refs, stashes, the command palette, a
    /// transient option, an arbitrary git command, an ignore pattern, a rebase
    /// base. The pending [`PickerAction`] says what the chosen value does.
    Picker(PickerState),
}

/// What to do with the picker's chosen value. The remote-level variants take a
/// remote *name*; the `*Ref` variants take a `remote/branch` ref (magit's
/// "elsewhere"), so the picker lists remote branches and can create a new one.
#[derive(Clone)]
enum Transfer {
    /// `git push [--set-upstream] <remote> <branch>`; `save_push_remote` records
    /// `branch.<b>.pushRemote` first (first push to a push-remote).
    Push {
        branch: String,
        set_upstream: bool,
        save_push_remote: bool,
    },
    /// Push the current branch to a chosen `remote/branch` ref (elsewhere),
    /// creating it if new: `git push <remote> <branch>:<target>`.
    PushRef { branch: String },
    /// `git pull <remote> <branch>` — `branch` is the remote branch to merge.
    Pull { branch: String },
    /// Pull a chosen `remote/branch` ref (elsewhere).
    PullRef,
    /// `git fetch <remote>`.
    Fetch,
}

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

/// A branch-transient operation carried out against a picked branch/name. Some
/// are two-step (`RenameFrom` → `RenameTo`): the first picker's confirm opens
/// the second.
#[derive(Clone)]
enum BranchAction {
    /// Check out the chosen branch/revision.
    Checkout,
    /// Create a branch named by the chosen value (from HEAD); check it out too
    /// when `checkout`.
    Create { checkout: bool },
    /// Step 1 of rename: the chosen branch is the one to rename.
    RenameFrom,
    /// Step 2 of rename: rename `old` to the chosen new name.
    RenameTo { old: String },
    /// Delete the chosen branch.
    Delete,
}

/// A stash-transient operation carried out against a picked stash entry. The
/// chosen value is the entry's display string; the `stash@{N}` reference is its
/// first whitespace-delimited token.
#[derive(Clone, Copy)]
enum StashAction {
    Apply,
    Pop,
    Drop,
}

/// What the picker does with its chosen value: a push/pull/fetch target, a
/// branch/stash operation, a value for a transient option, or the ref to log.
#[derive(Clone)]
enum PickerAction {
    Transfer(Transfer),
    Branch(BranchAction),
    Stash(StashAction),
    /// Set a transient option's value (`resume` carries the transient to
    /// reopen with the value applied).
    SetOption {
        key: String,
        description: String,
    },
    /// Log the chosen ref, with the flags/pathspecs/limit gathered from the log
    /// transient assembled around it.
    LogRef {
        flags: Vec<String>,
        paths: Vec<String>,
        limit: usize,
    },
    /// Run a registry [`Command`] chosen from the `:` palette (matched by title).
    RunCommand,
    /// Reset HEAD to the chosen commit, in the carried mode (hard is confirmed).
    Reset(magritte_core::ResetMode),
    /// Merge the chosen branch/ref into HEAD, with the carried args.
    Merge,
    /// Rebase the current branch onto the chosen ref, with the carried args.
    Rebase,
    /// Run an arbitrary git command typed by the user (magit's `!`).
    RunGit,
    /// Add the typed pattern (seeded with the file at point) to a gitignore file.
    Ignore(magritte_core::IgnoreDest),
}

impl PickerAction {
    /// The minibuffer prompt (styled spans) for this picker.
    fn prompt(&self) -> Vec<TitleSpan> {
        match self {
            PickerAction::Transfer(t) => t.prompt(),
            PickerAction::Branch(b) => match b {
                BranchAction::Checkout => transient::plain_title("Checkout"),
                BranchAction::Create { checkout: true } => {
                    transient::plain_title("Create & checkout branch")
                }
                BranchAction::Create { checkout: false } => transient::plain_title("Create branch"),
                BranchAction::RenameFrom => transient::plain_title("Rename branch"),
                BranchAction::RenameTo { old } => vec![
                    TitleSpan::text("Rename "),
                    TitleSpan::branch(old.clone()),
                    TitleSpan::text(" to"),
                ],
                BranchAction::Delete => transient::plain_title("Delete branch"),
            },
            PickerAction::Stash(s) => transient::plain_title(match s {
                StashAction::Apply => "Apply stash",
                StashAction::Pop => "Pop stash",
                StashAction::Drop => "Drop stash",
            }),
            PickerAction::SetOption { description, .. } => {
                transient::plain_title(description.clone())
            }
            PickerAction::LogRef { .. } => transient::plain_title("Log ref"),
            PickerAction::RunCommand => transient::plain_title("Run command"),
            PickerAction::Reset(_) => transient::plain_title("Reset to"),
            PickerAction::Merge => transient::plain_title("Merge"),
            PickerAction::Rebase => transient::plain_title("Rebase onto"),
            // Reads like magit's "git " prompt: the typed text follows "git".
            PickerAction::RunGit => transient::plain_title("Run"),
            PickerAction::Ignore(_) => transient::plain_title("Ignore pattern"),
        }
    }

    /// Notice shown when a selection-only picker (one you can't type into) turns
    /// up no candidates, so it closes instead of presenting an empty list. Only
    /// the selection-only actions need a real message; value-entry pickers stay
    /// open regardless and never use this.
    fn empty_message(&self) -> &'static str {
        match self {
            PickerAction::Stash(_) => "No stashes",
            PickerAction::Branch(_) => "No branches",
            PickerAction::Transfer(_) => "No remotes configured",
            _ => "Nothing to select",
        }
    }

    /// Imperative verb for the confirm key hint.
    fn confirm_label(&self) -> &'static str {
        match self {
            PickerAction::Transfer(Transfer::Push { .. } | Transfer::PushRef { .. }) => "push",
            PickerAction::Transfer(Transfer::Pull { .. } | Transfer::PullRef) => "pull",
            PickerAction::Transfer(Transfer::Fetch) => "fetch",
            PickerAction::Branch(BranchAction::Checkout) => "checkout",
            PickerAction::Branch(BranchAction::Create { .. }) => "create",
            PickerAction::Branch(BranchAction::RenameFrom | BranchAction::RenameTo { .. }) => {
                "rename"
            }
            PickerAction::Branch(BranchAction::Delete) => "delete",
            PickerAction::Stash(StashAction::Apply) => "apply",
            PickerAction::Stash(StashAction::Pop) => "pop",
            PickerAction::Stash(StashAction::Drop) => "drop",
            PickerAction::SetOption { .. } => "set",
            PickerAction::LogRef { .. } => "log",
            PickerAction::RunCommand => "run",
            PickerAction::Reset(_) => "reset",
            PickerAction::Merge => "merge",
            PickerAction::Rebase => "rebase",
            PickerAction::RunGit => "run",
            PickerAction::Ignore(_) => "ignore",
        }
    }
}

/// An open target picker (vertico-style): a prompt, an inline query input, a
/// ranked candidate list, and the pending action. It runs against the
/// highlighted (or clicked) candidate on Enter.
struct PickerState {
    /// The minibuffer-style prompt as styled spans, e.g. `Push `[main]` to` (the
    /// `:` and the typed text are rendered after it).
    prompt: Vec<TitleSpan>,
    /// The bare query input (type-to-filter).
    input: Entity<InputState>,
    /// The filter/rank/select model over the candidates.
    list: PickerList,
    /// Scrolls the (virtualized) candidate rows.
    scroll: UniformListScrollHandle,
    action: PickerAction,
    switches: Vec<String>,
    /// Candidates are still loading off the UI thread (shows "Loading…" in the
    /// reserved candidate area instead of "No match"). See `open_listed_picker`.
    loading: bool,
    /// Identifies this picker instance, so an async candidate load only fills
    /// the picker it was started for — not a later one the user opened meanwhile.
    gen: u64,
    /// Whether to reserve the fixed candidate-list area. True for every picker
    /// with candidates (so its height stays stable while filtering, and doesn't
    /// jump when async candidates load); false only for a pure free-text value
    /// prompt (e.g. `-n`), which collapses to just the input + hints.
    reserve_candidates: bool,
    /// A transient to reopen when this picker confirms or cancels — used when a
    /// transient option prompts for its value, so the menu comes back after.
    /// Boxed to keep the (already large) picker state from dominating `Popup`.
    resume: Option<Box<TransientState>>,
    /// Kept alive so the input-change subscription stays active.
    _sub: Subscription,
}

/// A flattened row of the git command-log view: a command, or one line of its
/// output. Flattening keeps the view a single uniform-height list.
enum GitLogRow {
    /// `prog` is the program (`git` for the common case), shown dimmed before
    /// the arguments.
    Command { prog: String, args: String, ok: bool },
    Output(String),
}

/// Why the log view is open. Browsing is the default; selecting picks a commit
/// to act on and confirms with Return (magit's `magit-log-select`).
#[derive(PartialEq, Eq)]
enum LogPurpose {
    /// Ordinary browsing: Return opens the commit's diff.
    Browse,
    /// Pick the commit to rebase interactively since (its `^`..HEAD becomes the
    /// editable todo). Carries the switches gathered in the rebase transient.
    SelectRebaseBase { args: Vec<String> },
}

/// The commit-log view (`l`): a scrollable list of commits with j/k navigation.
/// When browsing, Return opens the selected commit's diff in a [`CommitView`];
/// in a select mode, Return confirms the commit for the pending action.
struct LogState {
    entries: Vec<magritte_core::LogEntry>,
    selected: usize,
    scroll: UniformListScrollHandle,
    load: LogLoad,
    purpose: LogPurpose,
}

/// Load state of the log view, so the body can distinguish still-loading from a
/// load error from a genuinely empty history.
enum LogLoad {
    Loading,
    Loaded,
    Failed(String),
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

/// A single commit's detail (opened from the log): its header and diff, as the
/// same flattened rows the commit editor renders.
struct CommitView {
    /// The commit's full hash — passed to `diff_commit` and copied by the
    /// header's copy button.
    rev: String,
    /// The abbreviated hash, shown in the header next to the copy button.
    short: SharedString,
    /// The commit subject, shown after the hash in the header.
    subject: SharedString,
    rows: Vec<CommitDiffRow>,
    scroll: UniformListScrollHandle,
    /// The cursor row (drives scrolling) and the visual-selection anchor, so
    /// lines can be selected and yanked here too.
    selected: usize,
    visual: Option<usize>,
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

/// In a transient, save the current switch toggles as its defaults (magit's
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

fn section_source(section: SectionId) -> Option<DiffSource> {
    match section {
        SectionId::Unstaged => Some(DiffSource::Unstaged),
        SectionId::Staged => Some(DiffSource::Staged),
        // Untracked, ignored, and the commit/stash sections have no diff source.
        SectionId::Untracked
        | SectionId::Stashes
        | SectionId::Unpushed
        | SectionId::Unpulled
        | SectionId::UnpushedPushremote
        | SectionId::UnpulledPushremote
        | SectionId::Recent
        | SectionId::Ignored => None,
    }
}

/// git convention: keep the commit summary within 50 columns, and wrap the
/// body at 72.
const COMMIT_TITLE_LIMIT: usize = 50;
const COMMIT_BODY_WIDTH: usize = 72;
/// The key a transient suffix is invoked by, for matching `[transient]`
/// `"key" = "unbound"` removals. `None` for `Info` rows (no toggle key).
fn suffix_key(s: &Suffix) -> Option<&str> {
    match s {
        Suffix::Switch(sw) => Some(&sw.key),
        Suffix::Action(a) => Some(a.key),
        Suffix::Option(o) => Some(o.key),
        Suffix::Custom(c) => Some(&c.key),
        Suffix::Info(_) => None,
    }
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
        // Commit/stash/ignored sections carry no file-staging verbs (never
        // reached via a file Target, but the match must be exhaustive).
        SectionId::Stashes
        | SectionId::Unpushed
        | SectionId::Unpulled
        | SectionId::UnpushedPushremote
        | SectionId::UnpulledPushremote
        | SectionId::Recent
        | SectionId::Ignored => (false, false, false),
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
        /// The item count shown after the title, or `None` to omit it (e.g. the
        /// recent-commits section, which is capped to a fixed number anyway).
        count: Option<usize>,
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
    /// A commit row in a non-file section (unpushed/unpulled/recent): dim short
    /// hash, ref labels, and subject, like the log view. `hash` (full) drives
    /// act-at-point; `refs` are the classified `%D` decorations, parsed once at
    /// row-build time (not per frame).
    Commit {
        hash: String,
        short_hash: String,
        subject: String,
        refs: Vec<(String, RefKind)>,
    },
    /// A stash row: dim reference + message.
    Stash {
        reference: String,
        message: String,
    },
}

/// A pending yes/no confirmation shown in the bottom bar.
enum Confirm {
    /// A destructive staging action awaiting `y`.
    Action(Action),
    /// `c c` with nothing staged: on `y`, commit all tracked changes by
    /// opening the message editor with `--all` (the carried switches) appended.
    CommitAll(Vec<String>),
    /// Amend/reword/extend of an already-published HEAD: on `y`, proceed with
    /// the carried command + switches (rewriting pushed history).
    AmendPushed(transient::Command, Vec<String>),
    /// Abort the in-progress sequence (discards its progress): on `y`, run the
    /// abort for the carried kind.
    AbortSequence(SequenceKind),
    /// Destructive reset (hard or worktree-only, discards uncommitted changes):
    /// on `y`, reset to the target in the carried mode.
    Reset(ResetMode, String),
    /// Interactive rebase since an already-published commit: on `y`, open the
    /// todo editor for `rev^..HEAD` with the carried switches (rewriting pushed
    /// history).
    RebaseSincePushed { rev: String, args: Vec<String> },
    /// A user `[[command]]` that looks destructive (resolved command): on `y`,
    /// run it via the shell, refreshing unless opted out.
    CustomShell { command: String, refresh: bool },
    /// Drop the stash at point (`x` on a stash row): on `y`, drop the reference.
    DropStash(String),
}

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
    Revert,
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
    /// The interactive-rebase todo editor (`r i`).
    RebaseTodo(RebaseTodoView),
}

struct StatusView {
    /// The directory we tried to open (for error messages).
    root: PathBuf,
    repo: Option<Repo>,
    status: Option<Status>,
    /// Commit/stash lists for the non-file status sections (unpushed/unpulled/
    /// recent/stashes), refreshed alongside `status` off the UI thread.
    status_sections: StatusSections,
    /// Paths with an unmerged (conflicted) status, refreshed with `rebuild_rows`
    /// so `is_conflicted` is an O(1) lookup rather than an O(entries) scan per
    /// row per frame in `render_row`.
    conflicted: HashSet<String>,
    /// The in-progress merge/rebase/cherry-pick/revert/am, surfaced as a banner.
    sequence: Option<Sequence>,
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
    /// A prefix key awaiting the next key of a sequence (e.g. `g` before `g r`),
    /// with the generation that scopes its timeout. Any key that starts a
    /// multi-key binding can be a prefix; `None` when none is pending.
    pending_prefix: Option<PendingPrefix>,
    /// Bumped each time a prefix is entered, so a stale timeout (a newer prefix,
    /// or a resolved one) is ignored.
    prefix_gen: Generation,
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
    /// The loaded user config (theme/appearance/font), kept so we can re-apply
    /// on config-file edits or system appearance changes.
    config: config::Config,
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
    /// Per-command usage, for ranking the `:` palette by frecency.
    usage: config::Usage,
    /// Saved per-transient switch defaults (magit's `transient-save`), global scope.
    transient_switches: config::TransientSwitches,
    /// The same, scoped to this repo (`.git/magritte/transient-switches.toml`),
    /// overlaid on the global ones (repo wins per transient id). Empty with no repo.
    repo_transient_switches: config::TransientSwitches,
    /// This repo's settings dir (`.git/magritte`), for repo-scoped saves and the
    /// live-reload watcher. `None` with no repo.
    repo_scope_dir: Option<PathBuf>,
    /// The title-bar tag display: (nearest tag behind + commits-since, nearest
    /// tag ahead + commits-until). Refreshed with status when `show_tags` is on.
    tag_info: (Option<(String, usize)>, Option<(String, usize)>),
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
        // The repo's settings scope (`.git/magritte`) and its saved switch sets,
        // overlaid on the global ones when a transient opens.
        let repo_scope_dir = repo
            .as_ref()
            .and_then(|r| r.git_common_dir())
            .map(|d| config::repo_dir(&d));
        let repo_transient_switches = repo_scope_dir
            .as_ref()
            .map(|d| config::load_transient_switches_at(&d.join("transient-switches.toml")))
            .unwrap_or_default();
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
        if let Some(dir) = &repo_scope_dir {
            for id in config::load_fold_state(&dir.join("folds.toml")).collapsed {
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
            tag_info: (None, None),
            conflicted: HashSet::new(),
            sequence: None,
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
            generation: Generation::default(),
            read_cancel: Arc::new(AtomicBool::new(false)),
            job_cancel: None,
            screen_gen: Generation::default(),
            pending_prefix: None,
            prefix_gen: Generation::default(),
            popup: None,
            screen: Screen::Status,
            font,
            ui_font,
            config,
            keymap,
            _config_watcher: None,
            _appearance_sub: None,
            _activation_sub: None,
            usage: config::load_usage(),
            transient_switches: config::load_transient_switches(),
            repo_transient_switches,
            repo_scope_dir,
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
            if window.is_window_active()
                && view.config.refresh_on_focus
                && view.status.is_some()
                && matches!(view.screen, Screen::Status)
            {
                view.refresh(cx);
            }
        }));

        // Config file: watch its directory (so atomic save-via-rename, which
        // swaps the inode, still fires), forward matching events over a channel,
        // and re-apply on the UI thread. Watching the dir lets us pick up the
        // sibling transient-switches.toml too, while ignoring other siblings (e.g.
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
        // Which watched file changed — kept distinct so a transient-switches edit
        // doesn't run the config-reload path (theme rebuild, "Settings reloaded"
        // toast). All reload live, like the config always has.
        enum Changed {
            Config,
            TransientSwitches,
            RepoTransientSwitches,
        }
        let tv_target = config::transient_switches_path()
            .and_then(|p| p.file_name().map(|n| dir.join(n)));
        // The repo scope's settings dir, if it exists yet (canonicalize fails
        // otherwise) — so we can watch its config.toml / transient-switches.toml.
        // Created lazily on the first repo-scoped save, so a brand-new repo picks
        // it up next launch; an in-app save updates memory directly anyway.
        let repo_scope = self
            .repo_scope_dir
            .as_ref()
            .and_then(|d| std::fs::canonicalize(d).ok());
        let repo_tv_target = repo_scope.as_ref().map(|d| d.join("transient-switches.toml"));
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
                    let _ = tx.send_blocking(Changed::TransientSwitches);
                } else if cb_repo_tv.as_ref().is_some_and(|t| event.paths.contains(t)) {
                    let _ = tx.send_blocking(Changed::RepoTransientSwitches);
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
                    Changed::TransientSwitches => {
                        let values = config::load_transient_switches();
                        this.update_in(cx, |view, _window, cx| {
                            // Skip our own Ctrl-s save (we update in memory first,
                            // so the reload reads back identical values).
                            if values != view.transient_switches {
                                view.transient_switches = values;
                                view.set_status(
                                    "Switch defaults reloaded from disk".to_string(),
                                    true,
                                    cx,
                                );
                            }
                        })
                    }
                    Changed::RepoTransientSwitches => {
                        let values = repo_tv_target
                            .as_ref()
                            .map(|p| config::load_transient_switches_at(p))
                            .unwrap_or_default();
                        this.update_in(cx, |view, _window, cx| {
                            if values != view.repo_transient_switches {
                                view.repo_transient_switches = values;
                                view.set_status(
                                    "Switch defaults reloaded from disk".to_string(),
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
        self.config = cfg;
        self.font = theme::resolve_font(&self.config, cx);
        self.ui_font = theme::resolve_ui_font(&self.config, cx);
        let (keymap, mut warnings) = build_keymap(&self.config);
        self.keymap = keymap;
        warnings.extend(theme::config_value_warnings(&self.config, cx));
        self.reapply_theme(cx);
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

    /// The repo cloned for a background *read* (status/diff/prefetch), tagged
    /// with the current generation's cancel flag so a later `refresh` kills it.
    fn read_repo(&self) -> Option<magritte_core::Repo> {
        self.repo
            .clone()
            .map(|r| r.with_cancel(self.read_cancel.clone()))
    }

    /// Reload status from scratch, invalidating any in-flight work.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Capture the cursor's logical position so we can restore it after the
        // rebuild rather than leaving it at the same numeric index.
        let anchor = self.capture_anchor();
        // Cancel the previous generation's in-flight reads (kill the processes,
        // not just drop their results) and start a fresh cancel scope.
        self.read_cancel.store(true, Ordering::Relaxed);
        self.read_cancel = Arc::new(AtomicBool::new(false));
        let generation = self.generation.bump();
        self.diffs.clear();
        self.highlights.clear();
        self.diff_langs.clear();
        // Hunk indices shift when the diff changes, so don't carry collapse
        // state across a refresh.
        self.collapsed_hunks.clear();
        self.error = None;

        let Some(repo) = self.read_repo() else {
            self.error = Some(format!("Not a git repository: {}", self.root.display()));
            self.rebuild_rows();
            return;
        };

        let recent_count = self.config.status.recent_count;
        let want_ignored = self
            .config
            .status
            .section_ids()
            .iter()
            .any(|s| s == "ignored");
        let want_tags = self.config.show_tags;
        cx.spawn(async move |this, cx| {
            let (result, sequence, sections, tag_info) = cx
                .background_executor()
                .spawn(async move {
                    let status = repo.status();
                    let sequence = repo.sequence();
                    let tag_info = if want_tags {
                        repo.tags_around()
                    } else {
                        (None, None)
                    };
                    // The push target's listings only matter (and only resolve)
                    // in a triangular workflow — skip the git calls otherwise.
                    let triangular = status.as_ref().is_ok_and(|s| s.head.push.is_some());
                    // The non-file section listings (cheap git log / stash list).
                    // A missing upstream/push just yields an empty list.
                    let sections = StatusSections {
                        unpushed: repo.unpushed().unwrap_or_default(),
                        unpulled: repo.unpulled().unwrap_or_default(),
                        unpushed_pushremote: if triangular {
                            repo.unpushed_to_push().unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                        unpulled_pushremote: if triangular {
                            repo.unpulled_from_push().unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                        recent: repo.log("HEAD", recent_count).unwrap_or_default(),
                        stashes: repo.stash_list().unwrap_or_default(),
                        ignored: if want_ignored {
                            repo.ignored_files().unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                    };
                    (status, sequence, sections, tag_info)
                })
                .await;
            this.update(cx, |this, cx| {
                if !this.generation.is_current(generation) {
                    return;
                }
                this.sequence = sequence;
                this.status_sections = sections;
                this.tag_info = tag_info;
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
        let key = (source, path.clone());
        if self.diffs.contains_key(&key) {
            return;
        }
        let Some(repo) = self.read_repo() else {
            return;
        };
        self.diffs.insert(key.clone(), DiffState::Loading);
        let generation = self.generation.current();

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
                count: Some(entries.len()),
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
    /// over one `RowKind::Commit` per commit. Skipped when empty, like
    /// [`push_section`]. `count` is shown after the title when `Some` — `None`
    /// for the recent section, which is capped to a fixed number anyway.
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

    /// The loaded diff for a file in a given section, if available.
    fn diff_for(&self, file: &FileRef) -> Option<FileDiff> {
        self.diff_for_ref(file).cloned()
    }

    /// Borrow the loaded diff for `file`, for read-only lookups (a hunk's line
    /// count, a target line) that would otherwise clone the whole `FileDiff`
    /// just to read a field. `diff_for` is the owning variant, used only when an
    /// `Action` needs to keep the diff.
    fn diff_for_ref(&self, file: &FileRef) -> Option<&FileDiff> {
        let source = section_source(file.section)?;
        match self.diffs.get(&(source, file.path.clone()))? {
            DiffState::Loaded(diff) => Some(diff),
            _ => None,
        }
    }

    /// Whether `path` is an unmerged (conflicted) entry. Conflict resolution
    /// isn't supported in-app yet, so ordinary stage/unstage/discard is refused
    /// on these — `git add` would silently mark a conflict resolved (markers and
    /// all), and a discard could lose work.
    fn is_conflicted(&self, path: &str) -> bool {
        // O(1) against the set refreshed in `rebuild_rows`.
        self.conflicted.contains(path)
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
        // A conflicted path refuses unstage/discard, but `Stage` is allowed
        // through (its markers were already checked in `act`) so that staging a
        // manually-resolved conflict marks it resolved (`git add`).
        if op != Op::Stage && self.is_conflicted(target_path(&target)) {
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
                // Untracked has no hunks; commit/stash sections never reach here.
                _ => None,
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
                _ => None,
            },

            _ => None,
        }
    }

    /// The inclusive row range of the active visual selection, if any.
    fn visual_range(&self) -> Option<(usize, usize)> {
        self.visual
            .map(|anchor| (anchor.min(self.selected), anchor.max(self.selected)))
    }

    /// Copy the visual selection (rows joined by newlines), or the row at point
    /// when there's no selection, and flash a confirmation. Yanks the displayed
    /// text — for a diff line that's its content, without the `+`/`-` prefix.
    /// Exits visual mode (like an evil yank).
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = if let Some((lo, hi)) = self.visual_range() {
            let hi = hi.min(self.rows.len().saturating_sub(1));
            self.rows[lo..=hi]
                .iter()
                .map(row_text)
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(row) = self.rows.get(self.selected) {
            row_text(row)
        } else {
            return;
        };
        self.visual = None;
        self.copy_to_clipboard(text, cx);
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
                        if let Some(h) = self.diff_for_ref(file).and_then(|d| d.hunks.get(*hunk)) {
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

    /// Open the file at point (its row, or the file a hunk/line belongs to) in
    /// the external editor, at the diff's line when one is known. Bound to Return.
    fn open_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rows.get(self.selected).and_then(|r| r.target.clone()) else {
            return;
        };
        let path = match &target {
            Target::File(f) => f.path.clone(),
            Target::Hunk { file, .. } | Target::Line { file, .. } => file.path.clone(),
        };
        let line = self.diff_target_line(&target);
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let full = repo.workdir().join(&path);
        self.launch_editor(&full, line);
        self.set_status(format!("Opening {path}"), true, cx);
    }

    /// The new-side line number to open at for a target: the line at point, the
    /// hunk's first line, or the file's first hunk (where its diff starts).
    /// `None` when the diff isn't loaded (a collapsed file) — open without a line.
    fn diff_target_line(&self, target: &Target) -> Option<u32> {
        match target {
            Target::File(f) => self.diff_for_ref(f)?.hunks.first().map(|h| h.new_start),
            Target::Hunk { file, hunk } => self
                .diff_for_ref(file)?
                .hunks
                .get(*hunk)
                .map(|h| h.new_start),
            Target::Line { file, hunk, line } => {
                let diff = self.diff_for_ref(file)?;
                let h = diff.hunks.get(*hunk)?;
                // A deleted line has no new-side number; fall back to the hunk.
                Some(
                    h.lines
                        .get(*line)
                        .and_then(|l| l.new_lineno)
                        .unwrap_or(h.new_start),
                )
            }
        }
    }

    /// Open `path` in the user's configured editor, at `line` when given and the
    /// editor's goto convention is known (see [`editor_goto`]). An empty `editor`
    /// opens the OS default app; otherwise `editor` is run as a command
    /// (`code -w`, `zed`) and, failing that on macOS, treated as an application
    /// name to `open -a` (so "Zed" or "Visual Studio Code" work too).
    /// Best-effort, non-blocking.
    fn launch_editor(&self, path: &std::path::Path, line: Option<u32>) {
        let editor = self.config.editor.trim();
        if editor.is_empty() {
            editor_launch::open_with_os(path);
            return;
        }
        // Open at the line when we have one and recognize how this editor takes
        // a goto target; otherwise fall through to a plain open.
        if let Some(line) = line {
            if let Some((program, args)) =
                editor_launch::editor_goto(editor, &path.to_string_lossy(), line)
            {
                if std::process::Command::new(program)
                    .args(args)
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
        // First try `editor` as a command: program + optional flags, then the
        // file. This is how CLI launchers are written (`code -w`, `zed`,
        // `subl -n`, `/abs/path/to/bin`).
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or(editor);
        let args: Vec<&str> = parts.collect();
        if std::process::Command::new(program)
            .args(args)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return;
        }
        // The command wasn't on PATH. On macOS the user likely typed an
        // application *name* ("Zed", "Visual Studio Code") rather than a CLI
        // command — open the file with that app via `open -a`. (`open` always
        // spawns; a bad app name just surfaces its own error.)
        #[cfg(target_os = "macos")]
        if std::process::Command::new("open")
            .arg("-a")
            .arg(editor)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return;
        }
        // Nothing launched — fall back to the OS default handler.
        editor_launch::open_with_os(path);
    }

    /// Resolve the conflicted file at point by keeping one side (`git checkout
    /// --ours|--theirs` + stage), then refresh.
    fn resolve_at_point(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        let Some(path) = self.conflicted_in_selection() else {
            return;
        };
        self.run_job(
            &format!("Resolving {path}…"),
            "Resolved",
            move |repo| repo.resolve_conflict(&path, side).map(|()| String::new()),
            cx,
        );
    }

    /// Labels for the take-ours / take-theirs conflict actions. git's `--ours`
    /// and `--theirs` swap meaning across operations: in a merge "ours" is the
    /// current branch and "theirs" the branch merged in, but a rebase/cherry-
    /// pick replays your commits onto the other side, so "ours" is the side
    /// already applied and "theirs" is the commit being replayed — the opposite
    /// of what intuition says. Name each side by what it actually is.
    fn conflict_side_labels(&self) -> (&'static str, &'static str) {
        match self.sequence.as_ref().map(|s| &s.kind) {
            Some(SequenceKind::Rebase) => ("Take ours (rebased onto)", "Take theirs (your commit)"),
            Some(SequenceKind::CherryPick) => {
                ("Take ours (HEAD)", "Take theirs (cherry-picked commit)")
            }
            Some(SequenceKind::Revert) => ("Take ours (HEAD)", "Take theirs (reverted commit)"),
            Some(SequenceKind::Am) => ("Take ours (HEAD)", "Take theirs (patch)"),
            Some(SequenceKind::Merge) | None => ("Take ours (current)", "Take theirs (incoming)"),
        }
    }

    /// Whether the worktree file at `path` still contains conflict markers, so
    /// staging it wouldn't silently mark an unresolved conflict resolved.
    fn has_conflict_markers(&self, path: &str) -> bool {
        let Some(repo) = self.repo.as_ref() else {
            return false;
        };
        // A read failure (unreadable, binary, gone) means we can't prove the
        // file is clean — be conservative and treat it as still-conflicted, so
        // staging can't silently mark an unresolved conflict resolved. (We match
        // only the `<<<<<<<`/`>>>>>>>` pair, not a bare `=======`, which occurs
        // in ordinary text and would false-positive.)
        std::fs::read_to_string(repo.workdir().join(path))
            .map(|c| {
                c.lines()
                    .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> "))
            })
            .unwrap_or(true)
    }

    /// `s`/`u`/`x`: resolve and either run, or (for discard) ask to confirm.
    fn act(&mut self, op: Op, cx: &mut Context<Self>) {
        // A conflicted file in the selection: staging it marks it resolved, so
        // allow that once its markers are gone (manual resolution); otherwise
        // refuse the whole action rather than act on a subset, and say why.
        if let Some(path) = self.conflicted_in_selection() {
            let resolvable = op == Op::Stage && !self.has_conflict_markers(&path);
            if !resolvable {
                let msg = if op == Op::Stage {
                    format!("{path} still has conflict markers — resolve them first")
                } else {
                    format!("{path} is conflicted — resolve it first")
                };
                self.set_status(msg, false, cx);
                return;
            }
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
                // Status, not `error`: refresh() clears `error` at its top, so a
                // failure stored there would never be shown.
                match result {
                    Ok(()) => this.clear_status(cx),
                    Err(e) => this.set_status(format!("error: {e}"), false, cx),
                }
                this.refresh(cx);
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
            Some((_, Confirm::AmendPushed(command, switches))) => {
                self.proceed_history_rewrite(command, switches, window, cx);
            }
            Some((_, Confirm::AbortSequence(kind))) => self.run_sequence(SeqOp::Abort, kind, cx),
            Some((_, Confirm::Reset(mode, target))) => self.do_reset(mode, target, cx),
            Some((_, Confirm::RebaseSincePushed { rev, args })) => {
                self.open_rebase_todo(format!("{rev}^"), args, cx)
            }
            Some((_, Confirm::CustomShell { command, refresh })) => {
                self.run_custom_shell(command, refresh, cx)
            }
            Some((_, Confirm::DropStash(reference))) => {
                self.run_stash_action(StashAction::Drop, reference, cx)
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

    /// Open a transient, injecting any user-configured suffixes for it. `id` is
    /// the transient's command id (`branch`, `commit`, …); pass `""` for ad-hoc
    /// transients (e.g. an in-progress sequence) that take no user suffixes.
    fn open_transient(
        &mut self,
        id: &str,
        mut def: Transient,
        targets: RemoteTargets,
        cx: &mut Context<Self>,
    ) {
        // `"key" = "unbound"` removes the built-in suffix at that key
        // (keymap-style), so a user can drop a default flag/action — or replace
        // it by also binding their own at the same key.
        let unbinds: std::collections::HashSet<&str> = self
            .config
            .transient
            .get(id)
            .into_iter()
            .flatten()
            .filter(|(_, spec)| spec.is_unbound())
            .map(|(k, _)| k.as_str())
            .collect();
        if !unbinds.is_empty() {
            for g in def.groups.iter_mut() {
                g.suffixes
                    .retain(|s| suffix_key(s).is_none_or(|k| !unbinds.contains(k)));
            }
            // Drop a section emptied by the removals.
            def.groups.retain(|g| !g.suffixes.is_empty());
        }

        // Each injection resolves to a (target section title, suffix). Switches
        // default into the "Arguments" section (where switches live), actions
        // into "Custom"; an explicit `group` overrides.
        let placements: Vec<(String, transient::Suffix)> = self
            .config
            .transient
            .get(id)
            .into_iter()
            .flatten()
            // Skip the `"unbound"` removal entries (handled above).
            .filter(|(_, spec)| !spec.is_unbound())
            .filter_map(|(key, spec)| match spec.kind() {
                // A custom switch (toggleable git flag). Skip if the key collides
                // with a built-in switch/option (which wins).
                config::SuffixKind::Switch {
                    flag,
                    description,
                    group,
                } => {
                    if def.switches().any(|s| s.key == *key) || def.option_for(key).is_some() {
                        return None;
                    }
                    let suffix = transient::Suffix::Switch(transient::Switch::new(
                        key.clone(),
                        flag.to_string(),
                        description.to_string(),
                    ));
                    Some((group.unwrap_or("Arguments").to_string(), suffix))
                }
                // A custom action runs a command by id. Skip if the key collides
                // with a built-in action (which wins).
                config::SuffixKind::Action { id, group } => {
                    if def.action_for(key).is_some() {
                        return None;
                    }
                    // Label it with the command's title (built-in or user),
                    // falling back to the raw id if it names nothing.
                    let description = all_commands(&self.config)
                        .find(|c| c.id == id)
                        .map(|c| c.title.to_string())
                        .unwrap_or_else(|| id.to_string());
                    let suffix = transient::Suffix::Custom(transient::Custom {
                        key: key.clone(),
                        description,
                        id: id.to_string(),
                    });
                    Some((group.unwrap_or("Custom").to_string(), suffix))
                }
            })
            .collect();
        // Append into the named section if it exists, else create it.
        for (group_title, suffix) in placements {
            match def.groups.iter_mut().find(|g| group_text(g) == group_title) {
                Some(g) => g.suffixes.push(suffix),
                None => def.groups.push(transient::Group {
                    title: transient::plain_title(group_title),
                    suffixes: vec![suffix],
                }),
            }
        }
        // A switch tied to a git-config key starts on when that config is
        // enabled (e.g. --gpg-sign with commit.gpgSign=true); toggling it off
        // then sends the negation (--no-gpg-sign). Resolve those defaults now,
        // from the repo's effective config.
        if let Some(repo) = self.repo.as_ref() {
            for group in def.groups.iter_mut() {
                for suffix in group.suffixes.iter_mut() {
                    if let transient::Suffix::Switch(sw) = suffix {
                        if let Some(key) = sw.config_key.clone() {
                            sw.default_on = match key.as_str() {
                                // pull.rebase is an enum (true/interactive/merges)
                                // with a per-branch override, so it needs git's
                                // own resolution rather than a plain bool read.
                                "pull.rebase" => {
                                    repo.pull_rebase_default(targets.branch.as_deref())
                                }
                                _ => repo.config_bool(&key),
                            };
                        }
                    }
                }
            }
        }
        let mut state = TransientState::new(id, def, targets);
        // A saved switch set (magit's `transient-save`) overrides this
        // transient's defaults; that becomes the baseline, so the save hint only
        // appears once the user changes it. A negatable (config-derived) switch
        // is overridden only when the saved set names it *explicitly* — its key
        // (force on) or its negation flag (force off); otherwise it keeps the
        // config default, so an old/empty saved set can't silently flip e.g.
        // gpg-signing off.
        if let Some(saved) = self.saved_switches(id) {
            state.active = TransientState::apply_saved(&state.def, saved);
            state.baseline = state.active.clone();
        }
        self.popup = Some(Popup::Transient(state));
        cx.notify();
    }

    /// The saved switch set in effect for a transient id: the repo scope wins
    /// wholesale over the global scope (per-id replace), so a repo's entry fully
    /// defines that transient's defaults while global still covers the rest.
    fn saved_switches(&self, id: &str) -> Option<&Vec<String>> {
        self.repo_transient_switches
            .get(id)
            .or_else(|| self.transient_switches.get(id))
    }

    /// Persist the open transient's switch overrides to a scope (magit's
    /// `transient-save`), updating the in-memory set and the scope's file, and
    /// re-baselining so the save hint hides. Repo scope is a no-op with no repo.
    fn save_transient_defaults(&mut self, repo_scope: bool, cx: &mut Context<Self>) {
        let to_save = match &self.popup {
            Some(Popup::Transient(s)) if !s.id.is_empty() => Some((s.id.clone(), s.saved_overrides())),
            _ => None,
        };
        let Some((id, switches)) = to_save else {
            return;
        };
        let path = if repo_scope {
            let Some(dir) = self.repo_scope_dir.clone() else {
                return; // no repo to save into
            };
            dir.join("transient-switches.toml")
        } else {
            let Some(path) = config::transient_switches_path() else {
                return;
            };
            path
        };
        let values = if repo_scope {
            &mut self.repo_transient_switches
        } else {
            &mut self.transient_switches
        };
        // An empty set carries no overrides — drop the entry rather than writing
        // `id = []`, which used to read as "force everything off".
        if switches.is_empty() {
            values.remove(&id);
        } else {
            values.insert(id, switches);
        }
        config::save_transient_switches_at(&path, values);
        // The saved set is the new baseline, so the hint hides again.
        if let Some(Popup::Transient(s)) = self.popup.as_mut() {
            s.baseline = s.active.clone();
        }
        let scope = if repo_scope { "for this repo" } else { "globally" };
        self.set_status(format!("Saved switches {scope}"), true, cx);
    }

    /// The current branch's resolved push/pull/fetch targets (empty on error or
    /// no repo), for building and dispatching the remote transients.
    fn remote_targets(&self) -> RemoteTargets {
        self.repo
            .as_ref()
            .and_then(|r| r.remote_targets().ok())
            .unwrap_or_default()
    }

    fn handle_transient_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A save is in progress (the save key was pressed): this key picks the
        // scope — `g`lobal or `l`ocal (this repo). Any other key, including
        // Esc/C-g, cancels and stays in the transient. Handled first so it
        // captures the next keystroke before the close/dispatch paths below.
        let pending_save = matches!(&self.popup, Some(Popup::Transient(s)) if s.pending_save);
        if pending_save {
            if let Some(Popup::Transient(s)) = self.popup.as_mut() {
                s.pending_save = false;
            }
            match key {
                "g" => self.save_transient_defaults(false, cx),
                "l" if self.repo_scope_dir.is_some() => self.save_transient_defaults(true, cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        if key == "escape" || key == "q" {
            self.popup = None;
            cx.notify();
            return;
        }
        // The save key (`C-s`) begins a two-step save (magit's `transient-save`):
        // it doesn't save yet — the next key chooses the scope. Skipped for
        // ad-hoc transients (empty id), which have nothing to key the save by.
        if key == TRANSIENT_SAVE_KEY {
            if let Some(Popup::Transient(s)) = self.popup.as_mut() {
                if !s.id.is_empty() {
                    s.pending_save = true;
                    cx.notify();
                }
            }
            return;
        }
        let Some(Popup::Transient(state)) = self.popup.as_mut() else {
            return;
        };

        // Switches toggle magit-style (`-` then the letter, e.g. -f); a
        // value-reading option (e.g. -F) instead prompts for its value.
        if state.pending_dash {
            state.pending_dash = false;
            let full = format!("-{key}");
            if state.def.switches().any(|s| s.key == full) {
                if !state.active.remove(&full) {
                    state.active.insert(full);
                }
                cx.notify();
                return;
            }
            // Reading the option metadata ends the `state` borrow before we move
            // the transient into the prompt as its resume target.
            let opt = state
                .def
                .option_for(&full)
                .map(|o| (o.key.to_string(), o.description.to_string(), o.completion));
            if let Some((key, description, completion)) = opt {
                if let Some(Popup::Transient(ts)) = self.popup.take() {
                    self.open_option_prompt(key, description, completion, ts, window, cx);
                }
                return;
            }
            cx.notify();
            return;
        }
        if key == "-" {
            state.pending_dash = true;
            cx.notify();
            return;
        }

        // Invoke an action — or a user-injected custom suffix (which runs a
        // registry command by id, with default args).
        let action = state.def.action_for(key).cloned();
        let custom = state.def.custom_for(key).cloned();
        // The active git arguments: toggled switches plus set option values.
        let args = state.args();
        // Pathspec limits trail the revision behind a `--` (log only).
        let paths = state.pathspecs();
        let targets = state.targets.clone();
        let limit = state
            .values
            .get("-n")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(Self::LOG_LIMIT);
        if let Some(action) = action {
            let fired = ActionArgs {
                args,
                paths,
                targets,
                limit,
            };
            self.fire_action(action.command, fired, window, cx);
        } else if let Some(custom) = custom {
            self.popup = None;
            self.invoke_command(&custom.id, window, cx);
        }
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
            self.set_status("Nothing staged (or unstaged)".to_string(), false, cx);
            return;
        }
        self.confirm = Some((
            // `--all` stages tracked modifications/deletions only — untracked
            // files are never included, so don't promise "all changes".
            "Nothing staged. Commit all tracked changes?".to_string(),
            Confirm::CommitAll(switches),
        ));
        cx.notify();
    }

    /// React to an edit in the commit message: auto-wrap the body (if enabled)
    /// and refresh the over-50 summary warning (if enabled). Reads the toggles
    /// live from config so the settings screen takes effect without reopening.
    fn on_editor_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        let wrap = self.config.commit_body_wrap;
        let ruler = self.config.commit_title_ruler;
        state.update(cx, |s, cx| {
            if wrap {
                let value = s.value().to_string();
                let offset = s.cursor();
                if let Some(wrapped) =
                    commit_text::wrap_at_cursor(&value, offset, COMMIT_BODY_WIDTH)
                {
                    // Wrapping only turns a space into a newline, so the cursor's
                    // byte offset is unchanged — recompute its line/column in the
                    // rewrapped text and restore it.
                    s.set_value(wrapped.clone(), window, cx);
                    s.set_cursor_position(
                        commit_text::byte_offset_to_position(&wrapped, offset),
                        window,
                        cx,
                    );
                }
            }
            // Diagnostics carry their own copy of the text for position math;
            // reset it to the current value, then flag any summary overflow.
            let rope = s.text().clone();
            if let Some(diags) = s.diagnostics_mut() {
                diags.reset(&rope);
                if ruler {
                    if let Some((start, end)) =
                        commit_text::title_overflow(&rope.to_string(), COMMIT_TITLE_LIMIT)
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
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        state.update(cx, |s, cx| {
            let value = s.value().to_string();
            let reflowed = commit_text::reflow_body(&value, COMMIT_BODY_WIDTH);
            if reflowed != value {
                let end = reflowed.len(); // byte offset of the end
                s.set_value(reflowed.clone(), window, cx);
                s.set_cursor_position(
                    commit_text::byte_offset_to_position(&reflowed, end),
                    window,
                    cx,
                );
            }
        });
        // Refresh the summary warning against the reflowed text.
        self.on_editor_changed(window, cx);
    }

    /// The `GIT_EDITOR` command for writing commit messages in an external
    /// editor, or `None` (use the in-app editor) when none is configured. The
    /// configured command is used verbatim — the user supplies a blocking
    /// `--wait`-style flag as their editor requires.
    fn external_commit_editor(&self) -> Option<String> {
        if !self.config.commit_in_editor {
            return None;
        }
        let cmd = self.config.commit_editor.trim();
        (!cmd.is_empty()).then(|| cmd.to_string())
    }

    /// Make a commit by launching the external editor on its message (an
    /// interactive `git commit` on the background executor). The editor blocks
    /// git until it's closed; we show a waiting notice meanwhile, then report
    /// the outcome and refresh — an empty/aborted message surfaces as an error.
    fn commit_via_external_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        git_editor: String,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (waiting, done) = match mode {
            CommitMode::Create => ("Waiting for commit message…", "Committed"),
            CommitMode::Amend => ("Waiting for amended message…", "Amended"),
            CommitMode::Reword => ("Waiting for reworded message…", "Reworded"),
        };
        self.set_status(waiting.to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.commit_with_editor(mode, &args, &git_editor) })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If the user opted to write commit messages in their external editor,
        // hand off to an interactive `git commit` instead of the in-app editor.
        if let Some(git_editor) = self.external_commit_editor() {
            self.commit_via_external_editor(mode, args, git_editor, cx);
            return;
        }
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
        // Focus on the next frame, not now: the keystroke that opened the editor
        // (`c`) is still mid-dispatch, and focusing synchronously would let that
        // character land in the message (see open_picker for the same reasoning).
        let to_focus = state.clone();
        cx.on_next_frame(window, move |_this, window, cx| {
            to_focus.read(cx).focus_handle(cx).focus(window, cx);
        });
        self.screen = Screen::Editor(CommitEditor {
            state: state.clone(),
            mode,
            args,
            initial: String::new(),
            confirming_cancel: false,
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
                                s.set_value(msg.clone(), window, cx);
                            }
                        });
                    });
                    // set_value doesn't emit Change, so update the summary
                    // warning for the pre-filled message ourselves. Also record
                    // HEAD's message as the baseline, so canceling an unedited
                    // amend/reword doesn't prompt to discard.
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(ed) = this.editor_mut() {
                            ed.initial = msg;
                        }
                        this.on_editor_changed(window, cx);
                    });
                })
                .detach();
            }
        }
        // Preview the relevant diff: the staged change for create/amend, or the
        // reworded commit's own changes for reword.
        self.load_commit_diff(cx);
        cx.notify();
    }

    /// Load the diff to preview in the open editor, in the background, and
    /// flatten it (with syntax highlighting) for read-only display. Create/amend
    /// show the staged diff being committed (or, with `--all`, every tracked
    /// change vs HEAD that the commit will include); reword shows the diff of the
    /// commit it's renaming (HEAD's own changes), since it makes no tree change.
    fn load_commit_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(ed) = self.editor() else {
            return;
        };
        let reword = ed.mode == CommitMode::Reword;
        let also_unstaged = ed.args.iter().any(|a| a == "--all");
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    let loaded = if reword {
                        repo.diff_commit("HEAD")
                    } else if also_unstaged {
                        // `--all` records every tracked change vs HEAD, so
                        // preview that — not just the staged side, which would
                        // hide tracked unstaged work the commit will include.
                        repo.diff_tracked_vs_head()
                    } else {
                        repo.diff_all(DiffSource::Staged)
                    };
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
                if this.editor().is_none() {
                    return; // editor closed before the diff loaded
                }
                if let Some(err) = error {
                    if let Some(ed) = this.editor_mut() {
                        ed.diff = vec![CommitDiffRow::Note(format!("diff unavailable: {err}"))];
                    }
                    cx.notify();
                    return;
                }
                let rows = this.diff_rows(&files, cx);
                if let Some(ed) = this.editor_mut() {
                    ed.diff = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Flatten loaded file diffs (each paired with its detected language) into
    /// displayable rows with syntax highlighting. Shared by the commit editor's
    /// preview and the log's commit-detail view.
    fn diff_rows(
        &self,
        files: &[(FileDiff, Option<&'static str>)],
        cx: &mut Context<Self>,
    ) -> Vec<CommitDiffRow> {
        let default = cx.theme().foreground;
        let (fg, dim) = (self.palette.fg, self.palette.dim);
        let mut rows = Vec::new();
        for (diff, lang) in files {
            rows.push(CommitDiffRow::File(diff.display_path().to_string()));
            let hl = match lang {
                Some(l) if !diff.is_binary => Some(highlight::highlight_diff(diff, l, cx, default)),
                _ => None,
            };
            for (hi, hunk) in diff.hunks.iter().enumerate() {
                rows.push(CommitDiffRow::Hunk(status_label::hunk_header_text(hunk)));
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
        rows
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
        // The vertico picker's query input is focused, so steal navigation /
        // confirm / cancel keys before the input consumes them; everything else
        // (text, backspace) falls through to filter the list.
        if matches!(self.popup, Some(Popup::Picker(_))) {
            let ctrl = event.keystroke.modifiers.control;
            // Emacs minibuffer aliases: C-g cancels, C-n/C-p move the selection.
            let key = match event.keystroke.key.as_str() {
                "g" if ctrl => "escape",
                "n" if ctrl => "down",
                "p" if ctrl => "up",
                k => k,
            };
            match key {
                "up" => {
                    cx.stop_propagation();
                    self.picker_move(-1, cx);
                }
                "down" => {
                    cx.stop_propagation();
                    self.picker_move(1, cx);
                }
                "enter" => {
                    cx.stop_propagation();
                    self.confirm_picker(window, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    self.cancel_popup(window, cx);
                }
                _ => {}
            }
            return;
        }

        if self.editor().is_none() {
            return;
        }
        // C-g cancels here too; C-n/C-p are left to the Input for cursor motion.
        let key = match event.keystroke.key.as_str() {
            "g" if event.keystroke.modifiers.control => "escape",
            k => k,
        };
        // While the "discard message?" confirmation is up, capture y / n / esc.
        if self.editor().is_some_and(|e| e.confirming_cancel) {
            match key {
                "y" => {
                    cx.stop_propagation();
                    self.discard_editor(window, cx);
                }
                "n" | "escape" => {
                    cx.stop_propagation();
                    self.keep_editing(window, cx);
                }
                _ => {}
            }
            return;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.cancel_editor(window, cx);
        } else if key == "q" && event.keystroke.modifiers.alt {
            // alt-q reflows the body (Emacs fill-paragraph heritage); capture it
            // so the Input doesn't insert the character.
            cx.stop_propagation();
            self.reflow_editor(window, cx);
        }
    }

    /// Cancel the editor — but if there are unsaved edits, ask first rather than
    /// silently dropping the message.
    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = match self.editor() {
            Some(ed) => ed.state.read(cx).value().trim() != ed.initial.trim(),
            None => return,
        };
        if dirty {
            if let Some(ed) = self.editor_mut() {
                ed.confirming_cancel = true;
            }
            cx.notify();
        } else {
            self.discard_editor(window, cx);
        }
    }

    /// Close the editor, discarding its message.
    fn discard_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the discard confirmation and keep editing.
    fn keep_editing(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.editor_mut() {
            ed.confirming_cancel = false;
        }
        cx.notify();
    }

    /// Open the git command-log view (magit's `$` process buffer), scrolled to
    /// the most recent command.
    fn open_git_log(&mut self, cx: &mut Context<Self>) {
        // Dismiss any status toast — you came here to read the full output it
        // pointed at, and it would otherwise just float over this view.
        self.clear_status(cx);
        let scroll = UniformListScrollHandle::new();
        let last = self.git_log_rows().len().saturating_sub(1);
        scroll.scroll_to_item(last, gpui::ScrollStrategy::Bottom);
        self.screen = Screen::GitLog(ScrollView { scroll, top: last });
        cx.notify();
    }

    fn close_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Toggle whether the command log also lists the UI's own read-only queries.
    fn toggle_git_log_all(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.git_log_show_all = !self.git_log_show_all;
        cx.notify();
    }

    /// How many recent commits the log loads. Bounded so opening the log in a
    /// huge repo stays cheap; the bar notes when it's capped.
    const LOG_LIMIT: usize = 256;

    /// Open the commit-log view for `git log <args>`: show it immediately
    /// (empty), then load the commits off the UI thread. Args are assembled by
    /// [`build_log_args`] (including the default limit).
    fn start_log(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.show_log_loading(LogPurpose::Browse, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.log_with(&args) })
                .await;
            this.update(cx, |this, cx| this.fill_log(gen, result, cx))
                .ok();
        })
        .detach();
    }

    /// Open the log to pick the commit to rebase interactively *since* — magit's
    /// `magit-log-select`. The chosen commit and everything above it become the
    /// editable todo; `switches` carries the rebase transient's flags.
    fn start_log_select_rebase(&mut self, switches: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.show_log_loading(LogPurpose::SelectRebaseBase { args: switches }, cx);
        let args = build_log_args(Vec::new(), LogScope::Current, Vec::new(), Self::LOG_LIMIT);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.log_with(&args) })
                .await;
            this.update(cx, |this, cx| this.fill_log(gen, result, cx))
                .ok();
        })
        .detach();
    }

    /// Begin an interactive rebase since the commit selected in the log (its
    /// parent is the base, so that commit and everything above it are editable),
    /// opening the todo editor. `args` are the rebase switches. First checks
    /// (off the UI thread) whether that commit is already published; if so,
    /// confirm before rewriting pushed history — like magit's rebase assert and
    /// our amend/reword warning.
    fn rebase_since_selected(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(rev) = self
            .log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.short_hash.clone())
        else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let probe = rev.clone();
        cx.spawn(async move |this, cx| {
            let branches = cx
                .background_executor()
                .spawn(async move { repo.published_branches(&probe).unwrap_or_default() })
                .await;
            this.update(cx, |this, cx| {
                // base = commit^: `base..HEAD` then includes the selected commit.
                if branches.is_empty() {
                    this.open_rebase_todo(format!("{rev}^"), args, cx);
                    return;
                }
                // The confirmation bar is status-screen chrome, so leave the log
                // to show it; "yes" opens the todo editor.
                this.screen = Screen::Status;
                let target = match branches.as_slice() {
                    [one] => one.clone(),
                    many => format!("{} remote branches", many.len()),
                };
                this.confirm = Some((
                    format!("{rev} has already been pushed to {target}. Rebase since it anyway?"),
                    Confirm::RebaseSincePushed { rev, args },
                ));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the selected commit's diff (the clickable "view" button; Return does
    /// the same from the key handler).
    fn view_log_commit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_commit_view(cx);
    }

    /// Confirm the selected commit in a log-select mode (the clickable "select"
    /// button; Return does the same from the key handler).
    fn confirm_log_select(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(LogPurpose::SelectRebaseBase { args }) = self.log().map(|l| &l.purpose) {
            let args = args.clone();
            self.rebase_since_selected(args, cx);
        }
    }

    /// Open the reflog view (`l r`).
    fn start_reflog(&mut self, limit: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.show_log_loading(LogPurpose::Browse, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.reflog(limit) })
                .await;
            this.update(cx, |this, cx| this.fill_log(gen, result, cx))
                .ok();
        })
        .detach();
    }

    /// Show the (empty) log view immediately while commits load, returning the
    /// screen-load generation the matching `fill_log` must still see.
    fn show_log_loading(&mut self, purpose: LogPurpose, cx: &mut Context<Self>) -> u64 {
        let gen = self.next_screen_gen();
        self.screen = Screen::Log(LogState {
            entries: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            load: LogLoad::Loading,
            purpose,
        });
        cx.notify();
        gen
    }

    /// Fill the open log view with the load result: entries on success, the
    /// error otherwise (so the view shows it rather than an endless "Loading…").
    fn fill_log(
        &mut self,
        gen: u64,
        result: magritte_core::Result<Vec<magritte_core::LogEntry>>,
        cx: &mut Context<Self>,
    ) {
        // Drop a load a newer log/reflog request has superseded.
        if !self.screen_gen.is_current(gen) {
            return;
        }
        if let Some(log) = self.log_mut() {
            match result {
                Ok(entries) => {
                    log.entries = entries;
                    log.load = LogLoad::Loaded;
                }
                Err(e) => log.load = LogLoad::Failed(e.to_string()),
            }
        }
        cx.notify();
    }

    fn close_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Move the log's selection by `delta`, keeping it in view.
    fn log_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(log) = self.log_mut() {
            if log.entries.is_empty() {
                return;
            }
            let last = log.entries.len() - 1;
            log.selected = (log.selected as isize + delta).clamp(0, last as isize) as usize;
            log.scroll
                .scroll_to_item(log.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Cherry-pick or revert the commit selected in the log, then return to the
    /// status view (so a conflict shows in the in-progress banner). Runs on the
    /// background executor.
    fn pick_selected(&mut self, op: PickOp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rev) = self
            .log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.short_hash.clone())
        else {
            return;
        };
        let (verb, done) = match op {
            PickOp::CherryPick => ("Cherry-picking", "Cherry-picked"),
            PickOp::Revert => ("Reverting", "Reverted"),
        };
        self.close_log(window, cx);
        self.run_job(
            &format!("{verb} {rev}…"),
            done,
            move |repo| match op {
                PickOp::CherryPick => repo.cherry_pick(&rev),
                PickOp::Revert => repo.revert(&rev),
            },
            cx,
        );
    }

    /// Advance and return the screen-load generation. A screen-changing async
    /// load captures this and re-checks it before mutating the screen.
    fn next_screen_gen(&mut self) -> u64 {
        self.screen_gen.bump()
    }

    /// Open the commit selected in the log (Enter in the log view).
    fn open_commit_view(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.log().and_then(|l| l.entries.get(l.selected).cloned()) else {
            return;
        };
        self.open_commit(entry.hash, entry.short_hash, entry.subject, cx);
    }

    /// Open a commit's diff detail, overlaying the current screen (restored on
    /// close). Shared by the log view and status commit rows.
    fn open_commit(&mut self, hash: String, short: String, subject: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
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
                rows: vec![CommitDiffRow::Note("Loading…".to_string())],
                scroll: UniformListScrollHandle::new(),
                selected: 0,
                visual: None,
            },
            back,
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    repo.diff_commit(&rev).map(|diffs| {
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
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                // Bail if a newer screen load superseded this one, or the view
                // was closed before the diff arrived.
                if !this.screen_gen.is_current(gen) || this.commit_view().is_none() {
                    return;
                }
                let rows = match loaded {
                    Ok(files) => this.diff_rows(&files, cx),
                    Err(e) => vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))],
                };
                if let Some(cv) = this.commit_view_mut() {
                    cv.rows = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Return to the screen the commit view was opened from (log or status).
        if let Screen::Commit { back, .. } = std::mem::take(&mut self.screen) {
            self.screen = *back;
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Copy the full hash of the commit selected in the log.
    fn copy_log_commit(&mut self, cx: &mut Context<Self>) {
        let hash = self
            .log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.hash.clone());
        if let Some(hash) = hash {
            self.copy_to_clipboard(hash, cx);
        }
    }

    /// Move the commit-view cursor by `delta`, keeping it in view.
    fn commit_view_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(cv) = self.commit_view_mut() {
            if cv.rows.is_empty() {
                return;
            }
            let last = cv.rows.len() as isize - 1;
            cv.selected = (cv.selected as isize + delta).clamp(0, last) as usize;
            cv.scroll
                .scroll_to_item(cv.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Toggle a visual selection in the commit view, anchored at the cursor.
    fn commit_view_toggle_visual(&mut self, cx: &mut Context<Self>) {
        if let Some(cv) = self.commit_view_mut() {
            cv.visual = if cv.visual.is_some() {
                None
            } else {
                Some(cv.selected)
            };
            cx.notify();
        }
    }

    /// Copy the commit view's visual selection (or the line at point), then
    /// exit visual mode — the diff-view counterpart to [`Self::copy_selection`].
    fn copy_commit_selection(&mut self, cx: &mut Context<Self>) {
        let text = {
            let Some(cv) = self.commit_view() else {
                return;
            };
            let (lo, hi) = match cv.visual {
                Some(a) => (a.min(cv.selected), a.max(cv.selected)),
                None => (cv.selected, cv.selected),
            };
            let hi = hi.min(cv.rows.len().saturating_sub(1));
            cv.rows[lo..=hi]
                .iter()
                .map(commit_row_text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if let Some(cv) = self.commit_view_mut() {
            cv.visual = None;
        }
        self.copy_to_clipboard(text, cx);
    }

    fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor() else {
            return;
        };
        let text = ed.state.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.set_status("Commit message is empty".to_string(), false, cx);
            return;
        }
        let ed = self.take_editor().unwrap();
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
        self.run_job(
            "Committing…",
            "Committed",
            move |repo| repo.commit(&message, mode, &args),
            cx,
        );
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
        CommitDiffRow::File(p) => p.clone(),
        CommitDiffRow::Hunk(h) => h.clone(),
        CommitDiffRow::Line { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        CommitDiffRow::Note(n) => n.clone(),
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
        PushPushRemote | PushUpstream | PushElsewhere => "Pushing",
        PullPushRemote | PullUpstream | PullElsewhere => "Pulling",
        FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => "Fetching",
        CommitCreate | CommitAmend | CommitReword | CommitExtend => "Committing",
        // Branch, stash, and log commands route through their own picker/runner.
        BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
            "Working"
        }
        StashPush | StashPushAll | StashApply | StashPop | StashDrop => "Stashing",
        LogCurrent | LogAll | LogOther | LogReflog => "Logging",
        ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => "Resetting",
        MergePlain | MergeNoCommit | MergeSquash => "Merging",
        RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive => {
            "Rebasing"
        }
        IgnoreToplevel | IgnoreSubdir | IgnorePrivate | IgnoreGlobal => "Ignoring",
        // These route through run_sequence / the todo editor, which set their
        // own progress text.
        SequenceContinue | SequenceSkip | SequenceAbort | SequenceEditTodo => "Working",
    }
}

/// Past-tense success notice for a command (shown briefly when it succeeds).
fn command_done(command: transient::Command) -> &'static str {
    use transient::Command::*;
    match command {
        PushPushRemote | PushUpstream | PushElsewhere => "Pushed",
        PullPushRemote | PullUpstream | PullElsewhere => "Pulled",
        FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => "Fetched",
        CommitCreate | CommitAmend | CommitReword | CommitExtend => "Committed",
        BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
            "Done"
        }
        StashPush | StashPushAll | StashApply | StashPop | StashDrop => "Stashed",
        LogCurrent | LogAll | LogOther | LogReflog => "Done",
        ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => "Reset",
        MergePlain | MergeNoCommit | MergeSquash => "Merged",
        RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive => {
            "Rebased"
        }
        IgnoreToplevel | IgnoreSubdir | IgnorePrivate | IgnoreGlobal => "Ignored",
        SequenceContinue | SequenceSkip | SequenceAbort | SequenceEditTodo => "Done",
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
        theme::register_bundled_themes(cx);
        // Apply the saved appearance/themes. Theme::change first ensures the
        // Theme global exists so apply_appearance can set its slots.
        let (cfg, cfg_warning) = config::load_reporting();
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
        cx.activate(true);

        // A reasonable default window instead of filling the whole screen;
        // centered on the active display. The user can resize freely.
        let bounds = Bounds::centered(None, size(px(1000.0), px(720.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Transparent system bar so our custom `TitleBar` draws the chrome
            // (and the traffic lights sit where the component expects them).
            titlebar: Some(gpui_component::TitleBar::title_bar_options()),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            let window = cx
                .open_window(options, |window, cx| {
                    let view = cx.new(|cx| {
                        StatusView::new(start_dir.clone(), cfg.clone(), cfg_warning.clone(), cx)
                    });
                    // Now that the window exists, install the live-reload watchers
                    // (the appearance observer needs `&mut Window`).
                    view.update(cx, |view, cx| view.install_watchers(window, cx));
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
            // Surface invariants: a `?`-menu command needs a key; a leaf (no key)
            // must be palette-only.
            assert_eq!(
                c.menu,
                c.menu && c.key.is_some(),
                "menu command {:?} has no key",
                c.id
            );
            if c.key.is_none() {
                assert!(!c.menu, "keyless command {:?} can't be in the menu", c.id);
                assert!(
                    c.palette,
                    "keyless command {:?} should be in the palette",
                    c.id
                );
            }
        }
        // Every menu command is actually reachable from the `?` dispatch menu.
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
        for c in commands().iter().filter(|c| c.menu) {
            let key = c.key.unwrap();
            assert!(
                menu.contains(key),
                "menu command {:?} ({key}) missing from dispatch menu",
                c.id
            );
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
            "c", "b", "Z", "l", "p", "F", "f", "O", "m", "r", "i", "!", ",", "$", // commands
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

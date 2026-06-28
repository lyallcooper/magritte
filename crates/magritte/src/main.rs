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
    Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};

use gpui::prelude::FluentBuilder;

mod commit_text;
mod config;
mod controller;
#[cfg(feature = "debug")]
mod debug;
mod editor_launch;
mod editors;
mod git_action;
mod highlight;
mod kbd;
mod picker;
mod settings;
mod status_label;
mod targets;
mod theme;
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
actions!(magritte, [CopyConfigPath]);
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = magritte, no_json)]
struct OpenConfigWith(SharedString);
use gpui::Subscription;
use gpui_component::button::{Button, ButtonVariants, DropdownButton};
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::input::{Input, InputEvent, InputState, Position};
use gpui_component::menu::ContextMenuExt;
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Icon, IconName, IndexPath, Sizable};
use magritte_core::transient::{self, Group, Suffix, TitleSpan, Transient};
use magritte_core::{
    CommitMode, ConflictSide, DiffSource, EntryKind, FileDiff, FileEntry, IgnoreDest, LineKind,
    RebaseAction, RemoteTargets, Repo, ResetMode, Sequence, SequenceKind, Status,
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
    def: Transient,
    active: std::collections::HashSet<String>,
    /// Value-reading option values, keyed by the option's key (e.g. `-F` →
    /// `fix bug`). Combined with `active` to build the git argument list.
    values: std::collections::HashMap<String, String>,
    /// True after `-` is pressed, awaiting the switch/option letter (magit `-f`).
    pending_dash: bool,
    /// Resolved push/pull/fetch targets, so dispatch can route to the right
    /// remote without recomputing (empty for non-remote transients).
    targets: RemoteTargets,
}

impl TransientState {
    fn new(def: Transient, targets: RemoteTargets) -> Self {
        // Switches flagged default-on start toggled on (the user can turn them
        // off); the rest start off.
        let active = def
            .switches()
            .filter(|s| s.default_on)
            .map(|s| s.key.to_string())
            .collect();
        TransientState {
            def,
            active,
            values: std::collections::HashMap::new(),
            pending_dash: false,
            targets,
        }
    }

    /// The git flag arguments from the toggled switches and set options, in
    /// definition order (switches first, then options as `{arg}{value}`).
    /// Pathspec options are excluded — see [`Self::pathspecs`] — since they must
    /// trail the revision behind a `--`.
    fn args(&self) -> Vec<String> {
        let switches = self
            .def
            .switches()
            .filter(|s| self.active.contains(s.key))
            .map(|s| s.arg.to_string());
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

/// The arguments a leaf command runs with: the toggled switches/options, any
/// pathspec limits, the resolved remote targets, and the log commit limit.
/// Gathered from a transient's state, or [`ActionArgs::defaults`] for a
/// palette-fired command (no switches).
struct ActionArgs {
    args: Vec<String>,
    paths: Vec<String>,
    targets: RemoteTargets,
    limit: usize,
}

impl ActionArgs {
    fn defaults(targets: RemoteTargets, limit: usize) -> Self {
        Self {
            args: Vec::new(),
            paths: Vec::new(),
            targets,
            limit,
        }
    }
}

/// Groupings for the command registry — the `?` menu and `:` palette render in
/// this order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    /// Git porcelain (commit, branch, push, …).
    Commands,
    /// App/chrome commands that aren't git operations (settings, command log).
    Application,
    /// Working-tree edits (stage / unstage / discard).
    Applying,
    /// Always-available essentials (fold, refresh, visual selection).
    Essential,
    /// Cursor motions (move, page, section, edges). Resolved through the keymap
    /// like any command — so they're remappable — but dispatched screen-aware.
    Navigation,
}

impl Category {
    fn title(self) -> &'static str {
        match self {
            Category::Commands => "Commands",
            Category::Application => "Application",
            Category::Applying => "Applying changes",
            Category::Essential => "Essential",
            Category::Navigation => "Navigation",
        }
    }
}

/// A user-invokable command: a stable identity decoupled from the key that
/// triggers it. This is the single source of truth for *what a command does* —
/// the keymap (`on_key` via [`StatusView::run_command`], the `?` dispatch menu,
/// and the `:` command palette all resolve to one of these and call `run`.
/// Argument-taking commands (commit, branch, …) open their own picker/transient
/// from `run`; the registry deliberately doesn't model arguments.
#[derive(Clone, Copy)]
struct Command {
    /// Stable id, e.g. "stage", "branch", "push-upstream". Used by the keymap,
    /// the palette (resolving the chosen title), and tests.
    id: &'static str,
    /// Human label shown in the `?` menu and `:` palette.
    title: &'static str,
    /// Which `?`-menu group / palette category it belongs to.
    category: Category,
    /// Default keybinding, as the dispatch menu renders it (e.g. "Z", "g r").
    /// `None` for leaf subcommands reached via a transient or the palette, not a
    /// top-level key.
    key: Option<&'static str>,
    /// Show in the `?` dispatch menu. Mirrors magit's curated dispatch: the
    /// top-level prefixes and direct actions, not every leaf.
    menu: bool,
    /// Offer in the `:` command palette. Mirrors magit's `M-x`: prefixes *and*
    /// the leaf subcommands (e.g. "Push current to upstream").
    palette: bool,
    /// Whether it makes sense to offer right now — the palette filters on this.
    /// (Permissive today; argument-gathering happens in `run`.)
    enabled: fn(&StatusView) -> bool,
    /// For a leaf, the transient suffix it fires — used to show its full key
    /// sequence (prefix + suffix, e.g. `c c`) in the palette. `None` for
    /// top-level prefixes/actions, which advertise their own `key`.
    leaf: Option<transient::Command>,
    /// Perform the command. May open a transient/picker or act immediately.
    run: fn(&mut StatusView, &mut Window, &mut Context<StatusView>),
}

/// The command registry: the one place commands are defined. Pure motions
/// (j/k/gg/G/gj/gk) are not commands and stay in the keymap. Keep keys in sync
/// with the modal handling in `on_key` (shift variants, the `g` prefix); the
/// `dispatch_menu_covers_every_command` test guards menu/registry/dispatch
/// against drift.
fn commands() -> &'static [Command] {
    use transient::Command as Leaf;
    const ALWAYS: fn(&StatusView) -> bool = |_| true;

    // A top-level prefix or direct action: bound to a key, in the `?` menu and
    // the palette.
    macro_rules! top {
        ($id:literal, $title:literal, $cat:expr, $key:literal, $run:expr) => {
            Command {
                id: $id,
                title: $title,
                category: $cat,
                key: Some($key),
                menu: true,
                palette: true,
                enabled: ALWAYS,
                leaf: None,
                run: $run,
            }
        };
    }
    // A motion: bound to a key (so the keymap can remap it) but kept out of the
    // `?` menu and `:` palette. Its `run` is screen-aware.
    macro_rules! nav {
        ($id:literal, $title:literal, $key:literal, $run:expr) => {
            Command {
                id: $id,
                title: $title,
                category: Category::Navigation,
                key: Some($key),
                menu: false,
                palette: false,
                enabled: ALWAYS,
                leaf: None,
                run: $run,
            }
        };
    }
    // A leaf subcommand (a transient suffix): no top-level key, palette-only —
    // it's surfaced in the `?` menu through its prefix's transient. Firing it
    // runs the action directly with default arguments.
    macro_rules! leaf {
        ($id:literal, $title:literal, $cmd:expr) => {
            Command {
                id: $id,
                title: $title,
                category: Category::Commands,
                key: None,
                menu: false,
                palette: true,
                enabled: ALWAYS,
                leaf: Some($cmd),
                run: |t, w, cx| t.fire_command_default($cmd, w, cx),
            }
        };
    }

    const C: &[Command] = &[
        // Prefixes (open a transient).
        top!("commit", "Commit", Category::Commands, "c", |t, _w, cx| {
            t.open_transient(
                "commit",
                transient::commit_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("branch", "Branch", Category::Commands, "b", |t, _w, cx| {
            // The branch transient (checkout/create/rename/delete) doesn't use
            // remote targets, so don't resolve them just to open it.
            t.open_transient(
                "branch",
                transient::branch_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("stash", "Stash", Category::Commands, "Z", |t, _w, cx| {
            t.open_transient(
                "stash",
                transient::stash_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("reset", "Reset", Category::Commands, "O", |t, _w, cx| {
            t.open_transient(
                "reset",
                transient::reset_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!(
            "git-command",
            "Run command",
            Category::Commands,
            "!",
            |t, w, cx| { t.open_run_git(w, cx) }
        ),
        top!("rebase", "Rebase", Category::Commands, "r", |t, _w, cx| {
            // While a rebase is paused, `r` opens the continue/skip/abort
            // transient (magit's `r r` = continue) rather than starting a new one.
            if matches!(
                t.sequence.as_ref().map(|s| s.kind),
                Some(SequenceKind::Rebase)
            ) {
                t.open_transient(
                    "",
                    transient::sequence_transient(SequenceKind::Rebase),
                    RemoteTargets::default(),
                    cx,
                );
            } else {
                let rt = t.remote_targets();
                t.open_transient("rebase", transient::rebase_transient(&rt), rt, cx);
            }
        }),
        top!("merge", "Merge", Category::Commands, "m", |t, _w, cx| {
            // While a merge is in progress, `m` opens its abort action (you
            // finish a merge by committing); don't start another.
            if matches!(
                t.sequence.as_ref().map(|s| s.kind),
                Some(SequenceKind::Merge)
            ) {
                t.open_transient(
                    "",
                    transient::sequence_transient(SequenceKind::Merge),
                    RemoteTargets::default(),
                    cx,
                );
            } else {
                t.open_transient(
                    "merge",
                    transient::merge_transient(),
                    RemoteTargets::default(),
                    cx,
                );
            }
        }),
        top!("ignore", "Ignore", Category::Commands, "i", |t, _w, cx| {
            t.open_transient(
                "ignore",
                transient::ignore_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("log", "Log", Category::Commands, "l", |t, _w, cx| {
            t.open_transient(
                "log",
                transient::log_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("push", "Push", Category::Commands, "p", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient("push", transient::push_transient(&rt), rt, cx)
        }),
        top!("pull", "Pull", Category::Commands, "F", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient("pull", transient::pull_transient(&rt), rt, cx)
        }),
        top!("fetch", "Fetch", Category::Commands, "f", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient("fetch", transient::fetch_transient(&rt), rt, cx)
        }),
        // Leaf subcommands (palette-only; reached in the `?` menu via their
        // prefix's transient).
        leaf!("commit-create", "Create commit", Leaf::CommitCreate),
        leaf!("commit-amend", "Amend commit", Leaf::CommitAmend),
        leaf!("commit-reword", "Reword commit", Leaf::CommitReword),
        leaf!(
            "commit-extend",
            "Extend commit (keep message)",
            Leaf::CommitExtend
        ),
        leaf!(
            "push-pushremote",
            "Push current to push-remote",
            Leaf::PushPushRemote
        ),
        leaf!(
            "push-upstream",
            "Push current to upstream",
            Leaf::PushUpstream
        ),
        leaf!("push-elsewhere", "Push elsewhere", Leaf::PushElsewhere),
        leaf!(
            "pull-pushremote",
            "Pull from push-remote",
            Leaf::PullPushRemote
        ),
        leaf!("pull-upstream", "Pull from upstream", Leaf::PullUpstream),
        leaf!("pull-elsewhere", "Pull elsewhere", Leaf::PullElsewhere),
        leaf!(
            "fetch-pushremote",
            "Fetch push-remote",
            Leaf::FetchPushRemote
        ),
        leaf!("fetch-upstream", "Fetch upstream", Leaf::FetchUpstream),
        leaf!("fetch-all", "Fetch all remotes", Leaf::FetchAll),
        leaf!("fetch-elsewhere", "Fetch elsewhere", Leaf::FetchElsewhere),
        leaf!(
            "branch-checkout",
            "Checkout branch/revision",
            Leaf::BranchCheckout
        ),
        leaf!(
            "branch-create-checkout",
            "Create and checkout branch",
            Leaf::BranchCreateCheckout
        ),
        leaf!("branch-create", "Create branch", Leaf::BranchCreate),
        leaf!("branch-rename", "Rename branch", Leaf::BranchRename),
        leaf!("branch-delete", "Delete branch", Leaf::BranchDelete),
        leaf!("stash-push", "Stash worktree and index", Leaf::StashPush),
        leaf!(
            "stash-push-all",
            "Stash including untracked",
            Leaf::StashPushAll
        ),
        leaf!("stash-apply", "Apply stash", Leaf::StashApply),
        leaf!("stash-pop", "Pop stash", Leaf::StashPop),
        leaf!("stash-drop", "Drop stash", Leaf::StashDrop),
        leaf!("log-current", "Log current", Leaf::LogCurrent),
        leaf!("log-all", "Log all branches", Leaf::LogAll),
        leaf!("log-other", "Log other ref", Leaf::LogOther),
        leaf!("log-reflog", "Reflog", Leaf::LogReflog),
        // Application commands.
        top!(
            "settings",
            "Settings",
            Category::Application,
            ",",
            |t, w, cx| { t.open_settings(w, cx) }
        ),
        top!(
            "git-log",
            "Git command log",
            Category::Application,
            "$",
            |t, _w, cx| { t.open_git_log(cx) }
        ),
        // Applying changes.
        top!("stage", "Stage", Category::Applying, "s", |t, _w, cx| t
            .act(Op::Stage, cx)),
        top!(
            "unstage",
            "Unstage",
            Category::Applying,
            "u",
            |t, _w, cx| t.act(Op::Unstage, cx)
        ),
        top!(
            "stage-all",
            "Stage all",
            Category::Applying,
            "S",
            |t, _w, cx| { t.run_action(Action::StageAll, cx) }
        ),
        top!(
            "unstage-all",
            "Unstage all",
            Category::Applying,
            "U",
            |t, _w, cx| { t.run_action(Action::UnstageAll, cx) }
        ),
        top!(
            "discard",
            "Discard",
            Category::Applying,
            "x",
            |t, _w, cx| t.act(Op::Discard, cx)
        ),
        // Essentials.
        top!(
            "open-file",
            "Open file",
            Category::Essential,
            "enter",
            |t, _w, cx| { t.open_at_point(cx) }
        ),
        top!(
            "fold",
            "Fold / unfold",
            Category::Essential,
            "tab",
            |t, _w, cx| { t.toggle_fold(cx) }
        ),
        top!(
            "refresh",
            "Refresh",
            Category::Essential,
            "g r",
            |t, _w, cx| {
                t.refresh(cx);
                cx.notify();
            }
        ),
        top!(
            "visual",
            "Visual selection",
            Category::Essential,
            "v",
            |t, _w, cx| {
                t.visual = if t.visual.is_some() {
                    None
                } else {
                    Some(t.selected)
                };
                cx.notify();
            }
        ),
        top!("yank", "Copy", Category::Essential, "y", |t, _w, cx| t
            .copy_selection(cx)),
        // Motions: resolved through the keymap (so remappable) but applied
        // screen-aware via the `nav_*` helpers. Kept out of the `?` menu (the
        // static Navigation group shows them) and the `:` palette (navigating
        // from a list picker is pointless).
        nav!("move-down", "Move down", "j", |t, _w, cx| t.nav_line(1, cx)),
        nav!("move-up", "Move up", "k", |t, _w, cx| t.nav_line(-1, cx)),
        nav!("goto-top", "Top", "g g", |t, _w, cx| t.nav_edge(false, cx)),
        nav!("goto-bottom", "Bottom", "G", |t, _w, cx| t
            .nav_edge(true, cx)),
        nav!("next-section", "Next section", "g j", |t, _w, cx| t
            .nav_section(true, cx)),
        nav!("prev-section", "Previous section", "g k", |t, _w, cx| t
            .nav_section(false, cx)),
        nav!("half-page-down", "Half page down", "C-d", |t, w, cx| t
            .nav_page(true, false, w, cx)),
        nav!("half-page-up", "Half page up", "C-u", |t, w, cx| t
            .nav_page(false, false, w, cx)),
        nav!("page-down", "Page down", "C-f", |t, w, cx| t
            .nav_page(true, true, w, cx)),
        nav!("page-up", "Page up", "C-b", |t, w, cx| t
            .nav_page(false, true, w, cx)),
        // Quit (Emacs `C-x C-c`, bound in DEFAULT_BINDINGS): no single key, so a
        // literal rather than `top!`. Reachable via the palette too.
        Command {
            id: "quit",
            title: "Quit",
            category: Category::Application,
            key: None,
            menu: false,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |_t, _w, cx| cx.quit(),
        },
    ];
    C
}

/// Default *secondary* key bindings: aliases layered onto the registry's primary
/// keys in [`build_keymap`] (before the user's `[keymap]`, so they're remappable
/// and unbindable like any default). These keep modifier/arrow/sequence aliases
/// in the one keymap rather than hardcoded in the key handler.
const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    // Arrow + Emacs cursor motions.
    ("down", "move-down"),
    ("up", "move-up"),
    ("C-n", "move-down"),
    ("C-p", "move-up"),
    // Paging: full page also on Space; sections also on Emacs/bracket keys.
    ("space", "page-down"),
    ("C-j", "next-section"),
    ("C-k", "prev-section"),
    ("]", "next-section"),
    ("[", "prev-section"),
    // Emacs quit.
    ("C-x C-c", "quit"),
];

/// Canonical keystroke string for a keypress: modifier prefixes (`D-` cmd, `C-`
/// ctrl, `M-` alt) then the key, with a shifted letter uppercased (so `K`, not
/// `S-k`, matching the rest of the keymap). One token; multi-key sequences join
/// these with spaces (`C-x C-c`).
fn chord(key: &str, shift: bool, ctrl: bool, alt: bool, cmd: bool) -> String {
    let base = if shift && key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic()) {
        key.to_uppercase()
    } else {
        key.to_string()
    };
    let mut s = String::new();
    if cmd {
        s.push_str("D-");
    }
    if ctrl {
        s.push_str("C-");
    }
    if alt {
        s.push_str("M-");
    }
    s.push_str(&base);
    s
}

/// The effective keystroke → command-id map: the built-in defaults (every
/// registry command that has a key) overlaid with the user's `[keymap]`. A value
/// of `"unbound"` removes a default binding; an unknown id is skipped with a
/// warning rather than dropped silently. Only command keys live here — motions
/// and prefixes (`j`/`k`/`g …`) stay hardwired in `on_key`/`run_dispatch`.
fn build_keymap(config: &config::Config) -> (HashMap<String, String>, Vec<String>) {
    let mut map: HashMap<String, String> = commands()
        .iter()
        .filter_map(|c| c.key.map(|key| (key.to_string(), c.id.to_string())))
        .collect();
    // Secondary aliases (arrows, Emacs motions, Space, `C-x C-c`) — layered
    // before the user's table so they remap/unbind like any default.
    for (key, id) in DEFAULT_BINDINGS {
        map.insert(key.to_string(), id.to_string());
    }
    let mut warnings = Vec::new();
    // A binding target is valid if it's a built-in command or a user `[[command]]`.
    let known = |id: &str| {
        commands().iter().any(|c| c.id == id) || config.commands.iter().any(|c| c.id == id)
    };
    for (keystroke, id) in &config.keymap {
        if id == "unbound" {
            map.remove(keystroke);
        } else if !known(id) {
            warnings.push(format!("keymap: unknown command id \"{id}\""));
        } else {
            // Any keystroke sequence is allowed, to any depth — `dispatch`
            // accumulates keys until one resolves to a binding (or to nothing).
            map.insert(keystroke.clone(), id.clone());
        }
    }
    // Validate the user `[[command]]` definitions.
    let mut seen_ids = HashSet::new();
    for c in &config.commands {
        if c.run.trim().is_empty() {
            warnings.push(format!("command \"{}\": empty run", c.id));
        }
        if commands().iter().any(|b| b.id == c.id) {
            warnings.push(format!("command \"{}\": shadows a built-in command", c.id));
        }
        if !seen_ids.insert(c.id.as_str()) {
            warnings.push(format!("command \"{}\": duplicate id", c.id));
        }
    }
    // Validate the `[transient]` suffix injections: the section must name a
    // transient, and each value a real command (the injection itself happens in
    // `open_transient`).
    for (tid, suffixes) in &config.transient {
        if !TRANSIENT_IDS.contains(&tid.as_str()) {
            warnings.push(format!("transient: \"{tid}\" is not a transient"));
            continue;
        }
        for id in suffixes.values() {
            if !commands().iter().any(|c| c.id == id) {
                warnings.push(format!("transient.{tid}: unknown command id \"{id}\""));
            }
        }
    }
    (map, warnings)
}

/// Whether a custom command looks like it could throw away work — so the
/// frontend confirms first, like the built-in destructive ops. A word-level
/// scan for `clean`, `--hard`, or `--force`/`--force-with-lease`.
fn command_is_destructive(command: &str) -> bool {
    command.split_whitespace().any(|w| {
        matches!(w, "clean" | "--hard" | "--force" | "--force-with-lease")
    })
}

/// The command ids whose `?`/key opens a transient — the valid `[transient.<id>]`
/// sections for suffix injection.
const TRANSIENT_IDS: &[&str] = &[
    "commit", "branch", "stash", "reset", "rebase", "merge", "ignore", "log", "push", "pull",
    "fetch",
];

/// The keystroke sequence to reach the command with this palette title, as
/// space-separated keys: a top-level command's own key (e.g. `p`), or a leaf's
/// full prefix-then-suffix path (e.g. `c c` for "Create commit"). `None` if it
/// has no binding. Lets the `:` palette double as a keymap reference.
fn command_keys(keymap: &HashMap<String, String>, title: &str) -> Option<String> {
    let cmd = commands().iter().find(|c| c.title == title)?;
    // A current top-level key — including a leaf bound directly to one via
    // `[keymap]`. Reflects remaps and hides what the user unbound.
    if let Some(key) = current_key(keymap, cmd.id, cmd.key) {
        return Some(key);
    }
    // Otherwise a leaf reached through its prefix's transient: `<prefix>
    // <suffix>`, with the prefix's *current* key (the suffix is transient-fixed).
    let leaf = cmd.leaf?;
    let rt = RemoteTargets::default();
    let prefixes: [(&str, Transient); 7] = [
        ("commit", transient::commit_transient()),
        ("branch", transient::branch_transient()),
        ("stash", transient::stash_transient()),
        ("log", transient::log_transient()),
        ("push", transient::push_transient(&rt)),
        ("pull", transient::pull_transient(&rt)),
        ("fetch", transient::fetch_transient(&rt)),
    ];
    for (prefix_id, t) in &prefixes {
        for group in &t.groups {
            for suffix in &group.suffixes {
                if let Suffix::Action(a) = suffix {
                    if a.command == leaf {
                        let default = commands()
                            .iter()
                            .find(|c| c.id == *prefix_id)
                            .and_then(|c| c.key);
                        let prefix_key = current_key(keymap, prefix_id, default)?;
                        return Some(format!("{prefix_key} {}", a.key));
                    }
                }
            }
        }
    }
    None
}

/// The keystroke currently bound to command `id` in the effective `keymap`,
/// preferring its built-in `default` key when that's still bound to it — so the
/// `?` menu shows remapped keys and hides anything the user unbound.
fn current_key(
    keymap: &HashMap<String, String>,
    id: &str,
    default: Option<&str>,
) -> Option<String> {
    if let Some(def) = default {
        if keymap.get(def).map(String::as_str) == Some(id) {
            return Some(def.to_string());
        }
    }
    keymap
        .iter()
        .filter(|(_, v)| v.as_str() == id)
        .map(|(k, _)| k.clone())
        .min()
}

/// The `?` dispatch menu: a modal command transient (magit's dispatch),
/// generated from the [`commands`] registry (grouped by [`Category`]). Each
/// row shows its *current* key from `keymap` — remaps are reflected, and an
/// unbound command is dropped — and is invoked by that key or a click.
///
/// This menu is the discoverable face of the keymap. The
/// `dispatch_menu_covers_every_command` test cross-checks it against the keys
/// `run_dispatch` actually handles, so a command can't be shown-but-dead or
/// invocable-but-hidden.
fn dispatch_menu(keymap: &HashMap<String, String>) -> Transient {
    let group = |cat: Category| Group {
        title: transient::plain_title(cat.title()),
        // Navigation motions have `menu: false` but belong in the menu's
        // Navigation group; every other group shows its `menu` commands.
        suffixes: commands()
            .iter()
            .filter(|c| c.category == cat && (c.menu || cat == Category::Navigation))
            .filter_map(|c| {
                current_key(keymap, c.id, c.key).map(|keys| {
                    Suffix::Info(transient::Info {
                        keys,
                        description: c.title,
                    })
                })
            })
            .collect(),
    };
    // Essential gathers the always-available registry commands plus the `:`
    // palette — itself a meta-affordance (reach any command), not a registry
    // entry, so it's appended here rather than living in `commands()`.
    let mut essential = group(Category::Essential);
    essential.suffixes.push(Suffix::Info(transient::Info {
        keys: ":".to_string(),
        description: "Command palette",
    }));
    Transient {
        title: transient::plain_title("Help"),
        groups: vec![
            group(Category::Commands),
            group(Category::Applying),
            group(Category::Navigation),
            essential,
            group(Category::Application),
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
    /// A commit's diff detail, opened from the log with Enter. It overlays the
    /// log it came from (closing returns there), so it carries that `LogState`.
    Commit { view: CommitView, log: LogState },
    /// The interactive-rebase todo editor (`r i`).
    RebaseTodo(RebaseTodoView),
}

struct StatusView {
    /// The directory we tried to open (for error messages).
    root: PathBuf,
    repo: Option<Repo>,
    status: Option<Status>,
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
    generation: u64,
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
    screen_gen: u64,
    /// A prefix key awaiting the next key of a sequence (e.g. `g` before `g r`),
    /// with the generation that scopes its timeout. Any key that starts a
    /// multi-key binding can be a prefix; `None` when none is pending.
    pending_prefix: Option<PendingPrefix>,
    /// Bumped each time a prefix is entered, so a stale timeout (a newer prefix,
    /// or a resolved one) is ignored.
    prefix_gen: u64,
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
    status_seq: u64,
    /// Bumped per async picker open, stamped onto the picker, so a late
    /// candidate load only fills the picker it was started for.
    picker_gen: u64,
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
        // so opening a large repo loads no diffs until a file is expanded.
        let mut expanded = HashSet::new();
        expanded.insert(FoldKey::Section(SectionId::Untracked));
        expanded.insert(FoldKey::Section(SectionId::Unstaged));
        expanded.insert(FoldKey::Section(SectionId::Staged));

        let mut view = StatusView {
            root,
            repo,
            status: None,
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
            generation: 0,
            read_cancel: Arc::new(AtomicBool::new(false)),
            job_cancel: None,
            screen_gen: 0,
            pending_prefix: None,
            prefix_gen: 0,
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
            mono_fonts: Vec::new(),
            ui_fonts: Vec::new(),
            editors: Vec::new(),
            status_message: startup_warning,
            status_copied: None,
            status_keys: None,
            status_seq: 0,
            picker_gen: 0,
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
        // and re-apply on the UI thread. Watching the dir means we ignore events
        // for siblings like command-usage.toml by matching the exact path.
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
        let (tx, rx) = async_channel::unbounded::<()>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if event.paths.contains(&watch_target) {
                    let _ = tx.send_blocking(());
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        // A missing config dir (no config yet) just means nothing to watch.
        if notify::Watcher::watch(&mut watcher, &dir, notify::RecursiveMode::NonRecursive).is_err()
        {
            return;
        }
        self._config_watcher = Some(watcher);

        // spawn_in so the reload has a Window: applying a config can rebuild the
        // open settings form, whose Select/Input entities need one.
        cx.spawn_in(window, async move |this, cx| {
            while rx.recv().await.is_ok() {
                let (cfg, warning) = config::load_reporting();
                let updated = this.update_in(cx, |view, window, cx| {
                    if let Some(warning) = warning {
                        // The file is now invalid/unreadable. Keep the live
                        // config (don't reset to defaults on a transient bad
                        // edit) and surface why it was ignored.
                        view.set_status(warning, false, cx);
                    } else if cfg != view.config {
                        // Skip an unchanged config (our own in-app save, or a
                        // no-op external edit).
                        view.apply_config(cfg, window, cx);
                    }
                });
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
        self.generation += 1;
        let generation = self.generation;
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

        cx.spawn(async move |this, cx| {
            let (result, sequence) = cx
                .background_executor()
                .spawn(async move { (repo.status(), repo.sequence()) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
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
        let Some(repo) = self.read_repo() else {
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

        // The branch and its upstream/push tracking now live in the title bar
        // (see `render_title_bar`), not in header rows here.
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

    /// Move the cursor by ~`delta` rows for paging (Ctrl-d/u/f/b): clamp the
    /// target into range, then snap to the nearest selectable row (so paging at
    /// the ends lands on the last/first selectable row rather than stalling).
    fn page_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let target = (self.selected as isize + delta).clamp(0, last);
        for d in 0..=last {
            for cand in [target + d, target - d] {
                if (0..=last).contains(&cand) && self.rows[cand as usize].selectable {
                    self.selected = cand as usize;
                    return;
                }
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

    // --- Unified, screen-aware navigation ---------------------------------
    // One [keymap] drives motion in every cursor view: the registry's
    // Navigation commands resolve to these, dispatched to the active screen.

    /// Move the cursor/selection by `delta` rows in the active view.
    fn nav_line(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } => self.commit_view_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            _ => {
                self.move_selection(delta);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Page the cursor by a half- or full-screen in the active view.
    fn nav_page(&mut self, down: bool, full: bool, window: &mut Window, cx: &mut Context<Self>) {
        let page = page_rows(window) as isize;
        let amount = if full { page } else { (page / 2).max(1) };
        let delta = if down { amount } else { -amount };
        match self.screen {
            Screen::Log(_) => self.log_move(delta, cx),
            Screen::Commit { .. } => self.commit_view_move(delta, cx),
            Screen::RebaseTodo(_) => self.rebase_todo_move(delta, cx),
            _ => {
                self.page_selection(delta);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Jump to the first/last row of the active view.
    fn nav_edge(&mut self, to_bottom: bool, cx: &mut Context<Self>) {
        match self.screen {
            Screen::Log(_) | Screen::Commit { .. } | Screen::RebaseTodo(_) => self.nav_line(
                if to_bottom {
                    isize::MAX / 2
                } else {
                    isize::MIN / 2
                },
                cx,
            ),
            _ => {
                self.select_edge(to_bottom);
                self.scroll
                    .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Move to the next/previous section. Only the status view has sections; a
    /// no-op elsewhere.
    fn nav_section(&mut self, forward: bool, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Status) {
            self.select_section(forward);
            self.scroll
                .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Shared key handling for the cursor views (status / log / commit / rebase
    /// todo): the `g` prefix, the fixed motion aliases (arrows, Ctrl-paging,
    /// `]`/`[`), and the remappable motion keys resolved through the effective
    /// keymap. Returns whether it consumed the key.
    fn try_nav(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // All motions (arrows, `C-d`, Space, `]`, the `g` prefix, …) resolve
        // through the effective keymap — there are no hardcoded aliases.
        let chord = chord(key, shift, ctrl, false, false);
        // A prefix key begins a sequence.
        if self.is_prefix(&chord) {
            self.enter_prefix(chord, window, cx);
            return true;
        }
        // Run only if it's a motion, so a command key (e.g. `s`) isn't fired in
        // a non-status view.
        let Some(id) = self.keymap.get(&chord).cloned() else {
            return false;
        };
        if commands()
            .iter()
            .any(|c| c.id == id && c.category == Category::Navigation)
        {
            self.invoke_command(&id, window, cx);
            true
        } else {
            false
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
        let extra: Vec<transient::Suffix> = self
            .config
            .transient
            .get(id)
            .into_iter()
            .flatten()
            // Don't shadow a built-in suffix — its binding wins anyway, so an
            // injected duplicate would just be a dead row.
            .filter(|(key, _)| def.action_for(key).is_none())
            .map(|(key, cmd_id)| {
                let description = commands()
                    .iter()
                    .find(|c| c.id == cmd_id)
                    .map_or_else(|| cmd_id.clone(), |c| c.title.to_string());
                transient::Suffix::Custom(transient::Custom {
                    key: key.clone(),
                    description,
                    id: cmd_id.clone(),
                })
            })
            .collect();
        if !extra.is_empty() {
            def.groups.push(transient::Group {
                title: transient::plain_title("Custom"),
                suffixes: extra,
            });
        }
        self.popup = Some(Popup::Transient(TransientState::new(def, targets)));
        cx.notify();
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
        if key == "escape" || key == "q" {
            self.popup = None;
            cx.notify();
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

    /// How many non-empty output lines the most recent user (`!`) command
    /// produced — to decide whether its result warrants opening the `$` log.
    /// Called right after the command (before the post-job refresh appends its
    /// own queries), so the last user-flagged entry is that command.
    fn last_user_output_lines(&self) -> usize {
        self.repo
            .as_ref()
            .and_then(|r| r.command_log().into_iter().rev().find(|c| c.user))
            .map(|c| {
                c.stdout
                    .lines()
                    .chain(c.stderr.lines())
                    .filter(|l| !l.trim().is_empty())
                    .count()
            })
            .unwrap_or(0)
    }

    /// Open the git command-log view (magit's `$` process buffer), scrolled to
    /// the most recent command.
    fn open_git_log(&mut self, cx: &mut Context<Self>) {
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
        if self.screen_gen != gen {
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

    /// Open the selected commit's diff in a [`CommitView`], loaded off the UI
    /// thread.
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
        self.screen_gen = self.screen_gen.wrapping_add(1);
        self.screen_gen
    }

    fn open_commit_view(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(entry) = self.log().and_then(|l| l.entries.get(l.selected).cloned()) else {
            return;
        };
        let gen = self.next_screen_gen();
        // Move the log into the commit screen so closing returns to it.
        let Screen::Log(log) = std::mem::take(&mut self.screen) else {
            return;
        };
        let rev = entry.hash.clone();
        self.screen = Screen::Commit {
            view: CommitView {
                rev: rev.clone(),
                short: SharedString::from(entry.short_hash.clone()),
                subject: SharedString::from(entry.subject.clone()),
                rows: vec![CommitDiffRow::Note("Loading…".to_string())],
                scroll: UniformListScrollHandle::new(),
                selected: 0,
                visual: None,
            },
            log,
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
                if this.screen_gen != gen || this.commit_view().is_none() {
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
        // Return to the log the commit view was opened from.
        if let Screen::Commit { log, .. } = std::mem::take(&mut self.screen) {
            self.screen = Screen::Log(log);
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

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // While the editor is open the focused Input handles keys; commit/cancel
        // are caught in the capture phase (on_capture_key).
        if self.editor().is_some() {
            return;
        }

        let key = event.keystroke.key.to_lowercase();
        let shift = event.keystroke.modifiers.shift;
        let mut ctrl = event.keystroke.modifiers.control;
        let alt = event.keystroke.modifiers.alt;
        let cmd = event.keystroke.modifiers.platform;

        // C-g is the universal cancel (= Escape) everywhere — Emacs
        // keyboard-quit. Other Emacs motions (`C-n`/`C-p`, `C-x C-c`, …) are now
        // ordinary keymap entries (see DEFAULT_BINDINGS), not normalized here.
        let key = match key.as_str() {
            "g" if ctrl => {
                ctrl = false;
                "escape".to_string()
            }
            _ => key,
        };

        // A sequence is pending: this key continues it. Resolve here — before the
        // per-view branches — so sequences (including `C-x C-c`) work everywhere.
        if self.pending_prefix.is_some() {
            let next = chord(&key, shift, ctrl, alt, cmd);
            self.advance_prefix(&next, window, cx);
            return;
        }

        // While settings is open the focused Select handles keys; we only watch
        // for Esc (when no dropdown menu is open) to close the screen. Tab is
        // delivered via the ToggleFold action.
        if self.settings().is_some() {
            if key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }

        // The git command-log view takes over the window; esc/q/$ close it, and
        // it scrolls with the usual vi/less keys.
        if self.git_log().is_some() {
            if key == "escape" || key == "q" || key == "$" || (key == "4" && shift) {
                self.close_git_log(window, cx);
                return;
            }
            // `a` toggles showing the UI's own read-only queries.
            if key == "a" {
                self.toggle_git_log_all(window, cx);
                return;
            }
            let page = page_rows(window);
            let len = self.git_log_rows().len();
            // The pager has no cursor, so it scrolls via less-style keys rather
            // than the shared `nav_*`; translate a remapped motion to the key
            // apply_scroll_key understands, so [keymap] still drives it.
            let cased = if shift {
                key.to_uppercase()
            } else {
                key.clone()
            };
            let (skey, sshift) = match self.keymap.get(&cased).map(String::as_str) {
                Some("move-down") => ("j", false),
                Some("move-up") => ("k", false),
                Some("goto-bottom") => ("g", true),
                Some("goto-top") => ("g", false),
                _ => (key.as_str(), shift),
            };
            if let Some(sv) = self.git_log_mut() {
                apply_scroll_key(&sv.scroll, &mut sv.top, len, skey, sshift, ctrl, page);
            }
            cx.notify();
            return;
        }

        // A commit's diff detail (opened from the log) is topmost; esc/q returns
        // to the log, and it scrolls with the usual vi/less keys.
        // The interactive-rebase todo editor: set an action, reorder, then start.
        if self.rebase_todo().is_some() {
            // While the "discard edits?" confirmation is up, capture y / n / esc.
            if self.rebase_todo().is_some_and(|rt| rt.confirming_cancel) {
                match key.as_str() {
                    "y" => self.discard_rebase_todo(window, cx),
                    "n" | "escape" => self.keep_editing_rebase_todo(window, cx),
                    _ => {}
                }
                return;
            }
            if self.try_nav(&key, shift, ctrl, window, cx) {
                return;
            }
            match key.as_str() {
                "escape" | "q" => self.close_rebase_todo(window, cx),
                "enter" => self.run_rebase_todo(window, cx),
                // Move the selected commit up/down (shift+k / shift+j).
                "k" if shift => self.rebase_todo_reorder(-1, cx),
                "j" if shift => self.rebase_todo_reorder(1, cx),
                // Set the action of the commit at point.
                "p" => self.rebase_todo_set_action(RebaseAction::Pick, cx),
                "e" => self.rebase_todo_set_action(RebaseAction::Edit, cx),
                "s" => self.rebase_todo_set_action(RebaseAction::Squash, cx),
                "f" => self.rebase_todo_set_action(RebaseAction::Fixup, cx),
                "d" | "x" => self.rebase_todo_set_action(RebaseAction::Drop, cx),
                _ => {}
            }
            return;
        }

        if self.commit_view().is_some() {
            if self.try_nav(&key, shift, ctrl, window, cx) {
                return;
            }
            match key.as_str() {
                // Cancel a visual selection first; otherwise leave the view.
                "escape" | "q" => {
                    if self.commit_view().is_some_and(|cv| cv.visual.is_some()) {
                        if let Some(cv) = self.commit_view_mut() {
                            cv.visual = None;
                        }
                        cx.notify();
                    } else {
                        self.close_commit_view(window, cx);
                    }
                }
                "v" => self.commit_view_toggle_visual(cx),
                "y" => self.copy_commit_selection(cx),
                "c" if cmd => self.copy_commit_selection(cx),
                _ => {}
            }
            return;
        }

        // The commit-log view: Enter opens the commit; esc/q close; motions move
        // the selection (shared with every cursor view via `try_nav`).
        if self.log().is_some() {
            // In a select mode, Return confirms the commit for the pending
            // action; while browsing it opens the commit's diff.
            let select_args = match self.log().map(|l| &l.purpose) {
                Some(LogPurpose::SelectRebaseBase { args }) => Some(args.clone()),
                _ => None,
            };
            if self.try_nav(&key, shift, ctrl, window, cx) {
                return;
            }
            match key.as_str() {
                "escape" | "q" => self.close_log(window, cx),
                // Cmd+Return confirms the pending select (rebase since); plain
                // Return opens the commit's diff — in select mode too, so you can
                // inspect commits before choosing (magit lets you visit from the
                // log-select).
                "enter" if cmd => {
                    if let Some(args) = select_args.clone() {
                        self.rebase_since_selected(args, cx);
                    }
                }
                "enter" => self.open_commit_view(cx),
                // Apply the selected commit to the current branch (magit's `A`),
                // or revert it (`V`). Both return to the status view, where a
                // conflict surfaces as the in-progress banner.
                "a" if shift => self.pick_selected(PickOp::CherryPick, window, cx),
                // Revert is `_` (evil-collection-magit); `V` is visual-line there.
                "_" => self.pick_selected(PickOp::Revert, window, cx),
                "-" if shift => self.pick_selected(PickOp::Revert, window, cx),
                // `r`: rebase interactively since the commit at point (magit's
                // commit-at-point path) — only while browsing, with default args.
                "r" if select_args.is_none() => self.rebase_since_selected(Vec::new(), cx),
                // Yank the selected commit's hash.
                "y" => self.copy_log_commit(cx),
                "c" if cmd => self.copy_log_commit(cx),
                _ => {}
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

        // The vertico picker's focused input handles text; navigation, confirm
        // and cancel are caught in the capture phase (on_capture_key). Ignore the
        // rest here so typed characters aren't read as commands.
        if matches!(self.popup, Some(Popup::Picker(_))) {
            return;
        }

        // The `?` dispatch popup is modal (like magit's dispatch): a command
        // key runs that command, esc/q/? close it, other keys are ignored.
        if matches!(self.popup, Some(Popup::Dispatch(_))) {
            // (A pending prefix's second key was already resolved above.)
            match cased.as_str() {
                "escape" | "q" | "?" | "/" => {
                    self.popup = None;
                    cx.notify();
                }
                k if self.is_prefix(k) => self.enter_prefix(k.to_string(), window, cx),
                k if Self::is_dispatch_key(&self.keymap, k) => {
                    self.run_dispatch(&cased, window, cx)
                }
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

        // Command palette via cmd+p / cmd+k — before `try_nav`, so cmd+k isn't
        // read as the `k` motion.
        if cmd && matches!(key.as_str(), "p" | "k") {
            return self.open_command_palette(window, cx);
        }
        // Motions, paging, and the `g` prefix — remappable, applied screen-aware.
        if self.try_nav(&key, shift, ctrl, window, cx) {
            return;
        }
        match key.as_str() {
            // Tab toggles a fold (also delivered via the ToggleFold action, since
            // Root binds tab). Kept explicit — and out of the remappable keymap.
            "tab" => self.toggle_fold(cx),
            "escape" => {
                // A running job takes priority: C-g/Esc kills its subprocess.
                // Otherwise cancel a visual selection, else dismiss the
                // status/error banner if one is showing.
                if self.cancel_job(cx) {
                    return;
                }
                if self.visual.take().is_some() || self.status_message.take().is_some() {
                    cx.notify();
                }
                return;
            }
            // Modifier/symbol aliases that aren't plain registry keys, so they
            // can't ride the keymap below: Cmd-C yanks (before any `c` binding);
            // M-x / `:` / `;`+shift open the palette; `!`/`|` (and base-key+shift
            // fallbacks) run a git command; `?` / `/`+shift open Help.
            "c" if cmd => return self.invoke_command("yank", window, cx),
            "x" if alt => return self.open_command_palette(window, cx),
            "!" | "|" => return self.invoke_command("git-command", window, cx),
            "1" | "\\" if shift => return self.invoke_command("git-command", window, cx),
            "4" if shift => return self.invoke_command("git-log", window, cx),
            ":" => return self.open_command_palette(window, cx),
            ";" if shift => return self.open_command_palette(window, cx),
            "?" => {
                self.popup = Some(Popup::Dispatch(dispatch_menu(&self.keymap)));
                cx.notify();
                return;
            }
            "/" if shift => {
                self.popup = Some(Popup::Dispatch(dispatch_menu(&self.keymap)));
                cx.notify();
                return;
            }
            // Everything else resolves through the effective keymap (the
            // shift-cased keystroke → command id), so remap/unbind take effect.
            // The plain command keys (`c`, `s`/`S`, `O`, `F`, `enter`, `v`, …)
            // live there now, not as arms above — the single source of dispatch.
            _ => {
                if Self::is_dispatch_key(&self.keymap, &cased) {
                    return self.run_dispatch(&cased, window, cx);
                }
                // An unbound key: tell the user (emacs' "… is undefined"). Only
                // for plain/shifted keys — keys held with cmd/alt/ctrl are OS or
                // editor shortcuts we don't model in the keymap, so a "z is
                // unbound" toast for cmd-z would be misleading.
                if !cmd && !alt && !ctrl {
                    self.report_unbound(&cased, cx);
                }
                return;
            }
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

    /// Click on a value-reading option row: prompt for its value, stashing the
    /// transient to reopen after (mirrors pressing the option's `-X` key).
    fn click_option(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let opt = match &self.popup {
            Some(Popup::Transient(s)) => s
                .def
                .option_for(&key)
                .map(|o| (o.key.to_string(), o.description.to_string(), o.completion)),
            _ => None,
        };
        if let Some((k, desc, comp)) = opt {
            if let Some(Popup::Transient(ts)) = self.popup.take() {
                self.open_option_prompt(k, desc, comp, ts, window, cx);
            }
        }
    }

    /// Invoke a `?`-dispatch command (by key press or row click): close the
    /// dispatch menu and run the command, like magit's dispatch transient.
    fn run_dispatch(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        // A keymap-bound command (default or user-remapped), the `:` palette, or
        // a motion. Resolving through the effective keymap is what makes
        // remap/unbind take effect — and binding *any* command id (even a leaf
        // like `branch.delete`) to a key Just Works via `invoke_command`.
        if let Some(id) = self.keymap.get(key).cloned() {
            // Motions resolve here too (registry Navigation commands), applied
            // screen-aware by their `run`.
            self.invoke_command(&id, window, cx);
        } else if key == ":" {
            self.open_command_palette(window, cx);
        }
    }

    /// Invoke a registry [`Command`] by id — the keymap's bridge to the
    /// registry, so the command's behavior lives in exactly one place.
    fn invoke_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cmd) = commands().iter().find(|c| c.id == id) {
            (cmd.run)(self, window, cx);
        } else if let Some(custom) = self.config.commands.iter().find(|c| c.id == id).cloned() {
            self.run_custom_command(custom, window, cx);
        }
    }

    /// Run a user `[[command]]`: substitute its placeholders against the current
    /// selection, confirm if it looks destructive, then run it as a shell command
    /// on the background path.
    fn run_custom_command(
        &mut self,
        cmd: config::CustomCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = match self.expand_placeholders(&cmd.run) {
            Ok(c) => c,
            Err(e) => return self.set_status(e, false, cx),
        };
        if command.trim().is_empty() {
            return;
        }
        if command_is_destructive(&command) {
            self.confirm = Some((
                format!("Run `{command}`? (y/n)"),
                Confirm::CustomShell {
                    command,
                    refresh: cmd.refresh,
                },
            ));
            cx.notify();
        } else {
            self.run_custom_shell(command, cmd.refresh, cx);
        }
    }

    /// Substitute `{file}`/`{commit}`/`{branch}` in the command against the
    /// current selection, each shell-quoted so a path with spaces stays one word.
    /// `Err` (with why) if a placeholder can't be resolved — e.g. `{file}` with
    /// no file at point.
    fn expand_placeholders(&self, command: &str) -> Result<String, String> {
        let mut s = command.to_string();
        if s.contains("{file}") {
            let path = self
                .path_at_point()
                .ok_or_else(|| "No file at point for {file}".to_string())?;
            s = s.replace("{file}", &shell_words::quote(&path));
        }
        if s.contains("{branch}") {
            let branch = self
                .status
                .as_ref()
                .and_then(|st| st.head.branch.clone())
                .ok_or_else(|| "No current branch for {branch}".to_string())?;
            s = s.replace("{branch}", &shell_words::quote(&branch));
        }
        if s.contains("{commit}") {
            let hash = self
                .log()
                .and_then(|l| l.entries.get(l.selected))
                .map(|e| e.hash.clone())
                .ok_or_else(|| "No commit at point for {commit}".to_string())?;
            s = s.replace("{commit}", &shell_words::quote(&hash));
        }
        Ok(s)
    }

    /// The repo-relative path of the file at point (its row, or the file a
    /// hunk/line belongs to), if any.
    fn path_at_point(&self) -> Option<String> {
        match self.rows.get(self.selected)?.target.as_ref()? {
            Target::File(f) => Some(f.path.clone()),
            Target::Hunk { file, .. } | Target::Line { file, .. } => Some(file.path.clone()),
        }
    }

    /// Run a resolved custom command as one background job (`sh -c`), showing the
    /// first output line (or opening the `$` log for multi-line output) and
    /// refreshing unless opted out.
    fn run_custom_shell(&mut self, command: String, refresh: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        self.set_progress(format!("{command}…"), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.run_shell(&command) })
                .await;
            this.update(cx, |this, cx| {
                this.job_cancel = None;
                match result {
                    Ok(run) => {
                        let summary = run
                            .stdout
                            .lines()
                            .chain(run.stderr.lines())
                            .map(str::trim)
                            .find(|l| !l.is_empty())
                            .unwrap_or(if run.ok { "done" } else { "command failed" })
                            .to_string();
                        // A failure stays up (sticky); success fades.
                        this.set_status(summary, run.ok, cx);
                    }
                    Err(e) => this.report_error(e, cx),
                }
                if refresh {
                    this.refresh(cx);
                }
                // Show the whole thing when it spans more than a line.
                if this.last_user_output_lines() > 1 {
                    this.open_git_log(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Classify a keystroke sequence against the effective keymap: a complete
    /// binding, a prefix of one or more longer bindings, or neither.
    fn classify_seq(&self, seq: &str) -> KeyMatch {
        if let Some(id) = self.keymap.get(seq) {
            return KeyMatch::Command(id.clone());
        }
        let lead = format!("{seq} ");
        if self.keymap.keys().any(|k| k.starts_with(&lead)) {
            return KeyMatch::Prefix;
        }
        KeyMatch::Unbound
    }

    /// Whether `key` begins a longer binding — a prefix the next keystroke
    /// continues (it may also be a complete binding on its own; this only asks
    /// whether *more* could follow).
    fn is_prefix(&self, key: &str) -> bool {
        matches!(self.classify_seq(key), KeyMatch::Prefix)
    }

    /// Begin (or extend) a sequence: remember the keys typed so far and show the
    /// lightweight bottom strip. The sequence then waits indefinitely for the
    /// next key; after `which_key_delay_ms` the strip expands into the which-key
    /// list of continuations.
    fn enter_prefix(&mut self, seq: String, window: &mut Window, cx: &mut Context<Self>) {
        self.prefix_gen = self.prefix_gen.wrapping_add(1);
        let gen = self.prefix_gen;
        self.pending_prefix = Some(PendingPrefix {
            seq,
            gen,
            which_key: false,
        });
        cx.notify();
        let delay = Duration::from_millis(self.config.which_key_delay_ms);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |this, _window, cx| {
                // Reveal the which-key list only if this exact sequence is still
                // waiting (a newer prefix or a resolved sequence bumps/clears it).
                let Some(p) = this.pending_prefix.as_mut() else {
                    return;
                };
                if p.gen != gen || p.which_key {
                    return;
                }
                p.which_key = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Feed the next key into the pending sequence. Appends it and re-classifies:
    /// a complete binding runs (closing any dispatch popup), a deeper prefix
    /// keeps waiting, and an unbound sequence reports "… is unbound".
    fn advance_prefix(&mut self, next: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pending_prefix.take() else {
            return;
        };
        // Esc / C-g (normalized to "escape") aborts the sequence silently — it's
        // keyboard-quit, not an attempt at a binding, so no "unbound" notice.
        if next == "escape" {
            cx.notify();
            return;
        }
        let seq = format!("{} {next}", p.seq);
        match self.classify_seq(&seq) {
            KeyMatch::Command(id) => {
                self.popup = None;
                self.invoke_command(&id, window, cx);
            }
            KeyMatch::Prefix => self.enter_prefix(seq, window, cx),
            KeyMatch::Unbound => self.report_unbound(&seq, cx),
        }
        cx.notify();
    }

    /// Note that a keystroke sequence isn't bound (magit/emacs' "… is undefined"
    /// echo-area feedback), as a fading notice with the keys shown as keycaps.
    fn report_unbound(&mut self, seq: &str, cx: &mut Context<Self>) {
        self.set_status("is unbound".to_string(), true, cx);
        self.status_keys = Some(seq.to_string());
    }

    /// Note a command run *from the palette* for its frecency ranking, and
    /// persist it. Only palette runs count: a command you already invoke by key
    /// doesn't need surfacing at the top of the palette.
    fn record_use(&mut self, id: &str) {
        self.usage.record(id);
        config::save_usage(&self.usage);
    }

    /// Open the `:` command palette: the vertico picker over the (enabled)
    /// registry commands, matched by title. Enter runs the chosen command.
    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Order by frecency (most-used-recently first); a stable sort keeps the
        // registry order among never-used commands and ties. The picker's fuzzy
        // ranking takes over once the user types, with this order breaking ties.
        let mut entries: Vec<(&str, String)> = commands()
            .iter()
            .filter(|c| c.palette && (c.enabled)(self))
            .map(|c| (c.id, c.title.to_string()))
            .chain(
                // User `[[command]]`s — always palette-able.
                self.config
                    .commands
                    .iter()
                    .map(|c| (c.id.as_str(), c.title.clone())),
            )
            .collect();
        entries.sort_by(|a, b| {
            let (sa, sb) = (self.usage.score(a.0), self.usage.score(b.0));
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let choices: Vec<String> = entries.into_iter().map(|(_, title)| title).collect();
        self.open_picker(
            PickerAction::RunCommand,
            choices,
            CreateMode::None,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Whether `key` is a single-stroke dispatch key: bound in the effective
    /// keymap (a command), or one of the bare motions `j`/`k`/`G` and the `:`
    /// palette. Multi-stroke entries are handled elsewhere — Tab via the
    /// ToggleFold action, `g r`/`g g`/`g j`/`g k` via the g-prefix — so they're
    /// excluded even if a key like `g r` is bound.
    fn is_dispatch_key(keymap: &HashMap<String, String>, key: &str) -> bool {
        if matches!(key, "tab" | "g r" | "g g" | "g j" | "g k") {
            return false;
        }
        // Single-key motions (`j`/`k`/`G`) are registry commands in the keymap now.
        keymap.contains_key(key) || key == ":"
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

    /// The remote-picker overlay: a title and kbd hints over a searchable list
    /// of remotes (search field focused on appear). Enter / clicking a row runs
    /// the transfer; the "return" kbd button does the same.
    fn render_picker(&self, state: &PickerState, view: &Entity<Self>) -> gpui::Div {
        let confirm_label = state.action.confirm_label();

        // Reserve a fixed screenful for the candidate area, so the
        // bottom-anchored panel never resizes — neither while filtering (which
        // only shrinks the matches) nor when async candidates load. A pure
        // value-entry prompt has no candidates and collapses instead.
        const MAX_VISIBLE: usize = 8;
        let rows = state.list.row_count();
        let list_height = px(MAX_VISIBLE as f32 * ROW_HEIGHT);

        let body = if !state.reserve_candidates {
            // Value entry has nothing to match — collapse the candidate area
            // entirely so the hints sit right under the input.
            div().into_any_element()
        } else if rows == 0 {
            // No rows: either candidates are still loading off the UI thread, or
            // they're loaded and none match the query. A quiet line in the first
            // row keeps the reserved height so nothing shifts.
            let note = if state.loading {
                "Loading…"
            } else {
                "No match"
            };
            div()
                .h(list_height)
                .child(
                    div()
                        .h(px(ROW_HEIGHT))
                        .pl(px(ROW_PAD_LEFT))
                        .flex()
                        .items_center()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(note)),
                )
                .into_any_element()
        } else {
            uniform_list("picker-rows", rows, {
                let view = view.clone();
                move |range, _window, cx| match &view.read(cx).popup {
                    Some(Popup::Picker(p)) => {
                        // In the command palette, show each command's keybinding
                        // (when it has one) on the right, so it doubles as help.
                        let palette = matches!(p.action, PickerAction::RunCommand);
                        range
                            .map(|ix| match p.list.row(ix) {
                                Some(r) => {
                                    let hint = palette
                                        .then(|| command_keys(&view.read(cx).keymap, &r.label))
                                        .flatten()
                                        .map(SharedString::from);
                                    view.read(cx).render_picker_row(
                                        ix,
                                        r.label,
                                        r.is_create,
                                        ix == p.list.selected(),
                                        hint,
                                        &view,
                                    )
                                }
                                None => div().h(px(ROW_HEIGHT)).into_any_element(),
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                }
            })
            .track_scroll(&state.scroll)
            .h(list_height)
            .w_full()
            .into_any_element()
        };

        div()
            .w_full()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .py_2()
            .px_3()
            .flex()
            .flex_col()
            .gap_1()
            // Prompt with the query typed inline (vertico minibuffer).
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(ROW_PAD_LEFT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(self.render_title(&state.prompt, self.palette.section))
                            .child(
                                div()
                                    .text_color(self.palette.section)
                                    .child(SharedString::from(":")),
                            ),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .child(Input::new(&state.input).appearance(false)),
                    ),
            )
            .child(body)
            // Keyboard hints, consistent with the transient menus.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_1()
                    .pl(px(ROW_PAD_LEFT))
                    .child(self.key_action(
                        "remote-confirm",
                        "return",
                        confirm_label,
                        view,
                        Self::confirm_picker,
                    ))
                    .child(self.key_action(
                        "remote-picker-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::cancel_popup,
                    )),
            )
    }

    /// One candidate row: a full-width highlight when current (vertico-style, no
    /// boxy border), a subtle hover for the mouse, and click-to-confirm.
    fn render_picker_row(
        &self,
        ix: usize,
        label: SharedString,
        is_create: bool,
        selected: bool,
        hint: Option<SharedString>,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut el = div()
            .id(SharedString::from(format!("picker-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(ROW_PAD_LEFT))
            .cursor_pointer()
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                        p.list.set_selected(ix);
                    }
                    this.confirm_picker(window, vcx);
                });
            });
        if selected {
            el = el.bg(self.palette.selection);
        } else {
            // The picker sits on the elevated panel, where the neutral
            // `list.hover.background` can equal the panel itself (e.g. Selenized
            // White) and vanish. The translucent accent (also used for the
            // transient menu's hover) stays visible on any surface, and reads
            // distinctly from the neutral keyboard-selected row.
            el = el.hover(|s| s.bg(self.palette.visual));
        }
        let label_el = if is_create {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_color(self.palette.fg).child(label))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("(new)")),
                )
        } else {
            div().text_color(self.palette.fg).child(label)
        };
        el = el.child(label_el);
        // The command's binding (palette only) as subtle text right after the
        // name: a single key for top-level commands, or the full prefix→suffix
        // sequence for leaves (e.g. `c c` for "Create commit"). Plain text keeps
        // the rows at their normal height (a keycap would be too tall here).
        if let Some(seq) = hint {
            el = el.child(
                div()
                    .ml_1()
                    .text_color(self.palette.dim)
                    // Keys are monospace, like keycaps elsewhere, even under a
                    // proportional UI font.
                    .font_family(self.font.clone())
                    .child(SharedString::from(kbd::format_keys(&seq))),
            );
        }
        el.into_any_element()
    }

    /// Close the open picker. If it was prompting for a transient option value,
    /// reopen that transient unchanged rather than dismissing everything.
    fn cancel_popup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Popup::Picker(p)) = self.popup.take() {
            if let Some(ts) = p.resume {
                self.popup = Some(Popup::Transient(*ts));
            }
        }
        cx.notify();
    }

    fn render_transient(
        &self,
        def: &Transient,
        state: Option<&TransientState>,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let pending_dash = state.is_some_and(|s| s.pending_dash);

        // Magit's layout, derived from content rather than hand-authored: an
        // *argument* group (switches/options) is a full-width band, and bands
        // stack vertically; the *command* groups (actions/`?`-menu info) sit
        // side by side in a wrapping row beneath them. A tall argument band fans
        // its rows into sub-columns (capped ~BAND_CAP each) so it stays compact.
        // This reproduces magit's commit transient (Arguments band over a row of
        // Create/Edit/… columns), the log transient (Arguments band over the Log
        // command row), and the `?` dispatch (all command groups → one packed
        // row), without a per-transient layout spec.
        const BAND_CAP: usize = 7;
        let has_args = |g: &&Group| {
            g.suffixes
                .iter()
                .any(|s| matches!(s, Suffix::Switch(_) | Suffix::Option(_)))
        };

        let mut body = div().flex().flex_col().items_start().gap_3();
        for group in def.groups.iter().filter(has_args) {
            let k = group.suffixes.len().div_ceil(BAND_CAP).max(1);
            body = body.child(self.render_group(group, k, state, pending_dash, view));
        }
        let mut command_row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_x_8()
            .gap_y_3();
        let mut any_command = false;
        for group in def.groups.iter().filter(|g| !has_args(g)) {
            any_command = true;
            // A tall command group (e.g. the `?` dispatch's "Commands") fans into
            // sub-columns just like an argument band, so it doesn't tower over
            // the shorter groups beside it.
            let k = group.suffixes.len().div_ceil(BAND_CAP).max(1);
            command_row = command_row.child(self.render_group(group, k, state, pending_dash, view));
        }
        if any_command {
            body = body.child(command_row);
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
            .child(self.render_title(&def.title, self.palette.section))
            .child(body)
    }

    /// One transient group as a left-aligned band: its dim title above its
    /// suffix rows (switches, value options, actions, or `?`-menu info). A tall
    /// group spreads its rows across `subcols` sub-columns *within the band*
    /// (magit's `[[col][col]]`) so it doesn't dominate the panel height — e.g.
    /// the log transient's 8 arguments become two columns of four under one
    /// "Arguments" heading. `items_start` so each row's clickable hitbox hugs
    /// its content width (else clicks land on the wrong row).
    fn render_group(
        &self,
        group: &Group,
        subcols: usize,
        state: Option<&TransientState>,
        pending_dash: bool,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let n = group.suffixes.len();
        let k = subcols.clamp(1, n.max(1));
        let per = n.div_ceil(k).max(1);
        let mut buckets: Vec<Vec<AnyElement>> = (0..k).map(|_| Vec::new()).collect();
        for (i, suffix) in group.suffixes.iter().enumerate() {
            let bucket = (i / per).min(k - 1);
            buckets[bucket].push(self.render_suffix(suffix, state, pending_dash, view));
        }
        let mut row = div().flex().flex_row().items_start().gap_x_6();
        for bucket in buckets {
            let mut sc = div().flex().flex_col().items_start().gap_1();
            for el in bucket {
                sc = sc.child(el);
            }
            row = row.child(sc);
        }
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_1()
            .child(self.render_title(&group.title, self.palette.dim))
            .child(row)
    }

    /// One transient suffix as a clickable row (switch, value option, action,
    /// or `?`-menu info).
    fn render_suffix(
        &self,
        suffix: &Suffix,
        state: Option<&TransientState>,
        pending_dash: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        match suffix {
            Suffix::Switch(sw) => {
                let on = state.is_some_and(|s| s.active.contains(sw.key));
                // magit layout: key, description, then the literal git flag
                // in parens. Only the flag itself dims (off) or highlights
                // bold in the `modified` accent (on) — the parens stay a
                // constant neutral color.
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
                    .child(kbd::switch_chip(
                        sw.key,
                        self.palette.dim,
                        self.palette.removed,
                        pending_dash,
                        &self.font,
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
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), true, window, vcx));
                    })
                    .into_any_element()
            }
            // A value-reading option: like a switch, but the parens show the
            // current value (or the bare flag when unset). The parens are
            // omitted when there'd be nothing in them (an option whose value
            // *is* the flag, e.g. commit order, when unset).
            Suffix::Option(o) => {
                let value = state.and_then(|s| s.values.get(o.key).cloned());
                let set = value.is_some();
                let inner = format!("{}{}", o.arg, value.as_deref().unwrap_or_default());
                let color = if set {
                    self.palette.modified
                } else {
                    self.palette.dim
                };
                let view = view.clone();
                let okey = o.key.to_string();
                div()
                    .id(o.key)
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(o.key))
                    .child(kbd::switch_chip(
                        o.key,
                        self.palette.dim,
                        self.palette.removed,
                        pending_dash,
                        &self.font,
                    ))
                    .child(self.hover_label(o.description, self.palette.fg))
                    .when(!inner.is_empty(), |row| {
                        row.child(
                            div()
                                .text_color(color)
                                .child(SharedString::from(format!("({inner})"))),
                        )
                    })
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_option(okey.clone(), window, vcx));
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
                    .child(kbd::key_chip(a.key, self.palette.dim, &self.font))
                    .child(self.hover_label(&a.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
            // A dispatch command row: keycap + label, clickable to run.
            Suffix::Info(i) => {
                let view = view.clone();
                let key = SharedString::from(i.keys.clone());
                div()
                    .id(key.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(key.clone()))
                    .child(self.key_tokens(&i.keys))
                    .child(self.hover_label(i.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.run_dispatch(&key, window, vcx));
                    })
                    .into_any_element()
            }
            // A user-injected suffix (from `[transient]`): keycap + label,
            // clickable; dispatched by key like an action.
            Suffix::Custom(c) => {
                let view = view.clone();
                let key = SharedString::from(c.key.clone());
                div()
                    .id(key.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(key.clone()))
                    .child(kbd::key_chip(&c.key, self.palette.dim, &self.font))
                    .child(self.hover_label(&c.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
        }
    }

    /// Render a dialog heading from styled spans, with branch/ref names set off
    /// from the surrounding words as a subtly tinted, medium-weight chip so
    /// they're easy to pick out — e.g. the `main` in "Push main to". `base` is
    /// the color for the plain text (the heading vs. group-header convention).
    fn render_title(&self, spans: &[TitleSpan], base: Hsla) -> gpui::Div {
        let mut row = div().flex().items_center();
        for span in spans {
            row = match span {
                TitleSpan::Text(t) => {
                    row.child(div().text_color(base).child(SharedString::from(t.clone())))
                }
                TitleSpan::Branch(b) => row.child(self.branch_chip(b)),
            };
        }
        row
    }

    /// A branch/ref name as a subtly tinted, medium-weight chip — set off from
    /// surrounding text. Used in dialog titles and the repo header lines.
    fn branch_chip(&self, name: &str) -> gpui::Div {
        div()
            .px(px(5.0))
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            // Branch/ref names are identifiers — keep them monospace even when
            // the surrounding chrome uses a proportional UI font.
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .child(SharedString::from(name.to_string()))
    }

    /// A small copy-to-clipboard icon button: copies `text` and flashes the
    /// "Copied" confirmation; `tooltip` names what it copies.
    fn copy_icon_button(
        &self,
        view: &Entity<Self>,
        id: &'static str,
        text: String,
        tooltip: &'static str,
    ) -> impl IntoElement {
        let view = view.clone();
        let tip_font = self.font.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .cursor_pointer()
            .px(px(4.0))
            .child(track_target(id))
            .child(
                Icon::new(IconName::Copy)
                    .xsmall()
                    .text_color(self.palette.fg),
            )
            .tooltip(move |window, cx| {
                let font = tip_font.clone();
                Tooltip::element(move |_, _| div().font_family(font.clone()).child(tooltip))
                    .build(window, cx)
            })
            .tooltip_show_delay(Duration::ZERO)
            .on_click(move |_, _window, cx: &mut App| {
                let text = text.clone();
                view.update(cx, |v, vcx| v.copy_to_clipboard(text, vcx));
            })
    }

    /// The title-bar branch as a divided pill sharing one highlight: the name
    /// (click opens the branch transient) and a copy-name button.
    fn render_branch_chip(&self, view: &Entity<Self>, branch: &str) -> gpui::Div {
        let branch_click = view.clone();
        div()
            .flex()
            .items_center()
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .child(
                div()
                    .id("titlebar-branch")
                    .relative()
                    .cursor_pointer()
                    .px(px(5.0))
                    .child(track_target("titlebar-branch"))
                    .child(SharedString::from(branch.to_string()))
                    .on_click(move |_, window, cx: &mut App| {
                        branch_click.update(cx, |v, vcx| v.invoke_command("branch", window, vcx));
                    }),
            )
            // Divider between the two halves of the split chip.
            .child(div().w(px(1.0)).h(px(12.0)).bg(self.palette.dim))
            .child(self.copy_icon_button(
                view,
                "titlebar-branch-copy",
                branch.to_string(),
                "Copy branch name",
            ))
    }

    /// The in-progress sequence banner (merge/rebase/cherry-pick/revert/am):
    /// a heading, the plan steps, and the available continue/skip/abort
    /// controls. Sits above the status list so it's visible while resolving.
    fn render_sequence_banner(&self, seq: &Sequence, view: &Entity<Self>) -> gpui::Div {
        // The plan steps (capped so a long rebase todo can't dominate).
        const MAX_STEPS: usize = 8;
        let mut steps = div().flex().flex_col().gap_0().pl(px(2.0));
        for step in seq.steps.iter().take(MAX_STEPS) {
            let mut line = format!("{} ", step.action);
            if let Some(oid) = &step.oid {
                line.push_str(oid);
                line.push(' ');
            }
            line.push_str(&step.subject);
            steps = steps.child(
                div()
                    .text_color(self.palette.dim)
                    .font_family(self.font.clone())
                    .child(SharedString::from(line)),
            );
        }
        if seq.steps.len() > MAX_STEPS {
            steps = steps.child(div().text_color(self.palette.dim).child(SharedString::from(
                format!("… +{} more", seq.steps.len() - MAX_STEPS),
            )));
        }

        // Continue / skip / abort as keycap+label buttons. The keycap shows the
        // *full* keystroke that drives it from the status view — the prefix that
        // opens this sequence's transient plus the action key (so rebase continue
        // is `r r`, not a bare `r`, which would collide with "open rebase"). Only
        // rebase/merge have a status-view prefix; cherry-pick/revert/am are driven
        // only by clicking these buttons, so they show no (misleading) keycap.
        let prefix = match seq.kind {
            SequenceKind::Rebase => Some("r"),
            SequenceKind::Merge => Some("m"),
            SequenceKind::CherryPick | SequenceKind::Revert | SequenceKind::Am => None,
        };
        let keys = |action_key: &str| prefix.map(|p| format!("{p} {action_key}"));
        let mut actions = div().flex().items_center().gap_3();
        if seq.kind.can_continue() {
            actions = actions.child(self.seq_action(
                "seq-continue",
                keys("r"),
                "continue",
                view,
                Self::sequence_continue,
            ));
        }
        if seq.kind.can_skip() {
            actions = actions.child(self.seq_action(
                "seq-skip",
                keys("s"),
                "skip",
                view,
                Self::sequence_skip,
            ));
        }
        actions = actions.child(self.seq_action(
            "seq-abort",
            keys("a"),
            "abort",
            view,
            Self::sequence_abort,
        ));

        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .px_3()
            .py_2()
            .bg(self.palette.banner)
            .border_b_1()
            .border_color(self.palette.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.palette.section)
                    .child(SharedString::from(seq.heading.clone())),
            )
            .child(steps)
            .child(actions)
    }

    /// A sequence-banner action button: keycap + label, clickable to run
    /// `action`. `keys` is the full keystroke that triggers it from the status
    /// view (e.g. `r r`); when `None` (a sequence with no status-view prefix)
    /// the button is click-only, with no misleading keycap.
    fn seq_action(
        &self,
        id: &'static str,
        keys: Option<String>,
        label: &'static str,
        view: &Entity<Self>,
        action: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let view = view.clone();
        let mut row = div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id));
        if let Some(keys) = keys {
            row = row.child(kbd::key_chip(&keys, self.palette.dim, &self.font));
        }
        row.child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// A dim tracking entry for the title bar: an optional direction glyph
    /// (`⇡` push / `⇣` pull), the ref name, and `↑ahead`/`↓behind` (each shown
    /// only when non-zero). The ahead/behind are clickable: `↑` opens the push
    /// transient, `↓` the pull transient. `key` namespaces their element ids.
    fn track_chunk(
        &self,
        view: &Entity<Self>,
        key: &str,
        glyph: &str,
        name: &str,
        ahead: i64,
        behind: i64,
    ) -> gpui::Div {
        let mut chunk = div()
            .flex()
            .items_center()
            .gap_1()
            .text_color(self.palette.dim)
            .font_family(self.font.clone())
            .child(SharedString::from(format!("{glyph}{name}")));
        if ahead > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-ahead"),
                "push",
                SharedString::from(format!("↑{ahead}")),
            ));
        }
        if behind > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-behind"),
                "pull",
                SharedString::from(format!("↓{behind}")),
            ));
        }
        chunk
    }

    /// A clickable title-bar element that runs the registry command `command`
    /// (the branch chip → "branch", an ahead count → "push", a behind count →
    /// "pull"). Brightens on hover to signal it's actionable.
    fn titlebar_action(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        command: &'static str,
        child: impl IntoElement,
    ) -> impl IntoElement {
        let view = view.clone();
        let fg = self.palette.fg;
        let id = id.into();
        div()
            .id(id.clone())
            .relative()
            .cursor_pointer()
            .hover(move |s| s.text_color(fg))
            .child(track_target(id))
            .child(child)
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| v.invoke_command(command, window, vcx));
            })
    }

    /// The custom window title bar: the repo name, the current branch as a chip,
    /// its ahead/behind vs upstream, and a dirty marker — styled to match the
    /// app (so it reads as chrome, not the OS bar). The `TitleBar` component
    /// handles traffic-light spacing, dragging, and (off-macOS) window controls.
    fn render_title_bar(&self, view: &Entity<Self>) -> impl IntoElement {
        let repo_name = self
            .repo
            .as_ref()
            .map(|r| r.workdir())
            .unwrap_or(self.root.as_path())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".to_string());

        let mut info = div().flex().items_center().gap_2().child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(SharedString::from(repo_name)),
        );

        if let Some(status) = &self.status {
            let head = &status.head;
            // A real branch: a divided chip (name opens the branch transient,
            // the button copies the name). Detached: a plain clickable chip.
            info = info.child(match &head.branch {
                Some(branch) => self.render_branch_chip(view, branch).into_any_element(),
                None => self
                    .titlebar_action(
                        view,
                        "titlebar-branch",
                        "branch",
                        self.branch_chip("detached"),
                    )
                    .into_any_element(),
            });

            // Tracking: the upstream, plus a distinct push target when present
            // (a triangular workflow). When the push target equals the upstream,
            // the core leaves `head.push` unset, so we show a single entry.
            match (&head.push, &head.upstream) {
                (Some(push), upstream) => {
                    info = info.child(self.track_chunk(
                        view,
                        "push",
                        "⇡",
                        push,
                        head.push_ahead,
                        head.push_behind,
                    ));
                    if let Some(up) = upstream {
                        info = info.child(self.track_chunk(
                            view,
                            "up",
                            "⇣",
                            up,
                            head.ahead,
                            head.behind,
                        ));
                    }
                }
                (None, Some(up)) => {
                    info =
                        info.child(self.track_chunk(view, "up", "", up, head.ahead, head.behind));
                }
                (None, None) => {}
            }

            if !status.is_clean() {
                // Marks uncommitted changes in the working tree.
                info = info.child(div().text_color(self.palette.modified).child("○"));
            }
        }

        gpui_component::TitleBar::new()
            .bg(self.palette.bg)
            .border_color(self.palette.border)
            .child(info)
    }

    /// Render a key spec as a single keycap. A multi-keystroke sequence (e.g.
    /// `g r`) keeps its keys spaced *inside* the one cap (see [`format_keys`]).
    fn key_tokens(&self, keys: &str) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .child(kbd::key_chip(keys, self.palette.dim, &self.font))
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
            .child(kbd::key_chip(key, self.palette.dim, &self.font))
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
            // The message editor and diff preview are monospace (the 50/72
            // ruler depends on column alignment).
            .font_family(self.font.clone())
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
                    .map(|el| {
                        if ed.confirming_cancel {
                            // Unsaved edits: confirm before discarding the message.
                            el.child(
                                div()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from("Discard message?")),
                            )
                            .child(self.key_action(
                                "editor-discard-yes",
                                "y",
                                "discard",
                                view,
                                Self::discard_editor,
                            ))
                            .child(self.key_action(
                                "editor-discard-no",
                                "n",
                                "keep editing",
                                view,
                                Self::keep_editing,
                            ))
                        } else {
                            el.child(self.key_action(
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
                            ))
                        }
                    }),
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
                        match this.editor() {
                            Some(ed) => range
                                .map(|ix| this.render_commit_diff_row(&ed.diff[ix], false))
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

    fn render_commit_diff_row(&self, row: &CommitDiffRow, highlighted: bool) -> AnyElement {
        let base = div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .when(highlighted, |el| el.bg(self.palette.selection));
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

    /// Render the git command-log view (magit's `$` process buffer): a header
    /// and a scrollable list of the recent git invocations, newest at the
    /// bottom, each flagged with success/failure.
    fn render_git_log(&self, sv: &ScrollView, view: &Entity<Self>) -> gpui::Div {
        let count = self.git_log_rows().len();

        let body = if count == 0 {
            div()
                .text_color(self.palette.dim)
                .child(SharedString::from("No git commands have run yet."))
                .into_any_element()
        } else {
            uniform_list("git-log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    let rows = this.git_log_rows();
                    range
                        .filter_map(|ix| rows.get(ix).map(|r| this.render_git_log_row(r)))
                        .collect::<Vec<_>>()
                }
            })
            .track_scroll(&sv.scroll)
            .flex_grow(1.0)
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // Commands and their output are code — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Git command log")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.key_action(
                                "git-log-all",
                                "a",
                                if self.git_log_show_all {
                                    "hide queries"
                                } else {
                                    "show all"
                                },
                                view,
                                Self::toggle_git_log_all,
                            ))
                            .child(self.key_action(
                                "git-log-close",
                                "esc",
                                "close",
                                view,
                                Self::close_git_log,
                            )),
                    ),
            )
            .child(body)
    }

    /// The command log flattened into uniform rows: each invocation becomes a
    /// command row followed by its (dim, indented) stderr lines — git's
    /// progress/error narrative.
    fn git_log_rows(&self) -> Vec<GitLogRow> {
        let Some(repo) = self.repo.as_ref() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for c in repo.command_log() {
            // Hide the UI's own read-only queries unless asked to show all.
            if !self.git_log_show_all && c.is_query() {
                continue;
            }
            rows.push(GitLogRow::Command {
                prog: c.program.clone().unwrap_or_else(|| "git".to_string()),
                args: c.args.join(" "),
                ok: c.ok,
            });
            // Output, stdout then stderr. stdout is only stored for user `!`
            // commands (internal git calls leave it empty). Progress on stderr
            // often uses '\r' to overwrite; split on both so each update is its
            // own line, and drop the blanks.
            for stream in [&c.stdout, &c.stderr] {
                for line in stream.split(['\n', '\r']) {
                    if !line.trim().is_empty() {
                        rows.push(GitLogRow::Output(line.trim_end().to_string()));
                    }
                }
            }
        }
        rows
    }

    /// One row of the git command log: either a command (success/failure sigil,
    /// dim `git` prefix, arguments reddened on failure) or a dim, indented line
    /// of that command's stderr output.
    fn render_git_log_row(&self, row: &GitLogRow) -> AnyElement {
        match row {
            GitLogRow::Command { prog, args, ok } => {
                let (sigil, sigil_color) = if *ok {
                    ("✓", self.palette.added)
                } else {
                    ("✗", self.palette.removed)
                };
                let args_color = if *ok {
                    self.palette.fg
                } else {
                    self.palette.removed
                };
                div()
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_shrink_0()
                            .text_color(sigil_color)
                            .child(SharedString::from(sigil)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from(prog.clone())),
                            )
                            .child(
                                div()
                                    .text_color(args_color)
                                    .child(SharedString::from(args.clone())),
                            ),
                    )
                    .into_any_element()
            }
            GitLogRow::Output(line) => div()
                .h(px(ROW_HEIGHT))
                .w_full()
                .flex()
                .items_center()
                // Indent past the sigil gutter so output nests under its command.
                .pl(px(24.0))
                .text_color(self.palette.dim)
                .child(SharedString::from(line.clone()))
                .into_any_element(),
        }
    }

    /// Render the commit-log view (`l`): a header and a scrollable, navigable
    /// list of commits; the highlighted row opens on Enter or click.
    fn render_log(&self, log: &LogState, view: &Entity<Self>) -> gpui::Div {
        let count = log.entries.len();
        // Note when the listing is capped, rather than pretending it's complete.
        let capped = count >= Self::LOG_LIMIT;

        let note = |text: String, color: Hsla| {
            div()
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &log.load {
            LogLoad::Loading => note("Loading…".to_string(), self.palette.dim),
            LogLoad::Failed(e) => note(format!("log failed: {e}"), self.palette.dim),
            LogLoad::Loaded if count == 0 => note("No commits".to_string(), self.palette.dim),
            LogLoad::Loaded => uniform_list("log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    match this.log() {
                        Some(log) => range
                            .map(|ix| {
                                this.render_log_row(ix, &log.entries[ix], ix == log.selected, &view)
                            })
                            .collect::<Vec<_>>(),
                        None => Vec::new(),
                    }
                }
            })
            .track_scroll(&log.scroll)
            .flex_grow(1.0)
            .into_any_element(),
        };

        // In select mode the title becomes a prompt and Return confirms the
        // commit; while browsing it's just "Log".
        let selecting = matches!(log.purpose, LogPurpose::SelectRebaseBase { .. });
        let title = if selecting {
            "Select a commit to rebase since"
        } else {
            "Log"
        };
        let mut header = div().flex().items_center().gap_3().child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.palette.section)
                .child(SharedString::from(title)),
        );
        if capped {
            header = header.child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(format!("(first {})", Self::LOG_LIMIT))),
            );
        }
        if selecting {
            // Return inspects the commit; Cmd+Return picks it as the base.
            header = header.child(self.key_action(
                "log-select-view",
                "return",
                "view",
                view,
                Self::view_log_commit,
            ));
            header = header.child(self.key_action(
                "log-select-confirm",
                "cmd-enter",
                "select",
                view,
                Self::confirm_log_select,
            ));
        }
        let (close_key, close_label) = if selecting {
            ("esc", "cancel")
        } else {
            ("esc", "close")
        };
        header = header.child(self.key_action(
            "log-close",
            close_key,
            close_label,
            view,
            Self::close_log,
        ));

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // Commit rows are columnar (hash / subject / date) — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(header)
            .child(body)
    }

    /// One commit row: short hash, ref decorations, and subject; highlighted
    /// when current, clickable to open its diff.
    fn render_log_row(
        &self,
        ix: usize,
        entry: &magritte_core::LogEntry,
        selected: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut row = div()
            .id(SharedString::from(format!("log-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .cursor_pointer()
            .on_click(move |_, _window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(log) = this.log_mut() {
                        log.selected = ix;
                    }
                    this.open_commit_view(vcx);
                });
            });
        if selected {
            row = row.bg(self.palette.selection);
        } else {
            row = row.hover(|s| s.bg(self.palette.hover));
        }
        row = row.child(
            div()
                .flex_shrink_0()
                .text_color(self.palette.modified)
                .child(SharedString::from(entry.short_hash.clone())),
        );
        if !entry.refs.is_empty() {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.section)
                    .child(SharedString::from(format!("({})", entry.refs))),
            );
        }
        row.child(
            div()
                .text_color(self.palette.fg)
                .child(SharedString::from(entry.subject.clone())),
        )
        .child(div().flex_grow(1.0))
        .child(
            div()
                .flex_shrink_0()
                .text_color(self.palette.dim)
                .child(SharedString::from(entry.date.clone())),
        )
        .into_any_element()
    }

    /// Render a commit's diff detail (opened from the log): a header with the
    /// hash + subject, then the diff as the same rows the commit editor uses.
    fn render_commit_view(&self, cv: &CommitView, view: &Entity<Self>) -> gpui::Div {
        let count = cv.rows.len();
        let body = uniform_list("commit-view-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.commit_view() {
                    Some(cv) => {
                        let vis = cv.visual.map(|a| (a.min(cv.selected), a.max(cv.selected)));
                        range
                            .map(|ix| {
                                let highlighted = ix == cv.selected
                                    || vis.is_some_and(|(lo, hi)| ix >= lo && ix <= hi);
                                this.render_commit_diff_row(&cv.rows[ix], highlighted)
                            })
                            .collect::<Vec<_>>()
                    }
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&cv.scroll)
        .flex_grow(1.0);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // A commit's header + diff is code — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    // The hash and its copy button share one highlight as a
                    // divided pill, mirroring the title-bar branch chip.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .bg(self.palette.selection)
                            .text_color(self.palette.fg)
                            .font_weight(FontWeight::MEDIUM)
                            .child(div().px(px(5.0)).child(cv.short.clone()))
                            .child(div().w(px(1.0)).h(px(12.0)).bg(self.palette.dim))
                            .child(self.copy_icon_button(
                                view,
                                "commit-sha-copy",
                                cv.rev.clone(),
                                "Copy commit hash",
                            )),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.fg)
                            .child(cv.subject.clone()),
                    )
                    .child(self.key_action(
                        "commit-view-close",
                        "esc",
                        "back",
                        view,
                        Self::close_commit_view,
                    )),
            )
            .child(body)
    }

    /// The action keyword + its color for a rebase-todo row.
    fn rebase_action_style(&self, action: RebaseAction) -> (&'static str, Hsla) {
        match action {
            RebaseAction::Pick => ("pick", self.palette.fg),
            RebaseAction::Reword => ("reword", self.palette.modified),
            RebaseAction::Edit => ("edit", self.palette.modified),
            RebaseAction::Squash => ("squash", self.palette.modified),
            RebaseAction::Fixup => ("fixup", self.palette.modified),
            RebaseAction::Drop => ("drop", self.palette.removed),
        }
    }

    /// Render the interactive-rebase todo editor: a header, the editable commit
    /// list (action · hash · subject), and a key-hint footer.
    fn render_rebase_todo(&self, rt: &RebaseTodoView, view: &Entity<Self>) -> gpui::Div {
        let count = rt.steps.len();
        let body = uniform_list("rebase-todo-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.rebase_todo() {
                    Some(rt) => range
                        .map(|ix| this.render_rebase_todo_row(rt, ix))
                        .collect(),
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&rt.scroll)
        .flex_grow(1.0);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(if rt.confirming_cancel {
                // Unsaved edits to the plan: confirm before discarding them.
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Discard rebase edits?")),
                    )
                    .child(self.key_action(
                        "rebase-todo-discard",
                        "y",
                        "discard",
                        view,
                        Self::discard_rebase_todo,
                    ))
                    .child(self.key_action(
                        "rebase-todo-keep",
                        "n",
                        "keep editing",
                        view,
                        Self::keep_editing_rebase_todo,
                    ))
            } else {
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from(match rt.mode {
                                RebaseTodoMode::Start => format!("Rebase {}..HEAD", rt.base),
                                RebaseTodoMode::Edit => "Edit rebase todo".to_string(),
                            })),
                    )
                    .child(self.key_action(
                        "rebase-todo-start",
                        "return",
                        match rt.mode {
                            RebaseTodoMode::Start => "start",
                            RebaseTodoMode::Edit => "save",
                        },
                        view,
                        Self::run_rebase_todo,
                    ))
                    .child(self.key_action(
                        "rebase-todo-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::close_rebase_todo,
                    ))
            })
            .child(body)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(self.palette.dim)
                    .child(SharedString::from(
                        "p pick · e edit · s squash · f fixup · d drop · j/k move · J/K reorder",
                    )),
            )
    }

    /// One row of the rebase-todo editor.
    fn render_rebase_todo_row(&self, rt: &RebaseTodoView, ix: usize) -> gpui::Div {
        let step = &rt.steps[ix];
        let selected = ix == rt.selected;
        let (keyword, color) = self.rebase_action_style(step.action);
        let dropped = step.action == RebaseAction::Drop;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(ROW_HEIGHT))
            .when(selected, |el| el.bg(self.palette.selection))
            .child(
                div()
                    .w(px(56.0))
                    .flex_shrink_0()
                    .text_color(color)
                    .child(SharedString::from(keyword)),
            )
            .child(
                div()
                    .w(px(72.0))
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(step.oid.clone())),
            )
            .child(
                div()
                    .text_color(if dropped {
                        self.palette.dim
                    } else {
                        self.palette.fg
                    })
                    .child(SharedString::from(step.subject.clone())),
            )
    }

    /// The settings "Open config file" control: a split button whose main half
    /// opens the config in the OS default app, and whose dropdown offers "Copy
    /// path" plus an "Open in" list of the installed editors. It's an escape
    /// hatch for settings the UI doesn't expose, and a way to see where the file
    /// lives. Menu items dispatch actions routed to the status view's focus.
    fn open_config_button(&self, view: &Entity<Self>) -> impl IntoElement {
        let editors = self.editors.clone();
        let focus = self.focus.clone();
        let main = Button::new("open-config-main")
            .label("Open config file")
            .ghost()
            .small()
            .icon(IconName::ExternalLink)
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    view.update(cx, |this, _| this.open_config_file());
                }
            });
        DropdownButton::new("open-config")
            .ghost()
            .small()
            .button(main)
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu
                    .action_context(focus.clone())
                    .menu("Copy path", Box::new(CopyConfigPath));
                if !editors.is_empty() {
                    menu = menu.separator().label("Open in");
                    for (name, path) in &editors {
                        menu = menu.menu(name.clone(), Box::new(OpenConfigWith(path.clone())));
                    }
                }
                menu
            })
    }

    /// Persist the current config (so the file exists even if never edited) and
    /// return its path.
    fn saved_config_path(&self) -> Option<PathBuf> {
        config::save(&self.config);
        config::path()
    }

    /// A settings toggle (a `Switch` bound to a `bool` config field) paired with
    /// an info icon whose tooltip explains the setting. The tooltip shows
    /// immediately on hover (zero show-delay, unlike the library's 500ms managed
    /// tooltip) and wraps to a readable width rather than one long line. The
    /// switch flips the field and persists on click; all of it is mouse-driven,
    /// like the rest of the settings screen (not part of the Tab focus ring).
    fn toggle_control(
        &self,
        id: &'static str,
        checked: bool,
        explanation: &'static str,
        view: &Entity<Self>,
        set: fn(&mut config::Config, bool),
    ) -> AnyElement {
        let switch = Switch::new(id).checked(checked).on_click({
            let view = view.clone();
            move |on, _window, cx| {
                let on = *on;
                view.update(cx, |this, cx| {
                    set(&mut this.config, on);
                    config::save(&this.config);
                    cx.notify();
                });
            }
        });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(switch)
            .child(self.info_icon(format!("{id}-info"), explanation))
            .into_any_element()
    }

    /// A small dimmed `(i)` icon that reveals `explanation` in a tooltip on
    /// hover — for clarifying what a settings control does.
    fn info_icon(&self, id: String, explanation: &'static str) -> impl IntoElement {
        let font = self.font.clone();
        let dim = self.palette.dim;
        div()
            .id(SharedString::from(id.clone()))
            .relative()
            .child(track_target(id))
            .child(Icon::new(IconName::Info).xsmall().text_color(dim))
            // gpui's native tooltip (not the library's managed one) so we can
            // drop the show-delay to zero and bound the width so it wraps. The
            // library tooltip forces the theme's UI font; override it back to
            // our monospace chrome font so it matches the rest of the app.
            .tooltip(move |window, cx| {
                let font = font.clone();
                Tooltip::element(move |_, _| {
                    div()
                        .max_w(px(280.0))
                        .font_family(font.clone())
                        .child(SharedString::from(explanation))
                })
                .build(window, cx)
            })
            .tooltip_show_delay(Duration::ZERO)
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
        } else if clickable {
            // A subtle hover on rows you can act on (not the current line or a
            // visual selection, which already have a background) — the theme's
            // explicit hover wash, so it reads as a preview of selecting.
            el = el.hover(|s| s.bg(self.palette.hover));
        }

        // Code-, diff-, and path-bearing rows render monospace (alignment and
        // code legibility); prose rows (sections, headers, messages) inherit the
        // UI font from the root.
        if matches!(
            row.kind,
            RowKind::Diff { .. } | RowKind::HunkHeader { .. } | RowKind::File { .. }
        ) {
            el = el.font_family(self.font.clone());
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
                // The section count: just a dim number, no badge/tag chrome.
                .child(
                    div()
                        .text_color(self.palette.dim)
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
                    let conflicted = self.is_conflicted(target_path(target));
                    let (ours_label, theirs_label) = self.conflict_side_labels();
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
                        // A conflicted file resolves by taking a whole side.
                        if conflicted {
                            menu = menu
                                .menu(ours_label, Box::new(CtxTakeOurs))
                                .menu(theirs_label, Box::new(CtxTakeTheirs))
                                .separator();
                        }
                        if can_stage {
                            menu = menu.menu("Stage", Box::new(CtxStage));
                        }
                        if can_unstage {
                            menu = menu.menu("Unstage", Box::new(CtxUnstage));
                        }
                        if can_discard {
                            menu = menu.menu("Discard", Box::new(CtxDiscard));
                        }
                        menu.separator().menu("Copy", Box::new(CtxCopy))
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

    /// The pending-prefix strip, pinned to the window bottom. A lightweight line
    /// showing just the pressed key, until the which-key delay elapses — then it
    /// expands into the continuations (each `<prefix> <key>` and its command's
    /// label), like emacs' which-key.
    fn prefix_indicator(&self) -> Option<gpui::Div> {
        let pending = self.pending_prefix.as_ref()?;
        let mut bar = div()
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .text_color(self.palette.dim)
            .text_xs()
            .flex()
            .items_center()
            .gap_3();
        // The keys typed so far in a single keycap, with a trailing dash to show
        // the sequence is awaiting the next key (emacs' echo-area `g-` feedback).
        bar = bar.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(kbd::key_chip(&pending.seq, self.palette.dim, &self.font))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("-")),
                ),
        );
        if pending.which_key {
            // Group bindings by their immediate next key after the typed prefix.
            // A next key that completes a binding shows its command's label; one
            // that only leads deeper shows "…" to mark a further sub-sequence.
            let lead = format!("{} ", pending.seq);
            let mut conts: std::collections::BTreeMap<String, Option<&'static str>> =
                std::collections::BTreeMap::new();
            for (k, id) in &self.keymap {
                let Some(rest) = k.strip_prefix(&lead) else {
                    continue;
                };
                let token = rest.split(' ').next().unwrap_or(rest).to_string();
                let completes = format!("{lead}{token}") == *k;
                let title = completes
                    .then(|| commands().iter().find(|c| c.id == id).map(|c| c.title))
                    .flatten();
                // A completing binding's label wins over a sibling sub-prefix.
                let entry = conts.entry(token).or_insert(None);
                if title.is_some() {
                    *entry = title;
                }
            }
            for (token, title) in conts {
                bar = bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(kbd::key_chip(&token, self.palette.dim, &self.font))
                        .child(
                            div()
                                .text_color(self.palette.dim)
                                .child(SharedString::from(title.unwrap_or("…"))),
                        ),
                );
            }
        }
        Some(bar)
    }

    /// The status/confirmation banner ("Copied …", errors), as a bottom-pinned
    /// bar. The full-window sub-views (settings, commit, log, …) append this so
    /// a copy confirmation is visible there too, not only in the status view.
    fn status_toast(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let msg = self.status_message.clone()?;
        let bar = div()
            .id("status-bar")
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .text_color(self.palette.fg)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _window, cx| {
                this.clear_status(cx);
            }));
        // A keys-led message (e.g. "g x is unbound") renders each typed key as a
        // keycap before the text, matching the which-key strip.
        if let Some(keys) = self.status_keys.clone() {
            return Some(
                bar.flex()
                    .items_center()
                    .gap_2()
                    .child(kbd::key_chip(&keys, self.palette.dim, &self.font))
                    .child(SharedString::from(msg)),
            );
        }
        // A copy confirmation renders the copied value emphasized — accent
        // color, monospace, italic — so a path or hash reads as a literal.
        Some(match self.status_copied.clone() {
            Some(value) if msg == COPIED_LABEL => bar
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(SharedString::from(COPIED_LABEL))
                .child(
                    div()
                        .font_family(self.font.clone())
                        .italic()
                        .text_color(self.palette.section)
                        .child(value),
                ),
            // While a mutating job runs, hint that C-g/Esc cancels it.
            _ if self.job_cancel.is_some() => bar
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(SharedString::from(msg))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("C-g to cancel")),
                ),
            _ => bar.child(SharedString::from(msg)),
        })
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep keyboard focus on the status view whenever nothing else owns the
        // keyboard (the commit editor, settings, and the picker each have
        // their own focused input), so keys always land — including debug-channel
        // keystrokes while the window isn't frontmost.
        let owns_focus_elsewhere = self.editor().is_some()
            || self.settings().is_some()
            || matches!(self.popup, Some(Popup::Picker(_)));
        if !owns_focus_elsewhere && !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
        }
        self.palette = Palette::from_theme(cx);

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .key_context(STATUS_CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleFold, window, cx| {
                // Tab is delivered as an action (gpui's Root binds it for
                // focus-nav, which we override here), but its *effect* routes
                // through the keymap like any key, so rebinding/unbinding `tab`
                // in `[keymap]` takes effect.
                if this.settings().is_some() {
                    this.cycle_settings_focus(window, cx);
                } else if this.editor().is_none()
                    && matches!(this.popup, None | Some(Popup::Dispatch(_)))
                {
                    this.run_dispatch("tab", window, cx);
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
                if this.editor().is_none() && this.popup.is_none() && this.settings().is_none() {
                    this.open_settings(window, cx);
                }
            }))
            // Right-click menu actions, applied to the row at point / selection.
            .on_action(cx.listener(|this, _: &CtxStage, _window, cx| this.act(Op::Stage, cx)))
            .on_action(cx.listener(|this, _: &CtxUnstage, _window, cx| this.act(Op::Unstage, cx)))
            .on_action(cx.listener(|this, _: &CtxDiscard, _window, cx| this.act(Op::Discard, cx)))
            .on_action(cx.listener(|this, _: &CtxTakeOurs, _window, cx| {
                this.resolve_at_point(ConflictSide::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxTakeTheirs, _window, cx| {
                this.resolve_at_point(ConflictSide::Theirs, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxCopy, _window, cx| this.copy_selection(cx)))
            // Settings "Open config file" dropdown actions.
            .on_action(
                cx.listener(|this, _: &CopyConfigPath, _window, cx| this.copy_config_path(cx)),
            )
            .on_action(cx.listener(|this, action: &OpenConfigWith, _window, _cx| {
                this.open_config_with(&action.0)
            }))
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(self.palette.bg)
            .text_color(self.palette.fg)
            .text_size(px(13.0))
            // Proportional UI font is the base for prose chrome; code/diff/
            // tabular rows and the code views override back to monospace. When
            // no UI font is configured, `ui_font` equals `font`, so this is the
            // old all-monospace behavior.
            .font_family(self.ui_font.clone())
            .flex()
            .flex_col();

        // The title bar sits above every view (status, settings, editor, …).
        root = root.child(self.render_title_bar(&view));

        // Each non-Status screen takes over the window. One match defines the
        // active screen (no re-derived priority cascade); Status falls through to
        // the status list below.
        match &self.screen {
            Screen::Settings(s) => {
                return root
                    .child(self.render_settings(s, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Editor(ed) => {
                return root
                    .child(self.render_editor(ed, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::GitLog(scroll) => {
                return root
                    .child(self.render_git_log(scroll, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::RebaseTodo(rt) => {
                return root
                    .child(self.render_rebase_todo(rt, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Commit { view: cv, .. } => {
                return root
                    .child(self.render_commit_view(cv, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Log(log) => {
                return root
                    .child(self.render_log(log, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Status => {}
        }

        // An in-progress merge/rebase/cherry-pick/revert sits above the list,
        // visible while the user resolves it.
        if let Some(seq) = &self.sequence {
            root = root.child(self.render_sequence_banner(seq, &view));
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
                Popup::Picker(state) => self.render_picker(state, &view),
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
        } else {
            // The status/error/"Copied" banner: click it (or press Esc) to dismiss.
            root = root.children(self.status_toast(cx));
        }

        // A floating "?" button (bottom-right) opens the dispatch menu — a
        // mouse affordance for discovering commands. Hidden while a popup or a
        // bottom bar (confirm / visual / status) is shown, so it never overlaps
        // them.
        let bottom_bar = self.confirm.is_some()
            || self.visual.is_some()
            || self.status_message.is_some()
            || self.pending_prefix.is_some();
        if self.popup.is_none() && !bottom_bar {
            // A plain div (not gpui-component `Button`, which forces a default
            // cursor for non-link variants) so it shows the click cursor, like
            // the app's other affordances.
            let tip_font = self.font.clone();
            root = root.child(
                div()
                    .absolute()
                    .bottom_3()
                    .right_4()
                    .child(track_target("dispatch-help"))
                    .child(
                        div()
                            .id("dispatch-help")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(14.0))
                            .cursor_pointer()
                            .text_color(self.palette.dim)
                            .hover(|s| s.bg(self.palette.selection).text_color(self.palette.fg))
                            .child(SharedString::from("?"))
                            .tooltip(move |window, cx| {
                                let font = tip_font.clone();
                                Tooltip::element(move |_, _| {
                                    div().font_family(font.clone()).child("Help (?)")
                                })
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.popup = Some(Popup::Dispatch(dispatch_menu(&this.keymap)));
                                cx.notify();
                            })),
                    ),
            );
        }

        // The pending-prefix strip pins to the very bottom, below any other bar.
        root = root.children(self.prefix_indicator());

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
    }
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
fn apply_scroll_key(
    handle: &UniformListScrollHandle,
    top: &mut usize,
    len: usize,
    key: &str,
    shift: bool,
    ctrl: bool,
    page: usize,
) -> bool {
    let page = (page as isize).max(1);
    let half = (page / 2).max(1);
    let cur = *top as isize;
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
        _ => return false,
    };
    *top = target.clamp(0, max_top) as usize;
    // Strict scrolling positions the row even when it's already visible, so line
    // and half-page motions actually move. On the last page, pin the final row
    // to the *bottom* instead — the page-size estimate (header/padding overhead)
    // is slightly off, and pinning guarantees the very last row is reachable.
    if *top as isize >= max_top && len > 0 {
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
        let km = build_keymap(&config::Config::default()).0;
        let menu: HashSet<String> = dispatch_menu(&km)
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
        });
        config.keymap.insert("X".into(), "user.wip".into());
        config.keymap.insert("Y".into(), "user.nope".into()); // unknown id
        let (km, warnings) = build_keymap(&config);
        assert_eq!(km.get("X").map(String::as_str), Some("user.wip"));
        assert!(!km.contains_key("Y"), "unknown id isn't bound");
        assert_eq!(warnings.len(), 1, "only the unknown id warns: {warnings:?}");
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
            "C-d", "C-u", "C-f", "C-b", // half/full page motions
        ];
        // Keys allowed to be on only one side of the check. Empty today; add a
        // key here (with a comment) when an exception is genuinely warranted.
        const OVERRIDES: &[&str] = &[];

        let km = build_keymap(&config::Config::default()).0;
        let menu: HashSet<String> = dispatch_menu(&km)
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
}

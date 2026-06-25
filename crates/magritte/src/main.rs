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
use std::time::Duration;

use gpui::{
    actions, div, px, size, uniform_list, AnyElement, App, AppContext, Bounds, ClipboardItem,
    Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, TitlebarOptions, UniformListScrollHandle,
    Window, WindowAppearance, WindowBounds, WindowOptions,
};

use gpui::prelude::FluentBuilder;

mod config;
#[cfg(feature = "debug")]
mod debug;
mod git_action;
mod highlight;
mod picker;
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
actions!(magritte, [CtxStage, CtxUnstage, CtxDiscard]);
// Settings "Open config file" dropdown actions: copy the path, or open the
// config with a specific editor (carries the editor's app path). `no_json`
// avoids the serde/schemars requirement of keymap-loadable actions.
actions!(magritte, [CopyConfigPath]);
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = magritte, no_json)]
struct OpenConfigWith(SharedString);
use gpui::Subscription;
use gpui_component::button::{Button, ButtonRounded, ButtonVariants, DropdownButton};
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
    Change, CommitMode, DiffSource, EntryKind, FileDiff, FileEntry, LineKind, RemoteTargets, Repo,
    Status,
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
        TransientState {
            def,
            active: std::collections::HashSet::new(),
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
    /// Picking which remote to push/pull/fetch against (when the target isn't
    /// configured, or for "elsewhere", and more than one remote exists).
    RemotePicker(RemotePickerState),
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
        }
    }
}

/// An open target picker (vertico-style): a prompt, an inline query input, a
/// ranked candidate list, and the pending action. It runs against the
/// highlighted (or clicked) candidate on Enter.
struct RemotePickerState {
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
/// stderr output. Flattening keeps the view a single uniform-height list.
enum GitLogRow {
    Command { args: String, ok: bool },
    Output(String),
}

/// The commit-log view (`l`): a scrollable list of commits with j/k navigation;
/// Enter opens the selected commit's diff in a [`CommitView`].
struct LogState {
    entries: Vec<magritte_core::LogEntry>,
    selected: usize,
    scroll: UniformListScrollHandle,
}

/// A single commit's detail (opened from the log): its header and diff, as the
/// same flattened rows the commit editor renders.
struct CommitView {
    /// `<short-hash> <subject>`, shown in the header.
    title: SharedString,
    rows: Vec<CommitDiffRow>,
    scroll: UniformListScrollHandle,
    /// Tracked top-row index for keyboard scrolling (see [`apply_scroll_key`]).
    top: usize,
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
}

impl Category {
    fn title(self) -> &'static str {
        match self {
            Category::Commands => "Commands",
            Category::Application => "Application",
            Category::Applying => "Applying changes",
            Category::Essential => "Essential",
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
            t.open_transient(transient::commit_transient(), RemoteTargets::default(), cx)
        }),
        top!("branch", "Branch", Category::Commands, "b", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient(transient::branch_transient(), rt, cx)
        }),
        top!("stash", "Stash", Category::Commands, "Z", |t, _w, cx| {
            t.open_transient(transient::stash_transient(), RemoteTargets::default(), cx)
        }),
        top!("log", "Log", Category::Commands, "l", |t, _w, cx| {
            t.open_transient(transient::log_transient(), RemoteTargets::default(), cx)
        }),
        top!("push", "Push", Category::Commands, "p", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient(transient::push_transient(&rt), rt, cx)
        }),
        top!("pull", "Pull", Category::Commands, "F", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient(transient::pull_transient(&rt), rt, cx)
        }),
        top!("fetch", "Fetch", Category::Commands, "f", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient(transient::fetch_transient(&rt), rt, cx)
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
    ];
    C
}

/// The keystroke sequence to reach the command with this palette title, as
/// space-separated keys: a top-level command's own key (e.g. `p`), or a leaf's
/// full prefix-then-suffix path (e.g. `c c` for "Create commit"). `None` if it
/// has no binding. Lets the `:` palette double as a keymap reference.
fn command_keys(title: &str) -> Option<String> {
    let cmd = commands().iter().find(|c| c.title == title)?;
    if let Some(key) = cmd.key {
        return Some(key.to_string());
    }
    // A leaf: locate its suffix in the transients and prepend the prefix key.
    let leaf = cmd.leaf?;
    let rt = RemoteTargets::default();
    let prefixes: [(&str, Transient); 7] = [
        ("c", transient::commit_transient()),
        ("b", transient::branch_transient()),
        ("Z", transient::stash_transient()),
        ("l", transient::log_transient()),
        ("p", transient::push_transient(&rt)),
        ("F", transient::pull_transient(&rt)),
        ("f", transient::fetch_transient(&rt)),
    ];
    for (prefix_key, t) in &prefixes {
        for group in &t.groups {
            for suffix in &group.suffixes {
                if let Suffix::Action(a) = suffix {
                    if a.command == leaf {
                        return Some(format!("{prefix_key} {}", a.key));
                    }
                }
            }
        }
    }
    None
}

/// The `?` dispatch menu: a modal command transient (magit's dispatch),
/// generated from the [`commands`] registry (grouped by [`Category`]) plus a
/// static Navigation group for the pure motions. Each row is invoked by its key
/// or a click.
///
/// This menu is the discoverable face of the keymap. The
/// `dispatch_menu_covers_every_command` test cross-checks it against the keys
/// `run_dispatch` actually handles, so a command can't be shown-but-dead or
/// invocable-but-hidden.
fn dispatch_menu() -> Transient {
    let info = |keys, description| Suffix::Info(transient::Info { keys, description });
    let group = |cat: Category| Group {
        title: transient::plain_title(cat.title()),
        suffixes: commands()
            .iter()
            .filter(|c| c.menu && c.category == cat)
            .map(|c| {
                Suffix::Info(transient::Info {
                    keys: c.key.expect("a `?`-menu command has a key"),
                    description: c.title,
                })
            })
            .collect(),
    };
    // Essential gathers the always-available registry commands plus the `:`
    // palette — itself a meta-affordance (reach any command), not a registry
    // entry, so it's appended here rather than living in `commands()`.
    let mut essential = group(Category::Essential);
    essential.suffixes.push(info(":", "Command palette"));
    Transient {
        title: transient::plain_title("Dispatch"),
        groups: vec![
            group(Category::Commands),
            group(Category::Applying),
            Group {
                title: transient::plain_title("Navigation"),
                suffixes: vec![
                    info("j", "Move down"),
                    info("k", "Move up"),
                    info("g g", "Top"),
                    info("G", "Bottom"),
                    info("g j", "Next section"),
                    info("g k", "Previous section"),
                ],
            },
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
/// Width (points) of the repo header label column ("Head:", "Push:"), so their
/// values line up like a description list.
const HEADER_LABEL_WIDTH: f32 = 40.0;
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

/// Auto-wrap the commit body *only when the cursor is at the end of an
/// over-long line* — i.e. while typing at the end of a line — so that editing
/// in the middle of the message never reflows text under the user. The summary
/// (line 0) is never wrapped. Returns the rewrapped text when a wrap happened.
/// `cursor` is a byte offset (as the input reports it); because wrapping only
/// turns a space into a newline, that offset stays valid in the result.
fn wrap_at_cursor(text: &str, cursor: usize, width: usize) -> Option<String> {
    let mut line_start = 0; // byte offset of the current line's first char
    for (i, line) in text.split('\n').enumerate() {
        let line_end = line_start + line.len(); // byte offset before the '\n'
        if cursor <= line_end {
            // The cursor is on this line. Wrap only when it's at the very end of
            // the line, the line isn't the summary, and it overruns the width.
            if cursor != line_end || i == 0 || line.chars().count() <= width {
                return None;
            }
            let pieces = wrap_line(line, width);
            if pieces.len() <= 1 {
                return None; // unbreakable (e.g. a single long word)
            }
            let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            lines.splice(i..=i, pieces);
            return Some(lines.join("\n"));
        }
        line_start = line_end + 1; // + the '\n' byte
    }
    None
}

/// Reflow the commit *body* to `width`: each blank-line-separated paragraph is
/// joined into one line then re-wrapped, collapsing runs of whitespace. The
/// summary (line 0) and blank separator lines are left untouched. Unlike
/// [`wrap_at_cursor`], this *re-joins* manually-broken lines, so it's an
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

/// The character-column range of the part of the summary (line 0) that overruns
/// `limit` columns, as `(start, end)` for a diagnostic `Position` (whose
/// `character` field is a 0-based character count). `None` when the summary
/// fits.
fn title_overflow(text: &str, limit: usize) -> Option<(u32, u32)> {
    let title = text.split('\n').next().unwrap_or("");
    let chars = title.chars().count();
    if chars <= limit {
        return None;
    }
    Some((limit as u32, chars as u32))
}

/// Convert a byte offset into `text` (as the input reports the cursor) to a
/// 0-based line / character-column [`Position`], for restoring the cursor after
/// a programmatic edit.
fn byte_offset_to_position(text: &str, offset: usize) -> Position {
    let (mut line, mut col, mut bytes) = (0u32, 0u32, 0usize);
    for ch in text.chars() {
        if bytes >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1; // character column
        }
        bytes += ch.len_utf8();
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
    /// A repository header line (Head:/Push:): a dim fixed-width label, the
    /// value (a branch/ref rendered as the stylized chip used in dialogs, or
    /// plain text), and an optional dim detail (e.g. ahead/behind).
    Header {
        label: String,
        value: String,
        chip: bool,
        detail: Option<String>,
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
    ui_font: Entity<SelectState<SearchableVec<SharedString>>>,
    /// External editor. macOS picks from a dropdown of detected editor apps
    /// (plus "System Default"); elsewhere it's a free-text command.
    #[cfg(target_os = "macos")]
    editor: Entity<SelectState<SearchableVec<SharedString>>>,
    #[cfg(not(target_os = "macos"))]
    editor: Entity<InputState>,
    /// Which dropdown Tab focuses next (0=appearance,1=light,2=dark,3=font,
    /// 4=ui_font).
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

/// All selectable font families (for the proportional UI-font picker), sorted.
/// Unlike [`monospace_font_names`] this keeps proportional families too.
fn all_font_names(cx: &App) -> Vec<SharedString> {
    let mut names: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
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

/// Installed text editors as (display name, `.app` path), for the settings
/// editor picker and the "Open config in" menu.
///
/// We ask LaunchServices which installed apps register to *edit* plain text or
/// source code (`kLSRolesEditor`) and union the two sets — that's macOS's own
/// notion of "apps that report as text editors", and it picks up whatever the
/// user actually has (VS Code, Zed, BBEdit, TextEdit, …) with no hand-kept
/// allow-list. The catch is that office suites and a few system apps over-claim
/// the editor role for plain text, so [`is_bogus_editor`] drops the known
/// offenders. Names/paths come from resolving each bundle id to its app URL.
#[cfg(target_os = "macos")]
fn text_editors() -> Vec<(SharedString, SharedString)> {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use core_foundation::url::CFURL;
    use std::os::raw::c_void;

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSCopyAllRoleHandlersForContentType(content_type: CFStringRef, role: u32) -> CFArrayRef;
        fn LSCopyApplicationURLsForBundleIdentifier(
            bundle_id: CFStringRef,
            out_error: *mut c_void,
        ) -> CFArrayRef;
    }
    // kLSRolesEditor — handlers that can *edit* the type, not merely view it
    // (kLSRolesViewer, 0x2, which would pull in browsers and media players).
    const K_LS_ROLES_EDITOR: u32 = 0x0000_0004;

    let mut seen = std::collections::HashSet::new();
    let mut editors: Vec<(SharedString, SharedString)> = Vec::new();
    for content_type in ["public.plain-text", "public.source-code"] {
        let ct = CFString::new(content_type);
        let handlers = unsafe {
            let r =
                LSCopyAllRoleHandlersForContentType(ct.as_concrete_TypeRef(), K_LS_ROLES_EDITOR);
            if r.is_null() {
                continue;
            }
            CFArray::<CFString>::wrap_under_create_rule(r)
        };
        for bundle in handlers.iter() {
            let id = bundle.to_string();
            if is_bogus_editor(&id) || !seen.insert(id.clone()) {
                continue;
            }
            // Resolve the bundle id to its installed app URL(s); take the first.
            let urls = unsafe {
                let r = LSCopyApplicationURLsForBundleIdentifier(
                    bundle.as_concrete_TypeRef(),
                    std::ptr::null_mut(),
                );
                if r.is_null() {
                    continue;
                }
                CFArray::<CFURL>::wrap_under_create_rule(r)
            };
            if let Some(path) = urls.iter().next().and_then(|u| u.to_path()) {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| id.clone());
                editors.push((
                    SharedString::from(name),
                    SharedString::from(path.to_string_lossy().into_owned()),
                ));
            }
        }
    }
    editors.sort_by_key(|(name, _)| name.to_lowercase());
    editors
}

/// Bundle ids that register as plain-text editors but aren't general text
/// editors — office/productivity suites that over-claim the role.
#[cfg(target_os = "macos")]
fn is_bogus_editor(bundle_id: &str) -> bool {
    const DENY_PREFIXES: &[&str] = &[
        "com.apple.iWork",
        "com.apple.Numbers",
        "com.apple.Pages",
        "com.apple.Keynote",
        "com.apple.Notes",
        "com.microsoft.Word",
        "com.microsoft.Excel",
        "com.microsoft.Powerpoint",
        "org.libreoffice",
    ];
    DENY_PREFIXES.iter().any(|p| bundle_id.starts_with(p))
}

#[cfg(not(target_os = "macos"))]
fn text_editors() -> Vec<(SharedString, SharedString)> {
    Vec::new()
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
    // Fall back to our default theme when the configured name isn't found —
    // e.g. a config referencing a theme we've since dropped — rather than
    // leaving gpui-component's built-in default.
    let pick = |name: &str, fallback: &str| {
        registry
            .themes()
            .get(name)
            .or_else(|| registry.themes().get(fallback))
            .cloned()
    };
    let light = pick(cfg.light_theme(), config::DEFAULT_LIGHT_THEME);
    let dark = pick(cfg.dark_theme(), config::DEFAULT_DARK_THEME);
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
/// Label for the UI-font entry that reuses the monospace font — the default, so
/// the UI stays all-monospace until you opt into a proportional UI.
const UI_FONT_DEFAULT_LABEL: &str = "Same as monospace";
/// Config sentinel (and the "System Default" UI-font entry) for the platform's
/// proportional system UI font, distinct from an empty value (= monospace).
const SYSTEM_UI_FONT: &str = "system-ui";
/// Label for the editor-picker entry that opens files in the OS default app
/// (an empty `editor` config).
#[cfg(target_os = "macos")]
const EDITOR_OS_DEFAULT_LABEL: &str = "System Default";

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

/// The platform's system proportional UI font (the analog of
/// [`system_mono_font`]): `.AppleSystemUIFont` on macOS, else the theme's.
#[cfg(target_os = "macos")]
fn system_ui_font(_cx: &App) -> SharedString {
    SharedString::from(".AppleSystemUIFont")
}
#[cfg(not(target_os = "macos"))]
fn system_ui_font(cx: &App) -> SharedString {
    cx.theme().font_family.clone()
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

/// The UI font for prose chrome (menus, headings, labels): empty reuses the
/// monospace [`resolve_font`] (the default, so nothing changes until opted in),
/// the [`SYSTEM_UI_FONT`] sentinel uses the platform proportional font, and any
/// other value is a chosen family.
fn resolve_ui_font(cfg: &config::Config, cx: &App) -> SharedString {
    match cfg.ui_font.as_str() {
        "" => resolve_font(cfg, cx),
        SYSTEM_UI_FONT => system_ui_font(cx),
        name => SharedString::from(name.to_string()),
    }
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
    /// Whether the Emacs `C-x` prefix is pending (next key resolves it, e.g.
    /// `C-x C-c` to quit).
    pending_cx: bool,
    /// An open bottom popup (command transient or help menu), or `None`.
    popup: Option<Popup>,
    /// The commit message editor, when open (takes over the window).
    editor: Option<CommitEditor>,
    /// The live settings screen, when open (takes over the window).
    settings: Option<SettingsState>,
    /// The git command-log view (magit's `$` process buffer), when open. Holds
    /// the scroll state; the entries are read live from the repo.
    git_log: Option<ScrollView>,
    /// The commit-log view (`l`), when open.
    log: Option<LogState>,
    /// A commit's diff detail (opened from the log with Enter), when open.
    commit_view: Option<CommitView>,
    /// The monospace font family for code, diffs, and tabular columns.
    font: SharedString,
    /// The proportional UI font for prose chrome; equals `font` when unset.
    ui_font: SharedString,
    /// The loaded user config (theme/appearance/font), kept so we can re-apply
    /// on config-file edits or system appearance changes.
    config: config::Config,
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
    /// Bumped each time the status message changes, so an auto-dismiss timer
    /// only clears the message it was scheduled for (not a newer one).
    status_seq: u64,
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
        let font = resolve_font(&config, cx);
        let ui_font = resolve_ui_font(&config, cx);

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
            pending_cx: false,
            popup: None,
            editor: None,
            settings: None,
            git_log: None,
            log: None,
            commit_view: None,
            font,
            ui_font,
            config,
            usage: config::load_usage(),
            mono_fonts: Vec::new(),
            ui_fonts: Vec::new(),
            editors: Vec::new(),
            status_message: startup_warning,
            status_seq: 0,
            git_log_show_all: false,
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
        self.ui_font = resolve_ui_font(&self.config, cx);
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
        match &head.branch {
            // A real branch: the name as the stylized chip.
            Some(branch) => rows.push(header_row("Head:", branch.clone(), true, None)),
            // Detached HEAD: plain text, no chip (it isn't a branch).
            None => rows.push(header_row("Head:", "detached".to_string(), false, None)),
        }
        if let Some(upstream) = &head.upstream {
            // Only note ahead/behind when actually diverged.
            let detail = (head.ahead > 0 || head.behind > 0)
                .then(|| format!("+{} -{}", head.ahead, head.behind));
            rows.push(header_row("Push:", upstream.clone(), true, detail));
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

    /// Open the file at point (its row, or the file a hunk/line belongs to) in
    /// the external editor. Bound to Return.
    fn open_at_point(&mut self, cx: &mut Context<Self>) {
        let path = match self.rows.get(self.selected).and_then(|r| r.target.as_ref()) {
            Some(Target::File(f)) => f.path.clone(),
            Some(Target::Hunk { file, .. } | Target::Line { file, .. }) => file.path.clone(),
            _ => return,
        };
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let full = repo.workdir().join(&path);
        self.launch_editor(&full);
        self.set_status(format!("Opening {path}"), true, cx);
    }

    /// Open `path` in the user's configured editor. An empty `editor` opens the
    /// OS default app; otherwise `editor` is run as a command (`code -w`, `zed`)
    /// and, failing that on macOS, treated as an application name to `open -a`
    /// (so "Zed" or "Visual Studio Code" work too). Best-effort, non-blocking.
    fn launch_editor(&self, path: &std::path::Path) {
        let editor = self.config.editor.trim();
        if editor.is_empty() {
            open_with_os(path);
            return;
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
        open_with_os(path);
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
            Some((_, Confirm::AmendPushed(command, switches))) => {
                self.proceed_history_rewrite(command, switches, window, cx);
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

    fn open_transient(&mut self, def: Transient, targets: RemoteTargets, cx: &mut Context<Self>) {
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

        // Invoke an action.
        let action = state.def.action_for(key).cloned();
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
        }
    }

    /// Fire a leaf command (a transient suffix) with already-gathered arguments.
    /// Shared by the transient (which passes its toggled switches/options) and
    /// the `:` palette (which fires with defaults via [`Self::fire_command_default`]).
    fn fire_action(
        &mut self,
        command: transient::Command,
        fired: ActionArgs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ActionArgs {
            args,
            paths,
            targets,
            limit,
        } = fired;
        self.popup = None;
        use transient::Command::*;
        match command {
            CommitCreate => self.start_commit(args, window, cx),
            // Amend/reword/extend rewrite HEAD: warn first if it's published.
            CommitAmend | CommitReword | CommitExtend => {
                self.begin_history_rewrite(command, args, window, cx)
            }
            // Push/pull/fetch resolve a remote (prompting if needed) then run.
            PushPushRemote | PushUpstream | PushElsewhere | PullPushRemote | PullUpstream
            | PullElsewhere | FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => {
                self.dispatch_transfer(command, &targets, args, window, cx)
            }
            BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
                self.dispatch_branch(command, window, cx)
            }
            StashPush => self.run_stash_push(false, cx),
            StashPushAll => self.run_stash_push(true, cx),
            StashApply | StashPop | StashDrop => self.dispatch_stash(command, window, cx),
            // Log: assemble flags + scope + pathspecs in the order git needs.
            LogCurrent => self.start_log(build_log_args(args, LogScope::Current, paths, limit), cx),
            LogAll => self.start_log(build_log_args(args, LogScope::All, paths, limit), cx),
            LogOther => self.prompt_log_ref(args, paths, limit, window, cx),
            LogReflog => self.start_reflog(limit, cx),
        }
    }

    /// Fire a leaf command from the palette: no transient was open, so use
    /// default arguments (no switches/options, current targets, default log
    /// limit). The command still opens its own picker/editor when it needs one.
    fn fire_command_default(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.remote_targets();
        self.fire_action(
            command,
            ActionArgs::defaults(targets, Self::LOG_LIMIT),
            window,
            cx,
        );
    }

    /// Open the stash picker for an apply/pop/drop command.
    fn dispatch_stash(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        use transient::Command::*;
        let action = match command {
            StashApply => StashAction::Apply,
            StashPop => StashAction::Pop,
            StashDrop => StashAction::Drop,
            _ => return,
        };
        let stashes = repo.stash_list().unwrap_or_default();
        if stashes.is_empty() {
            self.status_message = Some("No stashes".to_string());
            cx.notify();
            return;
        }
        let choices = stashes.iter().map(|s| s.display()).collect();
        self.open_picker(
            PickerAction::Stash(action),
            choices,
            CreateMode::None,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Prompt for a transient option's value (free text, with completion
    /// candidates), stashing `resume` so the transient reopens with the value
    /// applied (or unchanged on cancel).
    fn open_option_prompt(
        &mut self,
        key: String,
        description: String,
        completion: transient::Completion,
        resume: TransientState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A fixed value set is selection-only; everything else is value entry
        // (free text, candidates are mere suggestions).
        let create = match completion {
            transient::Completion::OneOf(_) => CreateMode::None,
            _ => CreateMode::Value,
        };
        // Candidates available synchronously (a fixed set); git-backed sources
        // load below, off the UI thread, so opening stays instant in big repos.
        let initial: Vec<String> = match completion {
            transient::Completion::OneOf(values) => values.iter().map(|v| v.to_string()).collect(),
            _ => Vec::new(),
        };
        self.open_picker(
            PickerAction::SetOption { key, description },
            initial,
            create,
            Vec::new(),
            window,
            cx,
        );
        if let Some(Popup::RemotePicker(p)) = self.popup.as_mut() {
            p.resume = Some(Box::new(resume));
            // A free-text value with no completion candidates (e.g. `-n`) has no
            // candidate list — collapse it to just the input + hints.
            p.reserve_candidates = !matches!(completion, transient::Completion::None);
        }

        // Load git-backed candidates (authors, tracked files) asynchronously and
        // drop them into the open picker — `git ls-files` can be large/slow.
        let loader: Option<fn(&Repo) -> Vec<String>> = match completion {
            transient::Completion::Authors => Some(|r| r.authors().unwrap_or_default()),
            transient::Completion::Files => Some(|r| r.tracked_files().unwrap_or_default()),
            _ => None,
        };
        if let (Some(load), Some(repo)) = (loader, self.repo.clone()) {
            cx.spawn(async move |this, cx| {
                let items = cx
                    .background_executor()
                    .spawn(async move { load(&repo) })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(Popup::RemotePicker(p)) = this.popup.as_mut() {
                        // Only the still-open value prompt; ignore if dismissed.
                        if matches!(p.action, PickerAction::SetOption { .. }) {
                            p.list
                                .set_choices(items.into_iter().map(SharedString::from).collect());
                            cx.notify();
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Prompt for a ref to log (`l o`), carrying the gathered flags, pathspecs,
    /// and limit through so they apply once the ref is chosen.
    fn prompt_log_ref(
        &mut self,
        flags: Vec<String>,
        paths: Vec<String>,
        limit: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let mut choices = repo.local_branches().unwrap_or_default();
        choices.extend(repo.remote_branches().unwrap_or_default());
        self.open_picker(
            PickerAction::LogRef {
                flags,
                paths,
                limit,
            },
            choices,
            CreateMode::Any,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Open the picker for a branch-transient command: checkout/rename/delete
    /// pick an existing branch; create reads a new name (free text).
    fn dispatch_branch(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        use transient::Command::*;
        let (action, choices, create) = match command {
            BranchCheckout => {
                let mut choices = repo.local_branches().unwrap_or_default();
                choices.extend(repo.remote_branches().unwrap_or_default());
                (BranchAction::Checkout, choices, CreateMode::None)
            }
            BranchCreateCheckout => (
                BranchAction::Create { checkout: true },
                Vec::new(),
                CreateMode::Any,
            ),
            BranchCreate => (
                BranchAction::Create { checkout: false },
                Vec::new(),
                CreateMode::Any,
            ),
            BranchRename => (
                BranchAction::RenameFrom,
                repo.local_branches().unwrap_or_default(),
                CreateMode::None,
            ),
            BranchDelete => (
                BranchAction::Delete,
                repo.local_branches().unwrap_or_default(),
                CreateMode::None,
            ),
            _ => return,
        };
        self.open_picker(
            PickerAction::Branch(action),
            choices,
            create,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Begin an amend/reword/extend, first checking (off the UI thread) whether
    /// HEAD has already been pushed; if so, confirm before rewriting published
    /// history (mirrors magit's `magit-commit-amend-assert`).
    fn begin_history_rewrite(
        &mut self,
        command: transient::Command,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let branches = cx
                .background_executor()
                .spawn(async move { repo.published_branches("HEAD").unwrap_or_default() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if branches.is_empty() {
                    this.proceed_history_rewrite(command, switches, window, cx);
                    return;
                }
                let verb = match command {
                    transient::Command::CommitReword => "Reword",
                    transient::Command::CommitExtend => "Extend",
                    _ => "Amend",
                };
                let target = match branches.as_slice() {
                    [one] => one.clone(),
                    many => format!("{} remote branches", many.len()),
                };
                this.confirm = Some((
                    format!("This commit has already been pushed to {target}. {verb} it anyway?"),
                    Confirm::AmendPushed(command, switches),
                ));
                cx.notify();
            });
        })
        .detach();
    }

    /// Carry out an amend/reword/extend (after any published-history warning):
    /// amend/reword open the message editor; extend commits straight away.
    fn proceed_history_rewrite(
        &mut self,
        command: transient::Command,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            transient::Command::CommitAmend => {
                self.open_editor(CommitMode::Amend, switches, window, cx)
            }
            transient::Command::CommitReword => {
                self.open_editor(CommitMode::Reword, switches, window, cx)
            }
            _ => self.run_command(command, switches, cx),
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
                this.report(command_done(command), result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    // --- Push / pull / fetch --------------------------------------------

    /// Resolve a push/pull/fetch command to a concrete remote and run it: use
    /// the configured push-remote/upstream when present, otherwise pick a remote
    /// (prompting only when there's a real choice) — setting the relevant config
    /// for first push, like magit.
    fn dispatch_transfer(
        &mut self,
        command: transient::Command,
        targets: &RemoteTargets,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        // Push/pull need the current branch; fetch doesn't.
        let needs_branch = !matches!(
            command,
            FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere
        );
        if needs_branch && targets.branch.is_none() {
            self.status_message = Some("HEAD is detached — can't push/pull a branch".to_string());
            cx.notify();
            return;
        }
        let branch = targets.branch.clone().unwrap_or_default();
        match command {
            PushPushRemote => {
                let t = Transfer::Push {
                    branch,
                    set_upstream: false,
                    save_push_remote: targets.push_remote.is_none(),
                };
                self.resolve_remote(t, targets.push_remote.clone(), switches, window, cx);
            }
            PushUpstream => {
                let t = Transfer::Push {
                    branch,
                    set_upstream: targets.upstream.is_none(),
                    save_push_remote: false,
                };
                let remote = targets.upstream.as_ref().map(|u| u.remote.clone());
                self.resolve_remote(t, remote, switches, window, cx);
            }
            PushElsewhere => {
                // Choose (or type a new) remote branch to push the current
                // branch to.
                self.prompt_branch(Transfer::PushRef { branch }, true, switches, window, cx);
            }
            PullPushRemote => self.resolve_remote(
                Transfer::Pull { branch },
                targets.push_remote.clone(),
                switches,
                window,
                cx,
            ),
            PullUpstream => match &targets.upstream {
                Some(u) => self.run_transfer(
                    Transfer::Pull {
                        branch: u.branch.clone(),
                    },
                    u.remote.clone(),
                    switches,
                    cx,
                ),
                None => self.resolve_remote(Transfer::Pull { branch }, None, switches, window, cx),
            },
            // Pull an existing remote branch (no create — can't pull a new one).
            PullElsewhere => self.prompt_branch(Transfer::PullRef, false, switches, window, cx),
            FetchPushRemote => self.resolve_remote(
                Transfer::Fetch,
                targets.push_remote.clone(),
                switches,
                window,
                cx,
            ),
            FetchUpstream => {
                let remote = targets.upstream.as_ref().map(|u| u.remote.clone());
                self.resolve_remote(Transfer::Fetch, remote, switches, window, cx);
            }
            FetchAll => self.run_fetch_all(switches, cx),
            FetchElsewhere => self.prompt_remote(Transfer::Fetch, switches, window, cx),
            _ => {}
        }
    }

    /// Run `transfer` against `remote` if known; otherwise pick one — using the
    /// sole remote directly, prompting only when several exist.
    fn resolve_remote(
        &mut self,
        transfer: Transfer,
        remote: Option<String>,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(remote) = remote {
            self.run_transfer(transfer, remote, switches, cx);
            return;
        }
        let mut remotes = self
            .repo
            .as_ref()
            .and_then(|r| r.remotes().ok())
            .unwrap_or_default();
        match remotes.len() {
            0 => {
                self.status_message = Some("No remotes configured".to_string());
                cx.notify();
            }
            1 => self.run_transfer(transfer, remotes.into_iter().next().unwrap(), switches, cx),
            _ => {
                remotes.sort_by_key(|r| r != "origin");
                self.open_picker(
                    PickerAction::Transfer(transfer),
                    remotes,
                    CreateMode::None,
                    switches,
                    window,
                    cx,
                )
            }
        }
    }

    /// Always show the remote picker for a pending transfer (the "elsewhere"
    /// fetch, where the point is to choose) — even with a single remote.
    fn prompt_remote(
        &mut self,
        transfer: Transfer,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut remotes = self
            .repo
            .as_ref()
            .and_then(|r| r.remotes().ok())
            .unwrap_or_default();
        if remotes.is_empty() {
            self.status_message = Some("No remotes configured".to_string());
            cx.notify();
            return;
        }
        remotes.sort_by_key(|r| r != "origin");
        self.open_picker(
            PickerAction::Transfer(transfer),
            remotes,
            CreateMode::None,
            switches,
            window,
            cx,
        );
    }

    /// Show the remote-*branch* picker for a push/pull "elsewhere" (magit's
    /// ref-level target). `create` allows pushing to a freshly-typed branch.
    fn prompt_branch(
        &mut self,
        transfer: Transfer,
        create: bool,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let remotes = repo.remotes().unwrap_or_default();
        if remotes.is_empty() {
            self.status_message = Some("No remotes configured".to_string());
            cx.notify();
            return;
        }
        let existing = repo.remote_branches().unwrap_or_default();
        // Pull lists only existing branches (you can't pull one that doesn't
        // exist). Push seeds the same-named target on every remote — like magit —
        // so `origin/<current>` is always a normal candidate, existing or not.
        let choices = match &transfer {
            Transfer::PushRef { branch } if create => {
                seed_push_branches(repo, &remotes, branch, existing)
            }
            _ => existing,
        };
        let create_mode = if create {
            CreateMode::RemoteBranch
        } else {
            CreateMode::None
        };
        self.open_picker(
            PickerAction::Transfer(transfer),
            choices,
            create_mode,
            switches,
            window,
            cx,
        );
    }

    /// Open the vertico-style picker for a pending action. The query input is
    /// focused on appear, so it's type-to-filter immediately; the model re-ranks
    /// on every change.
    fn open_picker(
        &mut self,
        action: PickerAction,
        choices: Vec<String>,
        create: CreateMode,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = action.prompt();
        let items: Vec<SharedString> = choices.into_iter().map(SharedString::from).collect();
        // Reserve the candidate area only when there's actually a list to match
        // against. A picker with no choices (e.g. creating a branch — you type a
        // new name) is pure entry: no candidate area, no "No match". The async
        // completion prompts start empty but opt back in via `open_option_prompt`.
        let has_candidates = !items.is_empty();
        let input = cx.new(|cx| InputState::new(window, cx));
        // Re-filter as the query changes (Up/Down/Enter/Esc are handled in the
        // capture phase, so the input only ever sees text edits here).
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    if let Some(Popup::RemotePicker(p)) = this.popup.as_mut() {
                        p.list.set_query(&query);
                        p.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                        cx.notify();
                    }
                }
            },
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.popup = Some(Popup::RemotePicker(RemotePickerState {
            prompt,
            input,
            list: PickerList::new(items, create),
            scroll: UniformListScrollHandle::new(),
            action,
            switches,
            reserve_candidates: has_candidates,
            resume: None,
            _sub: sub,
        }));
        cx.notify();
    }

    /// Run the pending action against the candidate currently highlighted in the
    /// picker (Enter, a row click, or the kbd button).
    fn confirm_remote_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = match &self.popup {
            Some(Popup::RemotePicker(p)) => p.list.selected_choice(),
            _ => None,
        };
        let Some(chosen) = chosen else { return };
        if let Some(Popup::RemotePicker(p)) = self.popup.take() {
            match p.action {
                PickerAction::Transfer(t) => {
                    self.run_transfer(t, chosen.to_string(), p.switches, cx)
                }
                PickerAction::Branch(b) => {
                    self.run_branch_action(b, chosen.to_string(), window, cx)
                }
                PickerAction::Stash(s) => self.run_stash_action(s, chosen.to_string(), cx),
                // Set the option value (empty clears it) and reopen the transient.
                PickerAction::SetOption { key, .. } => {
                    if let Some(mut ts) = p.resume {
                        let value = chosen.to_string();
                        if value.trim().is_empty() {
                            ts.values.remove(&key);
                        } else {
                            ts.values.insert(key, value);
                        }
                        self.popup = Some(Popup::Transient(*ts));
                        cx.notify();
                    }
                }
                PickerAction::LogRef {
                    flags,
                    paths,
                    limit,
                } => {
                    let args =
                        build_log_args(flags, LogScope::Ref(chosen.to_string()), paths, limit);
                    self.start_log(args, cx);
                }
                // Resolve the chosen title back to its command and run it.
                PickerAction::RunCommand => {
                    if let Some(cmd) = commands().iter().find(|c| c.title == chosen.as_ref()) {
                        self.record_use(cmd.id);
                        (cmd.run)(self, window, cx);
                    }
                }
            }
        }
    }

    /// Move the picker highlight by `delta` rows (Up/Down), keeping it in view.
    fn picker_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(Popup::RemotePicker(p)) = self.popup.as_mut() {
            p.list.move_by(delta);
            p.scroll
                .scroll_to_item(p.list.selected(), gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Show a status-bar message. A `transient` one (a success notice) fades on
    /// its own after a moment; a sticky one (an error) stays until dismissed
    /// (Esc / click). Either way it can always be dismissed manually.
    fn set_status(&mut self, msg: String, transient: bool, cx: &mut Context<Self>) {
        self.status_seq = self.status_seq.wrapping_add(1);
        let seq = self.status_seq;
        self.status_message = Some(msg);
        cx.notify();
        if transient {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(STATUS_FADE_SECS))
                    .await;
                this.update(cx, |this, cx| {
                    // Only clear if no newer message has replaced it.
                    if this.status_seq == seq {
                        this.status_message = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Report a git operation's outcome: on success a brief `success` notice
    /// that auto-dismisses (we don't echo git's stderr); on failure the error,
    /// which sticks until dismissed.
    fn report(
        &mut self,
        success: &str,
        result: magritte_core::Result<String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => self.set_status(success.to_string(), true, cx),
            Err(e) => self.set_status(format!("error: {e}"), false, cx),
        }
    }

    /// Run a resolved push/pull/fetch on the background executor, then refresh.
    /// `chosen` is a remote name for the remote-level transfers, or a
    /// `remote/branch` ref (possibly newly typed) for the `*Ref` ones.
    fn run_transfer(
        &mut self,
        transfer: Transfer,
        chosen: String,
        switches: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.status_message = Some(format!("{}…", transfer.verb()));
        let done = match &transfer {
            Transfer::Push { .. } | Transfer::PushRef { .. } => "Pushed",
            Transfer::Pull { .. } | Transfer::PullRef => "Pulled",
            Transfer::Fetch => "Fetched",
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match transfer {
                        Transfer::Push {
                            branch,
                            set_upstream,
                            save_push_remote,
                        } => {
                            if save_push_remote {
                                let _ = repo.set_push_remote(&branch, &chosen);
                            }
                            repo.push_to(&chosen, &branch, set_upstream, &switches)
                        }
                        Transfer::PushRef { branch } => {
                            let (remote, target) = split_ref(&repo, &chosen);
                            repo.push_ref(&remote, &branch, &target, &switches)
                        }
                        Transfer::Pull { branch } => repo.pull_from(&chosen, &branch, &switches),
                        Transfer::PullRef => {
                            let (remote, branch) = split_ref(&repo, &chosen);
                            repo.pull_from(&remote, &branch, &switches)
                        }
                        Transfer::Fetch => repo.fetch_from(&chosen, &switches),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// `git fetch --all` (no remote needed).
    fn run_fetch_all(&mut self, switches: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.status_message = Some("Fetching…".to_string());
        let done = "Fetched";
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.fetch_all(&switches) })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Carry out a branch-transient action against the chosen branch/name.
    /// Rename is two-step: step 1 (`RenameFrom`) opens the name prompt rather
    /// than running git.
    fn run_branch_action(
        &mut self,
        action: BranchAction,
        chosen: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Step 1 of rename: the chosen branch is the one to rename — now prompt
        // for the new name (free text).
        if let BranchAction::RenameFrom = action {
            self.open_picker(
                PickerAction::Branch(BranchAction::RenameTo { old: chosen }),
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            );
            return;
        }

        let Some(repo) = self.repo.clone() else {
            return;
        };
        let verb = match &action {
            BranchAction::Checkout => "Checking out",
            BranchAction::Create { .. } => "Creating branch",
            BranchAction::RenameTo { .. } => "Renaming branch",
            BranchAction::Delete => "Deleting branch",
            BranchAction::RenameFrom => unreachable!("handled above"),
        };
        let done = match &action {
            BranchAction::Checkout => "Checked out",
            BranchAction::Create { .. } => "Created branch",
            BranchAction::RenameTo { .. } => "Renamed branch",
            BranchAction::Delete => "Deleted branch",
            BranchAction::RenameFrom => "Done",
        };
        self.status_message = Some(format!("{verb}…"));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match action {
                        BranchAction::Checkout => repo.checkout(&chosen),
                        BranchAction::Create { checkout: true } => {
                            repo.create_and_checkout(&chosen, None)
                        }
                        BranchAction::Create { checkout: false } => {
                            repo.create_branch(&chosen, None)
                        }
                        BranchAction::RenameTo { old } => repo.rename_branch(&old, &chosen),
                        BranchAction::Delete => repo.delete_branch(&chosen, false),
                        BranchAction::RenameFrom => unreachable!("handled above"),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Stash the working tree and index (`Z z` / `Z Z`), on the background
    /// executor, then refresh.
    fn run_stash_push(&mut self, include_untracked: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.status_message = Some("Stashing…".to_string());
        let done = "Stashed";
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.stash_push(None, include_untracked) })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Apply / pop / drop the chosen stash (`chosen` is the picker's display
    /// string; the `stash@{N}` reference is its first token).
    fn run_stash_action(&mut self, action: StashAction, chosen: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let reference = chosen
            .split_whitespace()
            .next()
            .unwrap_or(&chosen)
            .to_string();
        let verb = match action {
            StashAction::Apply => "Applying stash",
            StashAction::Pop => "Popping stash",
            StashAction::Drop => "Dropping stash",
        };
        let done = match action {
            StashAction::Apply => "Applied stash",
            StashAction::Pop => "Popped stash",
            StashAction::Drop => "Dropped stash",
        };
        self.status_message = Some(format!("{verb}…"));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match action {
                        StashAction::Apply => repo.stash_apply(&reference),
                        StashAction::Pop => repo.stash_pop(&reference),
                        StashAction::Drop => repo.stash_drop(&reference),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
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
                let offset = s.cursor();
                if let Some(wrapped) = wrap_at_cursor(&value, offset, COMMIT_BODY_WIDTH) {
                    // Wrapping only turns a space into a newline, so the cursor's
                    // byte offset is unchanged — recompute its line/column in the
                    // rewrapped text and restore it.
                    s.set_value(wrapped.clone(), window, cx);
                    s.set_cursor_position(byte_offset_to_position(&wrapped, offset), window, cx);
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
                let end = reflowed.len(); // byte offset of the end
                s.set_value(reflowed.clone(), window, cx);
                s.set_cursor_position(byte_offset_to_position(&reflowed, end), window, cx);
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
                        if let Some(ed) = this.editor.as_mut() {
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
    /// show the staged diff being committed (or, with `--all` and nothing
    /// staged, the unstaged changes that will be); reword shows the diff of the
    /// commit it's renaming (HEAD's own changes), since it makes no tree change.
    fn load_commit_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(ed) = self.editor.as_ref() else {
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
                    } else {
                        repo.diff_all(DiffSource::Staged).and_then(|staged| {
                            if staged.is_empty() && also_unstaged {
                                repo.diff_all(DiffSource::Unstaged)
                            } else {
                                Ok(staged)
                            }
                        })
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
                if this.editor.is_none() {
                    return; // editor closed before the diff loaded
                }
                if let Some(err) = error {
                    if let Some(ed) = this.editor.as_mut() {
                        ed.diff = vec![CommitDiffRow::Note(format!("diff unavailable: {err}"))];
                    }
                    cx.notify();
                    return;
                }
                let rows = this.diff_rows(&files, cx);
                if let Some(ed) = this.editor.as_mut() {
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
        if matches!(self.popup, Some(Popup::RemotePicker(_))) {
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
                    self.confirm_remote_picker(window, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    self.cancel_popup(window, cx);
                }
                _ => {}
            }
            return;
        }

        if self.editor.is_none() {
            return;
        }
        // C-g cancels here too; C-n/C-p are left to the Input for cursor motion.
        let key = match event.keystroke.key.as_str() {
            "g" if event.keystroke.modifiers.control => "escape",
            k => k,
        };
        // While the "discard message?" confirmation is up, capture y / n / esc.
        if self.editor.as_ref().is_some_and(|e| e.confirming_cancel) {
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
        let dirty = match &self.editor {
            Some(ed) => ed.state.read(cx).value().trim() != ed.initial.trim(),
            None => return,
        };
        if dirty {
            if let Some(ed) = self.editor.as_mut() {
                ed.confirming_cancel = true;
            }
            cx.notify();
        } else {
            self.discard_editor(window, cx);
        }
    }

    /// Close the editor, discarding its message.
    fn discard_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the discard confirmation and keep editing.
    fn keep_editing(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.editor.as_mut() {
            ed.confirming_cancel = false;
        }
        cx.notify();
    }

    /// Open the live settings screen: four `Select` dropdowns (appearance,
    /// light theme, dark theme, font), each applying its selection immediately.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut theme_names: Vec<SharedString> = gpui_component::ThemeRegistry::global(cx)
            .sorted_themes()
            .iter()
            .map(|t| t.name.clone())
            // gpui-component always seeds its built-in "Default Light/Dark", which
            // we can't remove from the registry — hide them so only our authored
            // themes are offered.
            .filter(|n| n.as_ref() != "Default Light" && n.as_ref() != "Default Dark")
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
        self.editors = text_editors();
        // Lead with a "System Default" entry (maps to an empty config value, so
        // it follows the OS monospace); the rest are concrete families.
        let mut font_items: Vec<SharedString> = vec![SharedString::from(SYSTEM_FONT_LABEL)];
        font_items.extend(self.mono_fonts.iter().cloned());
        let font_ix = if self.config.font.is_empty() {
            0
        } else {
            pos(&font_items, self.config.font.as_str())
        };

        if self.ui_fonts.is_empty() {
            self.ui_fonts = all_font_names(cx);
        }
        // Lead with "Same as monospace" (empty config = the monospace UI we had
        // before opting in) and "System Default" (the platform proportional
        // font); the rest are concrete families.
        let mut ui_font_items: Vec<SharedString> = vec![
            SharedString::from(UI_FONT_DEFAULT_LABEL),
            SharedString::from(SYSTEM_FONT_LABEL),
        ];
        ui_font_items.extend(self.ui_fonts.iter().cloned());
        let ui_font_ix = match self.config.ui_font.as_str() {
            "" => 0,
            SYSTEM_UI_FONT => 1,
            name => pos(&ui_font_items, name),
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
        let ui_font = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(ui_font_items),
                row(ui_font_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        // macOS: a dropdown of detected editor apps, led by "System Default"
        // (open in the OS default app). A command set via the config file that
        // isn't a detected app is injected so it stays selectable, not lost.
        #[cfg(target_os = "macos")]
        let editor = {
            let cur = self.config.editor.trim().to_string();
            let mut editor_items: Vec<SharedString> =
                vec![SharedString::from(EDITOR_OS_DEFAULT_LABEL)];
            if !cur.is_empty() && !self.editors.iter().any(|(n, _)| n.as_ref() == cur) {
                editor_items.push(SharedString::from(cur.clone()));
            }
            editor_items.extend(self.editors.iter().map(|(n, _)| n.clone()));
            let editor_ix = if cur.is_empty() {
                0
            } else {
                editor_items
                    .iter()
                    .position(|n| n.as_ref() == cur)
                    .unwrap_or(0)
            };
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(editor_items),
                    row(editor_ix),
                    &mut *window,
                    cx,
                )
                .searchable(true)
            })
        };
        #[cfg(not(target_os = "macos"))]
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. code -w, zed (OS default if empty)")
                .default_value(self.config.editor.clone())
        });

        let subs = vec![
            #[cfg(target_os = "macos")]
            cx.subscribe_in(
                &editor,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, _cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.config.editor = if name.as_ref() == EDITOR_OS_DEFAULT_LABEL {
                            String::new()
                        } else {
                            name.to_string()
                        };
                        config::save(&this.config);
                    }
                },
            ),
            #[cfg(not(target_os = "macos"))]
            cx.subscribe_in(&editor, window, |this, input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.config.editor = input.read(cx).value().trim().to_string();
                    config::save(&this.config);
                }
            }),
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
                        // The UI font may track the editor font ("Same as
                        // editor"), so re-resolve it too.
                        this.ui_font = resolve_ui_font(&this.config, cx);
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &ui_font,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.config.ui_font = match name.as_ref() {
                            // Reuse the monospace font (no proportional UI).
                            UI_FONT_DEFAULT_LABEL => String::new(),
                            // Platform proportional UI font.
                            SYSTEM_FONT_LABEL => SYSTEM_UI_FONT.to_string(),
                            other => other.to_string(),
                        };
                        this.ui_font = resolve_ui_font(&this.config, cx);
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
            ui_font,
            editor,
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
        s.focus_ix = (s.focus_ix + 1) % 5;
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
            3 => s.font.clone().update(cx, |st, cx| st.focus(window, cx)),
            _ => s.ui_font.clone().update(cx, |st, cx| st.focus(window, cx)),
        }
    }

    /// Close the settings screen, persisting and returning focus to the list.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        config::save(&self.config);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Open the git command-log view (magit's `$` process buffer), scrolled to
    /// the most recent command.
    fn open_git_log(&mut self, cx: &mut Context<Self>) {
        let scroll = UniformListScrollHandle::new();
        let last = self.git_log_rows().len().saturating_sub(1);
        scroll.scroll_to_item(last, gpui::ScrollStrategy::Bottom);
        self.git_log = Some(ScrollView { scroll, top: last });
        cx.notify();
    }

    fn close_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_log = None;
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
        self.show_log_loading(cx);
        cx.spawn(async move |this, cx| {
            let entries = cx
                .background_executor()
                .spawn(async move { repo.log_with(&args).unwrap_or_default() })
                .await;
            this.update(cx, |this, cx| this.fill_log(entries, cx)).ok();
        })
        .detach();
    }

    /// Open the reflog view (`l r`).
    fn start_reflog(&mut self, limit: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.show_log_loading(cx);
        cx.spawn(async move |this, cx| {
            let entries = cx
                .background_executor()
                .spawn(async move { repo.reflog(limit).unwrap_or_default() })
                .await;
            this.update(cx, |this, cx| this.fill_log(entries, cx)).ok();
        })
        .detach();
    }

    /// Show the (empty) log view immediately while commits load.
    fn show_log_loading(&mut self, cx: &mut Context<Self>) {
        self.log = Some(LogState {
            entries: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
        });
        cx.notify();
    }

    /// Fill the open log view with loaded entries.
    fn fill_log(&mut self, entries: Vec<magritte_core::LogEntry>, cx: &mut Context<Self>) {
        if let Some(log) = self.log.as_mut() {
            log.entries = entries;
        }
        cx.notify();
    }

    fn close_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.log = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Move the log's selection by `delta`, keeping it in view.
    fn log_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(log) = self.log.as_mut() {
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
    fn open_commit_view(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(entry) = self
            .log
            .as_ref()
            .and_then(|l| l.entries.get(l.selected).cloned())
        else {
            return;
        };
        let rev = entry.short_hash.clone();
        self.commit_view = Some(CommitView {
            title: SharedString::from(format!("{}  {}", entry.short_hash, entry.subject)),
            rows: vec![CommitDiffRow::Note("Loading…".to_string())],
            scroll: UniformListScrollHandle::new(),
            top: 0,
        });
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
                if this.commit_view.is_none() {
                    return; // closed before the diff loaded
                }
                let rows = match loaded {
                    Ok(files) => this.diff_rows(&files, cx),
                    Err(e) => vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))],
                };
                if let Some(cv) = this.commit_view.as_mut() {
                    cv.rows = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_view = None;
        // Return focus to the status root; the log view (still open) handles keys.
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
                this.report("Committed", result, cx);
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
        let mut ctrl = event.keystroke.modifiers.control;
        let alt = event.keystroke.modifiers.alt;

        // Emacs aliases, normalized up front so every downstream handler gets
        // them for free: C-g is the universal cancel (= Escape), and C-n/C-p
        // move down/up (= j/k) wherever those motions apply.
        let key = match key.as_str() {
            "g" if ctrl => {
                ctrl = false;
                "escape".to_string()
            }
            "n" if ctrl => {
                ctrl = false;
                "j".to_string()
            }
            "p" if ctrl => {
                ctrl = false;
                "k".to_string()
            }
            _ => key,
        };

        // Emacs C-x C-c quits. C-x starts a prefix (like the `g` prefix); the
        // next key resolves or cancels it. Handled before the modal branches so
        // it works from any view.
        if self.pending_cx {
            self.pending_cx = false;
            if ctrl && key == "c" {
                cx.quit();
            }
            return;
        }
        if ctrl && key == "x" {
            self.pending_cx = true;
            return;
        }

        // While settings is open the focused Select handles keys; we only watch
        // for Esc (when no dropdown menu is open) to close the screen. Tab is
        // delivered via the ToggleFold action.
        if self.settings.is_some() {
            if key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }

        // The git command-log view takes over the window; esc/q/$ close it, and
        // it scrolls with the usual vi/less keys.
        if self.git_log.is_some() {
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
            if let Some(sv) = self.git_log.as_mut() {
                apply_scroll_key(&sv.scroll, &mut sv.top, len, &key, shift, ctrl, page);
            }
            cx.notify();
            return;
        }

        // A commit's diff detail (opened from the log) is topmost; esc/q returns
        // to the log, and it scrolls with the usual vi/less keys.
        if self.commit_view.is_some() {
            if key == "escape" || key == "q" {
                self.close_commit_view(window, cx);
                return;
            }
            let page = page_rows(window);
            if let Some(cv) = self.commit_view.as_mut() {
                let len = cv.rows.len();
                apply_scroll_key(&cv.scroll, &mut cv.top, len, &key, shift, ctrl, page);
            }
            cx.notify();
            return;
        }

        // The commit-log view: Enter opens the commit; esc/q close; the vi/less
        // motion keys move the *selection* (paging by a screenful, g/G to ends).
        if self.log.is_some() {
            let page = page_rows(window) as isize;
            let half = (page / 2).max(1);
            match key.as_str() {
                "escape" | "q" => self.close_log(window, cx),
                "enter" => self.open_commit_view(cx),
                "j" => self.log_move(1, cx),
                "k" => self.log_move(-1, cx),
                "d" if ctrl => self.log_move(half, cx),
                "u" if ctrl => self.log_move(-half, cx),
                "space" => self.log_move(page, cx),
                "f" if ctrl => self.log_move(page, cx),
                "b" if ctrl => self.log_move(-page, cx),
                "g" if shift => self.log_move(isize::MAX / 2, cx), // G → bottom
                "g" => self.log_move(isize::MIN / 2, cx),          // g → top
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
        if matches!(self.popup, Some(Popup::RemotePicker(_))) {
            return;
        }

        // The `?` dispatch popup is modal (like magit's dispatch): a command
        // key runs that command, esc/q/? close it, other keys are ignored.
        if matches!(self.popup, Some(Popup::Dispatch(_))) {
            if self.pending_g {
                self.pending_g = false;
                match key.as_str() {
                    "r" => self.run_dispatch("g r", window, cx),
                    "g" => self.run_dispatch("g g", window, cx),
                    "j" => self.run_dispatch("g j", window, cx),
                    "k" => self.run_dispatch("g k", window, cx),
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

        // Page-motion size for Ctrl-d/u/f/b (a screenful of rows).
        let page = page_rows(window) as isize;
        let half = (page / 2).max(1);
        match key.as_str() {
            "j" => self.move_selection(1),
            "k" => self.move_selection(-1),
            // Vi-style paging (kept off the `?` menu — a scroll convenience).
            "d" if ctrl => self.page_selection(half),
            "u" if ctrl => self.page_selection(-half),
            "f" if ctrl => self.page_selection(page),
            "b" if ctrl => self.page_selection(-page),
            "g" if shift => self.select_edge(true), // G
            "g" => {
                self.pending_g = true;
                return;
            }
            // Tab is delivered via the ToggleFold action (Root binds tab), but
            // keep this as a fallback for any path that reaches on_key.
            "tab" => self.toggle_fold(cx),
            // Visual (region) selection; Escape cancels.
            "v" => return self.invoke_command("visual", window, cx),
            "escape" => {
                // Cancel a visual selection, else dismiss the status/error
                // banner if one is showing.
                if self.visual.take().is_some() || self.status_message.take().is_some() {
                    cx.notify();
                }
                return;
            }
            // Open the file at point in the external editor.
            "enter" => return self.invoke_command("open-file", window, cx),
            // Commands, dispatched through the registry so behavior lives in one
            // place (see `commands`). The shift/`g`-prefix decoding stays here;
            // the registry only sees the resolved command id.
            "s" if shift => return self.invoke_command("stage-all", window, cx),
            "s" => return self.invoke_command("stage", window, cx),
            "u" if shift => return self.invoke_command("unstage-all", window, cx),
            "u" => return self.invoke_command("unstage", window, cx),
            // M-x (Alt-x) opens the palette too, alongside `:`.
            "x" if alt => return self.open_command_palette(window, cx),
            "x" => return self.invoke_command("discard", window, cx),
            "c" => return self.invoke_command("commit", window, cx),
            "b" => return self.invoke_command("branch", window, cx),
            "z" if shift => return self.invoke_command("stash", window, cx),
            "l" => return self.invoke_command("log", window, cx),
            // Sync transients (evil-collection magit): p push, F pull, f fetch.
            "p" => return self.invoke_command("push", window, cx),
            "f" if shift => return self.invoke_command("pull", window, cx),
            "f" => return self.invoke_command("fetch", window, cx),
            "," => return self.invoke_command("settings", window, cx),
            "$" => return self.invoke_command("git-log", window, cx),
            "4" if shift => return self.invoke_command("git-log", window, cx),
            // The `:` command palette (M-x style). May arrive as ";" + shift.
            ":" => return self.open_command_palette(window, cx),
            ";" if shift => return self.open_command_palette(window, cx),
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
        // A registry command (resolved by its key), the `:` palette, or a motion.
        if let Some(cmd) = commands().iter().find(|c| c.key == Some(key)) {
            (cmd.run)(self, window, cx);
            return;
        }
        if key == ":" {
            self.open_command_palette(window, cx);
            return;
        }
        match key {
            "j" => self.move_selection(1),
            "k" => self.move_selection(-1),
            "g g" => self.select_edge(false),
            "G" => self.select_edge(true),
            "g j" => self.select_section(true),
            "g k" => self.select_section(false),
            _ => {}
        }
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Invoke a registry [`Command`] by id — the keymap's bridge to the
    /// registry, so the command's behavior lives in exactly one place.
    fn invoke_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cmd) = commands().iter().find(|c| c.id == id) {
            (cmd.run)(self, window, cx);
        }
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
        let mut cmds: Vec<&Command> = commands()
            .iter()
            .filter(|c| c.palette && (c.enabled)(self))
            .collect();
        cmds.sort_by(|a, b| {
            let (sa, sb) = (self.usage.score(a.id), self.usage.score(b.id));
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let choices: Vec<String> = cmds.iter().map(|c| c.title.to_string()).collect();
        self.open_picker(
            PickerAction::RunCommand,
            choices,
            CreateMode::None,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Whether `key` is a single-stroke `?`-dispatch key. Registry command keys
    /// route to their command; the bare motions `j`/`k`/`G` move the selection.
    /// Multi-stroke entries are handled elsewhere — Tab via the ToggleFold
    /// action, and `g r`/`g g`/`g j`/`g k` via the g-prefix — so they're excluded.
    fn is_dispatch_key(key: &str) -> bool {
        if matches!(key, "tab" | "g r" | "g g" | "g j" | "g k") {
            return false;
        }
        commands().iter().any(|c| c.key == Some(key)) || matches!(key, "j" | "k" | "G" | ":")
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
    fn render_remote_picker(&self, state: &RemotePickerState, view: &Entity<Self>) -> gpui::Div {
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
            // Candidates exist, but none match the query: a quiet "No match"
            // line in the first row, keeping the reserved height so nothing
            // shifts (the line stays at the top rather than floating mid-panel).
            div()
                .h(list_height)
                .child(
                    div()
                        .h(px(ROW_HEIGHT))
                        .pl(px(ROW_PAD_LEFT))
                        .flex()
                        .items_center()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("No match")),
                )
                .into_any_element()
        } else {
            uniform_list("picker-rows", rows, {
                let view = view.clone();
                move |range, _window, cx| match &view.read(cx).popup {
                    Some(Popup::RemotePicker(p)) => {
                        // In the command palette, show each command's keybinding
                        // (when it has one) on the right, so it doubles as help.
                        let palette = matches!(p.action, PickerAction::RunCommand);
                        range
                            .map(|ix| match p.list.row(ix) {
                                Some(r) => {
                                    let hint = palette
                                        .then(|| command_keys(&r.label))
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
                        Self::confirm_remote_picker,
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
                    if let Some(Popup::RemotePicker(p)) = this.popup.as_mut() {
                        p.list.set_selected(ix);
                    }
                    this.confirm_remote_picker(window, vcx);
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
                    .child(SharedString::from(format_keys(&seq))),
            );
        }
        el.into_any_element()
    }

    /// Close the open picker. If it was prompting for a transient option value,
    /// reopen that transient unchanged rather than dismissing everything.
    fn cancel_popup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Popup::RemotePicker(p)) = self.popup.take() {
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
            command_row = command_row.child(self.render_group(group, 1, state, pending_dash, view));
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
                    .child(switch_chip(
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
                    .child(switch_chip(
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
                    .child(key_chip(a.key, self.palette.dim, &self.font))
                    .child(self.hover_label(&a.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
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

    /// Render a key spec as a single keycap. A multi-keystroke sequence (e.g.
    /// `g r`) keeps its keys spaced *inside* the one cap (see [`format_keys`]).
    fn key_tokens(&self, keys: &str) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .child(key_chip(keys, self.palette.dim, &self.font))
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
            .child(key_chip(key, self.palette.dim, &self.font))
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
                args: c.args.join(" "),
                ok: c.ok,
            });
            // stderr progress often uses '\r' to overwrite; split on both so
            // each update is its own line, and drop the blanks.
            for line in c.stderr.split(['\n', '\r']) {
                if !line.trim().is_empty() {
                    rows.push(GitLogRow::Output(line.trim_end().to_string()));
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
            GitLogRow::Command { args, ok } => {
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
                                    .child(SharedString::from("git")),
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

        let body = if count == 0 {
            div()
                .text_color(self.palette.dim)
                .child(SharedString::from("Loading…"))
                .into_any_element()
        } else {
            uniform_list("log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    match this.log.as_ref() {
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
            .into_any_element()
        };

        let mut header = div().flex().items_center().gap_3().child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.palette.section)
                .child(SharedString::from("Log")),
        );
        if capped {
            header = header.child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(format!("(first {})", Self::LOG_LIMIT))),
            );
        }
        header = header.child(self.key_action("log-close", "esc", "close", view, Self::close_log));

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
                    if let Some(log) = this.log.as_mut() {
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
                match this.commit_view.as_ref() {
                    Some(cv) => range
                        .map(|ix| this.render_commit_diff_row(&cv.rows[ix]))
                        .collect::<Vec<_>>(),
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
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(cv.title.clone()),
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

    /// Render the live settings screen as a form of dropdowns. The `Select`
    /// components carry their own mouse + keyboard handling; Tab moves between
    /// them, Esc closes.
    fn render_settings(&self, s: &SettingsState, view: &Entity<Self>) -> gpui::Div {
        // A labelled control row: fixed-width label + the control.
        let field = |id: &'static str, label: &str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(130.0))
                        .flex_shrink_0()
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
        // A titled group: an uppercase heading over a bordered card of rows.
        let section = |title: &str, rows: Vec<gpui::Div>| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .px_1()
                        .text_xs()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(title.to_uppercase())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(self.palette.border)
                        .p_3()
                        .children(rows),
                )
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(620.0))
            .p_4()
            .gap_4()
            .child(
                // Header: title on the left; actions on the right.
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Settings")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.open_config_button(view))
                            .child(self.key_action(
                                "settings-close",
                                "esc",
                                "close",
                                view,
                                Self::close_settings,
                            )),
                    ),
            )
            .child(section(
                "Appearance",
                vec![
                    field(
                        "appearance",
                        "Mode",
                        Select::new(&s.appearance).into_any_element(),
                    ),
                    field(
                        "light-theme",
                        "Light theme",
                        Select::new(&s.light_theme)
                            .search_placeholder("Search themes")
                            .into_any_element(),
                    ),
                    field(
                        "dark-theme",
                        "Dark theme",
                        Select::new(&s.dark_theme)
                            .search_placeholder("Search themes")
                            .into_any_element(),
                    ),
                    field(
                        "font",
                        "Monospace font",
                        Select::new(&s.font)
                            .search_placeholder("Search fonts")
                            .into_any_element(),
                    ),
                    field(
                        "ui-font",
                        "UI font",
                        Select::new(&s.ui_font)
                            .search_placeholder("Search fonts")
                            .into_any_element(),
                    ),
                ],
            ))
            .child(section("Editor", {
                #[cfg(target_os = "macos")]
                let control = Select::new(&s.editor)
                    .search_placeholder("Search editors")
                    .into_any_element();
                #[cfg(not(target_os = "macos"))]
                let control = Input::new(&s.editor).into_any_element();
                vec![field("editor", "External editor", control)]
            }))
            .child(section(
                "Commit editor",
                vec![
                    field(
                        "commit-title-ruler",
                        "Summary ruler",
                        self.toggle_control(
                            "commit-title-ruler",
                            self.config.commit_title_ruler,
                            "Underlines characters past column 50 on the commit summary \
                             (first) line.",
                            view,
                            |cfg, on| cfg.commit_title_ruler = on,
                        ),
                    ),
                    field(
                        "commit-body-wrap",
                        "Body auto-wrap",
                        self.toggle_control(
                            "commit-body-wrap",
                            self.config.commit_body_wrap,
                            "Hard-wraps the commit body at 72 columns as you type at the \
                             end of a line (the summary line is never wrapped).",
                            view,
                            |cfg, on| cfg.commit_body_wrap = on,
                        ),
                    ),
                ],
            ))
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

    /// Write the current config (so the file exists even if untouched) and open
    /// it via the same path as opening a file at point: the configured editor,
    /// falling back to the OS default app when it's unset. (The split button's
    /// dropdown still opens a chosen app.)
    fn open_config_file(&self) {
        if let Some(path) = self.saved_config_path() {
            self.launch_editor(&path);
        }
    }

    /// Open the config file with a specific editor app (a `.app` path on macOS).
    fn open_config_with(&self, app: &str) {
        let Some(path) = self.saved_config_path() else {
            return;
        };
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open")
            .arg("-a")
            .arg(app)
            .arg(&path)
            .spawn();
        #[cfg(not(target_os = "macos"))]
        let _ = std::process::Command::new(app).arg(&path).spawn();
    }

    /// Copy the config file's path to the clipboard.
    fn copy_config_path(&self, cx: &mut Context<Self>) {
        if let Some(path) = config::path() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                path.to_string_lossy().into_owned(),
            ));
        }
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
        let info = div()
            .id(SharedString::from(format!("{id}-info")))
            .relative()
            .child(track_target(format!("{id}-info")))
            .child(
                Icon::new(IconName::Info)
                    .xsmall()
                    .text_color(self.palette.dim),
            )
            // gpui's native tooltip (not the library's managed one) so we can
            // drop the show-delay to zero and bound the width so it wraps. The
            // library tooltip forces the theme's UI font; override it back to
            // our monospace chrome font so it matches the rest of the app.
            .tooltip({
                let font = self.font.clone();
                move |window, cx| {
                    let font = font.clone();
                    Tooltip::element(move |_, _| {
                        div()
                            .max_w(px(280.0))
                            .font_family(font.clone())
                            .child(SharedString::from(explanation))
                    })
                    .build(window, cx)
                }
            })
            .tooltip_show_delay(Duration::ZERO);
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(switch)
            .child(info)
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
            RowKind::Header {
                label,
                value,
                chip,
                detail,
            } => {
                let mut el = el.child(
                    div()
                        .min_w(px(HEADER_LABEL_WIDTH))
                        .text_color(self.palette.dim)
                        .child(SharedString::from(label.clone())),
                );
                el = if *chip {
                    el.child(self.branch_chip(value))
                } else {
                    el.child(
                        div()
                            .text_color(self.palette.fg)
                            .child(SharedString::from(value.clone())),
                    )
                };
                if let Some(detail) = detail {
                    el = el.child(
                        div()
                            .text_color(self.palette.dim)
                            .child(SharedString::from(detail.clone())),
                    );
                }
                el
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
        // Keep keyboard focus on the status view whenever nothing else owns the
        // keyboard (the commit editor, settings, and the remote picker each have
        // their own focused input), so keys always land — including debug-channel
        // keystrokes while the window isn't frontmost.
        let owns_focus_elsewhere = self.editor.is_some()
            || self.settings.is_some()
            || matches!(self.popup, Some(Popup::RemotePicker(_)));
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

        // The settings screen, commit editor, and git-log view each take over
        // the window.
        if let Some(s) = &self.settings {
            return root.child(self.render_settings(s, &view));
        }
        if let Some(ed) = &self.editor {
            return root.child(self.render_editor(ed, &view));
        }
        if let Some(scroll) = &self.git_log {
            return root.child(self.render_git_log(scroll, &view));
        }
        // A commit's diff detail sits above the log; the log above the status.
        if let Some(cv) = &self.commit_view {
            return root.child(self.render_commit_view(cv, &view));
        }
        if let Some(log) = &self.log {
            return root.child(self.render_log(log, &view));
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
                Popup::RemotePicker(state) => self.render_remote_picker(state, &view),
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
            // The status/error banner: click it (or press Esc) to dismiss.
            root = root.child(
                status_bar(
                    msg.clone(),
                    self.palette.panel,
                    self.palette.fg,
                    self.palette.border,
                )
                .id("status-bar")
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.status_message = None;
                    cx.notify();
                })),
            );
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

/// A non-selectable repository header line (Head:/Push:).
fn header_row(label: &str, value: String, chip: bool, detail: Option<String>) -> Row {
    Row {
        indent: 0,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Header {
            label: label.to_string(),
            value,
            chip,
            detail,
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

/// Open `path` with the OS default application for its type.
fn open_with_os(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// The remote a bare (unqualified) branch name targets: the conventional
/// `origin` if present, else the first configured remote, else `origin`.
fn default_remote(repo: &Repo) -> String {
    let remotes = repo.remotes().unwrap_or_default();
    if remotes.iter().any(|r| r == "origin") {
        "origin".to_string()
    } else {
        remotes
            .into_iter()
            .next()
            .unwrap_or_else(|| "origin".to_string())
    }
}

/// The push "elsewhere" candidate list, magit-style: seed `<remote>/<current>`
/// for every remote (existing or not) so the same-named push target is always a
/// normal candidate, then append the existing remote branches. The preferred
/// remote (push-remote if set, else [`default_remote`]) comes first, so the most
/// likely target is the default selection.
fn seed_push_branches(
    repo: &Repo,
    remotes: &[String],
    current: &str,
    existing: Vec<String>,
) -> Vec<String> {
    if current.is_empty() {
        return existing;
    }
    let preferred = repo
        .remote_targets()
        .ok()
        .and_then(|t| t.push_remote)
        .unwrap_or_else(|| default_remote(repo));
    let mut ordered: Vec<&String> = remotes.iter().collect();
    ordered.sort_by_key(|r| **r != preferred);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(remotes.len() + existing.len());
    for cand in ordered
        .into_iter()
        .map(|r| format!("{r}/{current}"))
        .chain(existing)
    {
        if seen.insert(cand.clone()) {
            out.push(cand);
        }
    }
    out
}

/// Split a chosen `remote/branch` ref into its parts. A bare value (no `/`,
/// from a freshly-typed branch) defaults to [`default_remote`].
fn split_ref(repo: &Repo, chosen: &str) -> (String, String) {
    match chosen.split_once('/') {
        Some((remote, branch)) => (remote.to_string(), branch.to_string()),
        None => (default_remote(repo), chosen.to_string()),
    }
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
/// chips read as distinct keys rather than blending together. `font` is the
/// monospace family — keys read as keys, never the proportional UI font.
fn chip_box(color: Hsla, font: &SharedString) -> gpui::Div {
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
        .font_family(font.clone())
        .bg(with_alpha(color, 0.12))
}

/// The display label for a keystroke spec. A multi-keystroke *sequence* is
/// space-separated (e.g. `g r`, `c c`) and rendered with the keys spaced inside
/// one keycap. A *chord* joins modifiers to a key with `-` (e.g. `cmd-enter` →
/// `Cmd+Enter`). A lone token is word-ified (`tab` → `Tab`).
fn format_keys(key: &str) -> String {
    if key.contains(' ') {
        return key.split(' ').map(key_word).collect::<Vec<_>>().join(" ");
    }
    let parts: Vec<&str> = key.split('-').collect();
    let is_chord = parts.len() >= 2 && parts[..parts.len() - 1].iter().all(|p| is_modifier(p));
    if is_chord {
        parts
            .iter()
            .map(|p| key_word(p))
            .collect::<Vec<_>>()
            .join("+")
    } else {
        key_word(key)
    }
}

/// A keyboard key badge: a keycap chip with a word-style label (see
/// [`format_keys`]). `font` is the monospace family.
fn key_chip(key: &str, color: Hsla, font: &SharedString) -> AnyElement {
    chip_box(color, font)
        .child(SharedString::from(format_keys(key)))
        .into_any_element()
}

/// A switch keycap (`-a`). When a `-` prefix is pending (we're awaiting the
/// switch letter), only the dash *inside* the keycap changes color to the
/// accent, while the keycap itself stays neutral (magit's prefix feedback).
fn switch_chip(
    key: &str,
    color: Hsla,
    accent: Hsla,
    pending: bool,
    font: &SharedString,
) -> AnyElement {
    let rest = key.strip_prefix('-').unwrap_or(key);
    let dash_color = if pending { accent } else { color };
    chip_box(color, font)
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

/// Our own theme sets, embedded at compile time. Each file is a `ThemeSet` of
/// light/dark `ThemeConfig`s authored against the official palettes (replacing
/// gpui-component's bundled themes, which were loose ports). More land here as
/// they're authored; see `docs/` for the curated list.
const BUNDLED_THEMES: &[&str] = &[
    include_str!("../themes/github.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/selenized.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/nord.json"),
    include_str!("../themes/dracula.json"),
    include_str!("../themes/tao.json"),
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

    /// Every face `Palette::from_theme` (and the chrome) reads. If a bundled
    /// theme omits one, gpui-component silently falls back to a default color
    /// and the theme looks subtly wrong — this catches that at build time.
    #[test]
    fn bundled_themes_cover_every_face() {
        // Keys read out of the `colors` block.
        const COLORS: &[&str] = &[
            "background",
            "foreground",
            "muted.foreground",
            "border",
            "accent.background",     // selected row
            "list.hover.background", // hover wash
            "selection.background",  // visual-mode region
            "primary.background",    // section headings
            "secondary.background",  // elevated panel
            "base.red",
            "base.green",
            "base.yellow",
            "base.blue",
        ];
        // Keys read out of the `highlight` block (git status faces). `warning`
        // is the "modified" text color, which OVERRIDES base.yellow when set,
        // so it must be present and deliberate (not inherited).
        const HIGHLIGHT: &[&str] = &[
            "warning",
            "success.background", // added line band
            "error.background",   // removed line band
            "warning.background", // banner
        ];
        for set in BUNDLED_THEMES {
            let v: serde_json::Value =
                serde_json::from_str(set).expect("bundled theme is valid JSON");
            let themes = v["themes"].as_array().expect("theme set has `themes`");
            assert!(!themes.is_empty(), "theme set has no themes");
            for theme in themes {
                let name = theme["name"].as_str().unwrap_or("<unnamed>");
                let colors = theme["colors"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}: no `colors` block"));
                for key in COLORS {
                    assert!(
                        colors.contains_key(*key),
                        "theme {name:?} is missing colors.{key}"
                    );
                }
                let highlight = theme["highlight"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}: no `highlight` block"));
                for key in HIGHLIGHT {
                    assert!(
                        highlight.contains_key(*key),
                        "theme {name:?} is missing highlight.{key}"
                    );
                }
            }
        }
    }

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
    fn wrap_at_cursor_only_wraps_at_end_of_an_overlong_body_line() {
        // A wrappable body line (~114 chars of short words) with the cursor at
        // its end.
        let body = "alpha beta gamma delta ".repeat(5);
        let body = body.trim_end();
        let text = format!("summary\n\n{body}");
        let cursor = text.len(); // at the very end
        let wrapped = wrap_at_cursor(&text, cursor, 72).expect("should wrap");
        let body_lines: Vec<&str> = wrapped.lines().skip(2).collect();
        assert!(body_lines.len() > 1, "long body line should wrap");
        assert!(body_lines.iter().all(|l| l.chars().count() <= 72));
        // Only a space turned into a newline: total byte length is unchanged.
        assert_eq!(wrapped.len(), text.len());
    }

    #[test]
    fn wrap_at_cursor_ignores_mid_line_edits_and_the_summary() {
        let body = "alpha beta gamma delta ".repeat(5);
        let text = format!("summary\n\n{}", body.trim_end());
        // Cursor in the middle of the long body line: no wrap (don't reflow
        // under the user as they edit earlier in the line).
        let mid = "summary\n\n".len() + 10;
        assert_eq!(wrap_at_cursor(&text, mid, 72), None);
        // An over-long *summary* with the cursor at its end is never wrapped.
        let long_summary = "x".repeat(90);
        assert_eq!(wrap_at_cursor(&long_summary, long_summary.len(), 72), None);
    }

    #[test]
    fn wrap_at_cursor_leaves_unbreakable_long_words() {
        let word = "x".repeat(100);
        let text = format!("summary\n\n{word}");
        assert_eq!(wrap_at_cursor(&text, text.len(), 72), None);
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
    fn byte_offset_to_position_tracks_lines() {
        assert_eq!(byte_offset_to_position("abc", 2), Position::new(0, 2));
        // Offset just past the first newline -> start of line 1.
        assert_eq!(byte_offset_to_position("ab\ncd", 3), Position::new(1, 0));
        assert_eq!(byte_offset_to_position("ab\ncd", 5), Position::new(1, 2));
        // Multi-byte char: column counts characters, offset counts bytes.
        assert_eq!(byte_offset_to_position("é x", 3), Position::new(0, 2));
    }

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
        let menu: HashSet<&str> = dispatch_menu()
            .groups
            .iter()
            .flat_map(|g| &g.suffixes)
            .filter_map(|s| match s {
                Suffix::Info(i) => Some(i.keys),
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
        // Single-key commands route; multi-stroke / g-prefix entries don't.
        assert!(StatusView::is_dispatch_key("c"));
        assert!(StatusView::is_dispatch_key("s"));
        assert!(StatusView::is_dispatch_key("G"));
        assert!(!StatusView::is_dispatch_key("tab"));
        assert!(!StatusView::is_dispatch_key("g g"));
        assert!(!StatusView::is_dispatch_key("g r"));
        assert!(!StatusView::is_dispatch_key("z")); // not in the menu
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
            "c", "b", "Z", "l", "p", "F", "f", ",", "$", // commands
            "s", "u", "S", "U", "x", // applying changes
            "v", "tab", "g r", ":", "enter", // essential + open file + palette
            "j", "k", "g g", "G", "g j", "g k", // navigation / motions
        ];
        // Keys allowed to be on only one side of the check. Empty today; add a
        // key here (with a comment) when an exception is genuinely warranted.
        const OVERRIDES: &[&str] = &[];

        let menu: HashSet<&str> = dispatch_menu()
            .groups
            .iter()
            .flat_map(|g| &g.suffixes)
            .filter_map(|s| match s {
                Suffix::Info(i) => Some(i.keys),
                _ => None,
            })
            .collect();
        let dispatched: HashSet<&str> = DISPATCH_KEYS.iter().copied().collect();
        let overrides: HashSet<&str> = OVERRIDES.iter().copied().collect();

        let missing_from_menu: Vec<&str> = dispatched
            .difference(&menu)
            .copied()
            .filter(|k| !overrides.contains(k))
            .collect();
        assert!(
            missing_from_menu.is_empty(),
            "dispatchable commands missing from the `?` menu (add them to dispatch_menu \
             or OVERRIDES): {missing_from_menu:?}"
        );

        let missing_handler: Vec<&str> = menu
            .difference(&dispatched)
            .copied()
            .filter(|k| !overrides.contains(k))
            .collect();
        assert!(
            missing_handler.is_empty(),
            "`?` menu rows with no run_dispatch handler (add them to DISPATCH_KEYS \
             or OVERRIDES): {missing_handler:?}"
        );
    }
}

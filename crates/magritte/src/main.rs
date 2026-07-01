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
    actions, div, px, size, uniform_list, AnyElement, AnyWindowHandle, App, AppContext, Bounds,
    ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, SharedString, Styled,
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
mod ipc;
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
    CommitMetadata, CommitMode, ConflictSide, DiffSource, EntryKind, FileDiff, FileEntry,
    IgnoreDest, LineKind, LogEntry, RebaseAction, RemoteTargets, Repo, ResetMode, Sequence,
    SequenceKind, Stash, Status, TagsAround,
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/lyallcooper/homebrew-magritte/releases/latest";

/// The in-app commit message editor, backed by gpui-component's multi-line
/// Input. We keep the commit context (mode + switches) alongside it.
struct CommitEditor {
    state: Entity<InputState>,
    mode: CommitMode,
    args: Vec<String>,
    after_submit: CommitAfterSubmit,
    /// The baseline message we'd discard back to: empty for a new commit, or
    /// HEAD's message for amend/reword. Canceling only prompts when the current
    /// text differs from this.
    initial: String,
    /// Whether a "discard message?" confirmation is showing (cancel was pressed
    /// with unsaved edits).
    confirming_cancel: bool,
    /// Briefly true after a key other than y/n/esc is pressed while confirming —
    /// flashes the prompt to draw attention to it. Cleared by a timer.
    flash: bool,
    /// The staged diff being committed, flattened for read-only display below
    /// the message (magit's commit buffer). Empty until loaded, and left empty
    /// for reword (which commits no tree change).
    diff: Vec<CommitDiffRow>,
    diff_scroll: UniformListScrollHandle,
    /// Kept alive so the PressEnter subscription stays active.
    _sub: Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommitAfterSubmit {
    Commit,
    ContinueRebase { stopped_sha: String },
}

/// One flattened row of the commit editor's staged-diff preview.
enum CommitDiffRow {
    /// Extra commit metadata toggled in the commit detail view.
    Detail(String),
    /// A line from the commit's full message, shown above the diff in commit view.
    Message(String),
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
    /// The option values as opened (from saved defaults), paired with
    /// `baseline` so any argument change can offer transient-save.
    baseline_values: std::collections::HashMap<String, String>,
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
            baseline_values: std::collections::HashMap::new(),
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

    /// The option values from a saved set. Saved value entries use the compact
    /// `-k=value` form, so old switch-only files keep working unchanged.
    fn apply_saved_values(
        def: &Transient,
        saved: &[String],
    ) -> std::collections::HashMap<String, String> {
        let option_keys: std::collections::HashSet<&str> =
            def.options().map(|o| o.key).collect();
        saved
            .iter()
            .filter_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                option_keys.contains(key).then(|| (key.to_string(), value.to_string()))
            })
            .collect()
    }

    /// The argument overrides to persist: a plain switch when on, a negatable
    /// switch only when it differs from its config default — recorded as the key
    /// (forced on) or the negation flag (forced off), so omission round-trips as
    /// "follow config" — plus set options as `key=value` entries. The inverse
    /// of [`apply_saved`](Self::apply_saved) / [`apply_saved_values`](Self::apply_saved_values).
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
        let option_keys: std::collections::HashSet<&str> =
            self.def.options().map(|o| o.key).collect();
        for (key, value) in &self.values {
            if option_keys.contains(key.as_str()) {
                out.push(format!("{key}={value}"));
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
    /// Diff the chosen revision/range, with args/pathspecs gathered from the
    /// diff transient.
    DiffRange { args: Vec<String>, paths: Vec<String> },
    /// Show the chosen commit with args/pathspecs gathered from the diff transient.
    DiffCommit { args: Vec<String>, paths: Vec<String> },
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
            PickerAction::DiffRange { .. } => transient::plain_title("Diff range"),
            PickerAction::DiffCommit { .. } => transient::plain_title("Show commit"),
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
            PickerAction::DiffRange { .. } => "diff",
            PickerAction::DiffCommit { .. } => "show",
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
    /// Pick a commit to reword directly via an app-managed rebase stop.
    SelectRebaseReword { args: Vec<String> },
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
    details: Vec<String>,
    show_details: bool,
    rows: Vec<CommitDiffRow>,
    scroll: UniformListScrollHandle,
    /// The cursor row (drives scrolling) and the visual-selection anchor, so
    /// lines can be selected and yanked here too.
    selected: usize,
    visual: Option<usize>,
}

/// A standalone diff buffer (`d` / Magit's `magit-diff`): a title plus a
/// flattened, read-only list of file/hunk/line rows.
struct DiffView {
    title: SharedString,
    rows: Vec<CommitDiffRow>,
    scroll: UniformListScrollHandle,
    selected: usize,
    visual: Option<usize>,
}

#[derive(Clone)]
enum DiffRequest {
    Unstaged { args: Vec<String>, paths: Vec<String> },
    Staged { args: Vec<String>, paths: Vec<String> },
    Worktree { rev: String, args: Vec<String>, paths: Vec<String> },
    Range { range: String, args: Vec<String>, paths: Vec<String> },
}

impl DiffRequest {
    fn title(&self) -> String {
        match self {
            DiffRequest::Unstaged { paths, .. } => diff_title("Unstaged changes", paths),
            DiffRequest::Staged { paths, .. } => diff_title("Staged changes", paths),
            DiffRequest::Worktree { rev, paths, .. } => {
                diff_title(&format!("Working tree vs {rev}"), paths)
            }
            DiffRequest::Range { range, paths, .. } => diff_title(range, paths),
        }
    }
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
        /// The section's listing is being re-fetched; show a small spinner by
        /// the header. Only set on sections that already have data (so a
        /// first-load section just pops in rather than flashing a spinner).
        refreshing: bool,
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
    /// Reword an already-published commit: on `y`, run the direct reword rebase.
    RebaseRewordPushed { rev: String, args: Vec<String> },
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
            .and_then(|r| r.git_common_dir())
            .map(|d| config::repo_dir(&d));
        // UI state local to this checkout (just fold state) lives in the
        // *per-worktree* git dir instead — `.git/magritte` for the main worktree
        // (so it's unchanged), `.git/worktrees/<name>/magritte` for a linked one.
        let worktree_scope_dir = repo
            .as_ref()
            .and_then(|r| r.git_dir().ok())
            .map(|d| config::repo_dir(&d));
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
            loading_sections: HashSet::new(),
            tag_info: (None, None),
            conflicted: HashSet::new(),
            sequence: None,
            pending_rebase_rewords: HashSet::new(),
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
            auto_fetch_gen: Generation::default(),
            update_check_gen: Generation::default(),
            notified_update_version: None,
            activity: 0,
            busy: false,
            busy_gen: Generation::default(),
            last_refresh: None,
            pending_prefix: None,
            prefix_gen: Generation::default(),
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
            usage: config::load_usage(),
            transient_arguments: config::load_transient_arguments(),
            repo_transient_arguments,
            repo_scope_dir,
            worktree_scope_dir,
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
        // Hunk indices shift when the diff changes, so don't carry collapse
        // state across a refresh.
        self.collapsed_hunks.clear();
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
        let pushremote_configured = configured.contains(&SectionId::UnpushedPushremote)
            || configured.contains(&SectionId::UnpulledPushremote);

        // PRIORITY: `git status` + the in-progress sequence. Renders the main
        // file sections (and the header) the moment it lands, before the
        // auxiliary listings — and kicks off the pushremote fetch afterward,
        // since that one needs status to know the push target.
        self.spawn_status_fetch(stamp, pushremote_configured, cx);

        // Auxiliary listings, each its own fetch running concurrently with
        // status (none need our status — git resolves @{upstream}/HEAD itself),
        // so a slow listing can't hold up the main sections or the others. Each
        // pops into place as it lands; the title-bar spinner signals the work.
        if configured.contains(&SectionId::Unpushed) || configured.contains(&SectionId::Unpulled) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Unpushed, SectionId::Unpulled],
                cx,
                |repo| {
                    (
                        repo.unpushed().unwrap_or_default(),
                        repo.unpulled().unwrap_or_default(),
                    )
                },
                |this, (up, down)| {
                    this.status_sections.unpushed = up;
                    this.status_sections.unpulled = down;
                },
            );
        }
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
    /// cursor and re-warming diffs), then — now that the push target is known —
    /// fetches the pushremote sections when this is a triangular workflow.
    fn spawn_status_fetch(&mut self, stamp: u64, pushremote_configured: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        // Capture the cursor's logical position now (before the rebuild) so it
        // can be restored once status lands, rather than left at a stale index.
        let anchor = self.capture_anchor();
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let (result, sequence) = cx
                .background_executor()
                .spawn(async move { (repo.status(), repo.sequence()) })
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
                let triangular = this
                    .status
                    .as_ref()
                    .is_some_and(|s| s.head.push.is_some());
                // Pushremote sections only exist in a triangular workflow; clear
                // any stale listing otherwise so it doesn't linger from a prior
                // state (do it before the rebuild so the row reflects it).
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
                // Now that status resolved the push target, fetch the pushremote
                // listings; they pop into place (or drop their spinner) on land.
                if pushremote_configured && triangular {
                    this.spawn_fetch(
                        stamp,
                        &[SectionId::UnpushedPushremote, SectionId::UnpulledPushremote],
                        cx,
                        |repo| {
                            (
                                repo.unpushed_to_push().unwrap_or_default(),
                                repo.unpulled_from_push().unwrap_or_default(),
                            )
                        },
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
            Some((_, Confirm::RebaseRewordPushed { rev, args })) => {
                self.run_rebase_reword_from_rev(rev, args, window, cx)
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
        // A saved argument set (magit's `transient-save`) overrides this
        // transient's defaults; that becomes the baseline, so the save hint only
        // appears once the user changes it. A negatable (config-derived) switch
        // is overridden only when the saved set names it *explicitly* — its key
        // (force on) or its negation flag (force off); otherwise it keeps the
        // config default, so an old/empty saved set can't silently flip e.g.
        // gpg-signing off.
        if let Some(saved) = self.saved_arguments(id) {
            state.active = TransientState::apply_saved(&state.def, saved);
            state.baseline = state.active.clone();
            state.values = TransientState::apply_saved_values(&state.def, saved);
            state.baseline_values = state.values.clone();
        }
        self.popup = Some(Popup::Transient(state));
        cx.notify();
    }

    /// The saved argument set in effect for a transient id: the repo scope wins
    /// wholesale over the global scope (per-id replace), so a repo's entry fully
    /// defines that transient's defaults while global still covers the rest.
    fn saved_arguments(&self, id: &str) -> Option<&Vec<String>> {
        self.repo_transient_arguments
            .get(id)
            .or_else(|| self.transient_arguments.get(id))
    }

    /// Persist the open transient's argument overrides to a scope (magit's
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
            config::repo_transient_arguments_path(&dir)
        } else {
            let Some(path) = config::transient_arguments_path() else {
                return;
            };
            path
        };
        let values = if repo_scope {
            &mut self.repo_transient_arguments
        } else {
            &mut self.transient_arguments
        };
        // An empty set carries no overrides — drop the entry rather than writing
        // `id = []`, which used to read as "force everything off".
        if switches.is_empty() {
            values.remove(&id);
        } else {
            values.insert(id, switches);
        }
        config::save_transient_arguments_at(&path, values);
        // The saved set is the new baseline, so the hint hides again.
        if let Some(Popup::Transient(s)) = self.popup.as_mut() {
            s.baseline = s.active.clone();
            s.baseline_values = s.values.clone();
        }
        let scope = if repo_scope { "for this repo" } else { "globally" };
        self.set_status(format!("Saved arguments {scope}"), true, cx);
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
    pub(crate) fn external_commit_editor(&self) -> Option<String> {
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
        self.open_editor_after(mode, args, CommitAfterSubmit::Commit, window, cx);
    }

    fn open_editor_after(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        after_submit: CommitAfterSubmit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If the user opted to write ordinary commit messages in their external
        // editor, hand off to an interactive `git commit` instead of the in-app
        // editor. Mid-rebase rewords choose their editor before reaching this
        // helper, because they also have to continue the rebase afterward.
        if after_submit == CommitAfterSubmit::Commit {
            if let Some(git_editor) = self.external_commit_editor() {
                self.commit_via_external_editor(mode, args, git_editor, cx);
                return;
            }
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
            after_submit,
            initial: String::new(),
            confirming_cancel: false,
            flash: false,
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
        // While the "discard message?" confirmation is up, it owns the keyboard:
        // swallow every key so none reaches the message input (otherwise typing
        // would edit the message behind the prompt). Only y / n / esc act.
        if self.editor().is_some_and(|e| e.confirming_cancel) {
            cx.stop_propagation();
            match key {
                "y" => self.discard_editor(window, cx),
                "n" | "escape" => self.keep_editing(window, cx),
                // Any other key is ignored — flash the prompt so it's clear
                // input is paused and only y/n/esc do anything.
                _ => self.flash_discard_prompt(cx),
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
                ed.flash = false; // start un-flashed
            }
            cx.notify();
        } else {
            self.discard_editor(window, cx);
        }
    }

    /// Flash the discard confirmation to draw attention to it — invoked when a
    /// key other than y/n/esc is pressed while it's up. A generation-scoped
    /// timer clears the flash, so rapid keypresses keep it lit without an
    /// earlier timer cutting a later flash short.
    fn flash_discard_prompt(&mut self, cx: &mut Context<Self>) {
        if !self.editor().is_some_and(|e| e.confirming_cancel) {
            return;
        }
        if let Some(ed) = self.editor_mut() {
            ed.flash = true;
        }
        let gen = self.confirm_flash_gen.bump();
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(CONFIRM_FLASH_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.confirm_flash_gen.is_current(gen) {
                    if let Some(ed) = this.editor_mut() {
                        ed.flash = false;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
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

    fn start_log_select_rebase_reword(&mut self, switches: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.show_log_loading(LogPurpose::SelectRebaseReword { args: switches }, cx);
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
    /// or the commit at point in a status commit section, opening the todo
    /// editor. `args` are the rebase switches. First checks (off the UI thread)
    /// whether that commit is already published; if so, confirm before rewriting
    /// pushed history — like magit's rebase assert and our amend/reword warning.
    fn rebase_since_selected(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(rev) = self.selected_commit_hash() else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let probe = rev.clone();
        let branches = self.config.published_branches.clone();
        cx.spawn(async move |this, cx| {
            let published = cx
                .background_executor()
                .spawn(async move { repo.published_on(&probe, &branches) })
                .await;
            this.update(cx, |this, cx| {
                // base = commit^: `base..HEAD` then includes the selected commit.
                let Some(target) = published else {
                    this.open_rebase_todo(format!("{rev}^"), args, cx);
                    return;
                };
                // The confirmation bar is status-screen chrome, so leave the log
                // to show it; "yes" opens the todo editor.
                this.screen = Screen::Status;
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

    /// Reword the selected older commit using an interactive rebase, matching
    /// Magit's `c R` / `r w` / `magit-rebase-reword-commit` path.
    fn reword_past_selected(
        &mut self,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_commit_hash().is_some() {
            self.rebase_reword_selected(args, window, cx);
        } else {
            self.start_log_select_rebase_reword(args, cx);
        }
    }

    fn rebase_reword_selected(
        &mut self,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rev) = self.selected_commit_hash() else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let probe = rev.clone();
        let branches = self.config.published_branches.clone();
        cx.spawn_in(window, async move |this, cx| {
            let published = cx
                .background_executor()
                .spawn(async move { repo.published_on(&probe, &branches) })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(target) = published else {
                    this.run_rebase_reword_from_rev(rev, args, window, cx);
                    return;
                };
                this.screen = Screen::Status;
                this.confirm = Some((
                    format!("{rev} has already been pushed to {target}. Rebase since it anyway?"),
                    Confirm::RebaseRewordPushed { rev, args },
                ));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_rebase_reword_from_rev(
        &mut self,
        rev: String,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let base = format!("{rev}^");
                    let result = (|| {
                        let mut steps = repo.rebase_todo(&base)?;
                        let step = steps
                            .iter_mut()
                            .find(|s| rev.starts_with(&s.oid) || s.oid.starts_with(&rev))
                            .ok_or_else(|| {
                                magritte_core::Error::Message(
                                    "selected commit is not in the rebase range".to_string(),
                                )
                            })?;
                        let oid = step.oid.clone();
                        step.action = RebaseAction::Reword;
                        repo.rebase_interactive(&base, &steps, &args)?;
                        Ok::<_, magritte_core::Error>(oid)
                    })();
                    let stopped = if result.is_ok() {
                        repo.rebase_stopped_sha()
                    } else {
                        None
                    };
                    (result, stopped)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let (result, stopped) = outcome;
                this.job_cancel = None;
                match result {
                    Ok(oid) => {
                        this.pending_rebase_rewords.insert(oid);
                        if let Some(stopped) = stopped {
                            if this.open_pending_rebase_reword(stopped, window, cx) {
                                return;
                            }
                        }
                        this.report("Rebased", Ok(String::new()), cx);
                        this.refresh(cx);
                    }
                    Err(e) => {
                        this.report("Rebased", Err(e), cx);
                        this.refresh(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn pending_rebase_reword_matches(&self, stopped_sha: &str) -> bool {
        self.pending_rebase_rewords
            .iter()
            .any(|oid| stopped_sha.starts_with(oid) || oid.starts_with(stopped_sha))
    }

    fn open_pending_rebase_reword(
        &mut self,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.pending_rebase_reword_matches(&stopped_sha) {
            return false;
        }
        if let Some(git_editor) = self.external_commit_editor() {
            self.run_rebase_reword_with_external_editor(stopped_sha, git_editor, window, cx);
            return true;
        }
        self.clear_status(cx);
        self.open_editor_after(
            CommitMode::Reword,
            Vec::new(),
            CommitAfterSubmit::ContinueRebase { stopped_sha },
            window,
            cx,
        );
        true
    }

    /// The selected commit in the log, or the commit row at point in status.
    fn selected_commit_hash(&self) -> Option<String> {
        self.log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.hash.clone())
            .or_else(|| self.point_commit().map(|(hash, _, _)| hash))
    }

    /// Open the cherry-pick transient, using a status/log commit at point as the
    /// default when its suffix fires (Magit's commit-at-point model).
    fn open_cherry_pick_transient(&mut self, cx: &mut Context<Self>) {
        self.open_transient(
            "cherry-pick",
            transient::cherry_pick_transient(),
            RemoteTargets::default(),
            cx,
        );
    }

    /// Open the revert transient, using a status/log commit at point as the
    /// default when its suffix fires (Magit's commit-at-point model).
    fn open_revert_transient(&mut self, cx: &mut Context<Self>) {
        self.open_transient(
            "revert",
            transient::revert_transient(),
            RemoteTargets::default(),
            cx,
        );
    }

    /// Open the selected commit's diff (the clickable "view" button; Return does
    /// the same from the key handler).
    fn view_log_commit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_commit_view(cx);
    }

    /// Confirm the selected commit in a log-select mode (the clickable "select"
    /// button; Return does the same from the key handler).
    fn confirm_log_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.log().map(|l| &l.purpose) {
            Some(LogPurpose::SelectRebaseBase { args }) => {
                self.rebase_since_selected(args.clone(), cx);
            }
            Some(LogPurpose::SelectRebaseReword { args }) => {
                self.reword_past_selected(args.clone(), window, cx);
            }
            _ => {}
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

    /// Cherry-pick or revert the commit selected in the log, or the commit at
    /// point in a status commit section, then return to the status view (so a
    /// conflict shows in the in-progress banner). Runs on the background
    /// executor.
    fn pick_selected(&mut self, op: PickOp, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_selected_with_args(op, Vec::new(), window, cx);
    }

    fn pick_selected_with_args(
        &mut self,
        op: PickOp,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rev) = self.selected_commit_hash() else {
            self.set_status("No commit at point".to_string(), false, cx);
            return;
        };
        let (verb, done) = match op {
            PickOp::CherryPick => ("Cherry-picking", "Cherry-picked"),
            PickOp::CherryApply => ("Applying", "Applied"),
            PickOp::Revert => ("Reverting", "Reverted"),
            PickOp::RevertNoCommit => ("Reverting", "Reverted"),
        };
        if self.log().is_some() {
            self.close_log(window, cx);
        }
        self.run_job(
            &format!("{verb} {rev}…"),
            done,
            move |repo| match op {
                PickOp::CherryPick => repo.cherry_pick_with_args(&rev, &args),
                PickOp::CherryApply => repo.cherry_apply_with_args(&rev, &args),
                PickOp::Revert => {
                    let args = if args.is_empty() { vec!["--no-edit".to_string()] } else { args };
                    repo.revert_with_args(&rev, &args)
                }
                PickOp::RevertNoCommit => repo.revert_no_commit_with_args(&rev, &args),
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

    fn open_commit_with_args(
        &mut self,
        hash: String,
        short: String,
        subject: String,
        args: Vec<String>,
        paths: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.open_commit_inner(hash, short, subject, args, paths, cx);
    }

    /// Open a commit's diff detail, overlaying the current screen (restored on
    /// close). Shared by the log view and status commit rows.
    fn open_commit(&mut self, hash: String, short: String, subject: String, cx: &mut Context<Self>) {
        self.open_commit_inner(hash, short, subject, Vec::new(), Vec::new(), cx);
    }

    fn open_commit_inner(
        &mut self,
        hash: String,
        short: String,
        subject: String,
        args: Vec<String>,
        paths: Vec<String>,
        cx: &mut Context<Self>,
    ) {
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
                details: Vec::new(),
                show_details: false,
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
                    let metadata = repo.commit_metadata(&rev)?;
                    let message = repo.commit_message(&rev)?;
                    let files = repo.diff_commit_with(&rev, &args, &paths).map(|diffs| {
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
                    })?;
                    Ok::<_, magritte_core::Error>((metadata, message, files))
                })
                .await;
            this.update(cx, |this, cx| {
                // Bail if a newer screen load superseded this one, or the view
                // was closed before the diff arrived.
                if !this.screen_gen.is_current(gen) || this.commit_view().is_none() {
                    return;
                }
                let loaded = match loaded {
                    Ok(loaded) => loaded,
                    Err(e) => {
                        if let Some(cv) = this.commit_view_mut() {
                            cv.rows = vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))];
                        }
                        cx.notify();
                        return;
                    }
                };
                let (metadata, message, files) = loaded;
                let details = commit_metadata_lines(&metadata);
                let show_details = this.commit_view().is_some_and(|cv| cv.show_details);
                let mut rows = this.commit_detail_rows(&message, &files, cx);
                if show_details {
                    prepend_commit_details(&mut rows, &details);
                }
                if let Some(cv) = this.commit_view_mut() {
                    cv.details = details;
                    cv.rows = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_diff(&mut self, request: DiffRequest, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        let title = request.title();
        let back = Box::new(std::mem::take(&mut self.screen));
        self.screen = Screen::Diff {
            view: DiffView {
                title: SharedString::from(title.clone()),
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
                    let diffs = match request {
                        DiffRequest::Unstaged { args, paths } => repo.diff_unstaged(&args, &paths),
                        DiffRequest::Staged { args, paths } => repo.diff_staged(&args, &paths),
                        DiffRequest::Worktree { rev, args, paths } => {
                            repo.diff_worktree(&rev, &args, &paths)
                        }
                        DiffRequest::Range { range, args, paths } => {
                            repo.diff_range(&range, &args, &paths)
                        }
                    }?;
                    Ok::<_, magritte_core::Error>(
                        diffs
                            .into_iter()
                            .map(|d| {
                                let (head, tail) =
                                    file_head_tail(&repo.workdir().join(d.display_path()));
                                let lang =
                                    highlight::detect_language(d.display_path(), &head, &tail);
                                (d, lang)
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) || this.diff_view().is_none() {
                    return;
                }
                let rows = match loaded {
                    Ok(files) if files.is_empty() => {
                        vec![CommitDiffRow::Note("No changes".to_string())]
                    }
                    Ok(files) => this.diff_rows(&files, cx),
                    Err(e) => vec![CommitDiffRow::Note(format!("diff unavailable: {e}"))],
                };
                if let Some(dv) = this.diff_view_mut() {
                    dv.rows = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn commit_detail_rows(
        &self,
        message: &str,
        files: &[(FileDiff, Option<&'static str>)],
        cx: &mut Context<Self>,
    ) -> Vec<CommitDiffRow> {
        let mut rows = Vec::new();
        let mut body = message.lines().skip(1);
        if matches!(body.clone().next(), Some("")) {
            body.next();
        }
        let mut body = body.peekable();
        if body.peek().is_some() {
            rows.push(CommitDiffRow::Note(String::new()));
        }
        for line in body {
            rows.push(CommitDiffRow::Message(line.to_string()));
        }
        if !rows.is_empty() {
            rows.push(CommitDiffRow::Note(String::new()));
        }
        rows.extend(self.diff_rows(files, cx));
        rows
    }

    fn close_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Return to the screen the commit view was opened from (log or status).
        if let Screen::Commit { back, .. } = std::mem::take(&mut self.screen) {
            self.screen = *back;
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn close_diff_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Diff { back, .. } = std::mem::take(&mut self.screen) {
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

    fn diff_view_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(dv) = self.diff_view_mut() {
            if dv.rows.is_empty() {
                return;
            }
            let last = dv.rows.len() as isize - 1;
            dv.selected = (dv.selected as isize + delta).clamp(0, last) as usize;
            dv.scroll
                .scroll_to_item(dv.selected, gpui::ScrollStrategy::Top);
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

    fn toggle_commit_details(&mut self, cx: &mut Context<Self>) {
        if let Some(cv) = self.commit_view_mut() {
            cv.show_details = !cv.show_details;
            if cv.show_details {
                prepend_commit_details(&mut cv.rows, &cv.details);
            } else {
                cv.rows.retain(|row| !matches!(row, CommitDiffRow::Detail(_)));
                cv.selected = cv.selected.min(cv.rows.len().saturating_sub(1));
            }
            cx.notify();
        }
    }

    fn diff_view_toggle_visual(&mut self, cx: &mut Context<Self>) {
        if let Some(dv) = self.diff_view_mut() {
            dv.visual = if dv.visual.is_some() {
                None
            } else {
                Some(dv.selected)
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

    fn copy_diff_selection(&mut self, cx: &mut Context<Self>) {
        let text = {
            let Some(dv) = self.diff_view() else {
                return;
            };
            let (lo, hi) = match dv.visual {
                Some(a) => (a.min(dv.selected), a.max(dv.selected)),
                None => (dv.selected, dv.selected),
            };
            let hi = hi.min(dv.rows.len().saturating_sub(1));
            dv.rows[lo..=hi]
                .iter()
                .map(commit_row_text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if let Some(dv) = self.diff_view_mut() {
            dv.visual = None;
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
        let message = text.trim_end().to_string();
        match ed.after_submit {
            CommitAfterSubmit::Commit => self.run_commit(message, ed.mode, ed.args, cx),
            CommitAfterSubmit::ContinueRebase { stopped_sha } => {
                self.run_rebase_reword_commit(message, stopped_sha, window, cx)
            }
        }
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

    fn run_rebase_reword_commit(
        &mut self,
        message: String,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_reword_after_commit(
            stopped_sha,
            window,
            cx,
            move |repo| repo.commit(&message, CommitMode::Reword, &[]),
        );
    }

    fn run_rebase_reword_with_external_editor(
        &mut self,
        stopped_sha: String,
        git_editor: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_reword_after_commit(
            stopped_sha,
            window,
            cx,
            move |repo| repo.commit_with_editor(CommitMode::Reword, &[], &git_editor),
        );
    }

    fn run_rebase_reword_after_commit<F>(
        &mut self,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        commit: F,
    ) where
        F: FnOnce(&Repo) -> magritte_core::Result<String> + Send + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        cx.spawn_in(window, async move |this, cx| {
            let stopped_for_result = stopped_sha.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let commit_result = commit(&repo);
                    let committed = commit_result.is_ok();
                    let result = commit_result.and_then(|_| repo.sequence_continue(SequenceKind::Rebase));
                    let stopped = if result.is_ok() {
                        repo.rebase_stopped_sha()
                    } else {
                        None
                    };
                    (result, stopped, committed)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let (result, stopped, committed) = outcome;
                this.job_cancel = None;
                if committed {
                    this.pending_rebase_rewords.remove(&stopped_for_result);
                }
                if result.is_ok() {
                    if let Some(stopped) = stopped {
                        if this.open_pending_rebase_reword(stopped, window, cx) {
                            return;
                        }
                    }
                }
                this.report("Reworded", result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
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

fn describe_command(command: transient::Command) -> &'static str {
    use transient::Command::*;
    match command {
        PushPushRemote | PushUpstream | PushElsewhere => "Pushing",
        PullPushRemote | PullUpstream | PullElsewhere => "Pulling",
        FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => "Fetching",
        CommitCreate | CommitAmend | CommitReword | CommitRewordPast | CommitExtend => "Committing",
        // Branch, stash, and log commands route through their own picker/runner.
        BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
            "Working"
        }
        StashPush | StashPushAll | StashApply | StashPop | StashDrop => "Stashing",
        DiffDwim | DiffRange | DiffUnstaged | DiffStaged | DiffWorktree | DiffCommit => "Diffing",
        LogCurrent | LogAll | LogOther | LogReflog => "Logging",
        ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => "Resetting",
        MergePlain | MergeNoCommit | MergeSquash => "Merging",
        CherryPick | CherryApply => "Cherry-picking",
        RevertCommit | RevertNoCommit => "Reverting",
        RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive
        | RebaseRewordCommit => "Rebasing",
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
        CommitCreate | CommitAmend | CommitReword | CommitRewordPast | CommitExtend => "Committed",
        BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
            "Done"
        }
        StashPush | StashPushAll | StashApply | StashPop | StashDrop => "Stashed",
        DiffDwim | DiffRange | DiffUnstaged | DiffStaged | DiffWorktree | DiffCommit => "Done",
        LogCurrent | LogAll | LogOther | LogReflog => "Done",
        ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => "Reset",
        MergePlain | MergeNoCommit | MergeSquash => "Merged",
        CherryPick | CherryApply => "Cherry-picked",
        RevertCommit | RevertNoCommit => "Reverted",
        RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive
        | RebaseRewordCommit => "Rebased",
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

fn status_window_options(cx: &mut App) -> WindowOptions {
    // A reasonable default window instead of filling the whole screen;
    // centered on the active display. The user can resize freely.
    let bounds = Bounds::centered(None, size(px(1000.0), px(720.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Transparent system bar so our custom `TitleBar` draws the chrome
        // (and the traffic lights sit where the component expects them).
        titlebar: Some(gpui_component::TitleBar::title_bar_options()),
        ..Default::default()
    }
}

fn open_repo_window(start_dir: Option<PathBuf>, cx: &mut App) -> Option<AnyWindowHandle> {
    let (cfg, cfg_warning) = config::load_reporting();
    theme::apply_appearance(&cfg, cx);
    let options = status_window_options(cx);
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
                        let _ = cx.update(|cx| {
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
            "c", "b", "Z", "l", "d", "p", "F", "f", "O", "m", "r", "i", "!", ",", "$", // commands
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

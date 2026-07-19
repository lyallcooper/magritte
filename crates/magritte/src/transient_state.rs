//! Transient (popup menu) interaction state and the general minibuffer picker
//! model: which transient is open with which switches/options toggled, saved
//! argument defaults, multi-keystroke suffix dispatch, and the typed picker
//! actions the controller resolves. `impl StatusView` like the other slices.

use gpui::{Context, Window};
use magritte_core::{RemoteTargets, Repo};

use crate::*;

/// An open transient popup with the switches toggled on and the option values
/// set within it.
pub(crate) struct TransientState {
    /// The transient's command id (`commit`, `push`, …), for saving its switch
    /// defaults. Empty for ad-hoc transients (e.g. an in-progress sequence).
    pub(crate) id: String,
    pub(crate) def: Transient,
    pub(crate) active: std::collections::HashSet<String>,
    /// The active set as opened (its saved/built-in defaults), so the UI can
    /// tell when switches have been *modified* (to offer saving them).
    pub(crate) baseline: std::collections::HashSet<String>,
    /// Value-reading option values, keyed by the option's key (e.g. `-F` →
    /// `fix bug`). Combined with `active` to build the git argument list.
    pub(crate) values: std::collections::HashMap<String, String>,
    /// The option values as opened (from saved defaults), paired with
    /// `baseline` so any argument change can offer transient-save.
    pub(crate) baseline_values: std::collections::HashMap<String, String>,
    /// True after `-` is pressed, awaiting the switch/option letter (magit `-f`).
    pub(crate) pending_dash: bool,
    /// Keystrokes so far of a multi-key suffix (magit's `fu`/`pu` jump keys),
    /// kept while they prefix some suffix key. Empty when nothing is pending.
    pub(crate) pending_key: String,
    /// True after the save key is pressed, awaiting the scope letter (`g`lobal /
    /// `l`ocal) — magit-style two-step save.
    pub(crate) pending_save: bool,
    /// Resolved push/pull/fetch targets, so dispatch can route to the right
    /// remote without recomputing (empty for non-remote transients).
    pub(crate) targets: RemoteTargets,
}

impl TransientState {
    pub(crate) fn new(id: impl Into<String>, def: Transient, targets: RemoteTargets) -> Self {
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
            pending_key: String::new(),
            pending_save: false,
            targets,
        }
    }

    /// Toggle a switch by its full key (`-f`). Toggling on turns off any switch
    /// declared incompatible with it (magit's `:incompatible`) — the one rule
    /// both the keyboard and mouse toggle paths must share.
    pub(crate) fn toggle_switch(&mut self, key: &str) {
        if !self.active.remove(key) {
            for conflicting in conflicting_switch_keys(&self.def, key) {
                self.active.remove(&conflicting);
            }
            self.active.insert(key.to_string());
        }
    }

    /// The git flag arguments from the toggled switches and set options, in
    /// definition order (switches first, then options as `{arg}{value}`).
    /// Pathspec options are excluded — see [`Self::pathspecs`] — since they must
    /// trail the revision behind a `--`.
    pub(crate) fn args(&self) -> Vec<String> {
        let switches = self.def.switches().filter_map(|s| {
            let on = self.active.contains(s.key.as_str());
            match &s.negation {
                // A negatable switch reflects a git-config default: emit a flag
                // only when the toggle differs from that default — the positive
                // arg when turned on, the negation (e.g. --no-gpg-sign) when off.
                Some(neg) => {
                    (on != s.default_on).then(|| if on { s.arg.clone() } else { neg.clone() })
                }
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
    pub(crate) fn pathspecs(&self) -> Vec<String> {
        self.def
            .options()
            .filter(|o| o.pathspec)
            .filter_map(|o| self.values.get(o.key).cloned())
            .collect()
    }

    /// The active switch set from a saved set (magit's `transient-save`),
    /// reconciled against the transient's switches. Saved entries are the git
    /// *arguments* (`--all`, `--no-gpg-sign`), not keystrokes, so remapping or
    /// swapping a switch key can't misread a default. A plain switch is on iff
    /// the set names its argument. A *negatable* (config-derived) switch is
    /// forced on or off only when the set names its argument or its negation
    /// explicitly — otherwise it keeps its config default, so an old or empty
    /// saved set can't silently flip e.g. gpg-signing off by mere omission.
    pub(crate) fn apply_saved(
        def: &Transient,
        saved: &[String],
    ) -> std::collections::HashSet<String> {
        let saved: std::collections::HashSet<&str> = saved.iter().map(String::as_str).collect();
        let mut active = std::collections::HashSet::new();
        for sw in def.switches() {
            let on = match &sw.negation {
                Some(_) if saved.contains(sw.arg.as_str()) => true,
                Some(neg) if saved.contains(neg.as_str()) => false,
                Some(_) => sw.default_on,
                None => saved.contains(sw.arg.as_str()),
            };
            if on {
                active.insert(sw.key.clone());
            }
        }
        active
    }

    /// The option values from a saved set. Entries are the emitted git argument
    /// (`--grep=fix bug`, `-n50`, or a `--…-order` flag), matched back to their
    /// option: by the option's argument prefix, or — for a fixed-choice option
    /// whose value *is* the flag (log's order) — by the choice list. Pathspec
    /// options aren't saved (see [`saved_overrides`](Self::saved_overrides)).
    pub(crate) fn apply_saved_values(
        def: &Transient,
        saved: &[String],
    ) -> std::collections::HashMap<String, String> {
        // Longest argument prefix first, so a more specific option wins.
        let mut prefixed: Vec<&transient::Opt> = def
            .options()
            .filter(|o| !o.pathspec && !o.arg.is_empty())
            .collect();
        prefixed.sort_by_key(|o| std::cmp::Reverse(o.arg.len()));

        let mut out = std::collections::HashMap::new();
        for entry in saved {
            // A bare switch argument (or negation) belongs to apply_saved.
            if def
                .switches()
                .any(|s| s.arg == *entry || s.negation.as_deref() == Some(entry.as_str()))
            {
                continue;
            }
            if let Some(opt) = prefixed.iter().find(|o| entry.starts_with(o.arg)) {
                out.insert(opt.key.to_string(), entry[opt.arg.len()..].to_string());
                continue;
            }
            // A fixed-choice option whose value is itself the flag (log's `-o`).
            if let Some(opt) = def.options().find(|o| {
                o.arg.is_empty()
                    && matches!(o.completion, transient::Completion::OneOf(c) if c.contains(&entry.as_str()))
            }) {
                out.insert(opt.key.to_string(), entry.clone());
            }
        }
        out
    }

    /// The argument overrides to persist: the git argument each override emits
    /// — a plain switch when on, a negatable switch only when it differs from
    /// its config default (its argument when forced on, its negation when
    /// forced off, so omission round-trips as "follow config"), plus each set
    /// value option as `{arg}{value}` (exactly what [`args`](Self::args)
    /// emits). Pathspec options are per-invocation file scoping, not defaults,
    /// so they're excluded. Storing arguments rather than keystrokes keeps a
    /// saved set correct across key remaps. The inverse of
    /// [`apply_saved`](Self::apply_saved) / [`apply_saved_values`](Self::apply_saved_values).
    pub(crate) fn saved_overrides(&self) -> Vec<String> {
        let mut out = Vec::new();
        for sw in self.def.switches() {
            let on = self.active.contains(&sw.key);
            match &sw.negation {
                Some(neg) if on != sw.default_on => {
                    out.push(if on { sw.arg.clone() } else { neg.clone() })
                }
                Some(_) => {}
                None if on => out.push(sw.arg.clone()),
                None => {}
            }
        }
        for opt in self.def.options() {
            if opt.pathspec {
                continue;
            }
            if let Some(value) = self.values.get(opt.key) {
                out.push(format!("{}{}", opt.arg, value));
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
pub(crate) enum Popup {
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
pub(crate) enum Transfer {
    /// `git push [--set-upstream] <remote> <branch>`; `save_push_remote` records
    /// `branch.<b>.pushRemote` first (first push to a push-remote).
    Push {
        branch: String,
        set_upstream: bool,
        save_push_remote: bool,
    },
    /// Push the current branch to a chosen `remote/branch` ref (elsewhere),
    /// creating it if new: `git push <remote> <branch>:<target>`. Also the
    /// second step of push-other, where `branch` is the picked source.
    PushRef { branch: String },
    /// `git push <remote> <tag>` — push one tag (the remote is the picked value).
    PushTag { tag: String },
    /// `git push <remote> --tags` — push all tags.
    PushTags,
    /// `git pull <remote> <branch>` — `branch` is the remote branch to merge.
    Pull { branch: String },
    /// Pull a chosen `remote/branch` ref (elsewhere).
    PullRef,
    /// `git fetch <remote>`.
    Fetch,
}

impl Transfer {
    /// Present-tense label for the progress message.
    pub(crate) fn verb(&self) -> &'static str {
        match self {
            Transfer::Push { .. }
            | Transfer::PushRef { .. }
            | Transfer::PushTag { .. }
            | Transfer::PushTags => "Pushing",
            Transfer::Pull { .. } | Transfer::PullRef => "Pulling",
            Transfer::Fetch => "Fetching",
        }
    }

    /// The minibuffer prompt (styled spans): you push the current branch *to* a
    /// target, but pull/fetch *from* one (matching magit's "Push master to" /
    /// "Pull from" / "Fetch from"). The branch is set off as its own span.
    pub(crate) fn prompt(&self) -> Vec<TitleSpan> {
        match self {
            Transfer::Push { branch, .. } | Transfer::PushRef { branch } => {
                if branch.is_empty() {
                    transient::plain_title("Push to")
                } else {
                    vec![
                        TitleSpan::text("Push "),
                        TitleSpan::accent(branch.clone()),
                        TitleSpan::text(" to"),
                    ]
                }
            }
            Transfer::PushTag { tag } => vec![
                TitleSpan::text("Push "),
                TitleSpan::accent(tag.clone()),
                TitleSpan::text(" to remote"),
            ],
            Transfer::PushTags => transient::plain_title("Push tags to remote"),
            Transfer::Pull { .. } | Transfer::PullRef => transient::plain_title("Pull from"),
            Transfer::Fetch => transient::plain_title("Fetch from"),
        }
    }
}

/// A branch-transient operation carried out against a picked branch/name. Some
/// are two-step (`RenameFrom` → `RenameTo`): the first picker's confirm opens
/// the second.
#[derive(Clone)]
pub(crate) enum BranchAction {
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
    /// Open the chosen branch's config transient.
    Configure,
}

/// A tag-transient operation carried out against a typed tag name or picked tag.
#[derive(Clone, Copy)]
pub(crate) enum TagAction {
    Create { annotated: bool },
    Release { annotated: bool },
    Delete,
}

/// A remote-transient operation. Add/rename are two-step prompts.
#[derive(Clone)]
pub(crate) enum RemoteAction {
    AddName,
    AddUrl {
        name: String,
        args: Vec<String>,
    },
    RenameFrom,
    RenameTo {
        old: String,
    },
    Remove,
    /// Pick a remote to open its config transient.
    Configure,
}

/// A stash-transient operation carried out against a picked stash entry. The
/// chosen value is the entry's display string; the `stash@{N}` reference is its
/// first whitespace-delimited token.
#[derive(Clone, Copy)]
pub(crate) enum StashAction {
    Apply,
    Pop,
    Drop,
}

/// What the picker does with its chosen value: a push/pull/fetch target, a
/// branch/stash operation, a value for a transient option, or the ref to log.
#[derive(Clone)]
pub(crate) enum PickerAction {
    Transfer(Transfer),
    Branch(BranchAction),
    Tag(TagAction),
    Remote(RemoteAction),
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
    /// Log the chosen tracked file's history, with the log transient's flags.
    LogFile {
        flags: Vec<String>,
        limit: usize,
    },
    /// Diff the chosen revision/range, with args/pathspecs gathered from the
    /// diff transient.
    DiffRange {
        args: Vec<String>,
        paths: Vec<String>,
    },
    /// Show the chosen commit with args/pathspecs gathered from the diff transient.
    DiffCommit {
        args: Vec<String>,
        paths: Vec<String>,
    },
    /// Run a registry [`Command`] chosen from the `:` palette (matched by title).
    RunCommand,
    /// Reset HEAD to the chosen commit, in the carried mode (hard is confirmed).
    Reset(magritte_core::ResetMode),
    /// Step 1 of branch-reset (magit-branch-reset): the chosen value is the
    /// local branch to reset; step 2 picks the revision.
    ResetBranch,
    /// Step 2 of branch-reset: reset `branch` to the chosen revision — a hard
    /// reset when it's the current branch, `update-ref` otherwise.
    ResetBranchTo {
        branch: String,
    },
    /// Step 1 of file-checkout (magit-file-checkout): the chosen value is the
    /// revision to check the file out of.
    FileCheckoutRev,
    /// Step 2 of file-checkout: `git checkout <rev> -- <chosen file>`.
    FileCheckoutFile {
        rev: String,
    },
    /// Merge the chosen branch/ref into HEAD, with the carried args. With
    /// `edit`, merge `--no-commit` then conclude in the commit editor seeded
    /// with git's prepared message (magit-merge-editmsg).
    Merge {
        edit: bool,
    },
    /// Preview merging the chosen branch: the three-dot `HEAD...<branch>` diff.
    MergePreview,
    /// Step 1 of push-other (magit-push-other): the chosen value is the local
    /// branch/rev to push; step 2 picks the target `remote/branch`.
    PushOtherSource,
    /// Pick the tag to push; the remote is then resolved like the other pushes.
    PushTagSelect,
    /// Rebase the current branch onto the chosen ref, with the carried args.
    Rebase,
    /// Cherry-pick or revert the typed revision/range with the carried args.
    PickRange(PickOp),
    /// Run an arbitrary git command typed by the user (magit's `!`).
    /// The `!` run prompt: a git subcommand (prefilled `git `) or — with
    /// `shell` — a raw `sh -c` line; run in the worktree-relative `dir`
    /// (`None` = repository root).
    Run {
        shell: bool,
        dir: Option<String>,
    },
    /// Patch (magit's `W`): apply a typed patch file to the worktree, apply a
    /// mailbox as commits (`git am`), or create patch files for a typed range.
    PatchApply,
    PatchAm,
    PatchCreate,
    /// Bisect start (magit's `B B`): prompt for the known-bad revision (default
    /// HEAD), then the known-good revision, then `git bisect start <bad> <good>`.
    BisectBadRev,
    BisectGoodRev {
        bad: String,
    },
    /// Add the typed pattern (seeded with the file at point) to a gitignore file.
    Ignore(magritte_core::IgnoreDest),
    /// Stash with the typed message (empty = git's default "WIP on …"), in the
    /// carried mode, optionally limited to the carried pathspecs.
    StashMessage {
        kind: magritte_core::StashKind,
        untracked: magritte_core::StashUntracked,
        paths: Vec<String>,
    },
    /// Step 1 of stash-branch (magit-stash-branch): the chosen value is the
    /// stash to branch from; step 2 reads the new branch name.
    StashBranchStash,
    /// Step 2 of stash-branch: `git stash branch <chosen name> <stash>`.
    StashBranchName {
        stash: String,
    },
    /// The `%` worktree browser's create/move flows — each picks or types a
    /// value, then a directory, then runs a `git worktree` command.
    /// Pick an existing ref to check out in a new worktree (then a directory).
    WorktreeAddRef,
    /// Directory for a new worktree checking out `commit`.
    WorktreeAddDir {
        commit: String,
    },
    /// Type a new branch name to create in a new worktree (then a directory).
    WorktreeBranchName,
    /// Directory for a new worktree on the new `branch`.
    WorktreeBranchDir {
        branch: String,
    },
    /// New directory to move the worktree at `from` to.
    WorktreeMoveTo {
        from: String,
    },
    /// Rename the branch `old` (from the refs browser) to the typed name.
    RefsRename {
        old: String,
    },
    /// Set a git-config variable (`variable`) from a Configure transient to the
    /// typed value (empty unsets it), then reopen the transient.
    SetVariable {
        variable: String,
        description: String,
    },
}

impl PickerAction {
    /// The minibuffer prompt (styled spans) for this picker.
    pub(crate) fn prompt(&self) -> Vec<TitleSpan> {
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
                    TitleSpan::accent(old.clone()),
                    TitleSpan::text(" to"),
                ],
                BranchAction::Delete => transient::plain_title("Delete branch"),
                BranchAction::Configure => transient::plain_title("Configure branch"),
            },
            PickerAction::Tag(TagAction::Create { annotated: true }) => {
                transient::plain_title("Create annotated tag")
            }
            PickerAction::Tag(TagAction::Create { annotated: false }) => {
                transient::plain_title("Create tag")
            }
            PickerAction::Tag(TagAction::Release { .. }) => transient::plain_title("Release tag"),
            PickerAction::Tag(TagAction::Delete) => transient::plain_title("Delete tag"),
            PickerAction::Remote(r) => match r {
                RemoteAction::AddName => transient::plain_title("Add remote"),
                RemoteAction::AddUrl { name, .. } => vec![
                    TitleSpan::text("Add "),
                    TitleSpan::accent(name.clone()),
                    TitleSpan::text(" url"),
                ],
                RemoteAction::RenameFrom => transient::plain_title("Rename remote"),
                RemoteAction::RenameTo { old } => vec![
                    TitleSpan::text("Rename "),
                    TitleSpan::accent(old.clone()),
                    TitleSpan::text(" to"),
                ],
                RemoteAction::Remove => transient::plain_title("Remove remote"),
                RemoteAction::Configure => transient::plain_title("Configure remote"),
            },
            PickerAction::Stash(s) => transient::plain_title(match s {
                StashAction::Apply => "Apply stash",
                StashAction::Pop => "Pop stash",
                StashAction::Drop => "Drop stash",
            }),
            PickerAction::SetOption { description, .. }
            | PickerAction::SetVariable { description, .. } => {
                transient::plain_title(description.clone())
            }
            PickerAction::LogRef { .. } => transient::plain_title("Log ref"),
            PickerAction::LogFile { .. } => transient::plain_title("Log file"),
            PickerAction::DiffRange { .. } => transient::plain_title("Diff range"),
            PickerAction::DiffCommit { .. } => transient::plain_title("Show commit"),
            PickerAction::RunCommand => transient::plain_title("Run command"),
            PickerAction::Reset(_) => transient::plain_title("Reset to"),
            PickerAction::ResetBranch => transient::plain_title("Reset branch"),
            PickerAction::ResetBranchTo { branch } => vec![
                TitleSpan::text("Reset "),
                TitleSpan::accent(branch.clone()),
                TitleSpan::text(" to"),
            ],
            PickerAction::FileCheckoutRev => transient::plain_title("Checkout from revision"),
            PickerAction::FileCheckoutFile { rev } => vec![
                TitleSpan::text("Checkout file from "),
                TitleSpan::accent(rev.clone()),
            ],
            PickerAction::Merge { .. } => transient::plain_title("Merge"),
            PickerAction::MergePreview => transient::plain_title("Preview merge"),
            PickerAction::PushOtherSource => transient::plain_title("Push"),
            PickerAction::PushTagSelect => transient::plain_title("Push tag"),
            PickerAction::Rebase => transient::plain_title("Rebase onto"),
            PickerAction::PickRange(PickOp::CherryPick) => {
                transient::plain_title("Cherry-pick range")
            }
            PickerAction::PickRange(PickOp::Revert) => transient::plain_title("Revert range"),
            PickerAction::PickRange(PickOp::CherryApply) => transient::plain_title("Apply range"),
            PickerAction::PickRange(PickOp::RevertNoCommit) => {
                transient::plain_title("Revert changes in range")
            }
            // Reads like magit's "git " prompt: the typed text follows "git".
            PickerAction::Run { shell, dir } => {
                let what = if *shell { "Run shell" } else { "Run" };
                match dir {
                    Some(d) if !d.is_empty() => transient::plain_title(format!("{what} in {d}/")),
                    _ => transient::plain_title(what),
                }
            }
            PickerAction::Ignore(_) => transient::plain_title("Ignore pattern"),
            PickerAction::StashMessage { .. } => transient::plain_title("Stash message (optional)"),
            PickerAction::StashBranchStash => transient::plain_title("Branch stash"),
            PickerAction::StashBranchName { .. } => transient::plain_title("Branch name"),
            PickerAction::WorktreeAddRef => transient::plain_title("Worktree for ref"),
            PickerAction::WorktreeBranchName => transient::plain_title("New branch name"),
            PickerAction::WorktreeAddDir { .. }
            | PickerAction::WorktreeBranchDir { .. }
            | PickerAction::WorktreeMoveTo { .. } => transient::plain_title("Worktree directory"),
            PickerAction::RefsRename { old } => transient::plain_title(format!("Rename {old} to")),
            PickerAction::PatchApply => transient::plain_title("Apply patch file"),
            PickerAction::PatchAm => transient::plain_title("Apply mailbox (am)"),
            PickerAction::PatchCreate => transient::plain_title("format-patch"),
            PickerAction::BisectBadRev => transient::plain_title("Bisect: known-bad revision"),
            PickerAction::BisectGoodRev { .. } => {
                transient::plain_title("Bisect: known-good revision")
            }
        }
    }

    /// Notice shown when a selection-only picker (one you can't type into) turns
    /// up no candidates, so it closes instead of presenting an empty list. Only
    /// the selection-only actions need a real message; value-entry pickers stay
    /// open regardless and never use this.
    pub(crate) fn empty_message(&self) -> &'static str {
        match self {
            PickerAction::Stash(_) | PickerAction::StashBranchStash => "No stashes",
            PickerAction::Branch(_) => "No branches",
            PickerAction::Tag(_) | PickerAction::PushTagSelect => "No tags",
            PickerAction::Remote(_) => "No remotes configured",
            PickerAction::Transfer(_) => "No remotes configured",
            PickerAction::FileCheckoutFile { .. } => "No files in that revision",
            _ => "Nothing to select",
        }
    }

    /// Imperative verb for the confirm key hint.
    pub(crate) fn confirm_label(&self) -> &'static str {
        match self {
            PickerAction::Transfer(
                Transfer::Push { .. }
                | Transfer::PushRef { .. }
                | Transfer::PushTag { .. }
                | Transfer::PushTags,
            ) => "push",
            PickerAction::Transfer(Transfer::Pull { .. } | Transfer::PullRef) => "pull",
            PickerAction::Transfer(Transfer::Fetch) => "fetch",
            PickerAction::PushOtherSource => "next",
            PickerAction::PushTagSelect => "next",
            PickerAction::Branch(BranchAction::Checkout) => "checkout",
            PickerAction::Branch(BranchAction::Create { .. }) => "create",
            PickerAction::Branch(BranchAction::RenameFrom | BranchAction::RenameTo { .. }) => {
                "rename"
            }
            PickerAction::Branch(BranchAction::Delete) => "delete",
            PickerAction::Branch(BranchAction::Configure) => "configure",
            PickerAction::Tag(TagAction::Create { .. }) => "tag",
            PickerAction::Tag(TagAction::Release { .. }) => "release",
            PickerAction::Tag(TagAction::Delete) => "delete",
            PickerAction::Remote(RemoteAction::AddName | RemoteAction::AddUrl { .. }) => "add",
            PickerAction::Remote(RemoteAction::RenameFrom | RemoteAction::RenameTo { .. }) => {
                "rename"
            }
            PickerAction::Remote(RemoteAction::Remove) => "remove",
            PickerAction::Remote(RemoteAction::Configure) => "configure",
            PickerAction::Stash(StashAction::Apply) => "apply",
            PickerAction::Stash(StashAction::Pop) => "pop",
            PickerAction::Stash(StashAction::Drop) => "drop",
            PickerAction::SetOption { .. } | PickerAction::SetVariable { .. } => "set",
            PickerAction::ResetBranch => "next",
            PickerAction::ResetBranchTo { .. } => "reset",
            PickerAction::FileCheckoutRev => "next",
            PickerAction::FileCheckoutFile { .. } => "checkout",
            PickerAction::MergePreview => "preview",
            PickerAction::StashBranchStash => "next",
            PickerAction::StashBranchName { .. } => "branch",
            PickerAction::LogRef { .. } => "log",
            PickerAction::LogFile { .. } => "log",
            PickerAction::DiffRange { .. } => "diff",
            PickerAction::DiffCommit { .. } => "show",
            PickerAction::RunCommand => "run",
            PickerAction::Reset(_) => "reset",
            PickerAction::Merge { .. } => "merge",
            PickerAction::Rebase => "rebase",
            PickerAction::PickRange(PickOp::CherryPick | PickOp::CherryApply) => "pick",
            PickerAction::PickRange(PickOp::Revert | PickOp::RevertNoCommit) => "revert",
            PickerAction::Run { .. } => "run",
            PickerAction::Ignore(_) => "ignore",
            PickerAction::StashMessage { .. } => "stash",
            PickerAction::WorktreeAddRef | PickerAction::WorktreeBranchName => "next",
            PickerAction::WorktreeAddDir { .. } | PickerAction::WorktreeBranchDir { .. } => "add",
            PickerAction::WorktreeMoveTo { .. } => "move",
            PickerAction::RefsRename { .. } => "rename",
            PickerAction::PatchApply | PickerAction::PatchAm => "apply",
            PickerAction::PatchCreate => "create",
            PickerAction::BisectBadRev => "next",
            PickerAction::BisectGoodRev { .. } => "start",
        }
    }
}

/// An open target picker (vertico-style): a prompt, an inline query input, a
/// ranked candidate list, and the pending action. It runs against the
/// highlighted (or clicked) candidate on Enter.
pub(crate) struct PickerState {
    /// The minibuffer-style prompt as styled spans, e.g. `Push `[main]` to` (the
    /// `:` and the typed text are rendered after it).
    pub(crate) prompt: Vec<TitleSpan>,
    /// The bare query input (type-to-filter).
    pub(crate) input: Entity<InputState>,
    /// The filter/rank/select model over the candidates.
    pub(crate) list: PickerList,
    /// Scrolls the (virtualized) candidate rows.
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) action: PickerAction,
    pub(crate) switches: Vec<String>,
    /// Candidates are still loading off the UI thread (shows "Loading…" in the
    /// reserved candidate area instead of "No match"). See `open_listed_picker`.
    pub(crate) loading: bool,
    /// Identifies this picker instance, so an async candidate load only fills
    /// the picker it was started for — not a later one the user opened meanwhile.
    pub(crate) gen: u64,
    /// Whether to reserve the fixed candidate-list area. True for every picker
    /// with candidates (so its height stays stable while filtering, and doesn't
    /// jump when async candidates load); false only for a pure free-text value
    /// prompt (e.g. `-n`), which collapses to just the input + hints.
    pub(crate) reserve_candidates: bool,
    /// A transient to reopen when this picker confirms or cancels — used when a
    /// transient option prompts for its value, so the menu comes back after.
    /// Boxed to keep the (already large) picker state from dominating `Popup`.
    pub(crate) resume: Option<Box<TransientState>>,
    /// Kept alive so the input-change subscription stays active.
    pub(crate) _sub: Subscription,
    /// Per-label `(key hint, command id)` for the `:` palette's rows, resolved
    /// once per picker: `command_keys` walks the transient definitions to find
    /// a leaf's path, which is too heavy to repeat per row per frame. RefCell
    /// because render fills it with `&self`.
    pub(crate) hints: std::cell::RefCell<PaletteHints>,
}

/// Cached palette-row metadata by label — see [`PickerState::hints`].
pub(crate) type PaletteHints =
    std::collections::HashMap<SharedString, (Option<SharedString>, Option<SharedString>)>;

/// The key a transient suffix is invoked by, for matching `[transient]`
/// `"key" = "unbound"` removals. `None` for `Info` rows (no toggle key).
pub(crate) fn suffix_key(s: &Suffix) -> Option<&str> {
    match s {
        Suffix::Switch(sw) => Some(&sw.key),
        Suffix::Action(a) => Some(a.key),
        Suffix::Option(o) => Some(o.key),
        Suffix::Custom(c) => Some(&c.key),
        Suffix::Variable(v) => Some(&v.key),
        Suffix::Info(_) => None,
    }
}

/// Apply a user's `[transient.<id>]` entries to a built-in definition, in
/// config order: `"unbound"` removals first, then each remaining entry either
/// injects a new suffix (a switch or an action) or — when it carries only
/// placement fields — moves the suffix bound at its key. Because entries apply
/// sequentially, a later one can place relative to an earlier injection.
/// `describe_action` labels an injected action from its command id.
pub(crate) fn apply_user_suffixes(
    def: &mut Transient,
    entries: &indexmap::IndexMap<String, config::TransientSuffix>,
    describe_action: impl Fn(&str) -> String,
) {
    // `"key" = "unbound"` removes the built-in suffix at that key
    // (keymap-style), so a user can drop a default flag/action.
    let unbinds: std::collections::HashSet<&str> = entries
        .iter()
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

    for (key, spec) in entries {
        match spec.kind() {
            // The `"unbound"` removal entries (handled above).
            _ if spec.is_unbound() => {}
            // A custom switch (toggleable git flag). Skip if the key collides
            // with a built-in switch/option (which wins).
            config::SuffixKind::Switch {
                flag,
                description,
                placement,
            } => {
                if def.switches().any(|s| s.key == *key) || def.option_for(key).is_some() {
                    continue;
                }
                let suffix = transient::Suffix::Switch(transient::Switch::new(
                    key.clone(),
                    flag.to_string(),
                    description.to_string(),
                ));
                insert_suffix(def, suffix, key, placement, "Arguments");
            }
            // A custom action runs a command by id. Skip if the key collides
            // with a built-in action (which wins).
            config::SuffixKind::Action { id, placement } => {
                if def.action_for(key).is_some() {
                    continue;
                }
                let suffix = transient::Suffix::Custom(transient::Custom {
                    key: key.clone(),
                    description: describe_action(id),
                    id: id.to_string(),
                });
                insert_suffix(def, suffix, key, placement, "Custom");
            }
            config::SuffixKind::Move(placement) => move_suffix(def, key, placement),
        }
    }
}

/// Insert an injected suffix at its placement: next to the `before`/`after`
/// key when that resolves, else appended into the `group` (or the kind's
/// default section — where switches or actions respectively live).
fn insert_suffix(
    def: &mut Transient,
    suffix: Suffix,
    key: &str,
    placement: &config::Placement,
    default_group: &str,
) {
    if let Some((gi, si)) = placement_index(def, key, placement) {
        def.groups[gi].suffixes.insert(si, suffix);
        return;
    }
    let title = placement.group.as_deref().unwrap_or(default_group);
    append_to_group(def, title, suffix);
}

/// Move the suffix bound at `key` (a built-in, or an earlier injection) to the
/// entry's placement. With no resolvable destination it stays put; a move that
/// empties its old section drops the section, like an unbind.
fn move_suffix(def: &mut Transient, key: &str, placement: &config::Placement) {
    let Some((gi, si)) = find_suffix(def, key) else {
        return;
    };
    if placement_index(def, key, placement).is_none() && placement.group.is_none() {
        return;
    }
    let suffix = def.groups[gi].suffixes.remove(si);
    // Reinsert before sweeping the (possibly emptied) source section, so the
    // re-resolved indices stay valid. A relative target that resolved above
    // still does — it's a different, still-present suffix — leaving `None` to
    // the `group` fallback checked above.
    match placement_index(def, key, placement) {
        Some((gi, si)) => def.groups[gi].suffixes.insert(si, suffix),
        None => append_to_group(def, placement.group.as_deref().unwrap_or_default(), suffix),
    }
    def.groups.retain(|g| !g.suffixes.is_empty());
}

/// The insertion position a `before`/`after` placement names: the target key's
/// slot (plus one for `after`). `None` when the placement has no relative
/// part, the target key isn't in the transient, or it names the entry itself.
fn placement_index(
    def: &Transient,
    self_key: &str,
    placement: &config::Placement,
) -> Option<(usize, usize)> {
    let (target, offset) = match (&placement.before, &placement.after) {
        (Some(t), _) => (t, 0),
        (None, Some(t)) => (t, 1),
        (None, None) => return None,
    };
    if target == self_key {
        return None;
    }
    find_suffix(def, target).map(|(gi, si)| (gi, si + offset))
}

/// The (group, suffix) position of the suffix invoked by `key`, if any.
fn find_suffix(def: &Transient, key: &str) -> Option<(usize, usize)> {
    def.groups.iter().enumerate().find_map(|(gi, g)| {
        g.suffixes
            .iter()
            .position(|s| suffix_key(s) == Some(key))
            .map(|si| (gi, si))
    })
}

/// Append into the section with this title if it exists, else create it.
fn append_to_group(def: &mut Transient, title: &str, suffix: Suffix) {
    match def.groups.iter_mut().find(|g| group_text(g) == title) {
        Some(g) => g.suffixes.push(suffix),
        None => def.groups.push(transient::Group {
            title: transient::plain_title(title),
            suffixes: vec![suffix],
        }),
    }
}

impl StatusView {
    /// Open a transient, applying any user-configured suffixes for it. `id` is
    /// the transient's command id (`branch`, `commit`, …); pass `""` for ad-hoc
    /// transients (e.g. an in-progress sequence) that take no user suffixes.
    pub(crate) fn open_transient(
        &mut self,
        id: &str,
        mut def: Transient,
        targets: RemoteTargets,
        cx: &mut Context<Self>,
    ) {
        // Gate injection on the documented id list, so config validation and
        // behavior agree: an id the loader warned about ("not a transient" —
        // the run prompt, the jump menu, the configure sub-menus) is also not
        // silently customized here.
        if let Some(entries) = self
            .config
            .transient
            .get(id)
            .filter(|_| crate::commands::TRANSIENT_IDS.contains(&id))
        {
            // An action is labeled with its command's title (built-in or user,
            // placeholders expanded), falling back to the raw id if it names
            // nothing.
            apply_user_suffixes(&mut def, entries, |cid| {
                all_commands(&self.config)
                    .find(|c| c.id == cid)
                    .map(|c| self.expand_placeholders_display(c.title))
                    .unwrap_or_else(|| cid.to_string())
            });
        }
        // A switch tied to a git-config key starts on when that config is
        // enabled (e.g. --gpg-sign with commit.gpgSign=true); toggling it off
        // then sends the negation (--no-gpg-sign). Resolve those defaults now,
        // from the repo's effective config.
        if let Some(repo) = self.repo.clone() {
            let branch = targets.branch.clone();
            for group in def.groups.iter_mut() {
                for suffix in group.suffixes.iter_mut() {
                    if let transient::Suffix::Switch(sw) = suffix {
                        if let Some(key) = sw.config_key.clone() {
                            sw.default_on =
                                self.transient_config_default(&repo, &key, branch.as_deref());
                        }
                    }
                }
            }
            // Config-variable rows show their live values (and any fallback).
            // Each is a `git config` subprocess, so they're read off the UI
            // thread *before* the popup opens (values popping into an already-
            // open menu reads as flicker), cached per refresh generation so
            // reopening the same transient is instant.
            let var_keys: Vec<String> = def
                .variables_mut()
                .flat_map(|v| {
                    let fallback = match &v.kind {
                        transient::VariableKind::Choices { fallback, .. } => {
                            fallback.as_ref().map(|f| f.to_string())
                        }
                        _ => None,
                    };
                    std::iter::once(v.variable.clone()).chain(fallback)
                })
                .collect();
            let missing: Vec<String> = var_keys
                .iter()
                .filter(|k| !self.transient_config_values.contains_key(*k))
                .cloned()
                .collect();
            if !missing.is_empty() {
                // Load the uncached values in the background, then open. The
                // ~tens-of-ms delay reads as an instant open (magit reads the
                // same config synchronously), and there's no pop-in. Stamped so
                // a superseded or cancelled open (Escape, another popup opened
                // within the load window) is dropped rather than installing its
                // popup late over whatever the user is looking at now.
                let id = id.to_string();
                let gen = self.transient_open_gen.bump();
                cx.spawn(async move |this, cx| {
                    let values = cx
                        .background_executor()
                        .spawn(async move {
                            missing
                                .into_iter()
                                .map(|key| {
                                    let value = repo.config_get(&key).ok().flatten();
                                    (key, value)
                                })
                                .collect::<Vec<_>>()
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        // Keep the cache fill either way; open only if wanted.
                        this.transient_config_values.extend(values);
                        if !this.transient_open_gen.is_current(gen) || this.popup.is_some() {
                            return;
                        }
                        this.fill_transient_variables(&mut def);
                        this.finish_open_transient(&id, def, targets, cx);
                    })
                    .ok();
                })
                .detach();
                return;
            }
            self.fill_transient_variables(&mut def);
        }
        self.finish_open_transient(id, def, targets, cx);
    }

    /// Resolve each config-variable row's value (and fallback) from the
    /// per-refresh cache — see [`open_transient`](Self::open_transient).
    fn fill_transient_variables(&self, def: &mut Transient) {
        for var in def.variables_mut() {
            var.value = self
                .transient_config_values
                .get(&var.variable)
                .cloned()
                .flatten();
            if let transient::VariableKind::Choices {
                fallback: Some(fallback),
                ..
            } = &var.kind
            {
                var.fallback_value = self
                    .transient_config_values
                    .get(fallback.as_str())
                    .cloned()
                    .flatten();
            }
        }
    }

    /// The tail of [`open_transient`](Self::open_transient): wrap the resolved
    /// definition in its state (applying any saved argument defaults) and show
    /// it. Split out so a variable-value load can finish the open on land.
    fn finish_open_transient(
        &mut self,
        id: &str,
        def: Transient,
        targets: RemoteTargets,
        cx: &mut Context<Self>,
    ) {
        // Any transient actually opening supersedes a still-pending open.
        self.transient_open_gen.bump();
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
    pub(crate) fn saved_arguments(&self, id: &str) -> Option<&Vec<String>> {
        self.repo_transient_arguments
            .get(id)
            .or_else(|| self.transient_arguments.get(id))
    }

    /// Persist the open transient's argument overrides to a scope (magit's
    /// `transient-save`), updating the in-memory set and the scope's file, and
    /// re-baselining so the save hint hides. Repo scope is a no-op with no repo.
    pub(crate) fn save_transient_defaults(&mut self, repo_scope: bool, cx: &mut Context<Self>) {
        let to_save = match &self.popup {
            Some(Popup::Transient(s)) if !s.id.is_empty() => {
                Some((s.id.clone(), s.saved_overrides()))
            }
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
        let scope = if repo_scope {
            "for this repo"
        } else {
            "globally"
        };
        self.set_status(format!("Saved arguments {scope}"), true, cx);
    }

    /// The current branch's resolved push/pull/fetch targets (empty on error or
    /// no repo), for building and dispatching the remote transients.
    pub(crate) fn remote_targets(&self) -> RemoteTargets {
        if let Some(status) = self.status.as_ref() {
            // `from_head` reuses the parsed status; add the remote list so the
            // menus can name the sole remote an unconfigured target would use.
            let remotes = self
                .repo
                .as_ref()
                .and_then(|r| r.remotes().ok())
                .unwrap_or_default();
            return RemoteTargets::from_head(&status.head).with_remotes(&remotes);
        }
        self.repo
            .as_ref()
            .and_then(|r| r.remote_targets().ok())
            .unwrap_or_default()
    }

    pub(crate) fn transient_config_default(
        &mut self,
        repo: &Repo,
        key: &str,
        branch: Option<&str>,
    ) -> bool {
        let cache_key = if key == "pull.rebase" {
            format!("{key}\0{}", branch.unwrap_or_default())
        } else {
            key.to_string()
        };
        if let Some(value) = self.transient_config_defaults.get(&cache_key) {
            return *value;
        }
        let value = match key {
            // pull.rebase is an enum (true/interactive/merges) with a
            // per-branch override, so it needs git's own resolution rather than
            // a plain bool read.
            "pull.rebase" => repo.pull_rebase_default(branch),
            _ => repo.config_bool(key),
        };
        self.transient_config_defaults.insert(cache_key, value);
        value
    }

    /// Invoke a config-variable row in the open transient: cycle its choices in
    /// place (writing immediately), or open a prompt for a free-text value.
    pub(crate) fn set_variable_at(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Copy out what we need so the popup borrow ends before we act.
        let info = match &self.popup {
            Some(Popup::Transient(state)) => state.def.variable_for(key).map(|var| {
                (
                    var.variable.clone(),
                    var.description.clone(),
                    var.value.clone(),
                    var.kind.clone(),
                )
            }),
            _ => None,
        };
        let Some((variable, description, value, kind)) = info else {
            return;
        };
        match kind {
            transient::VariableKind::Choices { choices, .. } => {
                let next = cycle_choice(&choices, value.as_deref());
                self.write_variable(key, &variable, next, cx);
            }
            transient::VariableKind::Value { completion } => {
                if let Some(Popup::Transient(ts)) = self.popup.take() {
                    self.refresh_blocker_closed(cx);
                    self.open_variable_prompt(
                        variable,
                        description,
                        completion,
                        value.unwrap_or_default(),
                        ts,
                        window,
                        cx,
                    );
                }
            }
        }
    }

    /// Write (or unset, when `value` is `None`) a git-config variable, update the
    /// open transient's row in place, and refresh — a config change can move the
    /// title bar / status (e.g. `pushRemote`, `rebase`).
    pub(crate) fn write_variable(
        &mut self,
        key: &str,
        variable: &str,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let result = match &value {
            Some(v) => repo.config_set(variable, v),
            None => repo.config_unset(variable),
        };
        if let Err(e) = result {
            self.set_status(e.to_string(), false, cx);
            return;
        }
        if let Some(Popup::Transient(state)) = self.popup.as_mut() {
            if let Some(var) = state.def.variable_for_mut(key) {
                var.value = value;
            }
        }
        self.refresh(cx);
        cx.notify();
    }

    pub(crate) fn handle_transient_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        // Esc always closes; `q` does too, unless it's meaningful input to the
        // open transient — completing a pending `-`/multi-key sequence, or a
        // user-injected suffix bound at `q` (built-ins never use it).
        let q_closes = key == "q"
            && !matches!(&self.popup, Some(Popup::Transient(s)) if q_is_transient_input(s));
        if key == "escape" || q_closes {
            // A Configure sub-transient (reached via `b C` / `M C`) pops back to
            // its parent transient rather than closing outright.
            match self.popup {
                Some(Popup::Transient(ref s)) if s.id == "branch-configure" => {
                    self.open_branch_transient(cx)
                }
                Some(Popup::Transient(ref s)) if s.id == "remote-configure" => {
                    self.open_remote_transient(cx)
                }
                _ => {
                    self.popup = None;
                    self.refresh_blocker_closed(cx);
                    cx.notify();
                }
            }
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
                state.toggle_switch(&full);
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
                    self.refresh_blocker_closed(cx);
                    self.open_option_prompt(key, description, completion, ts, window, cx);
                }
                return;
            }
            self.report_unbound_suffix(&full, cx);
            return;
        }
        if key == "-" {
            state.pending_dash = true;
            state.pending_key.clear();
            cx.notify();
            return;
        }

        // A config-variable row: cycle its choices in place, or prompt for a
        // free-text value. Handled before the multi-key/action resolution since
        // variables are always single-key.
        if state.pending_key.is_empty() && state.def.variable_for(key).is_some() {
            self.set_variable_at(key, window, cx);
            return;
        }

        // Multi-key suffixes (magit's `fu`/`pu` jump keys): accumulate the
        // keystrokes while they still prefix some suffix key; a full match
        // fires below like any single-key suffix.
        let candidate = format!("{}{key}", state.pending_key);
        state.pending_key.clear();
        if state.def.action_for(&candidate).is_none() && state.def.custom_for(&candidate).is_none()
        {
            if state.def.has_key_prefix(&candidate) {
                state.pending_key = candidate;
                cx.notify();
                return;
            }
            if candidate != key {
                // An accumulated sequence that resolves to nothing: swallow it
                // rather than firing the lone final key.
                self.report_unbound_suffix(&candidate, cx);
                return;
            }
        }

        // Invoke an action — or a user-injected custom suffix (which runs a
        // registry command by id, with default args).
        let action = state.def.action_for(&candidate).cloned();
        let custom = state.def.custom_for(&candidate).cloned();
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
            self.refresh_blocker_closed(cx);
            self.invoke_command(&custom.id, window, cx);
        } else {
            self.report_unbound_suffix(&candidate, cx);
        }
    }

    /// Feedback for a key that means nothing in the open transient (magit
    /// echoes similarly). Modifier chords are ignored — they're usually an
    /// OS/app shortcut, matching the main keymap's convention.
    fn report_unbound_suffix(&mut self, key: &str, cx: &mut Context<Self>) {
        let plain = !["cmd-", "ctrl-", "alt-"].iter().any(|m| key.contains(m));
        if plain {
            self.set_status("is unbound in this menu".to_string(), true, cx);
            self.toast.keys = Some(key.to_string());
        }
        cx.notify();
    }
}

/// Whether a bare `q` is meaningful input to this transient rather than the
/// close key: it completes a pending `-` switch toggle or multi-key sequence,
/// or the definition binds a suffix at `q` (no built-in does, but a user
/// `[transient.*]` injection can).
pub(crate) fn q_is_transient_input(state: &TransientState) -> bool {
    state.pending_dash
        || !state.pending_key.is_empty()
        || state.def.action_for("q").is_some()
        || state.def.custom_for("q").is_some()
        || state.def.variable_for("q").is_some()
        || state.def.has_key_prefix("q")
}

/// The next value when cycling a choice variable (magit's
/// `(cadr (member value choices))`): unset → first, each choice → the next, and
/// the last choice → unset (`None`). A current value not among the choices
/// (e.g. a stale remote) restarts at the first.
pub(crate) fn cycle_choice(choices: &[String], current: Option<&str>) -> Option<String> {
    match current.and_then(|c| choices.iter().position(|x| x == c)) {
        Some(i) if i + 1 < choices.len() => Some(choices[i + 1].clone()),
        Some(_) => None,
        None => choices.first().cloned(),
    }
}

/// The switch keys that must deactivate when the switch bound to `key`
/// toggles on: every other switch declared mutually exclusive with it, in
/// either direction (so one side's declaration suffices).
pub(crate) fn conflicting_switch_keys(def: &Transient, key: &str) -> Vec<String> {
    let Some(sw) = def.switches().find(|s| s.key == key) else {
        return Vec::new();
    };
    def.switches()
        .filter(|other| {
            other.key != key
                && (sw.exclusive_with.contains(&other.arg)
                    || other.exclusive_with.contains(&sw.arg))
        })
        .map(|other| other.key.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_transient::{Command, Switch};
    use indexmap::IndexMap;

    /// A miniature commit-like transient: Arguments (`-a`, `-s`), Create (`c`),
    /// Edit HEAD (`e`).
    fn fixture() -> Transient {
        Transient {
            title: transient::plain_title("Commit"),
            groups: vec![
                Group {
                    title: transient::plain_title("Arguments"),
                    suffixes: vec![
                        Suffix::Switch(Switch::new("-a", "--all", "Stage all")),
                        Suffix::Switch(Switch::new("-s", "--signoff", "Signoff")),
                    ],
                },
                Group {
                    title: transient::plain_title("Create"),
                    suffixes: vec![transient::Action::suffix(
                        "c",
                        "Commit",
                        Command::CommitCreate,
                    )],
                },
                Group {
                    title: transient::plain_title("Edit HEAD"),
                    suffixes: vec![transient::Action::suffix(
                        "e",
                        "Extend",
                        Command::CommitCreate,
                    )],
                },
            ],
        }
    }

    /// `(section title, suffix keys)` per group — the whole layout as one
    /// comparable value.
    fn layout(def: &Transient) -> Vec<(String, Vec<String>)> {
        def.groups
            .iter()
            .map(|g| {
                let keys = g
                    .suffixes
                    .iter()
                    .filter_map(|s| suffix_key(s).map(str::to_string))
                    .collect();
                (group_text(g), keys)
            })
            .collect()
    }

    /// Parse `[transient.<id>]` entries from TOML and apply them, labeling
    /// injected actions `cmd:<id>`.
    fn apply(def: &mut Transient, entries: &str) {
        let entries: IndexMap<String, config::TransientSuffix> = toml::from_str(entries).unwrap();
        apply_user_suffixes(def, &entries, |id| format!("cmd:{id}"));
    }

    #[test]
    fn injects_a_switch_after_a_key() {
        let mut def = fixture();
        apply(&mut def, r#""-n" = { flag = "--no-verify", after = "-a" }"#);
        assert_eq!(layout(&def)[0].1, ["-a", "-n", "-s"]);
    }

    #[test]
    fn injects_an_action_before_a_key_in_that_group() {
        let mut def = fixture();
        apply(&mut def, r#""x" = { command = "user.x", before = "c" }"#);
        assert_eq!(layout(&def)[1].1, ["x", "c"]);
        let Suffix::Custom(custom) = &def.groups[1].suffixes[0] else {
            panic!("expected the injected custom suffix");
        };
        assert_eq!(custom.description, "cmd:user.x");
    }

    #[test]
    fn missing_target_falls_back_to_group_then_default() {
        let mut def = fixture();
        apply(
            &mut def,
            r#""-n" = { flag = "--x", after = "-z", group = "Create" }"#,
        );
        assert_eq!(layout(&def)[1].1, ["c", "-n"]);

        let mut def = fixture();
        apply(&mut def, r#""-n" = { flag = "--x", after = "-z" }"#);
        assert_eq!(layout(&def)[0].1, ["-a", "-s", "-n"]);
    }

    #[test]
    fn entries_apply_in_config_order_and_can_reference_each_other() {
        let mut def = fixture();
        apply(
            &mut def,
            "\"z\" = \"user.z\"\n\"y\" = { command = \"user.y\", before = \"z\" }\n",
        );
        let custom = layout(&def)
            .into_iter()
            .find(|(title, _)| title == "Custom")
            .expect("created Custom section");
        assert_eq!(custom.1, ["y", "z"]);
    }

    #[test]
    fn moves_a_builtin_within_its_group() {
        let mut def = fixture();
        apply(&mut def, r#""-s" = { before = "-a" }"#);
        assert_eq!(layout(&def)[0].1, ["-s", "-a"]);
    }

    #[test]
    fn moving_the_last_suffix_out_drops_the_emptied_section() {
        let mut def = fixture();
        apply(&mut def, r#""c" = { after = "e" }"#);
        let layout = layout(&def);
        assert!(
            layout.iter().all(|(title, _)| title != "Create"),
            "emptied section dropped: {layout:?}"
        );
        assert_eq!(
            layout[1],
            ("Edit HEAD".into(), vec!["e".into(), "c".into()])
        );
    }

    #[test]
    fn moves_a_builtin_into_a_created_group() {
        let mut def = fixture();
        apply(&mut def, r#""c" = { group = "Extras" }"#);
        let layout = layout(&def);
        assert_eq!(
            layout.last().unwrap(),
            &("Extras".to_string(), vec!["c".to_string()])
        );
    }

    #[test]
    fn unresolvable_moves_are_noops() {
        let mut def = fixture();
        let before = layout(&def);
        apply(&mut def, r#""c" = { after = "zz" }"#); // unknown target
        apply(&mut def, r#""zz" = { after = "c" }"#); // unknown key
        apply(&mut def, r#""c" = { after = "c" }"#); // names itself
        assert_eq!(layout(&def), before);
    }

    #[test]
    fn unbinds_apply_before_placements() {
        let mut def = fixture();
        apply(
            &mut def,
            "\"-a\" = \"unbound\"\n\"-n\" = { flag = \"--no-verify\", after = \"-s\" }\n",
        );
        assert_eq!(layout(&def)[0].1, ["-s", "-n"]);
    }

    #[test]
    fn colliding_injection_still_loses_to_the_builtin() {
        let mut def = fixture();
        apply(&mut def, r#""-a" = { flag = "--other", before = "-s" }"#);
        assert_eq!(layout(&def)[0].1, ["-a", "-s"]);
    }

    #[test]
    fn q_stays_input_while_pending_or_bound_to_a_suffix() {
        let plain = |def| TransientState::new("commit", def, RemoteTargets::default());
        // Built-in transients don't bind `q`, so it reads as close…
        assert!(!q_is_transient_input(&plain(fixture())));
        // …but a pending `-` awaits the switch letter (a user `-q` switch),
        let mut pending_dash = plain(fixture());
        pending_dash.pending_dash = true;
        assert!(q_is_transient_input(&pending_dash));
        // …a pending multi-key sequence consumes it,
        let mut pending_key = plain(fixture());
        pending_key.pending_key = "f".to_string();
        assert!(q_is_transient_input(&pending_key));
        // …and a user-injected action at `q` claims the key outright.
        let mut def = fixture();
        apply(&mut def, r#""q" = "user.quick""#);
        assert!(q_is_transient_input(&plain(def)));
    }

    #[test]
    fn cycle_choice_wraps_through_unset() {
        let choices = vec!["true".to_string(), "false".to_string()];
        // unset → first → second → unset → first …
        assert_eq!(cycle_choice(&choices, None).as_deref(), Some("true"));
        assert_eq!(
            cycle_choice(&choices, Some("true")).as_deref(),
            Some("false")
        );
        assert_eq!(cycle_choice(&choices, Some("false")), None);
        // A value not among the choices (a stale remote) restarts at the first.
        assert_eq!(
            cycle_choice(&choices, Some("gone")).as_deref(),
            Some("true")
        );
        // No choices → always unset.
        assert_eq!(cycle_choice(&[], Some("x")), None);
    }
}

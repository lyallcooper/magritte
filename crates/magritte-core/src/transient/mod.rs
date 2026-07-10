//! The transient model — magit's popup command menus (`P` push, `F` pull,
//! `f` fetch, `c` commit, …).
//!
//! A [`Transient`] is a declarative tree of groups and suffixes (switches and
//! actions). The model is UI-agnostic: it carries keys and descriptions as
//! data, but knows nothing about rendering. The frontend renders the popup,
//! tracks which switches are toggled on, and dispatches keys; when an action
//! fires it runs the [`Command`]'s operation with the active switch arguments.
//!
//! This module holds the generic model (the types and their methods); the
//! concrete built-in menu definitions live in the `menus` submodule, whose
//! builders are re-exported here.

mod menus;
pub use menus::*;

/// Which built-in key style to use for transient suffixes that differ between
/// vanilla Magit and evil-collection-magit. The commands are the same; only the
/// default keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapStyle {
    EvilCollection,
    Vanilla,
}

impl KeymapStyle {
    /// The delete/remove key for this preset (evil `x`, vanilla/Magit `k`),
    /// shared by the branch/tag/remote transients.
    fn delete_key(self) -> &'static str {
        match self {
            KeymapStyle::EvilCollection => "x",
            KeymapStyle::Vanilla => "k",
        }
    }
}

/// The git operation an [`Action`] runs. Push/pull/fetch come in magit's three
/// flavors — to the push-remote, to the upstream, or elsewhere (the frontend
/// resolves the actual remote, prompting when unconfigured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// The `!` run transient's variants (magit's `magit-run`): a git
    /// subcommand or a shell command, in the repository root or the
    /// working directory of the file at point.
    RunGitTopdir,
    RunGitWorkdir,
    RunShellTopdir,
    RunShellWorkdir,
    PushPushRemote,
    PushUpstream,
    PushElsewhere,
    /// Push an arbitrary local branch/rev to a chosen remote branch
    /// (magit-push-other; both ends are prompted for).
    PushOther,
    /// Push one tag (prompts for the tag, then resolves the remote).
    PushTag,
    /// Push all tags (`--tags`) to a resolved remote.
    PushTags,
    PullPushRemote,
    PullUpstream,
    PullElsewhere,
    FetchPushRemote,
    FetchUpstream,
    FetchAll,
    FetchElsewhere,
    /// New commit (needs a message — handled via the editor, not `execute`).
    CommitCreate,
    /// Amend HEAD (needs a message).
    CommitAmend,
    /// Reword HEAD (needs a message).
    CommitReword,
    /// Reword an older commit using an interactive rebase — the commit
    /// transient's `c R`. Distinct from [`Command::RebaseRewordCommit`] (`r w`)
    /// because the hosting transient's switches differ: commit switches (e.g.
    /// `--date=now`) are not valid rebase options, so this variant drops them,
    /// while `r w` carries the rebase transient's switches through.
    CommitRewordPast,
    /// Amend HEAD with staged changes, keeping its message.
    CommitExtend,
    /// Create a `fixup!` commit targeting the commit at point / a selected one.
    CommitFixup,
    /// Create a `squash!` commit targeting the commit at point / a selected one.
    CommitSquash,
    /// Create a `fixup!` commit and immediately autosquash it into its target.
    CommitInstantFixup,
    /// Create a `squash!` commit and immediately autosquash it into its target.
    CommitInstantSquash,
    /// Check out an existing branch/revision (the frontend prompts).
    BranchCheckout,
    /// Create a new branch and check it out (prompts for a name).
    BranchCreateCheckout,
    /// Create a new branch without checking it out (prompts for a name).
    BranchCreate,
    /// Rename a branch (prompts for the branch, then the new name).
    BranchRename,
    /// Delete a branch (prompts for the branch).
    BranchDelete,
    /// Open the branch config transient (git-config variables for a branch).
    BranchConfigure,
    /// Create a lightweight tag at point/HEAD (prompts for name).
    TagCreate,
    /// Create the next release tag on HEAD (proposes the name/message).
    TagRelease,
    /// Delete a local tag (prompts for the tag).
    TagDelete,
    /// Add a remote (prompts for name then URL).
    RemoteAdd,
    /// Rename a remote (prompts for old then new name).
    RemoteRename,
    /// Remove a remote (prompts for name).
    RemoteRemove,
    /// Open the remote config transient (git-config variables for a remote).
    RemoteConfigure,
    /// Stash the working tree and index.
    StashPush,
    /// Stash including untracked files.
    StashPushAll,
    /// Stash only the staged changes (`--staged`).
    StashPushStaged,
    /// Stash worktree and index but leave the index applied (`--keep-index`).
    StashPushKeepIndex,
    /// Apply a stash, keeping it (prompts for which).
    StashApply,
    /// Pop a stash (prompts for which).
    StashPop,
    /// Drop a stash (prompts for which).
    StashDrop,
    /// Create and check out a branch from a stash (`git stash branch`), picking
    /// the stash then prompting for the branch name.
    StashBranch,
    /// Diff the context-sensitive target, usually unstaged/staged/commit.
    DiffDwim,
    /// Diff an arbitrary revision or range.
    DiffRange,
    /// Diff unstaged worktree changes (`git diff`).
    DiffUnstaged,
    /// Diff staged/index changes (`git diff --cached`).
    DiffStaged,
    /// Diff the whole working tree against a revision (`git diff HEAD`).
    DiffWorktree,
    /// Show a single commit (message + diff).
    DiffCommit,
    /// Log the current branch (HEAD).
    LogCurrent,
    /// Log all branches (`--all`).
    LogAll,
    /// Log another ref (prompts for one).
    LogOther,
    /// Log one file's history (the file at point, else prompts for one).
    LogFile,
    /// Reflog of HEAD.
    LogReflog,
    /// Reset HEAD to a commit (the frontend prompts for the target). The mode
    /// is in the variant; hard is confirmed by the frontend.
    ResetSoft,
    ResetMixed,
    ResetHard,
    ResetKeep,
    ResetIndex,
    ResetWorktree,
    /// Reset a *branch* (not HEAD) to a picked revision (magit-branch-reset):
    /// the current branch hard-resets, any other moves via `update-ref`.
    ResetBranch,
    /// Check one file out of a picked revision (magit-file-checkout).
    ResetFile,
    /// Merge a branch/ref into HEAD (the frontend prompts for it).
    MergePlain,
    /// Merge but don't commit (`--no-commit`).
    MergeNoCommit,
    /// Squash-merge (`--squash`): stage the result without a merge commit.
    MergeSquash,
    /// Merge and edit the message (magit-merge-editmsg): merge `--no-commit
    /// --no-ff`, then conclude in the commit editor seeded with git's prepared
    /// MERGE_MSG.
    MergeEditMsg,
    /// Preview what merging a picked branch would introduce (≈
    /// magit-merge-preview): the three-dot `HEAD...<branch>` diff.
    MergePreview,
    /// Cherry-pick commit(s), creating commits.
    CherryPick,
    /// Cherry-pick a typed revision/range.
    CherryPickRange,
    /// Apply commit changes without committing.
    CherryApply,
    /// Revert commit(s), creating commits.
    RevertCommit,
    /// Revert a typed revision/range.
    RevertRange,
    /// Apply the reverse of commit changes without committing.
    RevertNoCommit,
    /// Rebase the current branch onto its upstream.
    RebaseOntoUpstream,
    /// Rebase onto the push-remote's same-named branch.
    RebaseOntoPushRemote,
    /// Rebase onto a branch/ref the frontend prompts for.
    RebaseElsewhere,
    /// Interactive rebase: prompt for a base, then edit the todo
    /// (pick/edit/squash/fixup/drop/reorder).
    RebaseInteractive,
    /// Reword a commit using an interactive rebase.
    RebaseRewordCommit,
    /// Autosquash existing fixup!/squash! commits into their targets.
    RebaseAutosquash,
    /// Add a gitignore rule (the frontend prompts for it, seeded with the file
    /// at point), to one of the four ignore files.
    IgnoreToplevel,
    IgnoreSubdir,
    IgnorePrivate,
    IgnoreGlobal,
    /// Drive an in-progress sequence (rebase/merge/cherry-pick/revert): run
    /// `--continue` / `--skip`, or abort it (the frontend confirms abort).
    SequenceContinue,
    SequenceSkip,
    SequenceAbort,
    /// Edit the remaining todo of an in-progress rebase (`--edit-todo`).
    SequenceEditTodo,
    /// Bisect: start (pick a known-good commit in the log, `HEAD` is bad), mark
    /// the checked-out commit good/bad/skip, or reset the session.
    BisectStart,
    BisectGood,
    BisectBad,
    BisectSkip,
    BisectReset,
    /// Patch (magit's `W`): apply a diff to the worktree, apply a mailbox as
    /// commits (`git am`), or create patch files for a range (`format-patch`).
    PatchApply,
    PatchAm,
    PatchCreate,
}

/// A toggleable flag (e.g. `-f` → `--force-with-lease`). Owned strings so a
/// user-injected `[transient.<id>]` switch is the same type as a built-in one.
#[derive(Debug, Clone)]
pub struct Switch {
    pub key: String,
    pub arg: String,
    pub description: String,
    /// Whether the flag starts toggled on when the transient opens (the user can
    /// still turn it off), like magit's `--autostash` on the rebase popup. Most
    /// switches start off. For a switch with a [`config_key`](Self::config_key)
    /// the frontend overwrites this from the git-config value at open time.
    pub default_on: bool,
    /// The negated flag (e.g. `--no-gpg-sign`) for a switch whose default comes
    /// from git config: when the toggle differs from the configured default, the
    /// frontend emits the positive `arg` (turned on) or this `negation` (turned
    /// off), so the user can override the default either way. `None` for a plain
    /// switch, which simply emits `arg` when on and nothing when off.
    pub negation: Option<String>,
    /// The git-config key whose value seeds [`default_on`](Self::default_on)
    /// (e.g. `commit.gpgSign`), resolved by the frontend at open time. `None` for
    /// a switch with a fixed default.
    pub config_key: Option<String>,
    /// Arguments this switch cannot combine with (magit's `:incompatible`):
    /// toggling this switch on turns off any active switch whose `arg` is
    /// listed here. Checked symmetrically, so declaring one side suffices.
    pub exclusive_with: Vec<String>,
}

impl Switch {
    /// Declare `args` mutually exclusive with this switch — toggling it on
    /// turns them off.
    pub fn exclusive_with(mut self, args: &[&str]) -> Self {
        self.exclusive_with = args.iter().map(|a| a.to_string()).collect();
        self
    }

    /// A switch that starts off (the common case).
    pub fn new(
        key: impl Into<String>,
        arg: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            key: key.into(),
            arg: arg.into(),
            description: description.into(),
            default_on: false,
            negation: None,
            config_key: None,
            exclusive_with: Vec::new(),
        }
    }

    /// A switch that starts on; the user toggles it off.
    pub fn on(
        key: impl Into<String>,
        arg: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            default_on: true,
            ..Switch::new(key, arg, description)
        }
    }

    /// A switch whose default reflects a git-config value (`config_key`): it
    /// starts on when that config is enabled, and toggling it then emits the
    /// `negation` flag (e.g. `--no-gpg-sign`) rather than dropping the positive
    /// one — so the user can override the configured default in either direction.
    pub fn negatable(
        key: impl Into<String>,
        arg: impl Into<String>,
        negation: impl Into<String>,
        config_key: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            negation: Some(negation.into()),
            config_key: Some(config_key.into()),
            ..Switch::new(key, arg, description)
        }
    }
}

/// Where a value-reading [`Opt`] sources its autocomplete candidates. The
/// frontend turns these into picker choices; the user can always type a value
/// not in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// No candidates — free text only.
    None,
    /// Repository author names (`Name <email>`), for `--author=`.
    Authors,
    /// A fixed set of values; the user picks one (no free text), e.g. the
    /// commit-order flags.
    OneOf(&'static [&'static str]),
    /// Tracked file paths (loaded off the UI thread; can be large), for a
    /// pathspec limit.
    Files,
}

/// A value-reading option (magit's transient option, e.g. `-F` → `--grep=<x>`).
/// The frontend prompts for a value (with [`Completion`] candidates), stores it,
/// and passes `{arg}{value}` to git. `arg` carries any trailing `=` so both
/// long (`--grep=`) and short (`-G`) forms concatenate correctly.
#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub key: &'static str,
    pub arg: &'static str,
    pub description: &'static str,
    pub completion: Completion,
    /// A pathspec limit (`-- <value>`): emitted after the revision rather than
    /// as a `{arg}{value}` flag, so the frontend gathers it separately.
    pub pathspec: bool,
}

/// An invokable command (e.g. `p` → push). The description is dynamic so the
/// push/pull/fetch menus can name their resolved targets (`master → origin/master`).
#[derive(Debug, Clone)]
pub struct Action {
    pub key: &'static str,
    /// A second key that invokes the same action, shown as `key/also_key`. Used
    /// when push-remote and upstream collapse to one entry (they hit the same
    /// ref), so both `p` and `u` still work.
    pub also_key: Option<&'static str>,
    pub description: String,
    pub command: Command,
    /// Whether the description is a concrete remote-tracking ref/remote (so the
    /// frontend colors it like one). False for placeholders ("…, setting it")
    /// and non-ref actions ("elsewhere", "all remotes").
    pub ref_label: bool,
}

impl Action {
    /// An action row — the one-per-menu-entry shorthand the builders use.
    pub fn suffix(key: &'static str, description: impl Into<String>, command: Command) -> Suffix {
        Suffix::Action(Action {
            key,
            also_key: None,
            description: description.into(),
            command,
            ref_label: false,
        })
    }

    /// A push/pull/fetch target row. `is_ref` marks a configured remote ref
    /// (colored like one); a placeholder label passes false.
    pub fn target(
        key: &'static str,
        description: impl Into<String>,
        command: Command,
        is_ref: bool,
    ) -> Suffix {
        Suffix::Action(Action {
            key,
            also_key: None,
            description: description.into(),
            command,
            ref_label: is_ref,
        })
    }

    /// A collapsed push-remote/upstream target invokable by either key
    /// (rendered `key/also`); always a configured ref.
    pub fn suffix_dual(
        key: &'static str,
        also: &'static str,
        description: impl Into<String>,
        command: Command,
    ) -> Suffix {
        Suffix::Action(Action {
            key,
            also_key: Some(also),
            description: description.into(),
            command,
            ref_label: true,
        })
    }
}

/// A keys-and-description row with no toggle state of its own (e.g. the rows of
/// the `?` dispatch menu). The frontend decides what a row does when invoked;
/// `keys` is the *current* binding (so the menu reflects remaps) and may list
/// several keystrokes (e.g. `g g`).
#[derive(Debug, Clone)]
pub struct Info {
    pub keys: String,
    /// Owned so user `[[command]]` titles (not `'static`) can appear too.
    pub description: String,
}

/// A user-injected suffix (from the `[transient]` config): a key + label that
/// runs a registry command by id. Core stores the id opaquely; the frontend
/// resolves it against the command registry.
#[derive(Debug, Clone)]
pub struct Custom {
    pub key: String,
    pub description: String,
    pub id: String,
}

/// How a git-config [`Variable`] is set when its transient row is invoked.
#[derive(Debug, Clone)]
pub enum VariableKind {
    /// Cycle through a fixed set of values on each press, wrapping through
    /// "unset" back to the first (magit's `magit--git-variable:choices`).
    Choices {
        choices: Vec<String>,
        /// Another config key whose value stands in when this one is unset
        /// (e.g. `branch.<b>.rebase` falls back to `pull.rebase`). Shown for
        /// context; the frontend resolves it at open time.
        fallback: Option<String>,
        /// The value git assumes when this and the fallback are both unset.
        default: Option<String>,
    },
    /// Prompt for a free-text value (magit's plain `magit--git-variable`).
    Value { completion: Completion },
}

/// A git-config variable shown in a transient's "Configure" section (magit's
/// `magit-branch-configure` / `magit-remote-configure`): its current value is
/// displayed and set/cycled in place. Owned strings, since the config key has
/// the branch/remote scope substituted at build time.
#[derive(Debug, Clone)]
pub struct Variable {
    pub key: String,
    /// The fully-resolved config key, scope already substituted (e.g.
    /// `branch.main.rebase`).
    pub variable: String,
    pub description: String,
    pub kind: VariableKind,
    /// The current value read from git config when the transient opens; `None`
    /// when unset. The frontend fills and updates this in place.
    pub value: Option<String>,
    /// The resolved value of a [`VariableKind::Choices`] `fallback` key, shown
    /// when `value` is unset. Filled by the frontend at open time.
    pub fallback_value: Option<String>,
}

impl Variable {
    /// A cycling choice variable (magit's `[true|false]` rows).
    pub fn choices(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        choices: &[&str],
        fallback: Option<&str>,
        default: Option<&str>,
    ) -> Suffix {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Choices {
                choices: choices.iter().map(|c| c.to_string()).collect(),
                fallback: fallback.map(str::to_string),
                default: default.map(str::to_string),
            },
            value: None,
            fallback_value: None,
        })
    }

    /// A cycling choice variable whose choices are computed at build time (e.g.
    /// the repo's remotes for a `pushRemote`/`pushDefault` row).
    pub fn choices_of(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        choices: Vec<String>,
        fallback: Option<&str>,
    ) -> Suffix {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Choices {
                choices,
                fallback: fallback.map(str::to_string),
                default: None,
            },
            value: None,
            fallback_value: None,
        })
    }

    /// A free-text variable (prompt for the value).
    pub fn value(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        completion: Completion,
    ) -> Suffix {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Value { completion },
            value: None,
            fallback_value: None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Suffix {
    Switch(Switch),
    Action(Action),
    Option(Opt),
    Info(Info),
    Custom(Custom),
    Variable(Variable),
}

#[derive(Debug, Clone)]
pub struct Group {
    pub title: Vec<TitleSpan>,
    pub suffixes: Vec<Suffix>,
}

/// A piece of a dialog title/prompt: plain text, or a branch/ref name the
/// frontend styles distinctly so it stands out from the surrounding words
/// (e.g. the `main` in "Push main to").
#[derive(Debug, Clone)]
pub enum TitleSpan {
    Text(String),
    Branch(String),
}

impl TitleSpan {
    pub fn text(s: impl Into<String>) -> Self {
        TitleSpan::Text(s.into())
    }
    pub fn branch(s: impl Into<String>) -> Self {
        TitleSpan::Branch(s.into())
    }
}

/// A title that's a single run of plain text.
pub fn plain_title(s: impl Into<String>) -> Vec<TitleSpan> {
    vec![TitleSpan::Text(s.into())]
}

#[derive(Debug, Clone)]
pub struct Transient {
    pub title: Vec<TitleSpan>,
    pub groups: Vec<Group>,
}

impl Transient {
    /// All suffixes across all groups, flattened — the accessors below are
    /// filters over this.
    fn suffixes(&self) -> impl Iterator<Item = &Suffix> {
        self.groups.iter().flat_map(|g| g.suffixes.iter())
    }

    fn suffixes_mut(&mut self) -> impl Iterator<Item = &mut Suffix> {
        self.groups.iter_mut().flat_map(|g| g.suffixes.iter_mut())
    }

    /// All switches across all groups.
    pub fn switches(&self) -> impl Iterator<Item = &Switch> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Switch(sw) => Some(sw),
            _ => None,
        })
    }

    /// All value-reading options across all groups.
    pub fn options(&self) -> impl Iterator<Item = &Opt> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Option(o) => Some(o),
            _ => None,
        })
    }

    /// The option bound to `key`, if any.
    pub fn option_for(&self, key: &str) -> Option<&Opt> {
        self.options().find(|o| o.key == key)
    }

    /// The action bound to `key` (its primary or secondary key), if any.
    pub fn action_for(&self, key: &str) -> Option<&Action> {
        self.suffixes().find_map(|s| match s {
            Suffix::Action(a) if a.key == key || a.also_key == Some(key) => Some(a),
            _ => None,
        })
    }

    /// The user-injected custom suffix bound to `key`, if any.
    pub fn custom_for(&self, key: &str) -> Option<&Custom> {
        self.suffixes().find_map(|s| match s {
            Suffix::Custom(c) if c.key == key => Some(c),
            _ => None,
        })
    }

    /// All git-config variables across all groups.
    pub fn variables_ref(&self) -> impl Iterator<Item = &Variable> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Variable(v) => Some(v),
            _ => None,
        })
    }

    /// All git-config variables across all groups (mutable — the frontend fills
    /// their current values at open time and updates them on set).
    pub fn variables_mut(&mut self) -> impl Iterator<Item = &mut Variable> {
        self.suffixes_mut().filter_map(|s| match s {
            Suffix::Variable(v) => Some(v),
            _ => None,
        })
    }

    /// The config variable bound to `key`, if any.
    pub fn variable_for(&self, key: &str) -> Option<&Variable> {
        self.variables_ref().find(|v| v.key == key)
    }

    /// The config variable bound to `key`, mutably (to update its value in place
    /// after a set).
    pub fn variable_for_mut(&mut self, key: &str) -> Option<&mut Variable> {
        self.variables_mut().find(|v| v.key == key)
    }

    /// Whether some action/custom suffix key strictly extends `prefix` — i.e.
    /// the keystrokes typed so far could still resolve to a multi-key suffix
    /// (magit's `fu`/`pu` jump keys).
    pub fn has_key_prefix(&self, prefix: &str) -> bool {
        self.suffixes()
            .flat_map(|s| match s {
                Suffix::Action(a) => vec![Some(a.key), a.also_key],
                Suffix::Custom(c) => vec![Some(c.key.as_str())],
                _ => vec![],
            })
            .flatten()
            .any(|key| key.len() > prefix.len() && key.starts_with(prefix))
    }
}

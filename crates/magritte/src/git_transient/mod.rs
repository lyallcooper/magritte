//! The app's git transient vocabulary: the [`Command`] enum the generic
//! transient model is instantiated with, the built-in menu builders
//! (magit's popups), and the concrete type aliases the rest of the crate
//! uses. The model itself (groups, suffixes, switches, accessors) lives in
//! `magritte_ui::transient` and is re-exported here.

pub(crate) use magritte_ui::transient::*;

mod menus;
pub(crate) use menus::*;

/// The transient model instantiated with the git [`Command`] vocabulary — the
/// concrete types every screen works with.
pub(crate) type Transient = magritte_ui::transient::Transient<Command>;
pub(crate) type Group = magritte_ui::transient::Group<Command>;
pub(crate) type Suffix = magritte_ui::transient::Suffix<Command>;

/// [`Completion::Source`] tags: repository author names (`Name <email>`, for
/// `--author=`) and tracked file paths (for pathspec limits). The frontend
/// loads the candidates off the UI thread.
pub(crate) const AUTHORS: &str = "authors";
pub(crate) const FILES: &str = "files";

/// Which built-in key style to use for transient suffixes that differ between
/// vanilla Magit and evil-collection-magit. The commands are the same; only the
/// default keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeymapStyle {
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
pub(crate) enum Command {
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
    /// Stash only the staged changes (`--staged`).
    StashPushStaged,
    /// Stash worktree and index but leave the index applied (`--keep-index`).
    StashPushKeepIndex,
    /// Snapshot the working tree and index onto `refs/stash` without
    /// resetting anything.
    StashSnapshotBoth,
    /// Snapshot only the index.
    StashSnapshotIndex,
    /// Snapshot only the working tree's unstaged changes.
    StashSnapshotWorktree,
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

//! The transient model — magit's popup command menus (`P` push, `F` pull,
//! `f` fetch, `c` commit, …).
//!
//! A [`Transient`] is a declarative tree of groups and suffixes (switches and
//! actions). The model is UI-agnostic: it carries keys and descriptions as
//! data, but knows nothing about rendering. The frontend renders the popup,
//! tracks which switches are toggled on, and dispatches keys; when an action
//! fires it calls [`Repo::execute`] with the active switch arguments.

use crate::error::{Error, Result};
use crate::remote::{RemoteTargets, Upstream};
use crate::repo::Repo;
use crate::sequence::SequenceKind;

/// The git operation an [`Action`] runs. Push/pull/fetch come in magit's three
/// flavors — to the push-remote, to the upstream, or elsewhere (the frontend
/// resolves the actual remote, prompting when unconfigured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    PushPushRemote,
    PushUpstream,
    PushElsewhere,
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
    /// Reword an older commit using an interactive rebase.
    CommitRewordPast,
    /// Amend HEAD with staged changes, keeping its message.
    CommitExtend,
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
    /// Stash the working tree and index.
    StashPush,
    /// Stash including untracked files.
    StashPushAll,
    /// Apply a stash, keeping it (prompts for which).
    StashApply,
    /// Pop a stash (prompts for which).
    StashPop,
    /// Drop a stash (prompts for which).
    StashDrop,
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
    /// Merge a branch/ref into HEAD (the frontend prompts for it).
    MergePlain,
    /// Merge but don't commit (`--no-commit`).
    MergeNoCommit,
    /// Squash-merge (`--squash`): stage the result without a merge commit.
    MergeSquash,
    /// Cherry-pick commit(s), creating commits.
    CherryPick,
    /// Apply commit changes without committing.
    CherryApply,
    /// Revert commit(s), creating commits.
    RevertCommit,
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
}

impl Switch {
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
    pub description: String,
    pub command: Command,
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

#[derive(Debug, Clone)]
pub enum Suffix {
    Switch(Switch),
    Action(Action),
    Option(Opt),
    Info(Info),
    Custom(Custom),
}

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

pub struct Transient {
    pub title: Vec<TitleSpan>,
    pub groups: Vec<Group>,
}

impl Transient {
    /// All switches across all groups.
    pub fn switches(&self) -> impl Iterator<Item = &Switch> {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .filter_map(|s| match s {
                Suffix::Switch(sw) => Some(sw),
                _ => None,
            })
    }

    /// All value-reading options across all groups.
    pub fn options(&self) -> impl Iterator<Item = &Opt> {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .filter_map(|s| match s {
                Suffix::Option(o) => Some(o),
                _ => None,
            })
    }

    /// The option bound to `key`, if any.
    pub fn option_for(&self, key: &str) -> Option<&Opt> {
        self.options().find(|o| o.key == key)
    }

    /// The action bound to `key`, if any.
    pub fn action_for(&self, key: &str) -> Option<&Action> {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .find_map(|s| match s {
                Suffix::Action(a) if a.key == key => Some(a),
                _ => None,
            })
    }

    /// The user-injected custom suffix bound to `key`, if any.
    pub fn custom_for(&self, key: &str) -> Option<&Custom> {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .find_map(|s| match s {
                Suffix::Custom(c) if c.key == key => Some(c),
                _ => None,
            })
    }
}

/// `branch → remote/branch`, the push-remote target label.
fn push_remote_label(t: &RemoteTargets) -> String {
    match (&t.branch, &t.push_remote) {
        (Some(b), Some(r)) => format!("{b} \u{2192} {r}/{b}"),
        _ => "push-remote".to_string(),
    }
}

/// The upstream label (`origin/master`).
fn upstream_label(t: &RemoteTargets) -> String {
    t.upstream
        .as_ref()
        .map(Upstream::display)
        .unwrap_or_else(|| "upstream".to_string())
}

pub fn push_transient(t: &RemoteTargets) -> Transient {
    // The target group reads "Push <branch> to" with the branch styled
    // distinctly (magit's framing); falls back to "Push to" when HEAD is
    // detached.
    let push_to = match &t.branch {
        Some(b) => vec![
            TitleSpan::text("Push "),
            TitleSpan::branch(b.clone()),
            TitleSpan::text(" to"),
        ],
        None => plain_title("Push to"),
    };
    Transient {
        title: plain_title("Push"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new("-f", "--force-with-lease", "Force with lease")),
                    Suffix::Switch(Switch::new("-F", "--force", "Force")),
                    Suffix::Switch(Switch::new("-h", "--no-verify", "Disable hooks")),
                    Suffix::Switch(Switch::new("-n", "--dry-run", "Dry run")),
                    Suffix::Switch(Switch::new("-u", "--set-upstream", "Set upstream")),
                    Suffix::Switch(Switch::new("-T", "--tags", "Include all tags")),
                    Suffix::Switch(Switch::new("-t", "--follow-tags", "Include related annotated tags")),
                ],
            },
            Group {
                title: push_to,
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "p",
                        description: push_remote_label(t),
                        command: Command::PushPushRemote,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: upstream_label(t),
                        command: Command::PushUpstream,
                    }),
                    Suffix::Action(Action {
                        key: "e",
                        description: "elsewhere".to_string(),
                        command: Command::PushElsewhere,
                    }),
                ],
            },
        ],
    }
}

pub fn branch_transient() -> Transient {
    Transient {
        title: plain_title("Branch"),
        groups: vec![
            Group {
                title: plain_title("Checkout"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "b",
                        description: "branch/revision".to_string(),
                        command: Command::BranchCheckout,
                    }),
                    Suffix::Action(Action {
                        key: "c",
                        description: "new branch".to_string(),
                        command: Command::BranchCreateCheckout,
                    }),
                ],
            },
            Group {
                title: plain_title("Create"),
                suffixes: vec![Suffix::Action(Action {
                    key: "n",
                    description: "new branch".to_string(),
                    command: Command::BranchCreate,
                })],
            },
            Group {
                title: plain_title("Do"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "m",
                        description: "rename".to_string(),
                        command: Command::BranchRename,
                    }),
                    Suffix::Action(Action {
                        key: "k",
                        description: "delete".to_string(),
                        command: Command::BranchDelete,
                    }),
                ],
            },
        ],
    }
}

pub fn stash_transient() -> Transient {
    Transient {
        title: plain_title("Stash"),
        groups: vec![
            Group {
                title: plain_title("Stash"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "z",
                        description: "both".to_string(),
                        command: Command::StashPush,
                    }),
                    Suffix::Action(Action {
                        key: "Z",
                        description: "both, incl. untracked".to_string(),
                        command: Command::StashPushAll,
                    }),
                ],
            },
            Group {
                title: plain_title("Use"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "a",
                        description: "apply".to_string(),
                        command: Command::StashApply,
                    }),
                    Suffix::Action(Action {
                        key: "p",
                        description: "pop".to_string(),
                        command: Command::StashPop,
                    }),
                    Suffix::Action(Action {
                        key: "k",
                        description: "drop".to_string(),
                        command: Command::StashDrop,
                    }),
                ],
            },
        ],
    }
}

pub fn log_transient() -> Transient {
    Transient {
        title: plain_title("Log"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new("-r", "--reverse", "Reverse order")),
                    Suffix::Option(Opt {
                        key: "-o",
                        // The value is the full `--…-order` flag, so no prefix.
                        arg: "",
                        description: "Order commits by",
                        completion: Completion::OneOf(&[
                            "--topo-order",
                            "--author-date-order",
                            "--date-order",
                        ]),
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-n",
                        arg: "-n",
                        description: "Limit number of commits",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-A",
                        arg: "--author=",
                        description: "Limit to author",
                        completion: Completion::Authors,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-F",
                        arg: "--grep=",
                        description: "Search messages",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-G",
                        arg: "-G",
                        description: "Search changes",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-S",
                        arg: "-S",
                        description: "Search occurrences",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        // Keyed `--` (entered by pressing `-` twice), matching
                        // magit's file-limit infix.
                        key: "--",
                        arg: "",
                        description: "Limit to files",
                        completion: Completion::Files,
                        pathspec: true,
                    }),
                ],
            },
            Group {
                title: plain_title("Log"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "l",
                        description: "current".to_string(),
                        command: Command::LogCurrent,
                    }),
                    Suffix::Action(Action {
                        key: "a",
                        description: "all branches".to_string(),
                        command: Command::LogAll,
                    }),
                    Suffix::Action(Action {
                        key: "o",
                        description: "other".to_string(),
                        command: Command::LogOther,
                    }),
                    Suffix::Action(Action {
                        key: "r",
                        description: "reflog".to_string(),
                        command: Command::LogReflog,
                    }),
                ],
            },
        ],
    }
}

pub fn diff_transient() -> Transient {
    Transient {
        title: plain_title("Diff"),
        groups: vec![
            Group {
                title: plain_title("Limit arguments"),
                suffixes: vec![
                    Suffix::Option(Opt {
                        key: "--",
                        arg: "",
                        description: "Limit to files",
                        completion: Completion::Files,
                        pathspec: true,
                    }),
                    Suffix::Option(Opt {
                        key: "-i",
                        arg: "--ignore-submodules=",
                        description: "Ignore submodules",
                        completion: Completion::OneOf(&["untracked", "dirty", "all"]),
                        pathspec: false,
                    }),
                    Suffix::Switch(Switch::new("-b", "--ignore-space-change", "Ignore whitespace changes")),
                    Suffix::Switch(Switch::new("-w", "--ignore-all-space", "Ignore all whitespace")),
                    Suffix::Switch(Switch::new("-D", "--irreversible-delete", "Omit preimage for deletes")),
                ],
            },
            Group {
                title: plain_title("Context arguments"),
                suffixes: vec![
                    Suffix::Option(Opt {
                        key: "-U",
                        arg: "-U",
                        description: "Context lines",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Switch(Switch::new("-W", "--function-context", "Show surrounding functions")),
                ],
            },
            Group {
                title: plain_title("Tune arguments"),
                suffixes: vec![
                    Suffix::Option(Opt {
                        key: "-A",
                        arg: "--diff-algorithm=",
                        description: "Diff algorithm",
                        completion: Completion::OneOf(&[
                            "default",
                            "minimal",
                            "patience",
                            "histogram",
                        ]),
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-X",
                        arg: "--diff-merges=",
                        description: "Diff merges",
                        completion: Completion::OneOf(&[
                            "off",
                            "first-parent",
                            "combined",
                            "dense-combined",
                        ]),
                        pathspec: false,
                    }),
                    Suffix::Switch(Switch::new("-M", "-M", "Detect renames")),
                    Suffix::Switch(Switch::new("-C", "-C", "Detect copies")),
                    Suffix::Switch(Switch::new("-R", "-R", "Reverse sides")),
                    Suffix::Switch(Switch::new("-x", "--no-ext-diff", "Disallow external diff drivers")),
                ],
            },
            Group {
                title: plain_title("Actions"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "d",
                        description: "smart".to_string(),
                        command: Command::DiffDwim,
                    }),
                    Suffix::Action(Action {
                        key: "r",
                        description: "range".to_string(),
                        command: Command::DiffRange,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: "unstaged".to_string(),
                        command: Command::DiffUnstaged,
                    }),
                    Suffix::Action(Action {
                        key: "s",
                        description: "staged".to_string(),
                        command: Command::DiffStaged,
                    }),
                    Suffix::Action(Action {
                        key: "w",
                        description: "worktree".to_string(),
                        command: Command::DiffWorktree,
                    }),
                    Suffix::Action(Action {
                        key: "c",
                        description: "show commit".to_string(),
                        command: Command::DiffCommit,
                    }),
                ],
            },
        ],
    }
}

pub fn commit_transient() -> Transient {
    Transient {
        title: plain_title("Commit"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new("-a", "--all", "Stage all modified and deleted files")),
                    Suffix::Switch(Switch::new("-e", "--allow-empty", "Allow empty commit")),
                    Suffix::Switch(Switch::new("-n", "--no-verify", "Disable hooks")),
                    Suffix::Switch(Switch::new(
                        "-R",
                        "--reset-author",
                        "Claim authorship and reset author date",
                    )),
                    Suffix::Option(Opt {
                        key: "-A",
                        arg: "--author=",
                        description: "Override the author",
                        completion: Completion::Authors,
                        pathspec: false,
                    }),
                    Suffix::Switch(Switch::new("-s", "--signoff", "Add Signed-off-by line")),
                    Suffix::Switch(Switch::negatable(
                        "-S",
                        "--gpg-sign",
                        "--no-gpg-sign",
                        "commit.gpgSign",
                        "Sign using gpg",
                    )),
                    Suffix::Switch(Switch::new("-D", "--date=now", "Use current time as author date")),
                ],
            },
            Group {
                title: plain_title("Create"),
                suffixes: vec![Suffix::Action(Action {
                    key: "c",
                    description: "Commit".to_string(),
                    command: Command::CommitCreate,
                })],
            },
            Group {
                title: plain_title("Edit HEAD"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "e",
                        description: "Extend (keep message)".to_string(),
                        command: Command::CommitExtend,
                    }),
                    Suffix::Action(Action {
                        key: "a",
                        description: "Amend".to_string(),
                        command: Command::CommitAmend,
                    }),
                    Suffix::Action(Action {
                        key: "w",
                        description: "Reword (message only)".to_string(),
                        command: Command::CommitReword,
                    }),
                ],
            },
            Group {
                title: plain_title("Edit and rebase"),
                suffixes: vec![Suffix::Action(Action {
                    key: "R",
                    description: "Reword past".to_string(),
                    command: Command::CommitRewordPast,
                })],
            },
        ],
    }
}

pub fn pull_transient(t: &RemoteTargets) -> Transient {
    // Pulling from the push-remote merges its same-named branch.
    let push_remote = match (&t.branch, &t.push_remote) {
        (Some(b), Some(r)) => format!("{r}/{b}"),
        _ => "push-remote".to_string(),
    };
    Transient {
        title: plain_title("Pull"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![Suffix::Switch(Switch::negatable(
                    "-r",
                    "--rebase",
                    "--no-rebase",
                    "pull.rebase",
                    "Rebase local commits",
                ))],
            },
            Group {
                title: plain_title("Pull from"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "p",
                        description: push_remote,
                        command: Command::PullPushRemote,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: upstream_label(t),
                        command: Command::PullUpstream,
                    }),
                    Suffix::Action(Action {
                        key: "e",
                        description: "elsewhere".to_string(),
                        command: Command::PullElsewhere,
                    }),
                ],
            },
        ],
    }
}

pub fn fetch_transient(t: &RemoteTargets) -> Transient {
    // Fetch acts on a whole remote, so label with the remote name.
    let push_remote = t
        .push_remote
        .clone()
        .unwrap_or_else(|| "push-remote".to_string());
    let upstream = t
        .upstream
        .as_ref()
        .map(|u| u.remote.clone())
        .unwrap_or_else(|| "upstream".to_string());
    Transient {
        title: plain_title("Fetch"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![Suffix::Switch(Switch::negatable(
                    "-p",
                    "--prune",
                    "--no-prune",
                    "fetch.prune",
                    "Prune deleted branches",
                ))],
            },
            Group {
                title: plain_title("Fetch from"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "p",
                        description: push_remote,
                        command: Command::FetchPushRemote,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: upstream,
                        command: Command::FetchUpstream,
                    }),
                    Suffix::Action(Action {
                        key: "a",
                        description: "all remotes".to_string(),
                        command: Command::FetchAll,
                    }),
                    Suffix::Action(Action {
                        key: "e",
                        description: "elsewhere".to_string(),
                        command: Command::FetchElsewhere,
                    }),
                ],
            },
        ],
    }
}

pub fn rebase_transient(t: &RemoteTargets) -> Transient {
    let push_remote = match (&t.branch, &t.push_remote) {
        (Some(b), Some(r)) => format!("{r}/{b}"),
        _ => "push-remote".to_string(),
    };
    Transient {
        title: plain_title("Rebase"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::on("-a", "--autostash", "Stash uncommitted changes around the rebase")),
                    Suffix::Switch(Switch::negatable(
                        "-s",
                        "--autosquash",
                        "--no-autosquash",
                        "rebase.autoSquash",
                        "Honor fixup!/squash! commits",
                    )),
                ],
            },
            Group {
                title: plain_title("Rebase onto"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "p",
                        description: push_remote,
                        command: Command::RebaseOntoPushRemote,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: upstream_label(t),
                        command: Command::RebaseOntoUpstream,
                    }),
                    Suffix::Action(Action {
                        key: "e",
                        description: "elsewhere".to_string(),
                        command: Command::RebaseElsewhere,
                    }),
                ],
            },
            Group {
                title: plain_title("Rebase"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "i",
                        description: "interactively".to_string(),
                        command: Command::RebaseInteractive,
                    }),
                    Suffix::Action(Action {
                        key: "w",
                        description: "to reword a commit".to_string(),
                        command: Command::RebaseRewordCommit,
                    }),
                ],
            },
        ],
    }
}

pub fn merge_transient() -> Transient {
    Transient {
        title: plain_title("Merge"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new("-n", "--no-ff", "Always create a merge commit")),
                    Suffix::Switch(Switch::new("-f", "--ff-only", "Fast-forward only")),
                ],
            },
            Group {
                title: plain_title("Merge"),
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "m",
                        description: "merge".to_string(),
                        command: Command::MergePlain,
                    }),
                    Suffix::Action(Action {
                        key: "n",
                        description: "merge, don't commit".to_string(),
                        command: Command::MergeNoCommit,
                    }),
                    Suffix::Action(Action {
                        key: "s",
                        description: "squash merge".to_string(),
                        command: Command::MergeSquash,
                    }),
                ],
            },
        ],
    }
}

pub fn cherry_pick_transient() -> Transient {
    Transient {
        title: plain_title("Cherry-pick"),
        groups: vec![Group {
            title: plain_title("Apply here"),
            suffixes: vec![
                Suffix::Action(Action {
                    key: "A",
                    description: "pick".to_string(),
                    command: Command::CherryPick,
                }),
                Suffix::Action(Action {
                    key: "a",
                    description: "apply".to_string(),
                    command: Command::CherryApply,
                }),
            ],
        }],
    }
}

pub fn revert_transient() -> Transient {
    Transient {
        title: plain_title("Revert"),
        groups: vec![Group {
            title: plain_title("Actions"),
            suffixes: vec![
                Suffix::Action(Action {
                    key: "V",
                    description: "revert commit".to_string(),
                    command: Command::RevertCommit,
                }),
                Suffix::Action(Action {
                    key: "v",
                    description: "revert changes".to_string(),
                    command: Command::RevertNoCommit,
                }),
            ],
        }],
    }
}

/// The transient shown when a sequence's prefix is pressed while that sequence
/// is already in progress: magit's continue / skip / abort, scoped to what the
/// operation supports. A merge has no continue/skip (you finish it by committing
/// the resolved index, or abort), so it shows only abort.
pub fn sequence_transient(kind: SequenceKind) -> Transient {
    let mut suffixes = Vec::new();
    let continue_key = match kind {
        SequenceKind::CherryPick => "A",
        SequenceKind::Revert => "V",
        SequenceKind::Am => "w",
        SequenceKind::Merge | SequenceKind::Rebase => "r",
    };
    if kind.can_continue() {
        suffixes.push(Suffix::Action(Action {
            key: continue_key,
            description: "continue".to_string(),
            command: Command::SequenceContinue,
        }));
    }
    if kind.can_skip() {
        suffixes.push(Suffix::Action(Action {
            key: "s",
            description: "skip".to_string(),
            command: Command::SequenceSkip,
        }));
    }
    if kind.can_edit_todo() {
        suffixes.push(Suffix::Action(Action {
            key: "e",
            description: "edit".to_string(),
            command: Command::SequenceEditTodo,
        }));
    }
    suffixes.push(Suffix::Action(Action {
        key: "a",
        description: "abort".to_string(),
        command: Command::SequenceAbort,
    }));
    let label = kind.label();
    let mut title = label.to_string();
    title[..1].make_ascii_uppercase();
    Transient {
        title: plain_title(format!("{title} in progress")),
        groups: vec![Group {
            title: plain_title("Actions"),
            suffixes,
        }],
    }
}

pub fn ignore_transient() -> Transient {
    Transient {
        title: plain_title("Gitignore"),
        groups: vec![Group {
            title: plain_title("Gitignore"),
            suffixes: vec![
                Suffix::Action(Action {
                    key: "t",
                    description: "shared at toplevel (.gitignore)".to_string(),
                    command: Command::IgnoreToplevel,
                }),
                Suffix::Action(Action {
                    key: "s",
                    description: "shared in subdirectory (.gitignore)".to_string(),
                    command: Command::IgnoreSubdir,
                }),
                Suffix::Action(Action {
                    key: "p",
                    description: "privately (.git/info/exclude)".to_string(),
                    command: Command::IgnorePrivate,
                }),
                Suffix::Action(Action {
                    key: "g",
                    description: "privately for all repositories".to_string(),
                    command: Command::IgnoreGlobal,
                }),
            ],
        }],
    }
}

pub fn reset_transient() -> Transient {
    Transient {
        title: plain_title("Reset"),
        groups: vec![Group {
            title: plain_title("Reset"),
            suffixes: vec![
                Suffix::Action(Action {
                    key: "m",
                    description: "mixed (HEAD and index)".to_string(),
                    command: Command::ResetMixed,
                }),
                Suffix::Action(Action {
                    key: "s",
                    description: "soft (HEAD only)".to_string(),
                    command: Command::ResetSoft,
                }),
                Suffix::Action(Action {
                    key: "h",
                    description: "hard (HEAD, index, working tree)".to_string(),
                    command: Command::ResetHard,
                }),
                Suffix::Action(Action {
                    key: "k",
                    description: "keep (HEAD and index, keep uncommitted)".to_string(),
                    command: Command::ResetKeep,
                }),
                Suffix::Action(Action {
                    key: "i",
                    description: "index (only)".to_string(),
                    command: Command::ResetIndex,
                }),
                Suffix::Action(Action {
                    key: "w",
                    description: "worktree (only)".to_string(),
                    command: Command::ResetWorktree,
                }),
            ],
        }],
    }
}

impl Repo {
    /// The current branch name, or `None` when HEAD is detached.
    pub fn current_branch(&self) -> Result<Option<String>> {
        let out = self.run(["rev-parse", "--abbrev-ref", "HEAD"])?;
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if name == "HEAD" { None } else { Some(name) })
    }

    /// Run a transient command that doesn't need a resolved remote or a message:
    /// currently only commit-extend. Push/pull/fetch are run via the dedicated
    /// [`Repo::push_to`]/[`pull_from`](Repo::pull_from)/etc. methods (the frontend
    /// resolves the remote first), and the message commits go through the editor.
    pub fn execute(&self, command: Command, switches: &[String]) -> Result<String> {
        match command {
            Command::CommitExtend => self.commit_extend(switches),
            _ => Err(Error::Message(
                "command is not run via execute()".to_string(),
            )),
        }
    }
}

//! The transient model — magit's popup command menus (`P` push, `F` pull,
//! `f` fetch, `c` commit, …).
//!
//! A [`Transient`] is a declarative tree of groups and suffixes (switches and
//! actions). The model is UI-agnostic: it carries keys and descriptions as
//! data, but knows nothing about rendering. The frontend renders the popup,
//! tracks which switches are toggled on, and dispatches keys; when an action
//! fires it runs the [`Command`]'s operation with the active switch arguments.

use crate::remote::{RemoteTargets, Upstream};
use crate::sequence::SequenceKind;

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
    /// Create a lightweight tag at point/HEAD (prompts for name).
    TagCreate,
    /// Delete a local tag (prompts for the tag).
    TagDelete,
    /// Add a remote (prompts for name then URL).
    RemoteAdd,
    /// Rename a remote (prompts for old then new name).
    RemoteRename,
    /// Remove a remote (prompts for name).
    RemoteRemove,
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

#[derive(Debug, Clone)]
pub enum Suffix {
    Switch(Switch),
    Action(Action),
    Option(Opt),
    Info(Info),
    Custom(Custom),
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

    /// The action bound to `key` (its primary or secondary key), if any.
    pub fn action_for(&self, key: &str) -> Option<&Action> {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .find_map(|s| match s {
                Suffix::Action(a) if a.key == key || a.also_key == Some(key) => Some(a),
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

    /// Whether some action/custom suffix key strictly extends `prefix` — i.e.
    /// the keystrokes typed so far could still resolve to a multi-key suffix
    /// (magit's `fu`/`pu` jump keys).
    pub fn has_key_prefix(&self, prefix: &str) -> bool {
        self.groups
            .iter()
            .flat_map(|g| g.suffixes.iter())
            .flat_map(|s| match s {
                Suffix::Action(a) => vec![Some(a.key), a.also_key],
                Suffix::Custom(c) => vec![Some(c.key.as_str())],
                _ => vec![],
            })
            .flatten()
            .any(|key| key.len() > prefix.len() && key.starts_with(prefix))
    }
}

/// The push-remote target label: `branch → remote/branch` when configured, else
/// magit's descriptive hint that invoking it configures the push-remote (push
/// saves it — "…, setting it" reads as an action, unlike a bare "push remote").
fn push_remote_label(t: &RemoteTargets) -> String {
    match (&t.branch, &t.push_remote) {
        (Some(b), Some(r)) => format!("{b} \u{2192} {r}/{b}"),
        // Unconfigured: name the sole remote it would push to (and save) when
        // there's just one, else the abstract hint.
        _ => match t.predicted_ref() {
            Some(r) => format!("{r}, setting it"),
            None => "push remote, setting it".to_string(),
        },
    }
}

/// The push-upstream target label: `remote/branch` when configured, else the
/// predicted sole-remote target (or the abstract hint) — invoking it configures
/// the upstream (push saves it).
fn push_upstream_label(t: &RemoteTargets) -> String {
    match t.upstream.as_ref() {
        Some(u) => u.display(),
        None => match t.predicted_ref() {
            Some(r) => format!("{r}, setting it"),
            None => "upstream, setting it".to_string(),
        },
    }
}

/// The upstream label (`origin/master`); the bare target for pull/fetch/rebase,
/// which act on it without configuring it.
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
                    Suffix::Switch(Switch::new(
                        "-t",
                        "--follow-tags",
                        "Include related annotated tags",
                    )),
                ],
            },
            Group {
                title: push_to,
                // When the push-remote and upstream are the same ref, one entry
                // (`p/u`) covers both; otherwise show them separately.
                suffixes: if t.push_matches_upstream() {
                    vec![
                        Action::suffix_dual(
                            "p",
                            "u",
                            push_upstream_label(t),
                            Command::PushUpstream,
                        ),
                        Action::suffix("e", "elsewhere", Command::PushElsewhere),
                    ]
                } else {
                    vec![
                        Action::target(
                            "p",
                            push_remote_label(t),
                            Command::PushPushRemote,
                            t.push_remote.is_some(),
                        ),
                        Action::target(
                            "u",
                            push_upstream_label(t),
                            Command::PushUpstream,
                            t.upstream.is_some(),
                        ),
                        Action::suffix("e", "elsewhere", Command::PushElsewhere),
                    ]
                },
            },
        ],
    }
}

pub fn branch_transient(style: KeymapStyle) -> Transient {
    Transient {
        title: plain_title("Branch"),
        groups: vec![
            Group {
                title: plain_title("Checkout"),
                suffixes: vec![
                    Action::suffix("b", "branch/revision", Command::BranchCheckout),
                    Action::suffix("c", "new branch", Command::BranchCreateCheckout),
                ],
            },
            Group {
                title: plain_title("Create"),
                suffixes: vec![Action::suffix("n", "new branch", Command::BranchCreate)],
            },
            Group {
                title: plain_title("Do"),
                suffixes: vec![
                    Action::suffix("m", "rename", Command::BranchRename),
                    Action::suffix(style.delete_key(), "delete", Command::BranchDelete),
                ],
            },
        ],
    }
}

pub fn tag_transient(style: KeymapStyle) -> Transient {
    Transient {
        title: plain_title("Tag"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new("-f", "--force", "Force")),
                    Suffix::Switch(Switch::new("-a", "--annotate", "Annotate")),
                ],
            },
            Group {
                title: plain_title("Create"),
                suffixes: vec![Action::suffix("t", "tag", Command::TagCreate)],
            },
            Group {
                title: plain_title("Do"),
                suffixes: vec![Action::suffix(
                    style.delete_key(),
                    "delete",
                    Command::TagDelete,
                )],
            },
        ],
    }
}

pub fn remote_transient(style: KeymapStyle) -> Transient {
    Transient {
        title: plain_title("Remote"),
        groups: vec![
            Group {
                title: plain_title("Arguments for add"),
                suffixes: vec![Suffix::Switch(Switch::on("-f", "-f", "Fetch after add"))],
            },
            Group {
                title: plain_title("Actions"),
                suffixes: vec![
                    Action::suffix("a", "add", Command::RemoteAdd),
                    Action::suffix("r", "rename", Command::RemoteRename),
                    Action::suffix(style.delete_key(), "remove", Command::RemoteRemove),
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
                    Action::suffix("z", "both", Command::StashPush),
                    Action::suffix("Z", "both, incl. untracked", Command::StashPushAll),
                ],
            },
            Group {
                title: plain_title("Use"),
                suffixes: vec![
                    Action::suffix("a", "apply", Command::StashApply),
                    Action::suffix("p", "pop", Command::StashPop),
                    Action::suffix("k", "drop", Command::StashDrop),
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
                    Suffix::Switch(Switch::new("-m", "--no-merges", "Omit merge commits")),
                    Suffix::Switch(Switch::new(
                        "-p",
                        "--first-parent",
                        "Follow only the first parent",
                    )),
                    Suffix::Option(Opt {
                        key: "-s",
                        arg: "--since=",
                        description: "Since date",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                    Suffix::Option(Opt {
                        key: "-u",
                        arg: "--until=",
                        description: "Until date",
                        completion: Completion::None,
                        pathspec: false,
                    }),
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
                    Action::suffix("l", "current", Command::LogCurrent),
                    Action::suffix("a", "all branches", Command::LogAll),
                    Action::suffix("o", "other", Command::LogOther),
                    Action::suffix("r", "reflog", Command::LogReflog),
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
                    Suffix::Switch(Switch::new(
                        "-b",
                        "--ignore-space-change",
                        "Ignore whitespace changes",
                    )),
                    Suffix::Switch(Switch::new(
                        "-w",
                        "--ignore-all-space",
                        "Ignore all whitespace",
                    )),
                    Suffix::Switch(Switch::new(
                        "-D",
                        "--irreversible-delete",
                        "Omit preimage for deletes",
                    )),
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
                    Suffix::Switch(Switch::new(
                        "-W",
                        "--function-context",
                        "Show surrounding functions",
                    )),
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
                    Suffix::Switch(Switch::new(
                        "-x",
                        "--no-ext-diff",
                        "Disallow external diff drivers",
                    )),
                ],
            },
            Group {
                title: plain_title("Actions"),
                suffixes: vec![
                    Action::suffix("d", "smart", Command::DiffDwim),
                    Action::suffix("r", "range", Command::DiffRange),
                    Action::suffix("u", "unstaged", Command::DiffUnstaged),
                    Action::suffix("s", "staged", Command::DiffStaged),
                    Action::suffix("w", "worktree", Command::DiffWorktree),
                    Action::suffix("c", "show commit", Command::DiffCommit),
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
                    Suffix::Switch(Switch::new(
                        "-a",
                        "--all",
                        "Stage all modified and deleted files",
                    )),
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
                    Suffix::Switch(Switch::new(
                        "-D",
                        "--date=now",
                        "Use current time as author date",
                    )),
                ],
            },
            Group {
                title: plain_title("Create"),
                suffixes: vec![Action::suffix("c", "Commit", Command::CommitCreate)],
            },
            Group {
                title: plain_title("Edit HEAD"),
                suffixes: vec![
                    Action::suffix("e", "Extend (keep message)", Command::CommitExtend),
                    Action::suffix("a", "Amend", Command::CommitAmend),
                    Action::suffix("w", "Reword (message only)", Command::CommitReword),
                ],
            },
            Group {
                title: plain_title("Edit"),
                suffixes: vec![
                    Action::suffix("f", "Fixup", Command::CommitFixup),
                    Action::suffix("s", "Squash", Command::CommitSquash),
                ],
            },
            Group {
                title: plain_title("Edit and rebase"),
                suffixes: vec![
                    Action::suffix("F", "Instant fixup", Command::CommitInstantFixup),
                    Action::suffix("S", "Instant squash", Command::CommitInstantSquash),
                    Action::suffix("R", "Reword past", Command::CommitRewordPast),
                ],
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
                suffixes: if t.push_matches_upstream() {
                    vec![
                        Action::suffix_dual("p", "u", upstream_label(t), Command::PullUpstream),
                        Action::suffix("e", "elsewhere", Command::PullElsewhere),
                    ]
                } else {
                    vec![
                        Action::target(
                            "p",
                            push_remote,
                            Command::PullPushRemote,
                            t.push_remote.is_some(),
                        ),
                        Action::target(
                            "u",
                            upstream_label(t),
                            Command::PullUpstream,
                            t.upstream.is_some(),
                        ),
                        Action::suffix("e", "elsewhere", Command::PullElsewhere),
                    ]
                },
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
                suffixes: if t.push_remote_is_upstream_remote() {
                    vec![
                        Action::suffix_dual("p", "u", upstream, Command::FetchUpstream),
                        Action::suffix("a", "all remotes", Command::FetchAll),
                        Action::suffix("e", "elsewhere", Command::FetchElsewhere),
                    ]
                } else {
                    vec![
                        Action::target(
                            "p",
                            push_remote,
                            Command::FetchPushRemote,
                            t.push_remote.is_some(),
                        ),
                        Action::target("u", upstream, Command::FetchUpstream, t.upstream.is_some()),
                        Action::suffix("a", "all remotes", Command::FetchAll),
                        Action::suffix("e", "elsewhere", Command::FetchElsewhere),
                    ]
                },
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
                    // Magit's keys: -A autostash, -a autosquash.
                    Suffix::Switch(Switch::on(
                        "-A",
                        "--autostash",
                        "Stash uncommitted changes around the rebase",
                    )),
                    Suffix::Switch(Switch::negatable(
                        "-a",
                        "--autosquash",
                        "--no-autosquash",
                        "rebase.autoSquash",
                        "Honor fixup!/squash! commits",
                    )),
                    Suffix::Switch(Switch::new("-m", "--rebase-merges", "Rebase merge commits")),
                    Suffix::Switch(Switch::new(
                        "-u",
                        "--update-refs",
                        "Update branches in the rebased range",
                    )),
                ],
            },
            Group {
                title: plain_title("Rebase onto"),
                suffixes: vec![
                    Action::target(
                        "p",
                        push_remote,
                        Command::RebaseOntoPushRemote,
                        t.push_remote.is_some(),
                    ),
                    Action::target(
                        "u",
                        upstream_label(t),
                        Command::RebaseOntoUpstream,
                        t.upstream.is_some(),
                    ),
                    Action::suffix("e", "elsewhere", Command::RebaseElsewhere),
                ],
            },
            Group {
                title: plain_title("Rebase"),
                suffixes: vec![
                    Action::suffix("i", "interactively", Command::RebaseInteractive),
                    Action::suffix("f", "to autosquash", Command::RebaseAutosquash),
                    Action::suffix("w", "to reword a commit", Command::RebaseRewordCommit),
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
                    Suffix::Switch(
                        Switch::new("-f", "--ff-only", "Fast-forward only")
                            .exclusive_with(&["--no-ff"]),
                    ),
                ],
            },
            Group {
                title: plain_title("Merge"),
                suffixes: vec![
                    Action::suffix("m", "merge", Command::MergePlain),
                    Action::suffix("n", "merge, don't commit", Command::MergeNoCommit),
                    Action::suffix("s", "squash merge", Command::MergeSquash),
                ],
            },
        ],
    }
}

pub fn cherry_pick_transient() -> Transient {
    Transient {
        title: plain_title("Cherry-pick"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(
                        Switch::on("-F", "--ff", "Attempt fast-forward").exclusive_with(&["-x"]),
                    ),
                    Suffix::Switch(Switch::new(
                        "-x",
                        "-x",
                        "Reference cherry in commit message",
                    )),
                    Suffix::Switch(Switch::new("-e", "--edit", "Edit commit messages")),
                    Suffix::Switch(Switch::new("-s", "--signoff", "Add Signed-off-by line")),
                    Suffix::Option(Opt {
                        key: "-m",
                        arg: "--mainline=",
                        description: "Replay merge relative to parent",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                ],
            },
            Group {
                title: plain_title("Apply here"),
                suffixes: vec![
                    Action::suffix("A", "pick", Command::CherryPick),
                    Action::suffix("a", "apply", Command::CherryApply),
                    Action::suffix("r", "range", Command::CherryPickRange),
                ],
            },
        ],
    }
}

pub fn revert_transient(style: KeymapStyle) -> Transient {
    let (revert_key, reverse_key) = match style {
        KeymapStyle::EvilCollection => ("_", "-"),
        KeymapStyle::Vanilla => ("V", "v"),
    };
    Transient {
        title: plain_title("Revert"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                // No `--edit`/`--no-edit`: revert always takes git's default
                // message (`--no-edit`, forced at run time). An interactive
                // `--edit` can't work in our background-git model — it would
                // block on an editor that isn't there. Documented deviation.
                suffixes: vec![
                    Suffix::Switch(Switch::new("-s", "--signoff", "Add Signed-off-by line")),
                    Suffix::Option(Opt {
                        key: "-m",
                        arg: "--mainline=",
                        description: "Replay merge relative to parent",
                        completion: Completion::None,
                        pathspec: false,
                    }),
                ],
            },
            Group {
                title: plain_title("Actions"),
                suffixes: vec![
                    Action::suffix(revert_key, "revert commit", Command::RevertCommit),
                    Action::suffix(reverse_key, "revert changes", Command::RevertNoCommit),
                    Action::suffix("r", "range", Command::RevertRange),
                ],
            },
        ],
    }
}

/// The transient shown when a sequence's prefix is pressed while that sequence
/// is already in progress: magit's continue / skip / abort, scoped to what the
/// operation supports. A merge has no continue/skip (you finish it by committing
/// the resolved index, or abort), so it shows only abort.
pub fn sequence_transient(kind: SequenceKind, style: KeymapStyle) -> Transient {
    let mut suffixes = Vec::new();
    let continue_key = match kind {
        SequenceKind::CherryPick => "A",
        SequenceKind::Revert => match style {
            KeymapStyle::EvilCollection => "_",
            KeymapStyle::Vanilla => "V",
        },
        SequenceKind::Am => "w",
        SequenceKind::Merge | SequenceKind::Rebase => "r",
    };
    if kind.can_continue() {
        suffixes.push(Action::suffix(
            continue_key,
            "continue",
            Command::SequenceContinue,
        ));
    }
    if kind.can_skip() {
        suffixes.push(Action::suffix("s", "skip", Command::SequenceSkip));
    }
    if kind.can_edit_todo() {
        suffixes.push(Action::suffix("e", "edit", Command::SequenceEditTodo));
    }
    suffixes.push(Action::suffix("a", "abort", Command::SequenceAbort));
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
                Action::suffix(
                    "t",
                    "shared at toplevel (.gitignore)",
                    Command::IgnoreToplevel,
                ),
                Action::suffix(
                    "s",
                    "shared in subdirectory (.gitignore)",
                    Command::IgnoreSubdir,
                ),
                Action::suffix("p", "privately (.git/info/exclude)", Command::IgnorePrivate),
                Action::suffix("g", "privately for all repositories", Command::IgnoreGlobal),
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
                Action::suffix("m", "mixed (HEAD and index)", Command::ResetMixed),
                Action::suffix("s", "soft (HEAD only)", Command::ResetSoft),
                Action::suffix("h", "hard (HEAD, index, working tree)", Command::ResetHard),
                Action::suffix(
                    "k",
                    "keep (HEAD and index, keep uncommitted)",
                    Command::ResetKeep,
                ),
                Action::suffix("i", "index (only)", Command::ResetIndex),
                Action::suffix("w", "worktree (only)", Command::ResetWorktree),
            ],
        }],
    }
}

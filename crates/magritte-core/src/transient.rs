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
    /// Log the current branch (HEAD).
    LogCurrent,
    /// Log all branches (`--all`).
    LogAll,
    /// Log another ref (prompts for one).
    LogOther,
    /// Reflog of HEAD.
    LogReflog,
}

/// A toggleable flag (e.g. `-f` → `--force-with-lease`).
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    pub key: &'static str,
    pub arg: &'static str,
    pub description: &'static str,
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
/// `keys` may list several keystrokes (e.g. `gg`).
#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub keys: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone)]
pub enum Suffix {
    Switch(Switch),
    Action(Action),
    Option(Opt),
    Info(Info),
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
                    Suffix::Switch(Switch {
                        key: "-f",
                        arg: "--force-with-lease",
                        description: "Force with lease",
                    }),
                    Suffix::Switch(Switch {
                        key: "-n",
                        arg: "--dry-run",
                        description: "Dry run",
                    }),
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
                    Suffix::Option(Opt {
                        key: "-n",
                        arg: "-n",
                        description: "Limit number of commits",
                        completion: Completion::None,
                    }),
                    Suffix::Option(Opt {
                        key: "-A",
                        arg: "--author=",
                        description: "Limit to author",
                        completion: Completion::Authors,
                    }),
                    Suffix::Option(Opt {
                        key: "-F",
                        arg: "--grep=",
                        description: "Search messages",
                        completion: Completion::None,
                    }),
                    Suffix::Option(Opt {
                        key: "-G",
                        arg: "-G",
                        description: "Search changes",
                        completion: Completion::None,
                    }),
                    Suffix::Option(Opt {
                        key: "-S",
                        arg: "-S",
                        description: "Search occurrences",
                        completion: Completion::None,
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

pub fn commit_transient() -> Transient {
    Transient {
        title: plain_title("Commit"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch {
                        key: "-a",
                        arg: "--all",
                        description: "Stage all modified and deleted files",
                    }),
                    Suffix::Switch(Switch {
                        key: "-e",
                        arg: "--allow-empty",
                        description: "Allow empty commit",
                    }),
                    Suffix::Switch(Switch {
                        key: "-n",
                        arg: "--no-verify",
                        description: "Disable hooks",
                    }),
                    Suffix::Switch(Switch {
                        key: "-s",
                        arg: "--signoff",
                        description: "Add Signed-off-by line",
                    }),
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
                suffixes: vec![Suffix::Switch(Switch {
                    key: "-r",
                    arg: "--rebase",
                    description: "Rebase local commits",
                })],
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
                suffixes: vec![Suffix::Switch(Switch {
                    key: "-p",
                    arg: "--prune",
                    description: "Prune deleted branches",
                })],
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

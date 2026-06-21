//! The transient model — magit's popup command menus (`P` push, `F` pull,
//! `f` fetch, `c` commit, …).
//!
//! A [`Transient`] is a declarative tree of groups and suffixes (switches and
//! actions). The model is UI-agnostic: it carries keys and descriptions as
//! data, but knows nothing about rendering. The frontend renders the popup,
//! tracks which switches are toggled on, and dispatches keys; when an action
//! fires it calls [`Repo::execute`] with the active switch arguments.

use crate::error::{Error, Result};
use crate::repo::Repo;

/// The git operation an [`Action`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Push,
    PushSetUpstream,
    Pull,
    Fetch,
    FetchAll,
    /// New commit (needs a message — handled via the editor, not `execute`).
    CommitCreate,
    /// Amend HEAD (needs a message).
    CommitAmend,
    /// Reword HEAD (needs a message).
    CommitReword,
    /// Amend HEAD with staged changes, keeping its message.
    CommitExtend,
}

/// A toggleable flag (e.g. `-f` → `--force-with-lease`).
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    pub key: &'static str,
    pub arg: &'static str,
    pub description: &'static str,
}

/// An invokable command (e.g. `p` → push).
#[derive(Debug, Clone, Copy)]
pub struct Action {
    pub key: &'static str,
    pub description: &'static str,
    pub command: Command,
}

#[derive(Debug, Clone, Copy)]
pub enum Suffix {
    Switch(Switch),
    Action(Action),
}

pub struct Group {
    pub title: &'static str,
    pub suffixes: Vec<Suffix>,
}

pub struct Transient {
    pub title: &'static str,
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
                Suffix::Action(_) => None,
            })
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

pub fn push_transient() -> Transient {
    Transient {
        title: "Push",
        groups: vec![
            Group {
                title: "Arguments",
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
                title: "Push",
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "p",
                        description: "Push to upstream",
                        command: Command::Push,
                    }),
                    Suffix::Action(Action {
                        key: "u",
                        description: "Push and set upstream (origin)",
                        command: Command::PushSetUpstream,
                    }),
                ],
            },
        ],
    }
}

pub fn commit_transient() -> Transient {
    Transient {
        title: "Commit",
        groups: vec![
            Group {
                title: "Arguments",
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
                title: "Create",
                suffixes: vec![Suffix::Action(Action {
                    key: "c",
                    description: "Commit",
                    command: Command::CommitCreate,
                })],
            },
            Group {
                title: "Edit HEAD",
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "e",
                        description: "Extend (keep message)",
                        command: Command::CommitExtend,
                    }),
                    Suffix::Action(Action {
                        key: "a",
                        description: "Amend",
                        command: Command::CommitAmend,
                    }),
                    Suffix::Action(Action {
                        key: "w",
                        description: "Reword (message only)",
                        command: Command::CommitReword,
                    }),
                ],
            },
        ],
    }
}

pub fn pull_transient() -> Transient {
    Transient {
        title: "Pull",
        groups: vec![
            Group {
                title: "Arguments",
                suffixes: vec![Suffix::Switch(Switch {
                    key: "-r",
                    arg: "--rebase",
                    description: "Rebase local commits",
                })],
            },
            Group {
                title: "Pull",
                suffixes: vec![Suffix::Action(Action {
                    key: "F",
                    description: "Pull from upstream",
                    command: Command::Pull,
                })],
            },
        ],
    }
}

pub fn fetch_transient() -> Transient {
    Transient {
        title: "Fetch",
        groups: vec![
            Group {
                title: "Arguments",
                suffixes: vec![Suffix::Switch(Switch {
                    key: "-p",
                    arg: "--prune",
                    description: "Prune deleted branches",
                })],
            },
            Group {
                title: "Fetch",
                suffixes: vec![
                    Suffix::Action(Action {
                        key: "f",
                        description: "Fetch from upstream",
                        command: Command::Fetch,
                    }),
                    Suffix::Action(Action {
                        key: "a",
                        description: "Fetch all remotes",
                        command: Command::FetchAll,
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

    /// Run a transient command with the given active switch arguments.
    /// Returns git's progress/result text (push and fetch report on stderr).
    pub fn execute(&self, command: Command, switches: &[String]) -> Result<String> {
        let mut args: Vec<String> = match command {
            Command::Push => vec!["push".into()],
            Command::PushSetUpstream => {
                let branch = self.current_branch()?.ok_or_else(|| {
                    Error::Message("cannot set upstream: HEAD is detached".into())
                })?;
                vec!["push".into(), "--set-upstream".into(), "origin".into(), branch]
            }
            Command::Pull => vec!["pull".into()],
            Command::Fetch => vec!["fetch".into()],
            Command::FetchAll => vec!["fetch".into(), "--all".into()],
            Command::CommitExtend => {
                vec!["commit".into(), "--amend".into(), "--no-edit".into()]
            }
            Command::CommitCreate | Command::CommitAmend | Command::CommitReword => {
                return Err(Error::Message(
                    "commit requires a message (use the editor)".into(),
                ));
            }
        };
        args.extend(switches.iter().cloned());

        let out = self.run(&args)?;
        let stderr = out.stderr.trim();
        let msg = if stderr.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            stderr.to_string()
        };
        Ok(msg)
    }
}

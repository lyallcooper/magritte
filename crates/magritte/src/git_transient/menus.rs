//! The built-in transient definitions — magit's popups, one builder per
//! command family (push, pull, fetch, commit, branch, rebase, …).

use magritte_core::remote::Upstream;
use magritte_core::{RemoteTargets, SequenceKind};

use super::{
    plain_title, Action, Command, Completion, Group, KeymapStyle, Opt, Suffix, Switch, TitleSpan,
    Transient, Variable, AUTHORS, FILES,
};

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
            TitleSpan::accent(b.clone()),
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
            Group {
                title: plain_title("Push"),
                suffixes: vec![
                    Action::suffix("o", "another branch", Command::PushOther),
                    Action::suffix("T", "a tag", Command::PushTag),
                    Action::suffix("t", "all tags", Command::PushTags),
                ],
            },
        ],
    }
}

/// The git-config variable groups scoped to `branch` (its own settings plus the
/// repository-wide defaults), shared by the branch transient's inline Configure
/// section and the `magit-branch-configure` sub-transient. `remotes` seeds the
/// push-remote choice lists.
fn branch_config_groups(branch: &str, remotes: Vec<String>) -> Vec<Group> {
    vec![
        Group {
            title: vec![TitleSpan::text("Configure "), TitleSpan::accent(branch)],
            suffixes: vec![
                Variable::value(
                    "d",
                    format!("branch.{branch}.description"),
                    "description",
                    Completion::None,
                ),
                Variable::choices(
                    "r",
                    format!("branch.{branch}.rebase"),
                    "rebase",
                    &["true", "false"],
                    Some("pull.rebase"),
                    Some("false"),
                ),
                Variable::choices_of(
                    "p",
                    format!("branch.{branch}.pushRemote"),
                    "pushRemote",
                    remotes.clone(),
                    Some("remote.pushDefault"),
                ),
            ],
        },
        Group {
            title: plain_title("Configure repository defaults"),
            suffixes: vec![
                Variable::choices(
                    "R",
                    "pull.rebase",
                    "pull.rebase",
                    &["true", "false"],
                    None,
                    Some("false"),
                ),
                Variable::choices_of(
                    "P",
                    "remote.pushDefault",
                    "remote.pushDefault",
                    remotes,
                    None,
                ),
            ],
        },
    ]
}

/// The branch transient. When `configure` is `Some((branch, remotes))` — i.e. a
/// branch is checked out — its git-config variables are shown inline above the
/// actions (magit's `magit-branch-direct-configure`); the `C` sub-transient is
/// always available regardless.
pub fn branch_transient(style: KeymapStyle, configure: Option<(&str, Vec<String>)>) -> Transient {
    let mut groups = Vec::new();
    if let Some((branch, remotes)) = configure {
        groups.extend(branch_config_groups(branch, remotes));
    }
    groups.extend([
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
                Action::suffix("C", "configure…", Command::BranchConfigure),
            ],
        },
    ]);
    Transient {
        title: plain_title("Branch"),
        groups,
    }
}

/// The branch config sub-transient (magit's `magit-branch-configure`): the same
/// git-config variables the branch transient can show inline, on their own. No
/// panel title — the leading group header already reads "Configure <branch>".
pub fn branch_configure_transient(branch: &str, remotes: Vec<String>) -> Transient {
    Transient {
        title: Vec::new(),
        groups: branch_config_groups(branch, remotes),
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
                suffixes: vec![
                    Action::suffix("t", "tag", Command::TagCreate),
                    Action::suffix("r", "release", Command::TagRelease),
                ],
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

/// The git-config variable group scoped to `remote`, shared by the remote
/// transient's inline Configure section and the `magit-remote-configure`
/// sub-transient.
fn remote_config_group(remote: &str) -> Group {
    Group {
        title: vec![TitleSpan::text("Configure "), TitleSpan::accent(remote)],
        suffixes: vec![
            Variable::value("u", format!("remote.{remote}.url"), "url", Completion::None),
            Variable::value(
                "U",
                format!("remote.{remote}.fetch"),
                "fetch refspec",
                Completion::None,
            ),
            Variable::value(
                "s",
                format!("remote.{remote}.pushurl"),
                "pushurl",
                Completion::None,
            ),
            Variable::value(
                "S",
                format!("remote.{remote}.push"),
                "push refspec",
                Completion::None,
            ),
            Variable::choices(
                "O",
                format!("remote.{remote}.tagOpt"),
                "tag fetching",
                &["--no-tags", "--tags"],
                None,
                None,
            ),
            Variable::choices(
                "h",
                format!("remote.{remote}.followRemoteHEAD"),
                "follow remote HEAD",
                &["create", "always", "warn"],
                None,
                None,
            ),
        ],
    }
}

/// The remote transient. When `configure` names a remote (the current one) its
/// git-config variables show inline above the actions (magit's
/// `magit-remote-direct-configure`); the `C` sub-transient is always available.
pub fn remote_transient(style: KeymapStyle, configure: Option<&str>) -> Transient {
    let mut groups = Vec::new();
    if let Some(remote) = configure {
        groups.push(remote_config_group(remote));
    }
    groups.extend([
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
                Action::suffix("C", "configure…", Command::RemoteConfigure),
            ],
        },
    ]);
    Transient {
        title: plain_title("Remote"),
        groups,
    }
}

/// The remote config sub-transient (magit's `magit-remote-configure`): the same
/// git-config variables the remote transient can show inline, on their own. No
/// panel title — the group header already reads "Configure <remote>".
pub fn remote_configure_transient(remote: &str) -> Transient {
    Transient {
        title: Vec::new(),
        groups: vec![remote_config_group(remote)],
    }
}

pub fn stash_transient() -> Transient {
    Transient {
        title: plain_title("Stash"),
        groups: vec![
            Group {
                title: plain_title("Arguments"),
                suffixes: vec![
                    Suffix::Switch(Switch::new(
                        "-u",
                        "--include-untracked",
                        "Also save untracked files",
                    )),
                    Suffix::Switch(Switch::new(
                        "-a",
                        "--all",
                        "Also save untracked and ignored files",
                    )),
                    // magit's file limit lives on the `z P` push sub-transient;
                    // we have one stash menu, so it rides here and applies to
                    // every push variant (snapshots ignore it, like magit's).
                    Suffix::Option(Opt {
                        key: "--",
                        arg: "",
                        description: "Limit to files",
                        completion: Completion::Source(FILES),
                        pathspec: true,
                    }),
                ],
            },
            Group {
                title: plain_title("Stash"),
                suffixes: vec![
                    Action::suffix("z", "both", Command::StashPush),
                    Action::suffix("i", "index", Command::StashPushStaged),
                    Action::suffix("x", "keeping index", Command::StashPushKeepIndex),
                ],
            },
            Group {
                title: plain_title("Snapshot"),
                suffixes: vec![
                    Action::suffix("Z", "both", Command::StashSnapshotBoth),
                    Action::suffix("I", "index", Command::StashSnapshotIndex),
                    Action::suffix("W", "worktree", Command::StashSnapshotWorktree),
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
            Group {
                title: plain_title("Transform"),
                suffixes: vec![Action::suffix("b", "branch", Command::StashBranch)],
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
                    // On by default: `--follow` only reaches git when the log
                    // is limited to exactly one file (it errors otherwise), so
                    // single-file histories follow renames unless toggled off.
                    Suffix::Switch(Switch::on("-f", "--follow", "Follow renames (single file)")),
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
                        completion: Completion::Source(AUTHORS),
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
                        completion: Completion::Source(FILES),
                        pathspec: true,
                    }),
                ],
            },
            Group {
                title: plain_title("Log"),
                suffixes: vec![
                    Action::suffix("l", "current", Command::LogCurrent),
                    Action::suffix("f", "file", Command::LogFile),
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
                        completion: Completion::Source(FILES),
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
                    // magit's `-X --diff-merges=` is omitted: every diff here is
                    // a two-endpoint `git diff`, where git accepts the flag but
                    // ignores it — the row could never do anything.
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
                        completion: Completion::Source(AUTHORS),
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
                    Suffix::Option(Opt {
                        key: "-s",
                        arg: "--strategy=",
                        description: "Strategy",
                        completion: Completion::OneOf(&[
                            "resolve",
                            "recursive",
                            "octopus",
                            "ours",
                            "subtree",
                            "ort",
                        ]),
                        pathspec: false,
                    }),
                ],
            },
            Group {
                title: plain_title("Merge"),
                suffixes: vec![
                    Action::suffix("m", "merge", Command::MergePlain),
                    Action::suffix("e", "merge, edit message", Command::MergeEditMsg),
                    Action::suffix("n", "merge, don't commit", Command::MergeNoCommit),
                    Action::suffix("s", "squash merge", Command::MergeSquash),
                    Action::suffix("p", "preview merge", Command::MergePreview),
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
                // No `--edit`: like revert, an interactive message editor can't
                // work in our background-git model — it would block on an
                // editor that isn't there. Documented deviation.
                suffixes: vec![
                    Suffix::Switch(
                        Switch::on("-F", "--ff", "Attempt fast-forward").exclusive_with(&["-x"]),
                    ),
                    Suffix::Switch(Switch::new(
                        "-x",
                        "-x",
                        "Reference cherry in commit message",
                    )),
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
    // A merge is finished by committing the resolved index (magit's in-progress
    // `m` "Commit merge" runs magit-commit-create), not by `--continue`.
    if matches!(kind, SequenceKind::Merge) {
        suffixes.push(Action::suffix("m", "commit merge", Command::CommitCreate));
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

/// The bisect transient (`B`): when a bisect is running, mark the checked-out
/// commit good/bad/skip or reset; otherwise start one (magit's `B`).
pub fn bisect_transient(bisecting: bool) -> Transient {
    let suffixes = if bisecting {
        vec![
            Action::suffix("g", "good", Command::BisectGood),
            Action::suffix("b", "bad", Command::BisectBad),
            Action::suffix("s", "skip", Command::BisectSkip),
            Action::suffix("r", "reset", Command::BisectReset),
        ]
    } else {
        vec![Action::suffix(
            "B",
            "start (pick a known-good commit)",
            Command::BisectStart,
        )]
    };
    Transient {
        title: plain_title(if bisecting { "Bisecting" } else { "Bisect" }),
        groups: vec![Group {
            title: plain_title("Bisect"),
            suffixes,
        }],
    }
}

/// The patch transient (magit's `W`): create patches for a range, apply a diff
/// to the worktree, or apply a mailbox as commits (`git am`).
/// magit's `!` run transient: a git subcommand or a raw shell command, in the
/// repository root — or, when the cursor is on a file in a subdirectory, in
/// that directory (`workdir`; magit's buffer-local default-directory has no
/// GUI equivalent, so the rows name the concrete directory and only appear
/// when there is one). magit's Launch group — gitk/git-gui — doesn't apply.
pub fn run_transient(workdir: Option<&str>) -> Transient {
    let mut git = vec![Action::suffix(
        "!",
        "in repository root",
        Command::RunGitTopdir,
    )];
    let mut shell = vec![Action::suffix(
        "s",
        "in repository root",
        Command::RunShellTopdir,
    )];
    if let Some(dir) = workdir {
        git.push(Action::suffix(
            "p",
            format!("in {dir}/ (file at point)"),
            Command::RunGitWorkdir,
        ));
        shell.push(Action::suffix(
            "S",
            format!("in {dir}/ (file at point)"),
            Command::RunShellWorkdir,
        ));
    }
    Transient {
        title: plain_title("Run"),
        groups: vec![
            Group {
                title: plain_title("Run git subcommand"),
                suffixes: git,
            },
            Group {
                title: plain_title("Run shell command"),
                suffixes: shell,
            },
        ],
    }
}

pub fn patch_transient() -> Transient {
    Transient {
        title: plain_title("Patch"),
        groups: vec![Group {
            title: plain_title("Patch"),
            suffixes: vec![
                Action::suffix("c", "create (format-patch a range)", Command::PatchCreate),
                Action::suffix("a", "apply a patch to the worktree", Command::PatchApply),
                Action::suffix("w", "apply a mailbox as commits (am)", Command::PatchAm),
            ],
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
        groups: vec![
            Group {
                title: plain_title("Reset"),
                suffixes: vec![
                    Action::suffix("b", "branch", Command::ResetBranch),
                    Action::suffix("f", "a file", Command::ResetFile),
                ],
            },
            Group {
                title: plain_title("Reset this"),
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
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(branch: &str, push_remote: &str, up_remote: &str, up_branch: &str) -> RemoteTargets {
        RemoteTargets {
            branch: Some(branch.to_string()),
            push_remote: Some(push_remote.to_string()),
            upstream: Some(Upstream {
                remote: up_remote.to_string(),
                branch: up_branch.to_string(),
            }),
            sole_remote: None,
        }
    }

    #[test]
    fn push_transient_defines_force_and_actions() {
        let tr = push_transient(&RemoteTargets::default());
        assert!(tr.switches().any(|s| s.arg == "--force-with-lease"));
        // push-remote / upstream / elsewhere.
        assert!(tr.action_for("p").is_some());
        assert!(tr.action_for("u").is_some());
        assert!(tr.action_for("e").is_some());
    }

    #[test]
    fn push_transient_labels_resolved_targets() {
        // A configured upstream names the resolved ref on its action row.
        let tr = push_transient(&targets("main", "origin", "origin", "main"));
        match tr.action_for("u") {
            Some(a) => assert_eq!(a.description, "origin/main"),
            None => panic!("missing upstream action"),
        }
    }

    #[test]
    fn action_dispatches_on_either_collapsed_key() {
        // The collapsed push entry is invokable by both `p` and `u`.
        let push = push_transient(&targets("main", "origin", "origin", "main"));
        assert!(push.action_for("p").is_some());
        assert!(push.action_for("u").is_some());
        assert_eq!(
            push.action_for("p").map(|a| &a.command),
            push.action_for("u").map(|a| &a.command),
        );
    }
}

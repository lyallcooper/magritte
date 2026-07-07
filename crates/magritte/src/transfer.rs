//! Push/pull/fetch orchestration: resolving a transfer command to a concrete
//! remote (prompting only when there's a real choice), the remote/branch
//! pickers for the "elsewhere" targets, and the transfer jobs themselves.
//! `impl StatusView` like the other view slices.

use gpui::{Context, Window};

use crate::*;

impl StatusView {
    /// Resolve a push/pull/fetch command to a concrete remote and run it: use
    /// the configured push-remote/upstream when present, otherwise pick a remote
    /// (prompting only when there's a real choice) — setting the relevant config
    /// for first push, like magit.
    pub(crate) fn dispatch_transfer(
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
            self.set_status(
                "HEAD is detached — can't push/pull a branch".to_string(),
                false,
                cx,
            );
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
    pub(crate) fn resolve_remote(
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
                self.set_status("No remotes configured".to_string(), false, cx);
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

    /// Always show the picker for a pending transfer (the "elsewhere"
    /// fetch, where the point is to choose) — even with a single remote.
    pub(crate) fn prompt_remote(
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
            self.set_status("No remotes configured".to_string(), false, cx);
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
    pub(crate) fn prompt_branch(
        &mut self,
        transfer: Transfer,
        create: bool,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let create_mode = if create {
            CreateMode::RemoteBranch
        } else {
            CreateMode::None
        };
        // The full remote-branch listing scales with the remotes' ref count, so
        // it loads into the picker asynchronously (like the branch/tag pickers)
        // rather than stalling the keypress on a big repo.
        let transfer_for_list = transfer.clone();
        self.open_listed_picker(
            PickerAction::Transfer(transfer),
            create_mode,
            switches,
            move |repo| {
                let remotes = repo.remotes().unwrap_or_default();
                if remotes.is_empty() {
                    // Empty + selection-only closes with "No remotes configured".
                    return Ok(Vec::new());
                }
                let existing = repo.remote_branches().unwrap_or_default();
                // Pull lists only existing branches (you can't pull one that
                // doesn't exist). Push seeds the same-named target on every
                // remote — like magit — so `origin/<current>` is always a
                // normal candidate, existing or not.
                Ok(match &transfer_for_list {
                    Transfer::PushRef { branch } if create => {
                        targets::seed_push_branches(repo, &remotes, branch, existing)
                    }
                    _ => existing,
                })
            },
            window,
            cx,
        );
    }

    /// Run a resolved push/pull/fetch on the background executor, then refresh.
    /// `chosen` is a remote name for the remote-level transfers, or a
    /// `remote/branch` ref (possibly newly typed) for the `*Ref` ones.
    pub(crate) fn run_transfer(
        &mut self,
        transfer: Transfer,
        chosen: String,
        switches: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let progress = format!("{}…", transfer.verb());
        let done = match &transfer {
            Transfer::Push { .. } | Transfer::PushRef { .. } => "Pushed",
            Transfer::Pull { .. } | Transfer::PullRef => "Pulled",
            Transfer::Fetch => "Fetched",
        };
        self.run_job(
            &progress,
            done,
            move |repo| match transfer {
                Transfer::Push {
                    branch,
                    set_upstream,
                    save_push_remote,
                } => {
                    // Save the push remote only after the push lands, so a
                    // rejected/offline push doesn't leave config pointing at a
                    // remote we never pushed to. A failure to record it surfaces
                    // rather than being swallowed.
                    let pushed = repo.push_to(&chosen, &branch, set_upstream, &switches);
                    if pushed.is_ok() && save_push_remote {
                        repo.set_push_remote(&branch, &chosen)?;
                    }
                    pushed
                }
                Transfer::PushRef { branch } => {
                    let (remote, target) = targets::split_ref(&repo, &chosen);
                    repo.push_ref(&remote, &branch, &target, &switches)
                }
                Transfer::Pull { branch } => repo.pull_from(&chosen, &branch, &switches),
                Transfer::PullRef => {
                    let (remote, branch) = targets::split_ref(&repo, &chosen);
                    repo.pull_from(&remote, &branch, &switches)
                }
                Transfer::Fetch => repo.fetch_from(&chosen, &switches),
            },
            cx,
        );
    }

    /// `git fetch --all` (no remote needed).
    pub(crate) fn run_fetch_all(&mut self, switches: Vec<String>, cx: &mut Context<Self>) {
        self.run_job(
            "Fetching…",
            "Fetched",
            move |repo| repo.fetch_all(&switches),
            cx,
        );
    }
}

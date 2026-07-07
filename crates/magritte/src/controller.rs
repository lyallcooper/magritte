//! Command dispatch: `fire_action` routing a transient/palette command to its
//! per-family dispatcher (branch, tag, stash, remote, diff, reset, merge, …)
//! and the picker orchestration those prompts share. The job runners and toast
//! plumbing live in `jobs`, push/pull/fetch in `transfer`, and the rebase
//! flows in `rebase_flow`. It stays `impl StatusView` because a GPUI view owns
//! its state and behavior together; a separate non-view controller would mean
//! message-passing ceremony for no gain in a single-Entity app (see the FB5
//! disposition in FEEDBACK.md).

#![allow(clippy::too_many_arguments)]

use gpui::prelude::*;
use gpui::{Context, SharedString, UniformListScrollHandle, Window};

use crate::*;

impl StatusView {
    /// Fire a leaf command (a transient suffix) with already-gathered arguments.
    /// Shared by the transient (which passes its toggled switches/options) and
    /// the `:` palette (which fires with defaults via [`Self::fire_command_default`]).
    pub(crate) fn fire_action(
        &mut self,
        command: transient::Command,
        fired: ActionArgs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ActionArgs {
            args,
            paths,
            targets,
            limit,
        } = fired;
        self.popup = None;
        // Repaint now: a dispatcher that early-returns (no repo, nothing at
        // point) must still visibly close the popup.
        cx.notify();
        use transient::Command::*;
        match command {
            CommitCreate => self.start_commit(args, window, cx),
            // Amend/reword/extend rewrite HEAD: warn first if it's published.
            CommitAmend | CommitReword | CommitExtend => {
                self.begin_history_rewrite(command, args, window, cx)
            }
            // `c R` is hosted in the commit transient for Magit parity, but the
            // operation itself is an interactive rebase. Commit switches such as
            // `--date=now` are not valid `git rebase` options; use `r w` when
            // rebase-specific switches should be carried through.
            CommitRewordPast => self.reword_past_selected(Vec::new(), window, cx),
            // Fixup/squash into a target commit (at point, or via log-select).
            // Commit switches (--all, --gpg-sign, …) carry through; the instant
            // variants then autosquash.
            CommitFixup => self.fixup_squash_selected(SquashOp::Fixup, args, cx),
            CommitSquash => self.fixup_squash_selected(SquashOp::Squash, args, cx),
            CommitInstantFixup => self.fixup_squash_selected(SquashOp::InstantFixup, args, cx),
            CommitInstantSquash => self.fixup_squash_selected(SquashOp::InstantSquash, args, cx),
            // Push/pull/fetch resolve a remote (prompting if needed) then run.
            PushPushRemote | PushUpstream | PushElsewhere | PullPushRemote | PullUpstream
            | PullElsewhere | FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => {
                self.dispatch_transfer(command, &targets, args, window, cx)
            }
            BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
                self.dispatch_branch(command, window, cx)
            }
            TagCreate | TagRelease | TagDelete => self.dispatch_tag(command, args, window, cx),
            RemoteAdd | RemoteRename | RemoteRemove => {
                self.dispatch_remote(command, args, window, cx)
            }
            // The `!` run transient: git/shell, at the root or the file at
            // point's directory (the GUI reading of magit's default-directory).
            RunGitTopdir => self.open_run_prompt(false, None, window, cx),
            RunGitWorkdir => self.open_run_prompt(false, self.dir_at_point(), window, cx),
            RunShellTopdir => self.open_run_prompt(true, None, window, cx),
            RunShellWorkdir => self.open_run_prompt(true, self.dir_at_point(), window, cx),
            BranchConfigure => self.open_branch_configure(window, cx),
            RemoteConfigure => self.open_remote_configure(window, cx),
            ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => {
                self.dispatch_reset(command, window, cx)
            }
            MergePlain | MergeNoCommit | MergeSquash => {
                self.dispatch_merge(command, args, window, cx)
            }
            CherryPick | CherryPickRange | CherryApply | RevertCommit | RevertRange
            | RevertNoCommit => self.dispatch_pick(command, args, window, cx),
            RebaseAutosquash => self.autosquash(args, cx),
            RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive
            | RebaseRewordCommit => self.dispatch_rebase(command, args, &targets, window, cx),
            IgnoreToplevel | IgnoreSubdir | IgnorePrivate | IgnoreGlobal => {
                self.dispatch_ignore(command, window, cx)
            }
            StashPush => self.prompt_stash_message(false, window, cx),
            StashPushAll => self.prompt_stash_message(true, window, cx),
            StashApply | StashPop | StashDrop => self.dispatch_stash(command, window, cx),
            DiffDwim | DiffRange | DiffUnstaged | DiffStaged | DiffWorktree | DiffCommit => {
                self.dispatch_diff(command, args, paths, window, cx)
            }
            // Log: assemble flags + scope + pathspecs in the order git needs.
            LogCurrent => self.start_log(build_log_args(args, LogScope::Current, paths, limit), cx),
            LogAll => self.start_log(build_log_args(args, LogScope::All, paths, limit), cx),
            LogOther => self.prompt_log_ref(args, paths, limit, window, cx),
            LogReflog => self.start_reflog(limit, cx),
            SequenceContinue => self.sequence_continue(window, cx),
            SequenceSkip => self.sequence_skip(window, cx),
            SequenceAbort => self.sequence_abort(window, cx),
            SequenceEditTodo => self.open_rebase_edit_todo(cx),
            BisectStart => self.open_value_prompt(PickerAction::BisectBadRev, "HEAD", window, cx),
            BisectGood => self.run_bisect_mark(BisectMark::Good, cx),
            BisectBad => self.run_bisect_mark(BisectMark::Bad, cx),
            BisectSkip => self.run_bisect_mark(BisectMark::Skip, cx),
            BisectReset => self.run_bisect_reset(cx),
            PatchApply => self.open_value_prompt(PickerAction::PatchApply, "", window, cx),
            PatchAm => self.open_value_prompt(PickerAction::PatchAm, "", window, cx),
            PatchCreate => self.open_value_prompt(PickerAction::PatchCreate, "-1 HEAD", window, cx),
        }
    }

    /// Fire a leaf command from the palette: no transient was open, so use
    /// default arguments (no switches/options, current targets, default log
    /// limit). The command still opens its own picker/editor when it needs one.
    pub(crate) fn fire_command_default(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.remote_targets();
        self.fire_action(
            command,
            ActionArgs::defaults(targets, Self::LOG_LIMIT),
            window,
            cx,
        );
    }

    /// Open the stash picker for an apply/pop/drop command.
    pub(crate) fn dispatch_stash(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        let action = match command {
            StashApply => StashAction::Apply,
            StashPop => StashAction::Pop,
            StashDrop => StashAction::Drop,
            _ => return,
        };
        self.open_listed_picker(
            PickerAction::Stash(action),
            CreateMode::None,
            Vec::new(),
            |r| Ok(r.stash_list()?.iter().map(|s| s.display()).collect()),
            window,
            cx,
        );
    }

    /// The shared body of the option/variable value prompts: derive the create
    /// mode and synchronous candidates from `completion` (a fixed value set is
    /// selection-only; everything else is value entry with the candidates as
    /// mere suggestions), open the picker with `resume` stashed so the
    /// transient reopens, and return the picker's generation stamp for async
    /// candidate loads.
    fn open_completion_prompt(
        &mut self,
        action: PickerAction,
        completion: &transient::Completion,
        resume: TransientState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let create = match completion {
            transient::Completion::OneOf(_) => CreateMode::None,
            _ => CreateMode::Value,
        };
        // Candidates available synchronously (a fixed set); git-backed sources
        // load off the UI thread, so opening stays instant in big repos.
        let initial: Vec<String> = match completion {
            transient::Completion::OneOf(values) => values.iter().map(|v| v.to_string()).collect(),
            _ => Vec::new(),
        };
        let gen = self.picker_gen.bump();
        self.open_picker(action, initial, create, Vec::new(), window, cx);
        if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.gen = gen;
            p.resume = Some(Box::new(resume));
            // A free-text value with no completion candidates (e.g. `-n`) has no
            // candidate list — collapse it to just the input + hints.
            p.reserve_candidates = !matches!(completion, transient::Completion::None);
        }
        gen
    }

    /// Prompt for a transient option's value (free text, with completion
    /// candidates), stashing `resume` so the transient reopens with the value
    /// applied (or unchanged on cancel).
    pub(crate) fn open_option_prompt(
        &mut self,
        key: String,
        description: String,
        completion: transient::Completion,
        resume: TransientState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gen = self.open_completion_prompt(
            PickerAction::SetOption { key, description },
            &completion,
            resume,
            window,
            cx,
        );

        // Load git-backed candidates (authors, tracked files) asynchronously and
        // drop them into the open picker — `git ls-files` can be large/slow.
        let loader: Option<fn(&Repo) -> Vec<String>> = match completion {
            transient::Completion::Authors => Some(|r| r.authors().unwrap_or_default()),
            transient::Completion::Files => Some(|r| r.tracked_files().unwrap_or_default()),
            _ => None,
        };
        if let (Some(load), Some(repo)) = (loader, self.repo.clone()) {
            cx.spawn(async move |this, cx| {
                let items = cx
                    .background_executor()
                    .spawn(async move { load(&repo) })
                    .await;
                this.update(cx, |this, cx| {
                    // Only fill the prompt these candidates were loaded for —
                    // not a different option prompt the user opened meanwhile.
                    if !matches!(&this.popup, Some(Popup::Picker(p)) if p.gen == gen) {
                        return;
                    }
                    if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                        p.list
                            .set_choices(items.into_iter().map(SharedString::from).collect());
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Open the branch transient (`b`), inlining the current branch's git-config
    /// variables (magit's direct-configure) when a branch is checked out. Shared
    /// by the command and by popping back from the branch-configure sub-transient.
    pub(crate) fn open_branch_transient(&mut self, cx: &mut Context<Self>) {
        let branch = self.status.as_ref().and_then(|s| s.head.branch.clone());
        let remotes = self
            .repo
            .as_ref()
            .and_then(|r| r.remotes().ok())
            .unwrap_or_default();
        let style = self.config.keymap_preset.transient_style();
        let configure = branch.as_deref().map(|b| (b, remotes));
        self.open_transient(
            "branch",
            transient::branch_transient(style, configure),
            RemoteTargets::default(),
            cx,
        );
    }

    /// Open the remote transient (`M`), inlining the current remote's git-config
    /// variables when the repo has one. Shared by the command and by popping back
    /// from the remote-configure sub-transient.
    pub(crate) fn open_remote_transient(&mut self, cx: &mut Context<Self>) {
        let branch = self.status.as_ref().and_then(|s| s.head.branch.clone());
        let remote = self
            .repo
            .as_ref()
            .and_then(|r| targets::current_remote(r, branch.as_deref()));
        let style = self.config.keymap_preset.transient_style();
        self.open_transient(
            "remote",
            transient::remote_transient(style, remote.as_deref()),
            RemoteTargets::default(),
            cx,
        );
    }

    /// The shared shell of the branch/remote Configure entry points: report
    /// when there's nothing to configure, configure a sole candidate directly,
    /// and open the picker only when there's a real choice.
    fn open_configure_picker(
        &mut self,
        list: fn(&Repo) -> magritte_core::Result<Vec<String>>,
        empty_message: &str,
        action: PickerAction,
        configure: fn(&mut Self, String, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let candidates = self
            .repo
            .as_ref()
            .and_then(|r| list(r).ok())
            .unwrap_or_default();
        match candidates.as_slice() {
            [] => self.set_status(empty_message.to_string(), false, cx),
            [only] => configure(self, only.clone(), cx),
            _ => self.open_listed_picker(action, CreateMode::None, Vec::new(), list, window, cx),
        }
    }

    /// Open the branch config transient (magit's `magit-branch-configure`).
    /// Prompts for which branch to configure only when there's more than one
    /// local branch; a sole branch is configured directly.
    pub(crate) fn open_branch_configure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_configure_picker(
            Repo::local_branches,
            "No branch to configure",
            PickerAction::Branch(BranchAction::Configure),
            Self::open_branch_configure_for,
            window,
            cx,
        );
    }

    /// Open the branch config transient for a specific branch, seeded with the
    /// repo's remotes for the pushRemote choice lists.
    pub(crate) fn open_branch_configure_for(&mut self, branch: String, cx: &mut Context<Self>) {
        let remotes = self
            .repo
            .as_ref()
            .and_then(|r| r.remotes().ok())
            .unwrap_or_default();
        let def = transient::branch_configure_transient(&branch, remotes);
        self.open_transient("branch-configure", def, self.remote_targets(), cx);
    }

    /// Open the remote config transient (magit's `magit-remote-configure`),
    /// picking the remote first (the sole remote is used directly).
    pub(crate) fn open_remote_configure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_configure_picker(
            targets::remotes,
            "No remotes configured",
            PickerAction::Remote(RemoteAction::Configure),
            Self::open_remote_configure_for,
            window,
            cx,
        );
    }

    /// Open the remote config transient for a specific remote name.
    pub(crate) fn open_remote_configure_for(&mut self, remote: String, cx: &mut Context<Self>) {
        let def = transient::remote_configure_transient(&remote);
        self.open_transient("remote-configure", def, self.remote_targets(), cx);
    }

    /// Prompt for a free-text config-variable value (from a Configure transient),
    /// seeded with the current value, stashing `resume` so the transient reopens
    /// with the new value applied (mirrors [`Self::open_option_prompt`]).
    pub(crate) fn open_variable_prompt(
        &mut self,
        variable: String,
        description: String,
        completion: transient::Completion,
        current: String,
        resume: TransientState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_completion_prompt(
            PickerAction::SetVariable {
                variable,
                description,
            },
            &completion,
            resume,
            window,
            cx,
        );
        // Seed the input with the current value so it can be edited in place.
        self.seed_picker_input(&current, window, cx);
    }

    /// Prompt for a ref to log (`l o`), carrying the gathered flags, pathspecs,
    /// and limit through so they apply once the ref is chosen.
    pub(crate) fn prompt_log_ref(
        &mut self,
        flags: Vec<String>,
        paths: Vec<String>,
        limit: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_listed_picker(
            PickerAction::LogRef {
                flags,
                paths,
                limit,
            },
            CreateMode::Any,
            Vec::new(),
            targets::all_branches,
            window,
            cx,
        );
    }

    /// Open the picker for a branch-transient command: checkout/rename/delete
    /// pick an existing branch (listed off the UI thread); create reads a new
    /// name (free text, no listing).
    pub(crate) fn dispatch_branch(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repo.is_none() {
            return;
        }
        use transient::Command::*;
        // Checkout offers every branch; rename/delete only local ones.
        let listed = |this: &mut Self,
                      action: BranchAction,
                      list: fn(&Repo) -> magritte_core::Result<Vec<String>>,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
            this.open_listed_picker(
                PickerAction::Branch(action),
                CreateMode::None,
                Vec::new(),
                list,
                window,
                cx,
            );
        };
        match command {
            BranchCheckout => listed(
                self,
                BranchAction::Checkout,
                targets::all_branches,
                window,
                cx,
            ),
            BranchRename => listed(
                self,
                BranchAction::RenameFrom,
                Repo::local_branches,
                window,
                cx,
            ),
            BranchDelete => listed(self, BranchAction::Delete, Repo::local_branches, window, cx),
            BranchCreateCheckout => self.open_picker(
                PickerAction::Branch(BranchAction::Create { checkout: true }),
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            ),
            BranchCreate => self.open_picker(
                PickerAction::Branch(BranchAction::Create { checkout: false }),
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            ),
            _ => {}
        }
    }

    /// Tag transient suffix: create a tag at the commit at point (or HEAD), or
    /// delete a picked local tag.
    pub(crate) fn dispatch_tag(
        &mut self,
        command: transient::Command,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        match command {
            TagCreate => {
                let annotated = args.iter().any(|s| s == "--annotate");
                self.open_picker(
                    PickerAction::Tag(TagAction::Create { annotated }),
                    Vec::new(),
                    CreateMode::Any,
                    args,
                    window,
                    cx,
                )
            }
            TagRelease => {
                // Propose the next release tag off the UI thread (it reads the
                // tag list + HEAD), then open the name prompt seeded with it so
                // the user can review or bump the version before tagging.
                let annotated = args.iter().any(|s| s == "--annotate");
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                cx.spawn_in(window, async move |this, cx| {
                    let seed = cx
                        .background_executor()
                        .spawn(async move { repo.next_release_seed() })
                        .await;
                    let tag = seed.map(|s| s.tag).unwrap_or_default();
                    this.update_in(cx, |this, window, cx| {
                        // Drop a stale seed if the user moved on (another
                        // popup/screen) while the tag listing loaded.
                        if !this.ui_idle_for_prompt() {
                            return;
                        }
                        this.open_picker_seeded(
                            PickerAction::Tag(TagAction::Release { annotated }),
                            tag,
                            args,
                            window,
                            cx,
                        );
                    })
                    .ok();
                })
                .detach();
            }
            TagDelete => self.open_listed_picker(
                PickerAction::Tag(TagAction::Delete),
                CreateMode::None,
                Vec::new(),
                Repo::tags,
                window,
                cx,
            ),
            _ => {}
        }
    }

    /// Remote transient suffix: add/rename/remove configured remotes.
    pub(crate) fn dispatch_remote(
        &mut self,
        command: transient::Command,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        match command {
            RemoteAdd => self.open_picker(
                PickerAction::Remote(RemoteAction::AddName),
                Vec::new(),
                CreateMode::Any,
                args,
                window,
                cx,
            ),
            RemoteRename => self.open_listed_picker(
                PickerAction::Remote(RemoteAction::RenameFrom),
                CreateMode::None,
                Vec::new(),
                Repo::remotes,
                window,
                cx,
            ),
            RemoteRemove => self.open_listed_picker(
                PickerAction::Remote(RemoteAction::Remove),
                CreateMode::None,
                Vec::new(),
                Repo::remotes,
                window,
                cx,
            ),
            _ => {}
        }
    }

    /// Reset transient suffix: pick the target commit (a branch/ref, or type any
    /// revision), then reset HEAD to it in the chosen mode.
    pub(crate) fn dispatch_reset(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        let mode = match command {
            ResetSoft => ResetMode::Soft,
            ResetMixed => ResetMode::Mixed,
            ResetHard => ResetMode::Hard,
            ResetKeep => ResetMode::Keep,
            ResetIndex => ResetMode::Index,
            ResetWorktree => ResetMode::Worktree,
            _ => return,
        };
        // `Value`: the typed text is itself a valid target (any revision/sha),
        // with the branches as suggestions (loaded off the UI thread).
        self.open_listed_picker(
            PickerAction::Reset(mode),
            CreateMode::Value,
            Vec::new(),
            targets::all_branches,
            window,
            cx,
        );
    }

    /// Reset to `target`; a destructive reset (hard or worktree) confirms first.
    pub(crate) fn run_reset(&mut self, mode: ResetMode, target: String, cx: &mut Context<Self>) {
        if mode.is_destructive() {
            let what = if matches!(mode, ResetMode::Worktree) {
                "Reset worktree"
            } else {
                "Hard reset"
            };
            self.confirm = Some((format!("{what} to {target}?"), Confirm::Reset(mode, target)));
            cx.notify();
        } else {
            self.do_reset(mode, target, cx);
        }
    }

    /// Merge transient suffix: fold the action's mode into the toggled
    /// switches, then pick the branch/ref to merge.
    pub(crate) fn dispatch_merge(
        &mut self,
        command: transient::Command,
        mut args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        match command {
            MergeNoCommit => args.push("--no-commit".to_string()),
            MergeSquash => args.push("--squash".to_string()),
            MergePlain => {}
            _ => return,
        }
        self.open_listed_picker(
            PickerAction::Merge,
            CreateMode::Value,
            args,
            targets::all_branches,
            window,
            cx,
        );
    }

    /// Run the merge on the background executor, then refresh — a conflict pauses
    /// it, which the in-progress banner then surfaces.
    pub(crate) fn run_merge(&mut self, target: String, args: Vec<String>, cx: &mut Context<Self>) {
        self.run_job(
            "Merging…",
            "Merged",
            move |repo| repo.merge(&target, &args),
            cx,
        );
    }

    /// The repo-relative path of the file at the cursor (file/hunk/line rows),
    /// used to seed prompts like ignore. `None` on a section header.
    pub(crate) fn current_file_path(&self) -> Option<String> {
        let row = self.rows.get(self.selected)?;
        row.target.as_ref().map(|t| target_path(t).to_string())
    }

    /// Ignore transient suffix: open a free-text prompt for the pattern (seeded
    /// with the file at point) for the chosen destination, then add it.
    pub(crate) fn dispatch_ignore(
        &mut self,
        command: transient::Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        let file = self.current_file_path();
        let dest = match command {
            IgnoreToplevel => IgnoreDest::Toplevel,
            // A subdir .gitignore matches relative to itself, so the rule below
            // defaults to the basename and the dir is the file's own directory.
            IgnoreSubdir => IgnoreDest::Subdir(
                file.as_deref()
                    .map(Path::new)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            ),
            IgnorePrivate => IgnoreDest::Private,
            IgnoreGlobal => IgnoreDest::Global,
            _ => return,
        };
        let default = default_ignore_pattern(command, file.as_deref());
        self.open_picker(
            PickerAction::Ignore(dest),
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
        // Seed the prompt with the default pattern.
        self.seed_picker_input(&default, window, cx);
    }

    pub(crate) fn dispatch_diff(
        &mut self,
        command: transient::Command,
        args: Vec<String>,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        match command {
            DiffDwim => self.diff_dwim(args, paths, window, cx),
            DiffRange => self.open_picker(
                PickerAction::DiffRange { args, paths },
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            ),
            DiffUnstaged => self.open_diff(DiffRequest::Unstaged { args, paths }, cx),
            DiffStaged => self.open_diff(DiffRequest::Staged { args, paths }, cx),
            DiffWorktree => self.open_diff(
                DiffRequest::Worktree {
                    rev: "HEAD".to_string(),
                    args,
                    paths,
                },
                cx,
            ),
            DiffCommit => self.open_listed_picker(
                PickerAction::DiffCommit { args, paths },
                CreateMode::Any,
                Vec::new(),
                |repo| {
                    Ok(repo
                        .log("HEAD", Self::LOG_LIMIT)?
                        .into_iter()
                        .map(|e| format!("{} {}", e.short_hash, e.subject))
                        .collect())
                },
                window,
                cx,
            ),
            _ => {}
        }
    }

    fn diff_dwim(
        &mut self,
        args: Vec<String>,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((hash, _, subject)) = self.point_commit() {
            return self.open_commit_with_args(hash, subject, args, paths, cx);
        }
        if let Some(source) = self.diff_source_at_point() {
            let request = match source {
                DiffSource::Unstaged => DiffRequest::Unstaged { args, paths },
                DiffSource::Staged => DiffRequest::Staged { args, paths },
            };
            return self.open_diff(request, cx);
        }
        if self
            .status
            .as_ref()
            .is_some_and(|s| s.unstaged().next().is_some())
        {
            self.open_diff(DiffRequest::Unstaged { args, paths }, cx);
        } else if self
            .status
            .as_ref()
            .is_some_and(|s| s.staged().next().is_some())
        {
            self.open_diff(DiffRequest::Staged { args, paths }, cx);
        } else {
            self.open_picker(
                PickerAction::DiffRange { args, paths },
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            );
        }
    }

    fn diff_source_at_point(&self) -> Option<DiffSource> {
        let row = self.rows.get(self.selected)?;
        if let Some(FoldKey::Section(section)) = &row.fold {
            if let Some(source) = section_source(*section) {
                return Some(source);
            }
        }
        match row.target.as_ref()? {
            Target::File(f) => section_source(f.section),
            Target::Hunk { file, .. } | Target::Line { file, .. } => section_source(file.section),
        }
    }

    /// Append the chosen pattern to the gitignore file for `dest` (off the UI
    /// thread), then refresh so a newly-ignored untracked file leaves the list.
    pub(crate) fn run_ignore(&mut self, dest: IgnoreDest, rule: String, cx: &mut Context<Self>) {
        self.run_job(
            "Ignoring…",
            "Ignored",
            move |repo| repo.add_ignore_rule(&rule, dest).map(|()| String::new()),
            cx,
        );
    }

    /// Rebase transient suffix: resolve the target to rebase onto (the upstream
    /// or push-remote when known, else prompt), then rebase.
    pub(crate) fn dispatch_rebase(
        &mut self,
        command: transient::Command,
        args: Vec<String>,
        targets: &RemoteTargets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        if matches!(command, RebaseInteractive | RebaseRewordCommit) {
            if self.selected_commit_hash().is_some() {
                if matches!(command, RebaseRewordCommit) {
                    self.reword_past_selected(args, window, cx);
                } else {
                    self.rebase_since_selected(args, cx);
                }
                return;
            }
            // magit's model: pick the commit to rebase *since* from the log
            // (not a free-text base) — that commit and everything above it
            // become the editable todo.
            if matches!(command, RebaseRewordCommit) {
                self.start_log_select_rebase_reword(args, cx);
            } else {
                self.start_log_select_rebase(args, cx);
            }
            return;
        }
        let onto = match command {
            RebaseOntoUpstream => targets.upstream.as_ref().map(|u| u.display()),
            RebaseOntoPushRemote => match (&targets.branch, &targets.push_remote) {
                (Some(b), Some(r)) => Some(format!("{r}/{b}")),
                _ => None,
            },
            RebaseElsewhere => None,
            _ => return,
        };
        match onto {
            Some(onto) => self.run_rebase(onto, args, cx),
            // Unknown target (or "elsewhere") — pick a branch/ref to rebase onto.
            None => self.open_listed_picker(
                PickerAction::Rebase,
                CreateMode::Value,
                args,
                targets::all_branches,
                window,
                cx,
            ),
        }
    }

    /// Cherry-pick/revert transient suffix: act on the commit at point (status
    /// commit row or log selection), matching Magit's commit-at-point default.
    pub(crate) fn dispatch_pick(
        &mut self,
        command: transient::Command,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use transient::Command::*;
        let op = match command {
            CherryPick => PickOp::CherryPick,
            CherryPickRange => {
                return self.open_picker(
                    PickerAction::PickRange(PickOp::CherryPick),
                    Vec::new(),
                    CreateMode::Value,
                    args,
                    window,
                    cx,
                );
            }
            CherryApply => PickOp::CherryApply,
            RevertCommit => PickOp::Revert,
            RevertRange => {
                return self.open_picker(
                    PickerAction::PickRange(PickOp::Revert),
                    Vec::new(),
                    CreateMode::Value,
                    args,
                    window,
                    cx,
                );
            }
            RevertNoCommit => PickOp::RevertNoCommit,
            _ => return,
        };
        self.pick_selected_with_args(op, args, window, cx);
    }

    /// Open the free-text command prompt (magit's `!` family). The git variant
    /// is prefilled with `git ` — run a subcommand by default, or delete the
    /// prefix to run any program; the shell variant runs the raw line via
    /// `sh -c` (pipes, `&&`). `dir` scopes it to a worktree subdirectory
    /// (magit's in-working-directory variants); `None` runs at the root.
    pub(crate) fn open_run_prompt(
        &mut self,
        shell: bool,
        dir: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(
            PickerAction::Run { shell, dir },
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
        if !shell {
            // Prefill with the `git ` prefix; delete it to run any command.
            self.seed_picker_input("git ", window, cx);
        }
    }

    /// The directory of the file at point, worktree-relative — what the run
    /// transient's "in working directory" variants mean in a GUI (magit runs in
    /// the buffer's directory). Root when the cursor isn't on a file.
    pub(crate) fn dir_at_point(&self) -> Option<String> {
        let path = self.path_at_point()?;
        let parent = std::path::Path::new(&path).parent()?;
        (!parent.as_os_str().is_empty()).then(|| parent.to_string_lossy().into_owned())
    }

    /// Run a user-typed command from the `!` prompt on the background executor,
    /// then refresh. The prompt is prefilled with `git `, so the first word
    /// selects the program: `git` (the default) runs a git subcommand via the
    /// repo wrapper, anything else runs that program directly in the working
    /// tree (delete the `git ` prefix to do so). Split with POSIX quoting (no
    /// shell) so quoted args like `-m "two words"` stay one argv entry. The full
    /// output is recorded in the `$` log; a multi-line result opens it,
    /// otherwise the first line shows as a notice.
    pub(crate) fn run_user_command(
        &mut self,
        input: String,
        dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut parts = match shell_words::split(input.trim()) {
            Ok(p) => p,
            Err(e) => return self.set_status(format!("parse error: {e}"), false, cx),
        };
        if parts.is_empty() {
            return;
        }
        // The first word is the program; `git` routes through the repo wrapper
        // (`None`), anything else runs directly.
        let program = if parts[0] == "git" {
            parts.remove(0);
            None
        } else {
            Some(parts.remove(0))
        };
        let rest = parts;
        if program.is_none() && rest.is_empty() {
            return;
        }
        let progress = match &program {
            Some(p) => format!("{p} {}…", rest.join(" ")),
            None => format!("git {}…", rest.join(" ")),
        };
        let dir = std::path::PathBuf::from(dir.unwrap_or_default());
        self.run_command_job(
            progress,
            true,
            move |repo| repo.run_user_in(program.as_deref(), &rest, &dir),
            cx,
        );
    }

    /// Run a raw shell line from the run transient's shell variants (`sh -c`,
    /// so pipes and `&&` work), in `dir` or the repository root.
    pub(crate) fn run_shell_prompt_command(
        &mut self,
        input: String,
        dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let dir = std::path::PathBuf::from(dir.unwrap_or_default());
        self.run_command_job(
            format!("{input}…"),
            true,
            move |repo| repo.run_shell_in(&input, &dir),
            cx,
        );
    }

    /// Open a free-text value prompt (a patch file/range, a bisect revision, …),
    /// seeded with `seed` (empty leaves it blank).
    pub(crate) fn open_value_prompt(
        &mut self,
        action: PickerAction,
        seed: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(
            action,
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
        self.seed_picker_input(seed, window, cx);
    }

    /// Apply a typed patch file to the worktree (`git apply`).
    pub(crate) fn run_patch_apply(&mut self, path: String, cx: &mut Context<Self>) {
        self.run_job(
            "Applying patch…",
            "Applied patch",
            move |repo| repo.apply_patch_file(path.trim()),
            cx,
        );
    }

    /// Apply a typed mailbox as commits (`git am`); a conflict pauses into the
    /// am sequence banner.
    pub(crate) fn run_patch_am(&mut self, path: String, cx: &mut Context<Self>) {
        self.run_job(
            "Applying patches…",
            "Applied patches",
            move |repo| repo.am_patch(path.trim()),
            cx,
        );
    }

    /// Create patch files for a typed range (`git format-patch`). Shell-style
    /// splitting so a quoted argument (`-o "some dir"`) survives.
    pub(crate) fn run_patch_create(&mut self, args: String, cx: &mut Context<Self>) {
        let args = shell_words::split(args.trim())
            .unwrap_or_else(|_| args.split_whitespace().map(str::to_string).collect());
        self.run_job(
            "Creating patch…",
            "Created",
            move |repo| repo.format_patch(&args),
            cx,
        );
    }

    /// Run the rebase on the background executor, then refresh — a conflict
    /// pauses it, which the in-progress banner then drives.
    pub(crate) fn run_rebase(&mut self, onto: String, args: Vec<String>, cx: &mut Context<Self>) {
        self.run_job(
            "Rebasing…",
            "Rebased",
            move |repo| repo.rebase(&onto, &args),
            cx,
        );
    }

    /// Run the reset on the background executor, then refresh.
    pub(crate) fn do_reset(&mut self, mode: ResetMode, target: String, cx: &mut Context<Self>) {
        self.run_job(
            "Resetting…",
            "Reset",
            move |repo| repo.reset(mode, &target),
            cx,
        );
    }

    /// Begin an amend/reword/extend, first checking (off the UI thread) whether
    /// HEAD has already been pushed; if so, confirm before rewriting published
    /// history (mirrors magit's `magit-commit-amend-assert`).
    pub(crate) fn begin_history_rewrite(
        &mut self,
        command: transient::Command,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let branches = self.config.published_branches.clone();
        cx.spawn_in(window, async move |this, cx| {
            let published = cx
                .background_executor()
                .spawn(async move { repo.published_on("HEAD", &branches) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // The probe raced user input: if another screen, popup, or
                // confirmation took over meanwhile, drop the result rather
                // than popping an editor/prompt over it.
                if !this.ui_idle_for_prompt() {
                    return;
                }
                let Some(target) = published else {
                    this.proceed_history_rewrite(command, switches, window, cx);
                    return;
                };
                let verb = match command {
                    transient::Command::CommitReword => "Reword",
                    transient::Command::CommitExtend => "Extend",
                    _ => "Amend",
                };
                this.confirm = Some((
                    format!("This commit has already been pushed to {target}. {verb} it anyway?"),
                    Confirm::AmendPushed(command, switches),
                ));
                cx.notify();
            });
        })
        .detach();
    }

    /// Carry out an amend/reword/extend (after any published-history warning):
    /// amend/reword open the message editor; extend commits straight away.
    pub(crate) fn proceed_history_rewrite(
        &mut self,
        command: transient::Command,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            transient::Command::CommitAmend => {
                self.open_editor(CommitMode::Amend, switches, window, cx)
            }
            transient::Command::CommitReword => {
                self.open_editor(CommitMode::Reword, switches, window, cx)
            }
            // Extend is the one rewrite that runs straight away (no editor).
            _ => self.run_job(
                "Committing…",
                "Committed",
                move |repo| repo.commit_extend(&switches),
                cx,
            ),
        }
    }

    /// Mark the checked-out bisect commit good/bad/skip and let git advance. The
    /// "Bisecting: N revisions left" line git prints is surfaced as the toast.
    pub(crate) fn run_bisect_mark(&mut self, mark: BisectMark, cx: &mut Context<Self>) {
        let (verb, done) = match mark {
            BisectMark::Good => ("Marking good", "good"),
            BisectMark::Bad => ("Marking bad", "bad"),
            BisectMark::Skip => ("Skipping", "skipped"),
        };
        self.run_job(
            &format!("{verb}…"),
            done,
            move |repo| repo.bisect_mark(mark),
            cx,
        );
    }

    /// End the bisect session, restoring the original branch.
    pub(crate) fn run_bisect_reset(&mut self, cx: &mut Context<Self>) {
        self.run_job(
            "Ending bisect…",
            "Bisect ended",
            |repo| repo.bisect_reset(),
            cx,
        );
    }

    /// Start a bisect between the entered bad and good revisions; git checks out
    /// the midpoint (its banner then drives good/bad/skip).
    pub(crate) fn run_bisect_start(&mut self, bad: String, good: String, cx: &mut Context<Self>) {
        self.run_job(
            "Starting bisect…",
            "Bisecting",
            move |repo| repo.bisect_start(bad.trim(), good.trim()),
            cx,
        );
    }

    // Fixed-signature wrappers so the bisect banner's clickable buttons can share
    // the `seq_action` shape (`fn(&mut Self, &mut Window, &mut Context)`).
    pub(crate) fn bisect_good_action(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.run_bisect_mark(BisectMark::Good, cx);
    }
    pub(crate) fn bisect_bad_action(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.run_bisect_mark(BisectMark::Bad, cx);
    }
    pub(crate) fn bisect_skip_action(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.run_bisect_mark(BisectMark::Skip, cx);
    }
    pub(crate) fn bisect_reset_action(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        self.run_bisect_reset(cx);
    }

    /// Open the vertico-style picker for a pending action. The query input is
    /// focused on appear, so it's type-to-filter immediately; the model re-ranks
    /// on every change.
    pub(crate) fn open_picker(
        &mut self,
        action: PickerAction,
        choices: Vec<String>,
        create: CreateMode,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker_searchable(action, choices, None, create, switches, window, cx);
    }

    /// A pure-entry picker (no candidate list, `CreateMode::Any`) pre-filled with
    /// `seed` — used by the release flow to propose a tag name the user can edit.
    pub(crate) fn open_picker_seeded(
        &mut self,
        action: PickerAction,
        seed: String,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(action, Vec::new(), CreateMode::Any, switches, window, cx);
        self.seed_picker_input(&seed, window, cx);
    }

    /// Seed the just-opened picker's prompt with `seed` (empty leaves it
    /// blank): set both the picker's query (what confirm reads) and the
    /// visible input (`set_value` emits no Change, so the query must be set by
    /// hand). The triggering key's focus is deferred a frame (see
    /// [`Self::open_picker`]), so the prefill isn't clobbered by that keystroke.
    fn seed_picker_input(&mut self, seed: &str, window: &mut Window, cx: &mut Context<Self>) {
        if seed.is_empty() {
            return;
        }
        let input = if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.list.set_query(seed);
            Some(p.input.clone())
        } else {
            None
        };
        if let Some(input) = input {
            let seed = seed.to_string();
            input.update(cx, |s, cx| s.set_value(seed, window, cx));
        }
    }

    /// [`Self::open_picker`], but each choice is matched against a parallel
    /// `search` string (title + hidden aliases) rather than its display text —
    /// so the palette can surface "Copy" when you type "yank". `search`, when
    /// given, must line up 1:1 with `choices`.
    pub(crate) fn open_picker_searchable(
        &mut self,
        action: PickerAction,
        choices: Vec<String>,
        search: Option<Vec<String>>,
        create: CreateMode,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = action.prompt();
        let items: Vec<SharedString> = choices.into_iter().map(SharedString::from).collect();
        // Reserve the candidate area only when there's actually a list to match
        // against. A picker with no choices (e.g. creating a branch — you type a
        // new name) is pure entry: no candidate area, no "No match". The async
        // completion prompts start empty but opt back in via `open_option_prompt`.
        let has_candidates = !items.is_empty();
        let input = cx.new(|cx| InputState::new(window, cx));
        // Re-filter as the query changes (Up/Down/Enter/Esc are handled in the
        // capture phase, so the input only ever sees text edits here).
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                        p.list.set_query(&query);
                        p.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                        cx.notify();
                    }
                }
            },
        );
        // Focus on the next frame, not now. A picker is usually opened by a
        // keystroke (e.g. `p` in the pull transient) that is still mid-dispatch;
        // focusing the input synchronously lets that same character land in it
        // (the macOS text-input phase runs after this handler). Next frame, the
        // triggering key's character delivery is done, so the input starts empty.
        let to_focus = input.clone();
        cx.on_next_frame(window, move |_this, window, cx| {
            to_focus.read(cx).focus_handle(cx).focus(window, cx);
        });
        self.popup = Some(Popup::Picker(PickerState {
            prompt,
            input,
            list: match search {
                Some(search) => PickerList::with_search(items, search, create),
                None => PickerList::new(items, create),
            },
            scroll: UniformListScrollHandle::new(),
            action,
            switches,
            loading: false,
            gen: 0,
            reserve_candidates: has_candidates,
            resume: None,
            _sub: sub,
            hints: Default::default(),
        }));
        cx.notify();
    }

    /// Open a picker whose candidates come from git, without blocking the UI
    /// thread on the listing: show the picker immediately (with a "Loading…"
    /// line), run `list` on the background executor, then fill the candidates.
    /// For a selection-only picker (`CreateMode::None`) an empty result closes
    /// it with the action's empty-message and a listing error closes it with the
    /// error (rather than silently showing "nothing"); a value-entry picker stays
    /// open either way, since you can still type a target. Ref/branch listings
    /// scale with a repo's ref count, so this keeps opening instant in large repos.
    pub(crate) fn open_listed_picker(
        &mut self,
        action: PickerAction,
        create: CreateMode,
        switches: Vec<String>,
        list: impl FnOnce(&Repo) -> magritte_core::Result<Vec<String>> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let selection_only = matches!(create, CreateMode::None);
        let empty_message = action.empty_message();
        let gen = self.picker_gen.bump();
        self.open_picker(action, Vec::new(), create, switches, window, cx);
        if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.loading = true;
            p.gen = gen;
            // Reserve the candidate area so the Loading line, then the rows,
            // don't shift the panel as they arrive.
            p.reserve_candidates = true;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { list(&repo) })
                .await;
            this.update(cx, |this, cx| {
                // Ignore the load if this picker was dismissed or superseded.
                if !matches!(&this.popup, Some(Popup::Picker(p)) if p.gen == gen) {
                    return;
                }
                match result {
                    Ok(choices) if choices.is_empty() && selection_only => {
                        this.popup = None;
                        this.set_status(empty_message.to_string(), true, cx);
                    }
                    Ok(choices) => {
                        if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                            p.loading = false;
                            p.list
                                .set_choices(choices.into_iter().map(SharedString::from).collect());
                        }
                        cx.notify();
                    }
                    Err(e) => {
                        if selection_only {
                            this.popup = None;
                        } else if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                            p.loading = false; // keep open for free-text entry
                        }
                        this.set_status(format!("error: {e}"), false, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run the pending action against the candidate currently highlighted in the
    /// picker (Enter, a row click, or the kbd button).
    pub(crate) fn confirm_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = match &self.popup {
            Some(Popup::Picker(p)) => p.list.selected_choice(),
            _ => None,
        };
        let Some(chosen) = chosen else { return };
        if let Some(Popup::Picker(p)) = self.popup.take() {
            match p.action {
                PickerAction::Transfer(t) => {
                    self.run_transfer(t, chosen.to_string(), p.switches, cx)
                }
                PickerAction::Branch(b) => {
                    self.run_branch_action(b, chosen.to_string(), window, cx)
                }
                PickerAction::Tag(t) => {
                    self.run_tag_action(t, chosen.to_string(), p.switches, window, cx)
                }
                PickerAction::Remote(r) => {
                    self.run_remote_action(r, chosen.to_string(), p.switches, window, cx)
                }
                PickerAction::Stash(s) => self.run_stash_action(s, chosen.to_string(), cx),
                PickerAction::Reset(mode) => self.run_reset(mode, chosen.to_string(), cx),
                PickerAction::Merge => self.run_merge(chosen.to_string(), p.switches, cx),
                PickerAction::Rebase => self.run_rebase(chosen.to_string(), p.switches, cx),
                PickerAction::PickRange(op) => {
                    self.pick_rev_with_args(op, chosen.to_string(), p.switches, window, cx)
                }
                PickerAction::Run { shell: false, dir } => {
                    self.run_user_command(chosen.to_string(), dir, cx)
                }
                PickerAction::Run { shell: true, dir } => {
                    self.run_shell_prompt_command(chosen.to_string(), dir, cx)
                }
                PickerAction::PatchApply => self.run_patch_apply(chosen.to_string(), cx),
                PickerAction::PatchAm => self.run_patch_am(chosen.to_string(), cx),
                PickerAction::PatchCreate => self.run_patch_create(chosen.to_string(), cx),
                // Bisect start: bad rev captured, now prompt for the good rev;
                // with both, run `git bisect start`.
                PickerAction::BisectBadRev => self.open_value_prompt(
                    PickerAction::BisectGoodRev {
                        bad: chosen.to_string(),
                    },
                    "",
                    window,
                    cx,
                ),
                PickerAction::BisectGoodRev { bad } => {
                    self.run_bisect_start(bad, chosen.to_string(), cx)
                }
                PickerAction::Ignore(dest) => self.run_ignore(dest, chosen.to_string(), cx),
                PickerAction::StashMessage { include_untracked } => {
                    self.run_stash_push(include_untracked, chosen.to_string(), cx)
                }
                // Worktree create/move: step one captures the ref/branch and
                // opens the directory prompt; step two runs the git command.
                PickerAction::WorktreeAddRef => {
                    self.prompt_worktree_dir(
                        PickerAction::WorktreeAddDir {
                            commit: chosen.to_string(),
                        },
                        &chosen,
                        window,
                        cx,
                    );
                }
                PickerAction::WorktreeBranchName => {
                    self.prompt_worktree_dir(
                        PickerAction::WorktreeBranchDir {
                            branch: chosen.to_string(),
                        },
                        &chosen,
                        window,
                        cx,
                    );
                }
                PickerAction::WorktreeAddDir { commit } => {
                    self.do_add_worktree(chosen.to_string(), commit, cx)
                }
                PickerAction::WorktreeBranchDir { branch } => {
                    self.do_add_branch_worktree(chosen.to_string(), branch, cx)
                }
                PickerAction::WorktreeMoveTo { from } => {
                    self.do_move_worktree(from, chosen.to_string(), cx)
                }
                PickerAction::RefsRename { old } => {
                    self.do_refs_rename(old, chosen.to_string(), cx)
                }
                // Set the option value (empty clears it) and reopen the transient.
                PickerAction::SetOption { key, .. } => {
                    if let Some(mut ts) = p.resume {
                        let value = chosen.to_string();
                        if value.trim().is_empty() {
                            ts.values.remove(&key);
                        } else {
                            ts.values.insert(key, value);
                        }
                        self.popup = Some(Popup::Transient(*ts));
                        cx.notify();
                    }
                }
                // Write the git-config variable (empty unsets it), then reopen the
                // Configure transient with the new value reflected.
                PickerAction::SetVariable { variable, .. } => {
                    if let Some(ts) = p.resume {
                        let value = chosen.to_string();
                        let value = (!value.trim().is_empty()).then_some(value);
                        let key = ts
                            .def
                            .variables_ref()
                            .find(|v| v.variable == variable)
                            .map(|v| v.key.clone());
                        self.popup = Some(Popup::Transient(*ts));
                        if let Some(key) = key {
                            self.write_variable(&key, &variable, value, cx);
                        }
                    }
                }
                PickerAction::LogRef {
                    flags,
                    paths,
                    limit,
                } => {
                    let args =
                        build_log_args(flags, LogScope::Ref(chosen.to_string()), paths, limit);
                    self.start_log(args, cx);
                }
                PickerAction::DiffRange { args, paths } => {
                    self.open_diff(
                        DiffRequest::Range {
                            range: chosen.to_string(),
                            args,
                            paths,
                        },
                        cx,
                    );
                }
                PickerAction::DiffCommit { args, paths } => {
                    let rev = chosen
                        .split_whitespace()
                        .next()
                        .unwrap_or(&chosen)
                        .to_string();
                    self.open_commit_with_args(rev, String::new(), args, paths, cx);
                }
                // Resolve the chosen title back to its command (built-in or a
                // user `[[command]]`) and run it through the shared dispatch.
                PickerAction::RunCommand => {
                    let id = all_commands(&self.config)
                        .find(|c| c.title == chosen.as_ref())
                        .map(|c| c.id.to_string());
                    if let Some(id) = id {
                        self.record_use(&id);
                        self.invoke_command(&id, window, cx);
                    }
                }
            }
        }
    }

    /// Move the picker highlight by `delta` rows (Up/Down), keeping it in view.
    pub(crate) fn picker_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.list.move_by(delta);
            p.scroll
                .scroll_to_item(p.list.selected(), gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// The screen-aware "Copy" (evil `yy`/`ys`, magit's `C-w`, and `Cmd-C`):
    /// copy the value at point for the active view — the selected diff text in a
    /// commit/diff buffer, the commit hash in the log, the ref at point in the
    /// refs browser, else the status selection (a commit/stash value or the row
    /// text). We don't split whole-line from section-value the way a text buffer
    /// does; our copy already yields the useful value at point.
    pub(crate) fn copy_at_point(&mut self, cx: &mut Context<Self>) {
        if self.commit_view().is_some() || self.diff_view().is_some() {
            self.copy_flat_diff_selection(cx);
        } else if self.log().is_some() {
            self.copy_log_commit(cx);
        } else if self.char_sel.is_some_and(|c| !c.is_empty()) {
            // A mouse char selection wins over the row's commit/stash value.
            self.copy_selection(cx);
        } else if let Some(name) = self
            .refs_view()
            .and_then(RefsView::selected_row)
            .and_then(RefsRow::ref_name)
            .map(str::to_string)
        {
            self.copy_to_clipboard(name, cx);
        } else if let Some((hash, ..)) = self.point_commit() {
            // The full hash of a status commit row (magit's `C-w`/`y y`).
            self.copy_to_clipboard(hash, cx);
        } else if let Some((reference, _)) = self.point_stash() {
            self.copy_to_clipboard(reference, cx);
        } else {
            self.copy_selection(cx);
        }
    }

    /// Copy the revision the current view represents (evil `yb`, magit's
    /// `magit-copy-buffer-revision`): the shown commit in a commit buffer, else
    /// the checked-out HEAD.
    pub(crate) fn copy_buffer_revision(&mut self, cx: &mut Context<Self>) {
        let rev = match &self.screen {
            Screen::Commit { view, .. } => Some(view.rev.clone()),
            _ => self.status.as_ref().and_then(|s| s.head.oid.clone()),
        };
        match rev {
            Some(rev) => self.copy_to_clipboard(rev, cx),
            None => self.set_status("No revision to copy".to_string(), true, cx),
        }
    }

    /// Carry out a branch-transient action against the chosen branch/name.
    /// Rename is two-step: step 1 (`RenameFrom`) opens the name prompt rather
    /// than running git.
    pub(crate) fn run_branch_action(
        &mut self,
        action: BranchAction,
        chosen: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Step 1 of rename: the chosen branch is the one to rename — now prompt
        // for the new name (free text).
        if let BranchAction::RenameFrom = action {
            self.open_picker(
                PickerAction::Branch(BranchAction::RenameTo { old: chosen }),
                Vec::new(),
                CreateMode::Any,
                Vec::new(),
                window,
                cx,
            );
            return;
        }
        // Configure opens the chosen branch's config transient, not a git job.
        if let BranchAction::Configure = action {
            self.open_branch_configure_for(chosen, cx);
            return;
        }

        let (verb, done) = match &action {
            BranchAction::Checkout => ("Checking out", "Checked out"),
            BranchAction::Create { .. } => ("Creating branch", "Created branch"),
            BranchAction::RenameTo { .. } => ("Renaming branch", "Renamed branch"),
            BranchAction::Delete => ("Deleting branch", "Deleted branch"),
            BranchAction::RenameFrom | BranchAction::Configure => unreachable!("handled above"),
        };
        self.run_job(
            &format!("{verb}…"),
            done,
            move |repo| match action {
                BranchAction::Checkout => repo.checkout(&chosen),
                BranchAction::Create { checkout: true } => repo.create_and_checkout(&chosen, None),
                BranchAction::Create { checkout: false } => repo.create_branch(&chosen, None),
                BranchAction::RenameTo { old } => repo.rename_branch(&old, &chosen),
                BranchAction::Delete => repo.delete_branch(&chosen, false),
                BranchAction::RenameFrom | BranchAction::Configure => {
                    unreachable!("handled above")
                }
            },
            cx,
        );
    }

    /// Carry out a tag-transient action. Tag creation targets the commit at
    /// point when one exists, otherwise HEAD.
    pub(crate) fn run_tag_action(
        &mut self,
        action: TagAction,
        chosen: String,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if chosen.trim().is_empty() {
            self.set_status("Tag name required".to_string(), false, cx);
            return;
        }
        let force = switches.iter().any(|s| s == "--force");
        let target = self
            .selected_commit_hash()
            .unwrap_or_else(|| "HEAD".to_string());
        match action {
            // An annotated tag carries a message, so open the editor to write it
            // (the tag is created on submit); the rest run straight away.
            TagAction::Create { annotated: true } => {
                self.start_annotated_tag(chosen, target, force, window, cx)
            }
            TagAction::Create { annotated: false } => self.run_job(
                "Tagging…",
                "Tagged",
                move |repo| repo.create_tag(&chosen, &target, force),
                cx,
            ),
            // A release: an annotated tag opens the editor pre-filled with the
            // proposed message (reusing the previous release's, version-swapped)
            // for review; a lightweight release just creates the tag.
            TagAction::Release { annotated: true } => {
                let Some(repo) = self.repo.clone() else {
                    return;
                };
                let tag = chosen.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let message = cx
                        .background_executor()
                        .spawn(async move { repo.release_message(&tag).unwrap_or_default() })
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        if !this.ui_idle_for_prompt() {
                            return;
                        }
                        this.start_release_tag(chosen, target, force, message, window, cx);
                    })
                    .ok();
                })
                .detach();
            }
            TagAction::Release { annotated: false } => self.run_job(
                "Tagging…",
                "Tagged",
                move |repo| repo.create_tag(&chosen, &target, force),
                cx,
            ),
            TagAction::Delete => self.run_job(
                "Deleting tag…",
                "Deleted tag",
                move |repo| repo.delete_tag(&chosen),
                cx,
            ),
        }
    }

    /// Carry out a remote-transient action. Add/rename are two-step: the first
    /// value opens the next prompt, the second runs git.
    pub(crate) fn run_remote_action(
        &mut self,
        action: RemoteAction,
        chosen: String,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if chosen.trim().is_empty() {
            self.set_status("Remote value required".to_string(), false, cx);
            return;
        }
        match action {
            RemoteAction::AddName => {
                self.open_picker(
                    PickerAction::Remote(RemoteAction::AddUrl {
                        name: chosen,
                        args: switches,
                    }),
                    Vec::new(),
                    CreateMode::Value,
                    Vec::new(),
                    window,
                    cx,
                );
            }
            RemoteAction::RenameFrom => {
                self.open_picker(
                    PickerAction::Remote(RemoteAction::RenameTo { old: chosen }),
                    Vec::new(),
                    CreateMode::Any,
                    Vec::new(),
                    window,
                    cx,
                );
            }
            RemoteAction::AddUrl { name, args } => self.run_job(
                "Adding remote…",
                "Added remote",
                move |repo| repo.add_remote(&name, &chosen, &args),
                cx,
            ),
            RemoteAction::RenameTo { old } => self.run_job(
                "Renaming remote…",
                "Renamed remote",
                move |repo| repo.rename_remote(&old, &chosen),
                cx,
            ),
            RemoteAction::Remove => self.run_job(
                "Removing remote…",
                "Removed remote",
                move |repo| repo.remove_remote(&chosen),
                cx,
            ),
            RemoteAction::Configure => self.open_remote_configure_for(chosen, cx),
        }
    }

    /// Prompt for an optional stash message (magit prompts too; empty keeps
    /// git's default "WIP on …"), then stash (`Z z` / `Z Z`).
    pub(crate) fn prompt_stash_message(
        &mut self,
        include_untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(
            PickerAction::StashMessage { include_untracked },
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
    }

    pub(crate) fn run_stash_push(
        &mut self,
        include_untracked: bool,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.run_job(
            "Stashing…",
            "Stashed",
            move |repo| repo.stash_push(Some(&message), include_untracked),
            cx,
        );
    }

    /// Apply / pop / drop the chosen stash (`chosen` is the picker's display
    /// string; the `stash@{N}` reference is its first token).
    pub(crate) fn run_stash_action(
        &mut self,
        action: StashAction,
        chosen: String,
        cx: &mut Context<Self>,
    ) {
        let reference = chosen
            .split_whitespace()
            .next()
            .unwrap_or(&chosen)
            .to_string();
        let (verb, done) = match action {
            StashAction::Apply => ("Applying stash", "Applied stash"),
            StashAction::Pop => ("Popping stash", "Popped stash"),
            StashAction::Drop => ("Dropping stash", "Dropped stash"),
        };
        let is_drop = matches!(action, StashAction::Drop);
        self.run_job_with(
            format!("{verb}…"),
            move |repo| match action {
                StashAction::Apply => repo.stash_apply(&reference),
                StashAction::Pop => repo.stash_pop(&reference),
                StashAction::Drop => repo.stash_drop(&reference),
            },
            // A dropped stash isn't gone: git echoes the commit it pointed at
            // ("Dropped refs/stash@{0} (<sha>)"), recoverable via `git stash
            // store`. Surface that line so the id is visible — like magit, which
            // echoes it too and (for a single drop) doesn't prompt; the explicit
            // pick is the safeguard.
            move |this, result, cx| match (is_drop, &result) {
                (true, Ok(line)) if !line.is_empty() => this.set_status(line.clone(), true, cx),
                _ => this.report(done, result, cx),
            },
            cx,
        );
    }
}

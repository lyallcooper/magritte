//! The controller layer: command dispatch (the registry `run` closures and the
//! transient `fire_action`), the git-execution `run_*` jobs, picker
//! orchestration, and the status/report/job plumbing they share. Split out of
//! the main view file so command handling reads as one concern. It stays
//! `impl StatusView` because a GPUI view owns its state and behavior together;
//! a separate non-view controller would mean message-passing ceremony for no
//! gain in a single-Entity app (see the FB5 disposition in FEEDBACK.md).

#![allow(clippy::too_many_arguments)]

use gpui::prelude::*;
use gpui::{Context, SharedString, UniformListScrollHandle, Window};

use crate::*;

/// The bottom-bar status toast — one logical value whose parts move together:
/// the message, an optional emphasized copied value (rendered only while the
/// message is the Copied label), optional leading keycaps, and the sequence
/// stamp that keeps a stale fade timer from clearing a newer message.
#[derive(Default)]
pub(crate) struct StatusToast {
    /// Last operation result / progress, shown in the bottom bar.
    pub(crate) message: Option<String>,
    /// For a copy confirmation, the copied value. Set by `copy_to_clipboard`;
    /// shown only when the message is exactly the Copied label, so writes that
    /// don't clear it can't accidentally trail a stale value.
    pub(crate) copied: Option<SharedString>,
    /// A keystroke to render as keycap(s) before the message (e.g. the unbound
    /// `g x` in "g x is unbound"). Cleared by every status post; set right
    /// after by the few messages that lead with a key.
    pub(crate) keys: Option<String>,
    /// Bumped each time the message changes, so an auto-dismiss timer only
    /// clears the message it was scheduled for (not a newer one).
    pub(crate) seq: Generation,
}

/// How a status-bar message behaves once shown. Every kind advances the status
/// sequence; only a `Notice` schedules its own fade.
pub(crate) enum StatusKind {
    /// A success notice — fades on its own after a moment.
    Notice,
    /// Work in progress ("Pushing…") — stays until the job reports.
    Progress,
    /// An error or condition — stays until dismissed (Esc / click).
    Sticky,
}

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
            // Push/pull/fetch resolve a remote (prompting if needed) then run.
            PushPushRemote | PushUpstream | PushElsewhere | PullPushRemote | PullUpstream
            | PullElsewhere | FetchPushRemote | FetchUpstream | FetchAll | FetchElsewhere => {
                self.dispatch_transfer(command, &targets, args, window, cx)
            }
            BranchCheckout | BranchCreateCheckout | BranchCreate | BranchRename | BranchDelete => {
                self.dispatch_branch(command, window, cx)
            }
            TagCreate | TagDelete => self.dispatch_tag(command, args, window, cx),
            RemoteAdd | RemoteRename | RemoteRemove => self.dispatch_remote(command, args, window, cx),
            ResetSoft | ResetMixed | ResetHard | ResetKeep | ResetIndex | ResetWorktree => {
                self.dispatch_reset(command, window, cx)
            }
            MergePlain | MergeNoCommit | MergeSquash => {
                self.dispatch_merge(command, args, window, cx)
            }
            CherryPick | CherryPickRange | CherryApply | RevertCommit | RevertRange
            | RevertNoCommit => {
                self.dispatch_pick(command, args, window, cx)
            }
            RebaseOntoUpstream | RebaseOntoPushRemote | RebaseElsewhere | RebaseInteractive
            | RebaseRewordCommit => self.dispatch_rebase(command, args, &targets, window, cx),
            IgnoreToplevel | IgnoreSubdir | IgnorePrivate | IgnoreGlobal => {
                self.dispatch_ignore(command, window, cx)
            }
            StashPush => self.run_stash_push(false, cx),
            StashPushAll => self.run_stash_push(true, cx),
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

    /// (Re)start the background auto-fetch loop. Bumping the generation retires
    /// any loop already running; a fresh one spawns only when `[fetch].auto` is
    /// on and there's a repo. Called at startup and whenever `[fetch]` changes.
    pub(crate) fn start_auto_fetch(&mut self, cx: &mut Context<Self>) {
        let gen = self.auto_fetch_gen.bump();
        if !self.config.fetch.auto || self.repo.is_none() {
            return;
        }
        let interval =
            std::time::Duration::from_secs(self.config.fetch.interval_minutes.max(1) * 60);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                // Stop if the view is gone, this loop was superseded by a newer
                // one, or auto-fetch was turned off.
                let alive = this
                    .update(cx, |this, _| {
                        this.auto_fetch_gen.is_current(gen) && this.config.fetch.auto
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
                this.update(cx, |this, cx| this.run_auto_fetch(cx)).ok();
            }
        })
        .detach();
    }

    /// Periodically check for a newer published release. Failures are silent;
    /// only an available update is surfaced, so offline/API-rate-limit cases do
    /// not nag the user.
    pub(crate) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        let gen = self.update_check_gen.bump();
        if !self.config.check_for_updates {
            return;
        }
        const FIRST_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
        const UPDATE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_DELAY).await;
            loop {
                let alive = this
                    .update(cx, |this, _| {
                        this.update_check_gen.is_current(gen) && this.config.check_for_updates
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
                this.update(cx, |this, cx| this.run_silent_update_check(cx)).ok();
                cx.background_executor().timer(UPDATE_CHECK_INTERVAL).await;
            }
        })
        .detach();
    }

    fn run_silent_update_check(&mut self, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async { latest_release_version() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Ok(latest) = result else { return };
                let Some(current_version) = parse_release_version(CURRENT_VERSION) else { return };
                let Some(latest_version) = parse_release_version(&latest) else { return };
                if current_version < latest_version
                    && this.notified_update_version.as_deref() != Some(latest.as_str())
                {
                    this.notified_update_version = Some(latest.clone());
                    this.set_status(
                        format!("Magritte {latest} is available — run `brew upgrade magritte`"),
                        true,
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run one background `git fetch`, then refresh so the unpushed/unpulled
    /// counts update. Skipped while another job is running, and silent — the
    /// user didn't initiate it, so no progress banner, and failures (offline,
    /// etc.) are ignored until the next tick. Uses a plain repo clone (not the
    /// read-cancel scope) so a routine refresh doesn't abort the fetch.
    fn run_auto_fetch(&mut self, cx: &mut Context<Self>) {
        if self.job_cancel.is_some() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let ok = cx
                .background_executor()
                .spawn(async move { repo.fetch_default(&[]).is_ok() })
                .await;
            this.update(cx, |this, cx| {
                if ok {
                    this.refresh(cx);
                }
                this.end_activity(cx);
            })
            .ok();
        })
        .detach();
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
        // A fixed value set is selection-only; everything else is value entry
        // (free text, candidates are mere suggestions).
        let create = match completion {
            transient::Completion::OneOf(_) => CreateMode::None,
            _ => CreateMode::Value,
        };
        // Candidates available synchronously (a fixed set); git-backed sources
        // load below, off the UI thread, so opening stays instant in big repos.
        let initial: Vec<String> = match completion {
            transient::Completion::OneOf(values) => values.iter().map(|v| v.to_string()).collect(),
            _ => Vec::new(),
        };
        let gen = self.picker_gen.bump();
        self.open_picker(
            PickerAction::SetOption { key, description },
            initial,
            create,
            Vec::new(),
            window,
            cx,
        );
        if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.gen = gen;
            p.resume = Some(Box::new(resume));
            // A free-text value with no completion candidates (e.g. `-n`) has no
            // candidate list — collapse it to just the input + hints.
            p.reserve_candidates = !matches!(completion, transient::Completion::None);
        }

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
            self.confirm = Some((
                format!("{what} to {target}?"),
                Confirm::Reset(mode, target),
            ));
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
        // Seed the prompt with the default pattern — set both the picker's query
        // (what confirm reads) and the visible input (set_value emits no Change).
        let input = if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.list.set_query(&default);
            Some(p.input.clone())
        } else {
            None
        };
        if let Some(input) = input {
            input.update(cx, |s, cx| s.set_value(default, window, cx));
        }
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
        if let Some((hash, short, subject)) = self.point_commit() {
            return self.open_commit_with_args(hash, short, subject, args, paths, cx);
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
        } else if self.status.as_ref().is_some_and(|s| s.staged().next().is_some()) {
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

    /// Open the interactive-rebase todo editor for `base..HEAD`: load the
    /// default todo (all `pick`, oldest first) off the UI thread, then show the
    /// editor — or report when the range is empty / the load fails.
    pub(crate) fn open_rebase_todo(
        &mut self,
        base: String,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        self.set_progress("Loading commits…".to_string(), cx);
        cx.spawn(async move |this, cx| {
            let for_load = base.clone();
            let loaded = cx
                .background_executor()
                .spawn(async move { repo.rebase_todo(&for_load) })
                .await;
            this.update(cx, |this, cx| {
                // Drop a load a newer screen request superseded.
                if !this.screen_gen.is_current(gen) {
                    return;
                }
                match loaded {
                    Ok(steps) if steps.is_empty() => {
                        this.set_status("No commits to rebase".to_string(), true, cx);
                    }
                    Ok(steps) => {
                        this.screen = Screen::RebaseTodo(RebaseTodoView {
                            base,
                            args,
                            initial: steps.clone(),
                            steps,
                            selected: 0,
                            scroll: UniformListScrollHandle::new(),
                            mode: RebaseTodoMode::Start,
                            confirming_cancel: false,
                        });
                        this.clear_status(cx);
                    }
                    Err(e) => this.set_status(format!("error: {e}"), false, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Open the todo editor on an in-progress rebase's remaining steps
    /// (`r e` → `git rebase --edit-todo`). Reads the current todo off the UI
    /// thread; an empty plan (nothing left to reorder) just says so.
    pub(crate) fn open_rebase_edit_todo(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        self.set_progress("Loading rebase todo…".to_string(), cx);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { repo.rebase_current_todo() })
                .await;
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) {
                    return;
                }
                match loaded {
                    Ok(steps) if steps.is_empty() => {
                        this.set_status("No remaining steps to edit".to_string(), true, cx);
                    }
                    Ok(mut steps) => {
                        for step in &mut steps {
                            if this.pending_rebase_reword_matches(&step.oid) {
                                step.action = RebaseAction::Reword;
                            }
                        }
                        this.screen = Screen::RebaseTodo(RebaseTodoView {
                            base: String::new(),
                            args: Vec::new(),
                            initial: steps.clone(),
                            steps,
                            selected: 0,
                            scroll: UniformListScrollHandle::new(),
                            mode: RebaseTodoMode::Edit,
                            confirming_cancel: false,
                        });
                        this.clear_status(cx);
                    }
                    Err(e) => this.set_status(format!("error: {e}"), false, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Move the cursor in the rebase-todo editor.
    pub(crate) fn rebase_todo_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(rt) = self.rebase_todo_mut() {
            let n = rt.steps.len();
            if n == 0 {
                return;
            }
            rt.selected = (rt.selected as isize + delta).clamp(0, n as isize - 1) as usize;
            rt.scroll
                .scroll_to_item(rt.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Set the action of the step at the cursor.
    pub(crate) fn rebase_todo_set_action(&mut self, action: RebaseAction, cx: &mut Context<Self>) {
        if let Some(rt) = self.rebase_todo_mut() {
            if let Some(step) = rt.steps.get_mut(rt.selected) {
                step.action = action;
                cx.notify();
            }
        }
    }

    /// Move the step at the cursor up/down (reorder), following it with the
    /// cursor so successive moves keep acting on the same commit.
    pub(crate) fn rebase_todo_reorder(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(rt) = self.rebase_todo_mut() {
            let n = rt.steps.len();
            if n < 2 {
                return;
            }
            let from = rt.selected;
            let to = (from as isize + delta).clamp(0, n as isize - 1) as usize;
            if to != from {
                rt.steps.swap(from, to);
                rt.selected = to;
                rt.scroll.scroll_to_item(to, gpui::ScrollStrategy::Top);
                cx.notify();
            }
        }
    }

    /// Run the edited todo as one interactive rebase (off the UI thread), close
    /// the editor, then refresh — a pause (an `edit`, or a conflict) surfaces in
    /// the in-progress banner for continue/skip/abort.
    pub(crate) fn run_rebase_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rt) = self.take_rebase_todo() else {
            return;
        };
        self.focus.focus(window, cx);
        let (progress, done): (&str, &'static str) = match rt.mode {
            RebaseTodoMode::Start => ("Rebasing…", "Rebased"),
            RebaseTodoMode::Edit => ("Updating todo…", "Todo updated"),
        };
        let has_pending_reword = rt.steps.iter().any(|s| s.action == RebaseAction::Reword);
        self.pending_rebase_rewords.extend(
            rt.steps
                .iter()
                .filter(|s| s.action == RebaseAction::Reword)
                .map(|s| s.oid.clone()),
        );
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        if !has_pending_reword {
            self.set_progress(progress.to_string(), cx);
            self.begin_activity(cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let result = match rt.mode {
                        RebaseTodoMode::Start => repo.rebase_interactive(&rt.base, &rt.steps, &rt.args),
                        RebaseTodoMode::Edit => repo.rebase_edit_todo(&rt.steps),
                    };
                    let stopped = if result.is_ok() {
                        repo.rebase_stopped_sha()
                    } else {
                        None
                    };
                    (result, stopped)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let (result, stopped) = outcome;
                this.job_cancel = None;
                if result.is_ok() {
                    if let Some(stopped) = stopped {
                        if this.open_pending_rebase_reword(stopped, window, cx) {
                            if !has_pending_reword {
                                this.end_activity(cx);
                            }
                            return;
                        }
                    }
                }
                this.report(done, result, cx);
                this.refresh(cx);
                if !has_pending_reword {
                    this.end_activity(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Cancel the rebase-todo editor — but if the plan has unsaved edits, ask
    /// first rather than silently dropping them (like the commit editor).
    pub(crate) fn close_rebase_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .rebase_todo()
            .is_some_and(|rt| !rt.confirming_cancel && rt.steps != rt.initial);
        if dirty {
            if let Some(rt) = self.rebase_todo_mut() {
                rt.confirming_cancel = true;
            }
            cx.notify();
        } else {
            self.discard_rebase_todo(window, cx);
        }
    }

    /// Close the editor, discarding any edits to the plan.
    pub(crate) fn discard_rebase_todo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the discard confirmation and keep editing the plan.
    pub(crate) fn keep_editing_rebase_todo(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(rt) = self.rebase_todo_mut() {
            rt.confirming_cancel = false;
        }
        cx.notify();
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

    /// Open the free-text command prompt (magit's `!`), prefilled with `git ` —
    /// run a git subcommand by default, or delete the prefix to run any command.
    pub(crate) fn open_run_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(
            PickerAction::RunGit,
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
        // Seed both the picker's query (what confirm reads) and the visible
        // input. The triggering key's focus is deferred a frame (see
        // `open_picker`), so this prefill isn't clobbered by that keystroke.
        let seed = "git ".to_string();
        let input = if let Some(Popup::Picker(p)) = self.popup.as_mut() {
            p.list.set_query(&seed);
            Some(p.input.clone())
        } else {
            None
        };
        if let Some(input) = input {
            input.update(cx, |s, cx| s.set_value(seed, window, cx));
        }
    }

    /// Run a user-typed command from the `!` prompt on the background executor,
    /// then refresh. The prompt is prefilled with `git `, so the first word
    /// selects the program: `git` (the default) runs a git subcommand via the
    /// repo wrapper, anything else runs that program directly in the working
    /// tree (delete the `git ` prefix to do so). Split with POSIX quoting (no
    /// shell) so quoted args like `-m "two words"` stay one argv entry. The full
    /// output is recorded in the `$` log; a multi-line result opens it,
    /// otherwise the first line shows as a notice.
    pub(crate) fn run_user_command(&mut self, input: String, cx: &mut Context<Self>) {
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
        self.run_command_job(
            progress,
            true,
            move |repo| repo.run_user(program.as_deref(), &rest),
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

    // --- In-progress sequence (merge/rebase/cherry-pick/revert/am) -------

    pub(crate) fn sequence_kind(&self) -> Option<SequenceKind> {
        self.sequence.as_ref().map(|s| s.kind)
    }

    /// Continue past a resolved stop.
    pub(crate) fn sequence_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.sequence_kind() {
            if kind == SequenceKind::Rebase {
                if let Some(repo) = self.repo.clone() {
                    cx.spawn_in(window, async move |this, cx| {
                        let stopped = cx
                            .background_executor()
                            .spawn(async move { repo.rebase_stopped_sha() })
                            .await;
                        this.update_in(cx, |this, window, cx| {
                            if let Some(stopped) = stopped {
                                if this.open_pending_rebase_reword(stopped, window, cx) {
                                    return;
                                }
                            }
                            this.run_sequence(SeqOp::Continue, kind, cx);
                        })
                        .ok();
                    })
                    .detach();
                    return;
                }
            }
            self.run_sequence(SeqOp::Continue, kind, cx);
        }
    }

    /// Skip the current step.
    pub(crate) fn sequence_skip(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.sequence_kind() {
            if kind == SequenceKind::Rebase {
                if let Some(repo) = self.repo.clone() {
                    if let Some(stopped) = repo.rebase_stopped_sha() {
                        self.pending_rebase_rewords
                            .retain(|oid| !(stopped.starts_with(oid) || oid.starts_with(&stopped)));
                    }
                }
            }
            self.run_sequence(SeqOp::Skip, kind, cx);
        }
    }

    /// Abort — discards the operation's progress, so confirm first (like magit).
    pub(crate) fn sequence_abort(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.sequence_kind() {
            self.confirm = Some((
                format!("Abort {}?", kind.label()),
                Confirm::AbortSequence(kind),
            ));
            cx.notify();
        }
    }

    /// Run a sequence control on the background executor, then refresh.
    pub(crate) fn run_sequence(&mut self, op: SeqOp, kind: SequenceKind, cx: &mut Context<Self>) {
        if matches!(op, SeqOp::Abort) {
            self.pending_rebase_rewords.clear();
        }
        let (verb, done) = match op {
            SeqOp::Continue => ("Continuing", "Continued"),
            SeqOp::Skip => ("Skipping", "Skipped"),
            SeqOp::Abort => ("Aborting", "Aborted"),
        };
        self.run_job(
            &format!("{verb}…"),
            done,
            move |repo| match op {
                SeqOp::Continue => repo.sequence_continue(kind),
                SeqOp::Skip => repo.sequence_skip(kind),
                SeqOp::Abort => repo.sequence_abort(kind),
            },
            cx,
        );
    }

    // --- Push / pull / fetch --------------------------------------------

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
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let remotes = repo.remotes().unwrap_or_default();
        if remotes.is_empty() {
            self.set_status("No remotes configured".to_string(), false, cx);
            return;
        }
        let existing = repo.remote_branches().unwrap_or_default();
        // Pull lists only existing branches (you can't pull one that doesn't
        // exist). Push seeds the same-named target on every remote — like magit —
        // so `origin/<current>` is always a normal candidate, existing or not.
        let choices = match &transfer {
            Transfer::PushRef { branch } if create => {
                targets::seed_push_branches(repo, &remotes, branch, existing)
            }
            _ => existing,
        };
        let create_mode = if create {
            CreateMode::RemoteBranch
        } else {
            CreateMode::None
        };
        self.open_picker(
            PickerAction::Transfer(transfer),
            choices,
            create_mode,
            switches,
            window,
            cx,
        );
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
            list: PickerList::new(items, create),
            scroll: UniformListScrollHandle::new(),
            action,
            switches,
            loading: false,
            gen: 0,
            reserve_candidates: has_candidates,
            resume: None,
            _sub: sub,
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
                PickerAction::Tag(t) => self.run_tag_action(t, chosen.to_string(), p.switches, cx),
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
                PickerAction::RunGit => self.run_user_command(chosen.to_string(), cx),
                PickerAction::Ignore(dest) => self.run_ignore(dest, chosen.to_string(), cx),
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
                    let rev = chosen.split_whitespace().next().unwrap_or(&chosen).to_string();
                    self.open_commit_with_args(rev.clone(), rev, String::new(), args, paths, cx);
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

    /// Post a status-bar message of a given `kind`. Every post advances
    /// `status_seq`, which is what an auto-dismiss timer checks before clearing:
    /// so a newer message of any kind always invalidates an older notice's
    /// pending fade. Only a `Notice` schedules its own fade; `Progress` stays
    /// until the job reports, `Sticky` until dismissed (Esc / click).
    pub(crate) fn status(&mut self, msg: String, kind: StatusKind, cx: &mut Context<Self>) {
        let seq = self.toast.seq.bump();
        self.toast.message = Some(msg);
        // Most messages have no leading keycap; the few that do set it right
        // after this call.
        self.toast.keys = None;
        cx.notify();
        if matches!(kind, StatusKind::Notice) {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(STATUS_FADE_SECS))
                    .await;
                this.update(cx, |this, cx| {
                    // Only clear if no newer message has replaced it.
                    if this.toast.seq.is_current(seq) {
                        this.toast.message = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// A success notice that fades on its own (`transient`) or a sticky
    /// condition that stays until dismissed.
    pub(crate) fn set_status(&mut self, msg: String, transient: bool, cx: &mut Context<Self>) {
        let kind = if transient {
            StatusKind::Notice
        } else {
            StatusKind::Sticky
        };
        self.status(msg, kind, cx);
    }

    /// Show an in-progress message ("Pushing…") that stays until the job
    /// reports. Advances the sequence so a stale notice's timer can't clear it.
    pub(crate) fn set_progress(&mut self, msg: String, cx: &mut Context<Self>) {
        self.status(msg, StatusKind::Progress, cx);
    }

    /// Check the latest GitHub release tag and report whether this build is current.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.set_progress("Checking for updates…".to_string(), cx);
        let task = cx.background_executor().spawn(async { latest_release_version() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(latest) => this.set_status(version_status_message(CURRENT_VERSION, &latest), false, cx),
                Err(e) => this.set_status(format!("Update check failed: {e}"), false, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Clear the status bar (advancing the sequence so no pending timer fires).
    pub(crate) fn clear_status(&mut self, cx: &mut Context<Self>) {
        self.toast.seq.bump();
        self.toast.message = None;
        self.toast.keys = None;
        cx.notify();
    }

    /// Surface a failed git operation: cancel/timeout get their own short
    /// notices, everything else the error text. Always sticky.
    pub(crate) fn report_error(&mut self, e: magritte_core::Error, cx: &mut Context<Self>) {
        let msg = match e {
            magritte_core::Error::Cancelled => "Cancelled".to_string(),
            magritte_core::Error::TimedOut => "Timed out".to_string(),
            e => format!("error: {e}"),
        };
        self.set_status(msg, false, cx);
    }

    /// Report a git operation's outcome: on success a brief `success` notice
    /// that auto-dismisses (we don't echo git's stderr); on failure the error,
    /// which sticks until dismissed.
    pub(crate) fn report(
        &mut self,
        success: &str,
        result: magritte_core::Result<String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => self.set_status(success.to_string(), true, cx),
            Err(e) => self.report_error(e, cx),
        }
    }

    /// Run a git operation off the UI thread, then `finish` with its result and
    /// refresh — the shape almost every mutating command shares. `progress`
    /// shows immediately; the git work runs on the background executor (so the
    /// UI never blocks); a cancel flag lives on `self` for its duration so
    /// `C-g`/Esc can kill it. The `run_job` wrapper covers the common
    /// fixed-notice shape; this core is for anything bespoke.
    pub(crate) fn run_job_with<F, G>(
        &mut self,
        progress: String,
        op: F,
        finish: G,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Repo) -> magritte_core::Result<String> + Send + 'static,
        G: FnOnce(&mut Self, magritte_core::Result<String>, &mut Context<Self>) + 'static,
    {
        self.run_job_core(
            progress,
            op,
            move |this, result, cx| {
                finish(this, result, cx);
                this.refresh(cx);
            },
            cx,
        );
    }

    /// The bare cancellable-job shell every runner shares: show `progress`, tag
    /// the job's git calls with a cancel flag so `C-g`/Esc can kill a hung
    /// subprocess (stored on `self` for the key handler; cleared when the job
    /// finishes), count activity for the busy spinner, run `op` on the
    /// background executor, then `finish` on the UI thread.
    fn run_job_core<T, F, G>(&mut self, progress: String, op: F, finish: G, cx: &mut Context<Self>)
    where
        T: Send + 'static,
        F: FnOnce(Repo) -> T + Send + 'static,
        G: FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        self.set_progress(progress, cx);
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(repo) })
                .await;
            this.update(cx, |this, cx| {
                this.job_cancel = None;
                finish(this, result, cx);
                this.end_activity(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Run a git operation, then post a fixed past-tense `done` notice on
    /// success (or the error on failure) and refresh.
    pub(crate) fn run_job<F>(
        &mut self,
        progress: &str,
        done: &'static str,
        op: F,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Repo) -> magritte_core::Result<String> + Send + 'static,
    {
        self.run_job_with(
            progress.to_string(),
            op,
            move |this, result, cx| this.report(done, result, cx),
            cx,
        );
    }

    /// Run a user command (the `!` prompt or a `[[command]]`) on the background
    /// path, then show its full output as a toast and refresh (unless opted
    /// out). A failure's output stays up (sticky); success fades. Output isn't a
    /// one-liner and we don't jump to the `$` log — the command behaves like any
    /// other action, just with its output surfaced.
    pub(crate) fn run_command_job<F>(
        &mut self,
        progress: String,
        refresh: bool,
        run: F,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Repo) -> magritte_core::Result<magritte_core::CommandRun> + Send + 'static,
    {
        self.run_job_core(
            progress,
            run,
            move |this, result, cx| {
                match result {
                    Ok(run) => {
                        // Cap the toast, pointing to the `$` log (with its
                        // current key) for the rest when the output is long.
                        let log_key = current_key(&this.keymap, "command-log", Some("$"));
                        let toast = command_toast(&run, log_key.as_deref());
                        this.set_status(toast, run.ok, cx);
                    }
                    Err(e) => this.report_error(e, cx),
                }
                if refresh {
                    this.refresh(cx);
                }
            },
            cx,
        );
    }

    /// Cancel the active mutating job, if any — killing its git subprocess.
    /// Returns whether a job was running (so the key handler can swallow the key).
    pub(crate) fn cancel_job(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(cancel) = self.job_cancel.take() else {
            return false;
        };
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.set_progress("Cancelling…".to_string(), cx);
        true
    }

    /// Copy `text` to the clipboard and flash a brief confirmation. The notice
    /// echoes a short single-line value (a path, a hash) but stays generic for
    /// multi-line or long copies.
    pub(crate) fn copy_to_clipboard(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        // Echo a short single-line value (a path, a hash) emphasized after the
        // label; stay generic for multi-line or long copies.
        let value =
            (!text.contains('\n') && text.chars().count() <= 40).then(|| SharedString::from(text));
        // Set the value before the message: `set_status` notifies, and the
        // render reads both, so the toast paints with the value present.
        self.toast.copied = value;
        self.set_status(COPIED_LABEL.to_string(), true, cx);
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

        let (verb, done) = match &action {
            BranchAction::Checkout => ("Checking out", "Checked out"),
            BranchAction::Create { .. } => ("Creating branch", "Created branch"),
            BranchAction::RenameTo { .. } => ("Renaming branch", "Renamed branch"),
            BranchAction::Delete => ("Deleting branch", "Deleted branch"),
            BranchAction::RenameFrom => unreachable!("handled above"),
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
                BranchAction::RenameFrom => unreachable!("handled above"),
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
        cx: &mut Context<Self>,
    ) {
        if chosen.trim().is_empty() {
            self.set_status("Tag name required".to_string(), false, cx);
            return;
        }
        let force = switches.iter().any(|s| s == "--force");
        let target = self.selected_commit_hash().unwrap_or_else(|| "HEAD".to_string());
        let (verb, done) = match action {
            TagAction::Create { .. } => ("Tagging", "Tagged"),
            TagAction::Delete => ("Deleting tag", "Deleted tag"),
        };
        self.run_job(
            &format!("{verb}…"),
            done,
            move |repo| match action {
                TagAction::Create { annotated: true } => {
                    repo.create_annotated_tag(&chosen, &target, force)
                }
                TagAction::Create { annotated: false } => repo.create_tag(&chosen, &target, force),
                TagAction::Delete => repo.delete_tag(&chosen),
            },
            cx,
        );
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
        }
    }

    /// Stash the working tree and index (`Z z` / `Z Z`), on the background
    /// executor, then refresh.
    pub(crate) fn run_stash_push(&mut self, include_untracked: bool, cx: &mut Context<Self>) {
        self.run_job(
            "Stashing…",
            "Stashed",
            move |repo| repo.stash_push(None, include_untracked),
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

// --- Mid-rebase reword (the rebase flow that pauses to edit a message) ----
//
// Moved next to the rest of the rebase/sequence dispatch so the whole rebase
// state machine lives in one file.

impl StatusView {
    pub(crate) fn run_rebase_reword_from_rev(
        &mut self,
        rev: String,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let base = format!("{rev}^");
                    let result = (|| {
                        let mut steps = repo.rebase_todo(&base)?;
                        let step = steps
                            .iter_mut()
                            .find(|s| rev.starts_with(&s.oid) || s.oid.starts_with(&rev))
                            .ok_or_else(|| {
                                magritte_core::Error::Message(
                                    "selected commit is not in the rebase range".to_string(),
                                )
                            })?;
                        let oid = step.oid.clone();
                        step.action = RebaseAction::Reword;
                        repo.rebase_interactive(&base, &steps, &args)?;
                        Ok::<_, magritte_core::Error>(oid)
                    })();
                    let stopped = if result.is_ok() {
                        repo.rebase_stopped_sha()
                    } else {
                        None
                    };
                    (result, stopped)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let (result, stopped) = outcome;
                this.job_cancel = None;
                match result {
                    Ok(oid) => {
                        this.pending_rebase_rewords.insert(oid);
                        if let Some(stopped) = stopped {
                            if this.open_pending_rebase_reword(stopped, window, cx) {
                                return;
                            }
                        }
                        this.report("Rebased", Ok(String::new()), cx);
                        this.refresh(cx);
                    }
                    Err(e) => {
                        this.report("Rebased", Err(e), cx);
                        this.refresh(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn pending_rebase_reword_matches(&self, stopped_sha: &str) -> bool {
        self.pending_rebase_rewords
            .iter()
            .any(|oid| stopped_sha.starts_with(oid) || oid.starts_with(stopped_sha))
    }

    pub(crate) fn open_pending_rebase_reword(
        &mut self,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.pending_rebase_reword_matches(&stopped_sha) {
            return false;
        }
        if let Some(git_editor) = self.external_commit_editor() {
            self.run_rebase_reword_with_external_editor(stopped_sha, git_editor, window, cx);
            return true;
        }
        self.clear_status(cx);
        self.open_editor_after(
            CommitMode::Reword,
            Vec::new(),
            CommitAfterSubmit::ContinueRebase { stopped_sha },
            window,
            cx,
        );
        true
    }

    pub(crate) fn run_rebase_reword_commit(
        &mut self,
        message: String,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_reword_after_commit(
            stopped_sha,
            window,
            cx,
            move |repo| repo.commit(&message, CommitMode::Reword, &[]),
        );
    }

    pub(crate) fn run_rebase_reword_with_external_editor(
        &mut self,
        stopped_sha: String,
        git_editor: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_reword_after_commit(
            stopped_sha,
            window,
            cx,
            move |repo| repo.commit_with_editor(CommitMode::Reword, &[], &git_editor),
        );
    }

    pub(crate) fn run_rebase_reword_after_commit<F>(
        &mut self,
        stopped_sha: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        commit: F,
    ) where
        F: FnOnce(&Repo) -> magritte_core::Result<String> + Send + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel);
        cx.spawn_in(window, async move |this, cx| {
            let stopped_for_result = stopped_sha.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let commit_result = commit(&repo);
                    let committed = commit_result.is_ok();
                    let result = commit_result.and_then(|_| repo.sequence_continue(SequenceKind::Rebase));
                    let stopped = if result.is_ok() {
                        repo.rebase_stopped_sha()
                    } else {
                        None
                    };
                    (result, stopped, committed)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let (result, stopped, committed) = outcome;
                this.job_cancel = None;
                if committed {
                    this.pending_rebase_rewords.remove(&stopped_for_result);
                }
                if result.is_ok() {
                    if let Some(stopped) = stopped {
                        if this.open_pending_rebase_reword(stopped, window, cx) {
                            return;
                        }
                    }
                }
                this.report("Reworded", result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }
}

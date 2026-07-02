//! The log screens: the `$` git-command log, the commit log/reflog loaders,
//! the log-as-picker flows (select a commit for rebase/reword), commit-at-point
//! actions from the log (cherry-pick/revert transients, yank), and the log
//! cursor. `impl StatusView` like the other view slices.

use gpui::{Context, UniformListScrollHandle, Window};
use magritte_core::{transient, LogEntry, Repo};

use crate::*;

/// A flattened row of the git command-log view: a command, or one line of its
/// output. Flattening keeps the view a single uniform-height list.
pub(crate) enum GitLogRow {
    /// `prog` is the program (`git` for the common case), shown dimmed before
    /// the arguments.
    Command {
        elapsed: String,
        slow: bool,
        very_slow: bool,
        prog: String,
        args: String,
        ok: bool,
    },
    Output(String),
}

/// Why the log view is open. Browsing is the default; selecting picks a commit
/// to act on and confirms with Return (magit's `magit-log-select`).
#[derive(PartialEq, Eq)]
pub(crate) enum LogPurpose {
    /// Ordinary browsing: Return opens the commit's diff.
    Browse,
    /// Pick the commit to rebase interactively since (its `^`..HEAD becomes the
    /// editable todo). Carries the switches gathered in the rebase transient.
    SelectRebaseBase { args: Vec<String> },
    /// Pick a commit to reword directly via an app-managed rebase stop.
    SelectRebaseReword { args: Vec<String> },
}

/// The commit-log view (`l`): a scrollable list of commits with j/k navigation.
/// When browsing, Return opens the selected commit's diff in a [`CommitView`];
/// in a select mode, Return confirms the commit for the pending action.
pub(crate) struct LogState {
    pub(crate) entries: Vec<magritte_core::LogEntry>,
    pub(crate) selected: usize,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) load: LogLoad,
    pub(crate) purpose: LogPurpose,
}

/// Load state of the log view, so the body can distinguish still-loading from a
/// load error from a genuinely empty history.
pub(crate) enum LogLoad {
    Loading,
    Loaded,
    Failed(String),
}

impl StatusView {
    /// How many recent commits the log loads. Bounded so opening the log in a
    /// huge repo stays cheap; the bar notes when it's capped.
    pub(crate) const LOG_LIMIT: usize = 256;

    /// Open the git command-log view (magit's `$` process buffer), scrolled to
    /// the most recent command.
    pub(crate) fn open_git_log(&mut self, cx: &mut Context<Self>) {
        // Dismiss any status toast — you came here to read the full output it
        // pointed at, and it would otherwise just float over this view.
        self.clear_status(cx);
        let scroll = UniformListScrollHandle::new();
        let last = self.git_log_rows().len().saturating_sub(1);
        scroll.scroll_to_item(last, gpui::ScrollStrategy::Bottom);
        self.screen = Screen::GitLog {
            view: ScrollView { scroll, top: last },
            show_all: false,
        };
        cx.notify();
    }

    pub(crate) fn close_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Toggle whether the command log also lists the UI's own read-only queries.
    pub(crate) fn toggle_git_log_all(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::GitLog { show_all, .. } = &mut self.screen {
            *show_all = !*show_all;
        }
        cx.notify();
    }

    /// Open the commit-log view for `git log <args>`: show it immediately
    /// (empty), then load the commits off the UI thread. Args are assembled by
    /// [`build_log_args`] (including the default limit).
    /// Show the log view (loading) for `purpose`, run `load` on the background
    /// executor, and fill the log when it lands — the shape all four log
    /// openers share. The screen-load generation guards against a superseded
    /// open populating a newer screen.
    fn spawn_log<F>(&mut self, purpose: LogPurpose, load: F, cx: &mut Context<Self>)
    where
        F: FnOnce(Repo) -> magritte_core::Result<Vec<LogEntry>> + Send + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.show_log_loading(purpose, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load(repo) })
                .await;
            this.update(cx, |this, cx| this.fill_log(gen, result, cx))
                .ok();
        })
        .detach();
    }

    pub(crate) fn start_log(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        self.spawn_log(LogPurpose::Browse, move |repo| repo.log_with(&args), cx);
    }

    /// Open the log to pick the commit to rebase interactively *since* — magit's
    /// `magit-log-select`. The chosen commit and everything above it become the
    /// editable todo; `switches` carries the rebase transient's flags.
    pub(crate) fn start_log_select_rebase(
        &mut self,
        switches: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let args = build_log_args(Vec::new(), LogScope::Current, Vec::new(), Self::LOG_LIMIT);
        self.spawn_log(
            LogPurpose::SelectRebaseBase { args: switches },
            move |repo| repo.log_with(&args),
            cx,
        );
    }

    pub(crate) fn start_log_select_rebase_reword(
        &mut self,
        switches: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let args = build_log_args(Vec::new(), LogScope::Current, Vec::new(), Self::LOG_LIMIT);
        self.spawn_log(
            LogPurpose::SelectRebaseReword { args: switches },
            move |repo| repo.log_with(&args),
            cx,
        );
    }

    /// Begin an interactive rebase since the commit selected in the log (its
    /// parent is the base, so that commit and everything above it are editable),
    /// or the commit at point in a status commit section, opening the todo
    /// editor. `args` are the rebase switches. First checks (off the UI thread)
    /// whether that commit is already published; if so, confirm before rewriting
    /// pushed history — like magit's rebase assert and our amend/reword warning.
    pub(crate) fn rebase_since_selected(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(rev) = self.selected_commit_hash() else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let probe = rev.clone();
        let branches = self.config.published_branches.clone();
        cx.spawn(async move |this, cx| {
            let published = cx
                .background_executor()
                .spawn(async move { repo.published_on(&probe, &branches) })
                .await;
            this.update(cx, |this, cx| {
                // base = commit^: `base..HEAD` then includes the selected commit.
                let Some(target) = published else {
                    this.open_rebase_todo(format!("{rev}^"), args, cx);
                    return;
                };
                // The confirmation bar is status-screen chrome, so leave the log
                // to show it; "yes" opens the todo editor.
                this.screen = Screen::Status;
                this.confirm = Some((
                    format!("{rev} has already been pushed to {target}. Rebase since it anyway?"),
                    Confirm::RebaseSincePushed { rev, args },
                ));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reword the selected older commit using an interactive rebase, matching
    /// Magit's `c R` / `r w` / `magit-rebase-reword-commit` path.
    pub(crate) fn reword_past_selected(
        &mut self,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_commit_hash().is_some() {
            self.rebase_reword_selected(args, window, cx);
        } else {
            self.start_log_select_rebase_reword(args, cx);
        }
    }

    pub(crate) fn rebase_reword_selected(
        &mut self,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rev) = self.selected_commit_hash() else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let probe = rev.clone();
        let branches = self.config.published_branches.clone();
        cx.spawn_in(window, async move |this, cx| {
            let published = cx
                .background_executor()
                .spawn(async move { repo.published_on(&probe, &branches) })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(target) = published else {
                    this.run_rebase_reword_from_rev(rev, args, window, cx);
                    return;
                };
                this.screen = Screen::Status;
                this.confirm = Some((
                    format!("{rev} has already been pushed to {target}. Rebase since it anyway?"),
                    Confirm::RebaseRewordPushed { rev, args },
                ));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The selected commit in the log, or the commit row at point in status.
    pub(crate) fn selected_commit_hash(&self) -> Option<String> {
        self.log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.hash.clone())
            .or_else(|| self.point_commit().map(|(hash, _, _)| hash))
    }

    /// Open the cherry-pick transient, using a status/log commit at point as the
    /// default when its suffix fires (Magit's commit-at-point model).
    pub(crate) fn open_cherry_pick_transient(&mut self, cx: &mut Context<Self>) {
        self.open_transient(
            "cherry-pick",
            transient::cherry_pick_transient(),
            RemoteTargets::default(),
            cx,
        );
    }

    /// Open the revert transient, using a status/log commit at point as the
    /// default when its suffix fires (Magit's commit-at-point model).
    pub(crate) fn open_revert_transient(&mut self, cx: &mut Context<Self>) {
        self.open_transient(
            "revert",
            transient::revert_transient(self.keymap_style()),
            RemoteTargets::default(),
            cx,
        );
    }

    pub(crate) fn keymap_style(&self) -> transient::KeymapStyle {
        self.config.keymap_preset.transient_style()
    }

    /// Open the selected commit's diff (the clickable "view" button; Return does
    /// the same from the key handler).
    pub(crate) fn view_log_commit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_commit_view(cx);
    }

    /// Confirm the selected commit in a log-select mode (the clickable "select"
    /// button; Return does the same from the key handler).
    pub(crate) fn confirm_log_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.log().map(|l| &l.purpose) {
            Some(LogPurpose::SelectRebaseBase { args }) => {
                self.rebase_since_selected(args.clone(), cx);
            }
            Some(LogPurpose::SelectRebaseReword { args }) => {
                self.reword_past_selected(args.clone(), window, cx);
            }
            _ => {}
        }
    }

    /// Open the reflog view (`l r`).
    pub(crate) fn start_reflog(&mut self, limit: usize, cx: &mut Context<Self>) {
        self.spawn_log(LogPurpose::Browse, move |repo| repo.reflog(limit), cx);
    }

    /// Show the (empty) log view immediately while commits load, returning the
    /// screen-load generation the matching `fill_log` must still see.
    pub(crate) fn show_log_loading(&mut self, purpose: LogPurpose, cx: &mut Context<Self>) -> u64 {
        let gen = self.next_screen_gen();
        self.screen = Screen::Log(LogState {
            entries: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            load: LogLoad::Loading,
            purpose,
        });
        cx.notify();
        gen
    }

    /// Fill the open log view with the load result: entries on success, the
    /// error otherwise (so the view shows it rather than an endless "Loading…").
    pub(crate) fn fill_log(
        &mut self,
        gen: u64,
        result: magritte_core::Result<Vec<magritte_core::LogEntry>>,
        cx: &mut Context<Self>,
    ) {
        // Drop a load a newer log/reflog request has superseded.
        if !self.screen_gen.is_current(gen) {
            return;
        }
        if let Some(log) = self.log_mut() {
            match result {
                Ok(entries) => {
                    log.entries = entries;
                    log.load = LogLoad::Loaded;
                }
                Err(e) => log.load = LogLoad::Failed(e.to_string()),
            }
        }
        cx.notify();
    }

    pub(crate) fn close_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Move the log's selection by `delta`, keeping it in view.
    pub(crate) fn log_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(log) = self.log_mut() {
            if log.entries.is_empty() {
                return;
            }
            let last = log.entries.len() - 1;
            log.selected = (log.selected as isize + delta).clamp(0, last as isize) as usize;
            log.scroll
                .scroll_to_item(log.selected, gpui::ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Cherry-pick or revert the commit selected in the log, or the commit at
    /// point in a status commit section, then return to the status view (so a
    /// conflict shows in the in-progress banner). Runs on the background
    /// executor.
    pub(crate) fn pick_selected(
        &mut self,
        op: PickOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_selected_with_args(op, Vec::new(), window, cx);
    }

    pub(crate) fn pick_selected_with_args(
        &mut self,
        op: PickOp,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rev) = self.selected_commit_hash() else {
            self.set_status("No commit at point".to_string(), false, cx);
            return;
        };
        self.pick_rev_with_args(op, rev, args, window, cx);
    }

    pub(crate) fn pick_rev_with_args(
        &mut self,
        op: PickOp,
        rev: String,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if rev.trim().is_empty() {
            self.set_status("Revision or range required".to_string(), false, cx);
            return;
        }
        let (verb, done) = match op {
            PickOp::CherryPick => ("Cherry-picking", "Cherry-picked"),
            PickOp::CherryApply => ("Applying", "Applied"),
            PickOp::Revert => ("Reverting", "Reverted"),
            PickOp::RevertNoCommit => ("Reverting", "Reverted"),
        };
        if self.log().is_some() {
            self.close_log(window, cx);
        }
        self.run_job(
            &format!("{verb} {rev}…"),
            done,
            move |repo| match op {
                PickOp::CherryPick => repo.cherry_pick_with_args(&rev, &args),
                PickOp::CherryApply => repo.cherry_apply_with_args(&rev, &args),
                PickOp::Revert => {
                    let args = if args.is_empty() {
                        vec!["--no-edit".to_string()]
                    } else {
                        args
                    };
                    repo.revert_with_args(&rev, &args)
                }
                PickOp::RevertNoCommit => repo.revert_no_commit_with_args(&rev, &args),
            },
            cx,
        );
    }

    /// Copy the full hash of the commit selected in the log.
    pub(crate) fn copy_log_commit(&mut self, cx: &mut Context<Self>) {
        let hash = self
            .log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.hash.clone());
        if let Some(hash) = hash {
            self.copy_to_clipboard(hash, cx);
        }
    }
}

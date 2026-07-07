//! The rebase flows: the interactive-rebase todo editor (load, reorder, run,
//! close), the in-progress sequence controls (continue/skip/abort), and the
//! mid-rebase reword state machine that pauses into the message editor.
//! `impl StatusView` like the other view slices.

use gpui::{Context, UniformListScrollHandle, Window};

use crate::*;

impl StatusView {
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
        // Guard on the repo before mutating anything, so a missing repo can't
        // close the editor and leave phantom pending rewords behind.
        if self.repo.is_none() {
            return;
        }
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
        // No progress notice when a reword is queued — the message editor opens
        // next, so a flashed "Rebasing…" would just be noise under it.
        let progress = (!has_pending_reword).then(|| progress.to_string());
        self.run_rebase_job(
            progress,
            move |repo| match rt.mode {
                RebaseTodoMode::Start => repo.rebase_interactive(&rt.base, &rt.steps, &rt.args),
                RebaseTodoMode::Edit => repo.rebase_edit_todo(&rt.steps),
            },
            move |this, result, stopped, window, cx| {
                if result.is_ok() {
                    if let Some(stopped) = stopped {
                        if this.open_pending_rebase_reword(stopped, window, cx) {
                            return;
                        }
                    }
                }
                this.report(done, result, cx);
                this.refresh(cx);
            },
            window,
            cx,
        );
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

    // --- In-progress sequence (merge/rebase/cherry-pick/revert/am) -------

    pub(crate) fn sequence_kind(&self) -> Option<SequenceKind> {
        self.sequence.as_ref().map(|s| s.kind)
    }

    /// Continue past a resolved stop.
    pub(crate) fn sequence_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sequence_control_blocked(cx) {
            return;
        }
        if let Some(kind) = self.sequence_kind() {
            if kind == SequenceKind::Rebase && self.repo.is_some() {
                // Probe for an app-managed reword stop first (off the UI
                // thread — the probe resolves the git dir); the whole probe
                // + continue runs as one job so a key bounce can't fire two.
                self.run_rebase_job(
                    None,
                    |_repo| Ok(()),
                    move |this, _result, stopped, window, cx| {
                        if let Some(stopped) = stopped {
                            if this.open_pending_rebase_reword(stopped, window, cx) {
                                return;
                            }
                        }
                        this.run_sequence(SeqOp::Continue, kind, cx);
                    },
                    window,
                    cx,
                );
                return;
            }
            self.run_sequence(SeqOp::Continue, kind, cx);
        }
    }

    /// Skip the current step.
    pub(crate) fn sequence_skip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sequence_control_blocked(cx) {
            return;
        }
        if let Some(kind) = self.sequence_kind() {
            if kind == SequenceKind::Rebase && self.repo.is_some() {
                // A skipped stop's pending reword no longer applies. Async like
                // continue — the sibling probe used to run on the UI thread.
                self.run_rebase_job(
                    None,
                    |_repo| Ok(()),
                    move |this, _result, stopped, _window, cx| {
                        if let Some(stopped) = stopped {
                            this.pending_rebase_rewords.retain(|oid| {
                                !(stopped.starts_with(oid) || oid.starts_with(&stopped))
                            });
                        }
                        this.run_sequence(SeqOp::Skip, kind, cx);
                    },
                    window,
                    cx,
                );
                return;
            }
            self.run_sequence(SeqOp::Skip, kind, cx);
        }
    }

    /// Whether a sequence control must wait: another mutating job is running
    /// (continuing mid-push, or a bounced double-press, would race it — git
    /// would hit the index lock anyway). Reports why, so the keypress isn't
    /// silently ignored.
    fn sequence_control_blocked(&mut self, cx: &mut Context<Self>) -> bool {
        if self.job_cancel.is_some() {
            self.set_status(
                "Another operation is running (Esc cancels it)".to_string(),
                false,
                cx,
            );
            return true;
        }
        false
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

    // --- Mid-rebase reword (the rebase flow that pauses to edit a message) ----

    pub(crate) fn run_rebase_reword_from_rev(
        &mut self,
        rev: String,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_job(
            Some("Rebasing…".to_string()),
            move |repo| {
                let base = format!("{rev}^");
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
                Ok(oid)
            },
            move |this, result, stopped, window, cx| match result {
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
            },
            window,
            cx,
        );
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
        self.run_rebase_reword_after_commit(stopped_sha, window, cx, move |repo| {
            repo.commit(&message, CommitMode::Reword, &[])
        });
    }

    pub(crate) fn run_rebase_reword_with_external_editor(
        &mut self,
        stopped_sha: String,
        git_editor: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_rebase_reword_after_commit(stopped_sha, window, cx, move |repo| {
            repo.commit_with_editor(CommitMode::Reword, &[], &git_editor)
        });
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
        let stopped_for_result = stopped_sha;
        self.run_rebase_job(
            Some("Continuing rebase…".to_string()),
            move |repo| {
                let commit_result = commit(repo);
                let committed = commit_result.is_ok();
                let result =
                    commit_result.and_then(|_| repo.sequence_continue(SequenceKind::Rebase));
                Ok((committed, result))
            },
            move |this, outcome, stopped, window, cx| {
                let (committed, result) = outcome.unwrap_or_else(|e| (false, Err(e)));
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
            },
            window,
            cx,
        );
    }
}

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
        let for_load = base.clone();
        self.open_rebase_todo_editor(
            "Loading commits…",
            "No commits to rebase",
            base,
            args,
            RebaseTodoMode::Start,
            move |repo| repo.rebase_todo(&for_load),
            cx,
        );
    }

    /// Open the todo editor on an in-progress rebase's remaining steps
    /// (`r e` → `git rebase --edit-todo`). Reads the current todo off the UI
    /// thread; an empty plan (nothing left to reorder) just says so.
    pub(crate) fn open_rebase_edit_todo(&mut self, cx: &mut Context<Self>) {
        self.open_rebase_todo_editor(
            "Loading rebase todo…",
            "No remaining steps to edit",
            String::new(),
            Vec::new(),
            RebaseTodoMode::Edit,
            |repo| repo.rebase_current_todo(),
            cx,
        );
    }

    /// The shared shell of the two todo-editor entry points: `load` the plan
    /// off the UI thread, then show the editor — or report when the plan is
    /// empty / the load fails. In `Edit` mode a step with a pending reword is
    /// re-marked `reword`, so the pause it caused stays visible in the plan.
    #[allow(clippy::too_many_arguments)]
    fn open_rebase_todo_editor<F>(
        &mut self,
        progress: &str,
        empty_msg: &'static str,
        base: String,
        args: Vec<String>,
        mode: RebaseTodoMode,
        load: F,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(&Repo) -> magritte_core::Result<Vec<magritte_core::RebaseStep>> + Send + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        self.set_progress(progress.to_string(), cx);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { load(&repo) })
                .await;
            this.update(cx, |this, cx| {
                // Drop a load a newer screen request superseded.
                if !this.screen_gen.is_current(gen) {
                    return;
                }
                match loaded {
                    Ok(steps) if steps.is_empty() => {
                        this.set_status(empty_msg.to_string(), true, cx);
                    }
                    Ok(mut steps) => {
                        if mode == RebaseTodoMode::Edit {
                            for step in &mut steps {
                                if this.pending_rebase_reword_matches(&step.oid) {
                                    step.action = RebaseAction::Reword;
                                }
                            }
                        }
                        this.screen = Screen::RebaseTodo(RebaseTodoView {
                            base,
                            args,
                            initial: steps.clone(),
                            steps,
                            selected: 0,
                            scroll: UniformListScrollHandle::new(),
                            mode,
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
            let Some(ix) = list_move(rt.selected, rt.steps.len(), delta, |_| true) else {
                return;
            };
            rt.selected = ix;
            rt.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Top);
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
                            this.pending_rebase_rewords
                                .retain(|oid| !same_commit(oid, &stopped));
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
                    .find(|s| same_commit(&s.oid, &rev))
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
            .any(|oid| same_commit(oid, stopped_sha))
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
                    // The set holds the todo's abbreviated (`%h`) oids while the
                    // stopped sha is full-length, so remove by prefix match — an
                    // exact remove would silently leave the entry behind.
                    this.pending_rebase_rewords
                        .retain(|oid| !same_commit(oid, &stopped_for_result));
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

/// Whether two commit ids name the same commit when either may be abbreviated
/// (the rebase todo's `%h` oids vs git's full-length `stopped-sha`).
fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

#[cfg(test)]
mod tests {
    use super::same_commit;
    use std::collections::HashSet;

    #[test]
    fn same_commit_matches_abbreviated_against_full() {
        let full = "40a65c138f5b60e07463f941af80cb3e6bf0979a";
        assert!(same_commit("40a65c1", full));
        assert!(same_commit(full, "40a65c1"));
        assert!(same_commit(full, full));
        assert!(!same_commit("40a65c1", "deadbeef"));
        assert!(!same_commit("", full));
        assert!(!same_commit(full, ""));
    }

    #[test]
    fn completed_reword_is_removed_despite_abbreviated_oids() {
        // The pending set holds `%h` oids; a finished reword must clear its
        // entry when compared against the full stopped sha.
        let mut pending: HashSet<String> = HashSet::from(["40a65c1".to_string()]);
        let stopped = "40a65c138f5b60e07463f941af80cb3e6bf0979a";
        pending.retain(|oid| !same_commit(oid, stopped));
        assert!(pending.is_empty());

        // An unrelated pending reword survives.
        let mut pending: HashSet<String> = HashSet::from(["deadbee".to_string()]);
        pending.retain(|oid| !same_commit(oid, stopped));
        assert_eq!(pending.len(), 1);
    }
}

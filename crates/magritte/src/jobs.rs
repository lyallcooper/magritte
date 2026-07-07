//! The background-job machinery: the cancellable `run_job*` runners every
//! mutating command shares, the status-toast/report plumbing, clipboard
//! copies, and the unattended auto-fetch / update-check loops. `impl
//! StatusView` like the other view slices.

use gpui::{Context, Window};

use crate::*;

/// The bottom-bar status toast — one logical value whose parts move together:
/// the message, an optional emphasized copied value (rendered only while the
/// message is the Copied label), optional leading keycaps, and the sequence
/// stamp that keeps a stale fade timer from clearing a newer message.
#[derive(Default)]
pub(crate) struct StatusToast {
    /// Last operation result / progress, shown in the bottom bar.
    pub(crate) message: Option<String>,
    /// A keystroke to render as keycap(s) before the message (e.g. the unbound
    /// `g x` in "g x is unbound"). Cleared by every status post; set right
    /// after by the few messages that lead with a key.
    pub(crate) keys: Option<String>,
    /// Whether the current message is a one-shot notice (e.g. "… is unbound"),
    /// which the next keypress dismisses — not a job's progress or a sticky
    /// condition, which stay until they resolve or are dismissed explicitly.
    pub(crate) transient: bool,
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
        self.toast.transient = matches!(kind, StatusKind::Notice);
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

    /// Clear the status bar (advancing the sequence so no pending timer fires).
    pub(crate) fn clear_status(&mut self, cx: &mut Context<Self>) {
        self.toast.seq.bump();
        self.toast.message = None;
        self.toast.keys = None;
        self.toast.transient = false;
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
        self.job_cancel = Some(cancel.clone());
        self.set_progress(progress, cx);
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(repo) })
                .await;
            this.update(cx, |this, cx| {
                this.clear_job_cancel(&cancel);
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
                        let log_key = current_key(this.screen_bindings(), "command-log", Some("$"));
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

    /// Clear the active job's cancel flag — but only if it's still *this* job's
    /// flag. A job that started while another was running installs its own; the
    /// first job's finish must not clobber it (which would strand the newer job
    /// un-cancellable and hide its "running" indicator).
    pub(crate) fn clear_job_cancel(&mut self, cancel: &Arc<AtomicBool>) {
        if self
            .job_cancel
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, cancel))
        {
            self.job_cancel = None;
        }
    }

    /// Whether a fire-and-forget probe's continuation may still open a prompt:
    /// the user hasn't switched screens or opened a popup/confirmation since
    /// it started. When this is false the user has moved on — drop the result
    /// (they can re-run the command) instead of interrupting the new context.
    pub(crate) fn ui_idle_for_prompt(&self) -> bool {
        matches!(self.screen, Screen::Status) && self.popup.is_none() && self.confirm.is_none()
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
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.set_status(COPIED_LABEL.to_string(), true, cx);
    }

    /// The shared shell of every rebase-driving job: run `op` with a cancel
    /// flag (and optional progress + spinner), probe git's stopped-sha on
    /// success, and hand both to `finish` on the UI thread — which typically
    /// routes a pending reword stop into the in-app editor before reporting.
    /// `spawn_in`, because opening that editor needs a `Window` (the plain
    /// `run_job` family doesn't carry one).
    pub(crate) fn run_rebase_job<T, F, G>(
        &mut self,
        progress: Option<String>,
        op: F,
        finish: G,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        T: Send + 'static,
        F: FnOnce(&Repo) -> magritte_core::Result<T> + Send + 'static,
        G: FnOnce(
                &mut Self,
                magritte_core::Result<T>,
                Option<String>,
                &mut Window,
                &mut Context<Self>,
            ) + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (repo, cancel) = repo.cancellable();
        self.job_cancel = Some(cancel.clone());
        let show_activity = progress.is_some();
        if let Some(progress) = progress {
            self.set_progress(progress, cx);
            self.begin_activity(cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let result = op(&repo);
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
                this.clear_job_cancel(&cancel);
                if show_activity {
                    this.end_activity(cx);
                }
                finish(this, result, stopped, window, cx);
            })
            .ok();
        })
        .detach();
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
        const UPDATE_CHECK_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(24 * 60 * 60);
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
                this.update(cx, |this, cx| this.run_silent_update_check(cx))
                    .ok();
                cx.background_executor().timer(UPDATE_CHECK_INTERVAL).await;
            }
        })
        .detach();
    }

    fn run_silent_update_check(&mut self, cx: &mut Context<Self>) {
        let task = cx
            .background_executor()
            .spawn(async { latest_release_version() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Ok(latest) = result else { return };
                let Some(current_version) = parse_release_version(CURRENT_VERSION) else {
                    return;
                };
                let Some(latest_version) = parse_release_version(&latest) else {
                    return;
                };
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

    /// How long an unattended auto-fetch may run before its subprocess is
    /// killed — generous for a slow link, far below "wedged forever".
    const AUTO_FETCH_TIMEOUT_SECS: u64 = 120;

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
        // Unattended, so give it a hard time bound: nobody is watching to
        // C-g a wedged remote, and an unbounded hang would occupy the busy
        // spinner (and this loop's slot) until restart.
        let repo = repo.with_timeout(std::time::Duration::from_secs(
            Self::AUTO_FETCH_TIMEOUT_SECS,
        ));
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

    /// Check the latest GitHub release tag and report whether this build is current.
    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.set_progress("Checking for updates…".to_string(), cx);
        let task = cx
            .background_executor()
            .spawn(async { latest_release_version() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(latest) => {
                    this.set_status(version_status_message(CURRENT_VERSION, &latest), false, cx)
                }
                Err(e) => this.set_status(format!("Update check failed: {e}"), false, cx),
            })
            .ok();
        })
        .detach();
    }
}

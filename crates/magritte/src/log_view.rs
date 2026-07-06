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
    /// Pick the commit to fix up / squash into (the `--fixup=`/`--squash=`
    /// target), for the chosen [`SquashOp`], with the commit transient's args.
    SelectSquash { op: SquashOp, args: Vec<String> },
}

/// The four fixup/squash flavors from the commit transient. Fixup keeps the
/// target's message; squash lets it be edited (we take the combined message).
/// The "instant" variants immediately autosquash the new commit into its
/// target; the plain variants leave that to a later `r f`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SquashOp {
    Fixup,
    Squash,
    InstantFixup,
    InstantSquash,
}

impl SquashOp {
    /// Whether this variant rewrites history immediately (instant = autosquash
    /// right away, so it must warn before touching published commits).
    pub(crate) fn is_instant(self) -> bool {
        matches!(self, SquashOp::InstantFixup | SquashOp::InstantSquash)
    }

    fn is_squash(self) -> bool {
        matches!(self, SquashOp::Squash | SquashOp::InstantSquash)
    }

    fn progress(self) -> &'static str {
        if self.is_squash() {
            "Squashing…"
        } else {
            "Fixing up…"
        }
    }
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
    /// The args the browse listing was fetched with, and its commit limit, so
    /// `+`/`-` can re-fetch with a doubled/halved limit (magit's log limit
    /// keys). Left empty for the select modes, which don't re-limit.
    pub(crate) args: Vec<String>,
    pub(crate) limit: usize,
    /// The active mouse char-range selection within a row's subject (a drag) —
    /// see [`CharSelection`]. The log's rows aren't line-selectable, so this is
    /// the only selection here.
    pub(crate) char_sel: Option<CharSelection>,
    /// Row a left-drag began on / its byte offset within the subject, while the
    /// button is held; and whether the press landed on a live selection (so the
    /// following click clears it rather than opening the commit).
    pub(crate) drag_anchor: Option<usize>,
    pub(crate) char_anchor: Option<usize>,
    pub(crate) char_click: bool,
}

/// The commit limit encoded in a log arg list (`--max-count=N` or `-nN`), if
/// any — so `+`/`-` know what they're doubling/halving.
fn log_arg_limit(args: &[String]) -> Option<usize> {
    args.iter().find_map(|a| {
        a.strip_prefix("--max-count=")
            .or_else(|| a.strip_prefix("-n"))
            .and_then(|n| n.parse().ok())
    })
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

    /// Whether the log is open in a select mode (picking a commit for a pending
    /// rebase/reword/squash) rather than plain browsing — gates the commit-log
    /// verbs that only make sense in one mode.
    pub(crate) fn log_selecting(&self) -> bool {
        matches!(
            self.log().map(|l| &l.purpose),
            Some(
                LogPurpose::SelectRebaseBase { .. }
                    | LogPurpose::SelectRebaseReword { .. }
                    | LogPurpose::SelectSquash { .. }
            )
        )
    }

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
        let stored = args.clone();
        self.spawn_log(LogPurpose::Browse, move |repo| repo.log_with(&args), cx);
        // Retain the args + limit so `+`/`-` can re-fetch with a new limit.
        let limit = log_arg_limit(&stored).unwrap_or(Self::LOG_LIMIT);
        if let Some(log) = self.log_mut() {
            log.args = stored;
            log.limit = limit;
        }
    }

    /// Re-fetch the browse log with a doubled (`+`) or halved (`-`) commit
    /// limit — magit's `magit-log-{double,half}-commit-limit`. No-op outside a
    /// browse listing.
    pub(crate) fn relimit_log(&mut self, double: bool, cx: &mut Context<Self>) {
        let Some(log) = self.log() else { return };
        if !matches!(log.purpose, LogPurpose::Browse) {
            return;
        }
        let new_limit = if double {
            log.limit.saturating_mul(2)
        } else {
            (log.limit / 2).max(1)
        };
        if new_limit == log.limit {
            return;
        }
        // Rebuild the args with the new limit: drop any existing count flag and
        // put the new one up front (before the revision / `--` paths).
        let mut args: Vec<String> = log
            .args
            .iter()
            .filter(|a| !a.starts_with("--max-count") && !a.starts_with("-n"))
            .cloned()
            .collect();
        args.insert(0, format!("--max-count={new_limit}"));
        self.start_log(args, cx);
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

    /// Fix up / squash into a target commit (the commit at point, else a
    /// log-select), for the chosen [`SquashOp`] — magit's `c f`/`c s`/`c F`/`c
    /// S`. Requires staged changes (git errors otherwise).
    pub(crate) fn fixup_squash_selected(
        &mut self,
        op: SquashOp,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(rev) = self.selected_commit_hash() {
            self.run_fixup_squash(op, rev, args, cx);
        } else {
            let log_args =
                build_log_args(Vec::new(), LogScope::Current, Vec::new(), Self::LOG_LIMIT);
            self.spawn_log(
                LogPurpose::SelectSquash { op, args },
                move |repo| repo.log_with(&log_args),
                cx,
            );
        }
    }

    /// Create the fixup!/squash! commit for `rev`, then autosquash it for the
    /// instant variants. The instant variants rewrite history, so they warn
    /// first when `rev` is already published (like reword/rebase-since); the
    /// plain variants only add a commit, so they run straight away.
    pub(crate) fn run_fixup_squash(
        &mut self,
        op: SquashOp,
        rev: String,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if !op.is_instant() {
            self.do_fixup_squash(op, rev, args, cx);
            return;
        }
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
            this.update(cx, |this, cx| match published {
                None => this.do_fixup_squash(op, rev, args, cx),
                Some(target) => {
                    this.confirm = Some((
                        format!(
                            "{rev} has already been pushed to {target}. Squash into it anyway?"
                        ),
                        Confirm::AutosquashPublished { op, rev, args },
                    ));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Run the fixup/squash commit (and, for instant variants, the autosquash
    /// rebase from the target's parent) on the background path.
    pub(crate) fn do_fixup_squash(
        &mut self,
        op: SquashOp,
        rev: String,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let squash = matches!(op, SquashOp::Squash | SquashOp::InstantSquash);
        let instant = op.is_instant();
        self.run_job(
            op.progress(),
            "Done",
            move |repo| {
                if squash {
                    repo.commit_squash(&rev, &args)?;
                } else {
                    repo.commit_fixup(&rev, &args)?;
                }
                if instant {
                    // Autosquash the just-created commit into its target; the
                    // target and everything above it are in the range.
                    repo.rebase_autosquash(&format!("{rev}^"), &[])?;
                }
                Ok(format!(
                    "{} into {}",
                    if squash { "Squashed" } else { "Fixed up" },
                    &rev[..7.min(rev.len())]
                ))
            },
            cx,
        );
    }

    /// Autosquash existing fixup!/squash! commits (`r f`): an interactive
    /// rebase since the upstream merge base. With no upstream to bound it,
    /// there's no safe automatic base, so point the user at the commit
    /// transient's fixup instead.
    pub(crate) fn autosquash(&mut self, args: Vec<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let base = cx
                .background_executor()
                .spawn(async move { repo.upstream_merge_base() })
                .await;
            this.update(cx, |this, cx| match base {
                Some(base) => this.run_job(
                    "Autosquashing…",
                    "Autosquashed",
                    move |repo| repo.rebase_autosquash(&base, &args),
                    cx,
                ),
                None => this.set_status(
                    "No upstream to autosquash against — use the commit transient's fixup (c f)"
                        .to_string(),
                    false,
                    cx,
                ),
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
            Some(LogPurpose::SelectSquash { op, args }) => {
                let (op, args) = (*op, args.clone());
                self.fixup_squash_selected(op, args, cx);
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
            args: Vec::new(),
            limit: Self::LOG_LIMIT,
            char_sel: None,
            drag_anchor: None,
            char_anchor: None,
            char_click: false,
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
            log.char_sel = None;
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
                    // Always take git's default message: `git revert` would open
                    // an editor otherwise, which our background-git model can't
                    // service (it would hang). Any transient args (--signoff,
                    // --mainline) ride alongside.
                    let mut args = args;
                    if !args.iter().any(|a| a == "--no-edit") {
                        args.push("--no-edit".to_string());
                    }
                    repo.revert_with_args(&rev, &args)
                }
                PickOp::RevertNoCommit => repo.revert_no_commit_with_args(&rev, &args),
            },
            cx,
        );
    }

    /// Copy the full hash of the commit selected in the log.
    pub(crate) fn copy_log_commit(&mut self, cx: &mut Context<Self>) {
        // A mouse char selection (within a row's subject) wins over the hash.
        let selected = self.log().and_then(|l| {
            let sel = l.char_sel.filter(|c| !c.is_empty())?;
            let entry = l.entries.get(sel.row)?;
            Some(sel.slice(&entry.subject).to_string())
        });
        if let Some(text) = selected {
            if let Some(log) = self.log_mut() {
                log.char_sel = None;
            }
            self.copy_to_clipboard(text, cx);
            return;
        }
        let hash = self
            .log()
            .and_then(|l| l.entries.get(l.selected))
            .map(|e| e.hash.clone());
        if let Some(hash) = hash {
            self.copy_to_clipboard(hash, cx);
        }
    }

    /// Reset the current branch to the commit at point (magit's `x`
    /// reset-quickly), `--mixed`: move HEAD and the index there, keeping the
    /// working tree — so the reset-away commits' changes survive as unstaged
    /// edits. Returns to the status view and confirms first (a one-key HEAD
    /// move deserves a check, unlike the deliberate reset transient).
    pub(crate) fn reset_quickly_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rev) = self.selected_commit_hash() else {
            return;
        };
        let short = rev[..7.min(rev.len())].to_string();
        if self.log().is_some() {
            self.close_log(window, cx);
        }
        self.confirm = Some((
            format!("Reset HEAD to {short}? (keeps the working tree)"),
            Confirm::Reset(magritte_core::ResetMode::Mixed, rev),
        ));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::log_arg_limit;

    #[test]
    fn log_arg_limit_reads_max_count_and_n() {
        let a = |s: &str| s.to_string();
        assert_eq!(log_arg_limit(&[a("--max-count=256"), a("HEAD")]), Some(256));
        assert_eq!(log_arg_limit(&[a("-n3"), a("dev")]), Some(3));
        assert_eq!(log_arg_limit(&[a("--reverse"), a("HEAD")]), None);
    }
}

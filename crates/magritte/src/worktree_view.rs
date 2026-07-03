//! The worktree browser (`%`, magit's `magit-worktree`): a scrollable list of
//! the repo's linked worktrees with act-at-point verbs — visit one (open or
//! focus its window), remove one, create one (checkout an existing ref or a new
//! branch into a new directory), and move one. `impl StatusView` like the other
//! view slices.

use std::path::PathBuf;

use gpui::{Context, UniformListScrollHandle, Window};
use magritte_core::Worktree;

use crate::*;

/// Load state, so the body distinguishes still-loading from a load error from a
/// (impossible in practice) empty list.
pub(crate) enum WorktreeLoad {
    Loading,
    Loaded,
    Failed(String),
}

/// The worktree browser screen: the worktrees plus a cursor.
pub(crate) struct WorktreeView {
    pub(crate) worktrees: Vec<Worktree>,
    pub(crate) selected: usize,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) load: WorktreeLoad,
}

impl WorktreeView {
    fn selected(&self) -> Option<&Worktree> {
        self.worktrees.get(self.selected)
    }
}

impl StatusView {
    pub(crate) fn worktree_view(&self) -> Option<&WorktreeView> {
        match &self.screen {
            Screen::Worktree(w) => Some(w),
            _ => None,
        }
    }

    pub(crate) fn worktree_view_mut(&mut self) -> Option<&mut WorktreeView> {
        match &mut self.screen {
            Screen::Worktree(w) => Some(w),
            _ => None,
        }
    }

    /// Open the worktree browser: show it (loading) immediately, then list the
    /// worktrees off the UI thread.
    pub(crate) fn open_worktrees(&mut self, cx: &mut Context<Self>) {
        self.clear_status(cx);
        self.screen = Screen::Worktree(WorktreeView {
            worktrees: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            load: WorktreeLoad::Loading,
        });
        cx.notify();
        self.load_worktrees(cx);
    }

    /// (Re)fetch the worktree list into the open browser. The screen-load
    /// generation guards a superseded load from populating a newer screen.
    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.worktrees() })
                .await;
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) {
                    return;
                }
                if let Some(view) = this.worktree_view_mut() {
                    match result {
                        Ok(mut worktrees) => {
                            // Keep the cursor in range across a reload (after a
                            // removal the list shrinks).
                            view.worktrees = std::mem::take(&mut worktrees);
                            view.selected =
                                view.selected.min(view.worktrees.len().saturating_sub(1));
                            view.load = WorktreeLoad::Loaded;
                        }
                        Err(e) => view.load = WorktreeLoad::Failed(e.to_string()),
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn close_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Move the cursor by `delta`, keeping it in view.
    pub(crate) fn worktrees_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(view) = self.worktree_view_mut() else {
            return;
        };
        if view.worktrees.is_empty() {
            return;
        }
        let last = view.worktrees.len() as isize - 1;
        view.selected = (view.selected as isize + delta).clamp(0, last) as usize;
        view.scroll
            .scroll_to_item(view.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Visit the worktree at point (magit's `magit-worktree-status` / `g`):
    /// open-or-focus its Magritte window. Visiting the current worktree is a
    /// no-op (you're already here).
    pub(crate) fn visit_worktree_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(wt) = self.worktree_view().and_then(WorktreeView::selected) else {
            return;
        };
        if wt.is_current {
            self.set_status("Already on this worktree".to_string(), true, cx);
            return;
        }
        let path = PathBuf::from(&wt.path);
        // Reach the window registry through the global so we can open-or-focus
        // (dedup) rather than always spawning a duplicate window.
        let windows = cx.try_global::<GlobalRepoWindows>().map(|g| g.0.clone());
        match windows {
            Some(windows) => {
                open_or_focus_repo(Some(path), &windows, cx);
            }
            None => {
                open_repo_window(Some(path), cx);
            }
        }
    }

    /// Remove the worktree at point (magit's `magit-worktree-delete` / `k`):
    /// refuse the main and current worktrees, otherwise confirm before removing.
    pub(crate) fn remove_worktree_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(wt) = self.worktree_view().and_then(WorktreeView::selected) else {
            return;
        };
        if wt.is_main {
            self.set_status("Can't remove the main worktree".to_string(), false, cx);
            return;
        }
        if wt.is_current {
            self.set_status("Can't remove the current worktree".to_string(), false, cx);
            return;
        }
        let path = wt.path.clone();
        let name = PathBuf::from(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        self.confirm = Some((
            format!("Remove worktree {name}?"),
            Confirm::RemoveWorktree(path),
        ));
        cx.notify();
    }

    /// Header-button wrappers (the clickable `visit`/`remove` hints), matching
    /// [`Self::key_action`]'s `(window, cx)` callback shape.
    pub(crate) fn visit_worktree_from_button(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.visit_worktree_at_point(cx);
    }

    pub(crate) fn remove_worktree_from_button(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_worktree_at_point(cx);
    }

    /// Carry out a confirmed worktree removal. Runs non-force, so git refuses a
    /// worktree with uncommitted changes (the error surfaces rather than
    /// silently discarding work).
    pub(crate) fn remove_worktree(&mut self, path: String, cx: &mut Context<Self>) {
        self.run_worktree_job(
            "Removing worktree…",
            move |repo| repo.worktree_remove(&path, false),
            cx,
        );
    }

    /// Start `b` — add a worktree checking out an existing ref: pick the ref
    /// (any branch, or type a commit), then the directory.
    pub(crate) fn start_add_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_listed_picker(
            PickerAction::WorktreeAddRef,
            CreateMode::Any,
            Vec::new(),
            |repo| repo.local_branches(),
            window,
            cx,
        );
    }

    /// Start `c` — create a new branch in a new worktree: type the branch name,
    /// then the directory.
    pub(crate) fn start_create_branch_worktree(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(
            PickerAction::WorktreeBranchName,
            Vec::new(),
            CreateMode::Value,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Start `m` — move the worktree at point to a new directory (refuse the
    /// main worktree, like magit). Seeds the prompt with its current path.
    pub(crate) fn start_move_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wt) = self.worktree_view().and_then(WorktreeView::selected) else {
            return;
        };
        if wt.is_main {
            self.set_status("Can't move the main worktree".to_string(), false, cx);
            return;
        }
        let from = wt.path.clone();
        self.open_seeded_dir_picker(
            PickerAction::WorktreeMoveTo { from: from.clone() },
            from,
            window,
            cx,
        );
    }

    /// Open the directory prompt for a create flow, seeded with a sibling of the
    /// current worktree named after the branch/ref (`../<leaf>`).
    pub(crate) fn prompt_worktree_dir(
        &mut self,
        action: PickerAction,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let seed = self.worktree_default_dir(name);
        self.open_seeded_dir_picker(action, seed, window, cx);
    }

    /// A value-entry picker prefilled with `seed` (an editable path) — the same
    /// seeding dance as the `!` command prompt.
    fn open_seeded_dir_picker(
        &mut self,
        action: PickerAction,
        seed: String,
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

    /// A sibling of the current worktree named after the branch/ref's leaf
    /// (`feature/x` → `../x`), the default location for a new worktree.
    fn worktree_default_dir(&self, name: &str) -> String {
        let leaf = name.rsplit('/').next().unwrap_or(name);
        let parent = self
            .repo
            .as_ref()
            .map(|r| r.workdir().to_path_buf())
            .and_then(|w| w.parent().map(|p| p.to_path_buf()));
        match parent {
            Some(parent) => parent.join(leaf).to_string_lossy().into_owned(),
            None => leaf.to_string(),
        }
    }

    pub(crate) fn do_add_worktree(&mut self, dir: String, commit: String, cx: &mut Context<Self>) {
        self.run_worktree_job(
            "Adding worktree…",
            move |repo| repo.worktree_add(&dir, &commit),
            cx,
        );
    }

    pub(crate) fn do_add_branch_worktree(
        &mut self,
        dir: String,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        self.run_worktree_job(
            "Adding worktree…",
            move |repo| repo.worktree_add_branch(&dir, &branch, None),
            cx,
        );
    }

    pub(crate) fn do_move_worktree(&mut self, from: String, to: String, cx: &mut Context<Self>) {
        self.run_worktree_job(
            "Moving worktree…",
            move |repo| repo.worktree_move(&from, &to),
            cx,
        );
    }

    /// Run a `git worktree` mutation off the UI thread, then reload the browser
    /// list (or surface the error). Shared by remove/add/move.
    fn run_worktree_job<F>(&mut self, progress: &str, op: F, cx: &mut Context<Self>)
    where
        F: FnOnce(magritte_core::Repo) -> magritte_core::Result<String> + Send + 'static,
    {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.set_progress(progress.to_string(), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(repo) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(msg) => {
                    this.set_status(msg, true, cx);
                    this.load_worktrees(cx);
                }
                Err(e) => this.report_error(e, cx),
            })
            .ok();
        })
        .detach();
    }
}

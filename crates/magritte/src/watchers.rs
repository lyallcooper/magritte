//! Live-reload plumbing: configuration, appearance, activation, and bounds.
//! Repository changes use `repo_monitor`: its native callback is bounded and
//! filtered, with adaptive polling when native registration is unavailable.

use gpui::{Context, Window};

use crate::*;

fn config_link_target(path: &Path) -> Option<PathBuf> {
    let mut link = std::fs::read_link(path).ok()?;
    if let Ok(target) = path.canonicalize() {
        return Some(target);
    }
    // Follow dangling chains too so creation of the final file can be watched.
    let mut parent = path.parent()?.to_path_buf();
    for _ in 0..40 {
        let target = parent.join(link);
        match std::fs::read_link(&target) {
            Ok(next) => {
                parent = target.parent()?.to_path_buf();
                link = next;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Some(
                    target
                        .parent()?
                        .canonicalize()
                        .ok()?
                        .join(target.file_name()?),
                );
            }
            Err(_) => return None,
        }
    }
    None
}

impl StatusView {
    /// Re-apply config edits and system light/dark changes live, event-driven
    /// (no polling): a native watch on the config file and GPUI's appearance
    /// observer. Needs `window` for the observer, so it runs once the window
    /// exists; the in-app settings screen is the other path. Held subscriptions
    /// keep both alive.
    pub(crate) fn install_watchers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The OS-level window title (Window menu, Dock window list, Mission
        // Control) — the in-window chrome draws its own richer title bar.
        window.set_window_title(
            &self
                .repo_display_name()
                .unwrap_or_else(|| "Magritte".into()),
        );

        // System light/dark: re-theme when the window's appearance flips (only
        // matters when the config follows the system, but `reapply_theme` is
        // cheap and idempotent).
        self._appearance_sub = Some(cx.observe_window_appearance(window, |view, _window, cx| {
            view.reapply_theme(cx);
        }));

        // Focus refresh remains independent from the repository monitor. It is
        // a correctness fallback for watcher loss and changes made while the OS
        // coalesces delivery, and can be disabled separately.
        self._activation_sub = Some(cx.observe_window_activation(window, |view, window, cx| {
            if !(window.is_window_active() && view.config.refresh_on_focus && view.status.is_some())
            {
                return;
            }
            // Refresh immediately on focus, but throttle: skip if we refreshed
            // recently (a manual `g r`, a post-action refresh, an auto-fetch, or
            // a prior focus), so rapid app-switching — or macOS firing several
            // activation events for one focus change — doesn't re-run a full
            // status each time.
            let recent = view
                .last_refresh
                .is_some_and(|t| t.elapsed() < Duration::from_millis(FOCUS_REFRESH_COOLDOWN_MS));
            if !recent {
                view.request_automatic_refresh(
                    RefreshOrigin::Focus,
                    repo_monitor::ChangeScope::RepositoryWide,
                    None,
                    cx,
                );
            }
        }));

        self._window_bounds_sub = Some(cx.observe_window_bounds(window, |view, window, cx| {
            let gen = view.window_bounds_save_gen.bump();
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                this.update_in(cx, |this, window, _cx| {
                    if this.window_bounds_save_gen.is_current(gen) {
                        save_window_state(this.worktree_scope_dir.as_deref(), window, _cx);
                    }
                })
                .ok();
            })
            .detach();
        }));
        save_window_state(self.worktree_scope_dir.as_deref(), window, cx);

        self.install_config_watcher(window, cx);

        // Collect throwaway files a previous run was killed before it could
        // remove. Once per window, and off the UI thread — it resolves the git
        // dir through git and then touches the filesystem.
        if let Some(repo) = self.repo.clone() {
            cx.background_executor()
                .spawn(async move { repo.sweep_stale_temp_files() })
                .detach();
        }
    }

    /// Watch the global config dir — and the repo scope dir when it exists —
    /// re-resolving the merged config / argument defaults on change. Re-run
    /// this when the repo scope dir is created mid-session ("Open repo config"),
    /// since a dir can't be watched before it exists; installing the new
    /// watcher into `_config_watcher` drops the old one.
    pub(crate) fn install_config_watcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config_path) = config::path() else {
            return;
        };
        let Some(dir) = config_path.parent() else {
            return;
        };
        // Match the resolved parent paths reported by the OS while keeping
        // each file's own directory entry visible to the watcher.
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let repo_scope = self
            .repo_scope_dir
            .as_ref()
            .and_then(|dir| dir.canonicalize().ok());
        let entries = [
            config_path.file_name().map(|name| dir.join(name)),
            repo_scope.as_ref().map(|dir| dir.join("config.toml")),
            Some(dir.join(config::TRANSIENT_ARGUMENTS_FILE)),
            repo_scope
                .as_deref()
                .map(config::repo_transient_arguments_path),
        ];
        let link_targets = entries
            .each_ref()
            .map(|path| path.as_deref().and_then(config_link_target));
        let cb_entries = entries.clone();
        let cb_link_targets = link_targets.clone();
        let (tx, rx) = async_channel::unbounded();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let matches = |path: &Option<PathBuf>| {
                    path.as_ref().is_some_and(|path| event.paths.contains(path))
                };
                let entry_changed = cb_entries.iter().any(matches);
                let [global, repo, global_args, repo_args] = std::array::from_fn(|i| {
                    matches(&cb_entries[i]) || matches(&cb_link_targets[i])
                });
                if global || repo || global_args || repo_args {
                    let _ =
                        tx.send_blocking((entry_changed, global || repo, global_args, repo_args));
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        // Watch both the link's directory and the target's directory so
        // replacing either entry by rename still triggers a reload.
        let mut watched_dirs = std::collections::HashSet::new();
        for directory in std::iter::once(dir.as_path())
            .chain(repo_scope.as_deref())
            .chain(link_targets.iter().flatten().filter_map(|p| p.parent()))
        {
            if watched_dirs.insert(directory) {
                let _ = notify::Watcher::watch(
                    &mut watcher,
                    directory,
                    notify::RecursiveMode::NonRecursive,
                );
            }
        }
        self._config_watcher = Some(watcher);

        let repo_config = self
            .repo_scope_dir
            .as_ref()
            .map(|dir| dir.join("config.toml"));
        let repo_args = self
            .repo_scope_dir
            .as_deref()
            .map(config::repo_transient_arguments_path);
        // Applying config can rebuild the open settings form, which needs a window.
        cx.spawn_in(window, async move |this, cx| {
            while let Ok((entry_changed, config_changed, global_args_changed, repo_args_changed)) =
                rx.recv().await
            {
                let updated = this.update_in(cx, |view, window, cx| {
                    let rewatch = entry_changed
                        && entries
                            .each_ref()
                            .map(|path| path.as_deref().and_then(config_link_target))
                            != link_targets;
                    if rewatch {
                        view.install_config_watcher(window, cx);
                    }
                    if config_changed || rewatch {
                        let (cfg, warning) = config::load_merged(repo_config.as_deref());
                        if let Some(warning) = warning {
                            // Keep the live config when an edit makes the file unreadable.
                            view.set_status(warning, false, cx);
                        } else if cfg != view.config {
                            view.apply_config(cfg, window, cx);
                        } else {
                            view.sync_unchanged_config_scopes(cx);
                        }
                    }
                    let mut args_changed = false;
                    if global_args_changed || rewatch {
                        let values = config::load_transient_arguments();
                        args_changed |= values != view.transient_arguments;
                        view.transient_arguments = values;
                    }
                    if repo_args_changed || rewatch {
                        let values = repo_args
                            .as_deref()
                            .map(config::load_transient_arguments_at)
                            .unwrap_or_default();
                        args_changed |= values != view.repo_transient_arguments;
                        view.repo_transient_arguments = values;
                    }
                    if args_changed {
                        view.set_status(
                            "Argument defaults reloaded from disk".to_string(),
                            true,
                            cx,
                        );
                    }
                    rewatch
                });
                if !matches!(updated, Ok(false)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Adopt a freshly-loaded config: store it, re-apply theme/appearance,
    /// update the font, and rebuild the effective keymap — so a `[keymap]` edit
    /// takes effect on save, like the other settings (any unknown id re-warns).
    pub(crate) fn apply_config(
        &mut self,
        cfg: config::Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fetch_changed = self.config.fetch != cfg.fetch;
        let auto_refresh_changed = self.config.auto_refresh != cfg.auto_refresh;
        let update_check_changed = self.config.check_for_updates != cfg.check_for_updates;
        let app_icon_changed = self.config.app_icon != cfg.app_icon;
        let vim_keymap_changed = self.config.vim.keymap != cfg.vim.keymap;
        // Some settings change *fetched data*, not just how it's painted — the
        // title-bar tag segment (and commit ref labels), which status sections
        // are populated, and the recent-commit count. Those need a refresh to
        // take effect live; a repaint alone leaves them stale until the next one.
        let data_changed = self.config.show_tags_in_title_bar != cfg.show_tags_in_title_bar
            || self.config.status != cfg.status;
        self.config = cfg;
        // Keep the global-only copy current too (the watcher fires for both the
        // global and the repo file), so a later settings save writes back the
        // latest global config rather than a stale one.
        self.config_global = config::load_reporting().0;
        if fetch_changed {
            self.start_auto_fetch(cx);
        }
        if auto_refresh_changed {
            self.configure_repository_monitor(cx);
        }
        if update_check_changed {
            self.start_update_checks(cx);
        }
        if app_icon_changed {
            self.apply_app_icon();
        }
        self.font = theme::resolve_font(&self.config, cx);
        self.ui_font = theme::resolve_ui_font(&self.config, cx);
        let (keymap, mut warnings) = build_keymap(&self.config);
        self.keymap = keymap;
        warnings.extend(theme::config_value_warnings(&self.config, cx));
        self.reapply_theme(cx);
        // Toggling Vim mode applies to an already-open commit editor too:
        // turning it on starts in Normal (view-focused, no caret), off
        // returns to plain editing with the input focused.
        let vim_on = self.config.commit_vim_mode;
        let user_map = vim::parse_user_map(&self.config.vim.keymap);
        let toggled = self
            .editor_mut()
            .filter(|ed| vim_on != ed.vim.is_some())
            .map(|ed| {
                ed.vim = vim_on.then(|| Box::new(vim::VimState::with_user_map(user_map.clone())));
                ed.state.clone()
            });
        if let Some(state) = toggled {
            if vim_on {
                self.focus.focus(window, cx);
            } else {
                state.read(cx).focus_handle(cx).focus(window, cx);
            }
        } else if vim_on && vim_keymap_changed {
            // Vim stayed on but `[vim.keymap]` changed: swap the map in
            // place, so the edited binding applies without closing the
            // editor and without resetting mode/pending/Vim-level undo.
            if let Some(vim) = self.editor_mut().and_then(|ed| ed.vim.as_mut()) {
                vim.set_user_map(user_map);
            }
        }
        if data_changed {
            self.refresh_with_origin(
                RefreshOrigin::Config,
                repo_monitor::ChangeScope::RepositoryWide,
                None,
                cx,
            );
        }
        // The open settings form's dropdowns/inputs were built from the old
        // config, so rebuild it in place against the reloaded values rather than
        // leave stale controls. Only external edits reach here; our own in-app
        // saves are filtered out upstream by the unchanged-config guard.
        if self.settings().is_some() {
            self.open_settings(window, cx);
        }
        // Confirm every external reload, on any screen. Problems take priority
        // and stay until dismissed; a clean reload posts a fading confirmation.
        // Since each reload posts a fresh status, fixing the config and saving
        // replaces a prior warning with the confirmation — so a resolved warning
        // clears itself.
        if warnings.is_empty() {
            self.set_status("Settings reloaded from disk".to_string(), true, cx);
        } else {
            self.set_status(warnings.join("; "), false, cx);
        }
    }

    /// Re-apply the current config's theme and refresh everything that bakes in
    /// theme colors. Diff/status/plain row colors are stored in the `Row` model
    /// and the syntax-highlight cache is theme-derived, so a live theme switch
    /// must rebuild both — otherwise the screen keeps the old theme's colors.
    pub(crate) fn reapply_theme(&mut self, cx: &mut Context<Self>) {
        theme::apply_appearance(&self.config, cx);
        self.palette = Palette::from_theme(cx);
        self.recompute_highlights(cx);
        self.rebuild_rows();
        // The flat-diff surfaces (commit/diff view, editor preview) bake theme
        // colors into their Line spans at load time; rebuild those too.
        self.restyle_diff_screens(cx);
        cx.notify();
    }

    /// Mark the start of a background operation. The first concurrent op arms a
    /// short timer; if work is still in flight when it fires, the title-bar
    /// spinner appears — so sub-threshold operations never flash it. Pair every
    /// call with [`end_activity`](Self::end_activity) on completion.
    pub(crate) fn begin_activity(&mut self, cx: &mut Context<Self>) {
        self.activity += 1;
        if self.activity != 1 {
            return; // already counting; one arm-timer covers the whole busy span
        }
        let gen = self.busy_gen.bump();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(BUSY_SPINNER_DELAY_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.busy_gen.is_current(gen) && this.activity > 0 && !this.busy {
                    this.busy = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Mark the end of a background operation. When the last one finishes the
    /// spinner is retired and any pending arm-timer is invalidated.
    pub(crate) fn end_activity(&mut self, cx: &mut Context<Self>) {
        self.activity = self.activity.saturating_sub(1);
        if self.activity == 0 {
            self.busy_gen.bump();
            if self.busy {
                self.busy = false;
                cx.notify();
            }
        }
    }
}

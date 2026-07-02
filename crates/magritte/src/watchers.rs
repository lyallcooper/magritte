//! Live-reload plumbing: the config-file watcher, appearance/activation/bounds
//! subscriptions, applying an externally-changed config to the running view,
//! and the delayed busy-spinner activity counter. `impl StatusView` like the
//! other view slices.

use gpui::{Context, Window};

use crate::*;

impl StatusView {
    /// Re-apply config edits and system light/dark changes live, event-driven
    /// (no polling): a native watch on the config file and GPUI's appearance
    /// observer. Needs `window` for the observer, so it runs once the window
    /// exists; the in-app settings screen is the other path. Held subscriptions
    /// keep both alive.
    pub(crate) fn install_watchers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // System light/dark: re-theme when the window's appearance flips (only
        // matters when the config follows the system, but `reapply_theme` is
        // cheap and idempotent).
        self._appearance_sub = Some(cx.observe_window_appearance(window, |view, _window, cx| {
            view.reapply_theme(cx);
        }));

        // Refresh when the window regains focus, so changes made outside the app
        // show up without a manual `g r` — the same cost as the `g r` you'd press
        // anyway, and opt-out via `refresh_on_focus`. We deliberately don't watch
        // the worktree (a large-repo event/refresh-storm hazard magit also
        // avoids); this is the bounded, on-demand alternative. Skipped until the
        // first status load lands so it doesn't double the startup refresh, and
        // only on the status screen (other screens have their own state).
        self._activation_sub = Some(cx.observe_window_activation(window, |view, window, cx| {
            if !(window.is_window_active()
                && view.config.refresh_on_focus
                && view.status.is_some()
                && matches!(view.screen, Screen::Status))
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
                view.refresh(cx);
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

        // Config file: watch its directory (so atomic save-via-rename, which
        // swaps the inode, still fires), forward matching events over a channel,
        // and re-apply on the UI thread. Watching the dir lets us pick up the
        // sibling transient-arguments.toml too, while ignoring other siblings (e.g.
        // command-usage.toml) by matching the exact paths.
        let Some(config_path) = config::path() else {
            return;
        };
        let Some(dir) = config_path.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        // Canonicalize the dir so the watch target matches the resolved paths the
        // OS reports (e.g. macOS reports `/private/tmp/…` for a `/tmp/…` watch).
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let watch_target = match config_path.file_name() {
            Some(name) => dir.join(name),
            None => return,
        };
        // Which watched file changed — kept distinct so a transient-arguments edit
        // doesn't run the config-reload path (theme rebuild, "Settings reloaded"
        // toast). All reload live, like the config always has.
        enum Changed {
            Config,
            TransientArguments,
            RepoTransientArguments,
        }
        let tv_target =
            config::transient_arguments_path().and_then(|p| p.file_name().map(|n| dir.join(n)));
        // The repo scope's settings dir, if it exists yet (canonicalize fails
        // otherwise) — so we can watch its config.toml / transient-arguments.toml.
        // Created lazily on the first repo-scoped save, so a brand-new repo picks
        // it up next launch; an in-app save updates memory directly anyway.
        let repo_scope = self
            .repo_scope_dir
            .as_ref()
            .and_then(|d| std::fs::canonicalize(d).ok());
        let repo_tv_target = repo_scope
            .as_ref()
            .map(|d| config::repo_transient_arguments_path(d));
        // For re-resolving the merged config: the plain repo config path (its
        // existence is checked at load time, so it works even if created later).
        let repo_config_load = self.repo_scope_dir.as_ref().map(|d| d.join("config.toml"));
        let cb_repo_tv = repo_tv_target.clone();
        let cb_repo_config = repo_scope.as_ref().map(|d| d.join("config.toml"));
        let (tx, rx) = async_channel::unbounded::<Changed>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                // Either config file (global or repo scope) re-resolves the merged
                // config — one path for both.
                if event.paths.contains(&watch_target)
                    || cb_repo_config
                        .as_ref()
                        .is_some_and(|t| event.paths.contains(t))
                {
                    let _ = tx.send_blocking(Changed::Config);
                } else if tv_target.as_ref().is_some_and(|t| event.paths.contains(t)) {
                    let _ = tx.send_blocking(Changed::TransientArguments);
                } else if cb_repo_tv.as_ref().is_some_and(|t| event.paths.contains(t)) {
                    let _ = tx.send_blocking(Changed::RepoTransientArguments);
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        // A missing config dir (no config yet) just means nothing to watch.
        if notify::Watcher::watch(&mut watcher, &dir, notify::RecursiveMode::NonRecursive).is_err()
        {
            return;
        }
        // Also watch the repo's settings dir (a different directory) when present.
        if let Some(repo_scope) = &repo_scope {
            let _ = notify::Watcher::watch(
                &mut watcher,
                repo_scope,
                notify::RecursiveMode::NonRecursive,
            );
        }
        self._config_watcher = Some(watcher);

        // spawn_in so the reload has a Window: applying a config can rebuild the
        // open settings form, whose Select/Input entities need one.
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(changed) = rx.recv().await {
                let updated = match changed {
                    Changed::Config => {
                        let (cfg, warning) = config::load_merged(repo_config_load.as_deref());
                        this.update_in(cx, |view, window, cx| {
                            if let Some(warning) = warning {
                                // The file is now invalid/unreadable. Keep the
                                // live config (don't reset to defaults on a
                                // transient bad edit) and surface why it was
                                // ignored.
                                view.set_status(warning, false, cx);
                            } else if cfg != view.config {
                                // Skip an unchanged config (our own in-app save,
                                // or a no-op external edit).
                                view.apply_config(cfg, window, cx);
                            }
                        })
                    }
                    Changed::TransientArguments => {
                        let values = config::load_transient_arguments();
                        this.update_in(cx, |view, _window, cx| {
                            // Skip our own Ctrl-s save (we update in memory first,
                            // so the reload reads back identical values).
                            if values != view.transient_arguments {
                                view.transient_arguments = values;
                                view.set_status(
                                    "Argument defaults reloaded from disk".to_string(),
                                    true,
                                    cx,
                                );
                            }
                        })
                    }
                    Changed::RepoTransientArguments => {
                        let values = repo_tv_target
                            .as_ref()
                            .map(|p| config::load_transient_arguments_at(p))
                            .unwrap_or_default();
                        this.update_in(cx, |view, _window, cx| {
                            if values != view.repo_transient_arguments {
                                view.repo_transient_arguments = values;
                                view.set_status(
                                    "Argument defaults reloaded from disk".to_string(),
                                    true,
                                    cx,
                                );
                            }
                        })
                    }
                };
                if updated.is_err() {
                    break; // window closed
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
        let update_check_changed = self.config.check_for_updates != cfg.check_for_updates;
        let app_icon_changed = self.config.app_icon != cfg.app_icon;
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
        if data_changed {
            self.refresh(cx);
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

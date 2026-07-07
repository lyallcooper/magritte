//! The async status/diff engine: the priority status refresh, the concurrent
//! per-section auxiliary fetches, lazy per-file diff loads with prefetch, and
//! highlight recomputation. Every fetch is stamped with the view generation so
//! a superseded read can't clobber a newer one; the read-cancel flag actually
//! kills outpaced subprocesses. `impl StatusView` like the other view slices.

use gpui::Context;
use magritte_core::DiffSource;

use crate::*;

impl StatusView {
    /// Recompute the syntax-highlight cache for every loaded diff against the
    /// current theme. Reuses the languages detected at load time, so no files
    /// are re-read.
    pub(crate) fn recompute_highlights(&mut self, cx: &mut Context<Self>) {
        let default = cx.theme().foreground;
        self.diff_cache
            .recompute_highlights(|diff, lang| highlight::highlight_diff(diff, lang, cx, default));
    }

    /// Reload status from scratch, invalidating any in-flight work.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        // Stamp the refresh so the focus-refresh throttle can tell how long it's
        // been since the status was last reloaded (by any path).
        self.last_refresh = Some(std::time::Instant::now());
        // Cancel the previous generation's in-flight reads (kill the processes,
        // not just drop their results) and start a fresh cancel scope.
        self.read_cancel.store(true, Ordering::Relaxed);
        self.read_cancel = Arc::new(AtomicBool::new(false));
        let stamp = self.generation.bump();
        let expanded_diff_keys: HashSet<(DiffSource, String)> = self
            .expanded
            .iter()
            .filter_map(|k| match k {
                FoldKey::File(source, path) => Some((*source, path.clone())),
                FoldKey::Section(_) | FoldKey::Hunk(..) => None,
            })
            .collect();
        self.diff_cache.retain(&expanded_diff_keys);
        self.transient_config_defaults.clear();
        // Hunk indices shift when the diff changes, so don't carry collapse
        // state across a refresh.
        self.collapsed_hunks.clear();
        self.collapse_new_hunks = false;
        // Row indices shift too: a visual/char selection made against the old
        // rows would silently cover different content once the reload lands
        // (auto-fetch and focus-refresh run mid-interaction), so drop it —
        // like magit, which deactivates the region on refresh.
        self.selection.visual = None;
        self.char_sel = None;
        self.error = None;

        if self.read_repo().is_none() {
            // A recorded open failure (git missing) wins over the generic
            // not-a-repository message.
            self.error = Some(
                self.open_error
                    .clone()
                    .unwrap_or_else(|| format!("Not a git repository: {}", self.root.display())),
            );
            self.loading_sections.clear();
            self.rebuild_rows();
            return;
        }

        // The configured sections, so we only fetch what's actually shown.
        let configured: HashSet<SectionId> = self
            .config
            .status
            .section_ids()
            .iter()
            .filter_map(|id| SectionId::from_config_id(id))
            .collect();
        // Mark every configured section (except the conditional pushremote ones)
        // as refreshing. A section already on screen shows a spinner by its
        // header until its fetch lands; a first-load section has no data yet, so
        // it just pops in. The file sections clear when `git status` lands; each
        // auxiliary listing clears when its own fetch does.
        self.loading_sections = configured
            .iter()
            .copied()
            .filter(|s| {
                !matches!(
                    s,
                    SectionId::UnpushedPushremote | SectionId::UnpulledPushremote
                )
            })
            .collect();

        let recent_count = self.config.status.recent_count;
        let want_tags = self.config.show_tags_in_title_bar;
        let upstream_configured =
            configured.contains(&SectionId::Unpushed) || configured.contains(&SectionId::Unpulled);
        let pushremote_configured = configured.contains(&SectionId::UnpushedPushremote)
            || configured.contains(&SectionId::UnpulledPushremote);

        // PRIORITY: `git status` + the in-progress sequence. Renders the main
        // file sections (and the header) the moment it lands, before the
        // auxiliary listings — and kicks off upstream/pushremote divergence
        // afterward, since status tells us whether those targets exist.
        self.spawn_status_fetch(stamp, upstream_configured, pushremote_configured, cx);

        // Auxiliary listings, each its own fetch running concurrently with
        // status when it doesn't need status metadata, so a slow listing can't
        // hold up the main sections or the others. Each pops into place as it
        // lands; the title-bar spinner signals the work.
        if configured.contains(&SectionId::Recent) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Recent],
                cx,
                move |repo| repo.log("HEAD", recent_count).unwrap_or_default(),
                |this, recent| this.status_sections.recent = recent,
            );
        }
        if configured.contains(&SectionId::Stashes) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Stashes],
                cx,
                |repo| repo.stash_list().unwrap_or_default(),
                |this, stashes| this.status_sections.stashes = stashes,
            );
        }
        if configured.contains(&SectionId::Ignored) {
            self.spawn_fetch(
                stamp,
                &[SectionId::Ignored],
                cx,
                |repo| repo.ignored_files().unwrap_or_default(),
                |this, ignored| this.status_sections.ignored = ignored,
            );
        }
        if want_tags {
            // Not a section (it's the title-bar tag segment), so it tracks no
            // section id — it just updates the header when it lands.
            self.spawn_fetch(
                stamp,
                &[],
                cx,
                |repo| repo.tags_around(),
                |this, tags| this.tag_info = tags,
            );
        } else {
            self.tag_info = (None, None);
        }
    }

    /// The priority fetch: `git status` and the in-progress sequence. Renders
    /// the main file sections and header as soon as it lands (restoring the
    /// cursor and re-warming diffs), then — now that the upstream/push targets
    /// are known — fetches those divergence sections only when they can exist.
    pub(crate) fn spawn_status_fetch(
        &mut self,
        stamp: u64,
        upstream_configured: bool,
        pushremote_configured: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        // Capture the cursor's logical position now (before the rebuild) so it
        // can be restored once status lands, rather than left at a stale index.
        let anchor = self.capture_anchor();
        let worktree_git_dir = self.worktree_git_dir.clone();
        let needs = RefreshNeeds {
            push_target: pushremote_configured,
        };
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let (result, sequence, bisect) = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = match worktree_git_dir.as_deref() {
                        Some(dir) => repo.refresh_snapshot_in_dir_with(dir, needs),
                        None => repo.refresh_snapshot_with(needs),
                    };
                    match snapshot {
                        Ok(snapshot) => (Ok(snapshot.status), snapshot.sequence, snapshot.bisect),
                        Err(e) => (Err(e), None, None),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.end_activity(cx);
                if !this.generation.is_current(stamp) {
                    return;
                }
                this.sequence = sequence;
                this.bisect = bisect;
                match result {
                    Ok(status) => {
                        this.status = Some(status);
                        this.error = None;
                    }
                    Err(e) if e.is_git_missing() => {
                        this.error = Some(GIT_MISSING_MESSAGE.to_string())
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                // The file sections are now fresh — drop their refreshing spinner.
                for s in [SectionId::Untracked, SectionId::Unstaged, SectionId::Staged] {
                    this.loading_sections.remove(&s);
                }
                let has_upstream = this
                    .status
                    .as_ref()
                    .is_some_and(|s| s.head.upstream.is_some());
                let triangular = this.status.as_ref().is_some_and(|s| s.head.push.is_some());
                // Divergence sections only exist when their target exists; clear
                // any stale listings otherwise so they don't linger from a prior
                // state (do it before the rebuild so the rows reflect it).
                if upstream_configured && !has_upstream {
                    this.status_sections.unpushed.clear();
                    this.status_sections.unpulled.clear();
                    this.loading_sections.remove(&SectionId::Unpushed);
                    this.loading_sections.remove(&SectionId::Unpulled);
                }
                if pushremote_configured && triangular {
                    this.loading_sections.insert(SectionId::UnpushedPushremote);
                    this.loading_sections.insert(SectionId::UnpulledPushremote);
                } else {
                    this.status_sections.unpushed_pushremote.clear();
                    this.status_sections.unpulled_pushremote.clear();
                }
                this.rebuild_rows();
                this.restore_anchor(anchor);
                // Re-load diffs for any files that were expanded before the
                // refresh cleared them, so they don't get stuck on "Loading…".
                this.reload_expanded_diffs(cx);
                // Warm a bounded set of small diffs so first expand feels instant.
                this.start_prefetch(cx);
                // Now that status resolved the upstream/push targets, fetch the
                // divergence listings; they pop into place (or drop their
                // spinners) on land.
                if upstream_configured && has_upstream {
                    this.spawn_fetch(
                        stamp,
                        &[SectionId::Unpushed, SectionId::Unpulled],
                        cx,
                        |repo| repo.upstream_divergence().unwrap_or_default(),
                        |this, (up, down)| {
                            this.status_sections.unpushed = up;
                            this.status_sections.unpulled = down;
                        },
                    );
                }
                if pushremote_configured && triangular {
                    this.spawn_fetch(
                        stamp,
                        &[SectionId::UnpushedPushremote, SectionId::UnpulledPushremote],
                        cx,
                        |repo| repo.push_divergence().unwrap_or_default(),
                        |this, (up, down)| {
                            this.status_sections.unpushed_pushremote = up;
                            this.status_sections.unpulled_pushremote = down;
                        },
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Spawn one independent background section fetch: run `fetch` off the UI
    /// thread, then on the UI thread (if still the current generation) hand the
    /// result to `apply`, clear `sections` from the refreshing set, and rebuild
    /// — so the section pops in (or drops its spinner). Pairs
    /// `begin_activity`/`end_activity` so the busy spinner accounts for it.
    pub(crate) fn spawn_fetch<T: Send + 'static>(
        &mut self,
        stamp: u64,
        sections: &[SectionId],
        cx: &mut Context<Self>,
        fetch: impl FnOnce(Repo) -> T + Send + 'static,
        apply: impl FnOnce(&mut Self, T) + 'static,
    ) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        let sections = sections.to_vec();
        self.begin_activity(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch(repo) })
                .await;
            this.update(cx, |this, cx| {
                this.end_activity(cx);
                if !this.generation.is_current(stamp) {
                    return;
                }
                apply(this, result);
                for s in &sections {
                    this.loading_sections.remove(s);
                }
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-trigger diff loads for every currently-expanded file.
    pub(crate) fn reload_expanded_diffs(&mut self, cx: &mut Context<Self>) {
        let files: Vec<(DiffSource, String)> = self
            .expanded
            .iter()
            .filter_map(|k| match k {
                FoldKey::File(source, path) => Some((*source, path.clone())),
                FoldKey::Section(_) | FoldKey::Hunk(..) => None,
            })
            .collect();
        for (source, path) in files {
            self.load_diff(source, path, true, cx);
        }
    }

    /// After a refresh, probe changed-line counts (cheap `git diff --numstat`)
    /// off the UI thread, then warm the diffs for a bounded number of small
    /// files so expanding them feels instant. Massive diffs are skipped and
    /// load lazily on explicit expand.
    pub(crate) fn start_prefetch(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.read_repo() else {
            return;
        };
        let generation = self.generation.current();

        cx.spawn(async move |this, cx| {
            let counts = cx
                .background_executor()
                .spawn(async move {
                    let mut all = Vec::new();
                    for source in [DiffSource::Unstaged, DiffSource::Staged] {
                        if let Ok(list) = repo.diff_line_counts(source) {
                            for (path, lines) in list {
                                all.push((source, path, lines));
                            }
                        }
                    }
                    all
                })
                .await;

            this.update(cx, |this, cx| {
                if !this.generation.is_current(generation) {
                    return;
                }
                let mut warmed = 0;
                for (source, path, lines) in counts {
                    if warmed >= PREFETCH_FILE_CAP {
                        break;
                    }
                    if lines > PREFETCH_LINE_CAP {
                        continue;
                    }
                    if this.diff_cache.contains(&(source, path.clone())) {
                        continue;
                    }
                    this.ensure_diff(source, path, cx);
                    warmed += 1;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Kick off a background diff load for a file if not already present.
    pub(crate) fn ensure_diff(&mut self, source: DiffSource, path: String, cx: &mut Context<Self>) {
        self.load_diff(source, path, false, cx);
    }

    /// Kick off a background diff load for a file. A forced reload preserves an
    /// existing loaded diff on screen until the replacement lands, so refreshing
    /// an expanded file never flashes a temporary "Loading…" body.
    pub(crate) fn load_diff(
        &mut self,
        source: DiffSource,
        path: String,
        replace_existing: bool,
        cx: &mut Context<Self>,
    ) {
        let key = (source, path.clone());
        if !replace_existing && self.diff_cache.contains(&key) {
            return;
        }
        let Some(repo) = self.read_repo() else {
            return;
        };
        // Non-default diff context (`+`/`-`) re-fetches with `-U<n>`.
        let repo = if self.diff_context == DEFAULT_DIFF_CONTEXT {
            repo
        } else {
            repo.with_diff_context(self.diff_context)
        };
        if !self.diff_cache.contains(&key) {
            self.diff_cache.set_state(key.clone(), DiffState::Loading);
        }
        // A rename's diff needs the original path in the pathspec — the new
        // path alone would come back as a whole-file addition.
        let orig = self.status.as_ref().and_then(|s| {
            s.entries
                .iter()
                .find(|e| e.path == path)
                .and_then(|e| e.orig_path.clone())
        });
        let generation = self.generation.current();
        self.begin_activity(cx);

        cx.spawn(async move |this, cx| {
            // Off the UI thread: load the diff and resolve the language
            // (extension/filename, falling back to a shebang sniff of the file).
            let (loaded, lang) = cx
                .background_executor()
                .spawn(async move {
                    let diff = repo.diff_path(source, &path, orig.as_deref());
                    let (head, tail) = file_head_tail(&repo.workdir().join(&path));
                    let lang = highlight::detect_language(&path, &head, &tail);
                    (diff, lang)
                })
                .await;
            this.update(cx, |this, cx| {
                this.end_activity(cx);
                if !this.generation.is_current(generation) {
                    return;
                }
                let state = match loaded {
                    Ok(Some(diff)) => DiffState::Loaded(std::sync::Arc::new(diff)),
                    Ok(None) => DiffState::Empty,
                    Err(e) => DiffState::Failed(e.to_string()),
                };
                if let Some(lang) = lang {
                    this.diff_cache.set_lang(key.clone(), lang);
                }
                // Precompute syntax highlighting for the loaded diff.
                if let DiffState::Loaded(diff) = &state {
                    if !diff.is_binary {
                        if let Some(lang) = lang {
                            let default = cx.theme().foreground;
                            let hl = highlight::highlight_diff(diff, lang, cx, default);
                            this.diff_cache.set_highlight(key.clone(), hl);
                        }
                    }
                    // Fold level 3 (hunks closed) extends to diffs that were
                    // still loading when it was applied.
                    if this.collapse_new_hunks {
                        for ix in 0..diff.hunks.len() {
                            this.collapsed_hunks
                                .insert(FoldKey::Hunk(key.0, key.1.clone(), ix));
                        }
                    }
                }
                this.diff_cache.set_state(key, state);
                // A diff finishing load inserts rows; keep the cursor put.
                this.rebuild_preserving_selection();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Show more (`+`) / fewer (`-`) / the default (`0`) context lines around
    /// hunks in the status view — magit's `magit-diff-{more,less,default}-
    /// context`. Each re-fetches the currently-shown diffs at the new width.
    pub(crate) fn diff_context_more(&mut self, cx: &mut Context<Self>) {
        self.set_diff_context(self.diff_context.saturating_add(1), cx);
    }

    pub(crate) fn diff_context_less(&mut self, cx: &mut Context<Self>) {
        self.set_diff_context(self.diff_context.saturating_sub(1), cx);
    }

    pub(crate) fn diff_context_default(&mut self, cx: &mut Context<Self>) {
        self.set_diff_context(DEFAULT_DIFF_CONTEXT, cx);
    }

    fn set_diff_context(&mut self, new: usize, cx: &mut Context<Self>) {
        if new == self.diff_context {
            return;
        }
        self.diff_context = new;
        // Re-fetch every loaded diff at the new context; each rebuilds the rows
        // (preserving the cursor) as it lands.
        for (source, path) in self.diff_cache.keys() {
            self.load_diff(source, path, true, cx);
        }
        self.set_status(format!("Diff context: {new}"), true, cx);
    }
}

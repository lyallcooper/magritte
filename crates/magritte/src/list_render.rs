//! The read-only list screens: the `$` command log, the commit log, the
//! refs/worktree browsers, blame, and the interactive-rebase todo editor.
//! `impl StatusView` like the other view slices.

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement};
use gpui_component::menu::ContextMenuExt;

use crate::render::{offset_at, push_run, push_styled, word_range, StyleRuns};
use crate::*;

fn git_log_elapsed_label(elapsed: std::time::Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

impl StatusView {
    /// Render the git command-log view (magit's `$` process buffer): a header
    /// and a scrollable list of the recent git invocations, newest at the
    /// bottom, each flagged with success/failure.
    pub(crate) fn render_git_log(&self, sv: &ScrollView, view: &Entity<Self>) -> gpui::Div {
        let count = self.git_log_rows().len();

        let body = if count == 0 {
            div()
                .text_color(self.palette.dim)
                .child(SharedString::from("No commands have run yet."))
                .into_any_element()
        } else {
            uniform_list("command-log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    let rows = this.git_log_rows();
                    range
                        .filter_map(|ix| rows.get(ix).map(|r| this.render_git_log_row(r)))
                        .collect::<Vec<_>>()
                }
            })
            .track_scroll(&sv.scroll)
            .flex_grow(1.0)
            .into_any_element()
        };

        self.screen_scaffold()
            .child(
                self.view_header(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(self.palette.section)
                        .child(SharedString::from("Command log")),
                    "close",
                    view,
                ),
            )
            .child(body)
    }

    /// A file's `git blame`: a monospace, scrollable list of annotated lines
    /// (commit · date · author gutter, shown once per commit run, then the line).
    pub(crate) fn render_blame(
        &self,
        sv: &ScrollView,
        path: &str,
        rows: &Rc<Vec<blame_view::BlameRow>>,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let count = rows.len();
        let body = uniform_list("blame-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match &this.screen {
                    Screen::Blame { rows, .. } => {
                        // Size the line-number gutter to the file's widest
                        // number (a fixed width clipped at 5+ digits). Line
                        // numbers ascend, so the last Line row carries the max.
                        let digits = rows
                            .iter()
                            .rev()
                            .find_map(|r| match r {
                                blame_view::BlameRow::Line { line_no, .. } => Some(*line_no),
                                _ => None,
                            })
                            .unwrap_or(0)
                            .to_string()
                            .len();
                        let gutter = px(digits.max(4) as f32 * 8.0 + 6.0);
                        range
                            .filter_map(|ix| rows.get(ix).map(|r| this.render_blame_row(r, gutter)))
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                }
            }
        })
        .track_scroll(&sv.scroll)
        .flex_grow(1.0);

        self.screen_scaffold()
            .child(
                self.view_header(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(self.palette.section)
                        .child(SharedString::from(format!("Blame: {path}"))),
                    "close",
                    view,
                ),
            )
            .child(body)
    }

    fn render_blame_row(&self, row: &blame_view::BlameRow, gutter: gpui::Pixels) -> AnyElement {
        let base = div()
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden();
        match row {
            // A full-width inline annotation above each commit run: sha, author,
            // date, and the commit summary (magit's inline blame).
            blame_view::BlameRow::Annotation {
                short,
                author,
                date,
                summary,
            } => base
                .bg(self.palette.banner)
                .text_color(self.palette.dim)
                .child(SharedString::from(format!(
                    "{short}  {author}  {date}  {summary}"
                )))
                .into_any_element(),
            blame_view::BlameRow::Line { line_no, text } => base
                .child(
                    div()
                        .w(gutter)
                        .flex_shrink_0()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(line_no.to_string())),
                )
                .child(
                    div()
                        .flex_grow(1.0)
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(self.palette.fg)
                        .child(SharedString::from(text.clone())),
                )
                .into_any_element(),
        }
    }

    /// The command log flattened into uniform rows: each invocation becomes a
    /// command row followed by its (dim, indented) stderr lines — git's
    /// progress/error narrative.
    /// The `$` log's flattened rows, memoized: flattening walks every recorded
    /// command and splits all its output lines, so doing it per frame (twice —
    /// count + visible range) scales with session length. The cache is keyed on
    /// the log's monotonic sequence and the show-all toggle.
    pub(crate) fn git_log_rows(&self) -> Rc<Vec<GitLogRow>> {
        let seq = self.repo.as_ref().map(|r| r.command_log_seq()).unwrap_or(0);
        let show_all = self.git_log_show_all();
        if let Some((cached_seq, cached_show, rows)) = self.git_log_cache.borrow().as_ref() {
            if *cached_seq == seq && *cached_show == show_all {
                return rows.clone();
            }
        }
        let rows = Rc::new(self.build_git_log_rows());
        *self.git_log_cache.borrow_mut() = Some((seq, show_all, rows.clone()));
        rows
    }

    fn build_git_log_rows(&self) -> Vec<GitLogRow> {
        let Some(repo) = self.repo.as_ref() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for c in repo.command_log() {
            // Hide the UI's own read-only queries unless asked to show all.
            if !self.git_log_show_all() && c.is_query() {
                continue;
            }
            rows.push(GitLogRow::Command {
                elapsed: git_log_elapsed_label(c.elapsed),
                slow: c.elapsed >= std::time::Duration::from_millis(500),
                very_slow: c.elapsed >= std::time::Duration::from_secs(2),
                prog: c.program.clone().unwrap_or_else(|| "git".to_string()),
                args: c.args.join(" "),
                ok: c.ok,
            });
            // Output, stdout then stderr. stdout is only stored for user `!`
            // commands (internal git calls leave it empty). Progress on stderr
            // often uses '\r' to overwrite; split on both so each update is its
            // own line, and drop the blanks.
            for stream in [&c.stdout, &c.stderr] {
                for line in stream.split(['\n', '\r']) {
                    if !line.trim().is_empty() {
                        rows.push(GitLogRow::Output(line.trim_end().to_string()));
                    }
                }
            }
        }
        rows
    }

    /// One row of the git command log: either a command (success/failure sigil,
    /// dim `git` prefix, arguments reddened on failure) or a dim, indented line
    /// of that command's stderr output.
    pub(crate) fn render_git_log_row(&self, row: &GitLogRow) -> AnyElement {
        match row {
            GitLogRow::Command {
                elapsed,
                slow,
                very_slow,
                prog,
                args,
                ok,
            } => {
                let (sigil, sigil_color) = if *ok {
                    ("✓", self.palette.added)
                } else {
                    ("✗", self.palette.removed)
                };
                let args_color = if *ok {
                    self.palette.fg
                } else {
                    self.palette.removed
                };
                let elapsed_color = if *very_slow {
                    self.palette.removed
                } else if *slow {
                    self.palette.modified
                } else {
                    self.palette.dim
                };
                div()
                    .h(px(self.row_h()))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_shrink_0()
                            .text_color(sigil_color)
                            .child(SharedString::from(sigil)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(44.0))
                                    .flex_shrink_0()
                                    .text_color(elapsed_color)
                                    .child(SharedString::from(elapsed.clone())),
                            )
                            .child(
                                div()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from(prog.clone())),
                            )
                            .child(
                                div()
                                    .text_color(args_color)
                                    .child(SharedString::from(args.clone())),
                            ),
                    )
                    .into_any_element()
            }
            GitLogRow::Output(line) => div()
                .h(px(self.row_h()))
                .w_full()
                .flex()
                .items_center()
                // Indent past the sigil gutter so output nests under its command.
                .pl(px(24.0))
                .text_color(self.palette.dim)
                .child(SharedString::from(line.clone()))
                .into_any_element(),
        }
    }

    /// Render the commit-log view (`l`): a header and a scrollable, navigable
    /// list of commits; the highlighted row opens on Enter or click.
    pub(crate) fn render_log(&self, log: &LogState, view: &Entity<Self>) -> gpui::Div {
        let count = log.entries.len();
        // Note when the listing is capped (against the *current* limit, which
        // `+`/`-` adjust), rather than pretending it's complete.
        let capped = count >= log.limit;

        let note = |text: String, color: Hsla| {
            div()
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &log.load {
            LogLoad::Loading => note("Loading…".to_string(), self.palette.dim),
            LogLoad::Failed(e) => note(format!("log failed: {e}"), self.palette.dim),
            LogLoad::Loaded if count == 0 => note("No commits".to_string(), self.palette.dim),
            LogLoad::Loaded => uniform_list("log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    match this.log() {
                        Some(log) => range
                            .filter_map(|ix| log.entries.get(ix).map(|e| (ix, e)))
                            .map(|(ix, entry)| {
                                this.render_log_row(ix, entry, ix == log.selected, &view)
                            })
                            .collect::<Vec<_>>(),
                        None => Vec::new(),
                    }
                }
            })
            .track_scroll(&log.scroll)
            .flex_grow(1.0)
            // A drag past the list's ends clamps to the first/last commit
            // instead of freezing (see drag_row_beyond_list).
            .on_mouse_move({
                let view = view.clone();
                let scroll = log.scroll.clone();
                move |ev: &gpui::MouseMoveEvent, _window, cx| {
                    if ev.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    view.update(cx, |v, vcx| {
                        let row_h = v.row_h();
                        let Some(log) = v.log_mut() else {
                            return;
                        };
                        let Some(anchor) = log.drag_anchor else {
                            return;
                        };
                        let Some(ix) =
                            drag_row_beyond_list(&scroll, log.entries.len(), ev.position, row_h)
                        else {
                            return;
                        };
                        if ix == anchor {
                            return;
                        }
                        if log.drag().mouse_move(ix, None) {
                            vcx.notify();
                        }
                    });
                }
            })
            .into_any_element(),
        };

        // In select mode the title becomes a prompt and Return confirms the
        // commit; while browsing it's just "Log".
        let selecting = !matches!(log.purpose, LogPurpose::Browse);
        let title = match &log.purpose {
            LogPurpose::SelectRebaseReword { .. } => "Select a commit to reword",
            LogPurpose::SelectRebaseBase { .. } => "Select a commit to rebase since",
            LogPurpose::SelectSquash { op, .. } if op.is_instant() => {
                "Select a commit to squash into"
            }
            LogPurpose::SelectSquash { .. } => "Select a commit to fix up / squash into",
            LogPurpose::Browse => "Log",
        };
        let mut left = div().flex().items_center().gap_3().child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.palette.section)
                .child(SharedString::from(title)),
        );
        if capped {
            // `--reverse` shows the same most-recent N commits oldest-first, so
            // "first N" would read as the oldest; say "last N" there instead.
            let reversed = log.args.iter().any(|a| a == "--reverse");
            let label = if reversed {
                format!("(last {})", log.limit)
            } else {
                format!("(first {})", log.limit)
            };
            left = left.child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(label)),
            );
        }
        // In select mode the user must act (pick a commit): Space inspects the
        // commit, Return picks it, and the close button reads "cancel". While
        // browsing, the `?` menu carries the verbs and the close reads "close".
        let header = if selecting {
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .child(left)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(self.header_action("log-select-view", "view", view))
                        .child(self.header_action("log-open", "select", view))
                        .child(self.header_action("close", "cancel", view)),
                )
        } else {
            self.view_header(left, "close", view)
        };

        self.screen_scaffold().child(header).child(body)
    }

    /// The refs browser (`y`): local branches, remotes, and tags in a scrollable
    /// list with a cursor. Enter checks out the ref at point; the delete key
    /// removes it. Ref names use the app-wide coloring (local blue, remote green,
    /// tag yellow, current branch bold).
    pub(crate) fn render_refs(&self, refs: &RefsView, view: &Entity<Self>) -> gpui::Div {
        let count = refs.rows.len();
        let note = |text: String, color: Hsla| {
            div()
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &refs.load {
            RefsLoad::Loading => note("Loading…".to_string(), self.palette.dim),
            RefsLoad::Failed(e) => note(format!("refs failed: {e}"), self.palette.dim),
            RefsLoad::Loaded if count == 0 => note("No refs".to_string(), self.palette.dim),
            RefsLoad::Loaded => uniform_list("refs-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    match this.refs_view() {
                        Some(refs) => range
                            .filter_map(|ix| refs.rows.get(ix).map(|r| (ix, r)))
                            .map(|(ix, row)| {
                                this.render_refs_row(ix, row, ix == refs.selected, &view)
                            })
                            .collect::<Vec<_>>(),
                        None => Vec::new(),
                    }
                }
            })
            .track_scroll(&refs.scroll)
            .flex_grow(1.0)
            .into_any_element(),
        };

        let title = div()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.palette.section)
            .child(SharedString::from("Refs"));

        self.screen_scaffold()
            .child(self.view_header(title, "close", view))
            .child(body)
    }

    /// One refs-browser row: a dimmed section header, or a ref name colored by
    /// kind (current branch bold, prefixed with a marker), highlighted and
    /// clickable when it's a ref.
    fn render_refs_row(
        &self,
        ix: usize,
        row: &RefsRow,
        selected: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        if let RefsRow::Header(title) = row {
            return div()
                .h(px(self.row_h()))
                .flex()
                .items_center()
                .px_2()
                .pt_1()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.palette.section)
                .child(SharedString::from(*title))
                .into_any_element();
        }
        let (label, kind, current, ahead, behind) = match row {
            RefsRow::Local {
                name,
                current,
                ahead,
                behind,
            } => (
                name.clone(),
                if *current {
                    RefKind::Head
                } else {
                    RefKind::Local
                },
                *current,
                *ahead,
                *behind,
            ),
            RefsRow::Remote(name) => (name.clone(), RefKind::Remote, false, 0, 0),
            RefsRow::Tag(name) => (name.clone(), RefKind::Tag, false, 0, 0),
            RefsRow::Header(_) => unreachable!("handled above"),
        };
        let view = view.clone();
        let mut container = div()
            .id(("refs-row", ix))
            .flex()
            .items_center()
            .gap_2()
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .cursor_pointer()
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(refs) = this.refs_view_mut() {
                        refs.selected = ix;
                    }
                    this.refs_checkout_at_point(window, vcx);
                });
            });
        if selected {
            container = container.bg(self.palette.selection);
        } else {
            container = container.hover(|s| s.bg(self.palette.hover));
        }
        // A leading dot marks the current branch (magit's `@`), kept in the
        // gutter so names still line up.
        container = container.child(
            div()
                .w(px(12.0))
                .flex_shrink_0()
                .text_color(self.palette.branch_local)
                .child(SharedString::from(if current { "●" } else { "" })),
        );
        container = container.child(self.ref_chip(&label, kind));
        // Ahead/behind vs upstream, matching the title bar's `↑ahead ↓behind`.
        if ahead > 0 {
            container = container.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(format!("↑{ahead}"))),
            );
        }
        if behind > 0 {
            container = container.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(format!("↓{behind}"))),
            );
        }
        container.into_any_element()
    }

    /// The worktree browser (`%`): the repo's linked worktrees in a scrollable
    /// list with a cursor. Enter/`g` visits the worktree at point (opens its
    /// window); the delete key removes it.
    pub(crate) fn render_worktrees(&self, wt: &WorktreeView, view: &Entity<Self>) -> gpui::Div {
        let count = wt.worktrees.len();
        let note = |text: String, color: Hsla| {
            div()
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &wt.load {
            WorktreeLoad::Loading => note("Loading…".to_string(), self.palette.dim),
            WorktreeLoad::Failed(e) => note(format!("worktrees failed: {e}"), self.palette.dim),
            WorktreeLoad::Loaded if count == 0 => {
                note("No worktrees".to_string(), self.palette.dim)
            }
            WorktreeLoad::Loaded => uniform_list("worktree-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    match this.worktree_view() {
                        Some(wt) => range
                            .filter_map(|ix| wt.worktrees.get(ix).map(|w| (ix, w)))
                            .map(|(ix, w)| {
                                this.render_worktree_row(ix, w, ix == wt.selected, &view)
                            })
                            .collect::<Vec<_>>(),
                        None => Vec::new(),
                    }
                }
            })
            .track_scroll(&wt.scroll)
            .flex_grow(1.0)
            .into_any_element(),
        };

        let title = div()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.palette.section)
            .child(SharedString::from("Worktrees"));

        self.screen_scaffold()
            .child(self.view_header(title, "close", view))
            .child(body)
    }

    /// One worktree row: a ● current marker, the branch (or detached hash) as a
    /// ref chip, and the path dimmed after it; highlighted and clickable to
    /// visit when it's not the current worktree.
    fn render_worktree_row(
        &self,
        ix: usize,
        wt: &magritte_core::Worktree,
        selected: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut row = div()
            .id(("worktree-row", ix))
            .flex()
            .items_center()
            .gap_2()
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .cursor_pointer()
            .on_click(move |_, _window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(v) = this.worktree_view_mut() {
                        v.selected = ix;
                    }
                    this.visit_worktree_at_point(vcx);
                });
            });
        if selected {
            row = row.bg(self.palette.selection);
        } else {
            row = row.hover(|s| s.bg(self.palette.hover));
        }
        // Current-worktree marker in the gutter (like the refs browser).
        row = row.child(
            div()
                .w(px(12.0))
                .flex_shrink_0()
                .text_color(self.palette.branch_local)
                .child(SharedString::from(if wt.is_current { "●" } else { "" })),
        );
        // The branch as a ref chip, or a detached short hash, or "(bare)".
        if let Some(branch) = &wt.branch {
            let kind = if wt.is_current {
                RefKind::Head
            } else {
                RefKind::Local
            };
            row = row.child(self.ref_chip(branch, kind));
        } else if wt.bare {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("(bare)")),
            );
        } else if let Some(head) = &wt.head {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.modified)
                    .child(SharedString::from(head.clone())),
            );
        }
        // The main-worktree tag, then the path.
        if wt.is_main {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("main")),
            );
        }
        row.child(
            div()
                .text_color(self.palette.dim)
                .child(SharedString::from(wt.path.clone())),
        )
        .into_any_element()
    }

    /// One ref decoration, colored by kind per the app-wide rule: local branch
    /// blue, remote-tracking ref green, tag yellow, current branch bold. A
    /// synced entry (current branch folded with its upstream) shows the
    /// `remote/` prefix green and the branch name in the current-branch color.
    pub(crate) fn ref_chip(&self, label: &str, kind: RefKind) -> AnyElement {
        if kind == RefKind::SyncedHead {
            let (prefix, branch) = label.rsplit_once('/').unwrap_or(("", label));
            return div()
                .flex()
                .items_center()
                .flex_shrink_0()
                .child(
                    div()
                        .text_color(self.palette.branch_remote)
                        .child(SharedString::from(format!("{prefix}/"))),
                )
                .child(
                    div()
                        .text_color(self.palette.branch_local)
                        .font_weight(FontWeight::BOLD)
                        .child(SharedString::from(branch.to_string())),
                )
                .into_any_element();
        }
        let (color, bold) = match kind {
            RefKind::Tag => (self.palette.tag, false),
            RefKind::Head => (self.palette.branch_local, true),
            RefKind::Local => (self.palette.branch_local, false),
            RefKind::Remote => (self.palette.branch_remote, false),
            RefKind::SyncedHead => unreachable!("handled above"),
        };
        let chip = div()
            .flex_shrink_0()
            .text_color(color)
            .child(SharedString::from(label.to_string()));
        if bold {
            chip.font_weight(FontWeight::BOLD).into_any_element()
        } else {
            chip.into_any_element()
        }
    }

    /// A log row's full selectable text — `<hash> <refs…> <subject>` as one
    /// string with the refs as styled tag runs — used by both the renderer and
    /// the copy path so offsets and copied text agree.
    pub(crate) fn log_row_text(
        &self,
        ix: usize,
        entry: &magritte_core::LogEntry,
    ) -> (SharedString, StyleRuns) {
        let (mut text, mut runs) = (String::new(), StyleRuns::new());
        push_run(
            &mut text,
            &mut runs,
            &entry.short_hash,
            self.palette.modified,
        );
        // Decorations were parsed when the entries landed (see `fill_log`);
        // only the styling (theme-dependent) is resolved per frame.
        let parsed = self.log().and_then(|l| l.parsed_refs.get(ix));
        for (label, kind) in parsed.into_iter().flatten() {
            push_run(&mut text, &mut runs, " ", self.palette.fg);
            push_styled(&mut text, &mut runs, label, self.ref_style(*kind));
        }
        push_run(&mut text, &mut runs, " ", self.palette.fg);
        push_run(&mut text, &mut runs, &entry.subject, self.palette.fg);
        (SharedString::from(text), runs)
    }

    /// One commit row: short hash, ref tags, and subject as one selectable
    /// string; highlighted when current, clickable to open its diff.
    pub(crate) fn render_log_row(
        &self,
        ix: usize,
        entry: &magritte_core::LogEntry,
        selected: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        // The char selection within this row's text, if it owns one.
        let sel = self
            .log()
            .and_then(|l| l.char_sel)
            .filter(|c| c.row == ix && !c.is_empty())
            .map(|c| c.range());
        let owns_char = sel.is_some();
        // Whether this row is in the line-wise region (a drag that spanned rows).
        let in_region = self
            .log()
            .and_then(|l| l.visual.map(|a| (a.min(l.selected), a.max(l.selected))))
            .is_some_and(|(lo, hi)| ix >= lo && ix <= hi);
        let mut row = div()
            .id(("log-row", ix))
            .flex()
            .items_center()
            .gap_2()
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .cursor_pointer();
        // A row mid-char-selection keeps the char background visible (no full-row
        // wash / hover over it); a line-wise region uses the region color.
        if in_region {
            row = row.bg(self.palette.visual);
        } else if selected && !owns_char {
            row = row.bg(self.palette.selection);
        } else if !owns_char {
            row = row.hover(|s| s.bg(self.palette.hover));
        }
        // The whole `hash refs subject` as one selectable StyledText (refs as
        // styled runs); its layout drives hit-testing. The date trails, right-
        // aligned, as its own element.
        let (text, runs) = self.log_row_text(ix, entry);
        let row_text = text.clone();
        let (line, layout) = self.selectable_text(text, runs, sel);
        row = row.child(line);
        let (down_layout, move_layout, right_layout) = (layout.clone(), layout.clone(), layout);
        let (v_down, v_move, v_up, v_open, v_right) = (
            view.clone(),
            view.clone(),
            view.clone(),
            view.clone(),
            view.clone(),
        );
        row.child(div().flex_grow(1.0))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(entry.date.clone())),
            )
            .on_mouse_down(
                MouseButton::Left,
                move |ev: &MouseDownEvent, _window, cx: &mut App| {
                    let offset = offset_at(&down_layout, ev.position);
                    v_down.update(cx, |this, vcx| {
                        // Match the other surfaces: a press under an open popup
                        // is a dismiss, not a selection.
                        if this.popup.is_some() {
                            return;
                        }
                        // This press is on a log row (which manages its own
                        // selection), not a click-to-dismiss off the content.
                        this.click_hit_selectable = true;
                        if let Some(log) = this.log_mut() {
                            log.drag().mouse_down(ix, Some(offset));
                            vcx.notify();
                        }
                    });
                },
            )
            .on_mouse_move(move |ev: &gpui::MouseMoveEvent, _window, cx: &mut App| {
                if ev.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let offset = offset_at(&move_layout, ev.position);
                v_move.update(cx, |this, vcx| {
                    if let Some(log) = this.log_mut() {
                        if log.drag().mouse_move(ix, Some(offset)) {
                            vcx.notify();
                        }
                    }
                });
            })
            .on_mouse_up(MouseButton::Left, move |_, _window, cx: &mut App| {
                v_up.update(cx, |this, vcx| {
                    if let Some(log) = this.log_mut() {
                        if log.drag().mouse_up() {
                            vcx.notify();
                        }
                    }
                });
            })
            .on_click(move |ev: &gpui::ClickEvent, _window, cx: &mut App| {
                // A drag selected text; don't also open the commit.
                if let gpui::ClickEvent::Mouse(e) = ev {
                    if (e.up.position.x - e.down.position.x).abs() > px(4.0)
                        || (e.up.position.y - e.down.position.y).abs() > px(4.0)
                    {
                        return;
                    }
                }
                v_open.update(cx, |this, vcx| {
                    if let Some(log) = this.log_mut() {
                        // A click on a row that had a selection just clears it.
                        if log.char_click || log.visual.is_some() {
                            log.char_click = false;
                            log.char_sel = None;
                            log.visual = None;
                            log.selected = ix;
                            vcx.notify();
                            return;
                        }
                        log.char_sel = None;
                        log.selected = ix;
                    }
                    this.open_commit_view(vcx);
                });
            })
            // Right-click selects the word (sha / ref / token) under the cursor;
            // the context menu then offers to copy it.
            .on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, _window, cx: &mut App| {
                    let offset = offset_at(&right_layout, ev.position);
                    let word = word_range(&row_text, offset);
                    v_right.update(cx, |this, vcx| {
                        // This row's Copy uses the selection, not a chrome value.
                        this.pending_copy = None;
                        if let Some(log) = this.log_mut() {
                            // Keep the selection when right-clicking inside it (the
                            // menu copies it); elsewhere clear it and select the word.
                            let inside = if let Some(anchor) = log.visual {
                                let (lo, hi) = (anchor.min(log.selected), anchor.max(log.selected));
                                ix >= lo && ix <= hi
                            } else if let Some(c) = log.char_sel.filter(|c| !c.is_empty()) {
                                let r = c.range();
                                c.row == ix && offset >= r.start && offset <= r.end
                            } else {
                                false
                            };
                            if !inside {
                                log.char_click = false;
                                log.char_sel = (!word.is_empty()).then_some(CharSelection {
                                    row: ix,
                                    anchor: word.start,
                                    cursor: word.end,
                                });
                                log.selected = ix;
                                vcx.notify();
                            }
                        }
                    });
                },
            )
            .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)))
            .into_any_element()
    }

    /// The action keyword + its color for a rebase-todo row.
    pub(crate) fn rebase_action_style(&self, action: RebaseAction) -> (&'static str, Hsla) {
        match action {
            RebaseAction::Pick => ("pick", self.palette.fg),
            RebaseAction::Reword => ("reword", self.palette.modified),
            RebaseAction::Edit => ("edit", self.palette.modified),
            RebaseAction::Squash => ("squash", self.palette.modified),
            RebaseAction::Fixup => ("fixup", self.palette.modified),
            RebaseAction::Drop => ("drop", self.palette.removed),
        }
    }

    /// Render the interactive-rebase todo editor: a header, the editable commit
    /// list (action · hash · subject), and a key-hint footer.
    pub(crate) fn render_rebase_todo(&self, rt: &RebaseTodoView, view: &Entity<Self>) -> gpui::Div {
        let count = rt.steps.len();
        let body = uniform_list("rebase-todo-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.rebase_todo() {
                    Some(rt) => range
                        .filter_map(|ix| rt.steps.get(ix).map(|s| (ix, s)))
                        .map(|(ix, step)| {
                            let selected = ix == rt.selected;
                            let hover = this.palette.hover;
                            let v = view.clone();
                            // Clicking a step moves the cursor to it; rows highlight
                            // on hover (the cursor row already has the selection wash).
                            this.render_rebase_todo_row(rt, step, ix)
                                .id(("rebase-row", ix))
                                .cursor_pointer()
                                .when(!selected, |d| d.hover(move |s| s.bg(hover)))
                                .on_click(move |_, _window, cx: &mut App| {
                                    v.update(cx, |view, vcx| {
                                        if let Some(rt) = view.rebase_todo_mut() {
                                            rt.selected = ix;
                                            vcx.notify();
                                        }
                                    });
                                })
                                .into_any_element()
                        })
                        .collect(),
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&rt.scroll)
        .flex_grow(1.0);

        self.screen_scaffold()
            .child(if rt.confirming_cancel {
                // Unsaved edits to the plan: confirm before discarding them.
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Discard rebase edits?")),
                    )
                    .child(self.key_action(
                        "rebase-todo-discard",
                        "y",
                        "discard",
                        view,
                        Self::discard_rebase_todo,
                    ))
                    .child(self.key_action(
                        "rebase-todo-keep",
                        "n",
                        "keep editing",
                        view,
                        Self::keep_editing_rebase_todo,
                    ))
            } else {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from(match rt.mode {
                                RebaseTodoMode::Start => format!("Rebase {}..HEAD", rt.base),
                                RebaseTodoMode::Edit => "Edit rebase todo".to_string(),
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.header_action(
                                "rebase-todo-run",
                                match rt.mode {
                                    RebaseTodoMode::Start => "start",
                                    RebaseTodoMode::Edit => "save",
                                },
                                view,
                            ))
                            .child(self.header_action("close", "cancel", view)),
                    )
            })
            .child(body)
            .child(
                div()
                    .text_size(px(self.font_px() - 1.0))
                    .text_color(self.palette.dim)
                    .child(SharedString::from(
                        "p pick · r/w reword · e edit · s squash · f fixup · d drop · j/k move · J/K reorder",
                    )),
            )
    }

    /// One row of the rebase-todo editor.
    pub(crate) fn render_rebase_todo_row(
        &self,
        rt: &RebaseTodoView,
        step: &magritte_core::RebaseStep,
        ix: usize,
    ) -> gpui::Div {
        let selected = ix == rt.selected;
        let (keyword, color) = self.rebase_action_style(step.action);
        let dropped = step.action == RebaseAction::Drop;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(self.row_h()))
            .when(selected, |el| el.bg(self.palette.selection))
            .child(
                div()
                    .w(px(56.0))
                    .flex_shrink_0()
                    .text_color(color)
                    .child(SharedString::from(keyword)),
            )
            .child(
                div()
                    .w(px(72.0))
                    .flex_shrink_0()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(step.oid.clone())),
            )
            .child(
                div()
                    .text_color(if dropped {
                        self.palette.dim
                    } else {
                        self.palette.fg
                    })
                    .child(SharedString::from(step.subject.clone())),
            )
    }
}

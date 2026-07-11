//! The read-only list screens: the `$` command log, the commit log, the
//! refs/worktree browsers, blame, and the interactive-rebase todo editor.
//! `impl StatusView` like the other view slices.

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, TextLayout};
use gpui_component::menu::ContextMenuExt;

use crate::render::{
    click_was_drag, color_run, offset_at, push_run, push_styled, word_range, StyleRuns,
};
use crate::*;

fn git_log_elapsed_label(elapsed: std::time::Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

/// The canonical selectable text of a `$`-log row (what render lays out and
/// copy yields): the command line past the sigil gutter, its elapsed column
/// space-padded for the monospace grid; an output line verbatim.
pub(crate) fn git_log_row_text(row: &GitLogRow) -> String {
    match row {
        GitLogRow::Command {
            elapsed,
            prog,
            args,
            ..
        } => format!("{elapsed:<5} {prog} {args}"),
        GitLogRow::Output(line) => line.clone(),
    }
}

/// The canonical selectable text of a blame row: the annotation line, or the
/// file line's content (without the line-number gutter).
pub(crate) fn blame_row_text(row: &blame_view::BlameRow) -> String {
    match row {
        blame_view::BlameRow::Annotation {
            short,
            author,
            date,
            summary,
        } => format!("{short}  {author}  {date}  {summary}"),
        blame_view::BlameRow::Line { text, .. } => text.clone(),
    }
}

impl StatusView {
    /// Render the git command-log view (magit's `$` process buffer): a header
    /// and a scrollable list of the recent git invocations, newest at the
    /// bottom, each flagged with success/failure.
    pub(crate) fn render_git_log(&self, sv: &ScrollView, view: &Entity<Self>) -> gpui::Div {
        let count = self.git_log_rows().len();

        let body = if count == 0 {
            self.load_note("No commands have run yet.")
        } else {
            uniform_list("command-log-rows", count, {
                let view = view.clone();
                move |range, _window, cx| {
                    let this = view.read(cx);
                    let rows = this.git_log_rows();
                    range
                        .filter_map(|ix| {
                            rows.get(ix).map(|r| this.render_git_log_row(ix, r, &view))
                        })
                        .collect::<Vec<_>>()
                }
            })
            .track_scroll(&sv.scroll)
            .flex_grow(1.0)
            .into_any_element()
        };

        // The header carries the query-visibility toggle beside close, so the
        // pager's one command is discoverable without the `?` menu.
        let queries_label = if self.git_log_show_all() {
            "hide queries"
        } else {
            "show queries"
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .child(self.view_title("Command log"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.header_action("git-log-toggle-queries", queries_label, view))
                    .child(self.key_action("close-view", "esc", "close", view, Self::close_screen)),
            );
        self.screen_scaffold().child(header).child(body)
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
                        let gutter = px(this.ch_px(digits.max(4) as f32) + 6.0);
                        range
                            .filter_map(|ix| {
                                rows.get(ix)
                                    .map(|r| this.render_blame_row(ix, r, gutter, &view))
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                }
            }
        })
        .track_scroll(&sv.scroll)
        .flex_grow(1.0);

        self.screen_scaffold()
            .child(self.view_header(self.view_title(format!("Blame: {path}")), "close", view))
            .child(body)
    }

    /// Wire a pager row for mouse text selection: the shared [`DragState`]
    /// transitions against `pager_sel`, with the row's text layout for
    /// pixel↔offset hit-testing.
    pub(crate) fn pager_selectable(
        &self,
        el: gpui::Stateful<gpui::Div>,
        ix: usize,
        layout: TextLayout,
        view: &Entity<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.drag_selectable(
            el,
            ix,
            Some(layout),
            view,
            |v| Some(v.pager_sel.drag()),
            |_, _| true,
            |_, _, _, _| false,
            // A click on a row that had a char selection only clears it.
            |v, _ev, _window, vcx| {
                if v.pager_sel.char_click {
                    v.pager_sel.char_click = false;
                    v.pager_sel.char_sel = None;
                    vcx.notify();
                }
            },
        )
    }

    fn render_blame_row(
        &self,
        ix: usize,
        row: &blame_view::BlameRow,
        gutter: gpui::Pixels,
        view: &Entity<Self>,
    ) -> AnyElement {
        let base = div()
            .id(("blame-row", ix))
            .h(px(self.row_h()))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden();
        let sel = self.pager_sel.char_sel.and_then(|c| c.range_on(ix));
        match row {
            // A full-width inline annotation above each commit run: sha, author,
            // date, and the commit summary (magit's inline blame).
            blame_view::BlameRow::Annotation { .. } => {
                let (styled, layout) = self.selectable_text(blame_row_text(row), Vec::new(), sel);
                self.pager_selectable(
                    base.bg(self.palette.banner)
                        .text_color(self.palette.dim)
                        .child(styled),
                    ix,
                    layout,
                    view,
                )
                .into_any_element()
            }
            blame_view::BlameRow::Line { line_no, .. } => {
                let (styled, layout) = self.selectable_text(blame_row_text(row), Vec::new(), sel);
                self.pager_selectable(
                    base.child(
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
                            .child(styled),
                    ),
                    ix,
                    layout,
                    view,
                )
                .into_any_element()
            }
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
    /// of that command's stderr output. The text past the sigil gutter is one
    /// selectable string (see [`git_log_row_text`]) so it drag-selects/copies.
    pub(crate) fn render_git_log_row(
        &self,
        ix: usize,
        row: &GitLogRow,
        view: &Entity<Self>,
    ) -> AnyElement {
        let sel = self.pager_sel.char_sel.and_then(|c| c.range_on(ix));
        let text = git_log_row_text(row);
        match row {
            GitLogRow::Command {
                slow,
                very_slow,
                prog,
                args,
                ok,
                ..
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
                // Offsets into the canonical text: `elapsed prog args` with the
                // elapsed column space-padded (monospace keeps it aligned).
                let elapsed_end = text.len() - prog.len() - args.len() - 2;
                let prog_end = elapsed_end + 1 + prog.len();
                let runs = vec![
                    color_run(0..elapsed_end, elapsed_color),
                    color_run(elapsed_end..prog_end, self.palette.dim),
                    color_run(prog_end..text.len(), args_color),
                ];
                let (styled, layout) = self.selectable_text(text, runs, sel);
                self.pager_selectable(
                    div()
                        .id(("git-log-row", ix))
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
                        .child(styled),
                    ix,
                    layout,
                    view,
                )
                .into_any_element()
            }
            GitLogRow::Output(_) => {
                let (styled, layout) = self.selectable_text(text, Vec::new(), sel);
                self.pager_selectable(
                    div()
                        .id(("git-log-row", ix))
                        .h(px(self.row_h()))
                        .w_full()
                        .flex()
                        .items_center()
                        // Indent past the sigil gutter so output nests under
                        // its command.
                        .pl(px(24.0))
                        .text_color(self.palette.dim)
                        .child(styled),
                    ix,
                    layout,
                    view,
                )
                .into_any_element()
            }
        }
    }

    /// Render the commit-log view (`l`): a header and a scrollable, navigable
    /// list of commits; the highlighted row opens on Enter or click.
    pub(crate) fn render_log(&self, log: &LogState, view: &Entity<Self>) -> gpui::Div {
        let count = log.entries.len();
        // Note when the listing is capped (against the *current* limit, which
        // `+`/`-` adjust), rather than pretending it's complete.
        let capped = count >= log.limit;

        let body = match &log.load {
            LogLoad::Loading => self.load_note("Loading…"),
            LogLoad::Failed(e) => self.load_note(format!("log failed: {e}")),
            LogLoad::Loaded if count == 0 => self.load_note("No commits"),
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
            .on_mouse_move(Self::on_drag_beyond_list(
                view,
                log.scroll.clone(),
                |v| v.log_mut().map(|l| l.drag()),
                |v| v.log().map_or(0, |l| l.entries.len()),
                |_, ix, _| Some(ix),
            ))
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
        // A path-limited browse log carries its pathspec in the title
        // ("Log -- src/main.rs"), like the diff views.
        let title = if selecting {
            title.to_string()
        } else {
            let paths: Vec<String> = log
                .args
                .iter()
                .skip_while(|a| *a != "--")
                .skip(1)
                .cloned()
                .collect();
            commit_diff_view::diff_title(title, &paths)
        };
        let mut left = div()
            .flex()
            .items_center()
            .gap_3()
            .child(self.view_title(title));
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
        let body = match &refs.load {
            RefsLoad::Loading => self.load_note("Loading…"),
            RefsLoad::Failed(e) => self.load_note(format!("refs failed: {e}")),
            RefsLoad::Loaded if count == 0 => self.load_note("No refs"),
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

        let title = self.view_title("Refs");

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
            return self
                .view_title(*title)
                .h(px(self.row_h()))
                .flex()
                .items_center()
                .px_2()
                .pt_1()
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
        let body = match &wt.load {
            WorktreeLoad::Loading => self.load_note("Loading…"),
            WorktreeLoad::Failed(e) => self.load_note(format!("worktrees failed: {e}")),
            WorktreeLoad::Loaded if count == 0 => self.load_note("No worktrees"),
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

        let title = self.view_title("Worktrees");

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
        let (color, bold) = self.ref_face(kind);
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
        // The char-selection range covering this row (partial on the endpoint
        // rows, whole rows between).
        let sel = self
            .log()
            .and_then(|l| l.char_sel)
            .and_then(|c| c.range_on(ix));
        let owns_char = sel.is_some();
        // Whether this row is in the line-wise region (a drag that spanned
        // rows without text anchoring, or keyboard `v`). A char selection
        // paints per-char instead.
        let in_region = !owns_char
            && self
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
        let right_layout = layout.clone();
        let v_right = view.clone();
        let row = row.child(div().flex_grow(1.0)).child(
            div()
                .flex_shrink_0()
                .text_color(self.palette.dim)
                .child(SharedString::from(entry.date.clone())),
        );
        self.drag_selectable(
            row,
            ix,
            Some(layout),
            view,
            |v| v.log_mut().map(|l| l.drag()),
            |_, _| true,
            |_, _, _, _| false,
            move |this, ev, _window, vcx| {
                // A drag selected text; don't also open the commit.
                if click_was_drag(ev) {
                    return;
                }
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
            },
        )
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
                        } else if let Some(c) = log.char_sel {
                            c.range_on(ix)
                                .is_some_and(|r| offset >= r.start && offset <= r.end)
                        } else {
                            false
                        };
                        if !inside {
                            log.char_click = false;
                            log.char_sel = (!word.is_empty())
                                .then(|| CharSelection::on_row(ix, word.start, word.end));
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
                    .child(self.view_title("Discard rebase edits?"))
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
                    .child(self.view_title(match rt.mode {
                        RebaseTodoMode::Start => format!("Rebase {}..HEAD", rt.base),
                        RebaseTodoMode::Edit => "Edit rebase todo".to_string(),
                    }))
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
            // The footer yields while a popup (the `?` help) floats over the
            // bottom of the window.
            .when(self.popup.is_none(), |el| {
                el.child(self.hint_footer(vec![
                    self.header_action("rebase-todo-pick", "pick", view)
                        .into_any_element(),
                    self.header_action("rebase-todo-reword", "reword", view)
                        .into_any_element(),
                    self.header_action("rebase-todo-edit", "edit", view)
                        .into_any_element(),
                    self.header_action("rebase-todo-squash", "squash", view)
                        .into_any_element(),
                    self.header_action("rebase-todo-fixup", "fixup", view)
                        .into_any_element(),
                    self.header_action("rebase-todo-drop", "drop", view)
                        .into_any_element(),
                    self.header_action_pair(
                        "rebase-todo-reorder-up",
                        "rebase-todo-reorder-down",
                        "reorder",
                        view,
                    )
                    .into_any_element(),
                    self.key_action("footer-help", "?", "help", view, Self::open_help)
                        .into_any_element(),
                ]))
            })
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
                    // "reword", the widest action keyword, plus slack.
                    .w(px(self.ch_px(7.0)))
                    .flex_shrink_0()
                    .text_color(color)
                    .child(SharedString::from(keyword)),
            )
            .child(
                div()
                    // An abbreviated oid plus slack.
                    .w(px(self.ch_px(9.0)))
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

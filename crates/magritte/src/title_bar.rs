//! The custom window title bar (repo name, branch chip, tracking/tag pills,
//! busy spinner) and the in-progress sequence/bisect banners. `impl
//! StatusView` like the other view slices.

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, Window};
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::Sizable;

use crate::render::with_copy_menu;
use crate::*;

/// A block container that middle-truncates its text when the title bar runs
/// out of width (an ellipsis replaces the middle, keeping both ends visible).
/// `floor` is the width it never shrinks below, so a few characters always
/// survive. Must wrap the text directly — inside a flex container the text
/// would keep its intrinsic width and clip instead of truncating.
fn truncatable(floor: gpui::Pixels) -> gpui::Div {
    div()
        .min_w(floor)
        .overflow_x_hidden()
        .whitespace_nowrap()
        .text_ellipsis_middle()
}

/// A title-bar remote-tracking chunk (the upstream or a distinct push target):
/// the direction glyph, the ref name, its ahead/behind divergence, and the
/// transient the name opens on click. See [`StatusView::track_chunk`].
pub(crate) struct TrackRef<'a> {
    /// id prefix for the chunk's clickable sub-elements.
    pub(crate) key: &'a str,
    /// Direction glyph: `⇡` (push) or `⇣` (fetch/pull).
    pub(crate) glyph: &'a str,
    /// The remote-tracking ref name.
    pub(crate) name: &'a str,
    /// `(ahead, behind)` commit counts vs this ref.
    pub(crate) divergence: (u32, u32),
    /// Hover tooltip describing the ref.
    pub(crate) tip: &'static str,
    /// The transient the ref name opens on click (matching its glyph: push/pull).
    pub(crate) command: &'static str,
}

impl StatusView {
    /// Truncation floor for a [`truncatable`] name: its estimated natural
    /// width — so names shorter than the cap aren't padded wider — capped at
    /// 6 characters, enough to keep a recognizable stub visible when the bar
    /// is squeezed hard. `font_px` is the size the name renders at.
    fn name_floor(&self, name: &str, font_px: f32) -> gpui::Pixels {
        px((font_px * 0.62 * 6.0_f32.min(name.chars().count() as f32)).round())
    }

    /// Render a dialog heading from styled spans, with branch/ref names set off
    /// from the surrounding words as a subtly tinted, medium-weight chip so
    /// they're easy to pick out — e.g. the `main` in "Push main to". `base` is
    /// the color for the plain text (the heading vs. group-header convention).
    pub(crate) fn render_title(&self, spans: &[TitleSpan], base: Hsla) -> gpui::Div {
        let mut row = div().flex().items_center();
        for span in spans {
            row = match span {
                TitleSpan::Text(t) => {
                    row.child(div().text_color(base).child(SharedString::from(t.clone())))
                }
                TitleSpan::Accent(b) => row.child(self.branch_chip(b)),
            };
        }
        row
    }

    /// A branch/ref name as a subtly tinted, medium-weight chip — set off from
    /// surrounding text. Used in dialog titles and the repo header lines.
    pub(crate) fn branch_chip(&self, name: &str) -> gpui::Div {
        div()
            .px(px(5.0))
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            // Branch/ref names are identifiers — keep them monospace even when
            // the surrounding chrome uses a proportional UI font.
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .child(SharedString::from(name.to_string()))
    }

    /// The title-bar branch as a chip (click opens the branch transient,
    /// right-click copies the name).
    pub(crate) fn render_branch_chip(&self, view: &Entity<Self>, branch: &str) -> impl IntoElement {
        let branch_click = view.clone();
        let tip_font = self.font.clone();
        let tip_name = SharedString::from(branch.to_string());
        let dim = self.palette.dim;
        // The branch name chip: left-click opens the branch transient; right-click
        // copies the name via the shared Copy menu. The name middle-truncates
        // when the bar is short on width; the tooltip carries the full name.
        let chip = div()
            .id("titlebar-branch")
            .relative()
            .flex()
            .items_center()
            .min_w_0()
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .px(px(5.0))
            .child(track_target("titlebar-branch"))
            .child(
                truncatable(self.name_floor(branch, self.font_px()))
                    .child(SharedString::from(branch.to_string())),
            )
            .when(!self.ctx_menu_open, |d| {
                d.tooltip(move |window, cx| {
                    let (font, name) = (tip_font.clone(), tip_name.clone());
                    Tooltip::element(move |_, _| {
                        div()
                            .max_w(px(480.0))
                            .font_family(font.clone())
                            .child("Current branch")
                            .child(div().text_color(dim).child(name.clone()))
                    })
                    .build(window, cx)
                })
                .tooltip_show_delay(Duration::from_millis(400))
            })
            .on_click(move |_, window, cx: &mut App| {
                branch_click.update(cx, |v, vcx| v.invoke_command("branch", window, vcx));
            });
        with_copy_menu(chip, view, branch.to_string())
    }

    /// The in-progress sequence banner (merge/rebase/cherry-pick/revert/am):
    /// a heading, the plan steps, and the available continue/skip/abort
    /// controls. Sits above the status list so it's visible while resolving.
    pub(crate) fn render_sequence_banner(&self, seq: &Sequence, view: &Entity<Self>) -> gpui::Div {
        // The plan steps (capped so a long rebase todo can't dominate).
        const MAX_STEPS: usize = 8;
        let mut steps = div().flex().flex_col().gap_0().pl(px(2.0));
        for step in seq.steps.iter().take(MAX_STEPS) {
            let mut line = format!("{} ", step.action);
            if let Some(oid) = &step.oid {
                line.push_str(oid);
                line.push(' ');
            }
            line.push_str(&step.subject);
            steps = steps.child(
                div()
                    .text_color(self.palette.dim)
                    .font_family(self.font.clone())
                    .child(SharedString::from(line)),
            );
        }
        if seq.steps.len() > MAX_STEPS {
            steps = steps.child(div().text_color(self.palette.dim).child(SharedString::from(
                format!("… +{} more", seq.steps.len() - MAX_STEPS),
            )));
        }

        // Continue / skip / abort as keycap+label buttons. The keycap shows the
        // *full* keystroke that drives it from the status view — the prefix that
        // opens this sequence's transient plus the action key (so rebase continue
        // is `r r`, not a bare `r`, which would collide with "open rebase"). Only
        // rebase/merge have a status-view prefix; cherry-pick/revert/am are driven
        // only by clicking these buttons, so they show no (misleading) keycap.
        let prefix = match seq.kind {
            SequenceKind::Rebase => Some("r"),
            SequenceKind::Merge => Some("m"),
            SequenceKind::CherryPick | SequenceKind::Revert | SequenceKind::Am => None,
        };
        let keys = |action_key: &str| prefix.map(|p| format!("{p} {action_key}"));
        let mut actions = div().flex().items_center().gap_3();
        if seq.kind.can_continue() {
            actions = actions.child(self.seq_action(
                "seq-continue",
                keys("r"),
                "continue",
                view,
                Self::sequence_continue,
            ));
        }
        if seq.kind.can_skip() {
            actions = actions.child(self.seq_action(
                "seq-skip",
                keys("s"),
                "skip",
                view,
                Self::sequence_skip,
            ));
        }
        // A merge is finished by committing the resolved index (`m m`, like
        // magit's in-progress "Commit merge"), not by `--continue`.
        if matches!(seq.kind, SequenceKind::Merge) {
            actions = actions.child(self.seq_action(
                "seq-commit-merge",
                keys("m"),
                "commit merge",
                view,
                Self::merge_commit_action,
            ));
        }
        actions = actions.child(self.seq_action(
            "seq-abort",
            keys("a"),
            "abort",
            view,
            Self::sequence_abort,
        ));

        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .px_3()
            .py_2()
            .bg(self.palette.banner)
            .border_b_1()
            .border_color(self.palette.border)
            .child(self.view_title(seq.heading.clone()))
            .child(steps)
            .child(actions)
    }

    /// The in-progress bisect banner: a heading, the recorded good/bad/skip
    /// decisions (tail), and the mark/reset controls (`B g`/`B b`/`B s`/`B r`).
    pub(crate) fn render_bisect_banner(&self, bisect: &Bisect, view: &Entity<Self>) -> gpui::Div {
        const MAX: usize = 6;
        let mut lines = div().flex().flex_col().gap_0().pl(px(2.0));
        let skipped = bisect.decisions.len().saturating_sub(MAX);
        for d in bisect.decisions.iter().skip(skipped) {
            lines = lines.child(
                div()
                    .text_color(self.palette.dim)
                    .font_family(self.font.clone())
                    .child(SharedString::from(d.clone())),
            );
        }
        let actions = div()
            .flex()
            .items_center()
            .gap_3()
            .child(self.seq_action(
                "bisect-good",
                Some("B g".to_string()),
                "good",
                view,
                Self::bisect_good_action,
            ))
            .child(self.seq_action(
                "bisect-bad",
                Some("B b".to_string()),
                "bad",
                view,
                Self::bisect_bad_action,
            ))
            .child(self.seq_action(
                "bisect-skip",
                Some("B s".to_string()),
                "skip",
                view,
                Self::bisect_skip_action,
            ))
            .child(self.seq_action(
                "bisect-reset",
                Some("B r".to_string()),
                "reset",
                view,
                Self::bisect_reset_action,
            ));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .px_3()
            .py_2()
            .bg(self.palette.banner)
            .border_b_1()
            .border_color(self.palette.border)
            .child(self.view_title("Bisecting"))
            .child(lines)
            .child(actions)
    }

    /// A sequence-banner action button: keycap + label, clickable to run
    /// `action`. `keys` is the full keystroke that triggers it from the status
    /// view (e.g. `r r`); when `None` (a sequence with no status-view prefix)
    /// the button is click-only, with no misleading keycap.
    pub(crate) fn seq_action(
        &self,
        id: &'static str,
        keys: Option<String>,
        label: &'static str,
        view: &Entity<Self>,
        action: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let view = view.clone();
        let mut row = self.hint_row(id).gap_1();
        if let Some(keys) = keys {
            row = row.child(kbd::key_chip(
                &keys,
                self.palette.dim,
                &self.font,
                &self.system_ui_font,
            ));
        }
        row.child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// A small, subtle count badge for the title bar (ahead/behind): a rounded
    /// `color`-tinted pill with `color` text.
    pub(crate) fn count_pill(&self, text: impl Into<SharedString>, color: Hsla) -> gpui::Div {
        div()
            .px(px(4.0))
            .rounded(px(6.0))
            .bg(with_alpha(color, 0.15))
            .text_size(px(self.font_px() - 2.0))
            .text_color(color)
            .child(text.into())
    }

    /// Wrap a title-bar item with a hover tooltip spelling out what it is (the
    /// bar's glyphs and counts are terse). Needs a stable `id` for the hitbox.
    /// `shrink` lets the flex layout squeeze the item below its content width
    /// when the bar runs out of room — set it only when the child truncates
    /// gracefully (a [`truncatable`] name); everything else keeps its size.
    pub(crate) fn titlebar_tip(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        tip: impl Into<SharedString>,
        copy: Option<String>,
        shrink: bool,
        child: impl IntoElement,
    ) -> impl IntoElement {
        self.titlebar_item(view, id.into(), tip.into(), copy, shrink, child, |d| d)
    }

    /// Shared core of [`titlebar_tip`]/[`titlebar_action`]: an id'd wrapper
    /// with the hover tooltip and the optional right-click Copy menu; `decorate`
    /// adds the action variant's click affordances to the wrapper itself, so
    /// their hitbox isn't occluded. `context_menu` changes the element type,
    /// so the copy variants branch into `AnyElement`.
    ///
    /// [`titlebar_tip`]: Self::titlebar_tip
    /// [`titlebar_action`]: Self::titlebar_action
    #[allow(clippy::too_many_arguments)]
    fn titlebar_item(
        &self,
        view: &Entity<Self>,
        id: SharedString,
        tip: SharedString,
        copy: Option<String>,
        shrink: bool,
        child: impl IntoElement,
        decorate: impl FnOnce(gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div>,
    ) -> gpui::AnyElement {
        let font = self.font.clone();
        // The bar may middle-truncate the item's text, so when the item carries
        // a value (the same one the Copy menu offers) the tooltip spells out
        // the full version under the description.
        // Cap the tooltip's width so a long value hard-wraps (the line wrapper
        // falls back to char-boundary breaks on solid ref names) instead of
        // running off the window edge.
        let value = copy.clone().map(SharedString::from);
        let dim = self.palette.dim;
        let base = decorate(div().id(id).when(shrink, |d| d.min_w_0()))
            .child(child)
            // Suppress the tooltip while a Copy menu is open so it can't paint
            // over the menu.
            .when(!self.ctx_menu_open, |d| {
                d.tooltip(move |window, cx| {
                    let (font, tip, value) = (font.clone(), tip.clone(), value.clone());
                    Tooltip::element(move |_, _| {
                        div()
                            .max_w(px(480.0))
                            .font_family(font.clone())
                            .child(tip.clone())
                            .when_some(value.clone(), |d, value| {
                                d.child(div().text_color(dim).child(value))
                            })
                    })
                    .build(window, cx)
                })
                .tooltip_show_delay(Duration::from_millis(400))
            });
        match copy {
            Some(value) => with_copy_menu(base, view, value).into_any_element(),
            None => base.into_any_element(),
        }
    }

    pub(crate) fn track_chunk(&self, view: &Entity<Self>, r: TrackRef) -> gpui::Div {
        let TrackRef {
            key,
            glyph,
            name,
            divergence: (ahead, behind),
            tip,
            command,
        } = r;
        let mut chunk = div()
            .flex()
            .items_center()
            .gap_1()
            .min_w_0()
            .font_family(self.font.clone())
            // Glyph (dim) and ref name (magit's green branch-remote face) sit
            // tight together; the ahead/behind chips follow with a gap. Right-
            // click the name to copy the ref.
            .child(
                self.titlebar_action(
                    view,
                    format!("{key}-name"),
                    command,
                    tip,
                    Some(name.to_string()),
                    true,
                    div()
                        .flex()
                        .items_center()
                        .min_w_0()
                        .when(!glyph.is_empty(), |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from(glyph.to_string())),
                            )
                        })
                        .child(
                            truncatable(self.name_floor(name, self.font_px()))
                                .text_color(self.palette.branch_remote)
                                .child(SharedString::from(name.to_string())),
                        ),
                ),
            );
        if ahead > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-ahead"),
                "push",
                "Unpushed commits",
                None,
                false,
                self.count_pill(format!("↑{ahead}"), self.palette.branch_remote),
            ));
        }
        if behind > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-behind"),
                "pull",
                "Unpulled commits",
                None,
                false,
                self.count_pill(format!("↓{behind}"), self.palette.branch_remote),
            ));
        }
        chunk
    }

    /// A clickable title-bar element that runs the registry command `command`
    /// (the branch chip → "branch", an ahead count → "push", a behind count →
    /// "pull"), with a hover tooltip describing it. The pointer cursor and
    /// tooltip signal it's actionable — the semantic text color is left intact
    /// (a hover recolor would fire only on items whose text has no explicit
    /// color, so it read inconsistently across the bar). When `copy` is set,
    /// right-clicking offers a `Copy` context menu for that value.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn titlebar_action(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        command: &'static str,
        tip: impl Into<SharedString>,
        copy: Option<String>,
        shrink: bool,
        child: impl IntoElement,
    ) -> impl IntoElement {
        let click_view = view.clone();
        let id = id.into();
        self.titlebar_item(
            view,
            id.clone(),
            tip.into(),
            copy,
            shrink,
            child,
            move |d| {
                d.relative()
                    .cursor_pointer()
                    .child(track_target(id))
                    .on_click(move |_, window, cx: &mut App| {
                        click_view.update(cx, |v, vcx| v.invoke_command(command, window, vcx));
                    })
            },
        )
    }

    /// The custom window title bar: the repo name, the current branch as a chip,
    /// its ahead/behind vs upstream, and a dirty marker — styled to match the
    /// app (so it reads as chrome, not the OS bar). The `TitleBar` component
    /// handles traffic-light spacing, dragging, and (off-macOS) window controls.
    /// The repo directory's name — the window's identity in the custom title
    /// bar and the OS-level window title (Window menu, Dock, Mission Control).
    pub(crate) fn repo_display_name(&self) -> Option<String> {
        self.repo
            .as_ref()
            .map(|r| r.workdir())
            .unwrap_or(self.root.as_path())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    pub(crate) fn render_title_bar(
        &self,
        view: &Entity<Self>,
        window: &Window,
    ) -> impl IntoElement {
        let repo_name = self.repo_display_name().unwrap_or_else(|| "—".to_string());

        // Cap the info row at the width actually available so long names
        // middle-truncate instead of pushing the bar past the window edge. A
        // `min_w_0` chain alone can't do it: intrinsic min-content widths
        // propagate through the TitleBar's internal flex row, which we can't
        // style. 80 mirrors the TitleBar's macOS traffic-light padding; the
        // rest leaves room for the busy-spinner overlay.
        let avail = window.viewport_size().width - px(80.0 + 40.0);
        let mut info = div().flex().items_center().gap_2().max_w(avail).child(
            self.titlebar_tip(
                view,
                "titlebar-repo",
                "Repository",
                Some(repo_name.clone()),
                false,
                // The repo name is the window's identity and usually short, so
                // it never gets squeezed by its long neighbors (its wrapper
                // doesn't shrink, so no floor either) — it only self-caps when
                // it is itself unreasonably long.
                truncatable(px(0.0))
                    .max_w(px(16.0 * self.font_px()))
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(repo_name)),
            ),
        );

        if let Some(status) = &self.status {
            let head = &status.head;
            // A real branch: a divided chip (name opens the branch transient,
            // the button copies the name). Detached: a plain clickable chip.
            info = info.child(match &head.branch {
                Some(branch) => self.render_branch_chip(view, branch).into_any_element(),
                None => self
                    .titlebar_action(
                        view,
                        "titlebar-branch",
                        "branch",
                        "Detached HEAD",
                        None,
                        false,
                        self.branch_chip("detached"),
                    )
                    .into_any_element(),
            });

            // Tracking: the upstream, plus a distinct push target when present
            // (a triangular workflow). When the push target equals the upstream,
            // the core leaves `head.push` unset, so we show a single entry.
            match (&head.push, &head.upstream) {
                // A distinct push target (triangular workflow): push ⇡, upstream ⇣.
                (Some(push), upstream) => {
                    info = info.child(self.track_chunk(
                        view,
                        TrackRef {
                            key: "push",
                            glyph: "⇡",
                            name: push,
                            divergence: (head.push_ahead, head.push_behind),
                            tip: "Push target",
                            command: "push",
                        },
                    ));
                    if let Some(up) = upstream {
                        info = info.child(self.track_chunk(
                            view,
                            TrackRef {
                                key: "up",
                                glyph: "⇣",
                                name: up,
                                divergence: (head.ahead, head.behind),
                                tip: "Upstream branch",
                                command: "pull",
                            },
                        ));
                    }
                }
                // Push and upstream are the same remote: one chunk, the push
                // arrow (⇡), since there's no separate upstream to distinguish.
                (None, Some(up)) => {
                    info = info.child(self.track_chunk(
                        view,
                        TrackRef {
                            key: "up",
                            glyph: "⇡",
                            name: up,
                            divergence: (head.ahead, head.behind),
                            tip: "Upstream branch",
                            command: "push",
                        },
                    ));
                }
                (None, None) => {}
            }

            // The nearest reachable tag: "v1 (5)", magit's current-tag header.
            // (Tags merely *containing* HEAD — e.g. on unpulled upstream
            // commits — aren't shown; a second tag here read as noise.)
            // Gate on the live config too, so toggling `show_tags_in_title_bar`
            // off hides the segment immediately (not just after the next
            // status refresh clears `tag_info`).
            if self.config.show_tags_in_title_bar {
                if let Some((name, count)) = &self.tag_info {
                    // A tag-tinted pill: the name (click opens the tag
                    // transient) and, divided off like the branch chip's copy
                    // button, the commits-since count.
                    let mut pill = div()
                        .flex()
                        .items_center()
                        .min_w_0()
                        .rounded(px(6.0))
                        .bg(with_alpha(self.palette.tag, 0.15))
                        .text_size(px(self.font_px() - 2.0))
                        .text_color(self.palette.tag)
                        .child(
                            self.titlebar_action(
                                view,
                                "titlebar-tag".to_string(),
                                "tag",
                                "Nearest tag",
                                Some(name.clone()),
                                true,
                                truncatable(self.name_floor(name, self.font_px() - 2.0))
                                    .px(px(5.0))
                                    .child(SharedString::from(name.clone())),
                            ),
                        );
                    if *count > 0 {
                        pill = pill
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(1.0))
                                    .h(px(12.0))
                                    .bg(with_alpha(self.palette.tag, 0.4)),
                            )
                            .child(
                                self.titlebar_tip(
                                    view,
                                    "titlebar-tag-count".to_string(),
                                    "Commits since tag",
                                    None,
                                    false,
                                    div()
                                        .px(px(4.0))
                                        .child(SharedString::from(count.to_string())),
                                ),
                            );
                    }
                    info = info.child(div().flex().items_center().gap_1().min_w_0().child(pill));
                }
            }

            if !status.is_clean() {
                // Marks uncommitted changes in the working tree.
                info = info.child(self.titlebar_tip(
                    view,
                    "titlebar-dirty",
                    "Working tree dirty",
                    None,
                    false,
                    div().text_color(self.palette.modified).child("○"),
                ));
            }
        }

        let bar = gpui_component::TitleBar::new()
            .bg(self.palette.bg)
            .border_color(self.palette.border)
            .child(info);

        // A spinner for background activity that outlasts the delay threshold.
        // It lives outside the title bar's flex flow, absolutely anchored to
        // the window's right edge, so wide bar content can't push it off-screen
        // — it paints on top instead. A subtle rounded background chip makes it
        // read as a deliberate indicator rather than blending into the bar.
        div()
            .relative()
            .flex_shrink_0()
            .child(bar)
            .when(self.busy, |wrapper| {
                wrapper.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_3()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .p_1()
                                .rounded(px(4.0))
                                .bg(self.palette.selection)
                                .child(Spinner::new().small().color(self.palette.fg)),
                        ),
                )
            })
    }
}

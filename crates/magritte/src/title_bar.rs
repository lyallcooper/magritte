//! The custom window title bar (repo name, branch chip, tracking/tag pills,
//! busy spinner) and the in-progress sequence/bisect banners. `impl
//! StatusView` like the other view slices.

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, Window};
use gpui_component::menu::ContextMenuExt;
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::Sizable;

use crate::*;

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
                TitleSpan::Branch(b) => row.child(self.branch_chip(b)),
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
        let branch_copy = view.clone();
        let value = branch.to_string();
        let tip_font = self.font.clone();
        // The branch name chip: left-click opens the branch transient; right-click
        // copies the name via the shared Copy menu. The right-down + context menu
        // sit on this same interactive element so its hitbox isn't occluded.
        div()
            .id("titlebar-branch")
            .relative()
            .flex()
            .items_center()
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .px(px(5.0))
            .child(track_target("titlebar-branch"))
            .child(SharedString::from(branch.to_string()))
            .when(!self.ctx_menu_open, |d| {
                d.tooltip(move |window, cx| {
                    let font = tip_font.clone();
                    Tooltip::element(move |_, _| {
                        div().font_family(font.clone()).child("Current branch")
                    })
                    .build(window, cx)
                })
                .tooltip_show_delay(Duration::from_millis(400))
            })
            .on_click(move |_, window, cx: &mut App| {
                branch_click.update(cx, |v, vcx| v.invoke_command("branch", window, vcx));
            })
            .on_mouse_down(MouseButton::Right, move |_, _window, cx: &mut App| {
                let value = value.clone();
                branch_copy.update(cx, |v, vcx| {
                    v.pending_copy = Some(value);
                    v.ctx_menu_open = true;
                    vcx.notify();
                });
            })
            .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)))
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
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.palette.section)
                    .child(SharedString::from(seq.heading.clone())),
            )
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
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.palette.section)
                    .child(SharedString::from("Bisecting")),
            )
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
        let mut row = div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id));
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
            .text_size(px(11.0))
            .text_color(color)
            .child(text.into())
    }

    /// Wrap a title-bar item with a hover tooltip spelling out what it is (the
    /// bar's glyphs and counts are terse). Needs a stable `id` for the hitbox.
    pub(crate) fn titlebar_tip(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        tip: impl Into<SharedString>,
        copy: Option<String>,
        child: impl IntoElement,
    ) -> impl IntoElement {
        let copy_view = view.clone();
        let font = self.font.clone();
        let tip = tip.into();
        let base = div()
            .id(id.into())
            .child(child)
            // Suppress the tooltip while a Copy menu is open so it can't paint
            // over the menu.
            .when(!self.ctx_menu_open, |d| {
                d.tooltip(move |window, cx| {
                    let (font, tip) = (font.clone(), tip.clone());
                    Tooltip::element(move |_, _| div().font_family(font.clone()).child(tip.clone()))
                        .build(window, cx)
                })
                .tooltip_show_delay(Duration::from_millis(400))
            });
        // When `copy` is set, right-click offers a Copy context menu for the
        // value (on this same element so its hitbox isn't occluded).
        match copy {
            Some(value) => base
                .on_mouse_down(MouseButton::Right, move |_, _window, cx: &mut App| {
                    let value = value.clone();
                    copy_view.update(cx, |v, vcx| {
                        v.pending_copy = Some(value);
                        v.ctx_menu_open = true;
                        vcx.notify();
                    });
                })
                .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)))
                .into_any_element(),
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
                    div()
                        .flex()
                        .items_center()
                        .when(!glyph.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from(glyph.to_string())),
                            )
                        })
                        .child(
                            div()
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
    pub(crate) fn titlebar_action(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        command: &'static str,
        tip: impl Into<SharedString>,
        copy: Option<String>,
        child: impl IntoElement,
    ) -> impl IntoElement {
        let view = view.clone();
        let copy_view = view.clone();
        let id = id.into();
        let font = self.font.clone();
        let tip = tip.into();
        let base = div()
            .id(id.clone())
            .relative()
            .cursor_pointer()
            .child(track_target(id))
            .child(child)
            // Suppress the tooltip while a Copy menu is open so it can't paint
            // over the menu.
            .when(!self.ctx_menu_open, |d| {
                d.tooltip(move |window, cx| {
                    let (font, tip) = (font.clone(), tip.clone());
                    Tooltip::element(move |_, _| div().font_family(font.clone()).child(tip.clone()))
                        .build(window, cx)
                })
                .tooltip_show_delay(Duration::from_millis(400))
            })
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| v.invoke_command(command, window, vcx));
            });
        // When `copy` is set, right-click copies the value via the shared Copy
        // menu. The handlers sit on this same interactive element so its hitbox
        // isn't occluded by a wrapper — the custom title bar otherwise swallows
        // the menu (`context_menu` changes the type, so we branch here).
        match copy {
            Some(value) => base
                .on_mouse_down(MouseButton::Right, move |_, _window, cx: &mut App| {
                    let value = value.clone();
                    copy_view.update(cx, |v, vcx| {
                        v.pending_copy = Some(value);
                        v.ctx_menu_open = true;
                        vcx.notify();
                    });
                })
                .context_menu(|menu, _window, _cx| menu.menu("Copy", Box::new(CtxCopy)))
                .into_any_element(),
            None => base.into_any_element(),
        }
    }

    /// The custom window title bar: the repo name, the current branch as a chip,
    /// its ahead/behind vs upstream, and a dirty marker — styled to match the
    /// app (so it reads as chrome, not the OS bar). The `TitleBar` component
    /// handles traffic-light spacing, dragging, and (off-macOS) window controls.
    pub(crate) fn render_title_bar(&self, view: &Entity<Self>) -> impl IntoElement {
        let repo_name = self
            .repo
            .as_ref()
            .map(|r| r.workdir())
            .unwrap_or(self.root.as_path())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".to_string());

        let mut info = div().flex().items_center().gap_2().child(
            self.titlebar_tip(
                view,
                "titlebar-repo",
                "Repository",
                Some(repo_name.clone()),
                div()
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

            // Nearest tag(s): "Tag: v1 (5)" (behind) or "Tags: v1 (5), v2 (2)"
            // (behind + ahead), magit's status tag header. Gated by `show_tags_in_title_bar`
            // (when off, `tag_info` is left empty so this is skipped).
            let (cur, next) = &self.tag_info;
            let entries: Vec<&(String, usize)> = [cur.as_ref(), next.as_ref()]
                .into_iter()
                .flatten()
                .collect();
            // Gate on the live config too, so toggling `show_tags_in_title_bar` off hides the
            // segment immediately (not just after the next status refresh clears
            // `tag_info`).
            if self.config.show_tags_in_title_bar && !entries.is_empty() {
                let mut seg = div().flex().items_center().gap_1();
                for (i, (name, count)) in entries.iter().enumerate() {
                    // The first entry is the tag HEAD is at or past; a second is
                    // the next tag ahead. The count is commits since (until) it.
                    let (tip, count_tip) = if i == 0 {
                        ("Nearest tag", "Commits since tag")
                    } else {
                        ("Next tag", "Commits until tag")
                    };
                    // A tag-tinted pill: the name (click opens the tag transient)
                    // and, divided off like the branch chip's copy button, the
                    // commits-since count.
                    let mut pill = div()
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .bg(with_alpha(self.palette.tag, 0.15))
                        .text_size(px(11.0))
                        .text_color(self.palette.tag)
                        .child(self.titlebar_action(
                            view,
                            format!("titlebar-tag-{i}"),
                            "tag",
                            tip,
                            Some(name.clone()),
                            div().px(px(5.0)).child(SharedString::from(name.clone())),
                        ));
                    if *count > 0 {
                        pill = pill
                            .child(
                                div()
                                    .w(px(1.0))
                                    .h(px(12.0))
                                    .bg(with_alpha(self.palette.tag, 0.4)),
                            )
                            .child(
                                self.titlebar_tip(
                                    view,
                                    format!("titlebar-tag-{i}-count"),
                                    count_tip,
                                    None,
                                    div()
                                        .px(px(4.0))
                                        .child(SharedString::from(count.to_string())),
                                ),
                            );
                    }
                    seg = seg.child(pill);
                }
                info = info.child(seg);
            }

            if !status.is_clean() {
                // Marks uncommitted changes in the working tree.
                info = info.child(self.titlebar_tip(
                    view,
                    "titlebar-dirty",
                    "Working tree dirty",
                    None,
                    div().text_color(self.palette.modified).child("○"),
                ));
            }
        }

        gpui_component::TitleBar::new()
            .bg(self.palette.bg)
            .border_color(self.palette.border)
            .child(info)
            // A spinner for background activity that outlasts the delay
            // threshold. The title bar lays children out `justify_between`, so a
            // second child sits at the far (right) end; pad it off the edge so
            // it isn't clipped. A subtle rounded background chip makes it read
            // as a deliberate indicator rather than blending into the bar.
            .when(self.busy, |bar| {
                bar.child(
                    div().pr_3().child(
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

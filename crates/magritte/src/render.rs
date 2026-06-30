//! Rendering for [`StatusView`]: the screen layouts (status tree, log, commit
//! diff, rebase-todo, settings chrome), the transient/help popups, the title
//! bar, and the `uniform_list` row renderer. Split out of `main.rs` — these are
//! `impl StatusView` methods plus the `Render` impl, so they read and write the
//! same private fields as the rest of the view.

use std::path::PathBuf;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Context, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::{Button, DropdownButton};
use gpui_component::input::Input;
use gpui_component::menu::ContextMenuExt;
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, IconName, Sizable};
use magritte_core::transient::{Group, Suffix, TitleSpan, Transient};
use magritte_core::{RebaseAction, Sequence};

use crate::*;

impl StatusView {
    /// Render a popup (command transient or the `?` help menu) as a bottom
    /// panel. `state` is `None` for the help menu, which has no toggled
    /// switches and no pending-dash prefix.
    /// A button label that gets a background highlight only when its containing
    /// [`KBD_ROW_GROUP`] row is hovered — so mousing over a keycap+label button
    /// highlights the text, not the keycap.
    pub(crate) fn hover_label(&self, text: &str, color: Hsla) -> gpui::Div {
        div()
            .px_1()
            .rounded(px(3.0))
            .text_color(color)
            .group_hover(KBD_ROW_GROUP, |s| s.bg(self.palette.visual))
            .child(SharedString::from(text.to_string()))
    }

    /// The remote-picker overlay: a title and kbd hints over a searchable list
    /// of remotes (search field focused on appear). Enter / clicking a row runs
    /// the transfer; the "return" kbd button does the same.
    pub(crate) fn render_picker(&self, state: &PickerState, view: &Entity<Self>) -> gpui::Div {
        let confirm_label = state.action.confirm_label();

        // Reserve a fixed screenful for the candidate area, so the
        // bottom-anchored panel never resizes — neither while filtering (which
        // only shrinks the matches) nor when async candidates load. A pure
        // value-entry prompt has no candidates and collapses instead.
        const MAX_VISIBLE: usize = 8;
        let rows = state.list.row_count();
        let list_height = px(MAX_VISIBLE as f32 * ROW_HEIGHT);

        let body = if !state.reserve_candidates {
            // Value entry has nothing to match — collapse the candidate area
            // entirely so the hints sit right under the input.
            div().into_any_element()
        } else if rows == 0 {
            // No rows: either candidates are still loading off the UI thread, or
            // they're loaded and none match the query. A quiet line in the first
            // row keeps the reserved height so nothing shifts.
            let note = if state.loading {
                "Loading…"
            } else {
                "No match"
            };
            div()
                .h(list_height)
                .child(
                    div()
                        .h(px(ROW_HEIGHT))
                        .pl(px(ROW_PAD_LEFT))
                        .flex()
                        .items_center()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(note)),
                )
                .into_any_element()
        } else {
            uniform_list("picker-rows", rows, {
                let view = view.clone();
                move |range, _window, cx| match &view.read(cx).popup {
                    Some(Popup::Picker(p)) => {
                        // In the command palette, show each command's keybinding
                        // (when it has one) on the right, so it doubles as help.
                        let palette = matches!(p.action, PickerAction::RunCommand);
                        range
                            .map(|ix| match p.list.row(ix) {
                                Some(r) => {
                                    let hint = palette
                                        .then(|| {
                                            let v = view.read(cx);
                                            command_keys(&v.keymap, &v.config, &r.label)
                                        })
                                        .flatten()
                                        .map(SharedString::from);
                                    view.read(cx).render_picker_row(
                                        ix,
                                        r.label,
                                        r.is_create,
                                        ix == p.list.selected(),
                                        hint,
                                        &view,
                                    )
                                }
                                None => div().h(px(ROW_HEIGHT)).into_any_element(),
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                }
            })
            .track_scroll(&state.scroll)
            .h(list_height)
            .w_full()
            .into_any_element()
        };

        div()
            .w_full()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .py_2()
            .px_3()
            .flex()
            .flex_col()
            .gap_1()
            // Prompt with the query typed inline (vertico minibuffer).
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(ROW_PAD_LEFT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(self.render_title(&state.prompt, self.palette.section))
                            .child(
                                div()
                                    .text_color(self.palette.section)
                                    .child(SharedString::from(":")),
                            ),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .child(Input::new(&state.input).appearance(false)),
                    ),
            )
            .child(body)
            // Keyboard hints, consistent with the transient menus.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_1()
                    .pl(px(ROW_PAD_LEFT))
                    .child(self.key_action(
                        "remote-confirm",
                        "return",
                        confirm_label,
                        view,
                        Self::confirm_picker,
                    ))
                    .child(self.key_action(
                        "remote-picker-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::cancel_popup,
                    )),
            )
    }

    /// One candidate row: a full-width highlight when current (vertico-style, no
    /// boxy border), a subtle hover for the mouse, and click-to-confirm.
    pub(crate) fn render_picker_row(
        &self,
        ix: usize,
        label: SharedString,
        is_create: bool,
        selected: bool,
        hint: Option<SharedString>,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut el = div()
            .id(SharedString::from(format!("picker-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(ROW_PAD_LEFT))
            .cursor_pointer()
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(Popup::Picker(p)) = this.popup.as_mut() {
                        p.list.set_selected(ix);
                    }
                    this.confirm_picker(window, vcx);
                });
            });
        if selected {
            el = el.bg(self.palette.selection);
        } else {
            // The picker sits on the elevated panel, where the neutral
            // `list.hover.background` can equal the panel itself (e.g. Selenized
            // White) and vanish. The translucent accent (also used for the
            // transient menu's hover) stays visible on any surface, and reads
            // distinctly from the neutral keyboard-selected row.
            el = el.hover(|s| s.bg(self.palette.visual));
        }
        let label_el = if is_create {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_color(self.palette.fg).child(label))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("(new)")),
                )
        } else {
            div().text_color(self.palette.fg).child(label)
        };
        el = el.child(label_el);
        // The command's binding (palette only) as subtle text right after the
        // name: a single key for top-level commands, or the full prefix→suffix
        // sequence for leaves (e.g. `c c` for "Create commit"). Plain text keeps
        // the rows at their normal height (a keycap would be too tall here).
        if let Some(seq) = hint {
            el = el.child(
                div()
                    .ml_1()
                    .text_color(self.palette.dim)
                    // Keys are monospace, like keycaps elsewhere, even under a
                    // proportional UI font.
                    .font_family(self.font.clone())
                    .child(SharedString::from(kbd::format_keys(&seq))),
            );
        }
        el.into_any_element()
    }

    /// Close the open picker. If it was prompting for a transient option value,
    /// reopen that transient unchanged rather than dismissing everything.
    pub(crate) fn cancel_popup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Popup::Picker(p)) = self.popup.take() {
            if let Some(ts) = p.resume {
                self.popup = Some(Popup::Transient(*ts));
            }
        }
        cx.notify();
    }

    pub(crate) fn render_transient(
        &self,
        def: &Transient,
        state: Option<&TransientState>,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let pending_dash = state.is_some_and(|s| s.pending_dash);

        // Magit's layout, derived from content rather than hand-authored: an
        // *argument* group (switches/options) is a full-width band, and bands
        // stack vertically; the *command* groups (actions/`?`-menu info) sit
        // side by side in a wrapping row beneath them. A tall argument band fans
        // its rows into sub-columns (capped ~BAND_CAP each) so it stays compact.
        // This reproduces magit's commit transient (Arguments band over a row of
        // Create/Edit/… columns), the log transient (Arguments band over the Log
        // command row), and the `?` dispatch (all command groups → one packed
        // row), without a per-transient layout spec.
        const BAND_CAP: usize = 7;
        let has_args = |g: &&Group| {
            g.suffixes
                .iter()
                .any(|s| matches!(s, Suffix::Switch(_) | Suffix::Option(_)))
        };

        let mut body = div().flex().flex_col().items_start().gap_3();
        for group in def.groups.iter().filter(has_args) {
            let k = group.suffixes.len().div_ceil(BAND_CAP).max(1);
            body = body.child(self.render_group(group, k, state, pending_dash, view));
        }
        let mut command_row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_x_8()
            .gap_y_3();
        let mut any_command = false;
        for group in def.groups.iter().filter(|g| !has_args(g)) {
            any_command = true;
            // A tall command group (e.g. the `?` dispatch's "Commands") fans into
            // sub-columns just like an argument band, so it doesn't tower over
            // the shorter groups beside it.
            let k = group.suffixes.len().div_ceil(BAND_CAP).max(1);
            command_row = command_row.child(self.render_group(group, k, state, pending_dash, view));
        }
        if any_command {
            body = body.child(command_row);
        }

        // The save hint sits at the *top* of the panel: the popup is
        // bottom-anchored, so adding a row here grows it upward into empty space
        // without shifting the title/groups — no reserved dead space and no
        // layout shift either way (a bottom row would push them up). It shows the
        // `C-s` prompt once the toggles differ from their saved/built-in
        // baseline, and turns into a scope chooser (`g`lobal / `l`ocal) once the
        // save key is pressed.
        let saving = state.is_some_and(|s| s.pending_save);
        let show_save = state.is_some_and(|s| !s.id.is_empty() && s.active != s.baseline);
        let has_repo = self.repo_scope_dir.is_some();

        div()
            .w_full()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .py_2()
            .px_3()
            .flex()
            .flex_col()
            .gap_2()
            .when(saving, |el| {
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("save as default:"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(kbd::key_chip("g", self.palette.dim, &self.font))
                            .child(SharedString::from("global")),
                    );
                if has_repo {
                    row = row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(kbd::key_chip("l", self.palette.dim, &self.font))
                            .child(SharedString::from("this repo")),
                    );
                }
                el.child(row)
            })
            .when(show_save && !saving, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(self.palette.dim)
                        .child(kbd::key_chip(TRANSIENT_SAVE_KEY, self.palette.dim, &self.font))
                        .child(SharedString::from("save these switches as the default")),
                )
            })
            .child(self.render_title(&def.title, self.palette.section))
            .child(body)
    }

    /// One transient group as a left-aligned band: its dim title above its
    /// suffix rows (switches, value options, actions, or `?`-menu info). A tall
    /// group spreads its rows across `subcols` sub-columns *within the band*
    /// (magit's `[[col][col]]`) so it doesn't dominate the panel height — e.g.
    /// the log transient's 8 arguments become two columns of four under one
    /// "Arguments" heading. `items_start` so each row's clickable hitbox hugs
    /// its content width (else clicks land on the wrong row).
    pub(crate) fn render_group(
        &self,
        group: &Group,
        subcols: usize,
        state: Option<&TransientState>,
        pending_dash: bool,
        view: &Entity<Self>,
    ) -> gpui::Div {
        let n = group.suffixes.len();
        let k = subcols.clamp(1, n.max(1));
        let per = n.div_ceil(k).max(1);
        let mut buckets: Vec<Vec<AnyElement>> = (0..k).map(|_| Vec::new()).collect();
        for (i, suffix) in group.suffixes.iter().enumerate() {
            let bucket = (i / per).min(k - 1);
            buckets[bucket].push(self.render_suffix(suffix, state, pending_dash, view));
        }
        let mut row = div().flex().flex_row().items_start().gap_x_6();
        for bucket in buckets {
            let mut sc = div().flex().flex_col().items_start().gap_1();
            for el in bucket {
                sc = sc.child(el);
            }
            row = row.child(sc);
        }
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_1()
            .child(self.render_title(&group.title, self.palette.dim))
            .child(row)
    }

    /// One transient suffix as a clickable row (switch, value option, action,
    /// or `?`-menu info).
    pub(crate) fn render_suffix(
        &self,
        suffix: &Suffix,
        state: Option<&TransientState>,
        pending_dash: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        match suffix {
            Suffix::Switch(sw) => {
                let on = state.is_some_and(|s| s.active.contains(sw.key.as_str()));
                // A negatable, config-derived switch (e.g. --gpg-sign) turned off
                // against an enabled config default shows its explicit negation
                // (--no-gpg-sign) — that's the flag we'll actually pass, so it
                // reads as an active override rather than a dim "off".
                let negated = !on && sw.default_on && sw.negation.is_some();
                let shown_flag = match (&sw.negation, negated) {
                    (Some(neg), true) => neg.clone(),
                    _ => sw.arg.clone(),
                };
                // magit layout: key, description, then the literal git flag
                // in parens. Only the flag itself dims (inactive) or highlights
                // bold in the `modified` accent (the active choice — on, or an
                // explicit negation) — the parens stay a constant neutral color.
                let active_flag = on || negated;
                let flag_color = if active_flag {
                    self.palette.modified
                } else {
                    self.palette.dim
                };
                let flag = if active_flag {
                    div().text_color(flag_color).font_weight(FontWeight::BOLD)
                } else {
                    div().text_color(flag_color)
                };
                let paren = || div().text_color(self.palette.fg);
                let view = view.clone();
                let key = SharedString::from(sw.key.clone());
                div()
                    .id(key.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(key.clone()))
                    .child(kbd::switch_chip(
                        &sw.key,
                        self.palette.dim,
                        self.palette.removed,
                        pending_dash,
                        &self.font,
                    ))
                    // A custom switch may have no description — show just its flag.
                    .when(!sw.description.is_empty(), |el| {
                        el.child(self.hover_label(&sw.description, self.palette.fg))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(paren().child(SharedString::from("(")))
                            .child(flag.child(SharedString::from(shown_flag)))
                            .child(paren().child(SharedString::from(")"))),
                    )
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), true, window, vcx));
                    })
                    .into_any_element()
            }
            // A value-reading option: like a switch, but the parens show the
            // current value (or the bare flag when unset). The parens are
            // omitted when there'd be nothing in them (an option whose value
            // *is* the flag, e.g. commit order, when unset).
            Suffix::Option(o) => {
                let value = state.and_then(|s| s.values.get(o.key).cloned());
                let set = value.is_some();
                let inner = format!("{}{}", o.arg, value.as_deref().unwrap_or_default());
                let color = if set {
                    self.palette.modified
                } else {
                    self.palette.dim
                };
                let view = view.clone();
                let okey = o.key.to_string();
                div()
                    .id(o.key)
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(o.key))
                    .child(kbd::switch_chip(
                        o.key,
                        self.palette.dim,
                        self.palette.removed,
                        pending_dash,
                        &self.font,
                    ))
                    .child(self.hover_label(o.description, self.palette.fg))
                    .when(!inner.is_empty(), |row| {
                        row.child(
                            div()
                                .text_color(color)
                                .child(SharedString::from(format!("({inner})"))),
                        )
                    })
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_option(okey.clone(), window, vcx));
                    })
                    .into_any_element()
            }
            Suffix::Action(a) => {
                let view = view.clone();
                let key = SharedString::from(a.key);
                div()
                    .id(a.key)
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(a.key))
                    .child(kbd::key_chip(a.key, self.palette.dim, &self.font))
                    .child(self.hover_label(&a.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
            // A dispatch command row: keycap + label, clickable to run.
            Suffix::Info(i) => {
                let view = view.clone();
                let key = SharedString::from(i.keys.clone());
                div()
                    .id(key.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(key.clone()))
                    .child(self.key_tokens(&i.keys))
                    .child(self.hover_label(&i.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.run_dispatch(&key, window, vcx));
                    })
                    .into_any_element()
            }
            // A user-injected suffix (from `[transient]`): keycap + label,
            // clickable; dispatched by key like an action.
            Suffix::Custom(c) => {
                let view = view.clone();
                let key = SharedString::from(c.key.clone());
                div()
                    .id(key.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .group(KBD_ROW_GROUP)
                    .child(track_target(key.clone()))
                    .child(kbd::key_chip(&c.key, self.palette.dim, &self.font))
                    .child(self.hover_label(&c.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
        }
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

    /// A small copy-to-clipboard icon button: copies `text` and flashes the
    /// "Copied" confirmation; `tooltip` names what it copies.
    pub(crate) fn copy_icon_button(
        &self,
        view: &Entity<Self>,
        id: &'static str,
        text: String,
        tooltip: &'static str,
    ) -> impl IntoElement {
        let view = view.clone();
        let tip_font = self.font.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .cursor_pointer()
            .px(px(4.0))
            .child(track_target(id))
            .child(
                Icon::new(IconName::Copy)
                    .xsmall()
                    .text_color(self.palette.fg),
            )
            .tooltip(move |window, cx| {
                let font = tip_font.clone();
                Tooltip::element(move |_, _| div().font_family(font.clone()).child(tooltip))
                    .build(window, cx)
            })
            .tooltip_show_delay(Duration::ZERO)
            .on_click(move |_, _window, cx: &mut App| {
                let text = text.clone();
                view.update(cx, |v, vcx| v.copy_to_clipboard(text, vcx));
            })
    }

    /// The title-bar branch as a divided pill sharing one highlight: the name
    /// (click opens the branch transient) and a copy-name button.
    pub(crate) fn render_branch_chip(&self, view: &Entity<Self>, branch: &str) -> gpui::Div {
        let branch_click = view.clone();
        div()
            .flex()
            .items_center()
            .rounded(px(4.0))
            .bg(self.palette.selection)
            .text_color(self.palette.fg)
            .font_family(self.font.clone())
            .font_weight(FontWeight::MEDIUM)
            .child(
                div()
                    .id("titlebar-branch")
                    .relative()
                    .cursor_pointer()
                    .px(px(5.0))
                    .child(track_target("titlebar-branch"))
                    .child(SharedString::from(branch.to_string()))
                    .on_click(move |_, window, cx: &mut App| {
                        branch_click.update(cx, |v, vcx| v.invoke_command("branch", window, vcx));
                    }),
            )
            // Divider between the two halves of the split chip.
            .child(div().w(px(1.0)).h(px(12.0)).bg(self.palette.dim))
            .child(self.copy_icon_button(
                view,
                "titlebar-branch-copy",
                branch.to_string(),
                "Copy branch name",
            ))
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
            row = row.child(kbd::key_chip(&keys, self.palette.dim, &self.font));
        }
        row.child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// A dim tracking entry for the title bar: an optional direction glyph
    /// (`⇡` push / `⇣` pull), the ref name, and `↑ahead`/`↓behind` (each shown
    /// only when non-zero). The ahead/behind are clickable: `↑` opens the push
    /// transient, `↓` the pull transient. `key` namespaces their element ids.
    pub(crate) fn track_chunk(
        &self,
        view: &Entity<Self>,
        key: &str,
        glyph: &str,
        name: &str,
        ahead: i64,
        behind: i64,
    ) -> gpui::Div {
        let mut chunk = div()
            .flex()
            .items_center()
            .gap_1()
            .text_color(self.palette.dim)
            .font_family(self.font.clone())
            .child(SharedString::from(format!("{glyph}{name}")));
        if ahead > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-ahead"),
                "push",
                SharedString::from(format!("↑{ahead}")),
            ));
        }
        if behind > 0 {
            chunk = chunk.child(self.titlebar_action(
                view,
                format!("{key}-behind"),
                "pull",
                SharedString::from(format!("↓{behind}")),
            ));
        }
        chunk
    }

    /// A clickable title-bar element that runs the registry command `command`
    /// (the branch chip → "branch", an ahead count → "push", a behind count →
    /// "pull"). Brightens on hover to signal it's actionable.
    pub(crate) fn titlebar_action(
        &self,
        view: &Entity<Self>,
        id: impl Into<SharedString>,
        command: &'static str,
        child: impl IntoElement,
    ) -> impl IntoElement {
        let view = view.clone();
        let fg = self.palette.fg;
        let id = id.into();
        div()
            .id(id.clone())
            .relative()
            .cursor_pointer()
            .hover(move |s| s.text_color(fg))
            .child(track_target(id))
            .child(child)
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| v.invoke_command(command, window, vcx));
            })
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
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(SharedString::from(repo_name)),
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
                        self.branch_chip("detached"),
                    )
                    .into_any_element(),
            });

            // Tracking: the upstream, plus a distinct push target when present
            // (a triangular workflow). When the push target equals the upstream,
            // the core leaves `head.push` unset, so we show a single entry.
            match (&head.push, &head.upstream) {
                (Some(push), upstream) => {
                    info = info.child(self.track_chunk(
                        view,
                        "push",
                        "⇡",
                        push,
                        head.push_ahead,
                        head.push_behind,
                    ));
                    if let Some(up) = upstream {
                        info = info.child(self.track_chunk(
                            view,
                            "up",
                            "⇣",
                            up,
                            head.ahead,
                            head.behind,
                        ));
                    }
                }
                (None, Some(up)) => {
                    info =
                        info.child(self.track_chunk(view, "up", "", up, head.ahead, head.behind));
                }
                (None, None) => {}
            }

            // Nearest tag(s): "Tag: v1 (5)" (behind) or "Tags: v1 (5), v2 (2)"
            // (behind + ahead), magit's status tag header. Gated by `show_tags`
            // (when off, `tag_info` is left empty so this is skipped).
            let (cur, next) = &self.tag_info;
            let entries: Vec<&(String, usize)> = [cur.as_ref(), next.as_ref()]
                .into_iter()
                .flatten()
                .collect();
            // Gate on the live config too, so toggling `show_tags` off hides the
            // segment immediately (not just after the next status refresh clears
            // `tag_info`).
            if self.config.show_tags && !entries.is_empty() {
                let label = if entries.len() > 1 { "Tags:" } else { "Tag:" };
                let mut seg = div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_color(self.palette.dim).child(SharedString::from(label)));
                for (i, (name, count)) in entries.iter().enumerate() {
                    let mut text = name.clone();
                    if *count > 0 {
                        text.push_str(&format!(" ({count})"));
                    }
                    if i + 1 < entries.len() {
                        text.push(',');
                    }
                    seg = seg.child(
                        div()
                            .text_color(self.palette.modified)
                            .child(SharedString::from(text)),
                    );
                }
                info = info.child(seg);
            }

            if !status.is_clean() {
                // Marks uncommitted changes in the working tree.
                info = info.child(div().text_color(self.palette.modified).child("○"));
            }
        }

        gpui_component::TitleBar::new()
            .bg(self.palette.bg)
            .border_color(self.palette.border)
            .child(info)
            // A subtle spinner for background activity that outlasts the delay
            // threshold. The title bar lays children out `justify_between`, so a
            // second child sits at the far (right) end; pad it off the edge so
            // it isn't clipped.
            .when(self.busy, |bar| {
                bar.child(
                    div()
                        .pr_3()
                        .child(Spinner::new().small().color(self.palette.fg)),
                )
            })
    }

    /// Render a key spec as a single keycap. A multi-keystroke sequence (e.g.
    /// `g r`) keeps its keys spaced *inside* the one cap (see [`format_keys`]).
    pub(crate) fn key_tokens(&self, keys: &str) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .child(kbd::key_chip(keys, self.palette.dim, &self.font))
    }

    /// A clickable key hint: a keycap + label that runs `action` (the same
    /// behavior its key triggers). Lets shown keys double as mouse buttons —
    /// used by the commit editor and settings screen.
    pub(crate) fn key_action(
        &self,
        id: &'static str,
        key: &'static str,
        label: &'static str,
        view: &Entity<Self>,
        action: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let view = view.clone();
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .rounded(px(4.0))
            .cursor_pointer()
            .group(KBD_ROW_GROUP)
            .child(track_target(id))
            .child(kbd::key_chip(key, self.palette.dim, &self.font))
            .child(self.hover_label(label, self.palette.dim))
            .on_click(move |_, window, cx: &mut App| {
                view.update(cx, |v, vcx| action(v, window, vcx));
            })
    }

    /// Render the commit message editor: a header, the editable text with a
    /// caret, all filling the window.
    pub(crate) fn render_editor(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        let title = match ed.mode {
            CommitMode::Create => "Commit message",
            CommitMode::Amend => "Amend commit",
            CommitMode::Reword => "Reword commit",
        };

        let root = div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .w_full()
            // The message editor and diff preview are monospace (the 50/72
            // ruler depends on column alignment).
            .font_family(self.font.clone())
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from(title)),
                    )
                    .map(|el| {
                        if ed.confirming_cancel {
                            // Unsaved edits: confirm before discarding the message.
                            el.child(
                                div()
                                    .text_color(self.palette.dim)
                                    .child(SharedString::from("Discard message?")),
                            )
                            .child(self.key_action(
                                "editor-discard-yes",
                                "y",
                                "discard",
                                view,
                                Self::discard_editor,
                            ))
                            .child(self.key_action(
                                "editor-discard-no",
                                "n",
                                "keep editing",
                                view,
                                Self::keep_editing,
                            ))
                        } else {
                            el.child(self.key_action(
                                "editor-commit",
                                "cmd-enter",
                                "commit",
                                view,
                                Self::submit_editor,
                            ))
                            .child(self.key_action(
                                "editor-reflow",
                                "alt-q",
                                "reflow",
                                view,
                                Self::reflow_editor,
                            ))
                            .child(self.key_action(
                                "editor-cancel",
                                "esc",
                                "cancel",
                                view,
                                Self::cancel_editor,
                            ))
                        }
                    }),
            );

        // With a staged diff to review, the message takes a fixed band at the
        // top and the diff fills the rest (scrollable); otherwise the message
        // fills the window.
        if ed.diff.is_empty() {
            root.child(
                div()
                    .flex_grow(1.0)
                    .w_full()
                    .child(Input::new(&ed.state).h_full()),
            )
        } else {
            root.child(
                div()
                    .h(px(176.0))
                    .w_full()
                    .child(Input::new(&ed.state).h_full()),
            )
            .child(self.render_commit_diff(ed, view))
        }
    }

    /// The read-only, scrollable staged-diff preview shown below the message.
    pub(crate) fn render_commit_diff(&self, ed: &CommitEditor, view: &Entity<Self>) -> gpui::Div {
        let count = ed.diff.len();
        div()
            .relative()
            .w_full()
            .flex_grow(1.0)
            .border_t_1()
            .border_color(self.palette.border)
            .child(
                uniform_list("commit-diff", count, {
                    let view = view.clone();
                    move |range, _window, cx| {
                        let this = view.read(cx);
                        match this.editor() {
                            Some(ed) => range
                                .map(|ix| this.render_commit_diff_row(&ed.diff[ix], false))
                                .collect::<Vec<_>>(),
                            None => Vec::new(),
                        }
                    }
                })
                .track_scroll(&ed.diff_scroll)
                .size_full()
                .py_1(),
            )
            .vertical_scrollbar(&ed.diff_scroll)
    }

    pub(crate) fn render_commit_diff_row(&self, row: &CommitDiffRow, highlighted: bool) -> AnyElement {
        let base = div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .when(highlighted, |el| el.bg(self.palette.selection));
        match row {
            CommitDiffRow::File(path) => base
                .child(
                    div()
                        .text_color(self.palette.section)
                        .child(SharedString::from(path.clone())),
                )
                .into_any_element(),
            CommitDiffRow::Hunk(text) => base
                .text_color(self.palette.hunk)
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            CommitDiffRow::Note(text) => base
                .text_color(self.palette.dim)
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            CommitDiffRow::Line { kind, spans } => {
                let (sign, sign_color, tint) = match kind {
                    LineKind::Added => ('+', self.palette.added, Some(self.palette.added_bg)),
                    LineKind::Removed => ('-', self.palette.removed, Some(self.palette.removed_bg)),
                    _ => (' ', self.palette.dim, None),
                };
                let mut el = base;
                if let Some(t) = tint {
                    el = el.bg(t);
                }
                let mut line = div().flex().child(
                    div()
                        .text_color(sign_color)
                        .child(SharedString::from(sign.to_string())),
                );
                for (text, color) in spans {
                    line = line.child(
                        div()
                            .text_color(*color)
                            .child(SharedString::from(text.clone())),
                    );
                }
                el.child(line).into_any_element()
            }
        }
    }

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

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // Commands and their output are code — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Command log")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.key_action(
                                "command-log-all",
                                "a",
                                if self.git_log_show_all {
                                    "hide queries"
                                } else {
                                    "show all"
                                },
                                view,
                                Self::toggle_git_log_all,
                            ))
                            .child(self.key_action(
                                "command-log-close",
                                "esc",
                                "close",
                                view,
                                Self::close_git_log,
                            )),
                    ),
            )
            .child(body)
    }

    /// The command log flattened into uniform rows: each invocation becomes a
    /// command row followed by its (dim, indented) stderr lines — git's
    /// progress/error narrative.
    pub(crate) fn git_log_rows(&self) -> Vec<GitLogRow> {
        let Some(repo) = self.repo.as_ref() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for c in repo.command_log() {
            // Hide the UI's own read-only queries unless asked to show all.
            if !self.git_log_show_all && c.is_query() {
                continue;
            }
            rows.push(GitLogRow::Command {
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
            GitLogRow::Command { prog, args, ok } => {
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
                div()
                    .h(px(ROW_HEIGHT))
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
                .h(px(ROW_HEIGHT))
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
        // Note when the listing is capped, rather than pretending it's complete.
        let capped = count >= Self::LOG_LIMIT;

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
                            .map(|ix| {
                                this.render_log_row(ix, &log.entries[ix], ix == log.selected, &view)
                            })
                            .collect::<Vec<_>>(),
                        None => Vec::new(),
                    }
                }
            })
            .track_scroll(&log.scroll)
            .flex_grow(1.0)
            .into_any_element(),
        };

        // In select mode the title becomes a prompt and Return confirms the
        // commit; while browsing it's just "Log".
        let selecting = matches!(log.purpose, LogPurpose::SelectRebaseBase { .. });
        let title = if selecting {
            "Select a commit to rebase since"
        } else {
            "Log"
        };
        let mut header = div().flex().items_center().gap_3().child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.palette.section)
                .child(SharedString::from(title)),
        );
        if capped {
            header = header.child(
                div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from(format!("(first {})", Self::LOG_LIMIT))),
            );
        }
        if selecting {
            // Return inspects the commit; Cmd+Return picks it as the base.
            header = header.child(self.key_action(
                "log-select-view",
                "return",
                "view",
                view,
                Self::view_log_commit,
            ));
            header = header.child(self.key_action(
                "log-select-confirm",
                "cmd-enter",
                "select",
                view,
                Self::confirm_log_select,
            ));
        }
        let (close_key, close_label) = if selecting {
            ("esc", "cancel")
        } else {
            ("esc", "close")
        };
        header = header.child(self.key_action(
            "log-close",
            close_key,
            close_label,
            view,
            Self::close_log,
        ));

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // Commit rows are columnar (hash / subject / date) — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(header)
            .child(body)
    }

    /// One commit row: short hash, ref decorations, and subject; highlighted
    /// when current, clickable to open its diff.
    pub(crate) fn render_log_row(
        &self,
        ix: usize,
        entry: &magritte_core::LogEntry,
        selected: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut row = div()
            .id(SharedString::from(format!("log-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_2()
            .cursor_pointer()
            .on_click(move |_, _window, cx: &mut App| {
                view.update(cx, |this, vcx| {
                    if let Some(log) = this.log_mut() {
                        log.selected = ix;
                    }
                    this.open_commit_view(vcx);
                });
            });
        if selected {
            row = row.bg(self.palette.selection);
        } else {
            row = row.hover(|s| s.bg(self.palette.hover));
        }
        row = row.child(
            div()
                .flex_shrink_0()
                .text_color(self.palette.modified)
                .child(SharedString::from(entry.short_hash.clone())),
        );
        if !entry.refs.is_empty() {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_color(self.palette.section)
                    .child(SharedString::from(format!("({})", entry.refs))),
            );
        }
        row.child(
            div()
                .text_color(self.palette.fg)
                .child(SharedString::from(entry.subject.clone())),
        )
        .child(div().flex_grow(1.0))
        .child(
            div()
                .flex_shrink_0()
                .text_color(self.palette.dim)
                .child(SharedString::from(entry.date.clone())),
        )
        .into_any_element()
    }

    /// Render a commit's diff detail (opened from the log): a header with the
    /// hash + subject, then the diff as the same rows the commit editor uses.
    pub(crate) fn render_commit_view(&self, cv: &CommitView, view: &Entity<Self>) -> gpui::Div {
        let count = cv.rows.len();
        let body = uniform_list("commit-view-rows", count, {
            let view = view.clone();
            move |range, _window, cx| {
                let this = view.read(cx);
                match this.commit_view() {
                    Some(cv) => {
                        let vis = cv.visual.map(|a| (a.min(cv.selected), a.max(cv.selected)));
                        range
                            .map(|ix| {
                                let highlighted = ix == cv.selected
                                    || vis.is_some_and(|(lo, hi)| ix >= lo && ix <= hi);
                                this.render_commit_diff_row(&cv.rows[ix], highlighted)
                            })
                            .collect::<Vec<_>>()
                    }
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&cv.scroll)
        .flex_grow(1.0);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            // A commit's header + diff is code — monospace.
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    // The hash and its copy button share one highlight as a
                    // divided pill, mirroring the title-bar branch chip.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .bg(self.palette.selection)
                            .text_color(self.palette.fg)
                            .font_weight(FontWeight::MEDIUM)
                            .child(div().px(px(5.0)).child(cv.short.clone()))
                            .child(div().w(px(1.0)).h(px(12.0)).bg(self.palette.dim))
                            .child(self.copy_icon_button(
                                view,
                                "commit-sha-copy",
                                cv.rev.clone(),
                                "Copy commit hash",
                            )),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.fg)
                            .child(cv.subject.clone()),
                    )
                    .child(self.key_action(
                        "commit-view-close",
                        "esc",
                        "back",
                        view,
                        Self::close_commit_view,
                    )),
            )
            .child(body)
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
                        .map(|ix| this.render_rebase_todo_row(rt, ix))
                        .collect(),
                    None => Vec::new(),
                }
            }
        })
        .track_scroll(&rt.scroll)
        .flex_grow(1.0);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .font_family(self.font.clone())
            .p_4()
            .gap_3()
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
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from(match rt.mode {
                                RebaseTodoMode::Start => format!("Rebase {}..HEAD", rt.base),
                                RebaseTodoMode::Edit => "Edit rebase todo".to_string(),
                            })),
                    )
                    .child(self.key_action(
                        "rebase-todo-start",
                        "return",
                        match rt.mode {
                            RebaseTodoMode::Start => "start",
                            RebaseTodoMode::Edit => "save",
                        },
                        view,
                        Self::run_rebase_todo,
                    ))
                    .child(self.key_action(
                        "rebase-todo-cancel",
                        "esc",
                        "cancel",
                        view,
                        Self::close_rebase_todo,
                    ))
            })
            .child(body)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(self.palette.dim)
                    .child(SharedString::from(
                        "p pick · e edit · s squash · f fixup · d drop · j/k move · J/K reorder",
                    )),
            )
    }

    /// One row of the rebase-todo editor.
    pub(crate) fn render_rebase_todo_row(&self, rt: &RebaseTodoView, ix: usize) -> gpui::Div {
        let step = &rt.steps[ix];
        let selected = ix == rt.selected;
        let (keyword, color) = self.rebase_action_style(step.action);
        let dropped = step.action == RebaseAction::Drop;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(ROW_HEIGHT))
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

    /// The settings "Open config file" control: a split button whose main half
    /// opens the config in the OS default app, and whose dropdown offers "Copy
    /// path" plus an "Open in" list of the installed editors. It's an escape
    /// hatch for settings the UI doesn't expose, and a way to see where the file
    /// lives. Menu items dispatch actions routed to the status view's focus.
    pub(crate) fn open_config_button(&self, view: &Entity<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let main = Button::new("open-config-main")
            .label("Open global config")
            .outline()
            .xsmall()
            .icon(IconName::ExternalLink)
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    view.update(cx, |this, _| this.open_config_file());
                }
            });
        DropdownButton::new("open-config")
            .outline()
            .xsmall()
            .button(main)
            .dropdown_menu(move |menu, _window, _cx| {
                menu.action_context(focus.clone())
                    .menu("Copy path", Box::new(CopyConfigPath))
            })
    }

    /// Persist the current config (so the file exists even if never edited) and
    /// return its path.
    pub(crate) fn saved_config_path(&self) -> Option<PathBuf> {
        config::save(&self.config);
        config::path()
    }

    /// A button that opens this repo's `.git/magritte/config.toml` (the per-repo
    /// overlay), creating it if absent, with a "Copy path" dropdown item. Shown
    /// only when there's a repo.
    pub(crate) fn open_repo_config_button(&self, view: &Entity<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let main = Button::new("open-repo-config-main")
            .label("Open repo config")
            .outline()
            .xsmall()
            .icon(IconName::ExternalLink)
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    view.update(cx, |this, _| this.open_repo_config_file());
                }
            });
        DropdownButton::new("open-repo-config")
            .outline()
            .xsmall()
            .button(main)
            .dropdown_menu(move |menu, _window, _cx| {
                menu.action_context(focus.clone())
                    .menu("Copy path", Box::new(CopyRepoConfigPath))
            })
    }

    /// Copy the repo-scoped config's path to the clipboard.
    pub(crate) fn copy_repo_config_path(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = &self.repo_scope_dir {
            let path = dir.join("config.toml").to_string_lossy().into_owned();
            self.copy_to_clipboard(path, cx);
        }
    }

    /// Open the repo-scoped config (`.git/magritte/config.toml`), creating an
    /// empty file (and its dir) first so the editor has something to open.
    pub(crate) fn open_repo_config_file(&self) {
        let Some(dir) = self.repo_scope_dir.clone() else {
            return;
        };
        let path = dir.join("config.toml");
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, "");
        }
        self.launch_editor(&path, None);
    }

    /// A settings toggle (a `Switch` bound to a `bool` config field) paired with
    /// an info icon whose tooltip explains the setting. The tooltip shows
    /// immediately on hover (zero show-delay, unlike the library's 500ms managed
    /// tooltip) and wraps to a readable width rather than one long line. The
    /// switch flips the field and persists on click; all of it is mouse-driven,
    /// like the rest of the settings screen (not part of the Tab focus ring).
    pub(crate) fn toggle_control(
        &self,
        id: &'static str,
        checked: bool,
        explanation: &'static str,
        view: &Entity<Self>,
        set: fn(&mut config::Config, bool),
    ) -> AnyElement {
        let switch = Switch::new(id).checked(checked).on_click({
            let view = view.clone();
            move |on, _window, cx| {
                let on = *on;
                view.update(cx, |this, cx| {
                    set(&mut this.config, on);
                    config::save(&this.config);
                    cx.notify();
                });
            }
        });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(switch)
            .child(self.info_icon(format!("{id}-info"), explanation))
            .into_any_element()
    }

    /// A small dimmed `(i)` icon that reveals `explanation` in a tooltip on
    /// hover — for clarifying what a settings control does.
    pub(crate) fn info_icon(&self, id: String, explanation: &'static str) -> impl IntoElement {
        let font = self.font.clone();
        let dim = self.palette.dim;
        div()
            .id(SharedString::from(id.clone()))
            .relative()
            .child(track_target(id))
            .child(Icon::new(IconName::Info).xsmall().text_color(dim))
            // gpui's native tooltip (not the library's managed one) so we can
            // drop the show-delay to zero and bound the width so it wraps. The
            // library tooltip forces the theme's UI font; override it back to
            // our monospace chrome font so it matches the rest of the app.
            .tooltip(move |window, cx| {
                let font = font.clone();
                Tooltip::element(move |_, _| {
                    div()
                        .max_w(px(280.0))
                        .font_family(font.clone())
                        .child(SharedString::from(explanation))
                })
                .build(window, cx)
            })
            .tooltip_show_delay(Duration::ZERO)
    }

    pub(crate) fn render_row(&self, ix: usize, view: &Entity<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected && row.selectable;
        let clickable = row.selectable || row.fold.is_some();
        let in_region = self
            .visual_range()
            .is_some_and(|(lo, hi)| ix >= lo && ix <= hi);

        let mut el = div()
            .id(SharedString::from(format!("status-row-{ix}")))
            .flex()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .w_full()
            .when(clickable, |el| el.cursor_pointer())
            .pl(px(ROW_PAD_LEFT + row.indent as f32 * INDENT_STEP));
        // In visual mode the whole region — including the current line — uses
        // the region color, so the cursor line doesn't stand out from it.
        // Otherwise the current line gets the selection accent.
        if in_region {
            el = el.bg(self.palette.visual);
        } else if selected {
            el = el.bg(self.palette.selection);
        } else if clickable {
            // A subtle hover on rows you can act on (not the current line or a
            // visual selection, which already have a background) — the theme's
            // explicit hover wash, so it reads as a preview of selecting.
            el = el.hover(|s| s.bg(self.palette.hover));
        }

        // Code-, diff-, and path-bearing rows render monospace (alignment and
        // code legibility); prose rows (sections, headers, messages) inherit the
        // UI font from the root.
        if matches!(
            row.kind,
            RowKind::Diff { .. }
                | RowKind::HunkHeader { .. }
                | RowKind::File { .. }
                | RowKind::Commit { .. }
                | RowKind::Stash { .. }
        ) {
            el = el.font_family(self.font.clone());
        }

        let content = match &row.kind {
            RowKind::Plain { text, color } => el
                .text_color(*color)
                .child(SharedString::from(text.clone())),
            RowKind::Section {
                title,
                count,
                expanded,
            } => el
                .child(chevron(*expanded, self.palette.dim))
                .child(
                    div()
                        .text_color(self.palette.section)
                        .child(SharedString::from(title.clone())),
                )
                // The section count: just a dim number, no badge/tag chrome.
                // Omitted (None) for sections capped to a fixed size (recent).
                .when_some(*count, |el, count| {
                    el.child(
                        div()
                            .text_color(self.palette.dim)
                            .child(SharedString::from(count.to_string())),
                    )
                }),
            RowKind::File {
                status,
                status_color,
                label,
                expanded,
            } => {
                let lead = match expanded {
                    Some(e) => chevron(*e, self.palette.dim).into_any_element(),
                    None => div().w(px(14.0)).into_any_element(),
                };
                let mut el = el.child(lead);
                // Only files with a status word get the fixed-width status
                // column; untracked files (no word) sit flush after the lead.
                if !status.is_empty() {
                    el = el.child(
                        div()
                            .w(px(STATUS_COL_WIDTH))
                            .text_color(*status_color)
                            .child(SharedString::from(status.clone())),
                    );
                }
                el.child(SharedString::from(label.clone()))
            }
            RowKind::HunkHeader { text, expanded } => {
                el.child(chevron(*expanded, self.palette.dim)).child(
                    div()
                        .text_color(self.palette.hunk)
                        .child(SharedString::from(text.clone())),
                )
            }
            RowKind::Diff { kind, spans } => {
                let (sign, sign_color, tint) = match kind {
                    LineKind::Added => ('+', self.palette.added, Some(self.palette.added_bg)),
                    LineKind::Removed => ('-', self.palette.removed, Some(self.palette.removed_bg)),
                    _ => (' ', self.palette.dim, None),
                };
                // Add/remove background tint, unless the row is selected/in-region.
                if let Some(t) = tint {
                    if !selected && !in_region {
                        el = el.bg(t);
                    }
                }
                // Sign + syntax-highlighted content as adjacent runs (no gap).
                let mut line = div().flex().child(
                    div()
                        .text_color(sign_color)
                        .child(SharedString::from(sign.to_string())),
                );
                for (text, color) in spans {
                    line = line.child(
                        div()
                            .text_color(*color)
                            .child(SharedString::from(text.clone())),
                    );
                }
                el.child(line)
            }
            // Commit/stash rows: a lead spacer to align under the section's
            // chevron, then a dim short hash / reference and the subject / message.
            RowKind::Commit {
                short_hash,
                subject,
                refs,
                ..
            } => {
                let mut el = el.child(div().w(px(14.0))).child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(short_hash.clone())),
                );
                // Ref labels (branches/tags/remotes), colored magit-style: tags
                // accent, branches added-green (current branch bold), remotes
                // section-blue. Parsed at row-build time (see RowKind::Commit).
                for (label, kind) in refs {
                    let (color, bold) = match kind {
                        RefKind::Tag => (self.palette.modified, false),
                        RefKind::Head => (self.palette.added, true),
                        RefKind::Local => (self.palette.added, false),
                        RefKind::Remote => (self.palette.section, false),
                    };
                    let mut chip = div()
                        .text_color(color)
                        .child(SharedString::from(label.clone()));
                    if bold {
                        chip = chip.font_weight(FontWeight::BOLD);
                    }
                    el = el.child(chip);
                }
                el.child(SharedString::from(subject.clone()))
            }
            RowKind::Stash { reference, message } => el
                .child(div().w(px(14.0)))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(reference.clone())),
                )
                .child(SharedString::from(message.clone())),
        };
        if clickable {
            let el = content
                .relative()
                .child(track_target(format!("status-row-{ix}")))
                .on_click({
                    let view = view.clone();
                    move |_, _window, cx: &mut App| {
                        view.update(cx, |v, cx| v.click_row(ix, cx));
                    }
                })
                // Click-and-drag selects a range, like pressing `v` and moving.
                // Shift-click extends a selection from the current cursor (or
                // the existing anchor) to the clicked row, like a list widget.
                .on_mouse_down(MouseButton::Left, {
                    let view = view.clone();
                    move |ev: &MouseDownEvent, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            if ev.modifiers.shift {
                                let anchor = v.visual.unwrap_or(v.selected);
                                v.visual = (ix != anchor).then_some(anchor);
                                v.selected = ix;
                                v.drag_anchor = None;
                                v.shift_click = true;
                            } else {
                                v.drag_anchor = Some(ix);
                                v.visual = None;
                                v.selected = ix;
                                v.shift_click = false;
                            }
                            vcx.notify();
                        });
                    }
                })
                .on_mouse_move({
                    let view = view.clone();
                    move |ev: &gpui::MouseMoveEvent, _window, cx: &mut App| {
                        if ev.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        view.update(cx, |v, vcx| {
                            let Some(anchor) = v.drag_anchor else { return };
                            if !v.rows.get(ix).is_some_and(|r| r.selectable) {
                                return;
                            }
                            // Skip redundant work while the cursor stays on one row.
                            if v.selected == ix && (ix == anchor || v.visual == Some(anchor)) {
                                return;
                            }
                            if ix != anchor {
                                v.visual = Some(anchor);
                            }
                            v.selected = ix;
                            vcx.notify();
                        });
                    }
                })
                .on_mouse_up(MouseButton::Left, {
                    let view = view.clone();
                    move |_, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if v.drag_anchor.take().is_some() {
                                vcx.notify();
                            }
                        });
                    }
                });
            // Right-click on a stageable row: select it (unless a visual
            // selection is in progress) and show a menu of the staging verbs
            // that apply. The actions act on the row at point / the selection.
            match &row.target {
                Some(target) => {
                    let (can_stage, can_unstage, can_discard) = target_ops(target);
                    let conflicted = self.is_conflicted(target_path(target));
                    let (ours_label, theirs_label) = self.conflict_side_labels();
                    let view = view.clone();
                    el.on_mouse_down(MouseButton::Right, move |_, _window, cx: &mut App| {
                        view.update(cx, |v, vcx| {
                            if v.visual.is_none() && v.rows.get(ix).is_some_and(|r| r.selectable) {
                                v.selected = ix;
                                vcx.notify();
                            }
                        });
                    })
                    .context_menu(move |mut menu, _window, _cx| {
                        // A conflicted file resolves by taking a whole side.
                        if conflicted {
                            menu = menu
                                .menu(ours_label, Box::new(CtxTakeOurs))
                                .menu(theirs_label, Box::new(CtxTakeTheirs))
                                .separator();
                        }
                        if can_stage {
                            menu = menu.menu("Stage", Box::new(CtxStage));
                        }
                        if can_unstage {
                            menu = menu.menu("Unstage", Box::new(CtxUnstage));
                        }
                        if can_discard {
                            menu = menu.menu("Discard", Box::new(CtxDiscard));
                        }
                        menu.separator().menu("Copy", Box::new(CtxCopy))
                    })
                    .into_any_element()
                }
                None => el.into_any_element(),
            }
        } else {
            content.into_any_element()
        }
    }

    /// Mouse click on a status row: select it, and toggle its fold if foldable.
    pub(crate) fn click_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        // A shift-click already set up the extended selection in `on_mouse_down`;
        // don't also toggle the row's fold.
        if self.shift_click {
            self.shift_click = false;
            cx.notify();
            return;
        }
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let foldable = row.fold.is_some();
        if row.selectable {
            self.selected = ix;
        }
        if foldable {
            self.toggle_fold(cx);
        } else {
            cx.notify();
        }
    }

    /// The pending-prefix strip, pinned to the window bottom. A lightweight line
    /// showing just the pressed key, until the which-key delay elapses — then it
    /// expands into the continuations (each `<prefix> <key>` and its command's
    /// label), like emacs' which-key.
    pub(crate) fn prefix_indicator(&self) -> Option<gpui::Div> {
        let pending = self.pending_prefix.as_ref()?;
        let mut bar = div()
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .text_color(self.palette.dim)
            .text_xs()
            .flex()
            .items_center()
            .gap_3();
        // The keys typed so far in a single keycap, with a trailing dash to show
        // the sequence is awaiting the next key (emacs' echo-area `g-` feedback).
        bar = bar.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(kbd::key_chip(&pending.seq, self.palette.dim, &self.font))
                .child(
                    div()
                        .text_color(self.palette.dim)
                        .child(SharedString::from("-")),
                ),
        );
        if pending.which_key {
            // Group bindings by their immediate next key after the typed prefix.
            // A next key that completes a binding shows its command's label; one
            // that only leads deeper shows "…" to mark a further sub-sequence.
            let lead = format!("{} ", pending.seq);
            let mut conts: std::collections::BTreeMap<String, Option<String>> =
                std::collections::BTreeMap::new();
            for (k, id) in &self.keymap {
                let Some(rest) = k.strip_prefix(&lead) else {
                    continue;
                };
                let token = rest.split(' ').next().unwrap_or(rest).to_string();
                let completes = format!("{lead}{token}") == *k;
                // The command's label (built-in or user `[[command]]`); a token
                // that only leads deeper has no completing binding yet.
                let title = completes
                    .then(|| {
                        all_commands(&self.config)
                            .find(|c| c.id == id)
                            .map(|c| c.title.to_string())
                    })
                    .flatten();
                // A completing binding's label wins over a sibling sub-prefix.
                let entry = conts.entry(token).or_insert(None);
                if title.is_some() {
                    *entry = title;
                }
            }
            for (token, title) in conts {
                bar = bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(kbd::key_chip(&token, self.palette.dim, &self.font))
                        .child(
                            div()
                                .text_color(self.palette.dim)
                                .child(SharedString::from(
                                    title.unwrap_or_else(|| "…".to_string()),
                                )),
                        ),
                );
            }
        }
        Some(bar)
    }

    /// The status/confirmation banner ("Copied …", errors), as a bottom-pinned
    /// bar. The full-window sub-views (settings, commit, log, …) append this so
    /// a copy confirmation is visible there too, not only in the status view.
    pub(crate) fn status_toast(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let msg = self.status_message.clone()?;
        let bar = div()
            .id("status-bar")
            .w_full()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .text_color(self.palette.fg)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _window, cx| {
                this.clear_status(cx);
            }))
            // Right-click copies the message — handy for a warning or error you
            // want to paste elsewhere. Includes the keycap prefix (e.g. the
            // `g x` of "g x is unbound") so the copied text reads in full.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _window, cx| {
                    let Some(msg) = this.status_message.clone() else {
                        return;
                    };
                    let text = match &this.status_keys {
                        Some(keys) => format!("{keys} {msg}"),
                        None => msg,
                    };
                    this.copy_to_clipboard(text, cx);
                }),
            );
        // A keys-led message (e.g. "g x is unbound") renders each typed key as a
        // keycap before the text, matching the which-key strip.
        if let Some(keys) = self.status_keys.clone() {
            return Some(
                bar.flex()
                    .items_center()
                    .gap_2()
                    .child(kbd::key_chip(&keys, self.palette.dim, &self.font))
                    .child(SharedString::from(msg)),
            );
        }
        // A copy confirmation renders the copied value emphasized — accent
        // color, monospace, italic — so a path or hash reads as a literal.
        Some(match self.status_copied.clone() {
            Some(value) if msg == COPIED_LABEL => bar
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(SharedString::from(COPIED_LABEL))
                .child(
                    div()
                        .font_family(self.font.clone())
                        .italic()
                        .text_color(self.palette.section)
                        .child(value),
                ),
            // While a mutating job runs, hint that C-g/Esc cancels it.
            _ if self.job_cancel.is_some() => bar
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(SharedString::from(msg))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_color(self.palette.dim)
                        .child(kbd::key_chip("ctrl-g", self.palette.dim, &self.font))
                        .child(SharedString::from("to cancel")),
                ),
            // A plain message, possibly multi-line (a command's full output):
            // one row per line so it renders as a block, not run together.
            _ => bar.flex().flex_col().children(
                msg.lines()
                    .map(|l| SharedString::from(l.to_string()))
                    .collect::<Vec<_>>(),
            ),
        })
    }
}

impl Render for StatusView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep keyboard focus on the status view whenever nothing else owns the
        // keyboard (the commit editor, settings, and the picker each have
        // their own focused input), so keys always land — including debug-channel
        // keystrokes while the window isn't frontmost.
        let owns_focus_elsewhere = self.editor().is_some()
            || self.settings().is_some()
            || matches!(self.popup, Some(Popup::Picker(_)));
        if !owns_focus_elsewhere && !self.focus.is_focused(window) {
            self.focus.focus(window, cx);
        }
        self.palette = Palette::from_theme(cx);

        let view = cx.entity();
        let count = self.rows.len();

        let mut root = div()
            .track_focus(&self.focus)
            .key_context(STATUS_CONTEXT)
            .on_action(cx.listener(|this, _: &ToggleFold, window, cx| {
                // Tab is delivered as an action (gpui's Root binds it for
                // focus-nav, which we override here), but its *effect* routes
                // through the keymap like any key, so rebinding/unbinding `tab`
                // in `[keymap]` takes effect.
                if this.settings().is_some() {
                    this.cycle_settings_focus(window, cx);
                } else if this.editor().is_none()
                    && matches!(this.popup, None | Some(Popup::Dispatch(_)))
                {
                    this.run_dispatch("tab", window, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, cx| {
                // Quit when closing the last window (no windowless lingering).
                let last = cx.windows().len() <= 1;
                window.remove_window();
                if last {
                    cx.quit();
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                if this.editor().is_none() && this.popup.is_none() && this.settings().is_none() {
                    this.open_settings(window, cx);
                }
            }))
            // Right-click menu actions, applied to the row at point / selection.
            .on_action(cx.listener(|this, _: &CtxStage, _window, cx| this.act(Op::Stage, cx)))
            .on_action(cx.listener(|this, _: &CtxUnstage, _window, cx| this.act(Op::Unstage, cx)))
            .on_action(cx.listener(|this, _: &CtxDiscard, _window, cx| this.act(Op::Discard, cx)))
            .on_action(cx.listener(|this, _: &CtxTakeOurs, _window, cx| {
                this.resolve_at_point(ConflictSide::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxTakeTheirs, _window, cx| {
                this.resolve_at_point(ConflictSide::Theirs, cx)
            }))
            .on_action(cx.listener(|this, _: &CtxCopy, _window, cx| this.copy_selection(cx)))
            // Settings "Open config file" dropdown actions.
            .on_action(
                cx.listener(|this, _: &CopyConfigPath, _window, cx| this.copy_config_path(cx)),
            )
            .on_action(cx.listener(|this, _: &CopyRepoConfigPath, _window, cx| {
                this.copy_repo_config_path(cx)
            }))
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .bg(self.palette.bg)
            .text_color(self.palette.fg)
            .text_size(px(13.0))
            // Proportional UI font is the base for prose chrome; code/diff/
            // tabular rows and the code views override back to monospace. When
            // no UI font is configured, `ui_font` equals `font`, so this is the
            // old all-monospace behavior.
            .font_family(self.ui_font.clone())
            .flex()
            .flex_col();

        // The title bar sits above every view (status, settings, editor, …).
        root = root.child(self.render_title_bar(&view));

        // Each non-Status screen takes over the window. One match defines the
        // active screen (no re-derived priority cascade); Status falls through to
        // the status list below.
        match &self.screen {
            Screen::Settings(s) => {
                return root
                    .child(self.render_settings(s, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Editor(ed) => {
                return root
                    .child(self.render_editor(ed, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::GitLog(scroll) => {
                return root
                    .child(self.render_git_log(scroll, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::RebaseTodo(rt) => {
                return root
                    .child(self.render_rebase_todo(rt, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Commit { view: cv, .. } => {
                return root
                    .child(self.render_commit_view(cv, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Log(log) => {
                return root
                    .child(self.render_log(log, &view))
                    .children(self.status_toast(cx))
                    .children(self.prefix_indicator());
            }
            Screen::Status => {}
        }

        // An in-progress merge/rebase/cherry-pick/revert sits above the list,
        // visible while the user resolves it.
        if let Some(seq) = &self.sequence {
            root = root.child(self.render_sequence_banner(seq, &view));
        }

        // The list takes the flexible space; the status bar (added below)
        // sits beneath it, so showing the bar never shifts content down.
        // Clicking the list area dismisses an open popup or an active visual
        // selection — including clicks on empty space, not just on rows. (A
        // bottom popup panel is a sibling, so clicks on it don't reach here.)
        let dismissable = self.popup.is_some() || self.visual.is_some();
        root = root.child(
            div()
                .id("list-area")
                .relative()
                .w_full()
                .flex_grow(1.0)
                .when(dismissable, |el| {
                    el.on_click(cx.listener(|this, _, _window, cx| {
                        if this.popup.is_some() {
                            this.popup = None;
                        } else {
                            this.visual = None;
                        }
                        cx.notify();
                    }))
                })
                .child(
                    uniform_list("rows", count, {
                        let view = view.clone();
                        move |range, _window, cx| {
                            let this = view.read(cx);
                            range
                                .map(|ix| this.render_row(ix, &view))
                                .collect::<Vec<_>>()
                        }
                    })
                    .track_scroll(&self.scroll)
                    .size_full()
                    .py_2()
                    .px_2(),
                )
                .vertical_scrollbar(&self.scroll),
        );

        if let Some(popup) = &self.popup {
            root = root.child(match popup {
                Popup::Transient(state) => self.render_transient(&state.def, Some(state), &view),
                Popup::Dispatch(def) => self.render_transient(def, None, &view),
                Popup::Picker(state) => self.render_picker(state, &view),
            });
        } else if let Some((prompt, _)) = &self.confirm {
            root = root.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(self.palette.border)
                    .bg(self.palette.banner)
                    .text_color(self.palette.fg)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(SharedString::from(prompt.clone()))
                    .child(self.key_action("confirm-yes", "y", "yes", &view, Self::confirm_yes))
                    .child(self.key_action("confirm-no", "n", "no", &view, Self::confirm_no)),
            );
        } else if self.visual.is_some() {
            root = root.child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(self.palette.border)
                    .bg(self.palette.visual)
                    .text_color(self.palette.fg)
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_color(self.palette.section)
                            .child(SharedString::from("VISUAL")),
                    )
                    .child(self.key_action("visual-stage", "s", "stage", &view, Self::visual_stage))
                    .child(self.key_action(
                        "visual-unstage",
                        "u",
                        "unstage",
                        &view,
                        Self::visual_unstage,
                    ))
                    .child(self.key_action(
                        "visual-discard",
                        "x",
                        "discard",
                        &view,
                        Self::visual_discard,
                    ))
                    .child(self.key_action(
                        "visual-cancel",
                        "esc",
                        "cancel",
                        &view,
                        Self::visual_cancel,
                    )),
            );
        } else {
            // The status/error/"Copied" banner: click it (or press Esc) to dismiss.
            root = root.children(self.status_toast(cx));
        }

        // A floating "?" button (bottom-right) opens the dispatch menu — a
        // mouse affordance for discovering commands. Hidden while a popup or a
        // bottom bar (confirm / visual / status) is shown, so it never overlaps
        // them.
        let bottom_bar = self.confirm.is_some()
            || self.visual.is_some()
            || self.status_message.is_some()
            || self.pending_prefix.is_some();
        if self.popup.is_none() && !bottom_bar {
            // A plain div (not gpui-component `Button`, which forces a default
            // cursor for non-link variants) so it shows the click cursor, like
            // the app's other affordances.
            let tip_font = self.font.clone();
            root = root.child(
                div()
                    .absolute()
                    .bottom_3()
                    .right_4()
                    .child(track_target("dispatch-help"))
                    .child(
                        div()
                            .id("dispatch-help")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(14.0))
                            .cursor_pointer()
                            .text_color(self.palette.dim)
                            .hover(|s| s.bg(self.palette.selection).text_color(self.palette.fg))
                            .child(SharedString::from("?"))
                            .tooltip(move |window, cx| {
                                let font = tip_font.clone();
                                Tooltip::element(move |_, _| {
                                    div().font_family(font.clone()).child("Help (?)")
                                })
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.popup = Some(Popup::Dispatch(dispatch_menu(&this.keymap, &this.config)));
                                cx.notify();
                            })),
                    ),
            );
        }

        // The pending-prefix strip pins to the very bottom, below any other bar.
        root = root.children(self.prefix_indicator());

        root
    }
}

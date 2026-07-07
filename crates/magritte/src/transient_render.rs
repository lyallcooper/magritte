//! Transient/popup rendering: the bottom-anchored command menus (argument
//! bands, command groups, git-config variable rows) and their content-derived
//! column layout. `impl StatusView` like the other view slices.

use gpui::prelude::FluentBuilder;
use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement, Window};

use crate::*;

/// Rough rendered width (px) of one transient suffix cell — its keycap plus
/// description (and the git flag in parens for switches/options). Used to decide
/// how many columns of a group fit the window; a slight over-estimate keeps text
/// from overflowing. Tuned for the ~13px UI/mono fonts.
fn suffix_cell_px(suffix: &Suffix) -> f32 {
    // (key chars, text chars) — the flag in parens counts toward the text.
    let (key, text) = match suffix {
        Suffix::Switch(sw) => (
            sw.key.chars().count(),
            sw.description.chars().count() + sw.arg.chars().count() + 3,
        ),
        Suffix::Option(o) => (
            o.key.chars().count(),
            o.description.chars().count() + o.arg.chars().count() + 3,
        ),
        Suffix::Action(a) => (a.key.chars().count(), a.description.chars().count()),
        Suffix::Info(i) => (i.keys.chars().count(), i.description.chars().count()),
        Suffix::Custom(c) => (c.key.chars().count(), c.description.chars().count()),
        Suffix::Variable(v) => (
            v.key.chars().count(),
            // description + the value/choices shown after it (rough).
            v.description.chars().count() + variable_value_width(v),
        ),
    };
    // Keycap: ~9px/char plus its border/padding. Description: ~7px/char at 13px,
    // plus the gap after the keycap and a little slack.
    (key as f32 * 9.0 + 16.0) + 8.0 + (text as f32 * 7.0) + 12.0
}

/// Rough char count of a config variable's rendered value/choices, so its cell
/// width estimate covers what's shown after the description.
/// The longest a variable's displayed value may run before it's elided — an
/// unbounded value (a long remote URL) would otherwise widen its column past
/// the window and push the sibling variables off-screen entirely.
const VARIABLE_VALUE_MAX: usize = 48;

/// A variable's value, elided in the middle past [`VARIABLE_VALUE_MAX`] (the
/// tail of a URL/refspec is usually the distinguishing part, so keep both ends).
fn elide_value(value: &str) -> String {
    let n = value.chars().count();
    if n <= VARIABLE_VALUE_MAX {
        return value.to_string();
    }
    let keep = (VARIABLE_VALUE_MAX - 1) / 2;
    let head: String = value.chars().take(keep).collect();
    let tail: String = value
        .chars()
        .skip(n - (VARIABLE_VALUE_MAX - 1 - keep))
        .collect();
    format!("{head}…{tail}")
}

fn variable_value_width(v: &transient::Variable) -> usize {
    match &v.kind {
        transient::VariableKind::Choices { choices, .. } => {
            // "[a|b|…]" — the choices plus separators/brackets, plus a fallback.
            choices.iter().map(|c| c.chars().count() + 1).sum::<usize>()
                + v.fallback_value
                    .as_ref()
                    .map_or(0, |f| f.chars().count().min(VARIABLE_VALUE_MAX) + 2)
                + 2
        }
        transient::VariableKind::Value { .. } => v
            .value
            .as_ref()
            .map_or(6, |val| val.chars().count().min(VARIABLE_VALUE_MAX) + 2),
    }
}

impl StatusView {
    pub(crate) fn render_transient(
        &self,
        def: &Transient,
        state: Option<&TransientState>,
        window: &Window,
        view: &Entity<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let pending_dash = state.is_some_and(|s| s.pending_dash);
        // Cap the argument band's columns to what fits the window width, so a
        // wide group (e.g. the log arguments) fans into more rows instead of
        // running off the right edge. Each group's column width is measured from
        // its widest suffix (keycap + description + flag), rather than assumed, so
        // narrow and wide bands both fill the width without overflowing. `px_3`
        // pads the panel on each side.
        let avail = f32::from(window.viewport_size().width) - 24.0;
        // How many columns of `group` fit in `width` px, given each column is its
        // widest cell plus the `gap_x_6` (24px) between sub-columns.
        let fit_columns = |group: &Group, width: f32| -> usize {
            let cell = group
                .suffixes
                .iter()
                .map(suffix_cell_px)
                .fold(0.0_f32, f32::max)
                .max(1.0);
            (((width + 24.0) / (cell + 24.0)).floor() as usize).max(1)
        };

        // Magit's layout, derived from content rather than hand-authored: an
        // *argument* group (switches/options) is a full-width band, and multiple
        // argument groups pack side-by-side; the *command* groups (actions/`?`
        // menu info) sit side by side in a wrapping row beneath them. Tall
        // transients lower the per-column row cap so groups fan into more
        // columns instead of consuming most of the screen.
        // This reproduces magit's commit transient (Arguments band over a row of
        // Create/Edit/… columns), the log transient (Arguments band over the Log
        // command row), and the `?` dispatch (all command groups → one packed
        // row), without a per-transient layout spec.
        // Git-config variable groups (magit's Configure section) lead the panel on
        // their own row, above the arguments and command groups.
        let has_config = |g: &&Group| g.suffixes.iter().any(|s| matches!(s, Suffix::Variable(_)));
        let has_args = |g: &&Group| {
            !has_config(g)
                && g.suffixes
                    .iter()
                    .any(|s| matches!(s, Suffix::Switch(_) | Suffix::Option(_)))
        };
        let config_groups = def.groups.iter().filter(has_config).collect::<Vec<_>>();
        let arg_groups = def.groups.iter().filter(has_args).collect::<Vec<_>>();
        let command_groups = def
            .groups
            .iter()
            .filter(|g| !has_config(g) && !has_args(g))
            .collect::<Vec<_>>();
        let group_rows = |group: &Group, cap: usize| {
            let n = group.suffixes.len();
            let cols = n.div_ceil(cap).max(1);
            n.div_ceil(cols).max(1)
        };
        let estimate_height = |cap: usize| {
            let arg_height = if arg_groups.len() <= 1 {
                arg_groups.first().map_or(0, |g| group_rows(g, cap))
            } else {
                arg_groups
                    .iter()
                    .map(|g| group_rows(g, cap))
                    .max()
                    .unwrap_or(0)
                    + arg_groups.len().saturating_sub(1)
            };
            let command_height = command_groups
                .iter()
                .map(|g| group_rows(g, cap))
                .max()
                .unwrap_or(0);
            let config_height = config_groups
                .iter()
                .map(|g| group_rows(g, cap))
                .max()
                .unwrap_or(0);
            config_height + arg_height + command_height
        };
        let band_cap = if estimate_height(7) < 10 {
            7
        } else if estimate_height(4) < 14 {
            4
        } else {
            3
        };

        let mut body = div().flex().flex_col().items_start().gap_3();
        // The Configure (git-config variable) groups lead, on their own row —
        // side by side among themselves, above the arguments and commands.
        if !config_groups.is_empty() {
            let mut config_row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_start()
                .gap_x_8()
                .gap_y_3();
            for group in &config_groups {
                let k = group.suffixes.len().div_ceil(band_cap).max(1);
                config_row =
                    config_row.child(self.render_group(group, k, state, pending_dash, view));
            }
            body = body.child(config_row);
        }
        if arg_groups.len() == 1 {
            let group = arg_groups[0];
            let k = group
                .suffixes
                .len()
                .div_ceil(band_cap)
                .max(1)
                .min(fit_columns(group, avail));
            body = body.child(self.render_group(group, k, state, pending_dash, view));
        } else if !arg_groups.is_empty() {
            let mut arg_row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_start()
                .gap_x_8()
                .gap_y_3();
            // Split the width budget across the side-by-side argument groups.
            let width_each = avail / arg_groups.len() as f32;
            for group in arg_groups {
                let k = group
                    .suffixes
                    .len()
                    .div_ceil(band_cap)
                    .max(1)
                    .min(fit_columns(group, width_each));
                arg_row = arg_row.child(self.render_group(group, k, state, pending_dash, view));
            }
            body = body.child(arg_row);
        }
        let mut command_row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap_x_8()
            .gap_y_3();
        let mut any_command = false;
        for group in command_groups {
            any_command = true;
            // A tall command group (e.g. the `?` dispatch's "Commands") fans into
            // sub-columns just like an argument band, so it doesn't tower over
            // the shorter groups beside it.
            let k = group.suffixes.len().div_ceil(band_cap).max(1);
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
        let show_save = state.is_some_and(|s| {
            !s.id.is_empty() && (s.active != s.baseline || s.values != s.baseline_values)
        });
        let has_repo = self.repo_scope_dir.is_some();

        // The band-cap heuristic keeps normal transients compact, but a short
        // window (or a huge user-extended menu) can still exceed the viewport;
        // cap the panel and let it scroll rather than clipping off-screen.
        let max_h = f32::from(window.viewport_size().height) * 0.8;
        self.bottom_panel()
            .id("transient-panel")
            .max_h(px(max_h))
            .overflow_y_scroll()
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
                            .child(kbd::key_chip(
                                "g",
                                self.palette.dim,
                                &self.font,
                                &self.system_ui_font,
                            ))
                            .child(SharedString::from("global")),
                    );
                if has_repo {
                    row = row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(kbd::key_chip(
                                "l",
                                self.palette.dim,
                                &self.font,
                                &self.system_ui_font,
                            ))
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
                        .child(kbd::key_chip(
                            TRANSIENT_SAVE_KEY,
                            self.palette.dim,
                            &self.font,
                            &self.system_ui_font,
                        ))
                        .child(SharedString::from("save these arguments as the default")),
                )
            })
            .when(!def.title.is_empty(), |el| {
                el.child(self.render_title(&def.title, self.palette.section))
            })
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
                // A collapsed push-remote/upstream entry shows both keys (`p/u`).
                let keycap = match a.also_key {
                    Some(also) => div()
                        .flex()
                        .items_center()
                        .gap(px(3.0))
                        .child(kbd::key_chip(
                            a.key,
                            self.palette.dim,
                            &self.font,
                            &self.system_ui_font,
                        ))
                        .child(div().text_color(self.palette.dim).child("/"))
                        .child(kbd::key_chip(
                            also,
                            self.palette.dim,
                            &self.font,
                            &self.system_ui_font,
                        ))
                        .into_any_element(),
                    None => {
                        kbd::key_chip(a.key, self.palette.dim, &self.font, &self.system_ui_font)
                    }
                };
                // A concrete remote ref is colored like one; placeholders and
                // non-ref actions ("elsewhere") use the normal foreground.
                let label_color = if a.ref_label {
                    self.palette.branch_remote
                } else {
                    self.palette.fg
                };
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
                    .child(keycap)
                    .child(self.hover_label(&a.description, label_color))
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
                        view.update(cx, |v, vcx| v.run_info_key(&key, window, vcx));
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
                    .child(kbd::key_chip(
                        &c.key,
                        self.palette.dim,
                        &self.font,
                        &self.system_ui_font,
                    ))
                    .child(self.hover_label(&c.description, self.palette.fg))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
            // A git-config variable (magit's Configure rows): keycap, name, then
            // the current value — cycling choices render `[a|b|fallback:x]` with
            // the active one accented; free-text shows `(value)` or a dim `unset`.
            Suffix::Variable(var) => {
                let view = view.clone();
                let key = SharedString::from(var.key.clone());
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
                    .child(kbd::key_chip(
                        &var.key,
                        self.palette.dim,
                        &self.font,
                        &self.system_ui_font,
                    ))
                    .child(self.hover_label(&var.description, self.palette.fg))
                    .child(self.render_variable_value(var))
                    .on_click(move |_, window, cx: &mut App| {
                        view.update(cx, |v, vcx| v.click_suffix(key.clone(), false, window, vcx));
                    })
                    .into_any_element()
            }
        }
    }

    /// The value cell of a config-variable row: the accented current value for a
    /// free-text variable (or a dim `unset`), or a `[a|b|fallback:x]` choice
    /// strip with the active choice accented.
    fn render_variable_value(&self, var: &transient::Variable) -> AnyElement {
        match &var.kind {
            transient::VariableKind::Value { .. } => match &var.value {
                Some(value) => div()
                    .text_color(self.palette.modified)
                    .child(SharedString::from(format!("({})", elide_value(value))))
                    .into_any_element(),
                None => div()
                    .text_color(self.palette.dim)
                    .child(SharedString::from("unset"))
                    .into_any_element(),
            },
            transient::VariableKind::Choices {
                choices, default, ..
            } => {
                let mut row = div().flex().items_center();
                row = row.child(div().text_color(self.palette.dim).child("["));
                for (i, choice) in choices.iter().enumerate() {
                    if i > 0 {
                        row = row.child(div().text_color(self.palette.dim).child("|"));
                    }
                    let active = var.value.as_deref() == Some(choice.as_str());
                    let cell = if active {
                        div()
                            .text_color(self.palette.modified)
                            .font_weight(FontWeight::BOLD)
                    } else {
                        div().text_color(self.palette.dim)
                    };
                    row = row.child(cell.child(SharedString::from(choice.clone())));
                }
                // When unset, show the inherited fallback value (or git's default)
                // so the effective setting is visible, magit-style.
                if var.value.is_none() {
                    if let Some(fallback) = &var.fallback_value {
                        row = row.child(
                            div()
                                .text_color(self.palette.dim)
                                .child(SharedString::from(format!("|→{}", elide_value(fallback)))),
                        );
                    } else if let Some(default) = default {
                        row = row.child(
                            div()
                                .text_color(self.palette.dim)
                                .child(SharedString::from(format!("|default:{default}"))),
                        );
                    }
                }
                row.child(div().text_color(self.palette.dim).child("]"))
                    .into_any_element()
            }
        }
    }
}

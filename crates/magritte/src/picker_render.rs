//! The bottom-anchored picker overlay (a vertico-style minibuffer): the
//! prompt with its inline query input, the fixed-height candidate list, and
//! the candidate rows. `impl StatusView` like the other view slices.

use gpui::{InteractiveElement, ParentElement, StatefulInteractiveElement};
use gpui_component::input::Input;

use crate::*;

/// How a picker's candidates are colored, derived from what it's choosing.
#[derive(Clone, Copy)]
pub(crate) enum PickerRefStyle {
    /// Branch names: remote-tracking (`origin/main`) green, local blue.
    Branchy,
    /// Tag names: yellow.
    Tag,
    /// Remote names: green.
    Remote,
}

/// The ref styling for a picker, or `None` for pickers whose candidates aren't
/// refs (commands, commit messages, ignore patterns, …).
fn picker_ref_style(action: &PickerAction) -> Option<PickerRefStyle> {
    match action {
        PickerAction::Branch(_) => Some(PickerRefStyle::Branchy),
        PickerAction::Tag(_) => Some(PickerRefStyle::Tag),
        PickerAction::Remote(_) => Some(PickerRefStyle::Remote),
        _ => None,
    }
}

/// The picker overlay as its own view: filtering and Up/Down notify THIS
/// entity, so a keystroke repaints only the bottom panel instead of the whole
/// window (title bar, status list, theme re-resolution) — that full-window
/// relayout was the `:`-palette typing lag.
pub(crate) struct PickerOverlay {
    pub(crate) parent: gpui::WeakEntity<StatusView>,
}

impl gpui::Render for PickerOverlay {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let Some(parent) = self.parent.upgrade() else {
            return div().into_any_element();
        };
        let this = parent.read(cx);
        match &this.popup {
            Some(Popup::Picker(state)) => this.render_picker(state, &parent).into_any_element(),
            _ => div().into_any_element(),
        }
    }
}

impl StatusView {
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
                        let ref_style = picker_ref_style(&p.action);
                        range
                            .map(|ix| match p.list.row(ix) {
                                Some(r) => {
                                    // Resolved once per label per picker (see
                                    // `PickerState::hints`), not per frame.
                                    let (hint, id) = if palette {
                                        let mut hints = p.hints.borrow_mut();
                                        match hints.get(&r.label) {
                                            Some(pair) => pair.clone(),
                                            None => {
                                                let v = view.read(cx);
                                                // Palette labels are shown
                                                // placeholder-expanded; the
                                                // by-title lookups need the
                                                // configured title.
                                                let title = v.raw_command_title(&r.label);
                                                let pair = (
                                                    command_keys(
                                                        v.screen_bindings(),
                                                        &v.config,
                                                        &title,
                                                    )
                                                    .map(SharedString::from),
                                                    commands::command_id_for_title(
                                                        &v.config, &title,
                                                    )
                                                    .map(SharedString::from),
                                                );
                                                hints.insert(r.label.clone(), pair.clone());
                                                pair
                                            }
                                        }
                                    } else {
                                        (None, None)
                                    };
                                    view.read(cx).render_picker_row(
                                        ix,
                                        r.label,
                                        r.is_create,
                                        ix == p.list.selected(),
                                        hint,
                                        id,
                                        ref_style,
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

        self.bottom_panel()
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

    /// The color a picker candidate's label takes, given the picker's ref style
    /// (from its action): branch pickers color remote-tracking entries green and
    /// local ones blue, tag pickers yellow, remote pickers green; other pickers
    /// (commands, messages, paths) use the plain foreground.
    fn picker_label_color(&self, label: &str, style: Option<PickerRefStyle>) -> Hsla {
        match style {
            Some(PickerRefStyle::Tag) => self.palette.tag,
            Some(PickerRefStyle::Remote) => self.palette.branch_remote,
            Some(PickerRefStyle::Branchy) if label.contains('/') => self.palette.branch_remote,
            Some(PickerRefStyle::Branchy) => self.palette.branch_local,
            None => self.palette.fg,
        }
    }

    /// One candidate row: a full-width highlight when current (vertico-style, no
    /// boxy border), a subtle hover for the mouse, and click-to-confirm.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_picker_row(
        &self,
        ix: usize,
        label: SharedString,
        is_create: bool,
        selected: bool,
        hint: Option<SharedString>,
        id: Option<SharedString>,
        ref_style: Option<PickerRefStyle>,
        view: &Entity<Self>,
    ) -> AnyElement {
        let view = view.clone();
        let mut el = div()
            .id(("picker-row", ix))
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
            div()
                .text_color(self.picker_label_color(&label, ref_style))
                .child(label)
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
        // The command id, dim and italic, at the row's end — the handle a user
        // needs for a `[keymap]` binding, surfaced right where they'd discover it.
        if let Some(id) = id {
            el = el.child(
                div()
                    .ml_auto()
                    .pr(px(ROW_PAD_LEFT))
                    .italic()
                    .text_color(self.palette.dim)
                    .font_family(self.font.clone())
                    .child(id),
            );
        }
        el.into_any_element()
    }
}

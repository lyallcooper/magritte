//! The settings screen: the appearance/theme/font/editor dropdowns and commit
//! -editor toggles, applied live (no save button). Its own concern — option-list
//! data, the GPUI select/input widgets, their subscriptions, and the live
//! config persistence — split out of the main view file.

#![allow(clippy::too_many_arguments)]

use gpui::prelude::*;
use gpui::{Context, Entity, ScrollHandle, SharedString, Subscription, Window};
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;

use crate::*;

/// The appearance options, in display order. Label paired with config value.
const APPEARANCE_OPTIONS: [(&str, &str); 3] = [
    ("Auto (system)", "auto"),
    ("Light", "light"),
    ("Dark", "dark"),
];

/// The live settings screen, built from gpui-component `Select` dropdowns (each
/// with built-in mouse + keyboard handling). Tab cycles focus between them;
/// confirming a selection applies it live.
pub(crate) struct SettingsState {
    appearance: Entity<SelectState<Vec<SharedString>>>,
    light_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    dark_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    font: Entity<SelectState<SearchableVec<SharedString>>>,
    ui_font: Entity<SelectState<SearchableVec<SharedString>>>,
    /// External editor. macOS picks from a dropdown of detected editor apps
    /// (plus "System Default"); elsewhere it's a free-text command.
    #[cfg(target_os = "macos")]
    editor: Entity<SelectState<SearchableVec<SharedString>>>,
    #[cfg(not(target_os = "macos"))]
    editor: Entity<InputState>,
    /// External commit-message editor command (free text, e.g. `zed --wait`).
    commit_editor: Entity<InputState>,
    /// Which control Tab focuses next (0=appearance, 1=light, 2=dark, 3=font,
    /// 4=ui_font, 5=editor, 6=commit_editor).
    focus_ix: usize,
    scroll: ScrollHandle,
    /// Kept alive so the Confirm subscriptions stay active.
    _subs: Vec<Subscription>,
}

impl StatusView {
    /// Open the live settings screen: four `Select` dropdowns (appearance,
    /// light theme, dark theme, font), each applying its selection immediately.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut theme_names: Vec<SharedString> = gpui_component::ThemeRegistry::global(cx)
            .sorted_themes()
            .iter()
            .map(|t| t.name.clone())
            // gpui-component always seeds its built-in "Default Light/Dark", which
            // we can't remove from the registry — hide them so only our authored
            // themes are offered.
            .filter(|n| n.as_ref() != "Default Light" && n.as_ref() != "Default Dark")
            .collect();
        theme_names.sort_by_key(|n| n.to_lowercase());

        let row = |ix: usize| Some(IndexPath::default().row(ix));
        let appearance_ix = APPEARANCE_OPTIONS
            .iter()
            .position(|(_, v)| *v == self.config.appearance)
            .unwrap_or(0);
        let pos = |list: &[SharedString], want: &str| {
            list.iter().position(|n| n.as_ref() == want).unwrap_or(0)
        };
        let light_ix = pos(&theme_names, self.config.light_theme());
        let dark_ix = pos(&theme_names, self.config.dark_theme());

        if self.mono_fonts.is_empty() {
            self.mono_fonts = theme::monospace_font_names(cx);
        }
        self.editors = editors::text_editors();
        // Lead with a "System Default" entry (maps to an empty config value, so
        // it follows the OS monospace); the rest are concrete families.
        let mut font_items: Vec<SharedString> = vec![SharedString::from(theme::SYSTEM_FONT_LABEL)];
        font_items.extend(self.mono_fonts.iter().cloned());
        let font_ix = if self.config.font.is_empty() {
            0
        } else {
            pos(&font_items, self.config.font.as_str())
        };

        if self.ui_fonts.is_empty() {
            self.ui_fonts = theme::all_font_names(cx);
        }
        // Lead with "Same as monospace" (empty config = the monospace UI we had
        // before opting in) and "System Default" (the platform proportional
        // font); the rest are concrete families.
        let mut ui_font_items: Vec<SharedString> = vec![
            SharedString::from(theme::UI_FONT_DEFAULT_LABEL),
            SharedString::from(theme::SYSTEM_FONT_LABEL),
        ];
        ui_font_items.extend(self.ui_fonts.iter().cloned());
        let ui_font_ix = match self.config.ui_font.as_str() {
            "" => 0,
            theme::SYSTEM_UI_FONT => 1,
            name => pos(&ui_font_items, name),
        };

        let appearance_items: Vec<SharedString> = APPEARANCE_OPTIONS
            .iter()
            .map(|(label, _)| SharedString::from(*label))
            .collect();

        let appearance =
            cx.new(|cx| SelectState::new(appearance_items, row(appearance_ix), &mut *window, cx));
        let light_theme = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(theme_names.clone()),
                row(light_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        let dark_theme = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(theme_names),
                row(dark_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        let font = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(font_items),
                row(font_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        let ui_font = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(ui_font_items),
                row(ui_font_ix),
                &mut *window,
                cx,
            )
            .searchable(true)
        });
        // macOS: a dropdown of detected editor apps, led by "System Default"
        // (open in the OS default app). A command set via the config file that
        // isn't a detected app is injected so it stays selectable, not lost.
        #[cfg(target_os = "macos")]
        let editor = {
            let cur = self.config.editor.trim().to_string();
            let mut editor_items: Vec<SharedString> =
                vec![SharedString::from(editors::EDITOR_OS_DEFAULT_LABEL)];
            if !cur.is_empty() && !self.editors.iter().any(|(n, _)| n.as_ref() == cur) {
                editor_items.push(SharedString::from(cur.clone()));
            }
            editor_items.extend(self.editors.iter().map(|(n, _)| n.clone()));
            let editor_ix = if cur.is_empty() {
                0
            } else {
                editor_items
                    .iter()
                    .position(|n| n.as_ref() == cur)
                    .unwrap_or(0)
            };
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(editor_items),
                    row(editor_ix),
                    &mut *window,
                    cx,
                )
                .searchable(true)
            })
        };
        #[cfg(not(target_os = "macos"))]
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. code -w, zed (OS default if empty)")
                .default_value(self.config.editor.clone())
        });
        let commit_editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. zed --wait")
                .default_value(self.config.commit_editor.clone())
        });

        let subs = vec![
            cx.subscribe_in(
                &commit_editor,
                window,
                |this, input, ev: &InputEvent, _w, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let val = input.read(cx).value().trim().to_string();
                        this.edit_global(|c| c.commit_editor = val.clone());
                        config::save(&this.config_global);
                    }
                },
            ),
            #[cfg(target_os = "macos")]
            cx.subscribe_in(
                &editor,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, _cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        let val = if name.as_ref() == editors::EDITOR_OS_DEFAULT_LABEL {
                            String::new()
                        } else {
                            name.to_string()
                        };
                        this.edit_global(|c| c.editor = val.clone());
                        config::save(&this.config_global);
                    }
                },
            ),
            #[cfg(not(target_os = "macos"))]
            cx.subscribe_in(&editor, window, |this, input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    let val = input.read(cx).value().trim().to_string();
                    this.edit_global(|c| c.editor = val.clone());
                    config::save(&this.config_global);
                }
            }),
            cx.subscribe_in(
                &appearance,
                window,
                |this, _, ev: &SelectEvent<Vec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(label)) = ev {
                        let value = APPEARANCE_OPTIONS
                            .iter()
                            .find(|(l, _)| *l == label.as_ref())
                            .map_or("auto", |(_, v)| v);
                        this.edit_global(|c| c.appearance = value.to_string());
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &light_theme,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.edit_global(|c| c.light_theme = name.to_string());
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &dark_theme,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        this.edit_global(|c| c.dark_theme = name.to_string());
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &font,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        // "System Default" → empty config (adaptive system mono).
                        let val = if name.as_ref() == theme::SYSTEM_FONT_LABEL {
                            String::new()
                        } else {
                            name.to_string()
                        };
                        this.edit_global(|c| c.font = val.clone());
                        this.font = theme::resolve_font(&this.config, cx);
                        // The UI font may track the editor font ("Same as
                        // editor"), so re-resolve it too.
                        this.ui_font = theme::resolve_ui_font(&this.config, cx);
                        this.apply_and_save(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &ui_font,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _w, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        let val = match name.as_ref() {
                            // Reuse the monospace font (no proportional UI).
                            theme::UI_FONT_DEFAULT_LABEL => String::new(),
                            // Platform proportional UI font.
                            theme::SYSTEM_FONT_LABEL => theme::SYSTEM_UI_FONT.to_string(),
                            other => other.to_string(),
                        };
                        this.edit_global(|c| c.ui_font = val.clone());
                        this.ui_font = theme::resolve_ui_font(&this.config, cx);
                        this.apply_and_save(cx);
                    }
                },
            ),
        ];

        appearance.update(cx, |st, cx| st.focus(window, cx));
        self.screen = Screen::Settings(SettingsState {
            appearance,
            light_theme,
            dark_theme,
            font,
            ui_font,
            editor,
            commit_editor,
            focus_ix: 0,
            scroll: ScrollHandle::new(),
            _subs: subs,
        });
        cx.notify();
    }

    /// Apply a global-settings change to both the live merged config (for the
    /// instant preview) and the global-only config that's persisted — so an
    /// in-app save writes just the user's edit to the global file, never the
    /// repo overlay that's merged into `self.config`.
    pub(crate) fn edit_global(&mut self, edit: impl Fn(&mut config::Config)) {
        edit(&mut self.config);
        edit(&mut self.config_global);
    }

    /// Re-apply the theme for the current config and persist the global config.
    pub(crate) fn apply_and_save(&mut self, cx: &mut Context<Self>) {
        self.reapply_theme(cx);
        config::save(&self.config_global);
    }

    /// Tab moves focus to the next settings control, cycling through every one
    /// of them (the dropdowns have distinct `SelectState` types and the editor
    /// fields are `Select`/`Input`, so each arm focuses its own entity).
    pub(crate) fn cycle_settings_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.settings_mut() else {
            return;
        };
        s.focus_ix = (s.focus_ix + 1) % 7;
        match s.focus_ix {
            0 => s
                .appearance
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            1 => s
                .light_theme
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            2 => s
                .dark_theme
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            3 => s.font.clone().update(cx, |st, cx| st.focus(window, cx)),
            4 => s.ui_font.clone().update(cx, |st, cx| st.focus(window, cx)),
            5 => s.editor.clone().update(cx, |st, cx| st.focus(window, cx)),
            _ => s
                .commit_editor
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
        }
    }

    /// Close the settings screen, persisting and returning focus to the list.
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        config::save(&self.config);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Render the live settings screen as a form of dropdowns. The `Select`
    /// components carry their own mouse + keyboard handling; Tab moves between
    /// them, Esc closes.
    pub(crate) fn render_settings(&self, s: &SettingsState, view: &Entity<Self>) -> impl IntoElement {
        // A labelled control row: fixed-width label + the control.
        let field = |id: &'static str, label: &str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(130.0))
                        .flex_shrink_0()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(label.to_string())),
                )
                .child(
                    div()
                        .relative()
                        .w(px(320.0))
                        .child(track_target(id))
                        .child(control),
                )
        };
        // A titled group: an uppercase heading over a bordered card of rows.
        let section = |title: &str, rows: Vec<gpui::Div>| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .px_1()
                        .text_xs()
                        .text_color(self.palette.dim)
                        .child(SharedString::from(title.to_uppercase())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(self.palette.border)
                        .p_3()
                        .children(rows),
                )
        };

        // The content column: width-capped and left-aligned. Wrapped below in a
        // full-width scroll container so the scrollbar sits at the window edge.
        let content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(620.0))
            .p_4()
            .gap_4()
            .child(
                // Header: title on the left; actions on the right.
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.palette.section)
                            .child(SharedString::from("Settings")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            // The related config buttons group tightly; the
                            // unrelated "close" action sits further off.
                            .gap_5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(self.open_config_button(view))
                                    .when(self.repo_scope_dir.is_some(), |el| {
                                        el.child(self.open_repo_config_button(view))
                                    }),
                            )
                            .child(self.key_action(
                                "settings-close",
                                "esc",
                                "close",
                                view,
                                Self::close_settings,
                            )),
                    ),
            )
            .child(section(
                "Appearance",
                vec![
                    field(
                        "appearance",
                        "Mode",
                        Select::new(&s.appearance).into_any_element(),
                    ),
                    field(
                        "light-theme",
                        "Light theme",
                        Select::new(&s.light_theme)
                            .search_placeholder("Search themes")
                            .into_any_element(),
                    ),
                    field(
                        "dark-theme",
                        "Dark theme",
                        Select::new(&s.dark_theme)
                            .search_placeholder("Search themes")
                            .into_any_element(),
                    ),
                    field(
                        "font",
                        "Monospace font",
                        Select::new(&s.font)
                            .search_placeholder("Search fonts")
                            .into_any_element(),
                    ),
                    field(
                        "ui-font",
                        "UI font",
                        Select::new(&s.ui_font)
                            .search_placeholder("Search fonts")
                            .into_any_element(),
                    ),
                ],
            ))
            .child(section("Editor", {
                #[cfg(target_os = "macos")]
                let control = Select::new(&s.editor)
                    .search_placeholder("Search editors")
                    .into_any_element();
                #[cfg(not(target_os = "macos"))]
                let control = Input::new(&s.editor).into_any_element();
                vec![
                    field("editor", "External editor", control).child(self.info_icon(
                        "editor-info".to_string(),
                        "The editor used when opening a file",
                    )),
                ]
            }))
            .child(section(
                "Behavior",
                vec![
                    field(
                        "refresh-on-focus",
                        "Refresh on focus",
                        self.toggle_control(
                            "refresh-on-focus",
                            self.config.refresh_on_focus,
                            "Refresh the status view automatically when window regains focus.",
                            view,
                            false,
                            |cfg, on| cfg.refresh_on_focus = on,
                        ),
                    ),
                    field(
                        "show-tags",
                        "Tags in title bar",
                        self.toggle_control(
                            "show-tags",
                            self.config.show_tags,
                            "Show the nearest tag(s) (e.g. `Tag: v1.0 (5)`) in the title bar.",
                            view,
                            // Needs the tag data fetched, so refresh on toggle.
                            true,
                            |cfg, on| cfg.show_tags = on,
                        ),
                    ),
                ],
            ))
            .child(section("Commit editor", {
                let mut rows = vec![field(
                    "commit-in-editor",
                    "Use external editor",
                    self.toggle_control(
                        "commit-in-editor",
                        self.config.commit_in_editor,
                        "Write commit messages with the editor command below (an interactive \
                         `git commit`) instead of the built-in editor.",
                        view,
                        false,
                        |cfg, on| cfg.commit_in_editor = on,
                    ),
                )];
                // With the external editor on, only its command is relevant; the
                // built-in editor's ruler/wrap aids don't apply, so hide them.
                if self.config.commit_in_editor {
                    rows.push(field(
                        "commit-editor",
                        "Editor command",
                        Input::new(&s.commit_editor).into_any_element(),
                    ));
                } else {
                    rows.push(field(
                        "commit-title-ruler",
                        "Summary ruler",
                        self.toggle_control(
                            "commit-title-ruler",
                            self.config.commit_title_ruler,
                            "Underlines characters past column 50 on the commit summary (first) \
                             line.",
                            view,
                            false,
                            |cfg, on| cfg.commit_title_ruler = on,
                        ),
                    ));
                    rows.push(field(
                        "commit-body-wrap",
                        "Body auto-wrap",
                        self.toggle_control(
                            "commit-body-wrap",
                            self.config.commit_body_wrap,
                            "Hard-wraps the commit body at 72 columns as you type at the end of a \
                             line (the summary line is never wrapped).",
                            view,
                            false,
                            |cfg, on| cfg.commit_body_wrap = on,
                        ),
                    ));
                }
                rows
            }));

        // Use the same two-layer shape as the virtualized views: the inner
        // element owns the scroll handle, and the outer full-height layer renders
        // the scrollbar. That keeps the thumb sized to the visible settings
        // viewport (below the title bar), not to the form content itself.
        div()
            .relative()
            .w_full()
            .flex_1()
            .child(
                div()
                    .id("settings-scroll")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .overflow_y_scroll()
                    .track_scroll(&s.scroll)
                    .child(content)
            )
            .vertical_scrollbar(&s.scroll)
    }

    /// Write the current config (so the file exists even if untouched) and open
    /// it via the same path as opening a file at point: the configured editor,
    /// falling back to the OS default app when it's unset. (The split button's
    /// dropdown still opens a chosen app.)
    pub(crate) fn open_config_file(&self) {
        if let Some(path) = self.saved_config_path() {
            self.launch_editor(&path, None);
        }
    }


    /// Copy the config file's path to the clipboard.
    pub(crate) fn copy_config_path(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = config::path() {
            self.copy_to_clipboard(path.to_string_lossy().into_owned(), cx);
        }
    }
}

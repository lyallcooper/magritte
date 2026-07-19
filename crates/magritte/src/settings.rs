//! The settings screen: the appearance/theme/font/editor dropdowns and commit
//! -editor toggles, applied live (no save button). Its own concern — option-list
//! data, the GPUI select/input widgets, their subscriptions, and the live
//! config persistence — split out of the main view file.

#![allow(clippy::too_many_arguments)]

use gpui::prelude::*;
use gpui::{Context, Entity, ScrollHandle, SharedString, Subscription, Window};
use gpui_component::button::{Button, ButtonGroup, DropdownButton};
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::switch::Switch;
use gpui_component::{IconName, Selectable, Sizable};

use crate::*;

/// Discovered option lists the settings screen needs (font families, installed
/// GUI editors). Cached on the view so reopening settings doesn't re-query the
/// system font list; only used by this screen.
#[derive(Default)]
pub(crate) struct SettingsCaches {
    /// Monospace font families (computed on first settings open).
    pub(crate) mono_fonts: Vec<SharedString>,
    /// All font families, for the UI-font picker.
    pub(crate) ui_fonts: Vec<SharedString>,
    /// Installed GUI editors, as (display name, .app path), for the settings
    /// "Open config file" dropdown. Computed once (first use or the startup
    /// prewarm) and kept for the session — a newly installed editor appears
    /// after a relaunch.
    pub(crate) editors: Vec<(SharedString, SharedString)>,
}

/// The appearance options, in display order. Label paired with config value.
const APPEARANCE_OPTIONS: [(&str, &str); 3] = [
    ("Auto (system)", "auto"),
    ("Light", "light"),
    ("Dark", "dark"),
];

/// The keymap presets, in display order. Label paired with the config value.
const KEYMAP_OPTIONS: [(&str, config::KeymapPreset); 2] = [
    ("Evil/Vim", config::KeymapPreset::EvilCollection),
    ("Vanilla Emacs", config::KeymapPreset::Vanilla),
];

const BEHAVIOR_GLOBAL_COLUMN_WIDTH: f32 = 44.0;
const BEHAVIOR_SCOPE_COLUMNS_WIDTH: f32 = 180.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum BehaviorSetting {
    AutoRefresh,
    RefreshOnFocus,
    ShowTags,
    CheckForUpdates,
}

impl BehaviorSetting {
    const ALL: [Self; 4] = [
        Self::AutoRefresh,
        Self::RefreshOnFocus,
        Self::ShowTags,
        Self::CheckForUpdates,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn id(self) -> &'static str {
        match self {
            Self::AutoRefresh => "auto-refresh",
            Self::RefreshOnFocus => "refresh-on-focus",
            Self::ShowTags => "show-tags",
            Self::CheckForUpdates => "check-for-updates",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::AutoRefresh => "auto_refresh",
            Self::RefreshOnFocus => "refresh_on_focus",
            Self::ShowTags => "show_tags_in_title_bar",
            Self::CheckForUpdates => "check_for_updates",
        }
    }

    fn alias(self) -> Option<&'static str> {
        matches!(self, Self::ShowTags).then_some("show_tags")
    }

    fn supports_repo_override(self) -> bool {
        self != Self::CheckForUpdates
    }

    fn get(self, config: &config::Config) -> bool {
        match self {
            Self::AutoRefresh => config.auto_refresh,
            Self::RefreshOnFocus => config.refresh_on_focus,
            Self::ShowTags => config.show_tags_in_title_bar,
            Self::CheckForUpdates => config.check_for_updates,
        }
    }

    fn set(self, config: &mut config::Config, value: bool) {
        match self {
            Self::AutoRefresh => config.auto_refresh = value,
            Self::RefreshOnFocus => config.refresh_on_focus = value,
            Self::ShowTags => config.show_tags_in_title_bar = value,
            Self::CheckForUpdates => config.check_for_updates = value,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BehaviorOverrides {
    values: [Option<bool>; BehaviorSetting::ALL.len()],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettingsLayout {
    stack_columns: bool,
    stack_fields: bool,
    stack_behavior_rows: bool,
}

impl SettingsLayout {
    fn for_width(window_width: f32) -> Self {
        let stack_columns = window_width < 700.0;
        // Mirror the rendered max width, page padding, column gap/split, and
        // card padding so Behavior changes layout as one section.
        let page_width = window_width.min(880.0);
        let columns_width = (page_width - 32.0).max(0.0);
        let behavior_card_width = if stack_columns {
            columns_width
        } else {
            ((columns_width - 16.0).max(0.0)) * 0.45
        };
        let behavior_inner_width = (behavior_card_width - 26.0).max(0.0);
        Self {
            stack_columns,
            stack_fields: window_width < 520.0,
            stack_behavior_rows: behavior_inner_width < 340.0,
        }
    }
}

impl BehaviorOverrides {
    fn load(path: Option<&std::path::Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let mut overrides = Self::default();
        for setting in BehaviorSetting::ALL {
            overrides.set(
                setting,
                config::load_bool_override_at(path, setting.key(), setting.alias()),
            );
        }
        overrides
    }

    fn get(self, setting: BehaviorSetting) -> Option<bool> {
        self.values[setting.index()]
    }

    fn set(&mut self, setting: BehaviorSetting, value: Option<bool>) {
        self.values[setting.index()] = value;
    }
}

/// The live settings screen, built from gpui-component `Select` dropdowns (each
/// with built-in mouse + keyboard handling). Tab cycles focus between them;
/// confirming a selection applies it live.
pub(crate) struct SettingsState {
    appearance: Entity<SelectState<Vec<SharedString>>>,
    light_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    dark_theme: Entity<SelectState<SearchableVec<SharedString>>>,
    font: Entity<SelectState<SearchableVec<SharedString>>>,
    ui_font: Entity<SelectState<SearchableVec<SharedString>>>,
    font_size: Entity<SelectState<Vec<SharedString>>>,
    /// External editor. macOS picks from a dropdown of detected editor apps
    /// (plus "System Default"); elsewhere it's a free-text command.
    #[cfg(target_os = "macos")]
    editor: Entity<SelectState<SearchableVec<SharedString>>>,
    #[cfg(not(target_os = "macos"))]
    editor: Entity<InputState>,
    /// External commit-message editor command (free text, e.g. `zed --wait`).
    commit_editor: Entity<InputState>,
    /// Keymap preset (Evil/Vim vs Vanilla Emacs).
    keymap_preset: Entity<SelectState<Vec<SharedString>>>,
    /// Explicit repo values for the Behavior card. `None` inherits Global.
    behavior_overrides: BehaviorOverrides,
    /// Which control Tab focuses next (0=appearance, 1=light, 2=dark, 3=font,
    /// 4=ui_font, 5=font_size, 6=editor, 7=keymap_preset, 8=commit_editor).
    focus_ix: usize,
    scroll: ScrollHandle,
    /// Kept alive so the Confirm subscriptions stay active.
    _subs: Vec<Subscription>,
}

impl StatusView {
    /// Compute the settings screen's font/editor lists on a background thread at
    /// startup and cache them, so the first "open settings" doesn't stall on the
    /// (slow) system font enumeration and per-font monospace probing.
    pub(crate) fn prewarm_settings_caches(&self, cx: &mut Context<Self>) {
        let text_system = cx.text_system().clone();
        let task = cx.background_executor().spawn(async move {
            (
                theme::monospace_font_names(text_system.as_ref()),
                theme::all_font_names(text_system.as_ref()),
                editors::text_editors(),
            )
        });
        cx.spawn(async move |this, cx| {
            let (mono, ui, editors) = task.await;
            this.update(cx, |this, _| {
                let c = &mut this.settings_caches;
                // Don't clobber a list the user already triggered a compute for.
                if c.mono_fonts.is_empty() {
                    c.mono_fonts = mono;
                }
                if c.ui_fonts.is_empty() {
                    c.ui_fonts = ui;
                }
                if c.editors.is_empty() {
                    c.editors = editors;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Subscribe to a settings `Select`'s confirm event, invoking `on_confirm`
    /// with the chosen item — folding away the `subscribe_in` + `SelectEvent::
    /// Confirm(Some(..))` unwrap each dropdown otherwise repeats.
    fn on_select_confirm<T>(
        entity: &Entity<SelectState<T>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_confirm: impl Fn(&mut Self, &SharedString, &mut Context<Self>) + 'static,
    ) -> Subscription
    where
        T: gpui_component::searchable_list::SearchableListDelegate + 'static,
        T::Item: gpui_component::searchable_list::SearchableListItem<Value = SharedString>,
    {
        cx.subscribe_in(
            entity,
            window,
            move |this, _, ev: &SelectEvent<T>, _w, cx| {
                if let SelectEvent::Confirm(Some(value)) = ev {
                    on_confirm(this, value, cx);
                }
            },
        )
    }

    /// Open the live settings screen: appearance/theme/font/keymap dropdowns,
    /// editor commands, and behavior toggles — every control applying its
    /// change immediately (no save button).
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let behavior_overrides = BehaviorOverrides::load(
            self.repo_scope_dir
                .as_ref()
                .map(|dir| dir.join("config.toml"))
                .as_deref(),
        );
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

        // These lists are normally prewarmed in the background at startup (see
        // `prewarm_settings_caches`); compute any that aren't ready yet.
        if self.settings_caches.mono_fonts.is_empty() {
            self.settings_caches.mono_fonts =
                theme::monospace_font_names(cx.text_system().as_ref());
        }
        if self.settings_caches.editors.is_empty() {
            self.settings_caches.editors = editors::text_editors();
        }
        // Lead with a "System Default" entry (maps to an empty config value, so
        // it follows the OS monospace); the rest are concrete families. A
        // configured family missing from the detected list (a repo-overlay
        // font, or one the monospace-trait probe misses) is injected so it
        // stays selectable rather than misreported as "System Default".
        let mut font_items: Vec<SharedString> = vec![SharedString::from(theme::SYSTEM_FONT_LABEL)];
        let cur_font = self.config.font.as_str();
        if !cur_font.is_empty()
            && !self
                .settings_caches
                .mono_fonts
                .iter()
                .any(|n| n.as_ref() == cur_font)
        {
            font_items.push(SharedString::from(cur_font.to_string()));
        }
        font_items.extend(self.settings_caches.mono_fonts.iter().cloned());
        let font_ix = if self.config.font.is_empty() {
            0
        } else {
            pos(&font_items, self.config.font.as_str())
        };

        if self.settings_caches.ui_fonts.is_empty() {
            self.settings_caches.ui_fonts = theme::all_font_names(cx.text_system().as_ref());
        }
        // Lead with "Same as monospace" (empty config = the monospace UI we had
        // before opting in) and "System Default" (the platform proportional
        // font); the rest are concrete families.
        let mut ui_font_items: Vec<SharedString> = vec![
            SharedString::from(theme::UI_FONT_DEFAULT_LABEL),
            SharedString::from(theme::SYSTEM_FONT_LABEL),
        ];
        // Same off-list injection as the monospace picker.
        let cur_ui_font = self.config.ui_font.as_str();
        if !cur_ui_font.is_empty()
            && cur_ui_font != theme::SYSTEM_UI_FONT
            && !self
                .settings_caches
                .ui_fonts
                .iter()
                .any(|n| n.as_ref() == cur_ui_font)
        {
            ui_font_items.push(SharedString::from(cur_ui_font.to_string()));
        }
        ui_font_items.extend(self.settings_caches.ui_fonts.iter().cloned());
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

        let keymap_items: Vec<SharedString> = KEYMAP_OPTIONS
            .iter()
            .map(|(label, _)| SharedString::from(*label))
            .collect();
        let keymap_ix = KEYMAP_OPTIONS
            .iter()
            .position(|(_, p)| *p == self.config.keymap_preset)
            .unwrap_or(0);
        let keymap_preset =
            cx.new(|cx| SelectState::new(keymap_items, row(keymap_ix), &mut *window, cx));
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
        // Font size choices (px), led by the platform default; an off-list
        // value from the config file is injected so it stays selectable
        // rather than silently remapped.
        let mut size_items: Vec<u32> = vec![10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24];
        let cur_size = self
            .config
            .font_size
            .filter(|&n| n != 0)
            .map(|n| n.clamp(9, 24));
        if let Some(cur) = cur_size {
            if !size_items.contains(&cur) {
                size_items.push(cur);
                size_items.sort_unstable();
            }
        }
        let size_ix = match cur_size {
            None => 0,
            Some(cur) => size_items
                .iter()
                .position(|&n| n == cur)
                .map_or(0, |i| i + 1),
        };
        let mut size_labels: Vec<SharedString> = vec![SharedString::from(format!(
            "{} ({} px)",
            theme::SYSTEM_FONT_LABEL,
            theme::system_font_size()
        ))];
        size_labels.extend(
            size_items
                .iter()
                .map(|n| SharedString::from(format!("{n} px"))),
        );
        let font_size = cx.new(|cx| SelectState::new(size_labels, row(size_ix), &mut *window, cx));
        // macOS: a dropdown of detected editor apps, led by "System Default"
        // (open in the OS default app). A command set via the config file that
        // isn't a detected app is injected so it stays selectable, not lost.
        #[cfg(target_os = "macos")]
        let editor = {
            let cur = self.config.editor.trim().to_string();
            let mut editor_items: Vec<SharedString> =
                vec![SharedString::from(editors::EDITOR_OS_DEFAULT_LABEL)];
            if !cur.is_empty()
                && !self
                    .settings_caches
                    .editors
                    .iter()
                    .any(|(n, _)| n.as_ref() == cur)
            {
                editor_items.push(SharedString::from(cur.clone()));
            }
            editor_items.extend(self.settings_caches.editors.iter().map(|(n, _)| n.clone()));
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
                        this.save_settings_debounced(cx);
                    }
                },
            ),
            #[cfg(target_os = "macos")]
            Self::on_select_confirm(&editor, window, cx, |this, name, cx| {
                let val = if name.as_ref() == editors::EDITOR_OS_DEFAULT_LABEL {
                    String::new()
                } else {
                    name.to_string()
                };
                this.edit_global(|c| c.editor = val.clone());
                this.save_global_config(cx);
            }),
            #[cfg(not(target_os = "macos"))]
            cx.subscribe_in(&editor, window, |this, input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    let val = input.read(cx).value().trim().to_string();
                    this.edit_global(|c| c.editor = val.clone());
                    this.save_settings_debounced(cx);
                }
            }),
            Self::on_select_confirm(&appearance, window, cx, |this, label, cx| {
                let value = APPEARANCE_OPTIONS
                    .iter()
                    .find(|(l, _)| *l == label.as_ref())
                    .map_or("auto", |(_, v)| v);
                this.edit_global(|c| c.appearance = value.to_string());
                this.apply_and_save(cx);
            }),
            Self::on_select_confirm(&keymap_preset, window, cx, |this, label, cx| {
                let preset = KEYMAP_OPTIONS
                    .iter()
                    .find(|(l, _)| *l == label.as_ref())
                    .map_or(config::KeymapPreset::default(), |(_, p)| *p);
                this.edit_global(|c| c.keymap_preset = preset);
                // The effective keymap is derived from the preset; rebuild it so
                // the change applies immediately.
                this.keymap = build_keymap(&this.config).0;
                this.apply_and_save(cx);
            }),
            Self::on_select_confirm(&light_theme, window, cx, |this, name, cx| {
                this.edit_global(|c| c.light_theme = name.to_string());
                this.apply_and_save(cx);
            }),
            Self::on_select_confirm(&dark_theme, window, cx, |this, name, cx| {
                this.edit_global(|c| c.dark_theme = name.to_string());
                this.apply_and_save(cx);
            }),
            Self::on_select_confirm(&font, window, cx, |this, name, cx| {
                // "System Default" → empty config (adaptive system mono).
                let val = if name.as_ref() == theme::SYSTEM_FONT_LABEL {
                    String::new()
                } else {
                    name.to_string()
                };
                this.edit_global(|c| c.font = val.clone());
                this.font = theme::resolve_font(&this.config, cx);
                // The UI font may track the editor font ("Same as editor"), so
                // re-resolve it too.
                this.ui_font = theme::resolve_ui_font(&this.config, cx);
                this.apply_and_save(cx);
            }),
            Self::on_select_confirm(&ui_font, window, cx, |this, name, cx| {
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
            }),
            Self::on_select_confirm(&font_size, window, cx, |this, label, cx| {
                if label.starts_with(theme::SYSTEM_FONT_LABEL) {
                    this.edit_global(|c| c.font_size = None);
                    this.apply_and_save(cx);
                } else if let Ok(n) = label.trim_end_matches(" px").parse::<u32>() {
                    this.edit_global(|c| c.font_size = Some(n));
                    this.apply_and_save(cx);
                }
            }),
        ];

        appearance.update(cx, |st, cx| st.focus(window, cx));
        self.screen = Screen::Settings(SettingsState {
            appearance,
            light_theme,
            dark_theme,
            font,
            ui_font,
            font_size,
            editor,
            commit_editor,
            keymap_preset,
            behavior_overrides,
            focus_ix: 0,
            scroll: ScrollHandle::new(),
            _subs: subs,
        });
        cx.notify();
    }

    /// The settings "Open … config" controls: a split button whose main half
    /// opens the file in the external editor / OS default app, and whose
    /// dropdown offers "Copy path". An escape hatch for settings the UI doesn't
    /// expose, and a way to see where each file lives. Menu items dispatch
    /// actions routed to the status view's focus.
    fn config_file_button(
        &self,
        id: &'static str,
        label: &'static str,
        copy_action: Box<dyn gpui::Action>,
        view: &Entity<Self>,
        open: fn(&mut Self, &mut Window, &mut Context<Self>),
    ) -> impl IntoElement {
        let focus = self.focus.clone();
        let main = Button::new(SharedString::from(format!("{id}-main")))
            .label(label)
            .outline()
            .xsmall()
            .icon(IconName::ExternalLink)
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    view.update(cx, |this, cx| open(this, window, cx));
                }
            });
        DropdownButton::new(id)
            .outline()
            .xsmall()
            .button(main)
            .dropdown_menu(move |menu, _window, _cx| {
                menu.action_context(focus.clone())
                    .menu("Copy path", copy_action.boxed_clone())
            })
    }

    pub(crate) fn open_config_button(&self, view: &Entity<Self>) -> impl IntoElement {
        self.config_file_button(
            "open-config",
            "Open global config",
            Box::new(CopyConfigPath),
            view,
            |this, _window, _cx| this.open_config_file(),
        )
    }

    /// Opens this repo's `.git/magritte/config.toml` (the per-repo overlay),
    /// creating it if absent. Shown only when there's a repo.
    pub(crate) fn open_repo_config_button(&self, view: &Entity<Self>) -> impl IntoElement {
        self.config_file_button(
            "open-repo-config",
            "Open repo config",
            Box::new(CopyRepoConfigPath),
            view,
            |this, window, cx| this.open_repo_config_file(window, cx),
        )
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
    pub(crate) fn open_repo_config_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.repo_scope_dir.clone() else {
            return;
        };
        let path = dir.join("config.toml");
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, "");
            // The scope dir may not have existed until now, in which case the
            // startup watcher couldn't cover it — re-install so edits to the
            // file we're about to open live-reload in this session.
            self.install_config_watcher(window, cx);
        }
        self.launch_editor(&path, None);
    }

    /// A settings toggle (a `Switch` bound to a `bool` config field). The switch
    /// flips the field and persists on click; like the rest of the settings
    /// screen, it is mouse-driven rather than part of the Tab focus ring.
    pub(crate) fn toggle_control(
        &self,
        id: &'static str,
        checked: bool,
        view: &Entity<Self>,
        // Whether flipping this toggle changes fetched data (e.g. the title-bar
        // tag segment) rather than just how the current data is painted. When
        // set, the change refreshes so it takes effect live; otherwise a repaint
        // suffices.
        refetch: bool,
        set: fn(&mut config::Config, bool),
    ) -> AnyElement {
        let switch = Switch::new(id).checked(checked).on_click({
            let view = view.clone();
            move |on, window, cx| {
                let on = *on;
                view.update(cx, |this, cx| {
                    // Apply to both the live merged config and the global-only
                    // config that's persisted, so the save doesn't leak the repo
                    // overlay into the global file.
                    set(&mut this.config, on);
                    set(&mut this.config_global, on);
                    this.save_global_config(cx);
                    // A toggle can hide the commit-editor input; pull a focus
                    // stranded on it back into the Tab ring.
                    this.clamp_settings_focus(window, cx);
                    if refetch {
                        this.refresh(cx);
                    } else {
                        cx.notify();
                    }
                });
            }
        });
        switch.into_any_element()
    }

    /// A Behavior row exposes its global switch and, for repo-scoped behavior,
    /// an explicit Inherit/On/Off override. The latter is intentionally
    /// tri-state so following Global never writes a redundant value that would
    /// mask later global edits.
    fn behavior_toggle_control(
        &self,
        setting: BehaviorSetting,
        repo_override: Option<bool>,
        show_scope_labels: bool,
        view: &Entity<Self>,
    ) -> AnyElement {
        let id = setting.id();
        let global = Switch::new(SharedString::from(format!("{id}-global")))
            .checked(setting.get(&self.config_global))
            .on_click({
                let view = view.clone();
                move |on, _window, cx| {
                    let on = *on;
                    view.update(cx, |this, cx| this.set_global_behavior(setting, on, cx));
                }
            });
        let repo = (self.repo_scope_dir.is_some() && setting.supports_repo_override()).then(|| {
            ButtonGroup::new(SharedString::from(format!("{id}-repo")))
                .outline()
                .compact()
                .xsmall()
                .child(
                    Button::new(SharedString::from(format!("{id}-inherit")))
                        .label("Inherit")
                        .selected(repo_override.is_none()),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-repo-on")))
                        .label("On")
                        .selected(repo_override == Some(true)),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-repo-off")))
                        .label("Off")
                        .selected(repo_override == Some(false)),
                )
                .on_click({
                    let view = view.clone();
                    move |selected, window, cx| {
                        let value = if selected.contains(&1) {
                            Some(true)
                        } else if selected.contains(&2) {
                            Some(false)
                        } else {
                            None
                        };
                        view.update(cx, |this, cx| {
                            this.set_repo_behavior_override(setting, value, window, cx);
                        });
                    }
                })
                .into_any_element()
        });

        div()
            .flex()
            .items_end()
            .gap_3()
            .when(self.repo_scope_dir.is_some(), |el| {
                el.min_w(px(BEHAVIOR_SCOPE_COLUMNS_WIDTH))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(BEHAVIOR_GLOBAL_COLUMN_WIDTH))
                    .gap_1()
                    .when(show_scope_labels, |el| {
                        el.child(div().text_xs().text_color(self.palette.dim).child("Global"))
                    })
                    .child(global),
            )
            .when_some(repo, |el, repo| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .when(show_scope_labels, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(self.palette.dim)
                                    .child("Repository"),
                            )
                        })
                        .child(repo),
                )
            })
            .into_any_element()
    }

    fn set_global_behavior(
        &mut self,
        setting: BehaviorSetting,
        value: bool,
        cx: &mut Context<Self>,
    ) {
        let old_effective = setting.get(&self.config);
        setting.set(&mut self.config_global, value);
        let repo_override = self
            .settings()
            .and_then(|settings| settings.behavior_overrides.get(setting));
        let effective = repo_override.unwrap_or(value);
        setting.set(&mut self.config, effective);
        self.save_global_config(cx);
        self.apply_behavior_change(setting, old_effective, effective, cx);
    }

    fn set_repo_behavior_override(
        &mut self,
        setting: BehaviorSetting,
        value: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .repo_scope_dir
            .as_ref()
            .map(|dir| dir.join("config.toml"))
        else {
            return;
        };
        if let Err(error) =
            config::save_bool_override_at(&path, setting.key(), setting.alias(), value)
        {
            self.set_status(error, false, cx);
            return;
        }
        if let Some(settings) = self.settings_mut() {
            settings.behavior_overrides.set(setting, value);
        }
        let old_effective = setting.get(&self.config);
        let effective = value.unwrap_or_else(|| setting.get(&self.config_global));
        setting.set(&mut self.config, effective);
        // The repo settings directory may have been created by this first
        // override, so make it part of the live config watcher immediately.
        self.install_config_watcher(window, cx);
        self.apply_behavior_change(setting, old_effective, effective, cx);
    }

    fn apply_behavior_change(
        &mut self,
        setting: BehaviorSetting,
        old_effective: bool,
        effective: bool,
        cx: &mut Context<Self>,
    ) {
        if old_effective == effective {
            cx.notify();
            return;
        }
        match setting {
            BehaviorSetting::AutoRefresh => self.configure_repository_monitor(cx),
            BehaviorSetting::RefreshOnFocus => cx.notify(),
            BehaviorSetting::ShowTags => self.refresh_with_origin(
                RefreshOrigin::Config,
                repo_monitor::ChangeScope::RepositoryWide,
                None,
                cx,
            ),
            BehaviorSetting::CheckForUpdates => self.start_update_checks(cx),
        }
        cx.notify();
    }

    /// A config-file edit can change which scope supplies a Behavior value
    /// without changing the effective merged value (for example, On → Inherit
    /// while Global is already on). Keep the two visible layers current even
    /// when the normal effective-config comparison is therefore unchanged.
    pub(crate) fn sync_unchanged_config_scopes(&mut self, cx: &mut Context<Self>) {
        let global = config::load_reporting().0;
        let global_changed = global != self.config_global;
        self.config_global = global;

        let overrides = BehaviorOverrides::load(
            self.repo_scope_dir
                .as_ref()
                .map(|dir| dir.join("config.toml"))
                .as_deref(),
        );
        let overrides_changed = self.settings_mut().is_some_and(|settings| {
            let changed = settings.behavior_overrides != overrides;
            settings.behavior_overrides = overrides;
            changed
        });
        if global_changed || overrides_changed {
            self.set_status("Settings reloaded from disk".to_string(), true, cx);
            cx.notify();
        }
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
        self.save_global_config(cx);
    }

    /// Persist the global config, surfacing a failed save — an unparseable
    /// on-disk file would otherwise silently drop the change while the live
    /// state shows it applied.
    pub(crate) fn save_global_config(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = config::save_settings(&self.config_global) {
            self.set_status(e, false, cx);
            return;
        }
        self.notice_repo_override(cx);
    }

    /// After persisting a global edit, detect the repo overlay reasserting a
    /// field the edit changed: adopt the effective merged config (so the file
    /// watcher's reload sees no difference — no "Settings reloaded" toast and
    /// no settings-form rebuild that would reset focus) and say explicitly
    /// that the repo config overrides the edit, instead of letting the value
    /// silently snap back.
    fn notice_repo_override(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.repo_scope_dir.as_ref().map(|d| d.join("config.toml")) else {
            return;
        };
        if !path.exists() {
            return;
        }
        let (merged, warning) = config::load_merged(Some(&path));
        if warning.is_some() || merged == self.config {
            return;
        }
        self.config = merged;
        self.font = theme::resolve_font(&self.config, cx);
        self.ui_font = theme::resolve_ui_font(&self.config, cx);
        self.keymap = build_keymap(&self.config).0;
        self.reapply_theme(cx);
        self.set_status(
            "Saved to the global config, but overridden by this repo's config".to_string(),
            false,
            cx,
        );
    }

    /// The number of Tab-focusable settings controls. The commit-editor input
    /// renders only when commit_in_editor is on; keep it out of the ring
    /// otherwise (a hidden control is a dead stop).
    fn settings_focus_ring(&self) -> usize {
        if self.config.commit_in_editor {
            9
        } else {
            8
        }
    }

    /// Pull a focus stranded past the ring's end back onto a visible control —
    /// toggling the external commit editor off hides its input while the
    /// (invisible) InputState would otherwise keep keyboard focus.
    fn clamp_settings_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ring = self.settings_focus_ring();
        let stranded = self
            .settings_mut()
            .filter(|s| s.focus_ix >= ring)
            .map(|s| s.focus_ix = ring - 1)
            .is_some();
        if stranded {
            self.focus_settings_control(window, cx);
        }
    }

    /// Tab moves focus to the next settings control, cycling through every one
    /// of them.
    pub(crate) fn cycle_settings_focus(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ring = self.settings_focus_ring();
        let Some(s) = self.settings_mut() else {
            return;
        };
        // The ring may have shrunk under the focus (commit-editor toggled off);
        // re-enter it from the last visible control.
        s.focus_ix = s.focus_ix.min(ring - 1);
        s.focus_ix = (s.focus_ix + if forward { 1 } else { ring - 1 }) % ring;
        self.focus_settings_control(window, cx);
    }

    /// Focus the settings control at the current `focus_ix` (the dropdowns have
    /// distinct `SelectState` types and the editor fields are `Select`/`Input`,
    /// so each arm focuses its own entity).
    fn focus_settings_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.settings_mut() else {
            return;
        };
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
            5 => s
                .font_size
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            6 => s.editor.clone().update(cx, |st, cx| st.focus(window, cx)),
            7 => s
                .keymap_preset
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
            _ => s
                .commit_editor
                .clone()
                .update(cx, |st, cx| st.focus(window, cx)),
        }
    }

    /// Close the settings screen, persisting and returning focus to the list.
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Flush any pending debounced save so closing can't drop the tail of a
        // free-text edit.
        self.flush_settings_save(cx);
        self.screen = Screen::Status;
        self.reconcile_visible_screen(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Write out a pending debounced settings save immediately (cancelling the
    /// outstanding timer) — closing the settings screen or the window must not
    /// drop the tail of a free-text edit.
    pub(crate) fn flush_settings_save(&mut self, cx: &mut Context<Self>) {
        if self.settings_save_pending {
            self.settings_save_gen.bump(); // cancel the outstanding timer
            self.settings_save_pending = false;
            self.save_global_config(cx);
        }
    }

    /// Persist the global config, debounced: the free-text settings inputs
    /// fire a Change per keystroke, and each save is a full read/parse/rewrite
    /// of the config file. The live config is already updated by the caller;
    /// only the disk write waits for the typing to pause.
    fn save_settings_debounced(&mut self, cx: &mut Context<Self>) {
        let gen = self.settings_save_gen.bump();
        self.settings_save_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            this.update(cx, |this, cx| {
                if this.settings_save_gen.is_current(gen) {
                    this.settings_save_pending = false;
                    this.save_global_config(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Render the live settings screen as a form of dropdowns. The `Select`
    /// components carry their own mouse + keyboard handling; Tab moves between
    /// them, Esc closes.
    pub(crate) fn render_settings(
        &self,
        s: &SettingsState,
        view: &Entity<Self>,
        window: &Window,
    ) -> impl IntoElement {
        let layout = SettingsLayout::for_width(f32::from(window.bounds().size.width));
        let stack_columns = layout.stack_columns;
        let stack_fields = layout.stack_fields;

        let setting_label = |id: &'static str, label: &str, explanation: Option<&'static str>| {
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .text_color(self.palette.dim)
                .child(SharedString::from(label.to_string()))
                .when_some(explanation, |el, explanation| {
                    el.child(self.info_icon(format!("{id}-info"), explanation))
                })
        };
        // A labelled control row: a fixed-width label with the control filling
        // the rest of the row. One control per row so everything left-aligns.
        let field = |id: &'static str,
                     label: &str,
                     explanation: Option<&'static str>,
                     control: AnyElement| {
            div()
                .flex()
                .gap_3()
                .when(stack_fields, |el| el.flex_col().gap_2())
                .when(!stack_fields, |el| el.items_center())
                .child(
                    div()
                        .flex_shrink_0()
                        .when(!stack_fields, |el| el.w(px(152.0)))
                        .child(setting_label(id, label, explanation)),
                )
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .w_full()
                        .min_w(px(0.0))
                        .child(track_target(id))
                        .child(control),
                )
        };
        // A labelled toggle row: label on the left, its global/repo control (or
        // a plain commit-editor switch) pinned to the right of the card.
        let toggle_field =
            |id: &'static str, label: &str, explanation: &'static str, control: AnyElement| {
                div()
                    .flex()
                    .w_full()
                    .gap_3()
                    .when(stack_fields, |el| el.flex_col().items_start().gap_2())
                    .when(!stack_fields, |el| {
                        el.flex_wrap().items_center().justify_between()
                    })
                    .child(setting_label(id, label, Some(explanation)))
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .child(track_target(id))
                            .child(control),
                    )
            };
        // Behavior rows share one layout decision so a longer label cannot
        // wrap by itself while its neighbors remain on one line. Their control
        // block reserves the same Global/Repository columns for every row.
        let behavior_field =
            |id: &'static str, label: &str, explanation: &'static str, control: AnyElement| {
                div()
                    .flex()
                    .w_full()
                    .gap_3()
                    .when(layout.stack_behavior_rows, |el| {
                        el.flex_col().items_start().gap_2()
                    })
                    .when(!layout.stack_behavior_rows, |el| {
                        el.items_center().justify_between()
                    })
                    .child(setting_label(id, label, Some(explanation)))
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .child(track_target(id))
                            .child(control),
                    )
            };
        // A titled group: an uppercase heading over a bordered card of rows.
        // Fills its masonry column.
        let section = |title: &str, rows: Vec<gpui::Div>| {
            div()
                .flex()
                .flex_col()
                .w_full()
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
        // Header: title on the left; actions on the right.
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
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
                    // The related config buttons group tightly; the unrelated
                    // "close" action sits further off.
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
            );

        let appearance = section("Appearance", {
            let mut rows = vec![
                field(
                    "appearance",
                    "Mode",
                    None,
                    Select::new(&s.appearance).into_any_element(),
                ),
                field(
                    "light-theme",
                    "Light theme",
                    None,
                    Select::new(&s.light_theme)
                        .search_placeholder("Search themes")
                        .into_any_element(),
                ),
                field(
                    "dark-theme",
                    "Dark theme",
                    None,
                    Select::new(&s.dark_theme)
                        .search_placeholder("Search themes")
                        .into_any_element(),
                ),
                field(
                    "font",
                    "Monospace font",
                    None,
                    Select::new(&s.font)
                        .search_placeholder("Search fonts")
                        .into_any_element(),
                ),
                field(
                    "ui-font",
                    "UI font",
                    None,
                    Select::new(&s.ui_font)
                        .search_placeholder("Search fonts")
                        .into_any_element(),
                ),
                field(
                    "font-size",
                    "Font size",
                    None,
                    Select::new(&s.font_size).into_any_element(),
                ),
            ];
            // The app-icon switcher sets the macOS Dock icon; no effect
            // elsewhere, so it's macOS-only. A radio of the icon images
            // themselves (no labels) — click one to select it.
            #[cfg(target_os = "macos")]
            {
                let current = app_icon::resolved_icon(&self.config.app_icon);
                let cell = |id: &'static str, thumb: &'static [u8], selected: bool| {
                    // Every cell has a stroke hugging its image: a 2px accent for
                    // the selected one, a thin subtle border otherwise. The image
                    // grows by the border difference (56 vs 58) so image+border is
                    // always 60px and the row never shifts. The image is rounded
                    // flush inside the stroke (outer radius 13, inner = 13 minus
                    // the stroke width).
                    let (img_size, img_radius) = if selected { (56.0, 11.0) } else { (58.0, 12.0) };
                    div()
                        .id(SharedString::from(format!("app-icon-{id}")))
                        .cursor_pointer()
                        .rounded(px(13.0))
                        .when(selected, |el| {
                            el.border_2().border_color(self.palette.section)
                        })
                        .when(!selected, |el| {
                            el.border_1().border_color(self.palette.border)
                        })
                        .child(
                            // A plain square thumbnail, rounded here at render —
                            // so the corners match the stroke with no baked margin
                            // to leave a gap.
                            gpui::img(std::sync::Arc::new(gpui::Image::from_bytes(
                                gpui::ImageFormat::Png,
                                thumb.to_vec(),
                            )))
                            .size(px(img_size))
                            .rounded(px(img_radius)),
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _window, cx| {
                                view.update(cx, |this, cx| this.set_app_icon(id, cx));
                            }
                        })
                };
                let radio = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .w_full()
                    .gap_2()
                    .children(
                        app_icon::ICONS
                            .iter()
                            .map(|icon| cell(icon.id, icon.thumb, icon.id == current)),
                    )
                    .into_any_element();
                rows.push(field("app-icon", "App icon", None, radio));
            }
            rows
        });

        let editor = section("Editor", {
            #[cfg(target_os = "macos")]
            let control = Select::new(&s.editor)
                .search_placeholder("Search editors")
                .into_any_element();
            #[cfg(not(target_os = "macos"))]
            let control = Input::new(&s.editor).into_any_element();
            vec![field(
                "editor",
                "External editor",
                Some("The editor used when opening a file"),
                control,
            )]
        });

        let behavior = section("Behavior", {
            let mut rows = vec![field(
                "keymap-preset",
                "Keybindings",
                None,
                Select::new(&s.keymap_preset).into_any_element(),
            )];
            if !layout.stack_behavior_rows {
                rows.push(
                    div().flex().w_full().justify_end().child(
                        div()
                            .flex()
                            .gap_3()
                            .when(self.repo_scope_dir.is_some(), |el| {
                                el.min_w(px(BEHAVIOR_SCOPE_COLUMNS_WIDTH))
                            })
                            .child(
                                div()
                                    .w(px(BEHAVIOR_GLOBAL_COLUMN_WIDTH))
                                    .text_xs()
                                    .text_color(self.palette.dim)
                                    .child("Global"),
                            )
                            .when(self.repo_scope_dir.is_some(), |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(self.palette.dim)
                                        .child("Repository"),
                                )
                            }),
                    ),
                );
            }
            rows.extend([
                behavior_field(
                    "auto-refresh",
                    "Auto refresh",
                    "Automatically refresh when changes are detected in the repo.",
                    self.behavior_toggle_control(
                        BehaviorSetting::AutoRefresh,
                        s.behavior_overrides.get(BehaviorSetting::AutoRefresh),
                        layout.stack_behavior_rows,
                        view,
                    ),
                ),
                behavior_field(
                    "refresh-on-focus",
                    "Refresh on focus",
                    "Refresh the status view automatically when window regains focus.",
                    self.behavior_toggle_control(
                        BehaviorSetting::RefreshOnFocus,
                        s.behavior_overrides.get(BehaviorSetting::RefreshOnFocus),
                        layout.stack_behavior_rows,
                        view,
                    ),
                ),
                behavior_field(
                    "show-tags",
                    "Tags in title bar",
                    "Show the nearest reachable tag (e.g. `v1.0 (5)`) in the title bar.",
                    self.behavior_toggle_control(
                        BehaviorSetting::ShowTags,
                        s.behavior_overrides.get(BehaviorSetting::ShowTags),
                        layout.stack_behavior_rows,
                        view,
                    ),
                ),
                behavior_field(
                    "check-for-updates",
                    "Check for updates",
                    "Periodically check for published Magritte releases.",
                    self.behavior_toggle_control(
                        BehaviorSetting::CheckForUpdates,
                        None,
                        layout.stack_behavior_rows,
                        view,
                    ),
                ),
            ]);
            rows
        });

        let commit = section("Commit editor", {
            let mut rows = vec![toggle_field(
                "commit-in-editor",
                "Use external editor",
                "Write commit messages with the editor command below (an interactive `git \
                 commit`) instead of the built-in editor.",
                self.toggle_control(
                    "commit-in-editor",
                    self.config.commit_in_editor,
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
                    None,
                    Input::new(&s.commit_editor).into_any_element(),
                ));
            } else {
                rows.push(toggle_field(
                    "commit-title-ruler",
                    "Summary ruler",
                    "Underlines characters past column 50 on the commit summary (first) line.",
                    self.toggle_control(
                        "commit-title-ruler",
                        self.config.commit_title_ruler,
                        view,
                        false,
                        |cfg, on| cfg.commit_title_ruler = on,
                    ),
                ));
                rows.push(toggle_field(
                    "commit-body-wrap",
                    "Body auto-wrap",
                    "Hard-wraps the commit body at 72 columns as you type at the end of a line \
                     (the summary line is never wrapped).",
                    self.toggle_control(
                        "commit-body-wrap",
                        self.config.commit_body_wrap,
                        view,
                        false,
                        |cfg, on| cfg.commit_body_wrap = on,
                    ),
                ));
                rows.push(toggle_field(
                    "commit-vim-mode",
                    "Vim mode",
                    "Vim emulation in the commit editor. Commit with ZZ, cancel with ZQ.",
                    self.toggle_control(
                        "commit-vim-mode",
                        self.config.commit_vim_mode,
                        view,
                        false,
                        |cfg, on| cfg.commit_vim_mode = on,
                    ),
                ));
            }
            rows
        });

        // The cards use two masonry columns when there is enough room, then
        // become one ordered column before either side gets cramped.
        let left_column = div()
            .flex()
            .flex_col()
            .min_w(px(0.0))
            .gap_4()
            .when(stack_columns, |el| el.w_full())
            .when(!stack_columns, |el| {
                el.flex_1().flex_basis(gpui::relative(0.55))
            })
            .child(appearance)
            .child(editor);
        let right_column = div()
            .flex()
            .flex_col()
            .min_w(px(0.0))
            .gap_4()
            .when(stack_columns, |el| el.w_full())
            .when(!stack_columns, |el| {
                el.flex_1().flex_basis(gpui::relative(0.45))
            })
            .child(behavior)
            .child(commit);
        let columns = div()
            .flex()
            .items_start()
            .gap_4()
            .w_full()
            .when(stack_columns, |el| el.flex_col())
            .child(left_column)
            .child(right_column);

        // The content column: width-capped and left-aligned. Wrapped below in a
        // full-width scroll container so the scrollbar sits at the window edge.
        let content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(880.0))
            .p_4()
            .gap_4()
            .child(header)
            .child(columns);

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
                    .child(content),
            )
            .vertical_scrollbar(&s.scroll)
    }

    /// Ensure the config file exists and open it via the same path as opening a
    /// file at point: the configured editor, falling back to the OS default app
    /// when it's unset. (The split button's dropdown still opens a chosen app.)
    pub(crate) fn open_config_file(&self) {
        if let Some(path) = config::ensure_file() {
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

#[cfg(test)]
mod tests {
    use super::SettingsLayout;

    #[test]
    fn responsive_settings_layout_uses_section_wide_breaks() {
        let wide = SettingsLayout::for_width(880.0);
        assert!(!wide.stack_columns);
        assert!(!wide.stack_behavior_rows);

        let narrower_columns = SettingsLayout::for_width(824.0);
        assert!(!narrower_columns.stack_columns);
        assert!(narrower_columns.stack_behavior_rows);

        let full_width_card = SettingsLayout::for_width(650.0);
        assert!(full_width_card.stack_columns);
        assert!(!full_width_card.stack_behavior_rows);
        assert!(!full_width_card.stack_fields);

        let cramped = SettingsLayout::for_width(380.0);
        assert!(cramped.stack_columns);
        assert!(cramped.stack_behavior_rows);
        assert!(cramped.stack_fields);
    }
}

//! The native menu bar and Dock menu (macOS chrome; inert elsewhere): the
//! standard app/File/Edit/Window/Help menus, wired to the existing commands
//! where one exists. The menu named "Window" becomes the system windows menu
//! (window list, tiling), and the Edit items carry the OS edit roles so text
//! inputs get the standard menu behavior.

use gpui::{
    actions, App, Context, Menu, MenuItem, OsAction, PathPromptOptions, SystemMenuType, Window,
};
use std::path::{Path, PathBuf};

use crate::*;

actions!(
    magritte,
    [
        About,
        CheckForUpdates,
        OpenRepository,
        HideApp,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        HelpMenu
    ]
);

/// Open a specific repository from the Dock menu's recent list.
#[derive(Clone, Default, PartialEq, gpui::Action)]
#[action(namespace = magritte, no_json)]
pub(crate) struct OpenRecent {
    pub path: PathBuf,
}

/// How many repositories the Dock menu's recent list keeps.
const MAX_RECENT_REPOS: usize = 10;

/// Record `path` at the head of the persisted recent-repos list and rebuild
/// the Dock menu to match. (The system recent-documents list ignores unbundled
/// binaries, so the Dock section is ours to maintain.)
pub(crate) fn note_recent_repo(path: &Path, cx: &mut App) {
    let Some(file) = state::global_path(state::RECENT_REPOS_FILE) else {
        return;
    };
    let mut recents = state::RecentRepos::load(&file);
    recents.entries.retain(|e| e.path != path);
    recents.entries.insert(
        0,
        state::RecentRepo {
            path: path.to_path_buf(),
            last_used: state::unix_now(),
        },
    );
    recents.entries.truncate(MAX_RECENT_REPOS);
    state::save_toml(&file, &recents);
    set_dock_menu(&recents, cx);
}

/// The Dock menu: the recent repositories (most recent first, by directory
/// name), then Open Repository…. Entries whose directory has vanished are
/// skipped.
fn set_dock_menu(recents: &state::RecentRepos, cx: &mut App) {
    let mut items: Vec<MenuItem> = recents
        .entries
        .iter()
        .filter(|e| e.path.is_dir())
        .map(|e| {
            let name = e
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| e.path.display().to_string());
            MenuItem::action(
                name,
                OpenRecent {
                    path: e.path.clone(),
                },
            )
        })
        .collect();
    if !items.is_empty() {
        items.push(MenuItem::separator());
    }
    items.push(MenuItem::action("Open Repository…", OpenRepository));
    cx.set_dock_menu(items);
}

impl StatusView {
    /// The About dialog — the app-menu item and the `about` palette command:
    /// the current app icon, name, version and description, the third-party
    /// attributions, and the update-check / GitHub actions.
    pub(crate) fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::prelude::*;
        use gpui::{div, px, FontWeight};
        use gpui_component::button::Button;
        use gpui_component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _};

        let icon_id = app_icon::resolved_icon(&self.config.app_icon);
        let thumb = app_icon::ICONS
            .iter()
            .find(|icon| icon.id == icon_id)
            .map(|icon| icon.thumb)
            .unwrap_or(app_icon::ICONS[0].thumb);
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog.w(px(430.0)).content(move |content, _, cx| {
                let dim = cx.theme().muted_foreground;
                let updates = Button::new("about-updates")
                    .label("Check for Updates…")
                    .outline()
                    .small()
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            // Close first so the check's outcome toast isn't
                            // hidden behind the dialog.
                            window.close_dialog(cx);
                            if let Some(view) = view.upgrade() {
                                view.update(cx, |view, cx| view.check_for_updates(cx));
                            }
                        }
                    });
                let github = Button::new("about-github")
                    .label("GitHub")
                    .icon(IconName::ExternalLink)
                    .outline()
                    .small()
                    .on_click(|_, _, cx| cx.open_url(env!("CARGO_PKG_REPOSITORY")));
                content
                    .items_center()
                    .gap_1()
                    .pt_2()
                    .child(
                        gpui::img(std::sync::Arc::new(gpui::Image::from_bytes(
                            gpui::ImageFormat::Png,
                            thumb.to_vec(),
                        )))
                        .size(px(72.0))
                        .rounded(px(16.0)),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Magritte"),
                    )
                    .child(
                        div()
                            .text_color(dim)
                            .text_sm()
                            .child(format!("Version {CURRENT_VERSION}")),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_center()
                            .child("A fast, keyboard-driven git client in the spirit of magit."),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_center()
                            .text_xs()
                            .text_color(dim)
                            .child(
                                "Built with GPUI (Zed Industries) and gpui-component \
                                 (Longbridge), used under the Apache License 2.0.",
                            )
                            .child(
                                "Bundled theme palettes after Solarized, Selenized, \
                                 Catppuccin, Dracula, Gruvbox, Nord, GitHub, and Tao.",
                            ),
                    )
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(updates)
                            .child(github),
                    )
            })
        });
    }
}

/// Run `f` on the active window, deferred out of the current dispatch: a menu
/// action is dispatched *inside* an update of the active window, so touching
/// that window immediately would re-enter it and silently fail.
fn on_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        window.update(cx, |_, window, cx| f(window, cx)).ok();
    });
}

/// Run `f` on the active window's status view (reached through the
/// `gpui_component::Root` the window was built with), deferred like
/// [`on_active_window`]. App-level handlers use this rather than element-tree
/// `on_action`s because a menu click dispatches from the window's *focused*
/// node — and must still work when nothing in the window holds focus.
fn on_status_view(
    cx: &mut App,
    f: impl FnOnce(&mut StatusView, &mut Window, &mut Context<StatusView>) + 'static,
) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| {
        window
            .update(cx, |root, window, cx| {
                let Ok(root) = root.downcast::<gpui_component::Root>() else {
                    return;
                };
                let Ok(view) = root.read(cx).view().clone().downcast::<StatusView>() else {
                    return;
                };
                view.update(cx, |view, cx| f(view, window, cx));
            })
            .ok();
    });
}

/// Install the menu bar, Dock menu, their app-level action handlers, and the
/// standard shortcuts. Close Window and Settings… stay on the status view
/// (their in-window guards need the focused state).
pub(crate) fn install(cx: &mut App) {
    cx.on_action(|_: &OpenRepository, cx| {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await {
                if let Some(path) = paths.pop() {
                    cx.update(|cx| {
                        let windows = cx.global::<GlobalRepoWindows>().0.clone();
                        open_or_focus_repo(Some(path), &windows, cx);
                    });
                }
            }
        })
        .detach();
    });
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| {
        on_active_window(cx, |window, _| window.minimize_window());
    });
    cx.on_action(|_: &Zoom, cx| {
        on_active_window(cx, |window, _| window.zoom_window());
    });
    cx.on_action(|_: &About, cx| {
        on_status_view(cx, |view, window, cx| {
            view.invoke_command("about", window, cx)
        });
    });
    cx.on_action(|action: &OpenRecent, cx| {
        let path = action.path.clone();
        let windows = cx.global::<GlobalRepoWindows>().0.clone();
        open_or_focus_repo(Some(path), &windows, cx);
    });
    cx.on_action(|_: &CheckForUpdates, cx| {
        on_status_view(cx, |view, window, cx| {
            view.invoke_command("check-updates", window, cx)
        });
    });
    cx.on_action(|_: &HelpMenu, cx| {
        on_status_view(cx, |view, window, cx| {
            view.invoke_command("help", window, cx)
        });
    });

    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenRepository, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
    ]);

    use gpui_component::input;
    cx.set_menus(vec![
        Menu::new("Magritte").items([
            MenuItem::action("About Magritte", About),
            MenuItem::action("Check for Updates…", CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Magritte", HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Magritte", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Repository…", OpenRepository),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        // The OS edit roles dispatch to the focused text input; on the status
        // view, Copy falls through to copy-at-point (handled in render).
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
        ]),
        // "Window" is adopted as the system windows menu (window list, tiling).
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("Magritte Help", HelpMenu)]),
    ]);
    let recents = state::global_path(state::RECENT_REPOS_FILE)
        .map(|p| state::RecentRepos::load(&p))
        .unwrap_or_default();
    set_dock_menu(&recents, cx);
}

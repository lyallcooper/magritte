//! The native menu bar and Dock menu (macOS chrome; inert elsewhere): the
//! standard app/File/Edit/Window/Help menus, wired to the existing commands
//! where one exists. The menu named "Window" becomes the system windows menu
//! (window list, tiling), and the Edit items carry the OS edit roles so text
//! inputs get the standard menu behavior.

use gpui::{
    actions, App, Menu, MenuItem, OsAction, PathPromptOptions, PromptLevel, SystemMenuType,
};

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

/// Install the menu bar, Dock menu, their app-level action handlers, and the
/// standard shortcuts. Window-scoped menu actions (Close Window, Settings…,
/// Check for Updates…, Help) are handled on the status view.
pub(crate) fn install(cx: &mut App) {
    cx.on_action(|_: &About, cx| {
        let Some(window) = cx.active_window() else {
            return;
        };
        window
            .update(cx, |_, window, cx| {
                let answer = window.prompt(
                    PromptLevel::Info,
                    &format!("Magritte {CURRENT_VERSION}"),
                    Some("A fast, keyboard-driven git client in the spirit of magit."),
                    &["OK"],
                    cx,
                );
                cx.spawn(async move |_| {
                    answer.await.ok();
                })
                .detach();
            })
            .ok();
    });
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
        if let Some(window) = cx.active_window() {
            window
                .update(cx, |_, window, _| window.minimize_window())
                .ok();
        }
    });
    cx.on_action(|_: &Zoom, cx| {
        if let Some(window) = cx.active_window() {
            window.update(cx, |_, window, _| window.zoom_window()).ok();
        }
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
    cx.set_dock_menu(vec![MenuItem::action("Open Repository…", OpenRepository)]);
}

//! The native menu bar and Dock menu (macOS chrome; inert elsewhere): the
//! standard app/File/Edit/Window/Help menus, wired to the existing commands
//! where one exists. The menu named "Window" becomes the system windows menu
//! (window list, tiling), and the Edit items carry the OS edit roles so text
//! inputs get the standard menu behavior.

use gpui::{
    actions, App, Context, Menu, MenuItem, OsAction, PathPromptOptions, SystemMenuType, Window,
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
        on_status_view(cx, |view, _, cx| {
            view.set_status(
                format!(
                    "Magritte {CURRENT_VERSION} — a fast, keyboard-driven git client \
                     in the spirit of magit"
                ),
                false,
                cx,
            );
        });
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
    cx.set_dock_menu(vec![MenuItem::action("Open Repository…", OpenRepository)]);
}

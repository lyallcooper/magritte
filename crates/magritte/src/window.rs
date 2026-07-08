//! Window/bootstrap plumbing: background detach, per-repo window placement
//! (persisted per worktree with a global fallback, clamped to visible
//! displays), and opening or focusing a repo window for single-instance
//! handoff.

use gpui::{
    point, px, size, AnyWindowHandle, App, AppContext, Bounds, Window, WindowBounds, WindowOptions,
};
use std::path::{Path, PathBuf};

use crate::*;

/// Launch a fresh copy in the background so the shell gets its prompt back
/// without continuing a forked process into AppKit. The child opts out of this
/// handoff with `MAGRITTE_FOREGROUND`, so it follows the normal app path.
pub(crate) fn detach_into_background(args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .args(args)
        .env("MAGRITTE_FOREGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

pub(crate) fn repo_window_key(start_dir: Option<&Path>) -> PathBuf {
    let root = start_root(start_dir);
    Repo::discover(&root)
        .map(|repo| repo.workdir().to_path_buf())
        .or_else(|_| std::fs::canonicalize(&root))
        .unwrap_or(root)
}

/// The directory a window request starts from: the given path, else the cwd,
/// else `.` — the shared preamble of the discovery helpers here.
fn start_root(start_dir: Option<&Path>) -> PathBuf {
    start_dir
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn status_window_options(
    worktree_scope_dir: Option<&Path>,
    cx: &mut App,
) -> WindowOptions {
    // Restore the repo/worktree frame first, then the global default. On first
    // launch, avoid the stiff "exactly centered on the primary display" feel:
    // place a reasonably sized window near the top-left of the usable display
    // area, and let later windows cascade from the active Magritte window.
    //
    // gpui window frames are per-display: a placement is the pair of a display
    // and bounds relative to it (`WindowOptions.display_id` picks the display —
    // primary when `None`). Both restore paths resolve the display alongside
    // the bounds, so a window comes back on the monitor it was closed on.
    let (bounds, display_id) = load_window_state(worktree_scope_dir)
        .and_then(|state| window_state_to_placement(state, cx))
        .unwrap_or_else(|| {
            let (bounds, display_id) = default_status_window_placement(cx);
            (WindowBounds::Windowed(bounds), display_id)
        });
    WindowOptions {
        window_bounds: Some(bounds),
        display_id,
        // Transparent system bar so our custom `TitleBar` draws the chrome
        // (and the traffic lights sit where the component expects them).
        titlebar: Some(gpui_component::TitleBar::title_bar_options()),
        ..Default::default()
    }
}

pub(crate) fn load_window_state(worktree_scope_dir: Option<&Path>) -> Option<state::WindowState> {
    worktree_scope_dir
        .map(|dir| state::scoped_path(dir, state::WINDOW_FILE))
        .and_then(|path| state::load_toml_opt(&path))
        .or_else(|| {
            state::global_path(state::WINDOW_FILE)
                .as_deref()
                .and_then(state::load_toml_opt)
        })
}

pub(crate) fn save_window_state(
    worktree_scope_dir: Option<&Path>,
    window: &mut Window,
    cx: &mut App,
) {
    let state = window_state_from_window(window, cx);
    if let Some(dir) = worktree_scope_dir {
        state::save_toml(&state::scoped_path(dir, state::WINDOW_FILE), &state);
    }
    if let Some(path) = state::global_path(state::WINDOW_FILE) {
        state::save_toml(&path, &state);
    }
}

/// Cascade from the active Magritte window on its own display, else near the
/// top-left of the primary display's usable area.
pub(crate) fn default_status_window_placement(
    cx: &mut App,
) -> (Bounds<gpui::Pixels>, Option<gpui::DisplayId>) {
    if let Some((bounds, display)) = cx.active_window().and_then(|window| {
        window
            .update(cx, |_, window, cx| {
                (window.window_bounds().get_bounds(), window.display(cx))
            })
            .ok()
    }) {
        let visible = display
            .as_ref()
            .map(|d| d.visible_bounds())
            .unwrap_or_else(|| primary_visible_bounds(cx));
        return (
            fit_window_bounds_to_visible_bounds(
                Bounds::new(bounds.origin + point(px(25.0), px(25.0)), bounds.size),
                visible,
            ),
            display.map(|d| d.id()),
        );
    }

    let display = primary_visible_bounds(cx);
    (
        fit_window_bounds_to_visible_bounds(
            Bounds::new(
                display.origin + point(px(80.0), px(60.0)),
                size(px(1000.0), px(720.0)),
            ),
            display,
        ),
        None,
    )
}

pub(crate) fn fit_window_bounds_to_visible_bounds(
    bounds: Bounds<gpui::Pixels>,
    display: Bounds<gpui::Pixels>,
) -> Bounds<gpui::Pixels> {
    let width = bounds.size.width.max(px(640.0)).min(display.size.width);
    let height = bounds.size.height.max(px(420.0)).min(display.size.height);
    let max_x = display.origin.x + display.size.width - width;
    let max_y = display.origin.y + display.size.height - height;
    Bounds::new(
        point(
            bounds.origin.x.max(display.origin.x).min(max_x),
            bounds.origin.y.max(display.origin.y).min(max_y),
        ),
        size(width, height),
    )
}

pub(crate) fn primary_visible_bounds(cx: &App) -> Bounds<gpui::Pixels> {
    cx.primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(800.0))))
}

pub(crate) fn window_state_to_placement(
    state: state::WindowState,
    cx: &mut App,
) -> Option<(WindowBounds, Option<gpui::DisplayId>)> {
    if !(state.x.is_finite()
        && state.y.is_finite()
        && state.width.is_finite()
        && state.height.is_finite())
        || state.width <= 0.0
        || state.height <= 0.0
    {
        return None;
    }
    let bounds = Bounds::new(
        point(px(state.x), px(state.y)),
        size(px(state.width), px(state.height)),
    );
    // The saved frame is relative to the display it was on; reopen there when
    // that monitor is still around, else fall back to the primary. The clamp
    // compares within one display's space, so it's consistent either way.
    let display = state.display_uuid.as_deref().and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|display| display.uuid().ok().is_some_and(|id| id.to_string() == uuid))
    });
    let (display_id, visible) = match &display {
        Some(display) => (Some(display.id()), display.visible_bounds()),
        None => (None, primary_visible_bounds(cx)),
    };
    let bounds = fit_window_bounds_to_visible_bounds(bounds, visible);
    let bounds = match state.mode {
        state::WindowMode::Windowed => WindowBounds::Windowed(bounds),
        state::WindowMode::Maximized => WindowBounds::Maximized(bounds),
        state::WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };
    Some((bounds, display_id))
}

pub(crate) fn window_state_from_window(window: &mut Window, cx: &mut App) -> state::WindowState {
    let display_uuid = window
        .display(cx)
        .and_then(|display| display.uuid().ok())
        .map(|uuid| uuid.to_string());
    let mode = match window.window_bounds() {
        WindowBounds::Windowed(_) => state::WindowMode::Windowed,
        WindowBounds::Maximized(_) => state::WindowMode::Maximized,
        WindowBounds::Fullscreen(_) => state::WindowMode::Fullscreen,
    };
    let bounds = window.window_bounds().get_bounds();
    state::WindowState {
        mode,
        display_uuid,
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
    }
}

pub(crate) fn discover_worktree_scope_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    let root = start_root(start_dir);
    Repo::discover(&root)
        .ok()
        .and_then(|repo| repo.git_dir().ok())
        .map(|dir| config::repo_dir(&dir))
}

pub(crate) fn open_repo_window(
    start_dir: Option<PathBuf>,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let (cfg, cfg_warning) = config::load_reporting();
    theme::apply_appearance(&cfg, cx);
    let worktree_scope_dir = discover_worktree_scope_dir(start_dir.as_deref());
    let options = status_window_options(worktree_scope_dir.as_deref(), cx);
    let window = cx
        .open_window(options, |window, cx| {
            let view = cx
                .new(|cx| StatusView::new(start_dir.clone(), cfg.clone(), cfg_warning.clone(), cx));
            // Now that the window exists, install the live-reload watchers (the
            // appearance observer needs `&mut Window`).
            view.update(cx, |view, cx| {
                view.install_watchers(window, cx);
                view.start_auto_fetch(cx);
                view.start_update_checks(cx);
                // The Dock-icon override is per-session, so set it each launch.
                view.apply_app_icon();
            });
            // The window's root must be a gpui-component Root (provides
            // theming, overlays, and the component context).
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        })
        .ok()?;
    Some(window.into())
}

pub(crate) fn open_or_focus_repo(
    start_dir: Option<PathBuf>,
    windows: &RepoWindows,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let key = repo_window_key(start_dir.as_deref());
    // Feed both recent lists: ours (rebuilt into the Dock menu) and the
    // system's (inert for an unbundled binary, live if we ever ship a bundle).
    menus::note_recent_repo(&key, cx);
    cx.add_recent_document(&key);
    if let Some(handle) = windows.borrow().get(&key).copied() {
        if cx
            .update_window(handle, |_, window, _| window.activate_window())
            .is_ok()
        {
            cx.activate(true);
            return Some(handle);
        }
        windows.borrow_mut().remove(&key);
    }

    let handle = open_repo_window(start_dir, cx)?;
    windows.borrow_mut().insert(key, handle);
    cx.activate(true);
    Some(handle)
}

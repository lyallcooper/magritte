//! Debug-mode control channel for fast, headless iteration during development.
//!
//! Enabled by setting `MAGRITTE_DEBUG_DIR=<dir>`. The app then polls that
//! directory for a `cmd` file, runs the commands it contains, and writes a
//! `done` file when finished. This lets a driver inject keystrokes and capture
//! screenshots without AppleScript, screen coordinates, or foregrounding the
//! window.
//!
//! Protocol (driver side):
//!   1. wait for any previous `done` to be removed
//!   2. atomically place a `cmd` file (write to `cmd.tmp`, rename to `cmd`)
//!   3. wait for `done` to appear, read it (the result), then remove it
//!
//! Commands (one per line):
//!   key <keystroke>      e.g. `key j`, `key shift-g`, `key tab`, `key escape`
//!   type <text...>       type the literal text into the focused input
//!   click <x> <y>        click at a window-relative point (logical points)
//!   click-id <id>        click a registered element by id (preferred — no
//!                        coordinate guessing; see `record_target`)
//!   targets              list registered clickable ids and their centers
//!   shot <path>          capture the window to a PNG at <path>
//!   sleep <millis>       pause (e.g. to let a frame paint)

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
#[cfg(all(target_os = "macos", not(feature = "debug-capture")))]
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use gpui::{
    point, px, AnyWindowHandle, AppContext, AsyncApp, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
};

/// The control directory, if debug mode is enabled.
pub fn control_dir() -> Option<PathBuf> {
    std::env::var_os("MAGRITTE_DEBUG_DIR").map(PathBuf::from)
}

/// Registry of clickable element ids → their on-screen center (logical points),
/// recorded during render via [`record_target`]. Lets the control channel click
/// an element by id (`click-id`) instead of guessing pixel coordinates.
static TARGETS: LazyLock<Mutex<HashMap<String, (f32, f32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record (or update) a clickable element's center point. No-op unless debug
/// mode is enabled, so it costs nothing in normal use.
pub fn record_target(id: &str, x: f32, y: f32) {
    if control_dir().is_none() {
        return;
    }
    if let Ok(mut targets) = TARGETS.lock() {
        targets.insert(id.to_string(), (x, y));
    }
}

/// Start the control loop if debug mode is enabled. No-op otherwise.
pub fn init(handle: AnyWindowHandle, cx: &mut gpui::App) {
    let Some(dir) = control_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    // The control channel can inject keys/clicks and trigger destructive git
    // actions, so the control dir must be private. Lock it to 0700 and refuse
    // to start if it stays group/world-accessible (e.g. owned by someone else).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        let private = fs::metadata(&dir)
            .map(|m| m.permissions().mode() & 0o077 == 0)
            .unwrap_or(false);
        if !private {
            eprintln!(
                "magritte debug: refusing control dir {} (not private; chmod 700 it)",
                dir.display()
            );
            return;
        }
    }
    // Clear any stale files from a previous run.
    let _ = fs::remove_file(dir.join("cmd"));
    let _ = fs::remove_file(dir.join("done"));
    eprintln!("magritte debug: watching {}", dir.display());

    cx.spawn(async move |cx: &mut AsyncApp| loop {
        cx.background_executor()
            .timer(Duration::from_millis(60))
            .await;
        let cmd_path = dir.join("cmd");
        let Ok(content) = fs::read_to_string(&cmd_path) else {
            continue;
        };
        let _ = fs::remove_file(&cmd_path);

        let mut out = String::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match run_command(line, handle, cx).await {
                Ok(Some(msg)) => {
                    out.push_str(&msg);
                    out.push('\n');
                }
                Ok(None) => {}
                Err(msg) => {
                    out.push_str("error: ");
                    out.push_str(&msg);
                    out.push('\n');
                }
            }
        }
        if out.is_empty() {
            out.push_str("ok\n");
        }
        let _ = fs::write(dir.join("done"), out);
    })
    .detach();
}

/// Run a single command line. Returns an optional message for the response.
async fn run_command(
    line: &str,
    handle: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<Option<String>, String> {
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (line, ""),
    };
    match verb {
        "key" => {
            let ks = parse_keystroke(rest)?;
            dispatch(handle, ks, cx)?;
            Ok(None)
        }
        "type" => {
            for ch in rest.chars() {
                dispatch(handle, char_keystroke(ch), cx)?;
            }
            Ok(None)
        }
        "click" | "shift-click" => {
            let mut parts = rest.split_whitespace();
            let x: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("click needs: x y")?;
            let y: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("click needs: x y")?;
            dispatch_click(handle, x, y, click_modifiers(verb), cx)?;
            Ok(None)
        }
        "move" => {
            let mut parts = rest.split_whitespace();
            let x: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("move needs: x y")?;
            let y: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("move needs: x y")?;
            dispatch_move(handle, x, y, cx)?;
            Ok(None)
        }
        "dblclick" => {
            let mut parts = rest.split_whitespace();
            let x: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("dblclick needs: x y")?;
            let y: f32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or("dblclick needs: x y")?;
            dispatch_double_click(handle, x, y, cx)?;
            Ok(None)
        }
        "drag" => {
            let coords: Vec<f32> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if coords.len() < 4 || coords.len() % 2 != 0 {
                return Err("drag needs: x1 y1 x2 y2 [x3 y3 …]".into());
            }
            let points: Vec<(f32, f32)> = coords.chunks(2).map(|p| (p[0], p[1])).collect();
            dispatch_drag(handle, &points, cx)?;
            Ok(None)
        }
        "click-id" | "shift-click-id" => {
            // Force a paint first: when the window is occluded the OS display
            // link is paused, so the target registry would otherwise be stale.
            force_draw(handle, cx);
            let target = TARGETS.lock().ok().and_then(|t| t.get(rest).copied());
            let (x, y) = target.ok_or_else(|| format!("no clickable target with id {rest:?}"))?;
            dispatch_click(handle, x, y, click_modifiers(verb), cx)?;
            Ok(Some(format!("clicked {rest} @ {x:.0},{y:.0}")))
        }
        "draw" => {
            // Synchronous layout+paint, no present. Refreshes the target
            // registry even while the window is occluded (paused display link).
            force_draw(handle, cx);
            Ok(Some("drew".into()))
        }
        "targets" => {
            force_draw(handle, cx);
            let targets = TARGETS.lock().map_err(|_| "targets lock poisoned")?;
            let mut lines: Vec<String> = targets
                .iter()
                .map(|(k, (x, y))| format!("{k}  {x:.0},{y:.0}"))
                .collect();
            lines.sort();
            Ok(Some(lines.join("\n")))
        }
        "sleep" => {
            let ms: u64 = rest.parse().map_err(|_| format!("bad millis: {rest}"))?;
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            Ok(None)
        }
        "shot" => {
            if rest.is_empty() {
                return Err("shot needs a path".into());
            }
            // With `debug-capture`: render the window's scene to an offscreen
            // image (via gpui's `render_to_image`). A forced `draw` first makes
            // the rendered frame current even when the OS display link is paused
            // (occluded/minimized), so this captures fresh pixels in the
            // background without foregrounding the window.
            #[cfg(feature = "debug-capture")]
            {
                force_draw(handle, cx);
                let img = cx
                    .update_window(handle, |_, window, _| window.render_to_image())
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                let (lw, lh) = cx
                    .update_window(handle, |_, window, _| {
                        let b = window.bounds();
                        (
                            b.size.width.as_f32().round() as u32,
                            b.size.height.as_f32().round() as u32,
                        )
                    })
                    .map_err(|e| e.to_string())?;
                let (dw, dh) = (img.width(), img.height());
                encode_png_downscaled(img.into_raw(), dw, dh, lw, lh, rest)?;
                Ok(Some(format!("shot {rest}")))
            }
            // Without the feature: capture via `screencapture`. Reads the
            // window-server surface, which only refreshes for a foregrounded
            // (non-occluded) window — see the module note on background paint.
            #[cfg(not(feature = "debug-capture"))]
            {
                let _ = cx.update_window(handle, |_, window, _| window.refresh());
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                // The window's logical size, so we can downscale the Retina
                // capture to point-space — then image px == click coords 1:1.
                let size = cx
                    .update_window(handle, |_, window, _| {
                        let b = window.bounds();
                        (b.size.width.as_f32(), b.size.height.as_f32())
                    })
                    .ok();
                screenshot(rest, size)?;
                Ok(Some(format!("shot {rest}")))
            }
        }
        other => Err(format!("unknown command: {other}")),
    }
}

/// Dispatch a bare mouse move (no button) to a window-relative point — e.g. to
/// hover an element and trigger its tooltip.
fn dispatch_move(handle: AnyWindowHandle, x: f32, y: f32, cx: &mut AsyncApp) -> Result<(), String> {
    let pos = point(px(x), px(y));
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: pos,
                pressed_button: None,
                modifiers: Modifiers::default(),
            }),
            cx,
        );
    })
    .map_err(|e| e.to_string())
}

/// Modifiers a click verb implies: the `shift-` prefixed variants hold shift.
fn click_modifiers(verb: &str) -> Modifiers {
    if verb.starts_with("shift-") {
        Modifiers {
            shift: true,
            ..Modifiers::default()
        }
    } else {
        Modifiers::default()
    }
}

/// Dispatch a left double-click at a window-relative point: two down/up pairs,
/// the second carrying `click_count = 2` (as the platform would), so handlers
/// that distinguish double-clicks from lone clicks fire correctly.
fn dispatch_double_click(
    handle: AnyWindowHandle,
    x: f32,
    y: f32,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let pos = point(px(x), px(y));
    let m = Modifiers::default();
    cx.update_window(handle, |_, window, cx| {
        for count in [1, 2] {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: pos,
                    modifiers: m,
                    click_count: count,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: pos,
                    modifiers: m,
                    click_count: count,
                }),
                cx,
            );
        }
    })
    .map_err(|e| e.to_string())
}

/// Dispatch a left click (move → down → up) at a window-relative point, with
/// the given keyboard modifiers held (e.g. shift, to test shift-click).
fn dispatch_click(
    handle: AnyWindowHandle,
    x: f32,
    y: f32,
    modifiers: Modifiers,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let pos = point(px(x), px(y));
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: pos,
                pressed_button: None,
                modifiers,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: pos,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: pos,
                modifiers,
                click_count: 1,
            }),
            cx,
        );
    })
    .map_err(|e| e.to_string())
}

/// Dispatch a left-drag along `points` (window-relative): press at the first,
/// button-held moves interpolated through each leg, then release at the last —
/// so the row mouse-move handlers (and the char-selection gesture) see the same
/// event stream a real drag produces. A polyline (3+ points) exercises paths
/// like drag-away-then-back.
fn dispatch_drag(
    handle: AnyWindowHandle,
    points: &[(f32, f32)],
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let first = points[0];
    let last = points[points.len() - 1];
    let start_pos = point(px(first.0), px(first.1));
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: start_pos,
                pressed_button: None,
                modifiers: Modifiers::default(),
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: start_pos,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        // Interpolate button-held moves along each leg so every crossed row
        // fires its handler.
        const STEPS: u32 = 8;
        for leg in points.windows(2) {
            let (from, to) = (leg[0], leg[1]);
            for step in 1..=STEPS {
                let t = step as f32 / STEPS as f32;
                let pos = point(
                    px(from.0 + (to.0 - from.0) * t),
                    px(from.1 + (to.1 - from.1) * t),
                );
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position: pos,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    }),
                    cx,
                );
            }
        }
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(last.0), px(last.1)),
                modifiers: Modifiers::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .map_err(|e| e.to_string())
}

/// Force a synchronous layout+paint pass. This repopulates the `track_target`
/// registry (written during paint) even when the window is occluded and macOS
/// has paused its display link — which is why `targets`/`click-id` would
/// otherwise read stale coordinates in the background. Note this does NOT
/// present pixels to the window surface (that path is driven only by the paused
/// display link), so it does not unfreeze `shot`; screenshots still need the
/// window foregrounded.
fn force_draw(handle: AnyWindowHandle, cx: &mut AsyncApp) {
    let _ = cx.update_window(handle, |_, window, cx| {
        let _ = window.draw(cx);
    });
}

/// Encode an offscreen-rendered RGBA frame to a PNG at `path`, downscaling from
/// device pixels (`dw`×`dh`, Retina) to logical points (`lw`×`lh`) so image
/// pixels map 1:1 to `click`/`click-id` coordinates — matching the contract of
/// the `screencapture` path.
#[cfg(feature = "debug-capture")]
fn encode_png_downscaled(
    raw: Vec<u8>,
    dw: u32,
    dh: u32,
    lw: u32,
    lh: u32,
    path: &str,
) -> Result<(), String> {
    let src = image::RgbaImage::from_raw(dw, dh, raw)
        .ok_or("render_to_image: raw buffer size mismatch")?;
    let out = if lw > 0 && lh > 0 && (lw, lh) != (dw, dh) {
        image::imageops::resize(&src, lw, lh, image::imageops::FilterType::Triangle)
    } else {
        src
    };
    out.save(path).map_err(|e| e.to_string())
}

fn dispatch(handle: AnyWindowHandle, ks: Keystroke, cx: &mut AsyncApp) -> Result<(), String> {
    cx.update_window(handle, |_root, window, cx| {
        window.dispatch_keystroke(ks, cx);
    })
    .map_err(|e| e.to_string())
}

/// Parse a keystroke spec like `shift-g`, `tab`, `escape`, or a bare char.
fn parse_keystroke(spec: &str) -> Result<Keystroke, String> {
    if let Ok(ks) = Keystroke::parse(spec) {
        return Ok(ks);
    }
    // Fall back to a single literal character (e.g. ",").
    let mut chars = spec.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Ok(char_keystroke(ch)),
        _ => Err(format!("bad keystroke: {spec}")),
    }
}

/// A keystroke that types one literal character (drives the IME input path).
fn char_keystroke(ch: char) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::default(),
        key: ch.to_lowercase().to_string(),
        key_char: Some(ch.to_string()),
    }
}

/// Capture our own window to a PNG using the screen-capture tool, targeting the
/// window by id so it works even when the window isn't frontmost. If a logical
/// window size is given, the Retina capture is downscaled to it so that image
/// pixels map 1:1 to click/dispatch coordinates (points).
#[cfg(all(target_os = "macos", not(feature = "debug-capture")))]
fn screenshot(path: &str, logical_size: Option<(f32, f32)>) -> Result<(), String> {
    let id = our_window_id().ok_or("could not find our window id")?;
    let status = Command::new("screencapture")
        .arg(format!("-l{id}"))
        .arg("-x")
        .arg("-o")
        .arg(path)
        .status()
        .map_err(|e| format!("screencapture failed to run: {e}"))?;
    if !status.success() {
        return Err(format!("screencapture exited with {status}"));
    }
    // Downscale device-pixel capture to logical points so screenshots read in
    // the same coordinate space as `click`/`key` dispatch.
    if let Some((w, h)) = logical_size {
        let _ = Command::new("sips")
            .arg("--resampleHeightWidth")
            .arg((h.round() as i64).to_string())
            .arg((w.round() as i64).to_string())
            .arg(path)
            .stdout(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

/// Find the CoreGraphics window number for our process's main window. This
/// reads only window metadata (no screen-recording permission required).
#[cfg(all(target_os = "macos", not(feature = "debug-capture")))]
fn our_window_id() -> Option<i64> {
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
        kCGWindowNumber, kCGWindowOwnerPID,
    };

    let pid = std::process::id() as i64;
    let info = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        0,
    )?;
    let pid_key = unsafe { CFString::wrap_under_get_rule(kCGWindowOwnerPID) };
    let num_key = unsafe { CFString::wrap_under_get_rule(kCGWindowNumber) };

    for i in 0..info.len() {
        let item = info.get(i)?;
        let dict = unsafe {
            CFDictionary::<CFString, core_foundation::base::CFType>::wrap_under_get_rule(
                *item as CFDictionaryRef,
            )
        };
        let owner = dict
            .find(&pid_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64());
        if owner == Some(pid) {
            if let Some(num) = dict
                .find(&num_key)
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64())
            {
                return Some(num);
            }
        }
    }
    None
}

#[cfg(all(not(target_os = "macos"), not(feature = "debug-capture")))]
fn screenshot(_path: &str, _logical_size: Option<(f32, f32)>) -> Result<(), String> {
    Err("screenshots are only implemented on macOS".into())
}

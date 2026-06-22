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
//!   shot <path>          capture the window to a PNG at <path>
//!   sleep <millis>       pause (e.g. to let a frame paint)

use std::path::PathBuf;
use std::time::Duration;
use std::{fs, process::Command};

use gpui::{
    point, px, AnyWindowHandle, AppContext, AsyncApp, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
};

/// The control directory, if debug mode is enabled.
pub fn control_dir() -> Option<PathBuf> {
    std::env::var_os("MAGRITTE_DEBUG_DIR").map(PathBuf::from)
}

/// Start the control loop if debug mode is enabled. No-op otherwise.
pub fn init(handle: AnyWindowHandle, cx: &mut gpui::App) {
    let Some(dir) = control_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    // Clear any stale files from a previous run.
    let _ = fs::remove_file(dir.join("cmd"));
    let _ = fs::remove_file(dir.join("done"));
    eprintln!("magritte debug: watching {}", dir.display());

    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
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
        }
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
        "click" => {
            let mut parts = rest.split_whitespace();
            let x: f32 = parts.next().and_then(|s| s.parse().ok()).ok_or("click needs: x y")?;
            let y: f32 = parts.next().and_then(|s| s.parse().ok()).ok_or("click needs: x y")?;
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
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: pos,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: pos,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })
            .map_err(|e| e.to_string())?;
            Ok(None)
        }
        "sleep" => {
            let ms: u64 = rest.parse().map_err(|_| format!("bad millis: {rest}"))?;
            cx.background_executor().timer(Duration::from_millis(ms)).await;
            Ok(None)
        }
        "shot" => {
            if rest.is_empty() {
                return Err("shot needs a path".into());
            }
            // Let the latest state paint before capturing.
            let _ = cx.update_window(handle, |_, window, _| window.refresh());
            cx.background_executor().timer(Duration::from_millis(80)).await;
            screenshot(rest)?;
            Ok(Some(format!("shot {rest}")))
        }
        other => Err(format!("unknown command: {other}")),
    }
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
/// window by id so it works even when the window isn't frontmost.
#[cfg(target_os = "macos")]
fn screenshot(path: &str) -> Result<(), String> {
    let id = our_window_id().ok_or("could not find our window id")?;
    let status = Command::new("screencapture")
        .arg(format!("-l{id}"))
        .arg("-x")
        .arg("-o")
        .arg(path)
        .status()
        .map_err(|e| format!("screencapture failed to run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("screencapture exited with {status}"))
    }
}

/// Find the CoreGraphics window number for our process's main window. This
/// reads only window metadata (no screen-recording permission required).
#[cfg(target_os = "macos")]
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

#[cfg(not(target_os = "macos"))]
fn screenshot(_path: &str) -> Result<(), String> {
    Err("screenshots are only implemented on macOS".into())
}

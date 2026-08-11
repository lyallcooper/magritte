//! Launching an external editor at a file (and line), and the OS "open" fallback.
//! Pure of any UI state — it only maps an editor command/app name to the right
//! `(program, args)` for its goto convention.

/// Split an editor command into `(program, args)` with shell-style quoting, so
/// a quoted argument survives (`code --user-data-dir "/pa th"`). Malformed
/// quoting falls back to plain whitespace splitting.
pub(crate) fn split_command(editor: &str) -> (String, Vec<String>) {
    let words = shell_words::split(editor)
        .unwrap_or_else(|_| editor.split_whitespace().map(String::from).collect());
    let mut words = words.into_iter();
    let program = words.next().unwrap_or_else(|| editor.to_string());
    (program, words.collect())
}

#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// Build the `(program, args)` to open `path` at `line` (1-based) with the
/// configured editor, per its goto convention, or `None` if we don't know how
/// (the caller then opens without a line). Editors fall into three families:
/// `+N file` (vim/emacs/nano/…), `--goto file:line` (VS Code family), and
/// `file:line` (Zed/Sublime/Helix). On macOS an app *name* (e.g. "Zed") is
/// resolved to the matching CLI inside its bundle.
pub(crate) fn editor_goto(editor: &str, path: &str, line: u32) -> Option<(String, Vec<String>)> {
    let (first, extra) = split_command(editor);
    let first = first.as_str();
    let stem = std::path::Path::new(first)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase();

    // `+N file` — vim family, emacs, and the common terminal editors.
    let plus = || {
        let mut a = extra.clone();
        a.push(format!("+{line}"));
        a.push(path.to_string());
        Some((first.to_string(), a))
    };
    // `[goto] file:line` — `goto` is e.g. `--goto` for VS Code, absent for Zed.
    let colon = |goto: Option<&str>| {
        let mut a = extra.clone();
        if let Some(g) = goto {
            a.push(g.to_string());
        }
        a.push(format!("{path}:{line}"));
        Some((first.to_string(), a))
    };

    match stem.as_str() {
        "vim" | "nvim" | "vi" | "view" | "mvim" | "gvim" | "nano" | "pico" | "micro" | "emacs"
        | "emacsclient" | "kak" | "joe" => plus(),
        "code" | "codium" | "cursor" | "code-insiders" => colon(Some("--goto")),
        "zed" | "subl" | "hx" | "helix" => colon(None),
        // On macOS the editor may be an app *name* (the Settings dropdown stores
        // these); resolve it to its bundle CLI.
        _ => {
            #[cfg(target_os = "macos")]
            {
                editor_app_goto(editor, path, line)
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        }
    }
}

/// macOS: open at a line for a known GUI editor named by its *app* (e.g. "Zed"),
/// via the CLI inside its `.app` bundle.
#[cfg(target_os = "macos")]
fn editor_app_goto(editor: &str, path: &str, line: u32) -> Option<(String, Vec<String>)> {
    // (app name, CLI path within the bundle, goto flag — None means `file:line`).
    const APPS: &[(&str, &str, Option<&str>)] = &[
        ("Zed", "Contents/MacOS/cli", None),
        (
            "Visual Studio Code",
            "Contents/Resources/app/bin/code",
            Some("--goto"),
        ),
        (
            "VSCodium",
            "Contents/Resources/app/bin/codium",
            Some("--goto"),
        ),
        (
            "Cursor",
            "Contents/Resources/app/bin/cursor",
            Some("--goto"),
        ),
        ("Sublime Text", "Contents/SharedSupport/bin/subl", None),
    ];
    let (rel, goto) = APPS
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(editor))
        .map(|(_, rel, goto)| (*rel, *goto))?;
    let cli = find_app_bundle(editor)?.join(rel);
    if !cli.exists() {
        return None;
    }
    let mut args = Vec::new();
    if let Some(g) = goto {
        args.push(g.to_string());
    }
    args.push(format!("{path}:{line}"));
    Some((cli.to_string_lossy().into_owned(), args))
}

/// The path to `<name>.app` in the standard application directories, if present.
#[cfg(target_os = "macos")]
fn find_app_bundle(name: &str) -> Option<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }
    dirs.into_iter()
        .map(|d| d.join(format!("{name}.app")))
        .find(|p| p.exists())
}

/// Open `path` with the OS default application for its type.
pub(crate) fn open_with_os(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    spawn_reaped(std::process::Command::new(program).arg(path));
}

/// Spawn a fire-and-forget child and reap it on a helper thread, reporting only
/// whether the spawn itself got off the ground.
///
/// The reaping is the point: `Child` deliberately doesn't wait on drop, so
/// abandoning the handle leaves a zombie for the lifetime of the window — and
/// these run once per file opened in an editor. A blocking launcher (`code -w`)
/// parks the reaper until the user closes the file, which is why this gets its
/// own thread rather than an executor slot: on the background pool it would hold
/// a worker hostage from status reads, diffs, and transfers. A small stack keeps
/// a pile of parked reapers cheap.
///
/// Callers run on the UI thread, so a thread that can't be created falls back to
/// dropping the handle (the old zombie) rather than panicking the app.
///
/// Note for future callers: `Child::wait` closes stdin first, so don't hand this
/// a command whose stdin is piped and still needed.
pub(crate) fn spawn_reaped(cmd: &mut std::process::Command) -> bool {
    match cmd.spawn() {
        Ok(mut child) => {
            let reaper = std::thread::Builder::new()
                .name("editor-reaper".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    let _ = child.wait();
                });
            let _ = reaper;
            true
        }
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod spawn_tests {
    use std::time::{Duration, Instant};

    /// An abandoned `Child` would leave the exited process as a zombie for the
    /// lifetime of the window — one per file opened in an editor.
    #[test]
    fn spawn_reaped_leaves_no_zombie() {
        let dir = std::env::temp_dir().join(format!("magritte-reap-test-{}", std::process::id()));
        // Clear first: a stranded `pid` file from an earlier run that died on
        // this pid would be read as "already reaped" and pass without ever
        // observing this run's child.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        assert!(super::spawn_reaped(
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("echo $$ > {}", pidfile.display()))
        ));

        let start = Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "child never ran");
            std::thread::sleep(Duration::from_millis(10));
        };

        // A reaped pid vanishes from the process table; an un-reaped one lingers
        // as `Z` indefinitely, so waiting it out distinguishes the two.
        let start = Instant::now();
        loop {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stat.is_empty() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "pid {pid} still in the process table as {stat:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

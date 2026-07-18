//! Single-instance handoff over a Unix domain socket: a later CLI invocation
//! sends its start directory to the running app instead of opening a second
//! process. The app describes itself with an [`Instance`] and calls the
//! methods on it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use async_channel::Sender;

/// The app identity the handoff socket is keyed by, plus the env vars that
/// disable it.
pub struct Instance {
    /// Directory under `~/Library/Application Support` holding the socket
    /// (macOS), e.g. `"Magritte"`.
    pub app_support_dir: &'static str,
    /// Directory name for the XDG fallback paths (non-macOS unix), e.g.
    /// `"magritte"`.
    pub runtime_dir: &'static str,
    /// Socket file name, e.g. `"magritte.sock"`.
    pub socket_name: &'static str,
    /// Env var that disables single-instance handoff when set.
    pub disable_env: &'static str,
    /// The debug control channel's directory env var; handoff is disabled
    /// while it is set.
    pub debug_dir_env: &'static str,
}

impl Instance {
    /// Disable the single-instance handoff when we are running the debug
    /// control channel: developers often want multiple isolated instances
    /// against scratch repos. `disable_env` is a manual escape hatch too.
    pub fn enabled(&self) -> bool {
        std::env::var_os(self.disable_env).is_none()
            && std::env::var_os(self.debug_dir_env).is_none()
    }

    /// Send an open-repo request to an already-running instance. Returns false
    /// when no listener is available, so the caller should become the app.
    #[cfg(unix)]
    pub fn try_handoff(&self, start_dir: Option<&Path>) -> bool {
        use std::os::unix::net::UnixStream;

        let Ok(mut stream) = UnixStream::connect(self.socket_path()) else {
            return false;
        };
        let path = request_path(start_dir);
        if writeln!(stream, "{}", path.to_string_lossy()).is_err() {
            return false;
        }
        // Close our write half so the server's read returns, then wait for its
        // one-byte ack: a successful write alone doesn't mean the request was
        // received (the server could die before reading), and a false success
        // here would mean no window opens anywhere.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut ack = [0u8; 1];
        stream.read_exact(&mut ack).is_ok()
    }

    #[cfg(not(unix))]
    pub fn try_handoff(&self, _start_dir: Option<&Path>) -> bool {
        false
    }

    /// Start listening for open-repo requests from later CLI invocations.
    /// Returns false when another instance appears to own the socket already.
    #[cfg(unix)]
    pub fn start_server(&self, tx: Sender<PathBuf>) -> bool {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let path = self.socket_path();
        let Some(dir) = path.parent() else {
            return false;
        };
        let _ = fs::create_dir_all(dir);
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));

        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(_) => {
                // If another process is really listening, leave it alone. Otherwise
                // clear a stale socket from an unclean exit and try once more.
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return false;
                }
                let _ = fs::remove_file(&path);
                let Ok(listener) = UnixListener::bind(&path) else {
                    return false;
                };
                listener
            }
        };

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Connections are served serially; without a deadline one client
                // that connects and never closes its write half would park this
                // thread forever, wedging single-instance handoff for good.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut request = String::new();
                if stream.read_to_string(&mut request).is_err() {
                    continue;
                }
                let path = request.trim_end_matches(['\r', '\n']);
                if path.is_empty() {
                    continue;
                }
                if tx.send_blocking(PathBuf::from(path)).is_ok() {
                    // Ack so the handing-off process knows the request landed.
                    let _ = stream.write_all(b"\n");
                }
            }
        });
        true
    }

    #[cfg(not(unix))]
    pub fn start_server(&self, _tx: Sender<PathBuf>) -> bool {
        false
    }

    #[cfg(target_os = "macos")]
    fn socket_path(&self) -> PathBuf {
        home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Application Support")
            .join(self.app_support_dir)
            .join(self.socket_name)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn socket_path(&self) -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".cache").join(self.runtime_dir)))
            .unwrap_or_else(|| std::env::temp_dir().join(self.runtime_dir))
            .join(self.socket_name)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(unix)]
fn request_path(start_dir: Option<&Path>) -> PathBuf {
    let path = start_dir
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

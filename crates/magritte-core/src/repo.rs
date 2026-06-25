use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// How many recent git invocations the command log keeps (a ring buffer).
const LOG_CAPACITY: usize = 500;

/// A handle to a git working tree.
///
/// `Repo` is deliberately synchronous and cheap to clone: it holds the
/// working-directory path and a shared command log. Every method shells out to
/// the `git` binary and returns plain data. The frontend is responsible for
/// running these calls off the UI thread (e.g. on a background executor) and
/// for cancellation. Clones share one command log (it's behind an `Arc`), so a
/// `Repo` cloned onto a background thread still records into the same log the
/// UI reads.
#[derive(Debug, Clone)]
pub struct Repo {
    workdir: PathBuf,
    log: Arc<Mutex<VecDeque<GitCommand>>>,
}

/// One recorded git invocation, for the command log (magit's process buffer).
#[derive(Debug, Clone)]
pub struct GitCommand {
    /// The git arguments, without the `git -C <dir>` boilerplate or the
    /// internal `-c core.quotepath=false` flags.
    pub args: Vec<String>,
    /// The process exit code, or `None` if it was killed by a signal or failed
    /// to spawn.
    pub code: Option<i32>,
    /// Whether git exited successfully (status 0).
    pub ok: bool,
    /// git's stderr — its progress/error narrative (`Switched to branch …`,
    /// fetch progress, error messages). Empty for the predicate `succeeds`
    /// calls, which discard output.
    pub stderr: String,
}

impl GitCommand {
    /// The command as a user would type it, e.g. `git fetch origin`.
    pub fn display(&self) -> String {
        format!("git {}", self.args.join(" "))
    }

    /// Whether this is a read-only query the UI issues on its own — the status
    /// refresh, diffs, and ref lookups — rather than something the user invoked.
    /// These are noise in the command log, so it hides them by default.
    pub fn is_query(&self) -> bool {
        match self.args.first().map(String::as_str) {
            Some(
                "status" | "diff" | "rev-parse" | "for-each-ref" | "show-ref" | "ls-files"
                | "symbolic-ref",
            ) => true,
            // Config *reads* (e.g. resolving the push-remote) are queries; a
            // config write (setting one) is a user action, so keep it visible.
            Some("config") => self.args.iter().any(|a| a == "--get" || a == "--get-all"),
            _ => false,
        }
    }
}

/// The raw result of a git invocation.
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Repo {
    /// Open the working tree that `path` belongs to, resolving to the top level.
    ///
    /// Returns [`Error::NotARepository`] if `path` is not tracked by git.
    pub fn discover(path: impl AsRef<Path>) -> Result<Repo> {
        let path = path.as_ref();
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|source| Error::Spawn { source })?;

        if !output.status.success() {
            return Err(Error::NotARepository {
                path: path.to_path_buf(),
            });
        }

        let top = String::from_utf8(output.stdout)
            .map_err(|_| Error::Encoding {
                context: "rev-parse --show-toplevel",
            })?
            .trim_end()
            .to_string();

        Ok(Repo {
            workdir: PathBuf::from(top),
            log: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// Construct a `Repo` for an already-known working-tree root without probing.
    pub fn at(workdir: impl Into<PathBuf>) -> Repo {
        Repo {
            workdir: workdir.into(),
            log: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// A snapshot of the recent git invocations, oldest first (for the command
    /// log view — magit's `$` process buffer).
    pub fn command_log(&self) -> Vec<GitCommand> {
        self.log
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Record one invocation in the ring-buffered command log.
    fn record(&self, args: &[String], code: Option<i32>, stderr: &str) {
        if let Ok(mut q) = self.log.lock() {
            if q.len() >= LOG_CAPACITY {
                q.pop_front();
            }
            q.push_back(GitCommand {
                args: args.to_vec(),
                code,
                ok: code == Some(0),
                stderr: stderr.to_string(),
            });
        }
    }

    /// Run `git <args>` in the working tree, returning stdout as raw bytes so
    /// that NUL-delimited (`-z`) output is preserved.
    pub fn run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            // Keep output stable and machine-readable regardless of user config.
            .args(["-c", "core.quotepath=false"])
            // Never block on an interactive credential/passphrase prompt: fail
            // fast instead of hanging a background thread with no terminal.
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(&arg_vec)
            .output()
            .map_err(|source| Error::Spawn { source })?;

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        self.record(&arg_vec, output.status.code(), &stderr);

        if !output.status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: output.status.code(),
                stderr,
            });
        }

        Ok(GitOutput {
            stdout: output.stdout,
            stderr,
        })
    }

    /// Like [`run`](Self::run) but feeds `input` to git's stdin. Used to pipe
    /// patches to `git apply`.
    pub fn run_with_input<I, S>(&self, args: I, input: &[u8]) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(&arg_vec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn { source })?;

        // Write the whole input, then drop the handle to signal EOF. Ignore a
        // write error: if git exited early (e.g. it rejected the patch) the
        // pipe breaks here, but git's real status + stderr are the authoritative
        // signal — so always wait and report those rather than a generic
        // broken-pipe error.
        {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            let _ = stdin.write_all(input);
        }

        let output = child
            .wait_with_output()
            .map_err(|source| Error::Spawn { source })?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        self.record(&arg_vec, output.status.code(), &stderr);

        if !output.status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: output.status.code(),
                stderr,
            });
        }

        Ok(GitOutput {
            stdout: output.stdout,
            stderr,
        })
    }

    /// Run `git <args>` and report whether it exited successfully, without
    /// treating a non-zero exit as an error. For predicate commands such as
    /// `git diff --quiet` (exit 1 means "there are differences").
    pub fn succeeds<I, S>(&self, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(&arg_vec)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|source| Error::Spawn { source })?;
        // Output is discarded (Stdio::null), so there's no stderr to log.
        self.record(&arg_vec, status.code(), "");
        Ok(status.success())
    }

    /// Like [`run`](Self::run) but a non-zero exit yields `Ok(None)` rather than
    /// an error — for queries where "no result" is expected (an unset config
    /// key, a branch with no upstream, …).
    pub fn run_optional<I, S>(&self, args: I) -> Result<Option<GitOutput>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(&arg_vec)
            .output()
            .map_err(|source| Error::Spawn { source })?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        self.record(&arg_vec, output.status.code(), &stderr);
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(GitOutput {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            stdout: output.stdout,
        }))
    }

    /// Read a single git config value (`git config --get <key>`), `None` if
    /// unset.
    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.run_optional(["config", "--get", key])?.and_then(|o| {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!v.is_empty()).then_some(v)
        }))
    }
}

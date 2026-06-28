use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// How many recent git invocations the command log keeps (a ring buffer).
const LOG_CAPACITY: usize = 500;

/// Distinguishes concurrent sequence-editor todo temp files (parallel tests, or
/// two rebases at once), since the pid alone isn't unique across threads.
static SEQ_TODO_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    /// When set, every invocation polls this flag and kills the child (returning
    /// [`Error::Cancelled`]) once it flips true — so a superseded or user
    /// -cancelled job stops *running*, not just gets its result dropped. Shared
    /// via the `Arc` so the caller can trigger it after handing the `Repo` to a
    /// background job. `None` means uncancellable (the fast `.output()` path).
    cancel: Option<Arc<AtomicBool>>,
    /// When set, an invocation exceeding this kills the child and returns
    /// [`Error::TimedOut`] — a backstop against a wedged remote/hook.
    timeout: Option<Duration>,
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
#[derive(Debug)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    /// Trimmed stdout as text (lossy UTF-8).
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// The one-line summary for commands whose result is the first line of
    /// stdout (e.g. `commit` → `[main abc123] subject`), falling back to stderr.
    pub fn first_line(&self) -> String {
        let stdout = self.stdout_text();
        if stdout.is_empty() {
            self.stderr.trim().to_string()
        } else {
            stdout.lines().next().unwrap_or("").to_string()
        }
    }

    /// The one-line summary for commands that print their status to stderr
    /// (rebase/cherry-pick/sequence progress): its last non-empty line, falling
    /// back to stdout.
    pub fn status_line(&self) -> String {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            self.stdout_text()
        } else {
            stderr.lines().next_back().unwrap_or("").to_string()
        }
    }

    /// The full stderr report (e.g. a push/pull/fetch summary, which can span
    /// lines), falling back to stdout.
    pub fn report(&self) -> String {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            self.stdout_text()
        } else {
            stderr.to_string()
        }
    }
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
            cancel: None,
            timeout: None,
        })
    }

    /// Construct a `Repo` for an already-known working-tree root without probing.
    pub fn at(workdir: impl Into<PathBuf>) -> Repo {
        Repo {
            workdir: workdir.into(),
            log: Arc::new(Mutex::new(VecDeque::new())),
            cancel: None,
            timeout: None,
        }
    }

    /// A clone of this repo whose invocations are cancellable, paired with the
    /// flag that cancels them. Hand the `Repo` to a background job and keep the
    /// flag; setting it kills the in-flight git child. The clone shares the
    /// command log (so its invocations still show in the `$` view).
    pub fn cancellable(&self) -> (Repo, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let mut repo = self.clone();
        repo.cancel = Some(flag.clone());
        (repo, flag)
    }

    /// A clone of this repo whose invocations time out after `d` (the child is
    /// killed and [`Error::TimedOut`] returned).
    pub fn with_timeout(&self, d: Duration) -> Repo {
        let mut repo = self.clone();
        repo.timeout = Some(d);
        repo
    }

    /// A clone of this repo cancelled by an existing flag — for sharing one
    /// cancel signal across a batch of jobs (e.g. all reads of a generation,
    /// cancelled together when a newer refresh supersedes them).
    pub fn with_cancel(&self, flag: Arc<AtomicBool>) -> Repo {
        let mut repo = self.clone();
        repo.cancel = Some(flag);
        repo
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

    /// A `git` command rooted at the working tree, with the signal environment
    /// of a shell. We spawn from a GPUI worker thread whose signal mask blocks
    /// signals; git's children inherit it, so when one fails mid-transport
    /// (e.g. a pull of a missing ref) git can't signal its stuck `upload-pack`
    /// child during cleanup and `git pull` hangs forever instead of erroring.
    /// Resetting the mask in the child fixes it. `GIT_TERMINAL_PROMPT=0` keeps
    /// git from blocking on a credential prompt with no terminal.
    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            // Keep output stable and machine-readable regardless of user config.
            .args(["-c", "core.quotepath=false"])
            .env("GIT_TERMINAL_PROMPT", "0");
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                // Only async-signal-safe calls here (post-fork, pre-exec).
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                Ok(())
            });
        }
        cmd
    }

    /// Run `git <args>` in the working tree, returning stdout as raw bytes so
    /// that NUL-delimited (`-z`) output is preserved. Honors this repo's cancel
    /// flag and timeout (if set) — see [`cancellable`](Self::cancellable).
    pub fn run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let mut cmd = self.git();
        cmd.args(&arg_vec);
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        self.record(&arg_vec, status.code(), &stderr);

        if !status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: status.code(),
                stderr,
            });
        }

        Ok(GitOutput { stdout, stderr })
    }

    /// Run `cmd` to completion, returning `(stdout, stderr, status)`.
    ///
    /// Without a cancel flag or timeout this is plain [`Command::output`]. With
    /// either set, it spawns the child and polls for exit while *draining both
    /// pipes on helper threads* — a full pipe would otherwise deadlock the wait
    /// — and kills the child on cancel ([`Error::Cancelled`]) or deadline
    /// ([`Error::TimedOut`]), reaping it so no zombie is left behind.
    fn collect_output(&self, mut cmd: Command) -> Result<(Vec<u8>, String, ExitStatus)> {
        if self.cancel.is_none() && self.timeout.is_none() {
            let out = cmd.output().map_err(|source| Error::Spawn { source })?;
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Ok((out.stdout, stderr, out.status));
        }

        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn { source })?;
        let mut out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out_pipe.read_to_end(&mut buf);
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        });

        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|source| Error::Spawn { source })? {
                break status;
            }
            let cancelled = self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed));
            let timed_out = self.timeout.is_some_and(|t| start.elapsed() >= t);
            if cancelled || timed_out {
                let _ = child.kill();
                let _ = child.wait();
                // Don't join the reader threads: we discard the output, and a
                // killed git can leave a grandchild (e.g. a hook) holding the
                // pipe's write end open, which would block the read until *it*
                // exits — defeating the prompt cancel. Let the readers detach;
                // they finish when the pipe finally closes.
                return Err(if cancelled {
                    Error::Cancelled
                } else {
                    Error::TimedOut
                });
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        Ok((stdout, stderr, status))
    }

    /// Like [`run`](Self::run) but with one extra environment variable set.
    /// Used to point `GIT_EDITOR` at the user's editor for an interactive
    /// `git commit` (which blocks until the editor exits), without disturbing
    /// the rest of git's environment.
    pub fn run_with_env<I, S>(&self, args: I, key: &str, value: &str) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();

        let output = self
            .git()
            .env(key, value)
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

        let mut child = self
            .git()
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

    /// Run `git <args>` where git would normally open the **sequence editor**
    /// (`rebase -i`, etc.), feeding it `todo` non-interactively. A throwaway
    /// `sequence.editor` copies `todo` over git's generated todo, and
    /// `GIT_EDITOR` is neutralized (`true`) so any `reword`/`squash` keeps its
    /// default message instead of blocking on an editor. The temp file is
    /// removed regardless of outcome. This isolates the no-TTY plumbing from the
    /// callers' domain logic (which just builds the todo + argv).
    pub fn run_with_sequence_editor(&self, todo: &str, args: &[String]) -> Result<GitOutput> {
        // A unique temp file (space-free path) holds the todo; pid+counter keeps
        // concurrent runs (and parallel tests) from sharing one file.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SEQ_TODO_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(format!("magritte-seq-todo-{unique}"));
        std::fs::write(&path, todo)
            .map_err(|e| Error::Message(format!("{}: {e}", path.display())))?;

        let mut argv = vec![
            "-c".to_string(),
            format!("sequence.editor=cp '{}'", path.display()),
        ];
        argv.extend(args.iter().cloned());

        let result = self.run_with_env(&argv, "GIT_EDITOR", "true");
        let _ = std::fs::remove_file(&path);
        result
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
        let status = self
            .git()
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
        let output = self
            .git()
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

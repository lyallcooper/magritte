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

/// One recorded command invocation, for the command log (magit's process
/// buffer). Usually git, but a user `!` shell escape records its program too.
#[derive(Debug, Clone)]
pub struct GitCommand {
    /// The program, if not git (a user shell command). `None` is the common
    /// case — a git subcommand, displayed with a `git` prefix.
    pub program: Option<String>,
    /// The arguments, without the `git -C <dir>` boilerplate or the internal
    /// `-c core.quotepath=false` flags.
    pub args: Vec<String>,
    /// The process exit code, or `None` if it was killed by a signal or failed
    /// to spawn.
    pub code: Option<i32>,
    /// Whether the command exited successfully (status 0).
    pub ok: bool,
    /// Whether the user invoked this directly (the `!` prompt), as opposed to
    /// the UI issuing it. User commands always show in the log (never hidden as
    /// a query) and keep their full output.
    pub user: bool,
    /// Captured stdout. Empty for the internal git calls (whose stdout the UI
    /// consumes directly); populated for user `!` commands so the log shows
    /// their full output.
    pub stdout: String,
    /// stderr — git's progress/error narrative (`Switched to branch …`, fetch
    /// progress, error messages), or a user command's. Empty for the predicate
    /// `succeeds` calls, which discard output.
    pub stderr: String,
}

impl GitCommand {
    /// The command as a user would type it, e.g. `git fetch origin` or `ls -la`.
    pub fn display(&self) -> String {
        let prog = self.program.as_deref().unwrap_or("git");
        format!("{prog} {}", self.args.join(" "))
    }

    /// Whether this is a read-only query the UI issues on its own — the status
    /// refresh, diffs, and ref lookups — rather than something the user invoked.
    /// These are noise in the command log, so it hides them by default.
    pub fn is_query(&self) -> bool {
        if self.user {
            return false;
        }
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

/// The result of a user `!` command: its text output and whether it succeeded.
/// Unlike [`GitOutput`], a non-zero exit isn't an error here.
#[derive(Debug)]
pub struct CommandRun {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Configure a child process for spawning from our GPUI worker threads — shared
/// by the `git` wrapper, the `!` prompt's arbitrary commands, and user
/// `[[command]]` shell commands so they all behave the same (it matters once any
/// of them invokes a networked git, directly or through `sh`):
///
/// - **Reset the signal mask.** Our worker threads block signals; children
///   inherit that, so when git's transport child (e.g. `upload-pack`) fails
///   mid-pull, git can't signal it during cleanup and hangs forever instead of
///   erroring. Clearing the mask in the child fixes it. The mask is inherited
///   across an intermediate `sh`, so resetting `sh`'s reaches git.
/// - **`GIT_TERMINAL_PROMPT=0`.** No terminal here, so git must not block on a
///   credential prompt; inherited by `sh` and its git child.
fn prepare_spawn(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
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
    fn record(&self, cmd: GitCommand) {
        if let Ok(mut q) = self.log.lock() {
            if q.len() >= LOG_CAPACITY {
                q.pop_front();
            }
            q.push_back(cmd);
        }
    }

    /// Record an internal git call (the UI's own invocations): a `git` command,
    /// not user-invoked, with stdout consumed by the caller rather than stored.
    fn record_git(&self, args: &[String], code: Option<i32>, stderr: &str) {
        self.record(GitCommand {
            program: None,
            args: args.to_vec(),
            code,
            ok: code == Some(0),
            user: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        });
    }

    /// A `git` command rooted at the working tree, with the spawn environment
    /// git needs under our worker threads (see [`prepare_spawn`]).
    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            // Keep output stable and machine-readable regardless of user config.
            .args(["-c", "core.quotepath=false"]);
        prepare_spawn(&mut cmd);
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
        self.finish(arg_vec, stdout, stderr, status)
    }

    /// Run a user-typed command from the `!` prompt — git by default, or an
    /// arbitrary `program` (its shell escape) run in the working tree. Unlike
    /// [`run`](Self::run), a non-zero exit is *not* an error: the output is the
    /// point, so it's returned either way. The full output is recorded in the
    /// command log (`user`-flagged, so it's always shown there).
    pub fn run_user(&self, program: Option<&str>, args: &[String]) -> Result<CommandRun> {
        let cmd = match program {
            None => {
                let mut c = self.git();
                c.args(args);
                c
            }
            Some(p) => {
                let mut c = Command::new(p);
                c.current_dir(&self.workdir).args(args);
                prepare_spawn(&mut c);
                c
            }
        };
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        self.record(GitCommand {
            program: program.map(String::from),
            args: args.to_vec(),
            code: status.code(),
            ok: status.success(),
            user: true,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
        });
        Ok(CommandRun {
            ok: status.success(),
            stdout,
            stderr,
        })
    }

    /// Run a user `[[command]]` — an arbitrary shell command (`sh -c`) in the
    /// working tree, supporting `&&`, pipes, etc. Like [`run_user`](Self::run_user),
    /// a non-zero exit isn't an error. Recorded in the command log as the command
    /// was written (split for display only — it runs via the shell).
    pub fn run_shell(&self, command: &str) -> Result<CommandRun> {
        let mut cmd = Command::new("sh");
        cmd.current_dir(&self.workdir).arg("-c").arg(command);
        prepare_spawn(&mut cmd);
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        // For the log: show the command as written. The first word reads as the
        // "program" (dim) and the rest as its arguments, like a git line.
        let mut words = command.split_whitespace().map(String::from);
        self.record(GitCommand {
            program: words.next(),
            args: words.collect(),
            code: status.code(),
            ok: status.success(),
            user: true,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
        });
        Ok(CommandRun {
            ok: status.success(),
            stdout,
            stderr,
        })
    }

    /// Run `cmd` to completion, returning `(stdout, stderr, status)`. `input`,
    /// when given, is written to the child's stdin.
    ///
    /// Without a cancel flag or timeout this is plain [`Command::output`] (or a
    /// spawn + stdin write). With either set, it spawns the child and polls for
    /// exit while *draining both pipes on helper threads* — a full pipe would
    /// otherwise deadlock the wait — and writing any stdin on its own thread for
    /// the same reason; it kills the child on cancel ([`Error::Cancelled`]) or
    /// deadline ([`Error::TimedOut`]), reaping it so no zombie is left behind.
    ///
    /// Routing every variant (incl. `run_with_env`, `run_with_input`) through
    /// here is what makes them all honor the cancel flag and timeout.
    fn collect_output_with(
        &self,
        mut cmd: Command,
        input: Option<&[u8]>,
    ) -> Result<(Vec<u8>, String, ExitStatus)> {
        if self.cancel.is_none() && self.timeout.is_none() {
            // Fast path: no cancellation/timeout to honor.
            let out = match input {
                None => cmd.output().map_err(|source| Error::Spawn { source })?,
                Some(input) => {
                    let mut child = cmd
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|source| Error::Spawn { source })?;
                    {
                        let mut stdin = child.stdin.take().expect("stdin piped");
                        let _ = stdin.write_all(input);
                    }
                    child
                        .wait_with_output()
                        .map_err(|source| Error::Spawn { source })?
                }
            };
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Ok((out.stdout, stderr, out.status));
        }

        let mut child = cmd
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn { source })?;
        // Write stdin on its own thread so a large patch can't deadlock against
        // git's output filling the stdout pipe before it has consumed stdin.
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("stdin piped");
            let buf = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&buf);
            });
        }
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

    /// Run `cmd` with no stdin — the common case.
    fn collect_output(&self, cmd: Command) -> Result<(Vec<u8>, String, ExitStatus)> {
        self.collect_output_with(cmd, None)
    }

    /// The shared tail of the erroring `run*` variants: record the invocation,
    /// then map a non-zero exit to [`Error::Git`].
    fn finish(
        &self,
        arg_vec: Vec<String>,
        stdout: Vec<u8>,
        stderr: String,
        status: ExitStatus,
    ) -> Result<GitOutput> {
        self.record_git(&arg_vec, status.code(), &stderr);
        if !status.success() {
            return Err(Error::Git {
                args: arg_vec,
                status: status.code(),
                stderr,
            });
        }
        Ok(GitOutput { stdout, stderr })
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

        let mut cmd = self.git();
        cmd.env(key, value).args(&arg_vec);
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        self.finish(arg_vec, stdout, stderr, status)
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

        let mut cmd = self.git();
        cmd.args(&arg_vec);
        // Route through collect_output_with so the stdin path also honors the
        // cancel flag and timeout (a wedged hook reading the patch can't hang
        // forever, and C-g/Esc kills it).
        let (stdout, stderr, status) = self.collect_output_with(cmd, Some(input))?;
        self.finish(arg_vec, stdout, stderr, status)
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
        self.record_git(&arg_vec, status.code(), "");
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
        self.record_git(&arg_vec, output.status.code(), &stderr);
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

    /// Read a boolean git config value (`git config --type=bool --get <key>`),
    /// canonicalized by git to `true`/`false`. `false` if unset or unreadable.
    pub fn config_bool(&self, key: &str) -> bool {
        self.run_optional(["config", "--type=bool", "--get", key])
            .ok()
            .flatten()
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
    }

    /// The nearest tag reachable from HEAD (with commits-since) and the nearest
    /// tag that *contains* HEAD (with commits-until) — magit's status "Tag/Tags"
    /// header (`magit-get-current-tag` / `magit-get-next-tag`). Either is `None`
    /// when there's no such tag.
    pub fn tags_around(&self) -> (Option<(String, usize)>, Option<(String, usize)>) {
        let current = self.current_tag();
        let next = self.next_tag(current.as_ref().map(|(t, _)| t.as_str()));
        (current, next)
    }

    /// `git describe --long --tags` → `(tag, commits-since)`; `None` if untagged.
    fn current_tag(&self) -> Option<(String, usize)> {
        let out = self
            .run_optional(["describe", "--long", "--tags"])
            .ok()
            .flatten()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // "<tag>-<count>-g<hash>": strip the "-g<hash>", then split the count.
        let without_hash = s.rsplit_once("-g")?.0;
        let (tag, count) = without_hash.rsplit_once('-')?;
        Some((tag.to_string(), count.parse().ok()?))
    }

    /// `git describe --contains HEAD` → `(tag, commits-until)` for the nearest
    /// tag HEAD is an ancestor of; `None` if none, or if it's the current tag.
    fn next_tag(&self, current: Option<&str>) -> Option<(String, usize)> {
        let out = self
            .run_optional(["describe", "--contains", "HEAD"])
            .ok()
            .flatten()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // "<tag>" possibly suffixed with `~N` / `^N`.
        let tag = s.split(['~', '^']).next().unwrap_or("").to_string();
        if tag.is_empty() || Some(tag.as_str()) == current {
            return None;
        }
        let count = self
            .run_optional(["rev-list", "--count", &format!("HEAD..{tag}")])
            .ok()
            .flatten()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0);
        Some((tag, count))
    }

    /// Ignored file paths (`git ls-files --others --ignored --exclude-standard`),
    /// repo-relative. For the opt-in `ignored` status section.
    pub fn ignored_files(&self) -> Result<Vec<String>> {
        let out = self.run([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// The repository's common git directory (`git rev-parse --git-common-dir`),
    /// as an absolute path. It's shared across linked worktrees, so per-repo
    /// state keyed off it lands in one place for the whole repo. `None` on error.
    pub fn git_common_dir(&self) -> Option<PathBuf> {
        let out = self.run(["rev-parse", "--git-common-dir"]).ok()?;
        let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if raw.is_empty() {
            return None;
        }
        // git reports it relative to the working tree we ran in (`-C workdir`).
        let dir = PathBuf::from(&raw);
        Some(if dir.is_absolute() {
            dir
        } else {
            self.workdir.join(dir)
        })
    }

    /// Whether `git pull` rebases by default, mirroring git's own resolution:
    /// `branch.<name>.rebase` overrides `pull.rebase`, and a value counts as
    /// rebase when it's `true`/`interactive`/`merges` (or the deprecated
    /// `preserve`) — so it can't go through [`config_bool`], whose `--type=bool`
    /// rejects those enum values.
    pub fn pull_rebase_default(&self, branch: Option<&str>) -> bool {
        fn rebase_ish(v: &str) -> bool {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1" | "interactive" | "merges" | "preserve"
            )
        }
        if let Some(b) = branch {
            if let Ok(Some(v)) = self.config_get(&format!("branch.{b}.rebase")) {
                return rebase_ish(&v);
            }
        }
        matches!(self.config_get("pull.rebase"), Ok(Some(v)) if rebase_ish(&v))
    }
}

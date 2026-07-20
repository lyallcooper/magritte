use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Configure a child process for spawning from GPUI worker threads — shared by
/// every engine invocation, the `!` prompt's arbitrary commands, and user
/// `[[command]]` shell commands so they all behave the same:
///
/// - **Reset the signal mask.** Our worker threads block signals; children
///   inherit that, so when a transport child (e.g. git's `upload-pack`) fails
///   mid-transfer, its parent can't signal it during cleanup and hangs forever
///   instead of erroring. Clearing the mask in the child fixes it. The mask is
///   inherited across an intermediate `sh`, so resetting `sh`'s reaches its
///   children.
/// - **A fresh process group.** The child becomes a group leader, so
///   cancellation can signal the *whole tree* — a VCS child spawns transports,
///   hooks, credential helpers, and editors, and killing only the direct child
///   would leave those running (possibly holding locks or the output pipes).
///   Consequence when the app runs foregrounded in a terminal: children are
///   outside the terminal's foreground group, so Ctrl-C no longer reaches
///   them (an in-flight fetch survives quitting a `--foreground` run), and a
///   child that reads the controlling tty gets SIGTTIN and stops instead of
///   prompting. Engines must therefore suppress prompts via environment
///   (git's `GIT_TERMINAL_PROMPT=0`), which they need for the detached GUI
///   case anyway.
///
/// Engine-specific environment (e.g. git's `GIT_TERMINAL_PROMPT=0`) is layered
/// on by the engine crates, not here.
pub fn prepare_spawn(cmd: &mut Command) {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // Only async-signal-safe calls here (post-fork, pre-exec).
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// How long, after SIGTERM, to let a cancelled child clean up before we SIGKILL
/// it. Runs on a background worker thread, so it never blocks the UI.
const TERMINATE_GRACE_MS: u64 = 300;

/// Signal the child's whole process group (it made itself the leader in
/// [`prepare_spawn`]), falling back to the direct pid in case `setpgid`
/// failed — its return value is deliberately ignored in `pre_exec`, and a
/// group-less child must still die. (There is no fork→setpgid race to cover:
/// `spawn()` only returns after exec is confirmed, by which point `pre_exec`
/// has run.)
///
/// SAFETY (of the pid): the caller holds the un-reaped `Child`, so its pid —
/// and therefore the group id — can't have been recycled.
#[cfg(unix)]
fn signal_tree(child: &std::process::Child, sig: libc::c_int) {
    let pid = child.id() as libc::pid_t;
    unsafe {
        if libc::kill(-pid, sig) != 0 {
            libc::kill(pid, sig);
        }
    }
}

/// Stop a child we're cancelling, and its descendants. SIGTERM first, so a
/// VCS process runs its cleanup — git's lockfile handler unlinks any `*.lock`
/// it holds, notably `.git/index.lock` from an interrupted `commit`/`add`. A
/// plain SIGKILL can't be caught, so it would orphan that lock and wedge the
/// next command with "Unable to create '.git/index.lock': File exists".
/// SIGKILL is the fallback if the tree ignores SIGTERM (e.g. wedged on the
/// network). Both signals go to the child's process group so transports,
/// hooks, and helpers die with it. One accepted gap: if the *leader* exits
/// within the grace period, a group member that ignored SIGTERM is never
/// SIGKILLed — signaling the group after reaping the leader would race pgid
/// reuse. (Still strictly better than pre-group-kill behavior, where such a
/// member was never signaled at all; the detached pipe readers tolerate it.)
#[cfg(unix)]
fn terminate(child: &mut std::process::Child) {
    signal_tree(child, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(TERMINATE_GRACE_MS);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return; // exited (and reaped) after cleaning up
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    signal_tree(child, libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Cancellation and timeout policy for collecting a child process — the
/// engine-agnostic back half of every command invocation. Both fields are
/// optional; with neither set, collection is a plain blocking wait.
#[derive(Debug, Clone, Default)]
pub struct ProcessControl {
    /// When set, collection polls this flag and kills the child's process tree
    /// (returning [`Error::Cancelled`]) once it flips true — so a superseded
    /// or user-cancelled job stops *running*, not just gets its result
    /// dropped. `None` means uncancellable (the fast blocking path).
    pub cancel: Option<Arc<AtomicBool>>,
    /// When set, an invocation exceeding this kills the tree and returns
    /// [`Error::TimedOut`] — a backstop against a wedged remote/hook.
    pub timeout: Option<Duration>,
}

impl ProcessControl {
    /// Run `cmd` to completion, returning `(stdout, stderr, status)`. `input`,
    /// when given, is written to the child's stdin.
    ///
    /// Without a cancel flag or timeout this is plain [`Command::output`] (or a
    /// spawn + stdin write). With either set, it spawns the child and polls for
    /// exit while *draining both pipes on helper threads* — a full pipe would
    /// otherwise deadlock the wait — and writing any stdin on its own thread for
    /// the same reason; it kills the child's process tree on cancel
    /// ([`Error::Cancelled`]) or deadline ([`Error::TimedOut`]), reaping the
    /// child so no zombie is left behind.
    ///
    /// Routing every invocation variant through here is what makes them all
    /// honor the cancel flag and timeout.
    pub fn collect_output_with(
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
                    // Write stdin on its own thread, like the guarded path: a
                    // large patch could otherwise deadlock against the child
                    // filling the stdout pipe before it has consumed stdin.
                    let mut stdin = child.stdin.take().expect("stdin piped");
                    let buf = input.to_vec();
                    let writer = std::thread::spawn(move || {
                        let _ = stdin.write_all(&buf);
                    });
                    let out = child
                        .wait_with_output()
                        .map_err(|source| Error::Spawn { source })?;
                    let _ = writer.join();
                    out
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
        // the child's output filling the stdout pipe before it has consumed
        // stdin.
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
        // Poll with backoff: most VCS calls finish in a few ms, so a fixed
        // 15ms sleep would tax every cancellable invocation ~7ms on average —
        // several sequential calls per refresh. Start at 1ms and grow toward
        // 15ms so short calls return promptly and long ones stay cheap to poll.
        let mut poll = Duration::from_millis(1);
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
                // SIGTERM (then SIGKILL) so the child can unlink any lock it
                // holds — e.g. `.git/index.lock` from a cancelled commit —
                // rather than orphaning it.
                terminate(&mut child);
                // Don't join the reader threads: we discard the output, and a
                // grandchild outside the killed process group could hold the
                // pipe's write end open, which would block the read until *it*
                // exits — defeating the prompt cancel. Let the readers detach;
                // they finish when the pipe finally closes.
                return Err(if cancelled {
                    Error::Cancelled
                } else {
                    Error::TimedOut
                });
            }
            std::thread::sleep(poll);
            poll = (poll * 2).min(Duration::from_millis(15));
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        Ok((stdout, stderr, status))
    }
}

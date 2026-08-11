use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// How long to wait for the output readers once the direct child has exited.
/// The child's own output is fully written by then, so this only covers
/// draining what is still sitting in the pipe. Any process that outlived the
/// child while holding the write end — an `ssh` `ControlPersist` master, a hook
/// that backgrounded itself without redirecting stdio — keeps `read` blocking
/// for as long as *it* lives, which without a bound hangs the invocation past
/// its timeout and deaf to its cancel flag.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// How long a draining reader blocks in `poll` before re-checking whether it has
/// been told to stop. Only reached while a pipe is idle, so the cost is a couple
/// of wakeups a second per in-flight command.
#[cfg(unix)]
const DRAIN_POLL_MS: libc::c_int = 250;

/// The capabilities the pipe helpers need: the relevant half of IO, plus (on
/// unix) a raw fd to poll, so IO blocked on a pipe someone else holds open can
/// be given up on.
#[cfg(unix)]
trait ReadPipe: Read + std::os::unix::io::AsRawFd + Send + 'static {}
#[cfg(unix)]
impl<T: Read + std::os::unix::io::AsRawFd + Send + 'static> ReadPipe for T {}
#[cfg(not(unix))]
trait ReadPipe: Read + Send + 'static {}
#[cfg(not(unix))]
impl<T: Read + Send + 'static> ReadPipe for T {}

#[cfg(unix)]
trait WritePipe: Write + std::os::unix::io::AsRawFd + Send + 'static {}
#[cfg(unix)]
impl<T: Write + std::os::unix::io::AsRawFd + Send + 'static> WritePipe for T {}
#[cfg(not(unix))]
trait WritePipe: Write + Send + 'static {}
#[cfg(not(unix))]
impl<T: Write + Send + 'static> WritePipe for T {}

/// A helper thread doing blocking IO on one of a child's pipes, which the
/// collector must be able to abandon: a process that outlived the child while
/// holding the far end open would otherwise park the thread — stranding its fd,
/// and for stdin the whole input buffer — for as long as *it* lives.
struct PipeThread {
    /// Nothing is ever sent on this; the thread's `Sender` drops when it ends,
    /// so a disconnect *is* the finished signal — and it arrives on a panic too.
    finished: std::sync::mpsc::Receiver<()>,
    stop: Arc<AtomicBool>,
}

impl PipeThread {
    /// Run `io` on a helper thread, handing it the flag to check between chunks.
    fn spawn(io: impl FnOnce(&AtomicBool) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, finished) = std::sync::mpsc::channel();
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            let _tx = tx; // dropped on exit, waking any waiting collector
            io(&thread_stop);
        });
        PipeThread { finished, stop }
    }

    /// Tell the thread to give up at its next check and stop waiting on it.
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Wait until `deadline` for the thread to finish on its own, telling it to
    /// give up if it hasn't.
    fn settle(&self, deadline: Instant) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            self.finished.recv_timeout(remaining)
        {
            self.stop();
        }
    }
}

/// One of a child's output pipes, drained on a helper thread so a full pipe
/// can't deadlock the wait. The buffer is *shared* rather than returned from the
/// thread, so the collector can take whatever has arrived without waiting for
/// EOF.
struct PipeDrain {
    io: PipeThread,
    buf: Arc<Mutex<Vec<u8>>>,
}

impl PipeDrain {
    fn spawn(pipe: impl ReadPipe) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let thread_buf = buf.clone();
        PipeDrain {
            io: PipeThread::spawn(move |stop| drain_pipe(pipe, &thread_buf, stop)),
            buf,
        }
    }

    fn stop(&self) {
        self.io.stop();
    }

    /// Take everything read so far, waiting until `deadline` for the reader to
    /// reach EOF first. A reader still blocked when the wait ends is told to
    /// stop; bytes it appends afterwards land in the emptied buffer and are
    /// dropped with it.
    fn take(&self, deadline: Instant) -> Vec<u8> {
        self.io.settle(deadline);
        std::mem::take(&mut *lock(&self.buf))
    }
}

/// A child's stdin, fed on a helper thread so a large input can't deadlock
/// against the child filling its stdout pipe before it has consumed stdin.
struct PipeFeed {
    io: PipeThread,
}

impl PipeFeed {
    fn spawn(pipe: impl WritePipe, bytes: Vec<u8>) -> Self {
        PipeFeed {
            io: PipeThread::spawn(move |stop| feed_pipe(pipe, &bytes, stop)),
        }
    }

    /// Abandon any unwritten input. Called once the child has exited, at which
    /// point it has read everything it intended to and the rest is moot — so
    /// there is nothing to wait for, only a thread to release.
    fn stop(&self) {
        self.io.stop();
    }
}

/// Lock a drain buffer, ignoring poisoning: a panicked reader leaves the bytes
/// it had already appended perfectly usable.
fn lock(buf: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    buf.lock().unwrap_or_else(|e| e.into_inner())
}

const DRAIN_CHUNK: usize = 16 * 1024;

/// Read `pipe` into `buf` until EOF, an error, or `stop` — waiting for
/// readability with a bounded `poll` so `stop` is noticed even when nothing is
/// writing.
#[cfg(unix)]
fn drain_pipe(mut pipe: impl ReadPipe, buf: &Mutex<Vec<u8>>, stop: &AtomicBool) {
    let fd = pipe.as_raw_fd();
    let mut chunk = [0u8; DRAIN_CHUNK];
    while !stop.load(Ordering::Relaxed) {
        if !wait_ready(fd, libc::POLLIN) {
            continue;
        }
        match pipe.read(&mut chunk) {
            Ok(0) => return, // EOF: every write end is closed
            Ok(n) => lock(buf).extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// No portable way to interrupt a blocking read, so `stop` is only honored
/// between chunks: a reader parked on a pipe held open elsewhere still lingers.
#[cfg(not(unix))]
fn drain_pipe(mut pipe: impl ReadPipe, buf: &Mutex<Vec<u8>>, stop: &AtomicBool) {
    let mut chunk = [0u8; DRAIN_CHUNK];
    while !stop.load(Ordering::Relaxed) {
        match pipe.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => lock(buf).extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Write `bytes` to `pipe`, giving up on `stop`, on a reader that has closed
/// (`EPIPE` — the child took what it wanted), or on any other error.
///
/// The mirror of [`drain_pipe`], and abandonable for the same reason: a process
/// that outlived the child holding stdin's *read* end open, without reading,
/// parks this thread on a full pipe — with a multi-megabyte patch still in hand.
#[cfg(unix)]
fn feed_pipe(mut pipe: impl WritePipe, bytes: &[u8], stop: &AtomicBool) {
    let fd = pipe.as_raw_fd();
    let mut rest = bytes;
    while !rest.is_empty() && !stop.load(Ordering::Relaxed) {
        if !wait_ready(fd, libc::POLLOUT) {
            continue;
        }
        match pipe.write(&rest[..rest.len().min(DRAIN_CHUNK)]) {
            Ok(0) => return,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return, // EPIPE and friends: the child is done reading
        }
    }
}

/// `stop` is only honored between chunks here; see [`drain_pipe`].
#[cfg(not(unix))]
fn feed_pipe(mut pipe: impl WritePipe, bytes: &[u8], stop: &AtomicBool) {
    for chunk in bytes.chunks(DRAIN_CHUNK) {
        if stop.load(Ordering::Relaxed) || pipe.write_all(chunk).is_err() {
            return;
        }
    }
}

/// Wait for `fd` to be ready for `events`, up to [`DRAIN_POLL_MS`]. `false`
/// means "nothing to do yet" — a timeout, so the caller re-checks its stop flag,
/// or an error it should retry. Errors that matter surface on the following IO
/// call, which is also why the returned events go unexamined: attempting the
/// read or write is more robust than predicting it from `revents`.
#[cfg(unix)]
fn wait_ready(fd: std::os::unix::io::RawFd, events: libc::c_short) -> bool {
    let mut poll_fd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, and the caller's `pipe` keeps `fd` open
    // for the whole call.
    match unsafe { libc::poll(&mut poll_fd, 1, DRAIN_POLL_MS) } {
        0 => false,
        n if n < 0 => std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted,
        _ => true,
    }
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
    /// spawn + stdin write) — an unbounded blocking wait, which is what asking
    /// for neither guard means. With either set, it spawns the child and polls
    /// for exit while *draining both pipes on helper threads* — a full pipe
    /// would otherwise deadlock the wait — and writing any stdin on its own
    /// thread for the same reason; it kills the child's process tree on cancel
    /// ([`Error::Cancelled`]) or deadline ([`Error::TimedOut`]), reaping the
    /// child so no zombie is left behind. Collecting the drained output is
    /// itself bounded ([`DRAIN_GRACE`]), so a process that outlives the child
    /// holding a pipe open can neither hang the invocation nor strand a reader.
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
                    let feed =
                        PipeFeed::spawn(child.stdin.take().expect("stdin piped"), input.to_vec());
                    let out = child
                        .wait_with_output()
                        .map_err(|source| Error::Spawn { source })?;
                    // Release the feeder rather than joining it: the child has
                    // exited, so anything unwritten is moot, and joining would
                    // hang forever on a survivor holding stdin's read end.
                    feed.stop();
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
        let feed = input
            .map(|input| PipeFeed::spawn(child.stdin.take().expect("stdin piped"), input.to_vec()));
        let out_drain = PipeDrain::spawn(child.stdout.take().expect("stdout piped"));
        let err_drain = PipeDrain::spawn(child.stderr.take().expect("stderr piped"));

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
                // Don't wait on the pipe helpers: we discard the output, and a
                // grandchild outside the killed process group could hold a
                // pipe's far end open, which would block until *it* exits —
                // defeating the prompt cancel. Tell them to give up instead, so
                // neither the threads nor their fds outlive us.
                out_drain.stop();
                err_drain.stop();
                if let Some(feed) = &feed {
                    feed.stop();
                }
                return Err(if cancelled {
                    Error::Cancelled
                } else {
                    Error::TimedOut
                });
            }
            std::thread::sleep(poll);
            poll = (poll * 2).min(Duration::from_millis(15));
        };
        // The child has exited, so it read all the input it meant to; release
        // the feeder before waiting on anything.
        if let Some(feed) = &feed {
            feed.stop();
        }
        // One deadline for both pipes, so the bound is on the drain as a whole.
        let deadline = Instant::now() + DRAIN_GRACE;
        let stdout = out_drain.take(deadline);
        let stderr = String::from_utf8_lossy(&err_drain.take(deadline)).into_owned();
        Ok((stdout, stderr, status))
    }
}

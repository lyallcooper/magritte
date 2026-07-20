use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// One recorded command invocation, for the command log (magit's process
/// buffer). Usually the engine's own program (git, jj, …), but a user `!`
/// shell escape records its program too.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    /// The program. Recorded as `None` for the log's default program (an
    /// internal engine invocation); [`CommandLog::record`] resolves it, so
    /// snapshots always carry the concrete name.
    pub program: Option<String>,
    /// The arguments, without the engine's invocation boilerplate (`-C <dir>`,
    /// internal config pins).
    pub args: Vec<String>,
    /// The process exit code, or `None` if it was killed by a signal or failed
    /// to spawn.
    pub code: Option<i32>,
    /// Whether the command exited successfully (status 0).
    pub ok: bool,
    /// Whether the exit status was part of the caller's expected protocol.
    /// Expected non-zero predicate/query results render neutrally in the log.
    pub expected: bool,
    /// Wall-clock time spent waiting for the child process. This is deliberately
    /// measured around the full spawn/output path, so slow hooks/remotes show up
    /// in the command log alongside slow reads.
    pub elapsed: Duration,
    /// Whether the user invoked this directly (the `!` prompt), as opposed to
    /// the UI issuing it. User commands always show in the log (never hidden as
    /// a query) and keep their full output.
    pub user: bool,
    /// Captured stdout. Empty for internal invocations (whose stdout the UI
    /// consumes directly); populated for user `!` commands so the log shows
    /// their full output.
    pub stdout: String,
    /// stderr — the progress/error narrative (`Switched to branch …`, fetch
    /// progress, error messages), or a user command's. Empty for predicate
    /// calls, which discard output.
    pub stderr: String,
    /// Whether this is a read-only query the UI issues on its own, per the
    /// log's injected classifier. Queries are noise in the command log, so it
    /// hides them by default. Owned by [`CommandLog::record`], which computes
    /// it unconditionally — construct entries with `..Default::default()` and
    /// never set it by hand.
    pub query: bool,
}

impl Default for CommandEntry {
    fn default() -> CommandEntry {
        CommandEntry {
            program: None,
            args: Vec::new(),
            code: None,
            ok: false,
            expected: false,
            elapsed: Duration::ZERO,
            user: false,
            stdout: String::new(),
            stderr: String::new(),
            query: false,
        }
    }
}

impl CommandEntry {
    /// The command as a user would type it, e.g. `git fetch origin` or
    /// `ls -la`. Recorded entries always have a resolved program; an
    /// un-recorded entry with `program: None` renders its args alone.
    pub fn display(&self) -> String {
        match &self.program {
            Some(prog) => format!("{prog} {}", self.args.join(" ")),
            None => self.args.join(" "),
        }
    }

    /// Whether this entry is a hidden-by-default read-only query — see
    /// [`CommandEntry::query`].
    pub fn is_query(&self) -> bool {
        self.query
    }
}

/// A ring buffer of recent command invocations, shared (behind an `Arc`)
/// between the engine that records and the UI that renders. Parameterized by
/// the engine: `default_program` names internal invocations recorded with
/// `program: None`, and `is_query` classifies the engine's read-only queries
/// so the log view can hide them by default.
pub struct CommandLog {
    default_program: &'static str,
    is_query: fn(&CommandEntry) -> bool,
    capacity: usize,
    ring: Mutex<VecDeque<CommandEntry>>,
    /// Total commands ever recorded (monotonic, unlike the capped ring's
    /// length), so a UI can cheaply tell whether the log changed since it last
    /// flattened it.
    seq: AtomicU64,
}

impl std::fmt::Debug for CommandLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandLog")
            .field("default_program", &self.default_program)
            .field("capacity", &self.capacity)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl CommandLog {
    pub fn new(
        default_program: &'static str,
        capacity: usize,
        is_query: fn(&CommandEntry) -> bool,
    ) -> CommandLog {
        CommandLog {
            default_program,
            is_query,
            capacity,
            ring: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Record one invocation: resolve its program (the default when `None`),
    /// classify it, and push it into the ring (evicting the oldest at
    /// capacity).
    pub fn record(&self, mut entry: CommandEntry) {
        if entry.program.is_none() {
            entry.program = Some(self.default_program.to_string());
        }
        entry.query = (self.is_query)(&entry);
        if let Ok(mut q) = self.ring.lock() {
            if q.len() >= self.capacity {
                q.pop_front();
            }
            q.push_back(entry);
            self.seq.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A snapshot of the recent invocations, oldest first (for the command log
    /// view — magit's `$` process buffer).
    pub fn snapshot(&self) -> Vec<CommandEntry> {
        self.ring
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// How many commands have ever been recorded — a cheap change stamp for
    /// [`snapshot`](Self::snapshot) consumers that cache a derived view.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use magritte_vcs::{CommandLog, ProcessControl};

use crate::error::{Error, Result};

/// One recorded invocation and the raw/user output shapes — the foundation's
/// types under their git-era names, so the rest of the workspace reads
/// naturally in a git context.
pub use magritte_vcs::{CommandEntry as GitCommand, CommandRun, Output as GitOutput};

/// How many recent git invocations the command log keeps (a ring buffer).
const LOG_CAPACITY: usize = 500;

/// Distinguishes concurrent throwaway files (parallel tests, or two operations
/// at once), since the pid alone isn't unique across threads.
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A `pid-counter` suffix that makes a temp-file name unique within and across
/// processes (sequence-editor todos, throwaway index files).
pub(crate) fn unique_temp_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

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
    log: Arc<CommandLog>,
    /// When set, every invocation polls this flag and kills the child's
    /// process tree (returning [`Error::Cancelled`]) once it flips true — so a
    /// superseded or user-cancelled job stops *running*, not just gets its
    /// result dropped. Shared via the `Arc` so the caller can trigger it after
    /// handing the `Repo` to a background job. `None` means uncancellable (the
    /// fast `.output()` path).
    cancel: Option<Arc<AtomicBool>>,
    /// When set, an invocation exceeding this kills the child and returns
    /// [`Error::TimedOut`] — a backstop against a wedged remote/hook.
    timeout: Option<Duration>,
    /// Diff context lines (`-U<n>`) for content diffs; `None` uses git's
    /// default of 3. The UI's `+`/`-`/`0` context keys set it.
    pub(crate) diff_context: Option<usize>,
}

/// A tag name paired with the number of commits between it and HEAD. See
/// [`Repo::nearest_tag`].
pub type TagDistance = (String, usize);

/// Whether an entry is a read-only query the UI issues on its own — the status
/// refresh, diffs, and ref lookups — rather than something the user invoked.
/// These are noise in the command log, so it hides them by default. Injected
/// into the shared [`CommandLog`] as its classifier.
fn git_is_query(cmd: &GitCommand) -> bool {
    if cmd.user {
        return false;
    }
    match cmd.args.first().map(String::as_str) {
        Some(
            "status" | "diff" | "rev-parse" | "rev-list" | "for-each-ref" | "show-ref" | "ls-files"
            | "symbolic-ref" | "describe" | "log" | "merge-base" | "blame" | "check-ignore",
        ) => true,
        // Config *reads* (e.g. resolving the push-remote) are queries; a
        // config write (setting one) is a user action, so keep it visible.
        Some("config") => cmd.args.iter().any(|a| a == "--get" || a == "--get-all"),
        // `git stash list` is the Stashes section's listing — a query; the
        // mutating stash verbs (push/pop/apply/drop/show) are user actions,
        // so they stay visible.
        Some("stash") => cmd.args.get(1).map(String::as_str) == Some("list"),
        // A bare `git remote` is the remote-name listing the pickers issue;
        // the mutating verbs (add/rename/remove/prune) stay visible.
        Some("remote") => cmd.args.len() == 1,
        // `git worktree list` is the worktree browser's listing; the
        // mutating verbs (add/remove/move/prune) stay visible.
        Some("worktree") => cmd.args.get(1).map(String::as_str) == Some("list"),
        // `git tag -n`/`--list` is the release listing; creating or
        // deleting a tag stays visible.
        Some("tag") => cmd
            .args
            .iter()
            .any(|a| a == "-n" || a.starts_with("-n") || a == "--list" || a == "-l"),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum ExitExpectation {
    Success,
    SuccessOrOne,
    Any,
}

impl ExitExpectation {
    fn accepts(self, code: Option<i32>) -> bool {
        match self {
            Self::Success => code == Some(0),
            Self::SuccessOrOne => matches!(code, Some(0 | 1)),
            Self::Any => code.is_some(),
        }
    }
}

/// Assemble a git argv from fixed leading words, a transient's toggled switch
/// arguments, and trailing operands — the `git <verb> [switches] <operands>`
/// shape every command wrapper shares.
pub(crate) fn git_args(lead: &[&str], switches: &[String], trail: &[&str]) -> Vec<String> {
    lead.iter()
        .map(|s| s.to_string())
        .chain(switches.iter().cloned())
        .chain(trail.iter().map(|s| s.to_string()))
        .collect()
}

/// Configure a child for spawning from our worker threads: the foundation's
/// spawn preparation (signal-mask reset, own process group — see
/// [`magritte_vcs::prepare_spawn`]) plus **`GIT_TERMINAL_PROMPT=0`** — no
/// terminal here, so git must not block on a credential prompt; inherited by
/// an intermediate `sh` and its git children, so the `!` prompt's and
/// `[[command]]` shell commands get it too (it matters once any of them
/// invokes a networked git).
fn prepare_spawn(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    magritte_vcs::prepare_spawn(cmd);
}

/// A `git` command rooted at `cwd`, with the config pins and spawn environment
/// every internal invocation shares:
///
/// - `core.quotepath=false` keeps output stable and machine-readable
///   regardless of user config.
/// - `GIT_OPTIONAL_LOCKS=0` skips the *optional* index lock: read-only
///   commands like `status`/`diff` otherwise grab `.git/index.lock` to write
///   back a refreshed stat cache, and we cancel (SIGKILL) superseded reads on
///   every overlapping refresh — which would orphan that lock. Commands that
///   *require* the lock (commit, add, …) are unaffected.
fn git_at(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["-c", "core.quotepath=false"])
        .env("GIT_OPTIONAL_LOCKS", "0");
    prepare_spawn(&mut cmd);
    cmd
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
            log: Arc::new(CommandLog::new("git", LOG_CAPACITY, git_is_query)),
            cancel: None,
            timeout: None,
            diff_context: None,
        })
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

    /// A clone whose content diffs use `n` context lines (`-U<n>`) — the UI's
    /// adjustable diff context.
    pub fn with_diff_context(&self, n: usize) -> Repo {
        let mut repo = self.clone();
        repo.diff_context = Some(n);
        repo
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// A snapshot of the recent git invocations, oldest first (for the command
    /// log view — magit's `$` process buffer).
    pub fn command_log(&self) -> Vec<GitCommand> {
        self.log.snapshot()
    }

    /// How many commands have ever been recorded — a cheap change stamp for
    /// [`command_log`](Self::command_log) consumers that cache a derived view.
    pub fn command_log_seq(&self) -> u64 {
        self.log.seq()
    }

    /// Record an internal git call (the UI's own invocations): a `git` command,
    /// not user-invoked, with stdout consumed by the caller rather than stored.
    fn record_git(
        &self,
        args: &[String],
        code: Option<i32>,
        expected: bool,
        stderr: &str,
        elapsed: Duration,
    ) {
        self.log.record(GitCommand {
            program: None,
            args: args.to_vec(),
            code,
            ok: code == Some(0),
            expected,
            elapsed,
            stderr: stderr.to_string(),
            ..Default::default()
        });
    }

    /// A `git` command rooted at the working tree, with the spawn environment
    /// git needs under our worker threads (see [`prepare_spawn`]).
    fn git(&self) -> Command {
        git_at(&self.workdir)
    }

    /// Execute one internal git invocation — the shared front half of every
    /// `run*` variant: collect the argv, spawn through [`git`](Self::git) (with
    /// an optional extra env var and stdin), time it, and record it in the
    /// command log. The caller applies its own exit-status policy.
    fn execute<I, S>(
        &self,
        args: I,
        env: Option<(&str, &str)>,
        input: Option<&[u8]>,
        expectation: ExitExpectation,
    ) -> Result<(Vec<String>, GitOutput, ExitStatus)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arg_vec: Vec<String> = args
            .into_iter()
            .map(|s| s.as_ref().to_string_lossy().into_owned())
            .collect();
        let mut cmd = self.git();
        if let Some((key, value)) = env {
            cmd.env(key, value);
        }
        cmd.args(&arg_vec);
        let start = Instant::now();
        let (stdout, stderr, status) = self.collect_output_with(cmd, input)?;
        self.record_git(
            &arg_vec,
            status.code(),
            expectation.accepts(status.code()),
            &stderr,
            start.elapsed(),
        );
        Ok((arg_vec, GitOutput { stdout, stderr }, status))
    }

    /// Map a non-zero exit to [`Error::Git`] — the policy of the erroring
    /// `run*` variants.
    fn checked(args: Vec<String>, out: GitOutput, status: ExitStatus) -> Result<GitOutput> {
        if !status.success() {
            return Err(Error::Git {
                args,
                status: status.code(),
                stderr: out.stderr,
            });
        }
        Ok(out)
    }

    /// Run `git <args>` in the working tree, returning stdout as raw bytes so
    /// that NUL-delimited (`-z`) output is preserved. Honors this repo's cancel
    /// flag and timeout (if set) — see [`cancellable`](Self::cancellable).
    pub fn run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, out, status) = self.execute(args, None, None, ExitExpectation::Success)?;
        Self::checked(args, out, status)
    }

    /// Run a user-typed command from the `!` prompt — git by default, or an
    /// arbitrary `program` (its shell escape) run in the working tree. Unlike
    /// [`run`](Self::run), a non-zero exit is *not* an error: the output is the
    /// point, so it's returned either way. The full output is recorded in the
    /// command log (`user`-flagged, so it's always shown there).
    pub fn run_user(&self, program: Option<&str>, args: &[String]) -> Result<CommandRun> {
        self.run_user_in(program, args, Path::new(""))
    }

    /// Like [`run_user`](Self::run_user), but run in `dir` (worktree-relative)
    /// instead of the repository root — magit's run-in-working-directory
    /// variants.
    pub fn run_user_in(
        &self,
        program: Option<&str>,
        args: &[String],
        dir: &Path,
    ) -> Result<CommandRun> {
        let cwd = self.workdir.join(dir);
        let cmd = match program {
            None => {
                // Rooted at `cwd` (not the toplevel) so path-relative
                // subcommands (`git add .`) resolve where the user expects.
                let mut c = git_at(&cwd);
                c.args(args);
                c
            }
            Some(p) => {
                let mut c = Command::new(p);
                c.current_dir(&cwd).args(args);
                prepare_spawn(&mut c);
                c
            }
        };
        let start = Instant::now();
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        let elapsed = start.elapsed();
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        self.log.record(GitCommand {
            program: program.map(String::from),
            args: args.to_vec(),
            code: status.code(),
            ok: status.success(),
            expected: status.success(),
            elapsed,
            user: true,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            ..Default::default()
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
        self.run_shell_in(command, Path::new(""))
    }

    /// Like [`run_shell`](Self::run_shell), but run in `dir` (worktree-relative).
    pub fn run_shell_in(&self, command: &str, dir: &Path) -> Result<CommandRun> {
        let mut cmd = Command::new("sh");
        cmd.current_dir(self.workdir.join(dir))
            .arg("-c")
            .arg(command);
        prepare_spawn(&mut cmd);
        let start = Instant::now();
        let (stdout, stderr, status) = self.collect_output(cmd)?;
        let elapsed = start.elapsed();
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        // For the log: show the command as written. The first word reads as the
        // "program" (dim) and the rest as its arguments, like a git line.
        let mut words = command.split_whitespace().map(String::from);
        self.log.record(GitCommand {
            program: words.next(),
            args: words.collect(),
            code: status.code(),
            ok: status.success(),
            expected: status.success(),
            elapsed,
            user: true,
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            ..Default::default()
        });
        Ok(CommandRun {
            ok: status.success(),
            stdout,
            stderr,
        })
    }

    /// Run `cmd` to completion via the foundation runner, under this repo's
    /// cancel flag and timeout — see [`ProcessControl::collect_output_with`].
    /// Routing every variant (incl. `run_with_env`, `run_with_input`) through
    /// here is what makes them all honor the cancel flag and timeout.
    fn collect_output_with(
        &self,
        cmd: Command,
        input: Option<&[u8]>,
    ) -> Result<(Vec<u8>, String, ExitStatus)> {
        ProcessControl {
            cancel: self.cancel.clone(),
            timeout: self.timeout,
        }
        .collect_output_with(cmd, input)
        .map_err(Error::from)
    }

    /// Run `cmd` with no stdin — the common case.
    fn collect_output(&self, cmd: Command) -> Result<(Vec<u8>, String, ExitStatus)> {
        self.collect_output_with(cmd, None)
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
        let (args, out, status) =
            self.execute(args, Some((key, value)), None, ExitExpectation::Success)?;
        Self::checked(args, out, status)
    }

    /// Like [`run`](Self::run) but feeds `input` to git's stdin. Used to pipe
    /// patches to `git apply`. The stdin path honors the cancel flag and
    /// timeout like every other variant (a wedged hook reading the patch can't
    /// hang forever, and C-g/Esc kills it).
    pub fn run_with_input<I, S>(&self, args: I, input: &[u8]) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, out, status) =
            self.execute(args, None, Some(input), ExitExpectation::Success)?;
        Self::checked(args, out, status)
    }

    /// Run a git query with stdin whose exit status is part of its protocol.
    ///
    /// This keeps protocol-style commands on the shared execution path (so
    /// cancellation, timeouts, `GIT_OPTIONAL_LOCKS=0`, and command logging all
    /// still apply) while letting the caller distinguish expected non-zero
    /// statuses from real failures.
    pub(crate) fn run_with_input_status<I, S>(
        &self,
        args: I,
        input: &[u8],
    ) -> Result<(Vec<String>, GitOutput, ExitStatus)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.execute(args, None, Some(input), ExitExpectation::SuccessOrOne)
    }

    /// Run `git <args>` where git would normally open the **sequence editor**
    /// (`rebase -i`, etc.), feeding it `todo` non-interactively. A throwaway
    /// `sequence.editor` copies `todo` over git's generated todo, and
    /// `GIT_EDITOR` is neutralized (`true`) so any message-bearing steps use
    /// their prepared message instead of blocking on an editor. The temp file is
    /// removed regardless of outcome. This isolates the no-TTY plumbing from the
    /// callers' domain logic (which just builds the todo + argv).
    pub fn run_with_sequence_editor(&self, todo: &str, args: &[String]) -> Result<GitOutput> {
        // A unique temp file holds the todo; pid+counter keeps concurrent runs
        // (and parallel tests) from sharing one file.
        let path = std::env::temp_dir().join(format!("magritte-seq-todo-{}", unique_temp_suffix()));
        std::fs::write(&path, todo)
            .map_err(|e| Error::Message(format!("{}: {e}", path.display())))?;

        // git runs sequence.editor through the shell, so single-quote the path
        // (escaping any quote inside it) rather than trusting temp_dir to be
        // shell-clean.
        let quoted = path.display().to_string().replace('\'', "'\\''");
        let mut argv = vec!["-c".to_string(), format!("sequence.editor=cp '{quoted}'")];
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
        let (_args, _out, status) = self.execute(args, None, None, ExitExpectation::Any)?;
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
        let (_args, out, status) = self.execute(args, None, None, ExitExpectation::Any)?;
        Ok(status.success().then_some(out))
    }

    /// Read a single git config value (`git config --get <key>`), `None` if
    /// unset.
    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .run_optional(["config", "--get", key])?
            .and_then(|o| o.text_opt()))
    }

    /// Whether `rev` resolves to an object (`git rev-parse --verify --quiet`).
    pub fn rev_exists(&self, rev: &str) -> bool {
        self.succeeds(["rev-parse", "--verify", "--quiet", rev])
            .unwrap_or(false)
    }

    /// Whether a local branch named `name` exists (`git show-ref --verify`).
    pub fn branch_exists(&self, name: &str) -> bool {
        let r = format!("refs/heads/{name}");
        self.succeeds(["show-ref", "--verify", "--quiet", r.as_str()])
            .unwrap_or(false)
    }

    /// Read a boolean git config value (`git config --type=bool --get <key>`),
    /// canonicalized by git to `true`/`false`. `false` if unset or unreadable.
    pub fn config_bool(&self, key: &str) -> bool {
        self.run_optional(["config", "--type=bool", "--get", key])
            .ok()
            .flatten()
            .is_some_and(|o| o.stdout_text() == "true")
    }

    /// Set a git config value in the repository (local) config
    /// (`git config <key> <value>`).
    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        self.run(["config", key, value]).map(|_| ())
    }

    /// Remove a git config key from the repository (local) config
    /// (`git config --unset <key>`). Unsetting an already-absent key is a no-op
    /// (git exits 5), not an error.
    pub fn config_unset(&self, key: &str) -> Result<()> {
        self.run_optional(["config", "--unset", key]).map(|_| ())
    }

    /// The nearest tag reachable from HEAD, with the commits since it
    /// (`git describe --long --tags`) — magit's `magit-get-current-tag`;
    /// `None` if untagged. (The "next" tag *containing* HEAD is deliberately
    /// not surfaced: with an upstream ahead of HEAD it reports tags on commits
    /// you haven't even pulled, which reads as noise in the title bar.)
    pub fn nearest_tag(&self) -> Option<TagDistance> {
        let out = self
            .run_optional(["describe", "--long", "--tags"])
            .ok()
            .flatten()?;
        let s = out.stdout_text();
        // "<tag>-<count>-g<hash>": strip the "-g<hash>", then split the count.
        let without_hash = s.rsplit_once("-g")?.0;
        let (tag, count) = without_hash.rsplit_once('-')?;
        Some((tag.to_string(), count.parse().ok()?))
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
    /// state keyed off it lands in one place for the whole repo.
    pub fn git_common_dir(&self) -> Result<PathBuf> {
        let out = self.run(["rev-parse", "--git-common-dir"])?;
        let raw = out.stdout_text();
        if raw.is_empty() {
            return Err(Error::Message("git reported no common dir".to_string()));
        }
        // git reports it relative to the working tree we ran in (`-C workdir`).
        let dir = PathBuf::from(&raw);
        Ok(if dir.is_absolute() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(args: &[&str], user: bool) -> GitCommand {
        GitCommand {
            program: None,
            args: args.iter().map(|s| s.to_string()).collect(),
            code: Some(0),
            ok: true,
            expected: true,
            elapsed: Duration::from_millis(1),
            user,
            ..Default::default()
        }
    }

    #[test]
    fn is_query_hides_ui_listings_and_reads() {
        // Read-only listings/reads the UI issues for its own sections — noise in
        // the command log, hidden by default.
        for args in [
            &["status", "--porcelain=v2"][..],
            &["log", "@{upstream}..HEAD"][..], // unpushed/unpulled/recent/log view
            &["stash", "list", "--format=%gd"][..], // the Stashes section
            &["diff", "--cached"][..],
            &["describe", "--tags"][..],
            &["config", "--get", "remote.pushDefault"][..],
        ] {
            assert!(git_is_query(&cmd(args, false)), "expected query: {args:?}");
        }
    }

    #[test]
    fn is_query_keeps_user_and_mutations_visible() {
        // A user-typed command always shows, even a read-only one.
        assert!(!git_is_query(&cmd(&["log"], true)));
        assert!(!git_is_query(&cmd(&["stash", "list"], true)));
        // Mutating stash verbs and config writes are user actions, not queries.
        assert!(!git_is_query(&cmd(&["stash", "push", "-m", "wip"], false)));
        assert!(!git_is_query(&cmd(&["stash", "pop"], false)));
        assert!(!git_is_query(&cmd(
            &["config", "remote.pushDefault", "origin"],
            false
        )));
        assert!(!git_is_query(&cmd(&["commit", "-m", "x"], false)));
        assert!(!git_is_query(&cmd(&["push"], false)));
    }
}

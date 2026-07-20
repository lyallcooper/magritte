/// The raw result of a successful VCS invocation. stdout is kept as bytes so
/// NUL-delimited (`-z`) output and non-UTF-8 file content survive; stderr is
/// the human narrative, decoded lossily.
#[derive(Debug)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    /// Trimmed stdout as text (lossy UTF-8) — the shape of every single-value
    /// query (a ref name, a config value, a count).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// Trimmed stdout, or `None` when empty — the shape of every optional
    /// single-value query (an unset config key, a branch with no upstream).
    pub fn text_opt(&self) -> Option<String> {
        let s = self.stdout_text();
        (!s.is_empty()).then_some(s)
    }

    /// stdout as trimmed, non-empty lines — the shape of every name-listing
    /// query (branches, tags, remotes).
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
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

/// The result of a user-invoked command (the `!` prompt, `[[command]]`s): its
/// text output and whether it succeeded. Unlike [`Output`], a non-zero exit
/// isn't an error here — the output is the point either way.
#[derive(Debug)]
pub struct CommandRun {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

//! Driving `git bisect` — the binary search for a commit that introduced a
//! change. Mirrors magit's bisect surfacing (`.reference/magit/lisp/
//! magit-bisect.el`): an in-progress bisect is detected from `BISECT_LOG` under
//! the git dir, and each step marks the checked-out commit good/bad/skip until
//! the culprit is found; `reset` ends the session and restores the branch.

use std::path::Path;

use crate::error::Result;
use crate::repo::Repo;

/// A snapshot of an in-progress bisect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bisect {
    /// The recorded decisions so far (`git bisect` command lines from
    /// `BISECT_LOG`, minus the leading "git " and the comment lines) — a compact
    /// view of where the search stands, oldest first.
    pub decisions: Vec<String>,
}

impl Repo {
    /// The in-progress bisect, if any. Detected like magit's
    /// `magit-bisect-in-progress-p`: the presence of `BISECT_LOG`.
    pub fn bisect(&self) -> Option<Bisect> {
        let dir = self.git_dir().ok()?;
        self.bisect_in_dir(&dir)
    }

    /// Like [`bisect`](Self::bisect), but reuses a git-dir path already resolved
    /// by a broader refresh snapshot.
    pub(crate) fn bisect_in_dir(&self, dir: &Path) -> Option<Bisect> {
        let log = std::fs::read_to_string(dir.join("BISECT_LOG")).ok()?;
        // Summarize the log: the `# good:`/`# bad:`/`# skip:`/`# first bad
        // commit:` markers (which carry the commit subject) and the `git bisect
        // <verb> …` decision lines, in order — dropping the transient `# status:`
        // waiting-for lines and the `git ` / `# ` noise prefixes.
        let decisions = log
            .lines()
            .filter_map(|l| {
                if let Some(rest) = l.strip_prefix("# ") {
                    (!rest.starts_with("status:")).then(|| rest.to_string())
                } else {
                    l.strip_prefix("git ").map(str::to_string)
                }
            })
            .collect();
        Some(Bisect { decisions })
    }

    /// Start a bisect between a known-bad and a known-good revision (`git bisect
    /// start <bad> <good>`); git checks out the midpoint to test.
    pub fn bisect_start(&self, bad: &str, good: &str) -> Result<String> {
        let out = self.run(["bisect", "start", bad, good])?;
        Ok(out.status_line())
    }

    /// Mark the checked-out bisect commit — `good`, `bad`, or `skip` — and let
    /// git advance to the next midpoint (or announce the first bad commit).
    pub fn bisect_mark(&self, verb: BisectMark) -> Result<String> {
        let out = self.run(["bisect", verb.verb()])?;
        Ok(out.status_line())
    }

    /// End the bisect session, restoring the original branch/HEAD.
    pub fn bisect_reset(&self) -> Result<String> {
        let out = self.run(["bisect", "reset"])?;
        Ok(out.status_line())
    }
}

/// A verdict on the checked-out bisect commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BisectMark {
    Good,
    Bad,
    Skip,
}

impl BisectMark {
    fn verb(self) -> &'static str {
        match self {
            BisectMark::Good => "good",
            BisectMark::Bad => "bad",
            BisectMark::Skip => "skip",
        }
    }
}

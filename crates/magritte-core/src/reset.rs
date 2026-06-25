//! `git reset` to a commit, in magit's modes (see `magit-reset.el`). Moving
//! HEAD with `--hard` discards working-tree changes, so the frontend confirms
//! that one.

use crate::error::Result;
use crate::repo::Repo;

/// How far a reset reaches: which of HEAD, the index, and the working tree it
/// rewinds to the target commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// HEAD only — leaves the index and working tree (changes become staged).
    Soft,
    /// HEAD and index — leaves the working tree (the default).
    Mixed,
    /// HEAD, index, and working tree — discards uncommitted changes.
    Hard,
    /// Like hard, but refuses to clobber uncommitted changes it can't preserve.
    Keep,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
        }
    }

    /// Whether this mode can throw away uncommitted work (so the UI confirms).
    pub fn is_destructive(self) -> bool {
        matches!(self, ResetMode::Hard)
    }
}

impl Repo {
    /// `git reset <mode> <target>` — move the current branch to `target`.
    pub fn reset(&self, mode: ResetMode, target: &str) -> Result<String> {
        let out = self.run(["reset", mode.flag(), target])?;
        // reset narrates on stdout ("Unstaged changes after reset:"); fall back
        // to stderr.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let summary = stdout.trim();
        Ok(if summary.is_empty() {
            out.stderr.trim().to_string()
        } else {
            summary.lines().next().unwrap_or("").to_string()
        })
    }
}

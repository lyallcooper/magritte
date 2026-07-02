//! `git reset` to a commit, in magit's modes (see `magit-reset.el`). Moving
//! HEAD with `--hard` discards working-tree changes, so the frontend confirms
//! that one.

use crate::error::Result;
use crate::repo::{unique_temp_suffix, Repo};

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
    /// Index only — leaves HEAD and the working tree (magit's reset-index).
    Index,
    /// Working tree only — leaves HEAD and the index (magit's reset-worktree).
    /// Overwrites uncommitted changes.
    Worktree,
}

impl ResetMode {
    /// Whether this mode can throw away uncommitted work (so the UI confirms).
    pub fn is_destructive(self) -> bool {
        matches!(self, ResetMode::Hard | ResetMode::Worktree)
    }
}

impl Repo {
    /// `git reset <mode> <target>` — move the current branch to `target`. The
    /// index- and worktree-only modes don't move HEAD; they delegate to
    /// [`reset_index`](Self::reset_index) / [`reset_worktree`](Self::reset_worktree).
    pub fn reset(&self, mode: ResetMode, target: &str) -> Result<String> {
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
            ResetMode::Index => return self.reset_index(target),
            ResetMode::Worktree => return self.reset_worktree(target),
        };
        // reset narrates on stdout ("Unstaged changes after reset:"), falling
        // back to stderr — exactly `GitOutput::first_line`.
        Ok(self.run(["reset", flag, target])?.first_line())
    }

    /// Reset only the index to `target`, leaving HEAD and the working tree
    /// (magit's `magit-reset-index`): `git reset <target> -- .`.
    pub fn reset_index(&self, target: &str) -> Result<String> {
        self.run(["reset", "-q", target, "--", "."])?;
        Ok(format!("Reset index to {target}"))
    }

    /// Reset only the working tree to `target`, leaving HEAD and the index
    /// (magit's `magit-reset-worktree`): read the target's tree into a throwaway
    /// index, then check that out over the working tree. Overwrites uncommitted
    /// changes, so the frontend confirms it like a hard reset.
    pub fn reset_worktree(&self, target: &str) -> Result<String> {
        let tmp = self
            .git_dir()?
            .join(format!("magritte-reset-worktree-index-{}", unique_temp_suffix()));
        let tmp_str = tmp.to_string_lossy().into_owned();
        // Populate the temp index from the target's tree, then write those files
        // to the working tree from it — neither touches the real index or HEAD.
        let result = self
            .run_with_env(["read-tree", target], "GIT_INDEX_FILE", &tmp_str)
            .and_then(|_| {
                self.run_with_env(
                    ["checkout-index", "--all", "--force"],
                    "GIT_INDEX_FILE",
                    &tmp_str,
                )
            });
        let _ = std::fs::remove_file(&tmp);
        result?;
        Ok(format!("Reset worktree to {target}"))
    }
}

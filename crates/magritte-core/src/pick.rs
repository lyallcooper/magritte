//! Cherry-pick and revert a commit (see `magit-sequence.el`). Both can pause on
//! a conflict, after which they're driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::Result;
use crate::repo::{git_args, Repo};

impl Repo {
    /// `git cherry-pick [args] <rev>` — apply `rev`'s change onto HEAD, keeping
    /// its original message (no editor). `args` carries the transient switches.
    pub fn cherry_pick_with_args(&self, rev: &str, args: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["cherry-pick"], args, &[rev]))?
            .status_line())
    }

    /// Apply a commit's changes without committing (`git cherry-pick --no-commit`).
    pub fn cherry_apply_with_args(&self, rev: &str, args: &[String]) -> Result<String> {
        // `--ff` contradicts `--no-commit`; drop it if toggled.
        let switches: Vec<String> = args
            .iter()
            .filter(|a| a.as_str() != "--ff")
            .cloned()
            .collect();
        Ok(self
            .run(git_args(&["cherry-pick", "--no-commit"], &switches, &[rev]))?
            .status_line())
    }

    /// `git revert [args] <rev>` — commit the inverse of `rev`. Pass
    /// `--no-edit` to take the default "Revert …" message without an editor.
    pub fn revert_with_args(&self, rev: &str, args: &[String]) -> Result<String> {
        Ok(self.run(git_args(&["revert"], args, &[rev]))?.status_line())
    }

    /// Apply a commit's inverse without committing (`git revert --no-commit`).
    pub fn revert_no_commit_with_args(&self, rev: &str, args: &[String]) -> Result<String> {
        Ok(self
            .run(git_args(&["revert", "--no-commit"], args, &[rev]))?
            .status_line())
    }
}

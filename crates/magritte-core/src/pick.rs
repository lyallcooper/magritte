//! Cherry-pick and revert a commit (see `magit-sequence.el`). Both can pause on
//! a conflict, after which they're driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::Result;
use crate::repo::{git_args, Repo};

impl Repo {
    /// `git cherry-pick <rev>` — apply `rev`'s change onto HEAD, keeping its
    /// original message (no editor).
    pub fn cherry_pick(&self, rev: &str) -> Result<String> {
        self.cherry_pick_with_args(rev, &[])
    }

    /// `git cherry-pick [args] <rev>` — the transient-driven form.
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

    /// `git revert --no-edit <rev>` — commit the inverse of `rev`, taking the
    /// default "Revert …" message (no editor).
    pub fn revert(&self, rev: &str) -> Result<String> {
        self.revert_with_args(rev, &["--no-edit".to_string()])
    }

    /// `git revert [args] <rev>` — the transient-driven form.
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

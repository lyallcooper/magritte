//! Cherry-pick and revert a commit (see `magit-sequence.el`). Both can pause on
//! a conflict, after which they're driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// `git cherry-pick <rev>` — apply `rev`'s change onto HEAD, keeping its
    /// original message (no editor).
    pub fn cherry_pick(&self, rev: &str) -> Result<String> {
        let out = self.run(["cherry-pick", rev])?;
        Ok(out.status_line())
    }

    /// `git revert --no-edit <rev>` — commit the inverse of `rev`, taking the
    /// default "Revert …" message (no editor).
    pub fn revert(&self, rev: &str) -> Result<String> {
        let out = self.run(["revert", "--no-edit", rev])?;
        Ok(out.status_line())
    }
}

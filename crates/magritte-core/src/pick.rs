//! Cherry-pick and revert a commit (see `magit-sequence.el`). Both can pause on
//! a conflict, after which they're driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::Result;
use crate::repo::Repo;

fn summary(stdout: &[u8], stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        stderr.lines().next_back().unwrap_or("").to_string()
    }
}

impl Repo {
    /// `git cherry-pick <rev>` — apply `rev`'s change onto HEAD, keeping its
    /// original message (no editor).
    pub fn cherry_pick(&self, rev: &str) -> Result<String> {
        let out = self.run(["cherry-pick", rev])?;
        Ok(summary(&out.stdout, &out.stderr))
    }

    /// `git revert --no-edit <rev>` — commit the inverse of `rev`, taking the
    /// default "Revert …" message (no editor).
    pub fn revert(&self, rev: &str) -> Result<String> {
        let out = self.run(["revert", "--no-edit", rev])?;
        Ok(summary(&out.stdout, &out.stderr))
    }
}

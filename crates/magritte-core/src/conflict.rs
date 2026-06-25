//! Resolving a merge conflict by taking one side wholesale (magit's
//! `magit-checkout-stage`): check out our or their version of the file, then
//! stage it to mark the path resolved.

use crate::error::Result;
use crate::repo::Repo;

/// Which side of a conflict to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    /// HEAD's version (`--ours`).
    Ours,
    /// The incoming version (`--theirs`).
    Theirs,
}

impl Repo {
    /// Resolve `path` by keeping one side: `git checkout --ours|--theirs -- path`
    /// then `git add -- path` to mark it resolved.
    pub fn resolve_conflict(&self, path: &str, side: ConflictSide) -> Result<()> {
        let flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
        };
        self.run(["checkout", flag, "--", path])?;
        self.run(["add", "--", path])?;
        Ok(())
    }
}

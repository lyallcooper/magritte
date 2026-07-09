//! `git merge` (see `magit-merge.el`). We merge non-interactively: a merge
//! commit takes git's default message (`--no-edit`). A conflicting merge exits
//! non-zero and leaves `MERGE_HEAD`, which the frontend surfaces as the
//! in-progress sequence (resolve, then commit) — see [`crate::sequence`].

use crate::error::Result;
use crate::repo::{git_args, Repo};

impl Repo {
    /// `git merge --no-edit [args] <target>`. `args` carries the toggled
    /// switches (`--no-ff`, `--ff-only`) and the action's mode (`--squash` /
    /// `--no-commit`).
    pub fn merge(&self, target: &str, args: &[String]) -> Result<String> {
        // merge reports on stderr ("Merge made by…", conflict notices), falling
        // back to stdout — exactly `GitOutput::status_line`.
        Ok(self
            .run(git_args(&["merge", "--no-edit"], args, &[target]))?
            .status_line())
    }

    /// The commit message git prepared for the merge in progress
    /// (`.git/MERGE_MSG`), with its `#` comment lines (e.g. the Conflicts
    /// block) stripped like an editor-cleanup commit would. `None` when no
    /// merge is in progress (or the message is empty).
    pub fn merge_msg(&self) -> Result<Option<String>> {
        let path = self.git_dir()?.join("MERGE_MSG");
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let msg = raw
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string();
        Ok((!msg.is_empty()).then_some(msg))
    }
}

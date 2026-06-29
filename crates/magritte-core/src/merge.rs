//! `git merge` (see `magit-merge.el`). We merge non-interactively: a merge
//! commit takes git's default message (`--no-edit`). A conflicting merge exits
//! non-zero and leaves `MERGE_HEAD`, which the frontend surfaces as the
//! in-progress sequence (resolve, then commit) — see [`crate::sequence`].

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// `git merge --no-edit [args] <target>`. `args` carries the toggled
    /// switches (`--no-ff`, `--ff-only`) and the action's mode (`--squash` /
    /// `--no-commit`).
    pub fn merge(&self, target: &str, args: &[String]) -> Result<String> {
        let mut argv = vec!["merge".to_string(), "--no-edit".to_string()];
        argv.extend(args.iter().cloned());
        argv.push(target.to_string());
        // merge reports on stderr ("Merge made by…", conflict notices), falling
        // back to stdout — exactly `GitOutput::status_line`.
        Ok(self.run(&argv)?.status_line())
    }
}

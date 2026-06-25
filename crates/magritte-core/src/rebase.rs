//! Starting a `git rebase` (see `magit-sequence.el`). Once a rebase pauses on a
//! conflict, it's driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// `git rebase [args] <onto>` — replay the current branch onto `onto`.
    /// `args` carries the toggled switches (`--autostash`, `--autosquash`). A
    /// conflict exits non-zero and leaves the rebase paused, which the frontend
    /// surfaces as the in-progress sequence.
    pub fn rebase(&self, onto: &str, args: &[String]) -> Result<String> {
        let mut argv = vec!["rebase".to_string()];
        argv.extend(args.iter().cloned());
        argv.push(onto.to_string());
        let out = self.run(&argv)?;
        let stderr = out.stderr.trim();
        Ok(if stderr.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            stderr.lines().next_back().unwrap_or("").to_string()
        })
    }
}

//! Branch operations — the `b` branch transient's commands (checkout, create,
//! rename, delete), mirroring magit's `magit-branch`.

use crate::error::Result;
use crate::remote::summary;
use crate::repo::Repo;

impl Repo {
    /// Local branch names (`refs/heads`), most-recently-committed first so the
    /// branches you're likely to want are near the top of the picker.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        let out = self.run([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads/",
        ])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Check out `target`, DWIM-creating a local tracking branch when a
    /// remote-only branch is chosen (matching magit's friendly checkout): a
    /// local branch is switched to directly; a `remote/branch` ref for a known
    /// remote checks out the short name so git auto-creates the tracking branch;
    /// anything else (a tag or commit) is checked out verbatim (detaching HEAD).
    pub fn checkout(&self, target: &str) -> Result<String> {
        let arg = self.checkout_arg(target)?;
        Ok(summary(self.run(["checkout", &arg])?))
    }

    fn checkout_arg(&self, target: &str) -> Result<String> {
        // An existing local branch: switch to it as-is (covers slashy names like
        // `feature/x` that would otherwise look like a remote ref).
        if self.succeeds([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{target}"),
        ])? {
            return Ok(target.to_string());
        }
        // A `remote/branch` ref for a configured remote: check out the short
        // name so git sets up tracking (`git checkout foo` ← `origin/foo`).
        if let Some((remote, rest)) = target.split_once('/') {
            if !rest.is_empty() && self.remotes()?.iter().any(|r| r == remote) {
                return Ok(rest.to_string());
            }
        }
        Ok(target.to_string())
    }

    /// `git branch <name> [start]` — create a branch (without checking it out).
    pub fn create_branch(&self, name: &str, start: Option<&str>) -> Result<String> {
        let mut args = vec!["branch".to_string(), name.to_string()];
        args.extend(start.map(str::to_string));
        Ok(summary(self.run(&args)?))
    }

    /// `git checkout -b <name> [start]` — create a branch and check it out.
    pub fn create_and_checkout(&self, name: &str, start: Option<&str>) -> Result<String> {
        let mut args = vec!["checkout".to_string(), "-b".to_string(), name.to_string()];
        args.extend(start.map(str::to_string));
        Ok(summary(self.run(&args)?))
    }

    /// `git branch -m <old> <new>` — rename a branch.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<String> {
        Ok(summary(self.run(["branch", "-m", old, new])?))
    }

    /// `git branch -d/-D <name>` — delete a branch (`-D` to force, for an
    /// unmerged branch).
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<String> {
        let flag = if force { "-D" } else { "-d" };
        Ok(summary(self.run(["branch", flag, name])?))
    }
}

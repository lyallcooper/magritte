//! Branch operations — the `b` branch transient's commands (checkout, create,
//! rename, delete), mirroring magit's `magit-branch`.

use crate::error::Result;
use crate::repo::Repo;

/// A local branch with its divergence from its upstream (0/0 when there's no
/// upstream or it's in sync) — for the refs browser's ahead/behind margin.
#[derive(Debug, Clone)]
pub struct LocalBranch {
    pub name: String,
    pub ahead: u32,
    pub behind: u32,
}

impl Repo {
    /// The current branch name, or `None` when HEAD is detached. Uses
    /// `symbolic-ref` rather than `rev-parse --abbrev-ref` so an unborn branch
    /// (fresh repo, no commits) still resolves to its name instead of erroring.
    pub fn current_branch(&self) -> Result<Option<String>> {
        // `-q` exits 1 silently on a detached HEAD; run_optional maps that to
        // None rather than an error.
        Ok(self
            .run_optional(["symbolic-ref", "--short", "-q", "HEAD"])?
            .map(|out| out.stdout_text())
            .filter(|name| !name.is_empty()))
    }

    /// Local branch names (`refs/heads`), most-recently-committed first so the
    /// branches you're likely to want are near the top of the picker.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        Ok(self
            .run([
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)",
                "refs/heads/",
            ])?
            .lines())
    }

    /// Local branches with their ahead/behind vs their upstream, in one
    /// `for-each-ref` — the refs browser's margin. `%(upstream:track)` reports
    /// `[ahead N, behind M]`; `nobracket` drops the brackets. A branch with no
    /// upstream (or a `gone` one) reports 0/0.
    pub fn local_branches_tracking(&self) -> Result<Vec<LocalBranch>> {
        let out = self.run([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%00%(upstream:track,nobracket)",
            "refs/heads/",
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|line| {
                let (name, track) = line.split_once('\0').unwrap_or((line, ""));
                if name.is_empty() {
                    return None;
                }
                let (mut ahead, mut behind) = (0, 0);
                for part in track.split(',') {
                    let part = part.trim();
                    if let Some(n) = part.strip_prefix("ahead ") {
                        ahead = n.trim().parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix("behind ") {
                        behind = n.trim().parse().unwrap_or(0);
                    }
                }
                Some(LocalBranch {
                    name: name.to_string(),
                    ahead,
                    behind,
                })
            })
            .collect())
    }

    /// Check out `target`, DWIM-creating a local tracking branch when a
    /// remote-only branch is chosen (matching magit's friendly checkout): a
    /// local branch is switched to directly; a `remote/branch` ref for a known
    /// remote checks out the short name so git auto-creates the tracking branch;
    /// anything else (a tag or commit) is checked out verbatim (detaching HEAD).
    pub fn checkout(&self, target: &str) -> Result<String> {
        let arg = self.checkout_arg(target)?;
        Ok(self.run(["checkout", &arg])?.report())
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
        Ok(self.run(&args)?.report())
    }

    /// `git checkout -b <name> [start]` — create a branch and check it out.
    pub fn create_and_checkout(&self, name: &str, start: Option<&str>) -> Result<String> {
        let mut args = vec!["checkout".to_string(), "-b".to_string(), name.to_string()];
        args.extend(start.map(str::to_string));
        Ok(self.run(&args)?.report())
    }

    /// `git branch -m <old> <new>` — rename a branch.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<String> {
        Ok(self.run(["branch", "-m", old, new])?.report())
    }

    /// `git branch -d/-D <name>` — delete a branch (`-D` to force, for an
    /// unmerged branch).
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<String> {
        let flag = if force { "-D" } else { "-d" };
        Ok(self.run(["branch", flag, name])?.report())
    }
}

//! Worktree operations — the `%` worktree browser/transient: list the linked
//! worktrees and add/remove/move them (magit's `magit-worktree`).

use crate::error::Result;
use crate::repo::Repo;

/// One entry from `git worktree list` — a checkout backed by the shared repo.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Absolute path to the worktree's root.
    pub path: String,
    /// Short HEAD hash, or `None` for a bare entry.
    pub head: Option<String>,
    /// Short branch name when on a branch (else detached or bare).
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
    /// The main (primary) worktree — the one holding the shared `.git`. It
    /// can't be removed.
    pub is_main: bool,
    /// The worktree this `Repo` is opened on.
    pub is_current: bool,
}

impl Repo {
    /// Linked worktrees (including the main one), in git's listing order — the
    /// main worktree first. Parsed from `git worktree list --porcelain`.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let out = self.run(["worktree", "list", "--porcelain"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let current = std::fs::canonicalize(self.workdir()).ok();
        let mut worktrees: Vec<Worktree> = Vec::new();
        let mut cur: Option<Worktree> = None;
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                if let Some(w) = cur.take() {
                    worktrees.push(w);
                }
                cur = Some(Worktree {
                    path: path.to_string(),
                    head: None,
                    branch: None,
                    bare: false,
                    detached: false,
                    locked: false,
                    prunable: false,
                    is_main: false,
                    is_current: false,
                });
            } else if let Some(w) = cur.as_mut() {
                if let Some(sha) = line.strip_prefix("HEAD ") {
                    w.head = Some(sha.chars().take(7).collect());
                } else if let Some(b) = line.strip_prefix("branch ") {
                    w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
                } else if line == "detached" {
                    w.detached = true;
                } else if line == "bare" {
                    w.bare = true;
                } else if line == "locked" || line.starts_with("locked ") {
                    w.locked = true;
                } else if line == "prunable" || line.starts_with("prunable ") {
                    w.prunable = true;
                }
            }
        }
        if let Some(w) = cur.take() {
            worktrees.push(w);
        }
        for (i, w) in worktrees.iter_mut().enumerate() {
            w.is_main = i == 0;
            w.is_current = current
                .as_deref()
                .zip(std::fs::canonicalize(&w.path).ok())
                .is_some_and(|(c, p)| p == c);
        }
        Ok(worktrees)
    }

    /// `git worktree add <dir> <commit>` — check out an existing commit or
    /// branch in a new worktree (git DWIMs a remote branch into tracking, like
    /// checkout).
    pub fn worktree_add(&self, dir: &str, commit: &str) -> Result<String> {
        Ok(self.run(["worktree", "add", dir, commit])?.report())
    }

    /// `git worktree add -b <branch> <dir> [start]` — create a new branch and
    /// check it out in a new worktree.
    pub fn worktree_add_branch(
        &self,
        dir: &str,
        branch: &str,
        start: Option<&str>,
    ) -> Result<String> {
        let mut args = vec!["worktree", "add", "-b", branch, dir];
        if let Some(start) = start {
            args.push(start);
        }
        Ok(self.run(args)?.report())
    }

    /// `git worktree remove [--force] <path>`. Force is needed when the
    /// worktree has uncommitted changes or is locked.
    pub fn worktree_remove(&self, path: &str, force: bool) -> Result<String> {
        let args: &[&str] = if force {
            &["worktree", "remove", "--force", path]
        } else {
            &["worktree", "remove", path]
        };
        Ok(self.run(args)?.report())
    }

    /// `git worktree move <from> <to>`.
    pub fn worktree_move(&self, from: &str, to: &str) -> Result<String> {
        Ok(self.run(["worktree", "move", from, to])?.report())
    }

    /// `git worktree prune` — drop administrative entries for worktrees whose
    /// directories were deleted out from under git.
    pub fn worktree_prune(&self) -> Result<String> {
        Ok(self.run(["worktree", "prune"])?.report())
    }
}

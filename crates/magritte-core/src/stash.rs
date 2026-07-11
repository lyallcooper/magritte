//! Stash operations — the `Z` stash transient's commands (push, apply, pop,
//! drop, list), mirroring magit's `magit-stash`.

use crate::error::{Error, Result};
use crate::repo::Repo;

/// What `git stash push` saves — magit's both / index / keeping-index variants
/// (`magit-stash-both`, `magit-stash-index`, `magit-stash-keep-index`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashKind {
    /// The index and the working tree (plain `git stash push`).
    Both,
    /// Only the staged changes (`--staged`, git ≥ 2.35); unstaged and
    /// untracked changes stay put.
    Staged,
    /// The index and the working tree, but leave the index applied afterwards
    /// (`--keep-index`).
    KeepIndex,
}

/// Whether a stash or snapshot also saves files git isn't tracking — the
/// stash menu's `-u` / `-a` switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashUntracked {
    /// Tracked changes only.
    No,
    /// Also untracked files (`--include-untracked`).
    Untracked,
    /// Also untracked and ignored files (`--all`).
    All,
}

impl StashUntracked {
    /// Read the `-u`/`-a` switches out of a transient's argument list; `--all`
    /// wins when both are set (it is a superset).
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Self {
        if args.iter().any(|a| a.as_ref() == "--all") {
            StashUntracked::All
        } else if args.iter().any(|a| a.as_ref() == "--include-untracked") {
            StashUntracked::Untracked
        } else {
            StashUntracked::No
        }
    }
}

/// What a snapshot records — magit's `Z`/`I`/`W` snapshot variants
/// (`magit-snapshot-both`, `magit-snapshot-index`, `magit-snapshot-worktree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// The index and the working tree.
    Both,
    /// Only the index.
    Index,
    /// Only the working tree's unstaged changes.
    Worktree,
}

/// One entry from `git stash list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// The stash ref, e.g. `stash@{0}`.
    pub reference: String,
    /// The stash subject, e.g. `WIP on main: 1a2b3c initial`.
    pub message: String,
}

impl Stash {
    /// `stash@{0}  WIP on main: …` — the picker/list display form.
    pub fn display(&self) -> String {
        format!("{}  {}", self.reference, self.message)
    }
}

impl Repo {
    /// The stash entries, newest (`stash@{0}`) first.
    pub fn stash_list(&self) -> Result<Vec<Stash>> {
        // `%gd` is the selector (stash@{N}); `%gs` the subject. NUL-terminate
        // each record and split on a unit separator so messages can't confuse
        // the parse.
        let out = self.run(["stash", "list", "--format=%gd%x1f%gs", "-z"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .split('\0')
            .filter(|r| !r.is_empty())
            .filter_map(|record| {
                record
                    .split_once('\u{1f}')
                    .map(|(reference, message)| Stash {
                        reference: reference.trim().to_string(),
                        message: message.trim().to_string(),
                    })
            })
            .collect())
    }

    /// `git stash push [--staged|--keep-index] [-u|-a] [-m <message>]
    /// [-- <paths>]` — stash per `kind`, optionally limited to `paths` (empty
    /// = everything).
    pub fn stash_push(
        &self,
        kind: StashKind,
        message: Option<&str>,
        untracked: StashUntracked,
        paths: &[String],
    ) -> Result<String> {
        let mut args = vec!["stash".to_string(), "push".to_string()];
        match kind {
            StashKind::Both => {}
            StashKind::Staged => args.push("--staged".into()),
            StashKind::KeepIndex => args.push("--keep-index".into()),
        }
        // An index-only stash has no untracked side; git rejects the
        // combination, and magit's index variant ignores the switches too.
        if kind != StashKind::Staged {
            match untracked {
                StashUntracked::No => {}
                StashUntracked::Untracked => args.push("--include-untracked".into()),
                StashUntracked::All => args.push("--all".into()),
            }
        }
        if let Some(m) = message.map(str::trim).filter(|m| !m.is_empty()) {
            args.push("--message".into());
            args.push(m.to_string());
        }
        if !paths.is_empty() {
            args.push("--".into());
            args.extend(paths.iter().cloned());
        }
        Ok(self.run(&args)?.report())
    }

    /// `git stash apply <reference>` — apply a stash, keeping it in the list.
    pub fn stash_apply(&self, reference: &str) -> Result<String> {
        Ok(self.run(["stash", "apply", reference])?.report())
    }

    /// `git stash pop <reference>` — apply a stash and drop it on success.
    pub fn stash_pop(&self, reference: &str) -> Result<String> {
        Ok(self.run(["stash", "pop", reference])?.report())
    }

    /// `git stash drop <reference>` — delete a stash without applying it.
    pub fn stash_drop(&self, reference: &str) -> Result<String> {
        Ok(self.run(["stash", "drop", reference])?.report())
    }

    /// `git stash branch <branch> <reference>` — create and check out `branch`
    /// from the commit the stash was made on, then apply the stash (dropping it
    /// if it applies cleanly), mirroring `magit-stash-branch`.
    pub fn stash_branch(&self, branch: &str, reference: &str) -> Result<String> {
        Ok(self.run(["stash", "branch", branch, reference])?.report())
    }

    /// Save a snapshot: record the chosen state on `refs/stash` *without*
    /// touching the index or working tree — a faithful port of magit's
    /// `magit-snapshot-*` commands (`magit-stash-save` with `keep=t`, built by
    /// hand from plumbing since `git stash create` can neither limit the sides
    /// nor include untracked files). The stash commit has the same shape
    /// `git stash` produces: HEAD (or a pre-stash index commit for a
    /// worktree-only snapshot) as first parent, the index commit second, and
    /// an untracked-files commit third when `-u`/`-a` is in effect.
    pub fn stash_snapshot(&self, kind: SnapshotKind, untracked: StashUntracked) -> Result<String> {
        let (want_index, want_worktree) = match kind {
            SnapshotKind::Both => (true, true),
            SnapshotKind::Index => (true, false),
            SnapshotKind::Worktree => (false, true),
        };
        // Like magit's index variant, an index snapshot has no untracked side.
        let untracked = match kind {
            SnapshotKind::Index => StashUntracked::No,
            _ => untracked,
        };
        // Anything to save? (magit-stash-save's guard, message and all.)
        let names = |args: &[&str]| -> Result<bool> {
            Ok(!self.run(args.iter().copied())?.stdout.is_empty())
        };
        let staged = names(&["diff", "--cached", "--name-only", "--no-ext-diff"])?;
        let unstaged = names(&["diff", "--name-only", "--no-ext-diff"])?;
        let untracked_files = self.untracked_snapshot_files(untracked)?;
        if !((want_index && staged) || (want_worktree && unstaged) || !untracked_files.is_empty()) {
            return Err(Error::Message(
                match kind {
                    SnapshotKind::Both => "No local changes to save",
                    SnapshotKind::Index => "No staged changes to save",
                    SnapshotKind::Worktree => "No unstaged changes to save",
                }
                .to_string(),
            ));
        }
        if self.run(["rev-parse", "--verify", "HEAD"]).is_err() {
            return Err(Error::Message(
                "You do not have the initial commit yet".to_string(),
            ));
        }
        // "branch: shorthash subject" — the summary every stash message carries.
        let summary = {
            let branch = self
                .current_branch()
                .ok()
                .flatten()
                .unwrap_or_else(|| "(no branch)".to_string());
            let head = self.run(["log", "-1", "--format=%h %s", "HEAD"])?;
            format!(
                "{branch}: {}",
                String::from_utf8_lossy(&head.stdout).trim_end()
            )
        };
        let message = format!("WIP on {summary}");
        let sha = |out: crate::GitOutput| String::from_utf8_lossy(&out.stdout).trim().to_string();
        let commit_tree = |tree: &str, parents: &[&str], msg: &str| -> Result<String> {
            let mut args = vec!["-c", "commit.gpgsign=false", "commit-tree", tree];
            for p in parents {
                args.push("-p");
                args.push(p);
            }
            args.push("-m");
            args.push(msg);
            Ok(sha(self.run(args)?))
        };
        // A worktree-only snapshot parents the chain on a throwaway commit of
        // the current index instead of HEAD, so the final diff (worktree vs
        // first parent) excludes the staged changes.
        let mut head = sha(self.run(["rev-parse", "HEAD"])?);
        let index_tree = sha(self.run(["write-tree"])?);
        if want_worktree && !want_index {
            head = commit_tree(&index_tree, &[&head], "pre-stash index")?;
        }
        let index_commit = commit_tree(&index_tree, &[&head], &format!("index on {summary}"))?;
        // The remaining trees are built in a temporary index so the real one
        // is never touched.
        let tmp_index = self
            .git_dir()?
            .join(format!("magritte-snapshot-index-{}", std::process::id()));
        let tmp = tmp_index.to_string_lossy().to_string();
        let result = (|| -> Result<String> {
            let with_tmp =
                |args: &[&str]| self.run_with_env(args.iter().copied(), "GIT_INDEX_FILE", &tmp);
            let untracked_commit = if untracked_files.is_empty() {
                None
            } else {
                with_tmp(&["read-tree", "--empty"])?;
                let mut args = vec!["update-index", "--add", "--"];
                args.extend(untracked_files.iter().map(String::as_str));
                with_tmp(&args)?;
                let tree = sha(with_tmp(&["write-tree"])?);
                Some(commit_tree(
                    &tree,
                    &[],
                    &format!("untracked files on {summary}"),
                )?)
            };
            let final_tree = if want_worktree {
                with_tmp(&["read-tree", &index_commit])?;
                let changed = with_tmp(&["diff", "-z", "--name-only", "--no-ext-diff", &head])?;
                let changed: Vec<String> = String::from_utf8_lossy(&changed.stdout)
                    .split('\0')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
                if !changed.is_empty() {
                    let mut args = vec!["update-index", "--add", "--remove", "--"];
                    args.extend(changed.iter().map(String::as_str));
                    with_tmp(&args)?;
                }
                sha(with_tmp(&["write-tree"])?)
            } else {
                index_tree.clone()
            };
            let mut parents = vec![head.as_str(), index_commit.as_str()];
            if let Some(u) = untracked_commit.as_deref() {
                parents.push(u);
            }
            let snapshot = commit_tree(&final_tree, &parents, &message)?;
            self.run(["stash", "store", "-m", &message, &snapshot])?;
            Ok(format!("Saved snapshot: {message}"))
        })();
        let _ = std::fs::remove_file(&tmp_index);
        result
    }

    /// The paths an `-u`/`-a` snapshot must record (NUL-separated `ls-files`);
    /// empty for [`StashUntracked::No`].
    fn untracked_snapshot_files(&self, untracked: StashUntracked) -> Result<Vec<String>> {
        let args: &[&str] = match untracked {
            StashUntracked::No => return Ok(Vec::new()),
            StashUntracked::Untracked => &["ls-files", "-z", "--others", "--exclude-standard"],
            StashUntracked::All => &["ls-files", "-z", "--others"],
        };
        let out = self.run(args.iter().copied())?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }
}

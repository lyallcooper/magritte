//! Starting a `git rebase` (see `magit-sequence.el`). Once a rebase pauses on a
//! conflict, it's driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::repo::Repo;

/// Distinguishes concurrent todo temp files (parallel tests, or two rebases),
/// since the pid alone isn't unique across threads.
static TODO_SEQ: AtomicU64 = AtomicU64::new(0);

/// What to do with a commit in an interactive-rebase todo (git's instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    /// Keep the commit as-is.
    Pick,
    /// Stop to edit the message (deferred in v1 — see `rebase_interactive`).
    Reword,
    /// Pause after applying so the commit can be amended (`--continue` resumes).
    Edit,
    /// Meld into the previous commit, combining messages.
    Squash,
    /// Meld into the previous commit, discarding this message.
    Fixup,
    /// Remove the commit.
    Drop,
}

impl RebaseAction {
    /// The git-rebase-todo keyword.
    pub fn keyword(self) -> &'static str {
        match self {
            RebaseAction::Pick => "pick",
            RebaseAction::Reword => "reword",
            RebaseAction::Edit => "edit",
            RebaseAction::Squash => "squash",
            RebaseAction::Fixup => "fixup",
            RebaseAction::Drop => "drop",
        }
    }
}

/// One line of an interactive-rebase todo: an action against a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStep {
    pub action: RebaseAction,
    /// Abbreviated commit hash (git resolves it within the todo).
    pub oid: String,
    /// Commit subject, for display in the editor.
    pub subject: String,
}

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

    /// The default interactive-rebase todo for `base..HEAD`: every commit as a
    /// `pick`, oldest first (the order git lists them in the todo).
    pub fn rebase_todo(&self, base: &str) -> Result<Vec<RebaseStep>> {
        let out = self.run([
            "log",
            "--reverse",
            "--format=%h%x1f%s",
            &format!("{base}..HEAD"),
        ])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let (oid, subject) = line.split_once('\u{1f}')?;
                Some(RebaseStep {
                    action: RebaseAction::Pick,
                    oid: oid.trim().to_string(),
                    subject: subject.to_string(),
                })
            })
            .collect())
    }

    /// Run an interactive rebase onto `base` with the given todo. The todo is
    /// injected via a throwaway sequence editor (`cp <file>`) instead of opening
    /// git's editor, and `GIT_EDITOR` is neutralized (`true`) so `squash` keeps
    /// the combined message rather than blocking on an editor. `edit` (and any
    /// conflict) pauses the rebase, which the in-progress banner then drives via
    /// continue/skip/abort.
    ///
    /// v1 note: `reword` is intentionally not offered by the frontend yet — it
    /// needs a message editor mid-rebase (with `GIT_EDITOR=true` it would be a
    /// no-op). `squash` keeps the auto-combined message (no inline edit).
    pub fn rebase_interactive(
        &self,
        base: &str,
        steps: &[RebaseStep],
        args: &[String],
    ) -> Result<String> {
        if steps.iter().all(|s| s.action == RebaseAction::Drop) {
            return Err(Error::Message(
                "nothing to do — every commit is dropped".into(),
            ));
        }
        let todo: String = steps
            .iter()
            .map(|s| format!("{} {}\n", s.action.keyword(), s.oid))
            .collect();

        // A unique temp file (space-free path) holds our todo; the sequence
        // editor copies it over git's generated todo. The pid+counter keeps
        // concurrent rebases (and parallel tests) from sharing one file.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TODO_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path: PathBuf = std::env::temp_dir().join(format!("magritte-rebase-todo-{unique}"));
        std::fs::write(&path, todo)
            .map_err(|e| Error::Message(format!("{}: {e}", path.display())))?;

        let mut argv = vec![
            "-c".to_string(),
            format!("sequence.editor=cp '{}'", path.display()),
            "rebase".to_string(),
            "-i".to_string(),
        ];
        argv.extend(args.iter().cloned());
        argv.push(base.to_string());

        let result = self.run_with_env(&argv, "GIT_EDITOR", "true");
        let _ = std::fs::remove_file(&path);
        let out = result?;
        let stderr = out.stderr.trim();
        Ok(if stderr.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            stderr.lines().next_back().unwrap_or("").to_string()
        })
    }
}

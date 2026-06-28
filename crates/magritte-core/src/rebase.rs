//! Starting a `git rebase` (see `magit-sequence.el`). Once a rebase pauses on a
//! conflict, it's driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::{Error, Result};
use crate::repo::Repo;

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

    /// Parse a git-rebase-todo keyword (long or short form), or `None` for one we
    /// don't model (e.g. `exec`, `label`, `merge`).
    pub fn from_keyword(word: &str) -> Option<Self> {
        Some(match word {
            "pick" | "p" => RebaseAction::Pick,
            "reword" | "r" => RebaseAction::Reword,
            "edit" | "e" => RebaseAction::Edit,
            "squash" | "s" => RebaseAction::Squash,
            "fixup" | "f" => RebaseAction::Fixup,
            "drop" | "d" => RebaseAction::Drop,
            _ => return None,
        })
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
        Ok(self.run(&argv)?.status_line())
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

    /// The remaining instructions of an in-progress interactive rebase, parsed
    /// from `rebase-merge/git-rebase-todo` — the steps git has yet to apply.
    /// Empty when the rebase has no editable plan left (e.g. paused on its last
    /// commit) or isn't using the interactive (merge) backend.
    pub fn rebase_current_todo(&self) -> Result<Vec<RebaseStep>> {
        let todo = self.git_dir()?.join("rebase-merge").join("git-rebase-todo");
        let Ok(text) = std::fs::read_to_string(&todo) else {
            return Ok(Vec::new());
        };
        Ok(text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let action = RebaseAction::from_keyword(parts.next()?)?;
                // The todo stores full oids; abbreviate for display (git still
                // resolves the prefix against the rebase's own commits on write).
                let oid: String = parts.next()?.chars().take(7).collect();
                // git may write the oneline as a trailing comment ("# subject");
                // strip the marker so the editor shows a clean subject.
                let subject = parts
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("# ")
                    .to_string();
                Some(RebaseStep {
                    action,
                    oid,
                    subject,
                })
            })
            .collect())
    }

    /// Rewrite the remaining todo of an in-progress rebase (`git rebase
    /// --edit-todo`), injected via the throwaway sequence editor — magit's
    /// `magit-rebase-edit`. The rebase stays paused at its current stop; the new
    /// plan governs what happens once it's continued.
    pub fn rebase_edit_todo(&self, steps: &[RebaseStep]) -> Result<String> {
        let todo: String = steps
            .iter()
            .map(|s| format!("{} {}\n", s.action.keyword(), s.oid))
            .collect();
        let argv = vec!["rebase".to_string(), "--edit-todo".to_string()];
        Ok(self.run_with_sequence_editor(&todo, &argv)?.status_line())
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
        // Build the todo; the sequence-editor runner handles feeding it to git.
        let todo: String = steps
            .iter()
            .map(|s| format!("{} {}\n", s.action.keyword(), s.oid))
            .collect();
        let mut argv = vec!["rebase".to_string(), "-i".to_string()];
        argv.extend(args.iter().cloned());
        argv.push(base.to_string());
        Ok(self.run_with_sequence_editor(&todo, &argv)?.status_line())
    }
}

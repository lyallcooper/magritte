//! Committing: create a new commit, or amend/reword the previous one.
//!
//! Messages are fed to git on stdin (`git commit --file -`) so we never touch
//! a temp file or the user's `$EDITOR`. Signing, hooks, and identity all come
//! from the user's git config, exactly as on the command line.

use crate::error::Result;
use crate::repo::Repo;

/// The `commit [--amend [--only --allow-empty]]` argv prefix for a mode, shared
/// by the stdin-message and external-editor commit paths.
fn commit_mode_args(mode: CommitMode) -> Vec<String> {
    let mut argv: Vec<String> = vec!["commit".into()];
    match mode {
        CommitMode::Create => {}
        CommitMode::Amend => argv.push("--amend".into()),
        CommitMode::Reword => {
            argv.push("--amend".into());
            argv.push("--only".into());
            // Match magit: a reword may legitimately end up empty-diff.
            argv.push("--allow-empty".into());
        }
    }
    argv
}

/// Which kind of commit to make with an edited message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMode {
    /// A new commit from the staged changes.
    Create,
    /// Replace HEAD, incorporating staged changes (`--amend`).
    Amend,
    /// Replace only HEAD's message, leaving its tree (`--amend --only`).
    Reword,
}

impl Repo {
    /// HEAD's full commit message (subject + body), for pre-filling an amend or
    /// reword editor.
    pub fn head_message(&self) -> Result<String> {
        let out = self.run(["log", "-1", "--format=%B"])?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Commit `message` according to `mode`, with the given extra arguments
    /// (e.g. `--signoff`, `--all`). Returns git's summary line.
    pub fn commit(&self, message: &str, mode: CommitMode, args: &[String]) -> Result<String> {
        let mut argv = commit_mode_args(mode);
        // Read the message from stdin.
        argv.push("--file".into());
        argv.push("-".into());
        argv.extend(args.iter().cloned());

        let out = self.run_with_input(&argv, message.as_bytes())?;
        Ok(out.first_line())
    }

    /// Commit according to `mode` by launching the user's external editor on the
    /// commit message (an interactive `git commit` with `GIT_EDITOR` set to
    /// `git_editor`), rather than supplying the message directly. git pre-fills
    /// `COMMIT_EDITMSG` (the template for a create, HEAD's message for an
    /// amend/reword) and blocks until the editor exits. An empty message aborts
    /// the commit (git's own behavior), surfaced as an error. Returns git's
    /// summary line.
    pub fn commit_with_editor(
        &self,
        mode: CommitMode,
        args: &[String],
        git_editor: &str,
    ) -> Result<String> {
        let mut argv = commit_mode_args(mode);
        argv.extend(args.iter().cloned());
        let out = self.run_with_env(&argv, "GIT_EDITOR", git_editor)?;
        Ok(out.first_line())
    }

    /// Whether `rev` is already published — i.e. contained in one of the
    /// configured `published` branches (it's an ancestor of that branch).
    /// Returns the first matching branch (to name it in the "already pushed to
    /// …" warning before a history rewrite) or `None`.
    ///
    /// This mirrors magit's `magit-commit-amend-assert`: a bounded
    /// `merge-base --is-ancestor` test against a small, configurable list
    /// (`magit-published-branches`), *not* a scan of every remote-tracking ref
    /// (`branch -r --contains`) — which is seconds-slow on a large repo (~14s
    /// across 58k refs), worst of all for the common case of amending an
    /// unpushed commit (it must check every ref to conclude "no"). Branches that
    /// don't exist here are simply not matched (the default list names both
    /// `origin/main` and `origin/master`).
    pub fn published_on(&self, rev: &str, published: &[String]) -> Option<String> {
        for branch in published {
            // `merge-base --is-ancestor` is one cheap test per branch — and it
            // returns false (not errors out of this loop) for a missing ref, so
            // no separate existence check is needed. It's hidden from the `$`
            // log as a query.
            if self
                .succeeds(["merge-base", "--is-ancestor", rev, branch.as_str()])
                .unwrap_or(false)
            {
                return Some(branch.clone());
            }
        }
        None
    }

    /// Amend HEAD with the staged changes, keeping its message (`--no-edit`).
    pub fn commit_extend(&self, args: &[String]) -> Result<String> {
        let mut argv: Vec<String> = vec!["commit".into(), "--amend".into(), "--no-edit".into()];
        argv.extend(args.iter().cloned());
        let out = self.run(&argv)?;
        Ok(out.first_line())
    }
}

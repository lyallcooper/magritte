//! Committing: create a new commit, or amend/reword the previous one.
//!
//! Messages are fed to git on stdin (`git commit --file -`) so we never touch
//! a temp file or the user's `$EDITOR`. Signing, hooks, and identity all come
//! from the user's git config, exactly as on the command line.

use crate::error::Result;
use crate::repo::Repo;

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
        // Read the message from stdin.
        argv.push("--file".into());
        argv.push("-".into());
        argv.extend(args.iter().cloned());

        let out = self.run_with_input(&argv, message.as_bytes())?;
        Ok(summary(&out.stdout, &out.stderr))
    }

    /// Remote-tracking branches that contain `rev` — i.e. where it's already
    /// been published. Empty means unpushed. Used to warn (naming the branch)
    /// before amending/rewording a pushed commit (rewriting published history).
    pub fn published_branches(&self, rev: &str) -> Result<Vec<String>> {
        let out = self.run(["branch", "-r", "--contains", rev])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            // Skip the "origin/HEAD -> origin/main" symbolic-ref line.
            .filter(|l| !l.is_empty() && !l.contains(" -> "))
            .map(str::to_string)
            .collect())
    }

    /// Amend HEAD with the staged changes, keeping its message (`--no-edit`).
    pub fn commit_extend(&self, args: &[String]) -> Result<String> {
        let mut argv: Vec<String> = vec!["commit".into(), "--amend".into(), "--no-edit".into()];
        argv.extend(args.iter().cloned());
        let out = self.run(&argv)?;
        Ok(summary(&out.stdout, &out.stderr))
    }
}

fn summary(stdout: &[u8], stderr: &str) -> String {
    let out = String::from_utf8_lossy(stdout);
    let out = out.trim();
    if out.is_empty() {
        stderr.trim().to_string()
    } else {
        out.lines().next().unwrap_or("").to_string()
    }
}

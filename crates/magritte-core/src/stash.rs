//! Stash operations — the `Z` stash transient's commands (push, apply, pop,
//! drop, list), mirroring magit's `magit-stash`.

use crate::error::Result;
use crate::remote::summary;
use crate::repo::Repo;

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

    /// `git stash push [-u] [-m <message>]` — stash the working tree and index.
    pub fn stash_push(&self, message: Option<&str>, include_untracked: bool) -> Result<String> {
        let mut args = vec!["stash".to_string(), "push".to_string()];
        if include_untracked {
            args.push("--include-untracked".into());
        }
        if let Some(m) = message.map(str::trim).filter(|m| !m.is_empty()) {
            args.push("--message".into());
            args.push(m.to_string());
        }
        Ok(summary(self.run(&args)?))
    }

    /// `git stash apply <reference>` — apply a stash, keeping it in the list.
    pub fn stash_apply(&self, reference: &str) -> Result<String> {
        Ok(summary(self.run(["stash", "apply", reference])?))
    }

    /// `git stash pop <reference>` — apply a stash and drop it on success.
    pub fn stash_pop(&self, reference: &str) -> Result<String> {
        Ok(summary(self.run(["stash", "pop", reference])?))
    }

    /// `git stash drop <reference>` — delete a stash without applying it.
    pub fn stash_drop(&self, reference: &str) -> Result<String> {
        Ok(summary(self.run(["stash", "drop", reference])?))
    }
}

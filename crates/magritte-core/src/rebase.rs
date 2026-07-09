//! Starting a `git rebase` (see `magit-sequence.el`). Once a rebase pauses on a
//! conflict, it's driven by the in-progress sequence controls
//! (continue/skip/abort) in [`crate::sequence`].

use crate::error::{Error, Result};
use crate::repo::{git_args, Repo};
use crate::sequence::parse_todo_line;

/// What to do with a commit in an interactive-rebase todo (git's instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    /// Keep the commit as-is.
    Pick,
    /// Stop to edit the message.
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
        Ok(self
            .run(git_args(&["rebase"], args, &[onto]))?
            .status_line())
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
    /// Only commit-carrying instructions are modeled (others pass through
    /// [`rebase_edit_todo`](Self::rebase_edit_todo) untouched). Empty when the
    /// rebase has no editable plan left (e.g. paused on its last commit) or
    /// isn't using the interactive (merge) backend.
    pub fn rebase_current_todo(&self) -> Result<Vec<RebaseStep>> {
        let todo = self.git_dir()?.join("rebase-merge").join("git-rebase-todo");
        let Ok(text) = std::fs::read_to_string(&todo) else {
            return Ok(Vec::new());
        };
        Ok(text
            .lines()
            .filter_map(parse_todo_line)
            .filter_map(|(verb, oid, subject)| {
                Some(RebaseStep {
                    action: RebaseAction::from_keyword(verb)?,
                    oid: oid?,
                    subject,
                })
            })
            .collect())
    }

    /// Rewrite the remaining todo of an in-progress rebase (`git rebase
    /// --edit-todo`), injected via the throwaway sequence editor — magit's
    /// `magit-rebase-edit`. The rebase stays paused at its current stop; the new
    /// plan governs what happens once it's continued. The edited steps are
    /// merged back into the current todo so instructions the editor doesn't
    /// model (`exec`, `label`, `reset`, `merge`, `update-ref`, `break`, …)
    /// survive the rewrite instead of being silently dropped.
    pub fn rebase_edit_todo(&self, steps: &[RebaseStep]) -> Result<String> {
        let path = self.git_dir()?.join("rebase-merge").join("git-rebase-todo");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let todo = merge_edited_todo(&current, steps);
        let argv = vec!["rebase".to_string(), "--edit-todo".to_string()];
        Ok(self.run_with_sequence_editor(&todo, &argv)?.status_line())
    }

    /// Run an interactive rebase onto `base` with the given todo. The todo is
    /// injected via a throwaway sequence editor (`cp <file>`) instead of opening
    /// git's editor. UI-level `reword` steps are written as git `edit` stops so
    /// the app can open its in-app message editor mid-rebase instead of relying
    /// on `$EDITOR`. `squash` keeps the auto-combined message; `edit`/`reword`
    /// (and any conflict) pause the rebase, which the UI drives via
    /// continue/skip/abort.
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
            .map(|s| format!("{} {}\n", rebase_action_for_git(s.action).keyword(), s.oid))
            .collect();
        let argv = git_args(&["rebase", "-i"], args, &[base]);
        Ok(self.run_with_sequence_editor(&todo, &argv)?.status_line())
    }

    /// Autosquash `fixup!`/`squash! ` commits into their targets: an
    /// interactive rebase since `base` with `--autosquash`, accepting git's
    /// auto-generated (and reordered) todo unedited. Unlike
    /// [`rebase_interactive`](Self::rebase_interactive), which injects our own
    /// todo, this hands git a no-op sequence editor (`true`) so its autosquash
    /// ordering stands; `GIT_EDITOR=true` keeps a `squash!` from blocking on a
    /// message editor (it takes the combined message). `--keep-empty` preserves
    /// any intentionally-empty commit in the range. A conflict pauses the
    /// rebase, surfaced as the in-progress sequence.
    pub fn rebase_autosquash(&self, base: &str, args: &[String]) -> Result<String> {
        // `-c sequence.editor=true` accepts git's auto-generated todo unedited;
        // GIT_EDITOR=true (via run_with_env) keeps a `squash!` from blocking on
        // a message editor, taking the combined message instead. Autosquash *is*
        // this command, so drop the transient's own `--[no-]autosquash` toggle —
        // switches land after the lead args, and a `--no-autosquash` (emitted
        // when `rebase.autoSquash` is configured on but toggled off) would win
        // and silently turn the whole rebase into a no-op.
        let switches: Vec<String> = args
            .iter()
            .filter(|a| *a != "--autosquash" && *a != "--no-autosquash")
            .cloned()
            .collect();
        let argv = git_args(
            &[
                "-c",
                "sequence.editor=true",
                "rebase",
                "-i",
                "--autosquash",
                "--keep-empty",
            ],
            &switches,
            &[base],
        );
        Ok(self
            .run_with_env(&argv, "GIT_EDITOR", "true")?
            .status_line())
    }

    /// The merge base of `@{upstream}` and HEAD, the default starting point for
    /// an autosquash (magit's `magit-rebase-autosquash`): only commits since
    /// the upstream are candidates. `None` when there's no upstream or no merge
    /// base (a fresh branch), leaving the caller to pick a base.
    pub fn upstream_merge_base(&self) -> Option<String> {
        let out = self
            .run_optional(["merge-base", "@{upstream}", "HEAD"])
            .ok()??;
        let base = out.stdout_text();
        (!base.is_empty()).then_some(base)
    }

    /// The original commit at which an interactive rebase is currently stopped,
    /// if git exposes one. Present for `edit` stops and therefore for app-managed
    /// `reword` stops (which are written as `edit` in git's todo).
    pub fn rebase_stopped_sha(&self) -> Option<String> {
        let path = self
            .git_dir()
            .ok()?
            .join("rebase-merge")
            .join("stopped-sha");
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

fn rebase_action_for_git(action: RebaseAction) -> RebaseAction {
    match action {
        RebaseAction::Reword => RebaseAction::Edit,
        other => other,
    }
}

/// Reassemble a todo from the edited steps while round-tripping the
/// instructions the editor doesn't model (`exec`, `label`, `reset`, `merge`,
/// `update-ref`, `break`, …): each such line stays attached to the modeled
/// step it followed in `current` (so it moves with that commit on reorder),
/// and lines before the first modeled step stay at the front. A step whose
/// effective action is unchanged keeps its original line verbatim, preserving
/// flags like `fixup -C` and the subject comment.
fn merge_edited_todo(current: &str, steps: &[RebaseStep]) -> String {
    struct Segment<'a> {
        oid: String,
        action: RebaseAction,
        line: &'a str,
        trailing: Vec<&'a str>,
    }
    let mut leading: Vec<&str> = Vec::new();
    let mut segments: Vec<Option<Segment>> = Vec::new();
    for line in current.lines() {
        let Some((verb, oid, _)) = parse_todo_line(line) else {
            continue; // blank/comment — git ignores them
        };
        match (RebaseAction::from_keyword(verb), oid) {
            (Some(action), Some(oid)) => segments.push(Some(Segment {
                oid,
                action,
                line,
                trailing: Vec::new(),
            })),
            _ => match segments.last_mut() {
                Some(Some(seg)) => seg.trailing.push(line),
                _ => leading.push(line),
            },
        }
    }

    let mut out = String::new();
    for line in &leading {
        out.push_str(line);
        out.push('\n');
    }
    for step in steps {
        // Consume the matching segment front-to-back so a duplicated oid pairs
        // up in order.
        let seg = segments
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|seg| seg.oid == step.oid))
            .and_then(Option::take);
        let action = rebase_action_for_git(step.action);
        match &seg {
            Some(seg) if seg.action == action => {
                out.push_str(seg.line);
                out.push('\n');
            }
            _ => {
                out.push_str(action.keyword());
                out.push(' ');
                out.push_str(&step.oid);
                out.push('\n');
            }
        }
        for line in seg.iter().flat_map(|s| &s.trailing) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{merge_edited_todo, RebaseAction, RebaseStep};

    fn step(action: RebaseAction, oid: &str) -> RebaseStep {
        RebaseStep {
            action,
            oid: oid.to_string(),
            subject: String::new(),
        }
    }

    #[test]
    fn merge_keeps_unmodeled_lines_attached_to_their_step() {
        let current = "label onto\npick 1111111 one\nexec make test\npick 2222222 two\nupdate-ref refs/heads/dep\n";
        // Reorder the picks; the exec follows its commit, the leading label
        // stays first, the trailing update-ref follows its commit.
        let steps = [
            step(RebaseAction::Pick, "2222222"),
            step(RebaseAction::Pick, "1111111"),
        ];
        assert_eq!(
            merge_edited_todo(current, &steps),
            "label onto\npick 2222222 two\nupdate-ref refs/heads/dep\npick 1111111 one\nexec make test\n"
        );
    }

    #[test]
    fn merge_rewrites_only_changed_actions() {
        let current = "pick 1111111 one\nfixup -C 2222222 # amend! one\n";
        // Unchanged fixup keeps its `-C` flag verbatim; the reworded pick is
        // rewritten as an app-managed edit stop.
        let steps = [
            step(RebaseAction::Reword, "1111111"),
            step(RebaseAction::Fixup, "2222222"),
        ];
        assert_eq!(
            merge_edited_todo(current, &steps),
            "edit 1111111\nfixup -C 2222222 # amend! one\n"
        );
    }
}

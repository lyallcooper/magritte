//! Detecting and driving an in-progress sequencing operation — merge, rebase,
//! cherry-pick, revert, or `am`. Mirrors magit's status surfacing and its
//! continue/skip/abort controls (see `.reference/magit/lisp/magit-sequence.el`
//! and `magit-merge.el`): the in-progress state is read from files under the
//! git dir, and each operation has a fixed set of continue/skip/abort commands.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::repo::Repo;

/// Which sequencing operation is currently in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceKind {
    Merge,
    /// Interactive (`rebase-merge/`) or non-interactive (`rebase-apply/`) rebase.
    Rebase,
    CherryPick,
    Revert,
    /// `git am` — applying a mailbox of patches.
    Am,
}

impl SequenceKind {
    /// The `git` subcommand whose `--continue`/`--skip`/`--abort` drive it.
    fn verb(self) -> &'static str {
        match self {
            SequenceKind::Merge => "merge",
            SequenceKind::Rebase => "rebase",
            SequenceKind::CherryPick => "cherry-pick",
            SequenceKind::Revert => "revert",
            SequenceKind::Am => "am",
        }
    }

    /// Human label for the operation (also the git subcommand).
    pub fn label(self) -> &'static str {
        self.verb()
    }

    /// A merge has no `--continue`/`--skip`: it's finished by committing the
    /// resolved index. The others advance through their plan.
    pub fn can_continue(self) -> bool {
        !matches!(self, SequenceKind::Merge)
    }
    pub fn can_skip(self) -> bool {
        !matches!(self, SequenceKind::Merge)
    }
}

/// One line in the sequence's plan, for display. `action` is the git verb
/// (`pick`, `revert`, …) or one of our markers: `stop` (the commit it halted
/// on) and `onto` (a rebase's base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceStep {
    pub action: String,
    pub oid: Option<String>,
    pub subject: String,
}

/// A snapshot of the in-progress sequencing operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    pub kind: SequenceKind,
    /// e.g. "Rebasing main onto abc1234", "Cherry Picking", "Merging feature".
    pub heading: String,
    /// Remaining/relevant steps, in plan order.
    pub steps: Vec<SequenceStep>,
}

impl Repo {
    /// The absolute git dir (handles worktrees and `.git`-file links), where the
    /// sequencing state files live.
    fn git_dir(&self) -> Result<PathBuf> {
        let out = self.run(["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(
            String::from_utf8_lossy(&out.stdout).trim_end(),
        ))
    }

    /// The in-progress sequencing operation, if any. The checks and their order
    /// mirror magit's `*-in-progress-p` predicates.
    pub fn sequence(&self) -> Option<Sequence> {
        let dir = self.git_dir().ok()?;
        let has = |p: &str| dir.join(p).exists();

        if has("rebase-apply/applying") {
            return Some(self.am_sequence(&dir));
        }
        if has("rebase-merge") || has("rebase-apply/onto") {
            return Some(self.rebase_sequence(&dir));
        }
        if has("CHERRY_PICK_HEAD") || self.sequencer_first_verb(&dir).as_deref() == Some("pick") {
            return Some(self.sequencer_sequence(&dir, SequenceKind::CherryPick, "Cherry Picking"));
        }
        if has("REVERT_HEAD") || self.sequencer_first_verb(&dir).as_deref() == Some("revert") {
            return Some(self.sequencer_sequence(&dir, SequenceKind::Revert, "Reverting"));
        }
        if has("MERGE_HEAD") {
            return Some(self.merge_sequence(&dir));
        }
        None
    }

    /// Continue the in-progress sequence (advance past a resolved stop). Run
    /// with a non-interactive editor so a message-bearing step takes its
    /// prepared message rather than blocking on an editor we can't show.
    pub fn sequence_continue(&self, kind: SequenceKind) -> Result<String> {
        let out = self.run_with_env([kind.verb(), "--continue"], "GIT_EDITOR", "true")?;
        Ok(summary(&out.stdout, &out.stderr))
    }

    /// Skip the current step.
    pub fn sequence_skip(&self, kind: SequenceKind) -> Result<String> {
        let out = self.run([kind.verb(), "--skip"])?;
        Ok(summary(&out.stdout, &out.stderr))
    }

    /// Abort the sequence, restoring the pre-operation state.
    pub fn sequence_abort(&self, kind: SequenceKind) -> Result<String> {
        let out = self.run([kind.verb(), "--abort"])?;
        Ok(summary(&out.stdout, &out.stderr))
    }

    fn rebase_sequence(&self, dir: &Path) -> Sequence {
        let merge = dir.join("rebase-merge");
        let apply = dir.join("rebase-apply");
        let base = if merge.exists() { &merge } else { &apply };
        let branch = read_trim(base.join("head-name"))
            .map(|s| s.trim_start_matches("refs/heads/").to_string());
        let onto = read_trim(base.join("onto")).map(|o| short(&o));
        let heading = match (branch, onto) {
            (Some(b), Some(o)) => format!("Rebasing {b} onto {o}"),
            (Some(b), None) => format!("Rebasing {b}"),
            _ => "Rebasing".to_string(),
        };
        // Interactive rebases keep a human-readable todo ("pick <sha> <subj>");
        // the steps are exactly those lines. Apply-based rebases don't, so we
        // fall back to a progress count.
        let steps = if let Some(todo) = read_trim(merge.join("git-rebase-todo")) {
            parse_todo(&todo)
        } else if let (Some(next), Some(last)) = (
            read_trim(apply.join("next")).and_then(|s| s.parse::<u32>().ok()),
            read_trim(apply.join("last")).and_then(|s| s.parse::<u32>().ok()),
        ) {
            vec![SequenceStep {
                action: "stop".to_string(),
                oid: None,
                subject: format!("patch {next}/{last}"),
            }]
        } else {
            Vec::new()
        };
        Sequence {
            kind: SequenceKind::Rebase,
            heading,
            steps,
        }
    }

    fn am_sequence(&self, dir: &Path) -> Sequence {
        let apply = dir.join("rebase-apply");
        let next = read_trim(apply.join("next")).unwrap_or_default();
        let last = read_trim(apply.join("last")).unwrap_or_default();
        Sequence {
            kind: SequenceKind::Am,
            heading: "Applying patches".to_string(),
            steps: vec![SequenceStep {
                action: "stop".to_string(),
                oid: None,
                subject: format!("patch {next}/{last}"),
            }],
        }
    }

    fn sequencer_sequence(&self, dir: &Path, kind: SequenceKind, label: &str) -> Sequence {
        let steps = read_trim(dir.join("sequencer/todo"))
            .map(|t| parse_todo(&t))
            .unwrap_or_default();
        Sequence {
            kind,
            heading: label.to_string(),
            steps,
        }
    }

    fn merge_sequence(&self, dir: &Path) -> Sequence {
        // MERGE_HEAD lists the commit(s) being merged (>1 for an octopus merge).
        let heads: Vec<String> = read_trim(dir.join("MERGE_HEAD"))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        // Resolve each to "<short> <subject>" in one call; fall back to bare oids.
        let steps = if heads.is_empty() {
            Vec::new()
        } else {
            let described = self
                .run(
                    ["log", "--no-walk=unsorted", "--format=%h%x00%s"]
                        .iter()
                        .map(|s| s.to_string())
                        .chain(heads.iter().cloned())
                        .collect::<Vec<_>>(),
                )
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            described
                .lines()
                .filter_map(|l| l.split_once('\0'))
                .map(|(oid, subject)| SequenceStep {
                    action: "join".to_string(),
                    oid: Some(oid.to_string()),
                    subject: subject.to_string(),
                })
                .collect()
        };
        let names = steps
            .iter()
            .map(|s| s.subject.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Sequence {
            kind: SequenceKind::Merge,
            heading: if names.is_empty() {
                "Merging".to_string()
            } else {
                format!("Merging {names}")
            },
            steps,
        }
    }

    /// The verb of the first entry in `sequencer/todo` ("pick"/"revert"), used
    /// to tell a cherry-pick from a revert when the `*_HEAD` marker is absent
    /// (e.g. a `--no-commit` run paused on a conflict).
    fn sequencer_first_verb(&self, dir: &Path) -> Option<String> {
        read_trim(dir.join("sequencer/todo"))?
            .lines()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string)
    }
}

/// Parse a git todo file ("pick <sha> <subject>" lines) into steps, skipping
/// blanks, comments, and `noop`.
fn parse_todo(text: &str) -> Vec<SequenceStep> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let action = parts.next()?.to_string();
            if action == "noop" {
                return None;
            }
            let oid = parts.next().map(short);
            let subject = parts.next().unwrap_or("").to_string();
            Some(SequenceStep {
                action,
                oid,
                subject,
            })
        })
        .collect()
}

fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

fn read_trim(path: PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
}

/// git reports operation progress on stderr; prefer it, else stdout.
fn summary(stdout: &[u8], stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::from_utf8_lossy(stdout).trim().to_string()
    } else {
        stderr.lines().next_back().unwrap_or("").to_string()
    }
}

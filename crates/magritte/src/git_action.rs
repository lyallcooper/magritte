//! The resolved git-mutation layer. An [`Action`] is a concrete operation,
//! produced from the view's "act at point" / region selection, that runs on the
//! background executor against a [`Repo`]. This module is UI-free — it depends
//! only on the core — so the mutation logic stays separate from rendering.

use magritte_core::{ApplyTarget, FileDiff, Repo, FileEntry};

/// The staging verb a keypress requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Stage,
    Unstage,
    Discard,
}

/// Selected changed lines within one file, grouped by hunk: each entry is
/// `(hunk index, line indices within that hunk)`.
pub type HunkSelections = Vec<(usize, Vec<usize>)>;

/// How a multi-hunk region selection should be applied.
#[derive(Debug, Clone, Copy)]
pub enum RegionKind {
    Stage,
    Unstage,
    Discard,
    DiscardStaged,
}

/// A resolved git mutation, runnable on the background executor.
pub enum Action {
    StageFile(String),
    UnstageFile(FileEntry),
    DiscardTracked(String),
    DiscardUntracked(String),
    StageAll,
    UnstageAll,
    StageHunk(FileDiff, usize),
    UnstageHunk(FileDiff, usize),
    DiscardHunk(FileDiff, usize),
    StageLines(FileDiff, usize, Vec<usize>),
    UnstageLines(FileDiff, usize, Vec<usize>),
    DiscardLines(FileDiff, usize, Vec<usize>),
    DiscardStagedFile(FileEntry),
    DiscardStagedHunk(FileDiff, usize),
    DiscardStagedLines(FileDiff, usize, Vec<usize>),
    /// A region selection spanning one file's hunks: hunk index -> line indices.
    ApplyRegion {
        kind: RegionKind,
        file: FileDiff,
        selections: HunkSelections,
    },
    /// Several actions applied in sequence (a region spanning multiple files).
    Batch(Vec<Action>),
}

impl Action {
    pub fn run(self, repo: &Repo) -> Result<(), String> {
        let hunk = |file: &FileDiff, ix: usize| -> Result<(), String> {
            file.hunks
                .get(ix)
                .ok_or_else(|| "hunk no longer present".to_string())
                .map(|_| ())
        };
        let to_err = |r: magritte_core::Result<()>| r.map_err(|e| e.to_string());
        match self {
            Action::StageFile(p) => to_err(repo.stage_file(&p)),
            Action::UnstageFile(e) => to_err(repo.unstage_file(&e)),
            Action::DiscardTracked(p) => to_err(repo.discard_tracked_file(&p)),
            Action::DiscardUntracked(p) => to_err(repo.discard_untracked_file(&p)),
            Action::StageAll => to_err(repo.stage_all()),
            Action::UnstageAll => to_err(repo.unstage_all()),
            Action::StageHunk(f, h) => {
                hunk(&f, h).and_then(|_| to_err(repo.stage_hunk(&f, &f.hunks[h])))
            }
            Action::UnstageHunk(f, h) => {
                hunk(&f, h).and_then(|_| to_err(repo.unstage_hunk(&f, &f.hunks[h])))
            }
            Action::DiscardHunk(f, h) => {
                hunk(&f, h).and_then(|_| to_err(repo.discard_hunk(&f, &f.hunks[h])))
            }
            Action::StageLines(f, h, l) => {
                hunk(&f, h).and_then(|_| to_err(repo.stage_lines(&f, &f.hunks[h], &l)))
            }
            Action::UnstageLines(f, h, l) => {
                hunk(&f, h).and_then(|_| to_err(repo.unstage_lines(&f, &f.hunks[h], &l)))
            }
            Action::DiscardLines(f, h, l) => {
                hunk(&f, h).and_then(|_| to_err(repo.discard_lines(&f, &f.hunks[h], &l)))
            }
            Action::DiscardStagedFile(e) => to_err(repo.discard_staged_file(&e)),
            Action::DiscardStagedHunk(f, h) => {
                hunk(&f, h).and_then(|_| to_err(repo.discard_staged_hunk(&f, &f.hunks[h])))
            }
            Action::DiscardStagedLines(f, h, l) => {
                hunk(&f, h).and_then(|_| to_err(repo.discard_staged_lines(&f, &f.hunks[h], &l)))
            }
            Action::ApplyRegion {
                kind,
                file,
                selections,
            } => to_err(match kind {
                RegionKind::Stage => repo.stage_file_lines(&file, &selections),
                RegionKind::Unstage => repo.unstage_file_lines(&file, &selections),
                RegionKind::Discard => repo.discard_file_lines(&file, &selections),
                RegionKind::DiscardStaged => repo.discard_staged_file_lines(&file, &selections),
            }),
            Action::Batch(actions) => {
                // Pre-verify the parts that can be dry-run (region applies, via
                // `check_patch`) so a bad patch aborts before anything mutates.
                // Whole-file ops (stage/unstage/discard of an entire file) can't
                // be dry-run, so they run in sequence; a later failure leaves the
                // earlier ones applied. We can't undo that, but we say so rather
                // than report only the last error as if nothing else happened.
                for action in &actions {
                    action.check(repo)?;
                }
                let total = actions.len();
                for (done, action) in actions.into_iter().enumerate() {
                    if let Err(e) = action.run(repo) {
                        return Err(if done == 0 {
                            e
                        } else {
                            format!("{e} — applied {done} of {total}; the rest were not")
                        });
                    }
                }
                Ok(())
            }
        }
    }

    /// Dry-run an action without mutating the repo. Only region applies are
    /// checkable (via `check_patch`); whole-file ops can't be dry-run and report
    /// `Ok` here, so a `Batch` mixing them is not fully precheckable — see the
    /// atomicity note in `run`.
    pub fn check(&self, repo: &Repo) -> Result<(), String> {
        match self {
            Action::ApplyRegion {
                kind,
                file,
                selections,
            } => {
                let (reverse, target) = match kind {
                    RegionKind::Stage => (false, ApplyTarget::Index),
                    RegionKind::Unstage => (true, ApplyTarget::Index),
                    RegionKind::Discard => (true, ApplyTarget::Worktree),
                    // The staged-discard worktree step is best-effort (--reject);
                    // the meaningful precondition is the index reverse-apply.
                    RegionKind::DiscardStaged => (true, ApplyTarget::Index),
                };
                let patch = magritte_core::stage::build_file_patch(file, selections, reverse);
                repo.check_patch(&patch, target, reverse)
                    .map_err(|e| e.to_string())
            }
            Action::Batch(actions) => actions.iter().try_for_each(|a| a.check(repo)),
            _ => Ok(()),
        }
    }
}

/// A human-readable confirmation prompt for a discard action.
pub fn describe_discard(action: &Action) -> String {
    match action {
        Action::DiscardUntracked(p) => format!("Delete untracked {p}?"),
        Action::DiscardTracked(p) => format!("Discard unstaged changes to {p}?"),
        Action::DiscardHunk(f, _) => format!("Discard hunk in {}?", f.display_path()),
        Action::DiscardLines(f, _, l) => {
            format!("Discard {} line(s) in {}?", l.len(), f.display_path())
        }
        Action::DiscardStagedFile(e) => format!("Discard staged changes to {}?", e.path),
        Action::DiscardStagedHunk(f, _) => {
            format!("Discard staged hunk in {}?", f.display_path())
        }
        Action::DiscardStagedLines(f, _, l) => {
            format!(
                "Discard {} staged line(s) in {}?",
                l.len(),
                f.display_path()
            )
        }
        Action::ApplyRegion {
            kind,
            file,
            selections,
        } => {
            let n: usize = selections.iter().map(|(_, l)| l.len()).sum();
            let staged = matches!(kind, RegionKind::DiscardStaged);
            format!(
                "Discard {n} {}line(s) in {}?",
                if staged { "staged " } else { "" },
                file.display_path()
            )
        }
        Action::Batch(actions) => {
            format!("Discard selection across {} files?", actions.len())
        }
        _ => "Discard?".to_string(),
    }
}

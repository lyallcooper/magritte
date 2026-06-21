//! Staging operations: file-, hunk-, and line-level stage / unstage / discard.
//!
//! File-level operations use plain git commands. Hunk- and line-level
//! operations synthesize a patch from the [`FileDiff`] model and feed it to
//! `git apply`, which is how magit implements partial staging.
//!
//! The delicate part is building a patch for a *subset* of a hunk's changed
//! lines. The rule depends on the direction the patch will be applied:
//!
//! * **Forward** (staging: `git apply --cached`) — the patch transforms the
//!   index toward the working tree. Unselected `+` lines are *dropped* (we are
//!   not adding them); unselected `-` lines become *context* (they must remain
//!   in the index).
//! * **Reverse** (unstaging or discarding: `git apply --reverse`) — the patch
//!   is read backwards. Unselected `+` lines become *context* (they must be
//!   preserved); unselected `-` lines are *dropped* (we are not restoring them).
//!
//! Context lines are always kept; the `\ No newline at end of file` marker is
//! emitted only when the line it annotates was emitted.

use crate::diff::{FileDiff, Hunk, LineKind};
use crate::error::Result;
use crate::repo::Repo;

/// Where a patch is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTarget {
    /// The index (`git apply --cached`).
    Index,
    /// The working tree (`git apply`).
    Worktree,
}

impl Repo {
    // --- File-level -------------------------------------------------------

    /// Stage all changes to `path` (including additions and deletions).
    pub fn stage_file(&self, path: &str) -> Result<()> {
        self.run(["add", "-A", "--", path])?;
        Ok(())
    }

    /// Unstage `path`, resetting its index entry to HEAD.
    pub fn unstage_file(&self, path: &str) -> Result<()> {
        self.run(["reset", "-q", "--", path])?;
        Ok(())
    }

    /// Stage every change in the working tree (`git add -A`).
    pub fn stage_all(&self) -> Result<()> {
        self.run(["add", "-A"])?;
        Ok(())
    }

    /// Unstage everything, resetting the whole index to HEAD.
    pub fn unstage_all(&self) -> Result<()> {
        self.run(["reset", "-q"])?;
        Ok(())
    }

    /// Discard unstaged changes to a tracked `path`, restoring it from the
    /// index. **Destructive.**
    pub fn discard_tracked_file(&self, path: &str) -> Result<()> {
        self.run(["checkout", "--", path])?;
        Ok(())
    }

    /// Remove an untracked `path` from the working tree. **Destructive.**
    pub fn discard_untracked_file(&self, path: &str) -> Result<()> {
        self.run(["clean", "-f", "-d", "-q", "--", path])?;
        Ok(())
    }

    // --- Hunk-level -------------------------------------------------------

    /// Stage an entire hunk into the index.
    pub fn stage_hunk(&self, file: &FileDiff, hunk: &Hunk) -> Result<()> {
        self.stage_lines(file, hunk, &all_change_indices(hunk))
    }

    /// Unstage an entire hunk from the index.
    pub fn unstage_hunk(&self, file: &FileDiff, hunk: &Hunk) -> Result<()> {
        self.unstage_lines(file, hunk, &all_change_indices(hunk))
    }

    /// Discard an entire hunk's changes from the working tree. **Destructive.**
    pub fn discard_hunk(&self, file: &FileDiff, hunk: &Hunk) -> Result<()> {
        self.discard_lines(file, hunk, &all_change_indices(hunk))
    }

    // --- Line-level -------------------------------------------------------
    //
    // `selected` holds indices into `hunk.lines` (Added/Removed lines) that the
    // operation should act on.

    /// Stage the selected changed lines of a hunk into the index.
    pub fn stage_lines(&self, file: &FileDiff, hunk: &Hunk, selected: &[usize]) -> Result<()> {
        let patch = build_patch(file, hunk, selected, false);
        self.apply_patch(&patch, ApplyTarget::Index, false)
    }

    /// Unstage the selected changed lines of a hunk from the index.
    pub fn unstage_lines(&self, file: &FileDiff, hunk: &Hunk, selected: &[usize]) -> Result<()> {
        let patch = build_patch(file, hunk, selected, true);
        self.apply_patch(&patch, ApplyTarget::Index, true)
    }

    /// Discard the selected changed lines of a hunk from the working tree.
    /// **Destructive.**
    pub fn discard_lines(&self, file: &FileDiff, hunk: &Hunk, selected: &[usize]) -> Result<()> {
        let patch = build_patch(file, hunk, selected, true);
        self.apply_patch(&patch, ApplyTarget::Worktree, true)
    }

    /// Apply a unidiff patch via `git apply`.
    pub fn apply_patch(&self, patch: &str, target: ApplyTarget, reverse: bool) -> Result<()> {
        self.run_apply(patch, target, reverse, false)
    }

    fn run_apply(
        &self,
        patch: &str,
        target: ApplyTarget,
        reverse: bool,
        check_only: bool,
    ) -> Result<()> {
        let mut args: Vec<&str> = vec!["apply"];
        if target == ApplyTarget::Index {
            args.push("--cached");
        }
        if reverse {
            args.push("--reverse");
        }
        if check_only {
            // --check verifies the patch applies without modifying anything.
            args.push("--check");
        }
        // Let git infer hunk line counts from the patch body; our `build_patch`
        // also computes them, but this is a cheap robustness margin.
        args.push("--recount");
        // git apply reads the patch from stdin when given no file arguments.
        self.run_with_input(args, patch.as_bytes())?;
        Ok(())
    }

    // --- Discarding staged changes ---------------------------------------
    //
    // Discarding a *staged* change reverts both the index and the working tree
    // to HEAD for the affected content. **Destructive.**

    /// Revert a staged file entirely to its HEAD state (index and worktree).
    pub fn discard_staged_file(&self, path: &str) -> Result<()> {
        self.run(["checkout", "HEAD", "--", path])?;
        Ok(())
    }

    /// Discard a staged hunk from both the index and the working tree.
    pub fn discard_staged_hunk(&self, file: &FileDiff, hunk: &Hunk) -> Result<()> {
        self.discard_staged_lines(file, hunk, &all_change_indices(hunk))
    }

    /// Discard the selected staged lines from both the index and working tree.
    ///
    /// Both reverse-applies are dry-run-checked first, so if either would fail
    /// (e.g. the worktree has further unstaged edits to these lines) we abort
    /// without leaving a half-applied, inconsistent state.
    pub fn discard_staged_lines(
        &self,
        file: &FileDiff,
        hunk: &Hunk,
        selected: &[usize],
    ) -> Result<()> {
        let patch = build_patch(file, hunk, selected, true);
        self.run_apply(&patch, ApplyTarget::Index, true, true)?;
        self.run_apply(&patch, ApplyTarget::Worktree, true, true)?;
        self.run_apply(&patch, ApplyTarget::Index, true, false)?;
        self.run_apply(&patch, ApplyTarget::Worktree, true, false)?;
        Ok(())
    }

    // --- Multi-hunk region operations ------------------------------------
    //
    // `selections` maps hunk index -> selected line indices, so a region that
    // spans several hunks of one file is applied as a single patch.

    pub fn stage_file_lines(&self, file: &FileDiff, selections: &[(usize, Vec<usize>)]) -> Result<()> {
        let patch = build_file_patch(file, selections, false);
        self.apply_patch(&patch, ApplyTarget::Index, false)
    }

    pub fn unstage_file_lines(&self, file: &FileDiff, selections: &[(usize, Vec<usize>)]) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.apply_patch(&patch, ApplyTarget::Index, true)
    }

    pub fn discard_file_lines(&self, file: &FileDiff, selections: &[(usize, Vec<usize>)]) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.apply_patch(&patch, ApplyTarget::Worktree, true)
    }

    pub fn discard_staged_file_lines(
        &self,
        file: &FileDiff,
        selections: &[(usize, Vec<usize>)],
    ) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.run_apply(&patch, ApplyTarget::Index, true, true)?;
        self.run_apply(&patch, ApplyTarget::Worktree, true, true)?;
        self.run_apply(&patch, ApplyTarget::Index, true, false)?;
        self.run_apply(&patch, ApplyTarget::Worktree, true, false)?;
        Ok(())
    }
}

/// Indices of all changed (Added/Removed) lines in a hunk.
fn all_change_indices(hunk: &Hunk) -> Vec<usize> {
    hunk.lines
        .iter()
        .enumerate()
        .filter(|(_, l)| matches!(l.kind, LineKind::Added | LineKind::Removed))
        .map(|(i, _)| i)
        .collect()
}

/// Build a unidiff patch for the selected lines of a single hunk.
///
/// See the module docs for the forward/reverse selection rules. When `selected`
/// contains every changed line this reproduces the original hunk verbatim.
pub fn build_patch(file: &FileDiff, hunk: &Hunk, selected: &[usize], reverse: bool) -> String {
    let mut out = file_header(file);
    out.push_str(&hunk_block(hunk, selected, reverse));
    out
}

/// Build a unidiff patch spanning multiple hunks of one file. `selections` maps
/// hunk index -> selected line indices; hunks not listed are omitted. Used for
/// region selections that cross hunk boundaries.
pub fn build_file_patch(
    file: &FileDiff,
    selections: &[(usize, Vec<usize>)],
    reverse: bool,
) -> String {
    let mut out = file_header(file);
    for (hunk_ix, selected) in selections {
        if let Some(hunk) = file.hunks.get(*hunk_ix) {
            out.push_str(&hunk_block(hunk, selected, reverse));
        }
    }
    out
}

fn file_header(file: &FileDiff) -> String {
    let mut out = String::new();
    for header in &file.header_lines {
        out.push_str(header);
        out.push('\n');
    }
    out
}

/// Build the `@@ ... @@` header and body for one hunk given the selected lines.
fn hunk_block(hunk: &Hunk, selected: &[usize], reverse: bool) -> String {
    let mut body = String::new();
    let mut old_count: u32 = 0;
    let mut new_count: u32 = 0;
    let mut prev_emitted = false;

    let emit = |body: &mut String, sign: char, content: &str| {
        body.push(sign);
        body.push_str(content);
        body.push('\n');
    };

    for (i, line) in hunk.lines.iter().enumerate() {
        let is_selected = selected.contains(&i);
        match line.kind {
            LineKind::Context => {
                emit(&mut body, ' ', &line.content);
                old_count += 1;
                new_count += 1;
                prev_emitted = true;
            }
            LineKind::Added => {
                if is_selected {
                    emit(&mut body, '+', &line.content);
                    new_count += 1;
                    prev_emitted = true;
                } else if reverse {
                    // Preserve the line on both sides so the apply leaves it.
                    emit(&mut body, ' ', &line.content);
                    old_count += 1;
                    new_count += 1;
                    prev_emitted = true;
                } else {
                    prev_emitted = false; // forward: drop
                }
            }
            LineKind::Removed => {
                if is_selected {
                    emit(&mut body, '-', &line.content);
                    old_count += 1;
                    prev_emitted = true;
                } else if reverse {
                    prev_emitted = false; // drop
                } else {
                    // forward: keep as context so the line stays in the index.
                    emit(&mut body, ' ', &line.content);
                    old_count += 1;
                    new_count += 1;
                    prev_emitted = true;
                }
            }
            LineKind::NoNewline => {
                if prev_emitted {
                    body.push_str(&line.content);
                    body.push('\n');
                }
            }
        }
    }

    let heading = if hunk.section_heading.is_empty() {
        String::new()
    } else {
        format!(" {}", hunk.section_heading)
    };
    let mut block = format!(
        "@@ -{},{} +{},{} @@{}\n",
        hunk.old_start, old_count, hunk.new_start, new_count, heading
    );
    block.push_str(&body);
    block
}

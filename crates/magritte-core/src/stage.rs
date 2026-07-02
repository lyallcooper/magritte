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

use std::fs;
use std::path::Path;

use crate::diff::{DiffSource, FileDiff, Hunk, LineKind};
use crate::error::{Error, Result};
use crate::repo::Repo;
use crate::status::{Change, FileEntry};

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

    /// Unstage `entry`'s path, resetting its index entry to HEAD. Takes the
    /// caller's already-parsed entry (rather than re-running `git status` per
    /// file) because a staged rename must reset both the new and original
    /// paths — `reset -- <new>` alone leaves the original's staged deletion
    /// behind (`D <old>`).
    pub fn unstage_file(&self, entry: &FileEntry) -> Result<()> {
        let orig = (entry.index == Change::Renamed)
            .then(|| entry.orig_path.clone())
            .flatten();
        match orig {
            Some(old) => self.run(["reset", "-q", "--", &entry.path, &old])?,
            None => self.run(["reset", "-q", "--", &entry.path])?,
        };
        Ok(())
    }

    /// Stage all changes to tracked files (`git add -u`, magit's
    /// stage-modified): new content and deletions, but not untracked files —
    /// those stage per file or via their section.
    pub fn stage_modified(&self) -> Result<()> {
        self.run(["add", "-u"])?;
        Ok(())
    }

    /// Stage the given untracked paths (the Untracked section's header verb).
    pub fn stage_untracked(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add".to_string(), "--".to_string()];
        args.extend(paths.iter().cloned());
        self.run(&args)?;
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

    /// Dry-run a patch apply (`git apply --check`) without modifying anything.
    /// Used to verify every action in a multi-file batch applies before any of
    /// them mutates the repo.
    pub fn check_patch(&self, patch: &str, target: ApplyTarget, reverse: bool) -> Result<()> {
        self.run_apply(patch, target, reverse, true)
    }

    fn run_apply(
        &self,
        patch: &str,
        target: ApplyTarget,
        reverse: bool,
        check_only: bool,
    ) -> Result<()> {
        let mut flags: Vec<&str> = Vec::new();
        if target == ApplyTarget::Index {
            flags.push("--cached");
        }
        if reverse {
            flags.push("--reverse");
        }
        if check_only {
            // --check verifies the patch applies without modifying anything.
            flags.push("--check");
        }
        self.git_apply(patch, &flags)
    }

    /// Run `git apply <flags> --recount` over `patch` (from stdin — git apply
    /// reads it there when given no file arguments). `--recount` lets git infer
    /// hunk line counts from the patch body; our `build_patch` also computes
    /// them, but this is a cheap robustness margin.
    fn git_apply(&self, patch: &str, flags: &[&str]) -> Result<()> {
        let mut args: Vec<&str> = vec!["apply"];
        args.extend_from_slice(flags);
        args.push("--recount");
        self.run_with_input(args, patch.as_bytes())?;
        Ok(())
    }

    /// Whether `path` has changes in the working tree not yet staged.
    fn has_unstaged(&self, path: &str) -> Result<bool> {
        // `git diff --quiet` exits non-zero when there are unstaged differences.
        Ok(!self.succeeds(["diff", "--quiet", "--", path])?)
    }

    /// Reverse-apply a discard `patch`, mirroring magit's `magit-discard-apply`:
    ///
    /// * **Unstaged** changes → reverse-apply to the working tree only.
    /// * **Staged** changes, file otherwise clean → reverse-apply to index and
    ///   working tree together (`--index`).
    /// * **Staged** changes on a file that *also* has unstaged edits → reverse
    ///   the staged delta in the index (`--cached`), then in the working tree
    ///   with `--reject` (so overlapping hunks land in `.rej` instead of
    ///   clobbering the unstaged edit).
    ///
    /// In that last case the index delta is removed first; if the working-tree
    /// `--reject` then exits non-zero, some hunks were left as `.rej` and we
    /// report the *partial* discard rather than silently treating it as success.
    fn discard_apply(&self, patch: &str, source: DiffSource, path: &str) -> Result<()> {
        match source {
            DiffSource::Unstaged => self.git_apply(patch, &["--reverse"]),
            DiffSource::Staged => {
                if self.has_unstaged(path)? {
                    self.git_apply(patch, &["--reverse", "--cached"])?;
                    if self.git_apply(patch, &["--reverse", "--reject"]).is_err() {
                        return Err(Error::Message(format!(
                            "Unstaged the change to {path}, but some working-tree hunks \
                             conflicted with your unstaged edits and were left as .rej \
                             files — resolve them manually."
                        )));
                    }
                    Ok(())
                } else {
                    self.git_apply(patch, &["--reverse", "--index"])
                }
            }
        }
    }

    // --- Discarding staged changes ---------------------------------------
    //
    // Discarding a *staged* change removes the staged delta from the index and
    // working tree while preserving any unrelated unstaged edits, mirroring
    // magit's `magit-discard-files` / `magit-discard-apply`. **Destructive.**

    /// Discard a staged file, dispatching on its already-parsed status entry
    /// the way magit does — `checkout HEAD -- path` is *not* used (it would
    /// also blow away unstaged worktree edits and fails on staged new/renamed
    /// files).
    pub fn discard_staged_file(&self, entry: &FileEntry) -> Result<()> {
        let path = &entry.path;
        match entry.index {
            // Staged new/copied file: delete it (or, if it also has unstaged
            // edits, fall back to untracked via add+reset, keeping the content).
            Change::Added | Change::Copied => {
                if entry.worktree.is_modified() {
                    self.run(["add", "--", path])?;
                    self.run(["reset", "-q", "--", path])?;
                } else {
                    self.run(["rm", "--cached", "--force", "--", path])?;
                    let _ = fs::remove_file(self.workdir().join(path));
                }
            }
            // Staged deletion: resurrect by unstaging it.
            Change::Deleted => {
                self.run(["reset", "-q", "--", path])?;
            }
            // Staged rename: rename back to the original path.
            Change::Renamed => {
                let orig = entry.orig_path.clone().unwrap_or_default();
                if orig.is_empty() {
                    // No original recorded; fall back to reverting the content.
                    self.discard_staged_modified(path)?;
                } else if self.workdir().join(path).exists() {
                    if let Some(parent) = Path::new(&orig).parent() {
                        if !parent.as_os_str().is_empty() {
                            let _ = fs::create_dir_all(self.workdir().join(parent));
                        }
                    }
                    self.run(["mv", path, &orig])?;
                } else {
                    self.run(["rm", "--cached", "--", path])?;
                    self.run(["reset", "-q", "--", &orig])?;
                }
            }
            // Staged content change (Modified / TypeChanged): reverse-apply.
            _ => self.discard_staged_modified(path)?,
        }
        Ok(())
    }

    /// Reverse-apply the entire staged diff of a modified file.
    fn discard_staged_modified(&self, path: &str) -> Result<()> {
        let Some(diff) = self.diff_path(DiffSource::Staged, path)? else {
            return Ok(());
        };
        let selections: Vec<(usize, Vec<usize>)> = diff
            .hunks
            .iter()
            .enumerate()
            .map(|(i, h)| (i, all_change_indices(h)))
            .collect();
        let patch = build_file_patch(&diff, &selections, true);
        self.discard_apply(&patch, DiffSource::Staged, path)
    }

    /// Discard a staged hunk from both the index and the working tree.
    pub fn discard_staged_hunk(&self, file: &FileDiff, hunk: &Hunk) -> Result<()> {
        self.discard_staged_lines(file, hunk, &all_change_indices(hunk))
    }

    /// Discard the selected staged lines from both the index and working tree,
    /// preserving any unrelated unstaged edits (see [`discard_apply`]).
    pub fn discard_staged_lines(
        &self,
        file: &FileDiff,
        hunk: &Hunk,
        selected: &[usize],
    ) -> Result<()> {
        let patch = build_patch(file, hunk, selected, true);
        self.discard_apply(&patch, DiffSource::Staged, file.display_path())
    }

    // --- Multi-hunk region operations ------------------------------------
    //
    // `selections` maps hunk index -> selected line indices, so a region that
    // spans several hunks of one file is applied as a single patch.

    pub fn stage_file_lines(
        &self,
        file: &FileDiff,
        selections: &[(usize, Vec<usize>)],
    ) -> Result<()> {
        let patch = build_file_patch(file, selections, false);
        self.apply_patch(&patch, ApplyTarget::Index, false)
    }

    pub fn unstage_file_lines(
        &self,
        file: &FileDiff,
        selections: &[(usize, Vec<usize>)],
    ) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.apply_patch(&patch, ApplyTarget::Index, true)
    }

    pub fn discard_file_lines(
        &self,
        file: &FileDiff,
        selections: &[(usize, Vec<usize>)],
    ) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.apply_patch(&patch, ApplyTarget::Worktree, true)
    }

    pub fn discard_staged_file_lines(
        &self,
        file: &FileDiff,
        selections: &[(usize, Vec<usize>)],
    ) -> Result<()> {
        let patch = build_file_patch(file, selections, true);
        self.discard_apply(&patch, DiffSource::Staged, file.display_path())
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

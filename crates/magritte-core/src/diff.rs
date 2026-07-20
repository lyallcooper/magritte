//! Git's side of content diffs: which views to diff ([`DiffSource`]) and the
//! `git diff` command wrappers. The unified-diff data model and parser live in
//! the shared foundation ([`magritte_vcs::diff`]) and are re-exported here
//! under their original paths.

use crate::error::{Error, Result};
use crate::repo::Repo;

pub use magritte_vcs::diff::{
    parse_diff, unquote_path, DiffLine, FileDiff, Hunk, LineChange, LineKind,
};

/// Which view of the changes to diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffSource {
    /// Working tree vs. index (`git diff`).
    Unstaged,
    /// Index vs. HEAD (`git diff --cached`).
    Staged,
}

/// The flags every content diff requests. `--default-prefix` forces `a/`,`b/`
/// prefixes regardless of the user's diff.mnemonicPrefix / diff.noprefix config,
/// so parsing is stable; `--no-color`/`--no-ext-diff` keep the output plain.
const DIFF_BASE: &[&str] = &[
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--default-prefix",
    "--find-renames",
];

impl Repo {
    fn diff_with(&self, mut args: Vec<String>, paths: &[String]) -> Result<Vec<FileDiff>> {
        if !paths.is_empty() {
            args.push("--".to_string());
            args.extend(paths.iter().cloned());
        }
        let out = self.run(&args)?;
        parse_diff(&out.stdout).map_err(Error::from)
    }

    fn diff_base(&self, extra: &[String]) -> Vec<String> {
        let mut args: Vec<String> = DIFF_BASE.iter().map(|s| s.to_string()).collect();
        // Adjustable context (`+`/`-`/`0` in the UI); git defaults to 3 when unset.
        if let Some(n) = self.diff_context {
            args.push(format!("-U{n}"));
        }
        args.extend(extra.iter().cloned());
        args
    }

    /// The base argv for diffing a [`DiffSource`] (`--cached` for the index).
    fn diff_source_args(&self, source: DiffSource, extra: &[String]) -> Vec<String> {
        let mut args = self.diff_base(extra);
        if source == DiffSource::Staged {
            args.push("--cached".to_string());
        }
        args
    }

    /// Diff a single path against the index or HEAD. For a rename/copy the
    /// caller must pass the original path too (`orig`): a pathspec of the new
    /// path alone excludes the old one, so git reports a whole-file addition
    /// instead of the rename diff. Returns `None` when there is no diff (e.g.
    /// the path is unchanged for that source).
    pub fn diff_path(
        &self,
        source: DiffSource,
        path: &str,
        orig: Option<&str>,
    ) -> Result<Option<FileDiff>> {
        let mut paths = vec![path.to_string()];
        paths.extend(orig.map(str::to_string));
        let mut diffs = self.diff_with(self.diff_source_args(source, &[]), &paths)?;
        Ok(if diffs.is_empty() {
            None
        } else {
            Some(diffs.remove(0))
        })
    }

    /// Diff every changed path for a source in one call (e.g. `git diff
    /// --cached` for all staged changes). Used to show the full staged diff in
    /// the commit editor.
    pub fn diff_all(&self, source: DiffSource) -> Result<Vec<FileDiff>> {
        self.diff_with(self.diff_source_args(source, &[]), &[])
    }

    /// Every tracked change vs. HEAD (`git diff HEAD`): staged and unstaged
    /// modifications/deletions combined, excluding untracked files. This is
    /// exactly the tree `git commit --all` records, so it's the preview for an
    /// all-commit (where the staged-only diff would hide tracked unstaged work).
    /// On an unborn branch there is no HEAD (so `git diff HEAD` would error) and
    /// nothing is tracked yet, so the staged diff is the whole story.
    pub fn diff_tracked_vs_head(&self) -> Result<Vec<FileDiff>> {
        if !self.rev_exists("HEAD") {
            return self.diff_all(DiffSource::Staged);
        }
        self.diff_range("HEAD", &[], &[])
    }

    /// The standalone diff transient's unstaged action (`git diff [args]`).
    pub fn diff_unstaged(&self, extra: &[String], paths: &[String]) -> Result<Vec<FileDiff>> {
        self.diff_with(self.diff_source_args(DiffSource::Unstaged, extra), paths)
    }

    /// The standalone diff transient's staged action (`git diff --cached [args]`).
    pub fn diff_staged(&self, extra: &[String], paths: &[String]) -> Result<Vec<FileDiff>> {
        self.diff_with(self.diff_source_args(DiffSource::Staged, extra), paths)
    }

    /// The whole working tree against a revision (Magit's `Diff worktree`,
    /// defaulting to `HEAD`): staged + unstaged tracked changes.
    pub fn diff_worktree(
        &self,
        rev: &str,
        extra: &[String],
        paths: &[String],
    ) -> Result<Vec<FileDiff>> {
        self.diff_range(rev, extra, paths)
    }

    /// Diff an arbitrary revision or range (`git diff <rev-or-range> [-- paths]`).
    pub fn diff_range(
        &self,
        rev_or_range: &str,
        extra: &[String],
        paths: &[String],
    ) -> Result<Vec<FileDiff>> {
        let mut args = self.diff_base(extra);
        args.push(rev_or_range.to_string());
        self.diff_with(args, paths)
    }

    /// The diff a single commit introduced (its changes vs. its first parent),
    /// for previewing the commit being reworded. Root commits (no parent) are
    /// diffed against the empty tree.
    pub fn diff_commit(&self, rev: &str) -> Result<Vec<FileDiff>> {
        self.diff_commit_with(rev, &[], &[])
    }

    pub fn diff_commit_with(
        &self,
        rev: &str,
        extra: &[String],
        paths: &[String],
    ) -> Result<Vec<FileDiff>> {
        // git's well-known empty-tree object, for diffing a parentless commit.
        const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let parent = format!("{rev}^");
        let base = if self.rev_exists(&parent) {
            parent
        } else {
            EMPTY_TREE.to_string()
        };
        let mut args = self.diff_base(extra);
        args.push(base);
        args.push(rev.to_string());
        self.diff_with(args, paths)
    }

    /// Cheap per-file changed-line counts via `git diff --numstat` (no content),
    /// returning `(path, added + removed)`. Used to decide which diffs are small
    /// enough to prefetch. Binary files and renames are omitted (best-effort).
    pub fn diff_line_counts(&self, source: DiffSource) -> Result<Vec<(String, u32)>> {
        let mut args = vec!["diff", "--numstat"];
        if source == DiffSource::Staged {
            args.push("--cached");
        }
        let out = self.run(args)?;
        let text = String::from_utf8_lossy(&out.stdout);

        let mut counts = Vec::new();
        for line in text.lines() {
            // "<added>\t<removed>\t<path>"; binary files report "-" for counts.
            let mut parts = line.splitn(3, '\t');
            let added = parts.next().unwrap_or("");
            let removed = parts.next().unwrap_or("");
            let Some(path) = parts.next() else { continue };
            if added == "-" || removed == "-" {
                continue; // binary
            }
            if path.contains(" => ") {
                continue; // a rename form; let it load on demand
            }
            let total = added.parse::<u32>().unwrap_or(0) + removed.parse::<u32>().unwrap_or(0);
            counts.push((unquote_path(path), total));
        }
        Ok(counts)
    }
}

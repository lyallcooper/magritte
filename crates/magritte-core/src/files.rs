//! Working-tree file listings, for path completion (e.g. the log file limit).

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// Tracked files (`git ls-files`), as repo-relative paths. Can be large in a
    /// monorepo, so callers should load it off the UI thread.
    pub fn tracked_files(&self) -> Result<Vec<String>> {
        let out = self.run(["ls-files", "-z"])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// The files present in `rev`'s tree (`git ls-tree -r --name-only`), as
    /// repo-relative paths — magit's `magit-revision-files`, the candidate set
    /// for checking a file out of a revision. Same size caveat as
    /// [`tracked_files`](Self::tracked_files).
    pub fn revision_files(&self, rev: &str) -> Result<Vec<String>> {
        let out = self.run(["ls-tree", "-z", "-r", "--name-only", rev])?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// `git checkout <rev> -- <file>` — restore one file's content (index and
    /// working tree) from `rev`, without moving HEAD (magit's
    /// `magit-file-checkout`).
    pub fn checkout_file(&self, rev: &str, file: &str) -> Result<String> {
        self.run(["checkout", rev, "--", file])?;
        Ok(format!("Checked out {file} from {rev}"))
    }
}

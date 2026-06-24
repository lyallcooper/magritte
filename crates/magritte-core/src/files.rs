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
}

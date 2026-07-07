//! Creating and applying patches — magit's patch (`W`) and am (`w`) transients.
//! `format-patch` writes patch files for a commit range; `apply` applies a diff
//! to the worktree without committing; `am` applies a mailbox of patches as
//! commits (pausing into the shared `am` sequence banner on conflict).

use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// Apply a patch/diff file to the working tree (`git apply -- <path>`),
    /// without creating a commit. Fails (surfaced) if it doesn't apply cleanly.
    pub fn apply_patch_file(&self, path: &str) -> Result<String> {
        let out = self.run(["apply", "--", path])?;
        Ok(out.status_line())
    }

    /// Apply a mailbox of patches as commits (`git am -- <path>`). A conflict
    /// leaves the am paused, which the sequence banner surfaces
    /// (continue/skip/abort).
    pub fn am_patch(&self, path: &str) -> Result<String> {
        let out = self.run(["am", "--", path])?;
        Ok(out.status_line())
    }

    /// Create patch files for a commit range/args (`git format-patch <args>`),
    /// writing them to the working directory; returns the created filenames.
    pub fn format_patch(&self, args: &[String]) -> Result<String> {
        let mut argv = vec!["format-patch".to_string()];
        argv.extend(args.iter().cloned());
        let out = self.run(argv)?;
        // format-patch prints the written filenames to stdout; prefer them over
        // the (empty) stderr status line.
        let files = String::from_utf8_lossy(&out.stdout);
        let files = files.trim();
        Ok(if files.is_empty() {
            out.status_line()
        } else {
            files.replace('\n', ", ")
        })
    }
}

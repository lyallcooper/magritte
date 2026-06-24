//! Commit log — the `l` log transient. A linear list of commits for a revision
//! (graph rendering is deferred); a commit's diff is read via
//! [`Repo::diff_commit`](crate::Repo::diff_commit).

use crate::error::Result;
use crate::repo::Repo;

/// One commit in a log listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Abbreviated commit hash.
    pub short_hash: String,
    /// Commit subject (first line of the message).
    pub subject: String,
    /// Ref decorations (`HEAD -> main, origin/main, tag: v1`), empty if none.
    pub refs: String,
    /// Author name.
    pub author: String,
    /// Author date, relative (e.g. `3 days ago`).
    pub date: String,
}

impl Repo {
    /// Up to `limit` commits reachable from `rev`, newest first. Fields are
    /// unit-separated and records NUL-terminated so subjects can't confuse the
    /// parse.
    pub fn log(&self, rev: &str, limit: usize) -> Result<Vec<LogEntry>> {
        let out = self.run([
            "log",
            &format!("--max-count={limit}"),
            "--format=%h%x1f%s%x1f%D%x1f%an%x1f%ar",
            "-z",
            rev,
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .split('\0')
            .filter(|r| !r.is_empty())
            .filter_map(parse_log_record)
            .collect())
    }
}

fn parse_log_record(record: &str) -> Option<LogEntry> {
    let mut fields = record.split('\u{1f}');
    Some(LogEntry {
        short_hash: fields.next()?.trim().to_string(),
        subject: fields.next()?.to_string(),
        refs: fields.next().unwrap_or("").trim().to_string(),
        author: fields.next().unwrap_or("").to_string(),
        date: fields.next().unwrap_or("").to_string(),
    })
}

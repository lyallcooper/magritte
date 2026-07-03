//! Commit log — the `l` log transient. A linear list of commits for a revision
//! (graph rendering is deferred); a commit's diff is read via
//! [`Repo::diff_commit`](crate::Repo::diff_commit).

use std::collections::HashSet;

use crate::error::Result;
use crate::repo::Repo;

/// The fields every log listing requests, unit-separated; records are
/// NUL-terminated (`-z`) so subjects can't confuse the parse.
const LOG_FORMAT: &str = "--format=%H%x1f%h%x1f%s%x1f%D%x1f%an%x1f%ar";

/// Cap on commits listed per divergence side (unpushed/unpulled) in the status
/// buffer, so a pathological divergence can't fetch or render thousands. The
/// exact ahead/behind counts still come from `git status --branch`, so the
/// section titles stay accurate even when the listing is capped.
pub const SECTION_COMMIT_CAP: usize = 256;

/// One commit in a log listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Full commit hash (what we copy / pass to plumbing).
    pub hash: String,
    /// Abbreviated commit hash, for display.
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
    /// Up to `limit` commits reachable from `rev`, newest first.
    pub fn log(&self, rev: &str, limit: usize) -> Result<Vec<LogEntry>> {
        self.log_with(&[format!("--max-count={limit}"), rev.to_string()])
    }

    /// `git log <our-format> <args>` — the general form the log transient drives.
    /// `args` carries the revision/scope (`HEAD`, `--all`, a ref) plus any
    /// limit/search/author options; the format and `-z` are always supplied.
    pub fn log_with(&self, args: &[String]) -> Result<Vec<LogEntry>> {
        let mut full = vec!["log".to_string(), LOG_FORMAT.to_string(), "-z".to_string()];
        full.extend(args.iter().cloned());
        let out = self.run(&full)?;
        Ok(parse_log(&out.stdout))
    }

    /// Commits on `HEAD` not yet on its upstream (`@{upstream}..HEAD`). `Err`
    /// when there's no upstream — callers treat that as an empty list.
    pub fn unpushed(&self) -> Result<Vec<LogEntry>> {
        self.log_with(&["@{upstream}..HEAD".to_string()])
    }

    /// Commits on the upstream not yet on `HEAD` (`HEAD..@{upstream}`). `Err`
    /// when there's no upstream — callers treat that as an empty list.
    pub fn unpulled(&self) -> Result<Vec<LogEntry>> {
        self.log_with(&["HEAD..@{upstream}".to_string()])
    }

    /// Commits unique to `HEAD` (unpushed) and to its upstream (unpulled), each
    /// capped at [`SECTION_COMMIT_CAP`].
    pub fn upstream_divergence(&self) -> Result<(Vec<LogEntry>, Vec<LogEntry>)> {
        self.divergence("HEAD", "@{upstream}")
    }

    /// Commits on `HEAD` not yet on the push target (`@{push}..HEAD`) — the
    /// triangular-workflow counterpart of [`unpushed`](Self::unpushed). `Err`
    /// when there's no distinct push target.
    pub fn unpushed_to_push(&self) -> Result<Vec<LogEntry>> {
        self.log_with(&["@{push}..HEAD".to_string()])
    }

    /// Commits on the push target not yet on `HEAD` (`HEAD..@{push}`). `Err`
    /// when there's no distinct push target.
    pub fn unpulled_from_push(&self) -> Result<Vec<LogEntry>> {
        self.log_with(&["HEAD..@{push}".to_string()])
    }

    /// Commits unique to `HEAD` (unpushed-to-push) and to its push target
    /// (unpulled-from-push), each capped at [`SECTION_COMMIT_CAP`].
    pub fn push_divergence(&self) -> Result<(Vec<LogEntry>, Vec<LogEntry>)> {
        self.divergence("HEAD", "@{push}")
    }

    fn divergence(&self, left: &str, right: &str) -> Result<(Vec<LogEntry>, Vec<LogEntry>)> {
        // Two capped range walks (left-only = ahead, right-only = behind)
        // rather than one symmetric `--left-right` walk: a single capped walk
        // would skew toward the larger side in a lopsided divergence, starving
        // the smaller section.
        let cap = format!("--max-count={SECTION_COMMIT_CAP}");
        let ahead = self.log_with(&[cap.clone(), format!("{right}..{left}")])?;
        let behind = self.log_with(&[cap, format!("{left}..{right}")])?;
        Ok((ahead, behind))
    }

    /// `git log -g` (the reflog), newest first. The reflog selector
    /// (`HEAD@{N}`) is surfaced via the `refs` field and the reflog subject via
    /// `subject`, so it renders with the same row layout as a normal log.
    pub fn reflog(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let out = self.run([
            "log",
            "-g",
            &format!("--max-count={limit}"),
            // %gd = reflog selector, %gs = reflog subject.
            "--format=%H%x1f%h%x1f%gs%x1f%gd%x1f%an%x1f%ar",
            "-z",
        ])?;
        Ok(parse_log(&out.stdout))
    }

    /// Distinct commit authors as `Name <email>`, most-recent first — the
    /// autocomplete candidates for the `--author=` log option. Bounded so it
    /// stays cheap in large repos.
    pub fn authors(&self) -> Result<Vec<String>> {
        let out = self.run([
            "log",
            "--all",
            "--max-count=2000",
            "--format=%aN <%aE>",
            "-z",
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut seen = HashSet::new();
        Ok(text
            .split('\0')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .filter(|a| seen.insert(a.to_string()))
            .map(str::to_string)
            .collect())
    }
}

fn parse_log(stdout: &[u8]) -> Vec<LogEntry> {
    String::from_utf8_lossy(stdout)
        .split('\0')
        .filter(|r| !r.is_empty())
        .filter_map(parse_log_record)
        .collect()
}

fn parse_log_record(record: &str) -> Option<LogEntry> {
    let mut fields = record.split('\u{1f}');
    Some(LogEntry {
        hash: fields.next()?.trim().to_string(),
        short_hash: fields.next()?.trim().to_string(),
        subject: fields.next()?.to_string(),
        refs: fields.next().unwrap_or("").trim().to_string(),
        author: fields.next().unwrap_or("").to_string(),
        date: fields.next().unwrap_or("").to_string(),
    })
}

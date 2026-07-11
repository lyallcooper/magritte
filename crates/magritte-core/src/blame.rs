//! `git blame` — per-line authorship for a file, mirroring magit's blame
//! annotations (`magit-blame.el`). We read the porcelain
//! format once and return one [`BlameLine`] per line: the commit, author, date,
//! and content, ready to render as an annotated, scrollable view.

use std::collections::HashMap;

use crate::error::Result;
use crate::repo::Repo;

/// One annotated line of a blamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// Abbreviated commit that last touched the line.
    pub short: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    /// The commit's one-line summary (subject).
    pub summary: String,
    /// 1-based line number in the final file.
    pub line_no: u32,
    pub text: String,
    /// Whether this line starts a new commit group (the first of a run with the
    /// same commit) — the renderer shows the gutter only on these.
    pub group_start: bool,
}

impl Repo {
    /// Blame the working-tree contents of `path` (like plain `git blame`, no
    /// revision — uncommitted lines annotate as "Not Committed Yet"), returning
    /// one entry per line. Uses the porcelain format so author/date metadata is
    /// unambiguous and shared across a commit's line runs.
    pub fn blame(&self, path: &str) -> Result<Vec<BlameLine>> {
        let out = self.run(["blame", "--porcelain", "--", path])?;
        Ok(parse_blame(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// Per-commit metadata, cached by full sha (the porcelain format emits it once
/// per commit, then references the sha alone on that commit's later lines).
#[derive(Default, Clone)]
struct Commit {
    author: String,
    date: String,
    summary: String,
}

/// Parse `git blame --porcelain` output into per-line annotations.
fn parse_blame(text: &str) -> Vec<BlameLine> {
    let mut commits: HashMap<String, Commit> = HashMap::new();
    let mut lines = Vec::new();
    let mut cur = Commit::default();
    let mut sha = String::new();
    let mut line_no = 0u32;
    let mut author_time: Option<i64> = None;
    let mut group_start = false;

    for line in text.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            // The content line closes a group: commit the cached metadata and
            // emit the annotated line.
            if let Some(t) = author_time.take() {
                cur.date = ymd(t);
            }
            let commit = commits.entry(sha.clone()).or_insert_with(|| cur.clone());
            lines.push(BlameLine {
                short: sha.chars().take(7).collect(),
                author: commit.author.clone(),
                date: commit.date.clone(),
                summary: commit.summary.clone(),
                line_no,
                text: content.to_string(),
                group_start: std::mem::take(&mut group_start),
            });
            continue;
        }
        // A header line: `<sha> <orig> <final> [<num>]`.
        if let Some((maybe_sha, rest)) = line.split_once(' ') {
            if maybe_sha.len() == 40 && maybe_sha.chars().all(|c| c.is_ascii_hexdigit()) {
                sha = maybe_sha.to_string();
                cur = commits.get(&sha).cloned().unwrap_or_default();
                // The final line number is the second field of the header.
                let mut fields = rest.split(' ');
                line_no = fields
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(line_no);
                // The num-lines field is present only on the header that opens
                // a contiguous group — a commit whose lines appear in several
                // separate runs opens a group for each run.
                group_start = fields.next().is_some();
                author_time = None;
                continue;
            }
            match maybe_sha {
                "author" => cur.author = rest.to_string(),
                "author-time" => author_time = rest.parse().ok(),
                "summary" => cur.summary = rest.to_string(),
                _ => {}
            }
        }
    }
    lines
}

/// Format a unix timestamp as `YYYY-MM-DD` (UTC), without pulling in a date
/// crate — blame only needs the day.
fn ymd(secs: i64) -> String {
    // Days since the unix epoch → civil date (Howard Hinnant's algorithm).
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::parse_blame;

    #[test]
    fn a_commit_with_separated_runs_starts_a_group_per_run() {
        // Commit A owns lines 1 and 3, commit B line 2. The porcelain header
        // carries a num-lines field only at the start of each contiguous run,
        // so A's second run must open a new group (get its own gutter) even
        // though A's metadata was already seen.
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let text = format!(
            "{a} 1 1 1\nauthor Alice\nauthor-time 0\nsummary first\n\tline one\n\
             {b} 2 2 1\nauthor Bob\nauthor-time 0\nsummary second\n\tline two\n\
             {a} 3 3 1\n\tline three\n"
        );
        let lines = parse_blame(&text);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.group_start), "three one-line runs");
        assert_eq!(lines[2].author, "Alice", "metadata reused from the cache");

        // A two-line run: only its first line starts the group.
        let text = format!(
            "{a} 1 1 2\nauthor Alice\nauthor-time 0\nsummary first\n\tone\n\
             {a} 2 2\n\ttwo\n"
        );
        let lines = parse_blame(&text);
        assert_eq!(
            lines.iter().map(|l| l.group_start).collect::<Vec<_>>(),
            [true, false]
        );
    }
}

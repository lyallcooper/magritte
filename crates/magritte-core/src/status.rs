//! Parsing of `git status --porcelain=v2 --branch -z` into a structured model.
//!
//! Porcelain v2 is the stable, machine-readable status format. We request the
//! `-z` variant so paths are NUL-terminated and never quoted, which removes all
//! ambiguity around spaces and unusual characters in filenames.

use crate::error::{Error, Result};
use crate::repo::Repo;

/// A single-character git status code for one side (index or worktree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Unmodified,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
}

impl Change {
    fn from_code(c: u8) -> Result<Change> {
        Ok(match c {
            b'.' => Change::Unmodified,
            b'M' => Change::Modified,
            b'T' => Change::TypeChanged,
            b'A' => Change::Added,
            b'D' => Change::Deleted,
            b'R' => Change::Renamed,
            b'C' => Change::Copied,
            b'U' => Change::Unmerged,
            _ => {
                return Err(Error::Parse {
                    context: "status XY code",
                    line: (c as char).to_string(),
                })
            }
        })
    }

    pub fn is_modified(self) -> bool {
        self != Change::Unmodified
    }
}

/// What category of working-tree entry this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// An ordinary tracked change (porcelain `1` record).
    Tracked,
    /// A rename or copy (porcelain `2` record); see [`FileEntry::orig_path`].
    RenamedOrCopied,
    /// An unmerged / conflicted path (porcelain `u` record).
    Unmerged,
    /// An untracked path (porcelain `?` record).
    Untracked,
    /// An ignored path (porcelain `!` record).
    Ignored,
}

/// One path reported by `git status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    /// For renames/copies, the path the file came from.
    pub orig_path: Option<String>,
    pub kind: EntryKind,
    /// Status of the change staged in the index (the `X` column).
    pub index: Change,
    /// Status of the change in the working tree (the `Y` column).
    pub worktree: Change,
}

impl FileEntry {
    /// Whether this entry has content staged for the next commit.
    pub fn is_staged(&self) -> bool {
        matches!(self.kind, EntryKind::RenamedOrCopied) || self.index.is_modified()
    }

    /// Whether this entry has changes not yet staged.
    pub fn is_unstaged(&self) -> bool {
        matches!(
            self.kind,
            EntryKind::Untracked | EntryKind::Unmerged
        ) || self.worktree.is_modified()
    }
}

/// Branch / upstream information from the `# branch.*` headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadInfo {
    /// Current commit OID, or `None` on an unborn branch.
    pub oid: Option<String>,
    /// Current branch name, or `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub detached: bool,
}

/// The full parsed status of a working tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head: HeadInfo,
    pub entries: Vec<FileEntry>,
}

impl Status {
    pub fn staged(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|e| e.is_unstaged() && e.kind != EntryKind::Untracked)
    }

    pub fn untracked(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Untracked)
    }

    pub fn is_clean(&self) -> bool {
        self.entries
            .iter()
            .all(|e| e.kind == EntryKind::Ignored)
    }
}

impl Repo {
    /// Run `git status` and parse the porcelain-v2 output.
    pub fn status(&self) -> Result<Status> {
        let out = self.run([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
            "-z",
        ])?;
        parse_porcelain_v2(&out.stdout)
    }
}

/// Parse the raw bytes of `git status --porcelain=v2 --branch -z`.
pub fn parse_porcelain_v2(bytes: &[u8]) -> Result<Status> {
    let mut status = Status::default();
    // Records are NUL-terminated. Rename/copy (`2`) records carry an *extra*
    // NUL-delimited field (the original path), so we iterate manually rather
    // than mapping over all fields uniformly.
    let mut records = NulRecords::new(bytes);

    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        match record[0] {
            b'#' => parse_header(record, &mut status.head)?,
            b'1' => status.entries.push(parse_ordinary(record)?),
            b'2' => {
                // The original path is the *next* NUL-delimited field.
                let orig = records.next().ok_or_else(|| Error::Parse {
                    context: "rename entry missing original path",
                    line: lossy(record),
                })?;
                status.entries.push(parse_rename(record, lossy(orig))?);
            }
            b'u' => status.entries.push(parse_unmerged(record)?),
            b'?' => status.entries.push(FileEntry {
                path: lossy(&record[2..]),
                orig_path: None,
                kind: EntryKind::Untracked,
                index: Change::Unmodified,
                worktree: Change::Modified,
            }),
            b'!' => status.entries.push(FileEntry {
                path: lossy(&record[2..]),
                orig_path: None,
                kind: EntryKind::Ignored,
                index: Change::Unmodified,
                worktree: Change::Unmodified,
            }),
            _ => {
                return Err(Error::Parse {
                    context: "unknown porcelain record type",
                    line: lossy(record),
                })
            }
        }
    }

    Ok(status)
}

/// Iterator over NUL-terminated records that does not allocate per record.
struct NulRecords<'a> {
    rest: &'a [u8],
}

impl<'a> NulRecords<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        NulRecords { rest: bytes }
    }
}

impl<'a> Iterator for NulRecords<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        match self.rest.iter().position(|&b| b == 0) {
            Some(i) => {
                let record = &self.rest[..i];
                self.rest = &self.rest[i + 1..];
                Some(record)
            }
            None => {
                // Trailing record without a terminator (shouldn't happen with -z).
                let record = self.rest;
                self.rest = &[];
                Some(record)
            }
        }
    }
}

fn parse_header(record: &[u8], head: &mut HeadInfo) -> Result<()> {
    let text = std::str::from_utf8(record).map_err(|_| Error::Encoding {
        context: "branch header",
    })?;
    // Format: "# branch.<key> <value...>"
    let body = text.strip_prefix("# ").unwrap_or(text);
    let (key, value) = match body.split_once(' ') {
        Some(kv) => kv,
        None => return Ok(()),
    };
    match key {
        "branch.oid" => {
            head.oid = if value == "(initial)" {
                None
            } else {
                Some(value.to_string())
            };
        }
        "branch.head" => {
            if value == "(detached)" {
                head.detached = true;
                head.branch = None;
            } else {
                head.branch = Some(value.to_string());
            }
        }
        "branch.upstream" => head.upstream = Some(value.to_string()),
        "branch.ab" => {
            // "+<ahead> -<behind>"
            for token in value.split_whitespace() {
                if let Some(n) = token.strip_prefix('+') {
                    head.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = token.strip_prefix('-') {
                    head.behind = n.parse().unwrap_or(0);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Split a record into the leading whitespace-delimited fields and the path.
///
/// Porcelain v2 paths may contain spaces, but the fixed-width metadata fields
/// never do, so we split off exactly `n` leading fields and treat the rest
/// (after the field separator) as the path.
fn split_fields(record: &[u8], n: usize) -> Result<(Vec<&[u8]>, &[u8])> {
    let mut fields = Vec::with_capacity(n);
    let mut rest = record;
    for _ in 0..n {
        // Skip leading spaces.
        while rest.first() == Some(&b' ') {
            rest = &rest[1..];
        }
        let end = rest.iter().position(|&b| b == b' ').ok_or_else(|| {
            Error::Parse {
                context: "truncated porcelain record",
                line: lossy(record),
            }
        })?;
        fields.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    Ok((fields, rest))
}

fn parse_xy(field: &[u8]) -> Result<(Change, Change)> {
    if field.len() != 2 {
        return Err(Error::Parse {
            context: "XY field",
            line: lossy(field),
        });
    }
    Ok((Change::from_code(field[0])?, Change::from_code(field[1])?))
}

fn parse_ordinary(record: &[u8]) -> Result<FileEntry> {
    // "1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>"
    // Field 0 is the "1" record type; we split the 8 metadata fields then path.
    let (fields, path) = split_fields(record, 8)?;
    let (index, worktree) = parse_xy(fields[1])?;
    Ok(FileEntry {
        path: lossy(path),
        orig_path: None,
        kind: EntryKind::Tracked,
        index,
        worktree,
    })
}

fn parse_rename(record: &[u8], orig_path: String) -> Result<FileEntry> {
    // "2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>"
    let (fields, path) = split_fields(record, 9)?;
    let (index, worktree) = parse_xy(fields[1])?;
    Ok(FileEntry {
        path: lossy(path),
        orig_path: Some(orig_path),
        kind: EntryKind::RenamedOrCopied,
        index,
        worktree,
    })
}

fn parse_unmerged(record: &[u8]) -> Result<FileEntry> {
    // "u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>"
    let (fields, path) = split_fields(record, 10)?;
    let (index, worktree) = parse_xy(fields[1])?;
    Ok(FileEntry {
        path: lossy(path),
        orig_path: None,
        kind: EntryKind::Unmerged,
        index,
        worktree,
    })
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

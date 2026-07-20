//! Parsing of git-format unified diff output into a structured model.
//!
//! The model keeps enough information (line origins and both old/new line
//! numbers, plus the raw file header) to later reconstruct patches for
//! hunk- and line-level staging, which is why `DiffLine` records more than a
//! renderer strictly needs. The format is git's, but not git-only: other
//! tools (e.g. `jj diff --git`) emit it too.

use crate::error::{Error, Result};

/// The role of a single line within a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// The `\ No newline at end of file` marker.
    NoNewline,
}

/// What a contiguous run of changed lines does, as a gutter indicator shows
/// it. Runs are maximal stretches of non-context lines: a `-U3` hunk can
/// carry several separate changes, each classified on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineChange {
    /// The run only adds lines.
    Added,
    /// The run only removes lines.
    Removed,
    /// The run replaces lines (both removals and additions).
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line content without the leading origin character or trailing newline,
    /// decoded lossily for display (file content need not be UTF-8).
    pub content: String,
    /// The original content bytes, kept only when they aren't valid UTF-8
    /// (`content` then holds replacement characters), so reconstructed patches
    /// carry the file's real bytes instead of U+FFFD.
    pub raw: Option<Vec<u8>>,
    /// 1-based line number on the old side, if this line exists there.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side, if this line exists there.
    pub new_lineno: Option<u32>,
}

impl DiffLine {
    /// The content's original bytes, for byte-exact patch reconstruction.
    pub fn content_bytes(&self) -> &[u8] {
        self.raw.as_deref().unwrap_or(self.content.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Text after the closing `@@` (the function/section heading), trimmed.
    pub section_heading: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The new-side line number to jump to when opening this hunk: the first
    /// *changed* line (the first added line's new-side number), rather than the
    /// hunk's leading context. Falls back to `new_start` for a delete-only hunk,
    /// whose change has no new-side line.
    pub fn first_change_new_line(&self) -> u32 {
        self.lines
            .iter()
            .find(|l| l.kind == LineKind::Added)
            .and_then(|l| l.new_lineno)
            .unwrap_or(self.new_start)
    }

    /// Classify each line by the change run it belongs to: `None` for context,
    /// otherwise the run's [`LineChange`]. A `\ No newline` marker inside a run
    /// (between its removed and added halves, or trailing it) stays part of the
    /// run; one following context is context.
    pub fn line_changes(&self) -> Vec<Option<LineChange>> {
        let mut out = vec![None; self.lines.len()];
        let mut ix = 0;
        while ix < self.lines.len() {
            if !matches!(self.lines[ix].kind, LineKind::Added | LineKind::Removed) {
                ix += 1;
                continue;
            }
            let start = ix;
            let (mut added, mut removed) = (false, false);
            while ix < self.lines.len() {
                match self.lines[ix].kind {
                    LineKind::Added => added = true,
                    LineKind::Removed => removed = true,
                    LineKind::NoNewline => {}
                    LineKind::Context => break,
                }
                ix += 1;
            }
            let change = match (added, removed) {
                (true, true) => LineChange::Changed,
                (true, false) => LineChange::Added,
                _ => LineChange::Removed,
            };
            out[start..ix].fill(Some(change));
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub is_new: bool,
    pub is_deleted: bool,
    pub is_binary: bool,
    /// Header lines from `diff --git` up to (not including) the first hunk.
    /// Preserved verbatim so patches can be reconstructed for staging.
    pub header_lines: Vec<String>,
    /// The raw header bytes (newline-terminated), kept only when some header
    /// line isn't valid UTF-8 (a non-UTF-8 path under `core.quotepath=false`),
    /// so reconstructed patches keep the original path bytes.
    pub header_raw: Option<Vec<u8>>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path to show in the UI (the new path, except for deletions).
    pub fn display_path(&self) -> &str {
        if self.is_deleted {
            &self.old_path
        } else {
            &self.new_path
        }
    }
}

/// One line of raw diff output paired with its (lossy) UTF-8 decoding. Parsing
/// works on the decoded text; the raw bytes are kept so patch reconstruction
/// can round-trip non-UTF-8 file content.
struct Line<'a> {
    raw: &'a [u8],
    text: std::borrow::Cow<'a, str>,
}

impl Line<'_> {
    /// Whether decoding lost bytes (the raw line isn't valid UTF-8).
    fn lossy(&self) -> bool {
        matches!(self.text, std::borrow::Cow::Owned(_))
    }
}

/// Parse git-format unified diff output into zero or more file diffs. Content
/// is decoded lossily for display, but the original bytes of any non-UTF-8
/// line are preserved for patch reconstruction.
pub fn parse_diff(bytes: &[u8]) -> Result<Vec<FileDiff>> {
    let mut files = Vec::new();
    // Split on '\n' manually rather than a lines iterator that strips a
    // trailing '\r', which would silently drop the carriage return from the
    // content of CRLF files and corrupt reconstructed patches. We trim a single
    // trailing newline first so we don't emit a spurious empty final line.
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let mut lines = body
        .split(|&b| b == b'\n')
        .map(|raw| Line {
            raw,
            text: String::from_utf8_lossy(raw),
        })
        .peekable();

    while let Some(line) = lines.peek() {
        if line.text.starts_with("diff --git ") {
            files.push(parse_file(&mut lines)?);
        } else {
            // Skip anything that isn't an ordinary file record — including a
            // whole `diff --cc` (combined, conflicted-merge) record, whose
            // `@@@` hunks this parser doesn't model.
            lines.next();
        }
    }
    Ok(files)
}

/// Whether `line` starts the next file record (ordinary or combined) — the
/// boundary every per-file/per-hunk loop stops at.
fn is_file_boundary(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("diff --cc ")
}

fn parse_file<'a, I>(lines: &mut std::iter::Peekable<I>) -> Result<FileDiff>
where
    I: Iterator<Item = Line<'a>>,
{
    let mut file = FileDiff::default();
    let mut header_raw = Vec::new();
    let mut header_lossy = false;
    let header = lines.next().expect("caller verified diff --git line");
    header_raw.extend_from_slice(header.raw);
    header_raw.push(b'\n');
    header_lossy |= header.lossy();
    file.header_lines.push(header.text.clone().into_owned());
    // Provisional paths from the `diff --git a/<x> b/<y>` line; refined below by
    // the more reliable `---`/`+++`/`rename` lines.
    if let Some((old, new)) = split_diff_git_paths(&header.text) {
        file.old_path = old;
        file.new_path = new;
    }

    // Extended header lines, until the first hunk or the next file.
    while let Some(line) = lines.peek() {
        if line.text.starts_with("@@") || is_file_boundary(&line.text) {
            break;
        }
        let line = lines.next().unwrap();
        header_raw.extend_from_slice(line.raw);
        header_raw.push(b'\n');
        header_lossy |= line.lossy();
        let text = line.text.as_ref();
        file.header_lines.push(text.to_string());

        if text.starts_with("new file mode ") {
            file.is_new = true;
        } else if text.starts_with("deleted file mode ") {
            file.is_deleted = true;
        } else if let Some(path) = text.strip_prefix("rename from ") {
            file.old_path = unquote_path(path);
        } else if let Some(path) = text.strip_prefix("rename to ") {
            file.new_path = unquote_path(path);
        } else if text.starts_with("Binary files ") || text.starts_with("GIT binary patch") {
            file.is_binary = true;
        } else if let Some(path) = text.strip_prefix("--- ") {
            if let Some(p) = strip_diff_path(path) {
                file.old_path = p;
            }
        } else if let Some(path) = text.strip_prefix("+++ ") {
            if let Some(p) = strip_diff_path(path) {
                file.new_path = p;
            }
        }
    }
    file.header_raw = header_lossy.then_some(header_raw);

    // Hunks.
    while let Some(line) = lines.peek() {
        if is_file_boundary(&line.text) {
            break;
        } else if line.text.starts_with("@@") {
            file.hunks.push(parse_hunk(lines)?);
        } else {
            // Stray line between hunks (shouldn't happen); skip defensively.
            lines.next();
        }
    }

    Ok(file)
}

fn parse_hunk<'a, I>(lines: &mut std::iter::Peekable<I>) -> Result<Hunk>
where
    I: Iterator<Item = Line<'a>>,
{
    let header = lines.next().expect("caller verified @@ line");
    let (old_start, old_count, new_start, new_count, section_heading) =
        parse_hunk_header(&header.text)?;

    let mut hunk = Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section_heading,
        lines: Vec::new(),
    };

    let mut old_no = old_start;
    let mut new_no = new_start;

    while let Some(line) = lines.peek() {
        // A hunk ends at the next hunk, the next file, or end of input.
        if line.text.starts_with("@@") || is_file_boundary(&line.text) {
            break;
        }
        let line = lines.next().unwrap();
        // The origin character is always ASCII, so it's safe to test (and
        // strip) on the raw bytes even when the content isn't UTF-8.
        let (kind, skip) = match line.raw.first() {
            Some(b' ') => (LineKind::Context, 1),
            Some(b'+') => (LineKind::Added, 1),
            Some(b'-') => (LineKind::Removed, 1),
            Some(b'\\') => (LineKind::NoNewline, 0), // "\ No newline at end of file"
            // An empty line inside a hunk represents a blank context line.
            None => (LineKind::Context, 0),
            _ => {
                return Err(Error::Parse {
                    context: "diff hunk line",
                    line: line.text.into_owned(),
                })
            }
        };
        let raw_content = &line.raw[skip..];
        // Lossy decoding for display; keep the original bytes only when they
        // aren't valid UTF-8, so patches can be rebuilt byte-exactly.
        let (content, raw) = match std::str::from_utf8(raw_content) {
            Ok(s) => (s.to_string(), None),
            Err(_) => (
                String::from_utf8_lossy(raw_content).into_owned(),
                Some(raw_content.to_vec()),
            ),
        };

        let (old_lineno, new_lineno) = match kind {
            LineKind::Context => {
                let o = old_no;
                let n = new_no;
                old_no += 1;
                new_no += 1;
                (Some(o), Some(n))
            }
            LineKind::Added => {
                let n = new_no;
                new_no += 1;
                (None, Some(n))
            }
            LineKind::Removed => {
                let o = old_no;
                old_no += 1;
                (Some(o), None)
            }
            LineKind::NoNewline => (None, None),
        };

        hunk.lines.push(DiffLine {
            kind,
            content,
            raw,
            old_lineno,
            new_lineno,
        });
    }

    Ok(hunk)
}

/// Parse `@@ -old[,n] +new[,n] @@[ heading]`.
fn parse_hunk_header(line: &str) -> Result<(u32, u32, u32, u32, String)> {
    let err = || Error::Parse {
        context: "hunk header",
        line: line.to_string(),
    };
    // Split into ["", " -a,b +c,d ", " heading"].
    let mut parts = line.splitn(3, "@@");
    parts.next().ok_or_else(err)?; // leading ""
    let ranges = parts.next().ok_or_else(err)?.trim();
    let heading = parts.next().unwrap_or("").trim().to_string();

    let mut range_iter = ranges.split_whitespace();
    let old = range_iter.next().ok_or_else(err)?;
    let new = range_iter.next().ok_or_else(err)?;

    let (old_start, old_count) = parse_range(old.strip_prefix('-').ok_or_else(err)?)?;
    let (new_start, new_count) = parse_range(new.strip_prefix('+').ok_or_else(err)?)?;

    Ok((old_start, old_count, new_start, new_count, heading))
}

/// Parse `start[,count]`; count defaults to 1 when omitted.
fn parse_range(s: &str) -> Result<(u32, u32)> {
    let err = || Error::Parse {
        context: "hunk range",
        line: s.to_string(),
    };
    let mut it = s.splitn(2, ',');
    let start: u32 = it.next().ok_or_else(err)?.parse().map_err(|_| err())?;
    let count: u32 = match it.next() {
        Some(c) => c.parse().map_err(|_| err())?,
        None => 1,
    };
    Ok((start, count))
}

/// Split the `diff --git a/<x> b/<y>` line into (old, new). Best-effort: paths
/// with spaces are ambiguous here, so the `---`/`+++`/`rename` lines are the
/// authoritative source and override this.
fn split_diff_git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    // A path with quote/backslash/control characters is C-quoted whole
    // (`"a/we\tird"`), which is unambiguous — parse the two quoted strings.
    if rest.starts_with('"') {
        let (old, rest) = take_c_quoted(rest)?;
        let (new, _) = take_c_quoted(rest.trim_start())?;
        return Some((strip_prefix_dir(&old), strip_prefix_dir(&new)));
    }
    let a_pos = rest.find("a/")?;
    let b_pos = rest.rfind(" b/")?;
    let old = &rest[a_pos + 2..b_pos];
    let new = &rest[b_pos + 3..];
    Some((old.to_string(), new.to_string()))
}

/// Strip the `a/` or `b/` prefix from a `---`/`+++` path, mapping `/dev/null`
/// to an empty string.
fn strip_diff_path(path: &str) -> Option<String> {
    // git appends a tab after a path containing spaces (never other trailing
    // whitespace, which can legitimately be part of a filename).
    let path = path.strip_suffix('\t').unwrap_or(path);
    if path == "/dev/null" {
        return Some(String::new());
    }
    Some(strip_prefix_dir(&unquote_path(path)))
}

/// Strip a diff prefix directory (`a/<p>`, `b/<p>`; tolerate git's mnemonic
/// `i/`,`w/`,`c/`,`o/` in case a caller diffs without --default-prefix).
fn strip_prefix_dir(path: &str) -> String {
    ["a/", "b/", "i/", "w/", "c/", "o/"]
        .iter()
        .find_map(|p| path.strip_prefix(p))
        .unwrap_or(path)
        .to_string()
}

/// Undo git's C-style quoting if `path` is quoted, else return it as-is. Even
/// with `core.quotepath=false` (which stops quoting of non-ASCII), git still
/// quotes paths containing quotes, backslashes, or control characters on the
/// `diff --git`, `---`/`+++`, and `rename from/to` lines (and in `--numstat`
/// output, which callers unquote with this too).
pub fn unquote_path(path: &str) -> String {
    match take_c_quoted(path) {
        Some((unquoted, _)) => unquoted,
        None => path.to_string(),
    }
}

/// Parse one C-quoted string at the start of `s`, returning it unescaped plus
/// the remainder after the closing quote. `None` if `s` isn't quoted (or the
/// quoting is malformed).
fn take_c_quoted(s: &str) -> Option<(String, &str)> {
    let inner = s.strip_prefix('"')?;
    let mut bytes = Vec::new();
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                let out = String::from_utf8_lossy(&bytes).into_owned();
                return Some((out, &inner[i + 1..]));
            }
            '\\' => {
                let (_, esc) = chars.next()?;
                match esc {
                    'n' => bytes.push(b'\n'),
                    't' => bytes.push(b'\t'),
                    'r' => bytes.push(b'\r'),
                    'a' => bytes.push(0x07),
                    'b' => bytes.push(0x08),
                    'f' => bytes.push(0x0c),
                    'v' => bytes.push(0x0b),
                    '\\' | '"' => bytes.push(esc as u8),
                    // Octal escape: exactly three digits per git's quoting.
                    '0'..='7' => {
                        let mut val = esc.to_digit(8)?;
                        for _ in 0..2 {
                            let (_, d) = chars.next()?;
                            val = val * 8 + d.to_digit(8)?;
                        }
                        bytes.push(val as u8);
                    }
                    _ => return None,
                }
            }
            _ => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    None
}

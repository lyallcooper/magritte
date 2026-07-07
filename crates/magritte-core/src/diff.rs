//! Parsing of `git diff` unified output into a structured model.
//!
//! The model keeps enough information (line origins and both old/new line
//! numbers, plus the raw file header) to later reconstruct patches for
//! hunk- and line-level staging, which is why `DiffLine` records more than a
//! renderer strictly needs.

use crate::error::{Error, Result};
use crate::repo::Repo;

/// Which view of the changes to diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffSource {
    /// Working tree vs. index (`git diff`).
    Unstaged,
    /// Index vs. HEAD (`git diff --cached`).
    Staged,
}

/// The role of a single line within a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// The `\ No newline at end of file` marker.
    NoNewline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line content without the leading origin character or trailing newline.
    pub content: String,
    /// 1-based line number on the old side, if this line exists there.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side, if this line exists there.
    pub new_lineno: Option<u32>,
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
        parse_diff(&out.stdout)
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
    fn diff_source_args(&self, source: DiffSource) -> Vec<String> {
        let mut args = self.diff_base(&[]);
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
        let mut diffs = self.diff_with(self.diff_source_args(source), &paths)?;
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
        self.diff_with(self.diff_source_args(source), &[])
    }

    /// Every tracked change vs. HEAD (`git diff HEAD`): staged and unstaged
    /// modifications/deletions combined, excluding untracked files. This is
    /// exactly the tree `git commit --all` records, so it's the preview for an
    /// all-commit (where the staged-only diff would hide tracked unstaged work).
    /// On an unborn branch there is no HEAD (so `git diff HEAD` would error) and
    /// nothing is tracked yet, so the staged diff is the whole story.
    pub fn diff_tracked_vs_head(&self) -> Result<Vec<FileDiff>> {
        if !self.succeeds(["rev-parse", "--verify", "--quiet", "HEAD"])? {
            return self.diff_all(DiffSource::Staged);
        }
        let mut args = self.diff_base(&[]);
        args.push("HEAD".to_string());
        self.diff_with(args, &[])
    }

    /// The standalone diff transient's unstaged action (`git diff [args]`).
    pub fn diff_unstaged(&self, extra: &[String], paths: &[String]) -> Result<Vec<FileDiff>> {
        self.diff_with(self.diff_base(extra), paths)
    }

    /// The standalone diff transient's staged action (`git diff --cached [args]`).
    pub fn diff_staged(&self, extra: &[String], paths: &[String]) -> Result<Vec<FileDiff>> {
        let mut args = self.diff_base(extra);
        args.push("--cached".to_string());
        self.diff_with(args, paths)
    }

    /// The whole working tree against a revision (Magit's `Diff worktree`,
    /// defaulting to `HEAD`): staged + unstaged tracked changes.
    pub fn diff_worktree(
        &self,
        rev: &str,
        extra: &[String],
        paths: &[String],
    ) -> Result<Vec<FileDiff>> {
        let mut args = self.diff_base(extra);
        args.push(rev.to_string());
        self.diff_with(args, paths)
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
        let base = if self.succeeds(["rev-parse", "--verify", "--quiet", &parent])? {
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

/// Parse the (UTF-8 lossy) output of `git diff` into zero or more file diffs.
pub fn parse_diff(bytes: &[u8]) -> Result<Vec<FileDiff>> {
    let text = String::from_utf8_lossy(bytes);
    let mut files = Vec::new();
    // Split on '\n' manually rather than `str::lines()`: `lines()` strips a
    // trailing '\r', which would silently drop the carriage return from the
    // content of CRLF files and corrupt reconstructed patches. We trim a single
    // trailing newline first so we don't emit a spurious empty final line.
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let mut lines = body.split('\n').peekable();

    while let Some(&line) = lines.peek() {
        if line.starts_with("diff --git ") {
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
    I: Iterator<Item = &'a str>,
{
    let mut file = FileDiff::default();
    let header = lines.next().expect("caller verified diff --git line");
    file.header_lines.push(header.to_string());
    // Provisional paths from the `diff --git a/<x> b/<y>` line; refined below by
    // the more reliable `---`/`+++`/`rename` lines.
    if let Some((old, new)) = split_diff_git_paths(header) {
        file.old_path = old;
        file.new_path = new;
    }

    // Extended header lines, until the first hunk or the next file.
    while let Some(&line) = lines.peek() {
        if line.starts_with("@@") || is_file_boundary(line) {
            break;
        }
        let line = lines.next().unwrap();
        file.header_lines.push(line.to_string());

        if line.starts_with("new file mode ") {
            file.is_new = true;
        } else if line.starts_with("deleted file mode ") {
            file.is_deleted = true;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = unquote_path(path);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.new_path = unquote_path(path);
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.is_binary = true;
        } else if let Some(path) = line.strip_prefix("--- ") {
            if let Some(p) = strip_diff_path(path) {
                file.old_path = p;
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(p) = strip_diff_path(path) {
                file.new_path = p;
            }
        }
    }

    // Hunks.
    while let Some(&line) = lines.peek() {
        if is_file_boundary(line) {
            break;
        } else if line.starts_with("@@") {
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
    I: Iterator<Item = &'a str>,
{
    let header = lines.next().expect("caller verified @@ line");
    let (old_start, old_count, new_start, new_count, section_heading) = parse_hunk_header(header)?;

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

    while let Some(&line) = lines.peek() {
        // A hunk ends at the next hunk, the next file, or end of input.
        if line.starts_with("@@") || is_file_boundary(line) {
            break;
        }
        let line = lines.next().unwrap();
        let (kind, content) = match line.as_bytes().first() {
            Some(b' ') => (LineKind::Context, &line[1..]),
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b'\\') => (LineKind::NoNewline, line), // "\ No newline at end of file"
            // An empty line inside a hunk represents a blank context line.
            None => (LineKind::Context, line),
            _ => {
                return Err(Error::Parse {
                    context: "diff hunk line",
                    line: line.to_string(),
                })
            }
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
            content: content.to_string(),
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
/// `diff --git`, `---`/`+++`, and `rename from/to` lines.
fn unquote_path(path: &str) -> String {
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

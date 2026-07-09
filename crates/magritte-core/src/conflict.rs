//! Merge-conflict handling: resolving a file by taking one side wholesale
//! (magit's `magit-checkout-stage`), plus a bytes-preserving parser/resolver
//! over the standard conflict markers for per-conflict resolution (the app's
//! smerge-style resolve view).

use crate::error::{Error, Result};
use crate::repo::Repo;

/// Which side of a conflict to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    /// HEAD's version (`--ours`).
    Ours,
    /// The incoming version (`--theirs`).
    Theirs,
}

impl Repo {
    /// Resolve `path` by keeping one side: `git checkout --ours|--theirs -- path`
    /// then `git add -- path` to mark it resolved.
    pub fn resolve_conflict(&self, path: &str, side: ConflictSide) -> Result<()> {
        let flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
        };
        self.run(["checkout", flag, "--", path])?;
        self.run(["add", "--", path])?;
        Ok(())
    }

    /// Read the worktree file at repo-relative `path` as raw bytes.
    pub fn read_worktree_file(&self, path: &str) -> Result<Vec<u8>> {
        std::fs::read(self.workdir().join(path))
            .map_err(|e| Error::Message(format!("failed to read {path}: {e}")))
    }

    /// Atomically replace the worktree file at repo-relative `path` with
    /// `bytes`: write a temp file in the same directory (keeping the original
    /// permissions, e.g. an executable bit), then rename over the target.
    pub fn write_worktree_file(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let full = self.workdir().join(path);
        let dir = full
            .parent()
            .ok_or_else(|| Error::Message(format!("no parent directory for {path}")))?;
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let tmp = dir.join(format!(".{name}.magritte-tmp-{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            std::fs::write(&tmp, bytes)?;
            if let Ok(meta) = std::fs::metadata(&full) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            std::fs::rename(&tmp, &full)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Error::Message(format!("failed to write {path}: {e}"))
        })
    }
}

/// One piece of a conflicted file: verbatim bytes between conflicts, or one
/// conflict. Concatenating the segments (with every conflict unresolved)
/// reproduces the file byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(Vec<u8>),
    Conflict(Conflict),
}

/// One parsed conflict: the bytes of each side (lines with their original
/// endings), the labels after the markers, and the conflict's raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub ours: Vec<u8>,
    /// The merge base's version, present with `merge.conflictStyle=diff3`.
    pub base: Option<Vec<u8>>,
    pub theirs: Vec<u8>,
    /// The text after `<<<<<<< ` (e.g. `HEAD`).
    pub ours_label: String,
    /// The text after `>>>>>>> ` (e.g. the incoming branch).
    pub theirs_label: String,
    /// The text after `||||||| ` (diff3 only).
    pub base_label: Option<String>,
    /// The conflict's original bytes, markers included. An unresolved conflict
    /// re-emits this verbatim, so a parse → resolve round-trip is
    /// byte-identical even when the marker lines carry endings (CRLF) the
    /// labels alone couldn't reconstruct.
    pub raw: Vec<u8>,
}

/// How to resolve one conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Keep our side (smerge-keep-upper).
    Ours,
    /// Keep their side (smerge-keep-lower).
    Theirs,
    /// Keep both, ours then theirs (smerge-keep-all's order).
    Both,
    /// Keep the merge base (diff3 only; smerge-keep-base).
    Base,
}

/// The label after a run of exactly seven `sigil` bytes at the start of
/// `line`, followed by a space — git's marker format (`<<<<<<< HEAD`). `None`
/// when the line isn't that marker (including a run of any other length).
fn marker_label(line: &[u8], sigil: u8) -> Option<String> {
    let sigils = line.iter().take_while(|&&b| b == sigil).count();
    if sigils != 7 || line.get(7) != Some(&b' ') {
        return None;
    }
    Some(String::from_utf8_lossy(strip_eol(&line[8..])).into_owned())
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn is_separator(line: &[u8]) -> bool {
    strip_eol(line) == b"======="
}

/// Which region of a conflict the parser is inside.
enum ConflictState {
    Ours,
    Base,
    Theirs,
}

/// A conflict being assembled, with its raw bytes so an abandoned (malformed)
/// one can be flushed back as plain text verbatim.
struct PendingConflict {
    raw: Vec<u8>,
    ours: Vec<u8>,
    base: Option<Vec<u8>>,
    theirs: Vec<u8>,
    ours_label: String,
    base_label: Option<String>,
    state: ConflictState,
}

/// Split `content` on the standard git conflict markers (`<<<<<<< `,
/// `||||||| ` diff3 base, `=======`, `>>>>>>> `, each at line start, marker
/// length exactly seven). Non-conflict bytes pass through verbatim in `Text`
/// segments; malformed or nested markers are treated as plain text rather
/// than an error, so any input parses.
pub fn parse_conflicts(content: &[u8]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut text: Vec<u8> = Vec::new();
    let mut pending: Option<PendingConflict> = None;

    for line in content.split_inclusive(|&b| b == b'\n') {
        // A start marker begins a conflict; inside one it means the previous
        // start was unmatched — flush the buffered lines back as text.
        if let Some(label) = marker_label(line, b'<') {
            if let Some(p) = pending.take() {
                text.extend_from_slice(&p.raw);
            }
            pending = Some(PendingConflict {
                raw: line.to_vec(),
                ours: Vec::new(),
                base: None,
                theirs: Vec::new(),
                ours_label: label,
                base_label: None,
                state: ConflictState::Ours,
            });
            continue;
        }
        let Some(mut p) = pending.take() else {
            // Outside any conflict, every line — including a stray `=======`
            // or `>>>>>>> ` with no opening marker — is plain text.
            text.extend_from_slice(line);
            continue;
        };
        p.raw.extend_from_slice(line);
        match p.state {
            ConflictState::Ours => {
                if let Some(label) = marker_label(line, b'|') {
                    p.base = Some(Vec::new());
                    p.base_label = Some(label);
                    p.state = ConflictState::Base;
                } else if is_separator(line) {
                    p.state = ConflictState::Theirs;
                } else if marker_label(line, b'>').is_some() {
                    // An end marker with no separator: malformed — plain text.
                    text.extend_from_slice(&p.raw);
                    continue;
                } else {
                    p.ours.extend_from_slice(line);
                }
            }
            ConflictState::Base => {
                if is_separator(line) {
                    p.state = ConflictState::Theirs;
                } else if marker_label(line, b'>').is_some() {
                    text.extend_from_slice(&p.raw);
                    continue;
                } else {
                    p.base
                        .as_mut()
                        .expect("base buffer")
                        .extend_from_slice(line);
                }
            }
            ConflictState::Theirs => {
                if let Some(theirs_label) = marker_label(line, b'>') {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    segments.push(Segment::Conflict(Conflict {
                        ours: p.ours,
                        base: p.base,
                        theirs: p.theirs,
                        ours_label: p.ours_label,
                        theirs_label,
                        base_label: p.base_label,
                        raw: p.raw,
                    }));
                    continue;
                }
                p.theirs.extend_from_slice(line);
            }
        }
        pending = Some(p);
    }
    // EOF inside a conflict: the markers never closed — plain text.
    if let Some(p) = pending {
        text.extend_from_slice(&p.raw);
    }
    if !text.is_empty() {
        segments.push(Segment::Text(text));
    }
    segments
}

/// Reassemble a file from its segments, replacing each conflict with its
/// chosen side's bytes. `choices` is indexed by conflict order; a missing or
/// `None` choice (and a `Base` choice on a conflict with no base) re-emits the
/// conflict's original markers verbatim.
pub fn resolve(segments: &[Segment], choices: &[Option<Resolution>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ix = 0;
    for segment in segments {
        match segment {
            Segment::Text(bytes) => out.extend_from_slice(bytes),
            Segment::Conflict(c) => {
                match choices.get(ix).copied().flatten() {
                    Some(Resolution::Ours) => out.extend_from_slice(&c.ours),
                    Some(Resolution::Theirs) => out.extend_from_slice(&c.theirs),
                    Some(Resolution::Both) => {
                        out.extend_from_slice(&c.ours);
                        out.extend_from_slice(&c.theirs);
                    }
                    Some(Resolution::Base) => match &c.base {
                        Some(base) => out.extend_from_slice(base),
                        None => out.extend_from_slice(&c.raw),
                    },
                    None => out.extend_from_slice(&c.raw),
                }
                ix += 1;
            }
        }
    }
    out
}

//! Building the status screen's flat row list: the section model (which
//! sections exist, in configured order) and rebuild_rows, which flattens the
//! parsed status + fold state + loaded diffs into [`Row`]s, plus the small
//! row/text constructors the builders and copy paths share.

use std::collections::HashSet;

use gpui::Hsla;
use magritte_core::{CommitMetadata, DiffSource, EntryKind, LogEntry, Stash, Status};

use crate::*;

/// The commit/stash listings for the non-file status sections, refreshed off
/// the UI thread (cheap `git log`/`stash list`). Empty lists (e.g. no upstream)
/// simply render no section.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusSections {
    /// Commits on HEAD not yet on the upstream.
    pub(crate) unpushed: Vec<LogEntry>,
    /// Commits on the upstream not yet pulled into HEAD.
    pub(crate) unpulled: Vec<LogEntry>,
    /// The triangular-workflow counterparts, vs the push target (empty unless a
    /// distinct push target is configured).
    pub(crate) unpushed_pushremote: Vec<LogEntry>,
    pub(crate) unpulled_pushremote: Vec<LogEntry>,
    /// The most recent commits (count from `[status].recent_count`).
    pub(crate) recent: Vec<LogEntry>,
    pub(crate) stashes: Vec<Stash>,
    /// Ignored file paths — fetched only when the `ignored` section is enabled.
    pub(crate) ignored: Vec<String>,
}

/// Which top-level section a row belongs to. Used as a stable fold key. The file
/// sections (Untracked/Unstaged/Staged) carry staging; the commit/stash sections
/// are read-only listings with act-at-point (open/yank/apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SectionId {
    Untracked,
    Unstaged,
    Staged,
    Stashes,
    Unpushed,
    Unpulled,
    /// Unpushed to / unpulled from the *push* target (triangular workflows).
    UnpushedPushremote,
    UnpulledPushremote,
    Recent,
    Ignored,
}

impl SectionId {
    /// Every section, in enum order — the source of truth for "all sections",
    /// used to seed the default-expanded set and resolve config ids.
    pub(crate) const ALL: [SectionId; 10] = [
        SectionId::Untracked,
        SectionId::Unstaged,
        SectionId::Staged,
        SectionId::Stashes,
        SectionId::Unpushed,
        SectionId::Unpulled,
        SectionId::UnpushedPushremote,
        SectionId::UnpulledPushremote,
        SectionId::Recent,
        SectionId::Ignored,
    ];

    /// The config id (`[status].sections` entry) for this section.
    pub(crate) fn config_id(self) -> &'static str {
        match self {
            SectionId::Untracked => "untracked",
            SectionId::Unstaged => "unstaged",
            SectionId::Staged => "staged",
            SectionId::Stashes => "stashes",
            SectionId::Unpushed => "unpushed",
            SectionId::Unpulled => "unpulled",
            SectionId::UnpushedPushremote => "unpushed-pushremote",
            SectionId::UnpulledPushremote => "unpulled-pushremote",
            SectionId::Recent => "recent",
            SectionId::Ignored => "ignored",
        }
    }

    /// The section for a config id, or `None` if unknown.
    pub(crate) fn from_config_id(id: &str) -> Option<SectionId> {
        SectionId::ALL.into_iter().find(|s| s.config_id() == id)
    }
}

pub(crate) fn plain(text: impl Into<String>, color: Hsla) -> Row {
    Row {
        indent: 0,
        selectable: true,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: text.into(),
            color,
        },
    }
}

/// The plain text of a row, for copying. A diff line yields its content without
/// the `+`/`-` sigil (so pasted code is clean); a file row joins its status word
/// and path.
pub(crate) fn row_text(row: &Row) -> String {
    match &row.kind {
        RowKind::Plain { text, .. } => text.clone(),
        RowKind::Section { title, .. } => title.clone(),
        RowKind::File { status, label, .. } => {
            if status.is_empty() {
                label.clone()
            } else {
                format!("{status}  {label}")
            }
        }
        RowKind::HunkHeader { text, .. } => text.clone(),
        RowKind::Diff { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        RowKind::Commit {
            short_hash,
            subject,
            ..
        } => format!("{short_hash}  {subject}"),
        RowKind::Stash { reference, message } => format!("{reference}  {message}"),
    }
}

/// How a `%D` ref decoration entry is classified, for coloring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RefKind {
    /// The current branch (`HEAD -> main`) or a detached `HEAD`.
    Head,
    Local,
    Remote,
    Tag,
    /// The current branch folded together with its matching upstream ref (both
    /// on this commit): the label is the full `remote/branch`, rendered with the
    /// remote prefix in the remote color and the branch in the current-branch
    /// color — magit's combined display.
    SyncedHead,
}

/// Classify one `%D` decoration entry (e.g. `origin/main`, `tag: v1`).
fn classify_ref(entry: &str) -> (String, RefKind) {
    if let Some(tag) = entry.strip_prefix("tag: ") {
        (tag.to_string(), RefKind::Tag)
    } else if let Some(branch) = entry.strip_prefix("HEAD -> ") {
        (branch.to_string(), RefKind::Head)
    } else if entry == "HEAD" {
        ("HEAD".to_string(), RefKind::Head)
    } else if entry.contains('/') {
        (entry.to_string(), RefKind::Remote)
    } else {
        (entry.to_string(), RefKind::Local)
    }
}

/// Parse a commit's `%D` decoration (e.g. `HEAD -> main, origin/main, tag: v1`)
/// into labeled, classified entries for rendering. `upstream` is the current
/// branch's upstream ref (e.g. `origin/main`), used to fold the current branch
/// and its upstream into one entry when both decorate this commit. Remote
/// `*/HEAD` pointers are dropped (magit hides them).
pub(crate) fn parse_refs(refs: &str, upstream: Option<&str>) -> Vec<(String, RefKind)> {
    let classified: Vec<(String, RefKind)> = refs
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(classify_ref)
        .filter(|(name, kind)| {
            !(*kind == RefKind::Remote && name.rsplit('/').next() == Some("HEAD"))
        })
        .collect();

    // Fold the current branch + its upstream (`main` and `origin/main`) into a
    // single synced entry when both are present on this commit.
    let synced = upstream.and_then(|u| u.rsplit_once('/').map(|(_, b)| (u, b)));
    let fold = synced.is_some_and(|(u, b)| {
        classified
            .iter()
            .any(|(n, k)| *k == RefKind::Head && n == b)
            && classified
                .iter()
                .any(|(n, k)| *k == RefKind::Remote && n == u)
    });
    if !fold {
        return classified;
    }
    let (u, b) = synced.unwrap();
    classified
        .into_iter()
        .filter_map(|(name, kind)| match kind {
            RefKind::Head if name == b => Some((u.to_string(), RefKind::SyncedHead)),
            RefKind::Remote if name == u => None, // folded into the synced entry
            _ => Some((name, kind)),
        })
        .collect()
}

/// The plain text of a commit-view row, for copying (diff line content without
/// the `+`/`-` sigil).
pub(crate) fn commit_row_text(row: &CommitDiffRow) -> String {
    match row {
        CommitDiffRow::DetailsHeader => "Details".to_string(),
        CommitDiffRow::Detail(d) => d.clone(),
        CommitDiffRow::Message(m) => m.clone(),
        CommitDiffRow::Stats {
            files,
            insertions,
            deletions,
        } => diffstat_text(*files, *insertions, *deletions),
        CommitDiffRow::StatLine {
            path,
            added,
            removed,
        } => {
            let (plus, minus) = stat_bar(*added, *removed);
            format!(
                "{path} {} {}{}",
                added + removed,
                "+".repeat(plus),
                "-".repeat(minus)
            )
        }
        CommitDiffRow::File { change, path } => {
            let word = status_label::change_word(*change);
            if word.is_empty() {
                path.clone()
            } else {
                format!("{word} {path}")
            }
        }
        CommitDiffRow::Hunk(h) => h.clone(),
        CommitDiffRow::Line { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        CommitDiffRow::Note(n) => n.clone(),
    }
}

/// The number of `+`/`-` marks for a per-file stat bar (git's `N ++--`), scaling
/// the added/removed counts down to at most `MAX` total marks while keeping each
/// nonzero side at least one mark. Small diffs render their exact counts.
pub(crate) fn stat_bar(added: usize, removed: usize) -> (usize, usize) {
    const MAX: usize = 20;
    let total = added + removed;
    if total == 0 || total <= MAX {
        return (added, removed);
    }
    let plus = if added == 0 {
        0
    } else {
        (((added as f64 / total as f64) * MAX as f64).round() as usize)
            .clamp(1, if removed > 0 { MAX - 1 } else { MAX })
    };
    let minus = if removed == 0 { 0 } else { (MAX - plus).max(1) };
    (plus, minus)
}

/// The diffstat summary text ("N files changed, K insertions(+), L
/// deletions(-)"), pluralized and omitting a zero side, like git.
pub(crate) fn diffstat_text(files: usize, insertions: usize, deletions: usize) -> String {
    let plural = |n: usize, s: &str| format!("{n} {s}{}", if n == 1 { "" } else { "s" });
    let mut parts = vec![format!("{} changed", plural(files, "file"))];
    if insertions > 0 {
        parts.push(format!("{}(+)", plural(insertions, "insertion")));
    }
    if deletions > 0 {
        parts.push(format!("{}(-)", plural(deletions, "deletion")));
    }
    parts.join(", ")
}

pub(crate) fn commit_metadata_lines(metadata: &CommitMetadata) -> Vec<String> {
    let mut lines = vec![
        format!("Author:    {}", metadata.author),
        format!("AuthorDate: {}", metadata.author_date),
        format!("Commit:    {}", metadata.committer),
        format!("CommitDate: {}", metadata.committer_date),
    ];
    if !metadata.refs.is_empty() {
        lines.push(format!("Refs:      {}", metadata.refs));
    }
    lines
}

pub(crate) fn message(text: &str, color: Hsla) -> Row {
    Row {
        indent: 2,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: text.to_string(),
            color,
        },
    }
}

pub(crate) fn spacer() -> Row {
    Row {
        indent: 0,
        selectable: false,
        fold: None,
        target: None,
        kind: RowKind::Plain {
            text: String::new(),
            color: gpui::transparent_black(),
        },
    }
}

pub(crate) fn chevron(expanded: bool, color: Hsla) -> gpui_component::Icon {
    let name = if expanded {
        gpui_component::IconName::ChevronDown
    } else {
        gpui_component::IconName::ChevronRight
    };
    gpui_component::Icon::new(name)
        .size(px(14.0))
        // A row's leading gutter must never shrink: flex items default to
        // flex-shrink 1, so on a row wide enough to overflow, a shrinking
        // chevron/spacer pulls everything after it left and breaks alignment.
        .flex_shrink_0()
        .text_color(color)
}

/// The unmerged (conflicted) paths in `status`. Cached on the view by
/// [`StatusView::rebuild_rows`] so is_conflicted (called per clickable row in
/// render) is an O(1) lookup, not an O(entries) scan per row.
pub(crate) fn conflicted_paths(status: &Status) -> HashSet<String> {
    status
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Unmerged)
        .map(|e| e.path.clone())
        .collect()
}

impl StatusView {
    pub(crate) fn rebuild_rows(&mut self) {
        self.conflicted = self
            .status
            .as_ref()
            .map(conflicted_paths)
            .unwrap_or_default();

        self.rows = if let Some(error) = &self.error {
            vec![plain(format!("Error: {error}"), self.palette.removed)]
        } else if let Some(status) = &self.status {
            StatusRows {
                status,
                status_sections: &self.status_sections,
                expanded: &self.expanded,
                collapsed_hunks: &self.collapsed_hunks,
                loading_sections: &self.loading_sections,
                diff_cache: &self.diff_cache,
                section_ids: self.config.status.section_ids(),
                recent_count: self.config.status.recent_count,
                palette: &self.palette,
            }
            .build()
        } else {
            vec![plain("Loading…", self.palette.dim)]
        };
    }
}

/// The borrowed view state that status-row building reads — passed by
/// [`StatusView::rebuild_rows`] so the builder is a pure function of its inputs
/// and testable without constructing a full view.
struct StatusRows<'a> {
    status: &'a Status,
    status_sections: &'a StatusSections,
    expanded: &'a HashSet<FoldKey>,
    collapsed_hunks: &'a HashSet<FoldKey>,
    loading_sections: &'a HashSet<SectionId>,
    diff_cache: &'a DiffCache,
    /// Section ids to render, in configured order (`[status].sections`).
    section_ids: Vec<String>,
    recent_count: usize,
    palette: &'a Palette,
}

impl StatusRows<'_> {
    /// Flatten the status into the ordered row list — sections/files/hunks/lines
    /// plus the commit and stash listings — honoring the fold state. Pure: a
    /// function of the borrowed inputs, mutating nothing.
    fn build(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        // The branch and its upstream/push tracking live in the title bar (see
        // `render_title_bar`), not in header rows here. Sections render in the
        // configured order; an unknown id was warned about at startup, skip it.
        let head = &self.status.head;
        let upstream = head.upstream.as_deref();
        // The distinct push target (triangular workflow), for the pushremote
        // sections; `None` when the push target is the upstream.
        let push = head.push.as_deref();
        // When there's nothing staged/unstaged/untracked, lead with the clean
        // notice — above the stashes/recent/log listings, not buried under them.
        if self.status.is_clean() {
            rows.push(spacer());
            rows.push(plain(
                "Nothing to commit, working tree clean",
                self.palette.dim,
            ));
        }
        for id in &self.section_ids {
            let Some(section) = SectionId::from_config_id(id) else {
                continue;
            };
            match section {
                SectionId::Untracked => self.push_section(
                    &mut rows,
                    section,
                    "Untracked files",
                    self.status.untracked().collect(),
                    None,
                ),
                SectionId::Unstaged => self.push_section(
                    &mut rows,
                    section,
                    "Unstaged changes",
                    self.status.unstaged().collect(),
                    Some(DiffSource::Unstaged),
                ),
                SectionId::Staged => self.push_section(
                    &mut rows,
                    section,
                    "Staged changes",
                    self.status.staged().collect(),
                    Some(DiffSource::Staged),
                ),
                SectionId::Stashes => self.push_stash_section(&mut rows),
                SectionId::Unpushed => {
                    // magit's heading: commits not on the upstream are
                    // "Unmerged into" it — after a rebase they may well be on
                    // the *push* target already, so "Unpushed" would lie;
                    // "Unpushed to <push>" is the pushremote section below.
                    let title = match upstream {
                        Some(t) => format!("Unmerged into {t}"),
                        None => "Unmerged".to_string(),
                    };
                    let n = self.status_sections.unpushed.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpushed,
                        Some(n),
                    );
                }
                SectionId::Unpulled => {
                    let title = match upstream {
                        Some(t) => format!("Unpulled from {t}"),
                        None => "Unpulled".to_string(),
                    };
                    let n = self.status_sections.unpulled.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpulled,
                        Some(n),
                    );
                }
                SectionId::Recent => {
                    // Honor recent_count at render too, so lowering it takes
                    // effect on the next reload (the list is fetched at the count
                    // from the last status refresh).
                    let n = self.recent_count.min(self.status_sections.recent.len());
                    self.push_commit_section(
                        &mut rows,
                        section,
                        "Recent commits",
                        &self.status_sections.recent[..n],
                        // No count — the recent list is capped to recent_count.
                        None,
                    );
                }
                SectionId::UnpushedPushremote => {
                    let title = match push {
                        Some(t) => format!("Unpushed to {t}"),
                        None => "Unpushed to pushremote".to_string(),
                    };
                    let n = self.status_sections.unpushed_pushremote.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpushed_pushremote,
                        Some(n),
                    );
                }
                SectionId::UnpulledPushremote => {
                    let title = match push {
                        Some(t) => format!("Unpulled from {t}"),
                        None => "Unpulled from pushremote".to_string(),
                    };
                    let n = self.status_sections.unpulled_pushremote.len();
                    self.push_commit_section(
                        &mut rows,
                        section,
                        &title,
                        &self.status_sections.unpulled_pushremote,
                        Some(n),
                    );
                }
                SectionId::Ignored => self.push_ignored_section(&mut rows),
            }
        }
        rows
    }

    fn push_section(
        &self,
        rows: &mut Vec<Row>,
        id: SectionId,
        title: &str,
        entries: Vec<&FileEntry>,
        source: Option<DiffSource>,
    ) {
        if entries.is_empty() {
            return;
        }
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: title.to_string(),
                count: Some(entries.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }

        for entry in entries {
            let path = entry.path.clone();
            let label = match &entry.orig_path {
                Some(orig) => format!("{orig} → {}", entry.path),
                None => entry.path.clone(),
            };
            let file_ref = FileRef {
                section: id,
                path: path.clone(),
            };
            let file_expanded =
                source.map(|s| self.expanded.contains(&FoldKey::File(s, path.clone())));
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: source.map(|s| FoldKey::File(s, path.clone())),
                target: Some(Target::File(file_ref.clone())),
                kind: RowKind::File {
                    status: status_label::status_label(entry, id),
                    status_color: status_label::status_color(entry, id, self.palette),
                    label,
                    expanded: file_expanded,
                },
            });

            if let (Some(src), Some(true)) = (source, file_expanded) {
                self.push_file_body(rows, src, &file_ref);
            }
        }
    }

    /// A commit-listing section (unpushed/unpulled/recent): a foldable header
    /// over one `RowKind::Commit` per commit. Skipped when empty — a still-
    /// loading section simply isn't rendered until its fetch lands (it pops in).
    /// `count` is shown after the title when `Some` — `None` for the recent
    /// section, which is capped to a fixed number anyway.
    pub(crate) fn push_commit_section(
        &self,
        rows: &mut Vec<Row>,
        id: SectionId,
        title: &str,
        commits: &[LogEntry],
        count: Option<usize>,
    ) {
        if commits.is_empty() {
            return;
        }
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: title.to_string(),
                count,
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        let upstream = self.status.head.upstream.as_deref();
        for c in commits {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::Commit {
                    hash: c.hash.clone(),
                    short_hash: c.short_hash.clone(),
                    subject: c.subject.clone(),
                    refs: parse_refs(&c.refs, upstream),
                },
            });
        }
    }

    /// The stashes section: a foldable header over one `RowKind::Stash` per
    /// entry. Skipped when there are no stashes.
    pub(crate) fn push_stash_section(&self, rows: &mut Vec<Row>) {
        let stashes = &self.status_sections.stashes;
        if stashes.is_empty() {
            return;
        }
        let id = SectionId::Stashes;
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: "Stashes".to_string(),
                count: Some(stashes.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        for s in stashes {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::Stash {
                    reference: s.reference.clone(),
                    message: s.message.clone(),
                },
            });
        }
    }

    /// The ignored-files section (opt-in): a foldable header over dim path rows
    /// (no staging — they're display-only). Skipped when there are none.
    pub(crate) fn push_ignored_section(&self, rows: &mut Vec<Row>) {
        let ignored = &self.status_sections.ignored;
        if ignored.is_empty() {
            return;
        }
        let id = SectionId::Ignored;
        rows.push(spacer());
        let expanded = self.expanded.contains(&FoldKey::Section(id));
        rows.push(Row {
            indent: 0,
            selectable: true,
            fold: Some(FoldKey::Section(id)),
            target: None,
            kind: RowKind::Section {
                title: "Ignored files".to_string(),
                count: Some(ignored.len()),
                expanded,
                refreshing: self.loading_sections.contains(&id),
            },
        });
        if !expanded {
            return;
        }
        for path in ignored {
            rows.push(Row {
                indent: 1,
                selectable: true,
                fold: None,
                target: None,
                kind: RowKind::File {
                    status: String::new(),
                    status_color: self.palette.dim,
                    label: path.clone(),
                    expanded: None,
                },
            });
        }
    }

    pub(crate) fn push_file_body(&self, rows: &mut Vec<Row>, source: DiffSource, file: &FileRef) {
        match self.diff_cache.state(&(source, file.path.clone())) {
            Some(DiffState::Loaded(diff)) => {
                if diff.is_binary {
                    rows.push(message("Binary file", self.palette.dim));
                } else if diff.hunks.is_empty() {
                    rows.push(message("(no textual changes)", self.palette.dim));
                }
                for (hunk_ix, hunk) in diff.hunks.iter().enumerate() {
                    let hunk_key = FoldKey::Hunk(source, file.path.clone(), hunk_ix);
                    let hunk_expanded = !self.collapsed_hunks.contains(&hunk_key);
                    rows.push(Row {
                        indent: 2,
                        selectable: true,
                        fold: Some(hunk_key),
                        target: Some(Target::Hunk {
                            file: file.clone(),
                            hunk: hunk_ix,
                        }),
                        kind: RowKind::HunkHeader {
                            text: status_label::hunk_header_text(hunk),
                            expanded: hunk_expanded,
                        },
                    });
                    if !hunk_expanded {
                        continue;
                    }
                    let file_hl = self.diff_cache.highlight(&(source, file.path.clone()));
                    for (line_ix, line) in hunk.lines.iter().enumerate() {
                        // Use cached highlight spans if present, else a single
                        // fallback span in the default color.
                        let spans: Arc<[Span]> = file_hl
                            .and_then(|h| h.get(&(hunk_ix, line_ix)))
                            .cloned()
                            .unwrap_or_else(|| {
                                let color = if line.kind == LineKind::NoNewline {
                                    self.palette.dim
                                } else {
                                    self.palette.fg
                                };
                                Arc::from(vec![(line.content.clone(), color)])
                            });
                        rows.push(Row {
                            indent: 2,
                            selectable: true,
                            fold: None,
                            target: Some(Target::Line {
                                file: file.clone(),
                                hunk: hunk_ix,
                                line: line_ix,
                            }),
                            kind: RowKind::Diff {
                                kind: line.kind,
                                spans,
                            },
                        });
                    }
                }
            }
            Some(DiffState::Loading) | None => {
                rows.push(message("Loading diff…", self.palette.dim))
            }
            Some(DiffState::Empty) => rows.push(message("(no changes)", self.palette.dim)),
            Some(DiffState::Failed(e)) => {
                rows.push(message(&format!("diff failed: {e}"), self.palette.dim))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{conflicted_paths, diffstat_text, parse_refs, stat_bar, RefKind, StatusRows};
    use crate::*;
    use magritte_core::{
        Change, DiffLine, EntryKind, FileDiff, FileEntry, HeadInfo, Hunk, LineKind, LogEntry,
        Stash, Status,
    };
    use std::collections::HashSet;

    /// Everything `StatusRows` borrows, owned by the test — build rows from any
    /// combination of status, listings, fold state, and loaded diffs.
    struct Inputs {
        status: Status,
        sections: StatusSections,
        expanded: HashSet<FoldKey>,
        collapsed: HashSet<FoldKey>,
        diff_cache: DiffCache,
        section_ids: Vec<&'static str>,
        recent_count: usize,
    }

    impl Default for Inputs {
        fn default() -> Self {
            Inputs {
                status: Status::default(),
                sections: StatusSections::default(),
                expanded: HashSet::new(),
                collapsed: HashSet::new(),
                diff_cache: DiffCache::default(),
                section_ids: Vec::new(),
                recent_count: 10,
            }
        }
    }

    impl Inputs {
        fn expand(mut self, key: FoldKey) -> Self {
            self.expanded.insert(key);
            self
        }

        fn build(&self) -> Vec<Row> {
            let loading = HashSet::new();
            let palette = Palette::default();
            StatusRows {
                status: &self.status,
                status_sections: &self.sections,
                expanded: &self.expanded,
                collapsed_hunks: &self.collapsed,
                loading_sections: &loading,
                diff_cache: &self.diff_cache,
                section_ids: self.section_ids.iter().map(|s| s.to_string()).collect(),
                recent_count: self.recent_count,
                palette: &palette,
            }
            .build()
        }
    }

    /// Build the status rows for `status` with the given expanded sections, over
    /// otherwise-empty inputs (no loaded diffs / listings).
    fn build(status: &Status, expanded: &HashSet<FoldKey>, section_ids: &[&str]) -> Vec<Row> {
        let sections = StatusSections::default();
        let (collapsed, loading) = (HashSet::new(), HashSet::new());
        let diff_cache = DiffCache::default();
        let palette = Palette::default();
        StatusRows {
            status,
            status_sections: &sections,
            expanded,
            collapsed_hunks: &collapsed,
            loading_sections: &loading,
            diff_cache: &diff_cache,
            section_ids: section_ids.iter().map(|s| s.to_string()).collect(),
            recent_count: 0,
            palette: &palette,
        }
        .build()
    }

    fn commit(n: usize) -> LogEntry {
        LogEntry {
            hash: format!("{n:040}"),
            short_hash: format!("{n:07}"),
            subject: format!("commit {n}"),
            refs: String::new(),
            author: "a".into(),
            date: "now".into(),
        }
    }

    /// The section headers in row order, as `(title, count, expanded)`.
    fn headers(rows: &[Row]) -> Vec<(String, Option<usize>, bool)> {
        rows.iter()
            .filter_map(|r| match &r.kind {
                RowKind::Section {
                    title,
                    count,
                    expanded,
                    ..
                } => Some((title.clone(), *count, *expanded)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn clean_status_shows_the_clean_notice() {
        let rows = build(&Status::default(), &HashSet::new(), &["unstaged", "staged"]);
        assert!(
            rows.iter().any(|r| matches!(
                &r.kind,
                RowKind::Plain { text, .. } if text.contains("Nothing to commit")
            )),
            "a clean status should lead with the clean notice"
        );
    }

    #[test]
    fn expanded_unstaged_section_renders_header_and_file() {
        let status = Status {
            head: HeadInfo::default(),
            entries: vec![FileEntry {
                path: "a.txt".into(),
                orig_path: None,
                kind: EntryKind::Tracked,
                index: Change::Unmodified,
                worktree: Change::Modified,
            }],
        };
        let mut expanded = HashSet::new();
        expanded.insert(FoldKey::Section(SectionId::Unstaged));
        let rows = build(&status, &expanded, &["unstaged"]);

        // No clean notice (there's a change), a counted+expanded section header,
        // and the file row beneath it.
        assert!(!rows.iter().any(|r| matches!(
            &r.kind,
            RowKind::Plain { text, .. } if text.contains("Nothing to commit")
        )));
        assert!(rows.iter().any(|r| matches!(
            &r.kind,
            RowKind::Section { title, count, expanded, .. }
                if title == "Unstaged changes" && *count == Some(1) && *expanded
        )));
        assert!(rows.iter().any(|r| matches!(
            &r.kind,
            RowKind::File { label, .. } if label == "a.txt"
        )));
    }

    #[test]
    fn collapsed_section_hides_its_files() {
        let status = Status {
            head: HeadInfo::default(),
            entries: vec![FileEntry {
                path: "a.txt".into(),
                orig_path: None,
                kind: EntryKind::Tracked,
                index: Change::Unmodified,
                worktree: Change::Modified,
            }],
        };
        // Section not in `expanded` → collapsed: header shows, no file row.
        let rows = build(&status, &HashSet::new(), &["unstaged"]);
        assert!(rows.iter().any(
            |r| matches!(&r.kind, RowKind::Section { title, .. } if title == "Unstaged changes")
        ));
        assert!(!rows.iter().any(|r| matches!(&r.kind, RowKind::File { .. })));
    }

    #[test]
    fn stat_bar_scales_to_at_most_twenty_marks() {
        // Small diffs render exact counts.
        assert_eq!(stat_bar(3, 2), (3, 2));
        assert_eq!(stat_bar(0, 0), (0, 0));
        assert_eq!(stat_bar(20, 0), (20, 0));
        // Large diffs scale down to <=20 total, keeping each nonzero side >=1.
        let (p, m) = stat_bar(300, 100);
        assert!(p + m <= 20 && p >= 1 && m >= 1, "got ({p}, {m})");
        assert!(p > m, "the +side should dominate a 3:1 diff");
        // A one-sided huge diff keeps the other side at zero.
        let (p, m) = stat_bar(1000, 0);
        assert_eq!((p, m), (20, 0));
        // A tiny minority side still shows at least one mark.
        let (p, m) = stat_bar(1000, 1);
        assert_eq!(m, 1);
        assert!(p <= 19);
    }

    #[test]
    fn diffstat_text_pluralizes_and_omits_zero_sides() {
        assert_eq!(diffstat_text(1, 1, 0), "1 file changed, 1 insertion(+)");
        assert_eq!(
            diffstat_text(2, 5, 3),
            "2 files changed, 5 insertions(+), 3 deletions(-)"
        );
        assert_eq!(diffstat_text(1, 0, 2), "1 file changed, 2 deletions(-)");
        assert_eq!(diffstat_text(3, 0, 0), "3 files changed");
    }

    #[test]
    fn listing_sections_render_in_configured_order_with_counts() {
        let mut inputs = Inputs {
            section_ids: vec!["stashes", "unpulled", "unpushed", "recent"],
            ..Inputs::default()
        }
        .expand(FoldKey::Section(SectionId::Stashes))
        .expand(FoldKey::Section(SectionId::Unpushed))
        .expand(FoldKey::Section(SectionId::Recent));
        inputs.status.head.upstream = Some("origin/main".to_string());
        inputs.sections.stashes = vec![Stash {
            reference: "stash@{0}".into(),
            message: "WIP on main".into(),
        }];
        inputs.sections.unpulled = vec![commit(1)];
        inputs.sections.unpushed = vec![commit(2), commit(3)];
        inputs.sections.recent = vec![commit(4)];
        let rows = inputs.build();

        // Headers follow `[status].sections` order; the divergence headers name
        // the upstream; recent shows no count (it's capped anyway); an
        // unexpanded section reports so.
        assert_eq!(
            headers(&rows),
            vec![
                ("Stashes".to_string(), Some(1), true),
                ("Unpulled from origin/main".to_string(), Some(1), false),
                ("Unmerged into origin/main".to_string(), Some(2), true),
                ("Recent commits".to_string(), None, true),
            ]
        );
        // Expanded sections list their rows; the collapsed one hides them.
        assert!(rows.iter().any(|r| matches!(
            &r.kind,
            RowKind::Stash { reference, .. } if reference == "stash@{0}"
        )));
        let commits: Vec<&str> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Commit { subject, .. } => Some(subject.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(commits, vec!["commit 2", "commit 3", "commit 4"]);
    }

    #[test]
    fn empty_listings_render_no_section() {
        let inputs = Inputs {
            section_ids: vec!["stashes", "unpushed", "unpulled", "recent"],
            ..Inputs::default()
        };
        assert!(headers(&inputs.build()).is_empty());
    }

    #[test]
    fn pushremote_sections_name_the_distinct_push_target() {
        let mut inputs = Inputs {
            section_ids: vec!["unpushed-pushremote", "unpulled-pushremote"],
            ..Inputs::default()
        };
        inputs.status.head.upstream = Some("origin/main".to_string());
        // A distinct @{push} (triangular workflow) titles the sections with it.
        inputs.status.head.push = Some("fork/main".to_string());
        inputs.sections.unpushed_pushremote = vec![commit(1)];
        inputs.sections.unpulled_pushremote = vec![commit(2)];
        let titles: Vec<String> = headers(&inputs.build()).into_iter().map(|h| h.0).collect();
        assert_eq!(
            titles,
            vec![
                "Unpushed to fork/main".to_string(),
                "Unpulled from fork/main".to_string()
            ]
        );
        // A push target equal to the upstream: the loader keeps these listings
        // empty, so no pushremote sections appear.
        inputs.status.head.push = None;
        inputs.sections.unpushed_pushremote.clear();
        inputs.sections.unpulled_pushremote.clear();
        assert!(headers(&inputs.build()).is_empty());
    }

    #[test]
    fn recent_listing_is_capped_to_recent_count() {
        let mut inputs = Inputs {
            section_ids: vec!["recent"],
            recent_count: 2,
            ..Inputs::default()
        }
        .expand(FoldKey::Section(SectionId::Recent));
        inputs.sections.recent = vec![commit(1), commit(2), commit(3)];
        let rows = inputs.build();
        let commits = rows
            .iter()
            .filter(|r| matches!(&r.kind, RowKind::Commit { .. }))
            .count();
        assert_eq!(commits, 2);
    }

    #[test]
    fn commit_rows_carry_parsed_ref_labels() {
        let mut inputs = Inputs {
            section_ids: vec!["recent"],
            ..Inputs::default()
        }
        .expand(FoldKey::Section(SectionId::Recent));
        inputs.status.head.upstream = Some("origin/main".to_string());
        let mut c = commit(1);
        c.refs = "HEAD -> main, origin/main, tag: v1".to_string();
        inputs.sections.recent = vec![c];
        let rows = inputs.build();
        let refs = rows
            .iter()
            .find_map(|r| match &r.kind {
                RowKind::Commit { refs, .. } => Some(refs.clone()),
                _ => None,
            })
            .expect("a commit row");
        // The current branch and its upstream fold into one synced entry.
        assert_eq!(
            refs,
            vec![
                ("origin/main".to_string(), RefKind::SyncedHead),
                ("v1".to_string(), RefKind::Tag)
            ]
        );
    }

    #[test]
    fn parse_refs_classifies_and_drops_remote_head() {
        assert_eq!(
            parse_refs("origin/HEAD, origin/main, feature, tag: v2", None),
            vec![
                ("origin/main".to_string(), RefKind::Remote),
                ("feature".to_string(), RefKind::Local),
                ("v2".to_string(), RefKind::Tag),
            ]
        );
        assert_eq!(
            parse_refs("HEAD", None),
            vec![("HEAD".to_string(), RefKind::Head)]
        );
        // No upstream on the commit: the current branch stays unfolded.
        assert_eq!(
            parse_refs("HEAD -> main", Some("origin/main")),
            vec![("main".to_string(), RefKind::Head)]
        );
    }

    #[test]
    fn conflicted_paths_collects_only_unmerged_entries() {
        let entry = |path: &str, kind: EntryKind| FileEntry {
            path: path.into(),
            orig_path: None,
            kind,
            index: Change::Unmodified,
            worktree: Change::Modified,
        };
        let status = Status {
            head: HeadInfo::default(),
            entries: vec![
                entry("clean.txt", EntryKind::Tracked),
                entry("theirs.txt", EntryKind::Unmerged),
                entry("new.txt", EntryKind::Untracked),
                entry("ours.txt", EntryKind::Unmerged),
            ],
        };
        let conflicted = conflicted_paths(&status);
        assert_eq!(
            conflicted,
            HashSet::from(["theirs.txt".to_string(), "ours.txt".to_string()])
        );
        assert!(conflicted_paths(&Status::default()).is_empty());
    }

    #[test]
    fn collapsed_hunk_keeps_its_header_and_hides_its_lines() {
        let path = "a.txt".to_string();
        let hunk = |start: u32| Hunk {
            old_start: start,
            old_count: 1,
            new_start: start,
            new_count: 2,
            section_heading: String::new(),
            lines: vec![
                DiffLine {
                    kind: LineKind::Added,
                    content: "new".into(),
                    raw: None,
                    old_lineno: None,
                    new_lineno: Some(start),
                },
                DiffLine {
                    kind: LineKind::Context,
                    content: "old".into(),
                    raw: None,
                    old_lineno: Some(start),
                    new_lineno: Some(start + 1),
                },
            ],
        };
        let mut inputs = Inputs {
            section_ids: vec!["unstaged"],
            ..Inputs::default()
        }
        .expand(FoldKey::Section(SectionId::Unstaged))
        .expand(FoldKey::File(DiffSource::Unstaged, path.clone()));
        inputs.status.entries = vec![FileEntry {
            path: path.clone(),
            orig_path: None,
            kind: EntryKind::Tracked,
            index: Change::Unmodified,
            worktree: Change::Modified,
        }];
        inputs.diff_cache.set_state(
            (DiffSource::Unstaged, path.clone()),
            DiffState::Loaded(Arc::new(FileDiff {
                old_path: path.clone(),
                new_path: path.clone(),
                hunks: vec![hunk(1), hunk(10)],
                ..FileDiff::default()
            })),
        );
        // Collapse the first hunk only.
        inputs
            .collapsed
            .insert(FoldKey::Hunk(DiffSource::Unstaged, path.clone(), 0));
        let rows = inputs.build();

        let hunk_headers: Vec<bool> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::HunkHeader { expanded, .. } => Some(*expanded),
                _ => None,
            })
            .collect();
        assert_eq!(hunk_headers, vec![false, true]);
        // Only the expanded hunk's two lines are projected as diff rows, and
        // they target the second hunk.
        let diff_targets: Vec<usize> = rows
            .iter()
            .filter(|r| matches!(&r.kind, RowKind::Diff { .. }))
            .filter_map(|r| match &r.target {
                Some(Target::Line { hunk, .. }) => Some(*hunk),
                _ => None,
            })
            .collect();
        assert_eq!(diff_targets, vec![1, 1]);
    }
}

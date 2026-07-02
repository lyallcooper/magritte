//! Building the status screen's flat row list: the section model (which
//! sections exist, in configured order) and rebuild_rows, which flattens the
//! parsed status + fold state + loaded diffs into [`Row`]s, plus the small
//! row/text constructors the builders and copy paths share.

use std::rc::Rc;

use gpui::Hsla;
use magritte_core::{CommitMetadata, DiffSource, EntryKind, LogEntry, Stash};

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
}

/// Parse a commit's `%D` decoration (e.g. `HEAD -> main, origin/main, tag: v1`)
/// into labeled, classified entries for rendering.
pub(crate) fn parse_refs(refs: &str) -> Vec<(String, RefKind)> {
    refs.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
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
        })
        .collect()
}

/// The plain text of a commit-view row, for copying (diff line content without
/// the `+`/`-` sigil).
pub(crate) fn commit_row_text(row: &CommitDiffRow) -> String {
    match row {
        CommitDiffRow::Detail(d) => d.clone(),
        CommitDiffRow::Message(m) => m.clone(),
        CommitDiffRow::File(p) => p.clone(),
        CommitDiffRow::Hunk(h) => h.clone(),
        CommitDiffRow::Line { spans, .. } => spans.iter().map(|(t, _)| t.as_str()).collect(),
        CommitDiffRow::Note(n) => n.clone(),
    }
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

pub(crate) fn prepend_commit_details(rows: &mut Vec<CommitDiffRow>, details: &[String]) {
    if details.is_empty() || rows.iter().any(|row| matches!(row, CommitDiffRow::Detail(_))) {
        return;
    }
    while matches!(rows.first(), Some(CommitDiffRow::Note(n)) if n.is_empty()) {
        rows.remove(0);
    }
    let mut prefix = details
        .iter()
        .cloned()
        .map(CommitDiffRow::Detail)
        .collect::<Vec<_>>();
    prefix.push(CommitDiffRow::Note(String::new()));
    rows.splice(0..0, prefix);
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
            color: gpui::black(),
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

impl StatusView {
    pub(crate) fn rebuild_rows(&mut self) {
        // Refresh the conflicted-path set so is_conflicted (called per clickable
        // row in render) is an O(1) lookup, not an O(entries) scan per row.
        self.conflicted = self
            .status
            .as_ref()
            .map(|s| {
                s.entries
                    .iter()
                    .filter(|e| e.kind == EntryKind::Unmerged)
                    .map(|e| e.path.clone())
                    .collect()
            })
            .unwrap_or_default();

        let mut rows = Vec::new();

        if let Some(error) = &self.error {
            rows.push(plain(format!("Error: {error}"), self.palette.removed));
            self.rows = rows;
            return;
        }
        let Some(status) = &self.status else {
            rows.push(plain("Loading…", self.palette.dim));
            self.rows = rows;
            return;
        };

        // The branch and its upstream/push tracking live in the title bar (see
        // `render_title_bar`), not in header rows here. Sections render in the
        // configured order (`[status].sections`); an unknown id was warned about
        // at startup and is skipped here.
        let head = &status.head;
        let upstream = head.upstream.as_deref();
        // The distinct push target (triangular workflow), for the pushremote
        // sections; `None` when the push target is the upstream.
        let push = head.push.as_deref();
        // When there's nothing staged/unstaged/untracked, lead with the clean
        // notice — above the stashes/recent/log listings, not buried under them.
        if status.is_clean() {
            rows.push(spacer());
            rows.push(plain(
                "Nothing to commit, working tree clean",
                self.palette.dim,
            ));
        }
        for id in self.config.status.section_ids() {
            let Some(section) = SectionId::from_config_id(&id) else {
                continue;
            };
            match section {
                SectionId::Untracked => self.push_section(
                    &mut rows,
                    section,
                    "Untracked files",
                    status.untracked().collect(),
                    None,
                ),
                SectionId::Unstaged => self.push_section(
                    &mut rows,
                    section,
                    "Unstaged changes",
                    status.unstaged().collect(),
                    Some(DiffSource::Unstaged),
                ),
                SectionId::Staged => self.push_section(
                    &mut rows,
                    section,
                    "Staged changes",
                    status.staged().collect(),
                    Some(DiffSource::Staged),
                ),
                SectionId::Stashes => self.push_stash_section(&mut rows),
                SectionId::Unpushed => {
                    let title = match upstream {
                        Some(t) => format!("Unpushed to {t}"),
                        None => "Unpushed".to_string(),
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
                    // effect on the next reload (the list is fetched at the
                    // count from the last status refresh).
                    let n = self
                        .config
                        .status
                        .recent_count
                        .min(self.status_sections.recent.len());
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

        self.rows = rows;
    }

    pub(crate) fn push_section(
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
                    status_color: status_label::status_color(entry, id, &self.palette),
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
                    refs: parse_refs(&c.refs),
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
        match self.diffs.get(&(source, file.path.clone())) {
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
                    let file_hl = self.highlights.get(&(source, file.path.clone()));
                    for (line_ix, line) in hunk.lines.iter().enumerate() {
                        // Use cached highlight spans if present, else a single
                        // fallback span in the default color.
                        let spans: Rc<[Span]> = file_hl
                            .and_then(|h| h.get(&(hunk_ix, line_ix)))
                            .cloned()
                            .unwrap_or_else(|| {
                                let color = if line.kind == LineKind::NoNewline {
                                    self.palette.dim
                                } else {
                                    self.palette.fg
                                };
                                Rc::from(vec![(line.content.clone(), color)])
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

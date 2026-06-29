//! Human-readable status words and colors for a file row, plus the diff hunk
//! header text — the small mapping from git's status/diff model to what the
//! status view shows. Pure functions over the core types.

use gpui::Hsla;

use magritte_core::{Change, EntryKind, FileEntry};

use crate::{Palette, SectionId};

/// The unified-diff hunk header (`@@ -a,b +c,d @@ heading`).
pub(crate) fn hunk_header_text(hunk: &magritte_core::Hunk) -> String {
    let mut text = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    );
    if !hunk.section_heading.is_empty() {
        text.push(' ');
        text.push_str(&hunk.section_heading);
    }
    text
}

/// The change relevant to a file within a given section: a staged row reflects
/// the index status, everything else the worktree status.
fn section_change(entry: &FileEntry, section: SectionId) -> Change {
    // Intent-to-add shows in both sections (magit-style): a staged *new file*
    // (the empty index placeholder) and an unstaged *modification* (the content
    // against that placeholder).
    if entry.is_intent_to_add() {
        return match section {
            SectionId::Staged => Change::Added,
            _ => Change::Modified,
        };
    }
    match section {
        SectionId::Staged => entry.index,
        _ => entry.worktree,
    }
}

/// A human-readable status word (magit-style) for a file in a section, e.g.
/// "modified", "new file", "deleted". Untracked files carry no word — the
/// section header already says "Untracked files".
pub(crate) fn status_label(entry: &FileEntry, section: SectionId) -> String {
    if entry.kind == EntryKind::Untracked {
        // No status word — the "Untracked files" header already says it, and
        // the filename sits flush rather than tabbed past an empty column.
        return String::new();
    }
    match section_change(entry, section) {
        Change::Unmodified => "",
        Change::Modified => "modified",
        Change::TypeChanged => "typechange",
        Change::Added => "new file",
        Change::Deleted => "deleted",
        Change::Renamed => "renamed",
        Change::Copied => "copied",
        Change::Unmerged => "conflicted",
    }
    .to_string()
}

/// The color for a file row's status word / name, by its change in the section.
pub(crate) fn status_color(entry: &FileEntry, section: SectionId, p: &Palette) -> Hsla {
    if entry.kind == EntryKind::Untracked {
        return p.added;
    }
    match section_change(entry, section) {
        Change::Added | Change::Copied => p.added,
        Change::Deleted => p.removed,
        _ => p.modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind, index: Change, worktree: Change) -> FileEntry {
        FileEntry {
            path: "f".into(),
            orig_path: None,
            kind,
            index,
            worktree,
        }
    }

    #[test]
    fn status_label_humanizes_per_section() {
        // A staged row reflects the index status; unstaged reflects the worktree.
        let staged_add = entry(EntryKind::Tracked, Change::Added, Change::Unmodified);
        assert_eq!(status_label(&staged_add, SectionId::Staged), "new file");

        let modified = entry(EntryKind::Tracked, Change::Unmodified, Change::Modified);
        assert_eq!(status_label(&modified, SectionId::Unstaged), "modified");

        let deleted = entry(EntryKind::Tracked, Change::Unmodified, Change::Deleted);
        assert_eq!(status_label(&deleted, SectionId::Unstaged), "deleted");

        let conflicted = entry(EntryKind::Unmerged, Change::Unmodified, Change::Unmerged);
        assert_eq!(status_label(&conflicted, SectionId::Unstaged), "conflicted");
    }

    #[test]
    fn intent_to_add_shows_new_file_staged_and_modified_unstaged() {
        // `git add -N`: porcelain `.A` (index unmodified, worktree added).
        let ita = entry(EntryKind::Tracked, Change::Unmodified, Change::Added);
        assert!(ita.is_intent_to_add());
        assert!(ita.is_staged() && ita.is_unstaged(), "appears in both sections");
        assert_eq!(status_label(&ita, SectionId::Staged), "new file");
        assert_eq!(status_label(&ita, SectionId::Unstaged), "modified");
    }

    #[test]
    fn untracked_files_carry_no_status_word() {
        let untracked = entry(EntryKind::Untracked, Change::Unmodified, Change::Modified);
        assert_eq!(status_label(&untracked, SectionId::Untracked), "");
    }
}

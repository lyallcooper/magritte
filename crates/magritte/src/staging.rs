//! Act-at-point: the row/target model (what the cursor is on), resolving an
//! operation against it or the visual selection into a concrete [`Action`],
//! conflict guards, destructive confirmation, and opening the file at point in
//! an external editor. `impl StatusView` like the other view slices.

use gpui::{Context, Window};
use magritte_core::{ConflictSide, DiffSource, FileDiff, Hunk, LineKind};

use crate::*;

/// Identity of a foldable node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FoldKey {
    Section(SectionId),
    File(DiffSource, String),
    /// A hunk within a file's diff: (source, path, hunk index). Unlike sections
    /// and files, hunks are expanded by default; see `collapsed_hunks`.
    Hunk(DiffSource, String, usize),
}

/// A file identified by its path and which section it appears in.
#[derive(Debug, Clone)]
pub(crate) struct FileRef {
    pub(crate) section: SectionId,
    pub(crate) path: String,
}

/// What the row at point represents, for "act on point" staging.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    File(FileRef),
    Hunk {
        file: FileRef,
        hunk: usize,
    },
    Line {
        file: FileRef,
        hunk: usize,
        line: usize,
    },
}

pub(crate) fn section_source(section: SectionId) -> Option<DiffSource> {
    match section {
        SectionId::Unstaged => Some(DiffSource::Unstaged),
        SectionId::Staged => Some(DiffSource::Staged),
        // Untracked, ignored, and the commit/stash sections have no diff source.
        SectionId::Untracked
        | SectionId::Stashes
        | SectionId::Unpushed
        | SectionId::Unpulled
        | SectionId::UnpushedPushremote
        | SectionId::UnpulledPushremote
        | SectionId::Recent
        | SectionId::Ignored => None,
    }
}

/// The repo-relative path of the file a target belongs to.
pub(crate) fn target_path(target: &Target) -> &str {
    match target {
        Target::File(f) => &f.path,
        Target::Hunk { file, .. } | Target::Line { file, .. } => &file.path,
    }
}

/// Which staging verbs apply to a target, by section: `(stage, unstage,
/// discard)`. Populates the right-click menu with only meaningful actions.
pub(crate) fn target_ops(target: &Target) -> (bool, bool, bool) {
    let section = match target {
        Target::File(f) => f.section,
        Target::Hunk { file, .. } | Target::Line { file, .. } => file.section,
    };
    match section {
        // Untracked/unstaged content can be staged or discarded.
        SectionId::Untracked | SectionId::Unstaged => (true, false, true),
        // Staged content can be unstaged or discarded.
        SectionId::Staged => (false, true, true),
        // Commit/stash/ignored sections carry no file-staging verbs (never
        // reached via a file Target, but the match must be exhaustive).
        SectionId::Stashes
        | SectionId::Unpushed
        | SectionId::Unpulled
        | SectionId::UnpushedPushremote
        | SectionId::UnpulledPushremote
        | SectionId::Recent
        | SectionId::Ignored => (false, false, false),
    }
}

/// Async state of a single file's diff.
pub(crate) enum DiffState {
    Loading,
    /// Shared (`Arc`) between this cache and any [`Action`] built from it, so
    /// staging one line doesn't clone the whole parsed file diff.
    Loaded(Arc<FileDiff>),
    Empty,
    Failed(String),
}

/// A loaded file diff's cache key: its source (staged/unstaged/…) and path.
pub(crate) type DiffKey = (DiffSource, String);

/// The lazily-loaded per-file diff cache for the status view: each file's async
/// [`DiffState`], the highlight language detected at load time, and the computed
/// highlight spans — all keyed by `(source, path)`. Bundling the three maps puts
/// their shared-key invariant in one place: they evict together on refresh
/// ([`retain`](Self::retain)) and the highlight map is rebuilt from the diffs +
/// languages on a theme change ([`recompute_highlights`](Self::recompute_highlights)).
#[derive(Default)]
pub(crate) struct DiffCache {
    states: std::collections::HashMap<DiffKey, DiffState>,
    langs: std::collections::HashMap<DiffKey, &'static str>,
    highlights: std::collections::HashMap<DiffKey, FileHighlights>,
}

impl DiffCache {
    /// The async load state of a file's diff, if a load has been started.
    pub(crate) fn state(&self, key: &DiffKey) -> Option<&DiffState> {
        self.states.get(key)
    }

    /// The cached highlight spans for a loaded diff, if computed.
    pub(crate) fn highlight(&self, key: &DiffKey) -> Option<&FileHighlights> {
        self.highlights.get(key)
    }

    /// Whether a load has been recorded for `key` (loading, loaded, or failed).
    pub(crate) fn contains(&self, key: &DiffKey) -> bool {
        self.states.contains_key(key)
    }

    /// The keys of every diff a load has been recorded for.
    pub(crate) fn keys(&self) -> Vec<DiffKey> {
        self.states.keys().cloned().collect()
    }

    /// Iterate the loaded diffs (key → parsed diff), skipping entries still
    /// loading, empty, or failed.
    pub(crate) fn loaded(&self) -> impl Iterator<Item = (&DiffKey, &FileDiff)> {
        self.states.iter().filter_map(|(key, state)| match state {
            DiffState::Loaded(diff) => Some((key, diff.as_ref())),
            _ => None,
        })
    }

    pub(crate) fn set_state(&mut self, key: DiffKey, state: DiffState) {
        self.states.insert(key, state);
    }

    pub(crate) fn set_lang(&mut self, key: DiffKey, lang: &'static str) {
        self.langs.insert(key, lang);
    }

    pub(crate) fn set_highlight(&mut self, key: DiffKey, highlights: FileHighlights) {
        self.highlights.insert(key, highlights);
    }

    /// Drop every entry whose key isn't in `keep` — across all three maps at
    /// once, so a status refresh keeps only the still-expanded files' diffs.
    pub(crate) fn retain(&mut self, keep: &std::collections::HashSet<DiffKey>) {
        self.states.retain(|key, _| keep.contains(key));
        self.langs.retain(|key, _| keep.contains(key));
        self.highlights.retain(|key, _| keep.contains(key));
    }

    /// Rebuild the highlight spans for every loaded, non-binary diff from its
    /// load-time language via `rehighlight` (which carries the current theme).
    /// No files are re-read — only the spans are recomputed.
    pub(crate) fn recompute_highlights(
        &mut self,
        mut rehighlight: impl FnMut(&FileDiff, &'static str) -> FileHighlights,
    ) {
        if self.highlights.is_empty() && self.langs.is_empty() {
            return;
        }
        let mut next = std::collections::HashMap::new();
        for (key, state) in &self.states {
            let DiffState::Loaded(diff) = state else {
                continue;
            };
            if diff.is_binary {
                continue;
            }
            if let Some(&lang) = self.langs.get(key) {
                next.insert(key.clone(), rehighlight(diff, lang));
            }
        }
        self.highlights = next;
    }
}

/// One rendered line. Every row is the same height so `uniform_list` can
/// virtualize them.
pub(crate) struct Row {
    pub(crate) indent: usize,
    pub(crate) selectable: bool,
    /// Present on foldable rows (sections, files); `TAB` toggles this key.
    pub(crate) fold: Option<FoldKey>,
    /// What this row represents for staging "at point" (s/u/x).
    pub(crate) target: Option<Target>,
    pub(crate) kind: RowKind,
}

pub(crate) enum RowKind {
    Plain {
        text: String,
        color: Hsla,
    },
    Section {
        title: String,
        /// The item count shown after the title, or `None` to omit it (e.g. the
        /// recent-commits section, which is capped to a fixed number anyway).
        count: Option<usize>,
        expanded: bool,
        /// The section's listing is being re-fetched; show a small spinner by
        /// the header. Only set on sections that already have data (so a
        /// first-load section just pops in rather than flashing a spinner).
        refreshing: bool,
    },
    File {
        /// Humanized status word ("modified", "new file", …); empty for untracked.
        status: String,
        status_color: Hsla,
        label: String,
        expanded: Option<bool>,
    },
    HunkHeader {
        text: String,
        expanded: bool,
    },
    Diff {
        kind: LineKind,
        /// Syntax-highlighted (or fallback) content runs, shared with the
        /// highlight cache (see [`FileHighlights`]).
        spans: Rc<[Span]>,
    },
    /// A commit row in a non-file section (unpushed/unpulled/recent): dim short
    /// hash, ref labels, and subject, like the log view. `hash` (full) drives
    /// act-at-point; `refs` are the classified `%D` decorations, parsed once at
    /// row-build time (not per frame).
    Commit {
        hash: String,
        short_hash: String,
        subject: String,
        refs: Vec<(String, RefKind)>,
    },
    /// A stash row: dim reference + message.
    Stash {
        reference: String,
        message: String,
    },
}

/// A pending yes/no confirmation shown in the bottom bar.
pub(crate) enum Confirm {
    /// A destructive staging action awaiting `y`.
    Action(Action),
    /// `c c` with nothing staged: on `y`, commit all tracked changes by
    /// opening the message editor with `--all` (the carried switches) appended.
    CommitAll(Vec<String>),
    /// Amend/reword/extend of an already-published HEAD: on `y`, proceed with
    /// the carried command + switches (rewriting pushed history).
    AmendPushed(transient::Command, Vec<String>),
    /// Abort the in-progress sequence (discards its progress): on `y`, run the
    /// abort for the carried kind.
    AbortSequence(SequenceKind),
    /// Destructive reset (hard or worktree-only, discards uncommitted changes):
    /// on `y`, reset to the target in the carried mode.
    Reset(ResetMode, String),
    /// Interactive rebase since an already-published commit: on `y`, open the
    /// todo editor for `rev^..HEAD` with the carried switches (rewriting pushed
    /// history).
    RebaseSincePushed { rev: String, args: Vec<String> },
    /// Reword an already-published commit: on `y`, run the direct reword rebase.
    RebaseRewordPushed { rev: String, args: Vec<String> },
    /// Instant fixup/squash into an already-published commit (it autosquashes
    /// immediately, rewriting pushed history): on `y`, run it.
    AutosquashPublished {
        op: SquashOp,
        rev: String,
        args: Vec<String>,
    },
    /// A user `[[command]]` that looks destructive (resolved command): on `y`,
    /// run it via the shell, refreshing unless opted out.
    CustomShell { command: String, refresh: bool },
    /// Drop the stash at point (`x` on a stash row): on `y`, drop the reference.
    DropStash(String),
    /// Remove the worktree at point in the browser: on `y`, `git worktree
    /// remove` its path (non-force, so git refuses a dirty worktree).
    RemoveWorktree(String),
    /// `S` with changes already staged (magit confirms: it blurs the
    /// staged/unstaged split): on `y`, stage all tracked changes.
    StageAll,
    /// `U` with unstaged/untracked changes present (same rationale): on `y`,
    /// unstage everything.
    UnstageAll,
}

impl StatusView {
    /// The loaded diff for `file`, if available — shared for embedding in an
    /// [`Action`] (an `Arc` clone, not a deep copy).
    pub(crate) fn diff_for(&self, file: &FileRef) -> Option<Arc<FileDiff>> {
        let source = section_source(file.section)?;
        match self.diff_cache.state(&(source, file.path.clone()))? {
            DiffState::Loaded(diff) => Some(diff.clone()),
            _ => None,
        }
    }

    /// Borrow the loaded diff for `file`, for read-only lookups (a hunk's line
    /// count, a target line).
    pub(crate) fn diff_for_ref(&self, file: &FileRef) -> Option<&FileDiff> {
        let source = section_source(file.section)?;
        match self.diff_cache.state(&(source, file.path.clone()))? {
            DiffState::Loaded(diff) => Some(diff),
            _ => None,
        }
    }

    /// Whether `path` is an unmerged (conflicted) entry. Conflict resolution
    /// isn't supported in-app yet, so ordinary stage/unstage/discard is refused
    /// on these — `git add` would silently mark a conflict resolved (markers and
    /// all), and a discard could lose work.
    pub(crate) fn is_conflicted(&self, path: &str) -> bool {
        // O(1) against the set refreshed in `rebuild_rows`.
        self.conflicted.contains(path)
    }

    /// The first conflicted file in the current selection — the row at point, or
    /// any file touched by the visual region. Used to refuse the *whole* action
    /// (point or region) rather than silently acting on a subset.
    pub(crate) fn conflicted_in_selection(&self) -> Option<String> {
        let path_at = |ix: usize| {
            self.rows
                .get(ix)
                .and_then(|r| r.target.as_ref())
                .map(target_path)
        };
        match self.visual_range() {
            Some((lo, hi)) => (lo..=hi)
                .filter_map(path_at)
                .find(|p| self.is_conflicted(p))
                .map(str::to_string),
            None => path_at(self.selected)
                .filter(|p| self.is_conflicted(p))
                .map(str::to_string),
        }
    }

    /// Resolve a whole-file staging action for `op` on `f`, honoring its
    /// section (e.g. you cannot stage a file that's already staged; discard
    /// means delete for untracked, revert-to-index for unstaged, and
    /// revert-the-index for staged). Shared by point resolution and by region
    /// selections that include a file-name row.
    pub(crate) fn file_action(&self, f: &FileRef, op: Op) -> Option<Action> {
        Some(match (op, f.section) {
            (Op::Stage, SectionId::Untracked | SectionId::Unstaged) => {
                Action::StageFile(f.path.clone())
            }
            // Unstage and staged-discard carry the parsed status entry: the
            // core dispatches on it (rename origins, unstaged edits) without
            // re-running `git status` per file.
            (Op::Unstage, SectionId::Staged) => Action::UnstageFile(self.file_entry(&f.path)?),
            (Op::Discard, SectionId::Untracked) => Action::DiscardUntracked(f.path.clone()),
            (Op::Discard, SectionId::Unstaged) => Action::DiscardTracked(f.path.clone()),
            (Op::Discard, SectionId::Staged) => {
                Action::DiscardStagedFile(self.file_entry(&f.path)?)
            }
            _ => return None,
        })
    }

    /// The parsed status entry for `path`, cloned for embedding in an [`Action`].
    fn file_entry(&self, path: &str) -> Option<FileEntry> {
        self.status
            .as_ref()?
            .entries
            .iter()
            .find(|e| e.path == path)
            .cloned()
    }

    /// Resolve the row at point + verb into a concrete git action, if the verb
    /// is meaningful there (e.g. you cannot stage something already staged).
    pub(crate) fn resolve_action(&self, op: Op) -> Option<Action> {
        let target = self.rows.get(self.selected)?.target.clone()?;
        // A conflicted path refuses unstage/discard, but `Stage` is allowed
        // through (its markers were already checked in `act`) so that staging a
        // manually-resolved conflict marks it resolved (`git add`).
        if op != Op::Stage && self.is_conflicted(target_path(&target)) {
            return None;
        }
        match (op, target) {
            // Whole-file staging (any verb) — shared with region selections.
            (op, Target::File(f)) => self.file_action(&f, op),

            // Stage: from the unstaged side.
            (Op::Stage, Target::Hunk { file, hunk }) if file.section == SectionId::Unstaged => {
                Some(Action::StageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Stage, Target::Line { file, hunk, line })
                if file.section == SectionId::Unstaged =>
            {
                Some(Action::StageLines(self.diff_for(&file)?, hunk, vec![line]))
            }

            // Unstage: from the staged side.
            (Op::Unstage, Target::Hunk { file, hunk }) if file.section == SectionId::Staged => {
                Some(Action::UnstageHunk(self.diff_for(&file)?, hunk))
            }
            (Op::Unstage, Target::Line { file, hunk, line })
                if file.section == SectionId::Staged =>
            {
                Some(Action::UnstageLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                ))
            }

            // Discard hunks/lines: unstaged reverts to the index, staged
            // reverts the index (whole-file discard is handled above).
            (Op::Discard, Target::Hunk { file, hunk }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardHunk(self.diff_for(&file)?, hunk)),
                SectionId::Staged => Some(Action::DiscardStagedHunk(self.diff_for(&file)?, hunk)),
                // Untracked has no hunks; commit/stash sections never reach here.
                _ => None,
            },
            (Op::Discard, Target::Line { file, hunk, line }) => match file.section {
                SectionId::Unstaged => Some(Action::DiscardLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                )),
                SectionId::Staged => Some(Action::DiscardStagedLines(
                    self.diff_for(&file)?,
                    hunk,
                    vec![line],
                )),
                _ => None,
            },

            _ => None,
        }
    }

    /// The inclusive row range of the active visual selection, if any.
    pub(crate) fn visual_range(&self) -> Option<(usize, usize)> {
        self.selection
            .visual
            .map(|anchor| (anchor.min(self.selected), anchor.max(self.selected)))
    }

    /// Copy the visual selection (rows joined by newlines), or the row at point
    /// when there's no selection, and flash a confirmation. Yanks the displayed
    /// text — for a diff line that's its content, without the `+`/`-` prefix.
    /// Exits visual mode (like an evil yank).
    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        // A mouse char selection (within one row's text) takes precedence.
        if let Some(sel) = self.char_sel.filter(|c| !c.is_empty()) {
            let slice = self
                .rows
                .get(sel.row)
                .and_then(|row| self.selectable_row_text(row))
                .map(|(text, _)| sel.slice(&text).to_string());
            if let Some(text) = slice {
                self.char_sel = None;
                self.copy_to_clipboard(text, cx);
                return;
            }
        }
        let text = if let Some((lo, hi)) = self.visual_range() {
            let hi = hi.min(self.rows.len().saturating_sub(1));
            self.rows[lo..=hi]
                .iter()
                .map(row_text)
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(row) = self.rows.get(self.selected) {
            // A single commit/stash row copies its value — the full hash or
            // stash reference (magit-copy-section-value) — not the row text.
            match &row.kind {
                RowKind::Commit { hash, .. } => hash.clone(),
                RowKind::Stash { reference, .. } => reference.clone(),
                _ => row_text(row),
            }
        } else {
            return;
        };
        self.selection.visual = None;
        self.copy_to_clipboard(text, cx);
    }

    /// Resolve a region (visual) selection into actions. Each file in the
    /// selection acts at the coarsest granularity it was selected with: a
    /// file-name row stages the whole file (even when its diff is collapsed),
    /// while selected hunks/lines act on just those. A selection spanning
    /// multiple files acts on *all* of them; parts whose section doesn't match
    /// the verb (e.g. a staged file when staging) are skipped.
    pub(crate) fn resolve_region_action(&self, op: Op) -> Option<Action> {
        let (lo, hi) = self.visual_range()?;

        /// The granularity at which a file in the selection should be acted on.
        /// A whole-file row wins over individual hunks/lines of the same file.
        enum Gran {
            Whole,
            Lines(HunkSelections),
        }
        fn add_lines(
            sels: &mut HunkSelections,
            hunk: usize,
            lines: impl IntoIterator<Item = usize>,
        ) {
            match sels.iter_mut().find(|(h, _)| *h == hunk) {
                Some((_, existing)) => existing.extend(lines),
                None => sels.push((hunk, lines.into_iter().collect())),
            }
        }

        // Collect per file (section+path), preserving encounter order.
        let mut files: Vec<(FileRef, Gran)> = Vec::new();
        let slot = |files: &mut Vec<(FileRef, Gran)>, f: &FileRef| -> usize {
            match files
                .iter()
                .position(|(g, _)| g.section == f.section && g.path == f.path)
            {
                Some(i) => i,
                None => {
                    files.push((f.clone(), Gran::Lines(Vec::new())));
                    files.len() - 1
                }
            }
        };
        for ix in lo..=hi {
            match self.rows.get(ix).and_then(|r| r.target.as_ref()) {
                Some(Target::File(f)) => {
                    let i = slot(&mut files, f);
                    files[i].1 = Gran::Whole;
                }
                Some(Target::Hunk { file, hunk }) => {
                    let i = slot(&mut files, file);
                    // Selecting a hunk header acts on the whole hunk: pull in
                    // every line index (the core ignores context lines).
                    if let Gran::Lines(sels) = &mut files[i].1 {
                        if let Some(h) = self.diff_for_ref(file).and_then(|d| d.hunks.get(*hunk)) {
                            add_lines(sels, *hunk, 0..h.lines.len());
                        }
                    }
                }
                Some(Target::Line { file, hunk, line }) => {
                    let i = slot(&mut files, file);
                    if let Gran::Lines(sels) = &mut files[i].1 {
                        add_lines(sels, *hunk, [*line]);
                    }
                }
                None => {}
            }
        }

        // Conflicted files in the region are handled up-front in `act` (the
        // whole action is refused), so none reach here.
        let mut actions = Vec::new();
        for (file, gran) in files {
            match gran {
                Gran::Whole => {
                    if let Some(a) = self.file_action(&file, op) {
                        actions.push(a);
                    }
                }
                Gran::Lines(mut selections) => {
                    let kind = match (op, file.section) {
                        (Op::Stage, SectionId::Unstaged) => RegionKind::Stage,
                        (Op::Unstage, SectionId::Staged) => RegionKind::Unstage,
                        (Op::Discard, SectionId::Unstaged) => RegionKind::Discard,
                        (Op::Discard, SectionId::Staged) => RegionKind::DiscardStaged,
                        _ => continue, // section doesn't match the verb
                    };
                    // A hunk header and its lines can both land in the range;
                    // collapse the duplicates.
                    for (_, lines) in &mut selections {
                        lines.sort_unstable();
                        lines.dedup();
                    }
                    if selections.iter().all(|(_, l)| l.is_empty()) {
                        continue;
                    }
                    let Some(diff) = self.diff_for(&file) else {
                        continue;
                    };
                    actions.push(Action::ApplyRegion {
                        kind,
                        file: diff,
                        selections,
                    });
                }
            }
        }

        match actions.len() {
            0 => None,
            1 => actions.pop(),
            _ => Some(Action::Batch(actions)),
        }
    }

    /// Open the file at point (its row, or the file a hunk/line belongs to) in
    /// the external editor, at the diff's line when one is known. Bound to Return.
    pub(crate) fn open_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rows.get(self.selected).and_then(|r| r.target.clone()) else {
            return;
        };
        let path = match &target {
            Target::File(f) => f.path.clone(),
            Target::Hunk { file, .. } | Target::Line { file, .. } => file.path.clone(),
        };
        let line = self.diff_target_line(&target);
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let full = repo.workdir().join(&path);
        self.launch_editor(&full, line);
        self.set_status(format!("Opening {path}"), true, cx);
    }

    /// The new-side line number to open at for a target: the line at point, or
    /// the first *changed* line of the hunk / the file's first hunk (so the
    /// editor lands on the edit, not the hunk's leading context).
    /// `None` when the diff isn't loaded (a collapsed file) — open without a line.
    pub(crate) fn diff_target_line(&self, target: &Target) -> Option<u32> {
        match target {
            Target::File(f) => self
                .diff_for_ref(f)?
                .hunks
                .first()
                .map(Hunk::first_change_new_line),
            Target::Hunk { file, hunk } => self
                .diff_for_ref(file)?
                .hunks
                .get(*hunk)
                .map(Hunk::first_change_new_line),
            Target::Line { file, hunk, line } => {
                let diff = self.diff_for_ref(file)?;
                let h = diff.hunks.get(*hunk)?;
                // A deleted line has no new-side number; fall back to the hunk.
                Some(
                    h.lines
                        .get(*line)
                        .and_then(|l| l.new_lineno)
                        .unwrap_or(h.new_start),
                )
            }
        }
    }

    /// Open `path` in the user's configured editor, at `line` when given and the
    /// editor's goto convention is known (see [`editor_goto`]). An empty `editor`
    /// opens the OS default app; otherwise `editor` is run as a command
    /// (`code -w`, `zed`) and, failing that on macOS, treated as an application
    /// name to `open -a` (so "Zed" or "Visual Studio Code" work too).
    /// Best-effort, non-blocking.
    pub(crate) fn launch_editor(&self, path: &std::path::Path, line: Option<u32>) {
        let editor = self.config.editor.trim();
        if editor.is_empty() {
            editor_launch::open_with_os(path);
            return;
        }
        // Open at the line when we have one and recognize how this editor takes
        // a goto target; otherwise fall through to a plain open.
        if let Some(line) = line {
            if let Some((program, args)) =
                editor_launch::editor_goto(editor, &path.to_string_lossy(), line)
            {
                if std::process::Command::new(program)
                    .args(args)
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
        // First try `editor` as a command: program + optional flags, then the
        // file. This is how CLI launchers are written (`code -w`, `zed`,
        // `subl -n`, `/abs/path/to/bin`).
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or(editor);
        let args: Vec<&str> = parts.collect();
        if std::process::Command::new(program)
            .args(args)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return;
        }
        // The command wasn't on PATH. On macOS the user likely typed an
        // application *name* ("Zed", "Visual Studio Code") rather than a CLI
        // command — open the file with that app via `open -a`. (`open` always
        // spawns; a bad app name just surfaces its own error.)
        #[cfg(target_os = "macos")]
        if std::process::Command::new("open")
            .arg("-a")
            .arg(editor)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return;
        }
        // Nothing launched — fall back to the OS default handler.
        editor_launch::open_with_os(path);
    }

    /// Resolve the conflicted file at point by keeping one side (`git checkout
    /// --ours|--theirs` + stage), then refresh.
    pub(crate) fn resolve_at_point(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        let Some(path) = self.conflicted_in_selection() else {
            return;
        };
        self.run_job(
            &format!("Resolving {path}…"),
            "Resolved",
            move |repo| repo.resolve_conflict(&path, side).map(|()| String::new()),
            cx,
        );
    }

    /// Labels for the take-ours / take-theirs conflict actions. git's `--ours`
    /// and `--theirs` swap meaning across operations: in a merge "ours" is the
    /// current branch and "theirs" the branch merged in, but a rebase/cherry-
    /// pick replays your commits onto the other side, so "ours" is the side
    /// already applied and "theirs" is the commit being replayed — the opposite
    /// of what intuition says. Name each side by what it actually is.
    pub(crate) fn conflict_side_labels(&self) -> (&'static str, &'static str) {
        match self.sequence.as_ref().map(|s| &s.kind) {
            Some(SequenceKind::Rebase) => ("Take ours (rebased onto)", "Take theirs (your commit)"),
            Some(SequenceKind::CherryPick) => {
                ("Take ours (HEAD)", "Take theirs (cherry-picked commit)")
            }
            Some(SequenceKind::Revert) => ("Take ours (HEAD)", "Take theirs (reverted commit)"),
            Some(SequenceKind::Am) => ("Take ours (HEAD)", "Take theirs (patch)"),
            Some(SequenceKind::Merge) | None => ("Take ours (current)", "Take theirs (incoming)"),
        }
    }

    /// Whether the worktree file at `path` still contains conflict markers, so
    /// staging it wouldn't silently mark an unresolved conflict resolved.
    pub(crate) fn has_conflict_markers(&self, path: &str) -> bool {
        let Some(repo) = self.repo.as_ref() else {
            return false;
        };
        // A read failure (unreadable, binary, gone) means we can't prove the
        // file is clean — be conservative and treat it as still-conflicted, so
        // staging can't silently mark an unresolved conflict resolved. (We match
        // only the `<<<<<<<`/`>>>>>>>` pair, not a bare `=======`, which occurs
        // in ordinary text and would false-positive.)
        std::fs::read_to_string(repo.workdir().join(path))
            .map(|c| {
                c.lines()
                    .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> "))
            })
            .unwrap_or(true)
    }

    /// `s`/`u`/`x`: resolve and either run, or (for discard) ask to confirm.
    /// `S`: stage all tracked changes (`git add -u`, magit's stage-modified).
    /// Confirms first when something is already staged — the operation blurs
    /// the staged/unstaged split the user has built up (magit confirms too).
    pub(crate) fn stage_all_command(&mut self, cx: &mut Context<Self>) {
        let anything_staged = self
            .status
            .as_ref()
            .is_some_and(|s| s.staged().next().is_some());
        if anything_staged {
            self.confirm = Some(("Stage all tracked changes?".to_string(), Confirm::StageAll));
            cx.notify();
        } else {
            self.run_action(Action::StageAll, cx);
        }
    }

    /// `U`: unstage everything. Errors when nothing is staged; confirms when
    /// unstaged or untracked changes exist alongside (magit's rule).
    pub(crate) fn unstage_all_command(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status.as_ref() else {
            return;
        };
        if status.staged().next().is_none() {
            self.set_status("Nothing to unstage".to_string(), false, cx);
            return;
        }
        let dirty = status.unstaged().next().is_some() || status.untracked().next().is_some();
        if dirty {
            self.confirm = Some(("Unstage all changes?".to_string(), Confirm::UnstageAll));
            cx.notify();
        } else {
            self.run_action(Action::UnstageAll, cx);
        }
    }

    pub(crate) fn act(&mut self, op: Op, cx: &mut Context<Self>) {
        // A conflicted file in the selection: staging it marks it resolved, so
        // allow that once its markers are gone (manual resolution); otherwise
        // refuse the whole action rather than act on a subset, and say why.
        if let Some(path) = self.conflicted_in_selection() {
            let resolvable = op == Op::Stage && !self.has_conflict_markers(&path);
            if !resolvable {
                let msg = if op == Op::Stage {
                    format!("{path} still has conflict markers — resolve them first")
                } else {
                    format!("{path} is conflicted — resolve it first")
                };
                self.set_status(msg, false, cx);
                return;
            }
        }
        // On a section header, the verb acts on the whole section (magit's
        // list scope): `s` on Untracked stages all untracked, `s` on Unstaged
        // stages all tracked changes, `u` on Staged unstages everything.
        if self.selection.visual.is_none() {
            if let Some(Row {
                kind: RowKind::Section { .. },
                fold: Some(FoldKey::Section(id)),
                ..
            }) = self.rows.get(self.selected)
            {
                match (op, id) {
                    (Op::Stage, SectionId::Untracked) => {
                        let paths: Vec<String> = self
                            .status
                            .as_ref()
                            .map(|s| s.untracked().map(|e| e.path.clone()).collect())
                            .unwrap_or_default();
                        self.run_action(Action::StageUntracked(paths), cx);
                        return;
                    }
                    (Op::Stage, SectionId::Unstaged) => {
                        self.stage_all_command(cx);
                        return;
                    }
                    (Op::Unstage, SectionId::Staged) => {
                        self.unstage_all_command(cx);
                        return;
                    }
                    _ => {}
                }
            }
        }
        let resolved = if self.selection.visual.is_some() {
            self.resolve_region_action(op)
        } else {
            self.resolve_action(op)
        };
        let Some(action) = resolved else {
            return;
        };
        if op == Op::Discard {
            self.confirm = Some((describe_discard(&action), Confirm::Action(action)));
        } else {
            self.run_action(action, cx);
        }
        cx.notify();
    }

    /// Untrack the file at point (`K`, `git rm --cached`): drop it from the
    /// index while keeping the working copy, so it becomes untracked (magit's
    /// `K`). Only a tracked file has something to untrack.
    pub(crate) fn untrack_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rows.get(self.selected).and_then(|r| r.target.as_ref()) else {
            return;
        };
        let section = match target {
            Target::File(f) => f.section,
            Target::Hunk { file, .. } | Target::Line { file, .. } => file.section,
        };
        let path = target_path(target).to_string();
        match section {
            SectionId::Untracked => {
                self.set_status(format!("{path} is already untracked"), false, cx)
            }
            SectionId::Ignored => self.set_status(format!("{path} is ignored"), false, cx),
            _ => self.run_action(Action::Untrack(path), cx),
        }
    }

    /// Run a git mutation on the background executor, then refresh.
    pub(crate) fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        self.confirm = None;
        self.selection.visual = None;
        let Some(repo) = self.repo.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action.run(&repo) })
                .await;
            this.update(cx, |this, cx| {
                // Status, not `error`: refresh() clears `error` at its top, so a
                // failure stored there would never be shown.
                match result {
                    Ok(()) => this.clear_status(cx),
                    Err(e) => this.set_status(format!("error: {e}"), false, cx),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Confirm a pending action (the `y` key or the confirm bar's "yes"
    /// button): run the destructive action, or proceed with a commit-all.
    pub(crate) fn confirm_yes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.confirm.take() {
            Some((_, Confirm::Action(action))) => self.run_action(action, cx),
            Some((_, Confirm::CommitAll(mut switches))) => {
                if !switches.iter().any(|s| s == "--all") {
                    switches.push("--all".into());
                }
                self.open_editor(CommitMode::Create, switches, window, cx);
            }
            Some((_, Confirm::AmendPushed(command, switches))) => {
                self.proceed_history_rewrite(command, switches, window, cx);
            }
            Some((_, Confirm::AbortSequence(kind))) => self.run_sequence(SeqOp::Abort, kind, cx),
            Some((_, Confirm::Reset(mode, target))) => self.do_reset(mode, target, cx),
            Some((_, Confirm::RebaseSincePushed { rev, args })) => {
                self.open_rebase_todo(format!("{rev}^"), args, cx)
            }
            Some((_, Confirm::RebaseRewordPushed { rev, args })) => {
                self.run_rebase_reword_from_rev(rev, args, window, cx)
            }
            Some((_, Confirm::AutosquashPublished { op, rev, args })) => {
                self.do_fixup_squash(op, rev, args, cx)
            }
            Some((_, Confirm::CustomShell { command, refresh })) => {
                self.run_custom_shell(command, refresh, cx)
            }
            Some((_, Confirm::DropStash(reference))) => {
                self.run_stash_action(StashAction::Drop, reference, cx)
            }
            Some((_, Confirm::RemoveWorktree(path))) => self.remove_worktree(path, cx),
            Some((_, Confirm::StageAll)) => self.run_action(Action::StageAll, cx),
            Some((_, Confirm::UnstageAll)) => self.run_action(Action::UnstageAll, cx),
            None => {}
        }
        cx.notify();
    }

    /// Cancel a pending destructive action (any other key, or the "no" button).
    pub(crate) fn confirm_no(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    // Visual-mode bar buttons (mirror the s/u/x/esc keys on the region).
    pub(crate) fn visual_stage(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Stage, cx);
    }

    pub(crate) fn visual_unstage(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Unstage, cx);
    }

    pub(crate) fn visual_discard(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.act(Op::Discard, cx);
    }

    pub(crate) fn visual_cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection.visual = None;
        cx.notify();
    }
}

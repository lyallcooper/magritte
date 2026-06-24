//! A reusable, keyboard-first searchable picker model over a flat list of
//! string choices — vertico-style: a prompt with the query typed inline, a
//! ranked candidate list, and the current candidate highlighted (mouse can
//! still click/scroll). This module owns only the *model* (filtering, ranking,
//! selection); the owner renders the prompt, input, and rows.
//!
//! Built for robustness on large lists (thousands of branches/commits):
//! filtering is allocation-free per keystroke — choices are lowercased once up
//! front, and scoring streams over chars without per-item allocation — and the
//! owner virtualizes the rows. Intended for the remote picker and future
//! pickers (branches, stashes, …).

use gpui::SharedString;

/// Whether (and how) a freely-typed value that isn't in the list is accepted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CreateMode {
    /// No free text — selection only.
    None,
    /// Offer only a `remote/branch`-form query (both parts non-empty) as a
    /// trailing "create" row, like magit's push target. A bare name just filters.
    RemoteBranch,
    /// Offer any non-empty query that isn't already a choice as a "create" row —
    /// for naming a new thing (e.g. a branch to create).
    Any,
    /// Plain value entry: the typed text is the value, candidates are mere
    /// suggestions. No "create" row and no "no match" placeholder — Enter takes
    /// the highlighted suggestion, or the typed text when nothing matches.
    Value,
}

/// One displayable row: an existing choice, or the "create" row for a freshly
/// typed value.
pub struct PickerRow {
    pub label: SharedString,
    /// True for the trailing "create" row (the typed value, shown as `… (new)`).
    pub is_create: bool,
}

/// The picker's filter/rank/select model over a flat list of string choices.
pub struct PickerList {
    choices: Vec<SharedString>,
    /// `choices` pre-lowercased, so per-keystroke scoring doesn't re-lowercase.
    lowered: Vec<String>,
    /// Indices into `choices` after filtering, in display (ranked) order.
    matched: Vec<usize>,
    /// Index of the highlighted row (into the visible rows: matched, then the
    /// optional trailing create row). Always valid when `row_count() > 0`.
    selected: usize,
    /// How a freely-typed value not in the list is offered (or not).
    create: CreateMode,
    /// The current (trimmed, original-case) query — the value the create row
    /// yields.
    query: String,
}

impl PickerList {
    pub fn new(choices: Vec<SharedString>, create: CreateMode) -> Self {
        let lowered = choices.iter().map(|c| c.to_lowercase()).collect();
        let matched = (0..choices.len()).collect();
        Self {
            choices,
            lowered,
            matched,
            selected: 0,
            create,
            query: String::new(),
        }
    }

    /// Re-filter and re-rank against `query`, resetting the highlight to the top
    /// (the best match, vertico-style).
    pub fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_string();
        let q = self.query.to_lowercase();
        if q.is_empty() {
            // No query: keep the caller's order (which it picks to be useful).
            self.matched = (0..self.choices.len()).collect();
        } else {
            // Fuzzy filter + rank (vertico-style): best match first.
            let mut scored: Vec<(i32, usize)> = self
                .lowered
                .iter()
                .enumerate()
                .filter_map(|(i, c)| fuzzy_score(c, &q).map(|s| (s, i)))
                .collect();
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| self.lowered[a.1].len().cmp(&self.lowered[b.1].len()))
                    .then(a.1.cmp(&b.1))
            });
            self.matched = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.selected = 0;
    }

    /// Whether a "create" row for the typed query is shown. The query must not
    /// already be a choice; `RemoteBranch` additionally requires the
    /// `remote/branch` form (both parts non-empty), mirroring magit.
    fn create_row(&self) -> bool {
        let novel =
            !self.query.is_empty() && !self.choices.iter().any(|c| c.as_ref() == self.query);
        match self.create {
            CreateMode::None | CreateMode::Value => false,
            CreateMode::Any => novel,
            CreateMode::RemoteBranch => {
                novel
                    && matches!(
                        self.query.split_once('/'),
                        Some((remote, branch)) if !remote.is_empty() && !branch.is_empty()
                    )
            }
        }
    }

    /// Whether this is a plain value-entry picker (no "create" row, no "no
    /// match" placeholder — the typed text is itself a valid answer).
    pub fn is_value_entry(&self) -> bool {
        self.create == CreateMode::Value
    }

    /// Number of visible rows (matches plus the optional create row).
    pub fn row_count(&self) -> usize {
        self.matched.len() + usize::from(self.create_row())
    }

    /// The highlighted row index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Move the highlight by `delta` rows, wrapping around (vertico-style).
    pub fn move_by(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let cur = self.selected as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }

    /// Highlight a specific row (e.g. from a mouse hover/click).
    pub fn set_selected(&mut self, row: usize) {
        if row < self.row_count() {
            self.selected = row;
        }
    }

    /// The row at `row`, if any.
    pub fn row(&self, row: usize) -> Option<PickerRow> {
        if let Some(&i) = self.matched.get(row) {
            Some(PickerRow {
                label: self.choices[i].clone(),
                is_create: false,
            })
        } else if self.create_row() && row == self.matched.len() {
            Some(PickerRow {
                label: SharedString::from(self.query.clone()),
                is_create: true,
            })
        } else {
            None
        }
    }

    /// The highlighted choice: an existing match (or the "create" row's typed
    /// value). In value-entry mode with nothing matching, the typed query is
    /// itself the answer, so it's returned even with no visible row.
    pub fn selected_choice(&self) -> Option<SharedString> {
        if let Some(row) = self.row(self.selected) {
            return Some(row.label);
        }
        if self.is_value_entry() {
            return Some(SharedString::from(self.query.clone()));
        }
        None
    }
}

/// Fuzzy (subsequence) match score; `None` when `query` isn't a subsequence of
/// `text`. Both must already be lowercased (the model does this once per choice
/// and once per keystroke). Higher is better: bonuses for matches at the start
/// or just after a separator and for contiguous runs; penalties for gaps and
/// for longer candidates.
///
/// Streams over chars with no per-call allocation, so filtering a large list
/// each keystroke stays cheap.
fn fuzzy_score(text: &str, query: &str) -> Option<i32> {
    let mut needle = query.chars();
    let Some(mut want) = needle.next() else {
        return Some(0); // empty query matches everything
    };
    let mut score = 0i32;
    let mut len = 0i32;
    let mut prev: Option<char> = None;
    // Chars since the last match; -1 until the first match.
    let mut gap: i32 = -1;
    let mut done = false; // all query chars consumed
    for c in text.chars() {
        len += 1;
        if !done && c == want {
            score += 1;
            let boundary = prev.is_none_or(|p| matches!(p, '/' | '-' | '_' | ' ' | '.' | ':'));
            if boundary {
                score += 10;
            }
            if gap == 0 {
                score += 8; // contiguous with the previous match
            } else if gap > 0 {
                score -= gap.min(5); // gap penalty (capped)
            }
            gap = 0;
            match needle.next() {
                Some(n) => want = n,
                None => done = true,
            }
        } else if gap >= 0 {
            gap += 1;
        }
        prev = Some(c);
    }
    if !done {
        return None; // didn't consume the whole query → not a subsequence
    }
    score -= len / 5; // mild preference for shorter matches
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_score, CreateMode, PickerList};
    use gpui::SharedString;

    fn list(choices: &[&str], create: CreateMode) -> PickerList {
        PickerList::new(
            choices.iter().map(|c| SharedString::from(*c)).collect(),
            create,
        )
    }

    #[test]
    fn empty_query_keeps_order_and_selects_first() {
        let l = list(&["origin/main", "backup/main"], CreateMode::None);
        assert_eq!(l.row_count(), 2);
        assert_eq!(l.selected_choice().as_deref(), Some("origin/main"));
    }

    #[test]
    fn fuzzy_ranks_best_match_first() {
        let mut l = list(
            &["origin/zztest", "origin/main", "backup/main"],
            CreateMode::None,
        );
        l.set_query("main");
        // Both `*/main` match; the create row is off, and the top match is
        // auto-selected.
        assert!(l.row_count() >= 2);
        assert!(l
            .selected_choice()
            .as_deref()
            .is_some_and(|s| s.ends_with("/main")));
    }

    #[test]
    fn create_row_requires_remote_slash_branch_form() {
        let mut l = list(&["origin/master", "origin/dev"], CreateMode::RemoteBranch);

        // A bare name is never offered as new (magit requires REMOTE/BRANCH);
        // it just filters the candidates instead.
        l.set_query("feature");
        assert_eq!(l.row_count(), 0);

        // A `remote/branch` form not already a choice is offered, verbatim, and
        // auto-selected as the only row.
        l.set_query("origin/feature");
        assert_eq!(l.row_count(), 1);
        assert_eq!(l.selected_choice().as_deref(), Some("origin/feature"));

        // …but not when it already exists.
        l.set_query("origin/master");
        assert!(l
            .row(l.row_count().saturating_sub(1))
            .is_some_and(|r| !r.is_create));
    }

    #[test]
    fn create_mode_any_offers_any_novel_name() {
        let mut l = list(&["main", "dev"], CreateMode::Any);
        // A bare name not in the list is offered as a new entry, verbatim.
        l.set_query("feature");
        assert_eq!(l.selected_choice().as_deref(), Some("feature"));
        assert!(l.row(0).is_some_and(|r| r.is_create));
        // An existing name is not offered as new.
        l.set_query("main");
        assert!(l.row(0).is_some_and(|r| !r.is_create));
    }

    #[test]
    fn move_by_wraps_around() {
        let mut l = list(&["a/x", "b/x", "c/x"], CreateMode::None);
        assert_eq!(l.selected(), 0);
        l.move_by(-1);
        assert_eq!(l.selected(), 2, "up from the top wraps to the bottom");
        l.move_by(1);
        assert_eq!(l.selected(), 0, "down from the bottom wraps to the top");
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(fuzzy_score("backup", "or").is_none()); // no 'o' in "backup"
        assert!(fuzzy_score("origin", "ox").is_none());
    }

    #[test]
    fn contiguous_and_boundary_matches_rank_higher() {
        // Contiguous prefix beats a scattered match of the same query.
        let tight = fuzzy_score("origin", "ori").unwrap();
        let loose = fuzzy_score("organic-input", "ori").unwrap();
        assert!(tight > loose, "tight {tight} should beat loose {loose}");

        // A match right after a separator scores better than mid-word.
        let boundary = fuzzy_score("my-upstream", "u").unwrap();
        let midword = fuzzy_score("aaauck", "u").unwrap();
        assert!(boundary > midword);
    }
}

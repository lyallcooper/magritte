//! A reusable, keyboard-driven searchable picker over a flat list of string
//! choices, built on gpui-component's `List` (a search input + filtered list).
//!
//! Focusing the list focuses its search field, so it's type-to-filter the
//! moment it appears — fully keyboard-driven, no synthetic keypresses. The
//! owner renders `List::new(&state)` and subscribes to `ListEvent` (Confirm /
//! Cancel), reading the choice via [`ChoiceDelegate::selected_choice`]. Intended
//! for the remote picker and future pickers (branches, stashes, …).

use gpui::{App, Context, ParentElement as _, SharedString, Task, Window};
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::IndexPath;

/// A `ListDelegate` over a flat list of string choices with fuzzy search.
///
/// Built for robustness on large lists (thousands of branches/commits): the
/// list rendering is virtualized by `List`, and filtering is allocation-free
/// per keystroke — choices are lowercased once up front, and scoring streams
/// over chars without per-item allocation.
pub struct ChoiceDelegate {
    choices: Vec<SharedString>,
    /// `choices` pre-lowercased, so per-keystroke scoring doesn't re-lowercase.
    lowered: Vec<String>,
    /// Indices into `choices` after filtering, in display (ranked) order.
    matched: Vec<usize>,
    /// Index into `matched` (or the trailing create row) of the highlighted row.
    selected: Option<usize>,
    /// When set, a query of the form `remote/branch` that isn't already a choice
    /// offers a trailing "create" row yielding that ref verbatim (e.g. a new
    /// branch to push to). Like magit, a bare name (no `/`) is *not* offered —
    /// it just filters the candidates (which include the seeded same-named
    /// targets), so you reach `origin/master` by selecting it, not creating it.
    allow_create: bool,
    /// The current (trimmed, original-case) query — the value the create row
    /// yields.
    query: String,
}

impl ChoiceDelegate {
    pub fn new(choices: Vec<SharedString>) -> Self {
        let lowered = choices.iter().map(|c| c.to_lowercase()).collect();
        let matched = (0..choices.len()).collect();
        let selected = (!choices.is_empty()).then_some(0);
        Self {
            choices,
            lowered,
            matched,
            selected,
            allow_create: false,
            query: String::new(),
        }
    }

    /// Allow choosing a freely-typed `remote/branch` value that isn't in the
    /// list (shown as a trailing "create" row).
    pub fn allow_create(mut self, yes: bool) -> Self {
        self.allow_create = yes;
        self
    }

    /// Whether a "create" row for the typed query is currently shown — only for
    /// a `remote/branch`-form query (both parts non-empty) that isn't already a
    /// choice, mirroring magit's requirement that the target name that form.
    fn create_row(&self) -> bool {
        if !self.allow_create {
            return false;
        }
        match self.query.split_once('/') {
            Some((remote, branch)) if !remote.is_empty() && !branch.is_empty() => {
                !self.choices.iter().any(|c| c.as_ref() == self.query)
            }
            _ => false,
        }
    }

    /// The currently highlighted choice — an existing one, or the typed
    /// `remote/branch` value when the create row is selected.
    pub fn selected_choice(&self) -> Option<SharedString> {
        let row = self.selected?;
        match self.matched.get(row) {
            Some(&i) => Some(self.choices[i].clone()),
            None if self.create_row() && row == self.matched.len() => {
                Some(SharedString::from(self.query.clone()))
            }
            None => None,
        }
    }
}

impl ListDelegate for ChoiceDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matched.len() + usize::from(self.create_row())
    }

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
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
        // The List re-selects the first row after a search, so the best match is
        // auto-selected; keep our mirror in sync for the initial render.
        let rows = self.matched.len() + usize::from(self.create_row());
        self.selected = (rows > 0).then_some(0);
        Task::ready(())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let selected = self.selected == Some(ix.row);
        // The trailing create row (when the query matches nothing existing).
        if ix.row == self.matched.len() && self.create_row() {
            return Some(
                ListItem::new(ix.row)
                    .selected(selected)
                    .child(SharedString::from(format!("{}  (new)", self.query))),
            );
        }
        let &choice_ix = self.matched.get(ix.row)?;
        Some(
            ListItem::new(ix.row)
                .selected(selected)
                .child(self.choices[choice_ix].clone()),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|i| i.row);
        cx.notify();
    }
}

/// Fuzzy (subsequence) match score; `None` when `query` isn't a subsequence of
/// `text`. Both must already be lowercased (the delegate does this once per
/// choice and once per keystroke). Higher is better: bonuses for matches at the
/// start or just after a separator and for contiguous runs; penalties for gaps
/// and for longer candidates.
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
    use super::{fuzzy_score, ChoiceDelegate};
    use gpui::SharedString;

    fn delegate(choices: &[&str]) -> ChoiceDelegate {
        ChoiceDelegate::new(choices.iter().map(|c| SharedString::from(*c)).collect())
    }

    #[test]
    fn create_row_requires_remote_slash_branch_form() {
        let mut d = delegate(&["origin/master", "origin/dev"]).allow_create(true);

        // A bare name is never offered as new (magit requires REMOTE/BRANCH);
        // it just filters the candidates instead.
        d.query = "feature".to_string();
        assert!(!d.create_row());

        // A `remote/branch` form not already a choice is offered.
        d.query = "origin/feature".to_string();
        assert!(d.create_row());

        // …but not when it already exists.
        d.query = "origin/master".to_string();
        assert!(!d.create_row());

        // When the create row is the highlighted row, it yields the query
        // verbatim (here: nothing else matched, so it sits at index 0).
        d.query = "origin/feature".to_string();
        d.matched = vec![];
        d.selected = Some(0);
        assert_eq!(d.selected_choice().as_deref(), Some("origin/feature"));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(fuzzy_score("backup", "or").is_none()); // no 'o' in "backup"
        assert!(fuzzy_score("origin", "ox").is_none());
    }

    #[test]
    fn empty_query_matches() {
        // Both inputs are pre-lowercased by the delegate; an empty query is the
        // "show everything" case.
        assert_eq!(fuzzy_score("origin", ""), Some(0));
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

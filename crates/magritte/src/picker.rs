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

/// A `ListDelegate` over a flat list of string choices with substring search.
pub struct ChoiceDelegate {
    choices: Vec<SharedString>,
    /// Indices into `choices` after filtering by the query.
    matched: Vec<usize>,
    /// Index into `matched` of the highlighted row.
    selected: Option<usize>,
}

impl ChoiceDelegate {
    pub fn new(choices: Vec<SharedString>) -> Self {
        let matched = (0..choices.len()).collect();
        let selected = (!choices.is_empty()).then_some(0);
        Self {
            choices,
            matched,
            selected,
        }
    }

    /// The currently highlighted choice, if any.
    pub fn selected_choice(&self) -> Option<SharedString> {
        let row = self.selected?;
        self.matched.get(row).map(|&i| self.choices[i].clone())
    }
}

impl ListDelegate for ChoiceDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        let q = query.trim();
        if q.is_empty() {
            // No query: keep the caller's order (which it picks to be useful).
            self.matched = (0..self.choices.len()).collect();
        } else {
            // Fuzzy filter + rank (vertico-style): best match first.
            let mut scored: Vec<(i32, usize)> = self
                .choices
                .iter()
                .enumerate()
                .filter_map(|(i, c)| fuzzy_score(c, q).map(|s| (s, i)))
                .collect();
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| self.choices[a.1].len().cmp(&self.choices[b.1].len()))
                    .then(a.1.cmp(&b.1))
            });
            self.matched = scored.into_iter().map(|(_, i)| i).collect();
        }
        // The List re-selects the first row after a search, so the best match is
        // auto-selected; keep our mirror in sync for the initial render.
        self.selected = (!self.matched.is_empty()).then_some(0);
        Task::ready(())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let &choice_ix = self.matched.get(ix.row)?;
        let selected = self.selected == Some(ix.row);
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

/// Case-insensitive fuzzy (subsequence) match score; `None` when `query` isn't
/// a subsequence of `text`. Higher is better: bonuses for matches at the start
/// or just after a separator and for contiguous runs; penalties for gaps and
/// for longer candidates. Good enough for the short lists these pickers show.
fn fuzzy_score(text: &str, query: &str) -> Option<i32> {
    let tl: Vec<char> = text.to_lowercase().chars().collect();
    let ql: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut last: Option<usize> = None;
    for (i, &c) in tl.iter().enumerate() {
        if qi >= ql.len() {
            break;
        }
        if c == ql[qi] {
            score += 1;
            let boundary = i == 0 || matches!(tl[i - 1], '/' | '-' | '_' | ' ' | '.' | ':');
            if boundary {
                score += 10;
            }
            match last {
                Some(p) if p + 1 == i => score += 8, // contiguous run
                Some(p) => score -= ((i - p - 1) as i32).min(5), // gap (capped)
                None => {}
            }
            last = Some(i);
            qi += 1;
        }
    }
    if qi != ql.len() {
        return None; // not a subsequence
    }
    score -= tl.len() as i32 / 5; // mild preference for shorter matches
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(fuzzy_score("backup", "or").is_none()); // no 'o' in "backup"
        assert!(fuzzy_score("origin", "ox").is_none());
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(fuzzy_score("Origin", "or").is_some());
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

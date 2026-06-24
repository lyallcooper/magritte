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
        let q = query.trim().to_lowercase();
        self.matched = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, c)| q.is_empty() || c.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
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

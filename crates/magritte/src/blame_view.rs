//! The blame view: `git blame` for the file at point, rendered as a scrollable
//! list of annotated lines. A pager like the command log (no cursor). `impl
//! StatusView` like the other view slices.

use std::rc::Rc;

use gpui::{Context, UniformListScrollHandle, Window};

use crate::{PagerSelection, Screen, ScrollView, StatusView};

/// A row in the blame pager: an inline commit annotation (shown once per commit
/// run, magit-style) or a file line with its number.
pub(crate) enum BlameRow {
    Annotation {
        short: String,
        author: String,
        date: String,
        summary: String,
    },
    Line {
        line_no: u32,
        text: String,
    },
}

fn flatten_blame(lines: Vec<magritte_core::BlameLine>) -> Vec<BlameRow> {
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        if line.group_start {
            rows.push(BlameRow::Annotation {
                short: line.short,
                author: line.author,
                date: line.date,
                summary: line.summary,
            });
        }
        rows.push(BlameRow::Line {
            line_no: line.line_no,
            text: line.text,
        });
    }
    rows
}

impl StatusView {
    /// Blame the file at point (magit's `git blame`), loading annotations off the
    /// UI thread and opening the blame view. A no-op with a notice when the
    /// cursor isn't on a file.
    pub(crate) fn open_blame(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path_at_point() else {
            self.set_status("No file at point to blame".to_string(), true, cx);
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.clear_status(cx);
        let load_path = path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.blame(&load_path) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(lines) => {
                    let rows = flatten_blame(lines);
                    this.pager_sel = PagerSelection::default();
                    this.screen = Screen::Blame {
                        view: ScrollView {
                            scroll: UniformListScrollHandle::new(),
                            top: 0,
                        },
                        path,
                        rows: Rc::new(rows),
                    };
                    cx.notify();
                }
                Err(e) => this.set_status(format!("blame failed: {e}"), true, cx),
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn reload_blame_silent(&mut self, cx: &mut Context<Self>) {
        let Screen::Blame { view, path, rows } = &self.screen else {
            return;
        };
        let path = path.clone();
        let top_line = rows.iter().skip(view.top).find_map(|row| match row {
            BlameRow::Line { line_no, .. } => Some(*line_no),
            BlameRow::Annotation { .. } => None,
        });
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        let load_path = path.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.blame(&load_path) })
                .await;
            let Ok(lines) = result else { return };
            let rows = flatten_blame(lines);
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) {
                    return;
                }
                let Screen::Blame {
                    view,
                    path: current_path,
                    rows: current_rows,
                } = &mut this.screen
                else {
                    return;
                };
                if *current_path != path {
                    return;
                }
                let top = top_line
                    .and_then(|line| {
                        rows.iter().position(
                            |row| matches!(row, BlameRow::Line { line_no, .. } if *line_no == line),
                        )
                    })
                    .unwrap_or(0);
                view.top = top;
                view.scroll.scroll_to_item(top, gpui::ScrollStrategy::Top);
                *current_rows = Rc::new(rows);
                this.pager_sel = PagerSelection::default();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn close_blame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.reconcile_visible_screen(cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
}

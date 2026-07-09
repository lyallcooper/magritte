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
                    // Flatten to rows: an annotation before each commit run, then
                    // the file lines (magit's inline blame).
                    let mut rows = Vec::with_capacity(lines.len());
                    for l in lines {
                        if l.group_start {
                            rows.push(BlameRow::Annotation {
                                short: l.short,
                                author: l.author,
                                date: l.date,
                                summary: l.summary,
                            });
                        }
                        rows.push(BlameRow::Line {
                            line_no: l.line_no,
                            text: l.text,
                        });
                    }
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

    pub(crate) fn close_blame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }
}

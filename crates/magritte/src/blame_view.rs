//! The blame view: `git blame` for the file at point, rendered as a scrollable
//! list of annotated lines. A pager like the command log (no cursor). `impl
//! StatusView` like the other view slices.

use std::rc::Rc;

use gpui::{Context, UniformListScrollHandle, Window};

use crate::{Screen, ScrollView, StatusView};

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
                    this.screen = Screen::Blame {
                        view: ScrollView {
                            scroll: UniformListScrollHandle::new(),
                            top: 0,
                        },
                        path,
                        lines: Rc::new(lines),
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

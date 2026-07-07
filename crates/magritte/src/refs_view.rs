//! The refs browser (`y`, magit's `magit-show-refs`): a scrollable listing of
//! local branches, remote-tracking branches, and tags with a cursor and
//! act-at-point verbs (Return visits the tip commit, `b` checks out, plus
//! delete/rename). `impl StatusView` like the other view slices; the list is a
//! flat `Vec<RefsRow>` so section headers and refs share one uniform list.

use gpui::{Context, UniformListScrollHandle, Window};
use magritte_core::{LocalBranch, Repo};

use crate::*;

/// One row of the refs browser: a section header (not selectable) or a ref of a
/// given kind. The kind drives both the act-at-point verb and the coloring.
pub(crate) enum RefsRow {
    Header(&'static str),
    Local {
        name: String,
        current: bool,
        /// Divergence from the branch's upstream (0/0 when in sync or no
        /// upstream), shown as an `↑ahead ↓behind` margin.
        ahead: u32,
        behind: u32,
    },
    Remote(String),
    Tag(String),
}

impl RefsRow {
    fn is_selectable(&self) -> bool {
        !matches!(self, RefsRow::Header(_))
    }

    /// The ref name to act on, if this row is a ref.
    pub(crate) fn ref_name(&self) -> Option<&str> {
        match self {
            RefsRow::Header(_) => None,
            RefsRow::Local { name, .. } => Some(name),
            RefsRow::Remote(name) | RefsRow::Tag(name) => Some(name),
        }
    }
}

/// The refs the browser lists, gathered in one background pass.
pub(crate) struct RefsData {
    pub(crate) current: Option<String>,
    pub(crate) locals: Vec<LocalBranch>,
    pub(crate) remotes: Vec<String>,
    pub(crate) tags: Vec<String>,
}

/// Load state, so the body distinguishes still-loading from a load error from a
/// genuinely empty repo.
pub(crate) enum RefsLoad {
    Loading,
    Loaded,
    Failed(String),
}

/// The refs browser screen: rows plus a cursor that skips headers.
pub(crate) struct RefsView {
    pub(crate) rows: Vec<RefsRow>,
    pub(crate) selected: usize,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) load: RefsLoad,
}

impl RefsView {
    /// The ref at the cursor, if the selected row is a ref.
    pub(crate) fn selected_row(&self) -> Option<&RefsRow> {
        self.rows.get(self.selected)
    }
}

/// Flatten the gathered refs into display rows: only non-empty sections get a
/// header, so an empty repo shows nothing rather than three empty headings.
fn build_rows(data: RefsData) -> Vec<RefsRow> {
    let mut rows = Vec::new();
    if !data.locals.is_empty() {
        rows.push(RefsRow::Header("Branches"));
        for b in data.locals {
            let current = data.current.as_deref() == Some(b.name.as_str());
            rows.push(RefsRow::Local {
                name: b.name,
                current,
                ahead: b.ahead,
                behind: b.behind,
            });
        }
    }
    if !data.remotes.is_empty() {
        rows.push(RefsRow::Header("Remotes"));
        rows.extend(data.remotes.into_iter().map(RefsRow::Remote));
    }
    if !data.tags.is_empty() {
        rows.push(RefsRow::Header("Tags"));
        rows.extend(data.tags.into_iter().map(RefsRow::Tag));
    }
    rows
}

impl StatusView {
    pub(crate) fn refs_view(&self) -> Option<&RefsView> {
        match &self.screen {
            Screen::Refs(r) => Some(r),
            _ => None,
        }
    }

    pub(crate) fn refs_view_mut(&mut self) -> Option<&mut RefsView> {
        match &mut self.screen {
            Screen::Refs(r) => Some(r),
            _ => None,
        }
    }

    /// Open the refs browser: show it (loading) immediately, then gather the
    /// branch/remote/tag lists off the UI thread and fill it in. The screen-load
    /// generation guards a superseded open from populating a newer screen.
    pub(crate) fn open_refs(&mut self, cx: &mut Context<Self>) {
        self.clear_status(cx);
        self.screen = Screen::Refs(RefsView {
            rows: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            load: RefsLoad::Loading,
        });
        cx.notify();
        self.load_refs(cx);
    }

    /// (Re)gather the ref lists into the open browser off the UI thread — used
    /// on open and after a rename. The screen-load generation drops a superseded
    /// load.
    fn load_refs(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let gen = self.next_screen_gen();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { gather_refs(&repo) })
                .await;
            this.update(cx, |this, cx| this.fill_refs(gen, result, cx))
                .ok();
        })
        .detach();
    }

    fn fill_refs(
        &mut self,
        gen: u64,
        result: magritte_core::Result<RefsData>,
        cx: &mut Context<Self>,
    ) {
        if !self.screen_gen.is_current(gen) {
            return;
        }
        if let Some(refs) = self.refs_view_mut() {
            match result {
                Ok(data) => {
                    refs.rows = build_rows(data);
                    // Land the cursor on the first selectable row (past the
                    // leading header).
                    refs.selected = refs
                        .rows
                        .iter()
                        .position(RefsRow::is_selectable)
                        .unwrap_or(0);
                    refs.load = RefsLoad::Loaded;
                }
                Err(e) => refs.load = RefsLoad::Failed(e.to_string()),
            }
        }
        cx.notify();
    }

    pub(crate) fn close_refs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Move the cursor by `delta` rows, skipping section headers, and keep it in
    /// view.
    pub(crate) fn refs_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(refs) = self.refs_view_mut() else {
            return;
        };
        let rows = &refs.rows;
        let Some(ix) = list_move(refs.selected, rows.len(), delta, |i| {
            rows[i].is_selectable()
        }) else {
            return;
        };
        refs.selected = ix;
        refs.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Visit the ref at point (Return — magit's `magit-visit-ref` default):
    /// open its tip commit's detail over the browser, without touching the
    /// checkout. The tip hash + subject resolve off the UI thread first.
    pub(crate) fn refs_visit_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self
            .refs_view()
            .and_then(RefsView::selected_row)
            .and_then(RefsRow::ref_name)
            .map(str::to_string)
        else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let entry = cx
                .background_executor()
                .spawn(async move { repo.log(&name, 1).ok().and_then(|mut l| l.pop()) })
                .await;
            this.update(cx, |this, cx| {
                // Only if the browser is still up — the commit view opens over
                // it (Esc returns), so a superseded screen must not be covered.
                if this.refs_view().is_none() {
                    return;
                }
                match entry {
                    Some(e) => this.open_commit(e.hash, e.subject, cx),
                    None => this.set_status("Could not resolve ref".to_string(), false, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Check out the ref at point (`b`): a local branch is switched to, a
    /// remote-tracking ref DWIMs into a local tracking branch, a tag detaches
    /// HEAD — all handled by [`Repo::checkout`].
    pub(crate) fn refs_checkout_at_point(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .refs_view()
            .and_then(RefsView::selected_row)
            .and_then(RefsRow::ref_name)
            .map(str::to_string)
        else {
            return;
        };
        self.close_refs(window, cx);
        self.run_job(
            &format!("Checking out {name}…"),
            "Checked out",
            move |repo| repo.checkout(&name),
            cx,
        );
    }

    /// Delete the ref at point. Local branches and tags delete directly (git
    /// refuses an unmerged branch, surfaced in the report); a remote-tracking
    /// ref isn't a local ref to delete, so point at the push transient instead.
    pub(crate) fn refs_delete_at_point(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.refs_view().and_then(RefsView::selected_row) else {
            return;
        };
        match row {
            RefsRow::Local { name, current, .. } => {
                if *current {
                    self.set_status("Can't delete the current branch".to_string(), false, cx);
                    return;
                }
                let name = name.clone();
                self.close_refs(window, cx);
                self.run_job(
                    &format!("Deleting branch {name}…"),
                    "Deleted branch",
                    move |repo| repo.delete_branch(&name, false),
                    cx,
                );
            }
            RefsRow::Tag(name) => {
                let name = name.clone();
                self.close_refs(window, cx);
                self.run_job(
                    &format!("Deleting tag {name}…"),
                    "Deleted tag",
                    move |repo| repo.delete_tag(&name),
                    cx,
                );
            }
            RefsRow::Remote(_) => self.set_status(
                "Delete a remote branch from the push transient (P k)".to_string(),
                false,
                cx,
            ),
            RefsRow::Header(_) => {}
        }
    }

    /// Rename the local branch at point (`R`): prompt for the new name over the
    /// browser (staying on it, so the picker's input keeps focus), then rename
    /// and reload the list. Only local branches can be renamed.
    pub(crate) fn refs_rename_at_point(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(RefsRow::Local { name, .. }) = self.refs_view().and_then(RefsView::selected_row)
        else {
            return;
        };
        let old = name.clone();
        self.open_picker(
            PickerAction::RefsRename { old },
            Vec::new(),
            CreateMode::Any,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Carry out a confirmed refs-browser rename, then reload the list. A blank
    /// or unchanged name is a no-op (an accidental Return).
    pub(crate) fn do_refs_rename(&mut self, old: String, new: String, cx: &mut Context<Self>) {
        let new = new.trim().to_string();
        if new.is_empty() || new == old {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.set_progress(format!("Renaming {old}…"), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.rename_branch(&old, &new) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(msg) => {
                    this.set_status(msg, true, cx);
                    this.load_refs(cx);
                }
                Err(e) => this.report_error(e, cx),
            })
            .ok();
        })
        .detach();
    }
}

/// Gather the branch/remote/tag lists in one background pass. A missing current
/// branch (detached HEAD) is fine — nothing is marked current.
fn gather_refs(repo: &Repo) -> magritte_core::Result<RefsData> {
    Ok(RefsData {
        current: repo.current_branch()?,
        locals: repo.local_branches_tracking()?,
        remotes: repo.remote_branches()?,
        tags: repo.tags()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn local(name: &str, ahead: u32, behind: u32) -> LocalBranch {
        LocalBranch {
            name: name.to_string(),
            ahead,
            behind,
        }
    }

    #[test]
    fn build_rows_headers_only_for_nonempty_sections() {
        // No remotes: the Remotes header is skipped entirely, the current branch
        // is marked, and ahead/behind ride along.
        let rows = build_rows(RefsData {
            current: Some("main".to_string()),
            locals: vec![local("main", 0, 0), local("dev", 2, 1)],
            remotes: Vec::new(),
            tags: s(&["v1.0.0"]),
        });
        let shape: Vec<_> = rows
            .iter()
            .map(|r| match r {
                RefsRow::Header(h) => format!("#{h}"),
                RefsRow::Local {
                    name,
                    current,
                    ahead,
                    behind,
                } => format!("{name}{}+{ahead}-{behind}", if *current { "*" } else { "" }),
                RefsRow::Remote(n) => format!("r:{n}"),
                RefsRow::Tag(n) => format!("t:{n}"),
            })
            .collect();
        assert_eq!(
            shape,
            vec!["#Branches", "main*+0-0", "dev+2-1", "#Tags", "t:v1.0.0"]
        );
    }

    #[test]
    fn build_rows_empty_repo_is_empty() {
        let rows = build_rows(RefsData {
            current: None,
            locals: Vec::new(),
            remotes: Vec::new(),
            tags: Vec::new(),
        });
        assert!(rows.is_empty());
    }
}

//! Magritte — M1: a GPUI window that renders the live git status of the
//! current working tree.
//!
//! This milestone exists to de-risk the GPUI dependency and to prove the
//! async pipeline that the whole project depends on: the UI thread never runs
//! git. On launch we spawn the (synchronous) `magritte-core` status call on the
//! background executor, then update the view when it completes. Virtualized
//! rendering and the collapsible section tree arrive in M2 — for now the list
//! is rendered plainly.

use gpui::{
    div, px, AnyElement, App, AppContext, Application, Context, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowOptions,
};
use magritte_core::{Change, EntryKind, FileEntry, Repo, Status};

/// Zed-ish dark palette, enough to make M1 look intentional.
mod theme {
    use gpui::{rgb, Rgba};
    pub fn bg() -> Rgba {
        rgb(0x1e2025)
    }
    pub fn fg() -> Rgba {
        rgb(0xced2da)
    }
    pub fn dim() -> Rgba {
        rgb(0x7f8694)
    }
    pub fn section() -> Rgba {
        rgb(0x7aa2f7)
    }
    pub fn added() -> Rgba {
        rgb(0x9ece6a)
    }
    pub fn modified() -> Rgba {
        rgb(0xe0af68)
    }
    pub fn deleted() -> Rgba {
        rgb(0xf7768e)
    }
}

/// Async load state for the status view.
enum Load {
    Loading,
    Loaded(Status),
    Failed(String),
}

struct StatusView {
    load: Load,
}

impl StatusView {
    fn new(cx: &mut Context<Self>) -> Self {
        // Spawn the git call off the UI thread. `this` is a weak handle to this
        // view; `cx` is an async context that survives across the await.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_status() })
                .await;
            this.update(cx, |this, cx| {
                this.load = match result {
                    Ok(status) => Load::Loaded(status),
                    Err(message) => Load::Failed(message),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();

        StatusView { load: Load::Loading }
    }
}

/// Synchronous: discover the repo at the cwd and read its status. Runs on the
/// background executor, never on the UI thread.
fn load_status() -> Result<Status, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let repo = Repo::discover(&cwd).map_err(|e| e.to_string())?;
    repo.status().map_err(|e| e.to_string())
}

impl Render for StatusView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let children: Vec<AnyElement> = match &self.load {
            Load::Loading => vec![line("Loading…", theme::dim())],
            Load::Failed(message) => vec![line(format!("Error: {message}"), theme::deleted())],
            Load::Loaded(status) => status_rows(status),
        };

        div()
            .id("status")
            .size_full()
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(px(13.0))
            .p_4()
            .flex()
            .flex_col()
            .gap_0p5()
            .overflow_y_scroll()
            .children(children)
    }
}

fn status_rows(status: &Status) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    let head = &status.head;

    let branch = head
        .branch
        .clone()
        .unwrap_or_else(|| "HEAD (detached)".to_string());
    rows.push(line(format!("Head:    {branch}"), theme::fg()));
    if let Some(upstream) = &head.upstream {
        rows.push(line(
            format!(
                "Push:    {upstream}  (+{} -{})",
                head.ahead, head.behind
            ),
            theme::dim(),
        ));
    }

    let section = |rows: &mut Vec<AnyElement>, title: &str, entries: Vec<&FileEntry>| {
        if entries.is_empty() {
            return;
        }
        rows.push(spacer());
        rows.push(
            div()
                .text_color(theme::section())
                .child(SharedString::from(format!("{title} ({})", entries.len())))
                .into_any_element(),
        );
        for entry in entries {
            rows.push(file_row(entry));
        }
    };

    section(&mut rows, "Untracked files", status.untracked().collect());
    section(&mut rows, "Unstaged changes", status.unstaged().collect());
    section(&mut rows, "Staged changes", status.staged().collect());

    if rows.len() <= 2 {
        rows.push(spacer());
        rows.push(line("Nothing to commit, working tree clean", theme::dim()));
    }
    rows
}

fn file_row(entry: &FileEntry) -> AnyElement {
    let code = status_code(entry);
    let label = match &entry.orig_path {
        Some(orig) => format!("{orig} → {}", entry.path),
        None => entry.path.clone(),
    };
    div()
        .flex()
        .gap_2()
        .pl_4()
        .child(
            div()
                .w(px(20.0))
                .text_color(code_color(entry))
                .child(SharedString::from(code)),
        )
        .child(SharedString::from(label))
        .into_any_element()
}

/// Two-character git-style status code, e.g. "M ", " M", "??", "A ".
fn status_code(entry: &FileEntry) -> String {
    if entry.kind == EntryKind::Untracked {
        return "??".to_string();
    }
    let glyph = |c: Change| match c {
        Change::Unmodified => ' ',
        Change::Modified => 'M',
        Change::TypeChanged => 'T',
        Change::Added => 'A',
        Change::Deleted => 'D',
        Change::Renamed => 'R',
        Change::Copied => 'C',
        Change::Unmerged => 'U',
    };
    format!("{}{}", glyph(entry.index), glyph(entry.worktree))
}

fn code_color(entry: &FileEntry) -> gpui::Rgba {
    if entry.kind == EntryKind::Untracked {
        return theme::dim();
    }
    let dominant = if entry.index != Change::Unmodified {
        entry.index
    } else {
        entry.worktree
    };
    match dominant {
        Change::Added | Change::Copied => theme::added(),
        Change::Deleted => theme::deleted(),
        _ => theme::modified(),
    }
}

fn line(text: impl Into<String>, color: gpui::Rgba) -> AnyElement {
    div()
        .text_color(color)
        .child(SharedString::from(text.into()))
        .into_any_element()
}

fn spacer() -> AnyElement {
    div().h(px(8.0)).into_any_element()
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Magritte")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(StatusView::new),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}

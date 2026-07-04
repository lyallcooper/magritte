//! The in-app commit message editor: opening it (create/amend/reword, or the
//! external-editor handoff), the 50/72 message assistance, the read-only diff
//! preview beneath the message, cancel confirmation, and submitting the commit.
//! `impl StatusView` like the other view slices (see controller.rs's note).

use gpui::prelude::*;
use gpui::{Context, UniformListScrollHandle, Window};
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::input::{InputEvent, InputState, Position};
use magritte_core::{Change, CommitMode, DiffSource, FileDiff, LineKind};

use std::rc::Rc;

use crate::*;

/// The in-app commit message editor, backed by gpui-component's multi-line
/// Input. We keep the commit context (mode + switches) alongside it.
pub(crate) struct CommitEditor {
    pub(crate) state: Entity<InputState>,
    pub(crate) mode: CommitMode,
    pub(crate) args: Vec<String>,
    pub(crate) after_submit: CommitAfterSubmit,
    /// The baseline message we'd discard back to: empty for a new commit, or
    /// HEAD's message for amend/reword. Canceling only prompts when the current
    /// text differs from this.
    pub(crate) initial: String,
    /// Whether a "discard message?" confirmation is showing (cancel was pressed
    /// with unsaved edits).
    pub(crate) confirming_cancel: bool,
    /// Briefly true after a key other than y/n/esc is pressed while confirming —
    /// flashes the prompt to draw attention to it. Cleared by a timer.
    pub(crate) flash: bool,
    /// The staged diff being committed, flattened for read-only display below
    /// the message (magit's commit buffer). Empty until loaded, and left empty
    /// for reword (which commits no tree change).
    pub(crate) diff: Vec<CommitDiffRow>,
    pub(crate) diff_scroll: UniformListScrollHandle,
    /// Kept alive so the PressEnter subscription stays active.
    pub(crate) _sub: Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitAfterSubmit {
    Commit,
    ContinueRebase {
        stopped_sha: String,
    },
    /// The editor is capturing an annotated tag's message; on submit, create
    /// the tag at `target` (the commit at point, or HEAD).
    CreateTag {
        name: String,
        target: String,
        force: bool,
    },
}

/// One flattened row of the commit editor's staged-diff preview.
pub(crate) enum CommitDiffRow {
    /// Extra commit metadata toggled in the commit detail view.
    Detail(String),
    /// A line from the commit's full message, shown above the diff in commit view.
    Message(String),
    /// The diffstat summary shown above the files (files changed, +ins, -del).
    Stats {
        files: usize,
        insertions: usize,
        deletions: usize,
    },
    /// A file header: its change kind (for the status-style word/color) and path.
    File { change: Change, path: String },
    /// A hunk header (`@@ … @@`).
    Hunk(String),
    /// A diff line: its kind plus syntax-highlighted (or fallback) content.
    Line { kind: LineKind, spans: Rc<[Span]> },
    /// A dim status note (e.g. when the staged diff couldn't be loaded).
    Note(String),
}

/// The status-style change kind for a diff'd file (magit's "modified"/"new
/// file"/"deleted"/"renamed" word), derived from the file diff's flags.
pub(crate) fn file_change(diff: &FileDiff) -> Change {
    if diff.is_new {
        Change::Added
    } else if diff.is_deleted {
        Change::Deleted
    } else if diff.old_path != diff.new_path {
        Change::Renamed
    } else {
        Change::Modified
    }
}

impl StatusView {
    /// Begin a new commit (`c c`). Mirrors magit's `magit-commit-assert`: a
    /// commit only takes the *staged* changes, so with nothing staged we either
    /// refuse (nothing to commit at all) or offer to commit everything (`--all`,
    /// like `git commit -a`). An explicit `--all`/`--allow-empty` switch means
    /// the user already decided, so we skip straight to the editor.
    pub(crate) fn start_commit(
        &mut self,
        switches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_staged = self
            .status
            .as_ref()
            .is_some_and(|s| s.staged().next().is_some());
        let preempted = switches
            .iter()
            .any(|s| s == "--all" || s == "--allow-empty");
        if has_staged || preempted {
            self.open_editor(CommitMode::Create, switches, window, cx);
            return;
        }
        // Nothing staged. `--all` only stages *tracked* modifications (so does
        // `Status::unstaged`, which excludes untracked) — if there's nothing
        // there either, there is genuinely nothing to commit.
        let has_unstaged = self
            .status
            .as_ref()
            .is_some_and(|s| s.unstaged().next().is_some());
        if !has_unstaged {
            self.set_status("Nothing staged (or unstaged)".to_string(), false, cx);
            return;
        }
        self.confirm = Some((
            // `--all` stages tracked modifications/deletions only — untracked
            // files are never included, so don't promise "all changes".
            "Nothing staged. Commit all tracked changes?".to_string(),
            Confirm::CommitAll(switches),
        ));
        cx.notify();
    }

    /// React to an edit in the commit message: auto-wrap the body (if enabled)
    /// and refresh the over-50 summary warning (if enabled). Reads the toggles
    /// live from config so the settings screen takes effect without reopening.
    pub(crate) fn on_editor_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        let wrap = self.config.commit_body_wrap;
        let ruler = self.config.commit_title_ruler;
        state.update(cx, |s, cx| {
            if wrap {
                let value = s.value().to_string();
                let offset = s.cursor();
                if let Some(wrapped) =
                    commit_text::wrap_at_cursor(&value, offset, COMMIT_BODY_WIDTH)
                {
                    // Wrapping only turns a space into a newline, so the cursor's
                    // byte offset is unchanged — recompute its line/column in the
                    // rewrapped text and restore it.
                    s.set_value(wrapped.clone(), window, cx);
                    s.set_cursor_position(
                        commit_text::byte_offset_to_position(&wrapped, offset),
                        window,
                        cx,
                    );
                }
            }
            // Diagnostics carry their own copy of the text for position math;
            // reset it to the current value, then flag any summary overflow.
            let rope = s.text().clone();
            if let Some(diags) = s.diagnostics_mut() {
                diags.reset(&rope);
                if ruler {
                    if let Some((start, end)) =
                        commit_text::title_overflow(&rope.to_string(), COMMIT_TITLE_LIMIT)
                    {
                        diags.push(
                            Diagnostic::new(
                                Position::new(0, start)..Position::new(0, end),
                                "Summary longer than 50 characters",
                            )
                            .with_severity(DiagnosticSeverity::Warning),
                        );
                    }
                }
            }
        });
        cx.notify();
    }

    /// Reflow the commit body to 72 columns (the `alt-q` key / "reflow" button).
    /// Unlike auto-wrap, this rejoins manually-broken lines before re-wrapping,
    /// so it tidies a paragraph you've been editing.
    pub(crate) fn reflow_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.editor().map(|e| e.state.clone()) else {
            return;
        };
        state.update(cx, |s, cx| {
            let value = s.value().to_string();
            let reflowed = commit_text::reflow_body(&value, COMMIT_BODY_WIDTH);
            if reflowed != value {
                let end = reflowed.len(); // byte offset of the end
                s.set_value(reflowed.clone(), window, cx);
                s.set_cursor_position(
                    commit_text::byte_offset_to_position(&reflowed, end),
                    window,
                    cx,
                );
            }
        });
        // Refresh the summary warning against the reflowed text.
        self.on_editor_changed(window, cx);
    }

    /// The `GIT_EDITOR` command for writing commit messages in an external
    /// editor, or `None` (use the in-app editor) when none is configured. The
    /// configured command is used verbatim — the user supplies a blocking
    /// `--wait`-style flag as their editor requires.
    pub(crate) fn external_commit_editor(&self) -> Option<String> {
        if !self.config.commit_in_editor {
            return None;
        }
        let cmd = self.config.commit_editor.trim();
        (!cmd.is_empty()).then(|| cmd.to_string())
    }

    /// Make a commit by launching the external editor on its message (an
    /// interactive `git commit` on the background executor). The editor blocks
    /// git until it's closed; we show a waiting notice meanwhile, then report
    /// the outcome and refresh — an empty/aborted message surfaces as an error.
    pub(crate) fn commit_via_external_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        git_editor: String,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let (waiting, done) = match mode {
            CommitMode::Create => ("Waiting for commit message…", "Committed"),
            CommitMode::Amend => ("Waiting for amended message…", "Amended"),
            CommitMode::Reword => ("Waiting for reworded message…", "Reworded"),
        };
        self.set_status(waiting.to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.commit_with_editor(mode, &args, &git_editor) })
                .await;
            this.update(cx, |this, cx| {
                this.report(done, result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_editor(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_editor_after(mode, args, CommitAfterSubmit::Commit, window, cx);
    }

    /// Begin creating an annotated tag: capture its message in the in-app editor
    /// (or hand off to the external editor, like commit messages, when one is
    /// configured), then create the tag at `target`.
    pub(crate) fn start_annotated_tag(
        &mut self,
        name: String,
        target: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(git_editor) = self.external_commit_editor() {
            self.create_tag_via_external_editor(name, target, force, git_editor, cx);
            return;
        }
        self.open_editor_after(
            CommitMode::Create,
            Vec::new(),
            CommitAfterSubmit::CreateTag {
                name,
                target,
                force,
            },
            window,
            cx,
        );
    }

    /// Create an annotated tag by launching the external editor on its message
    /// (mirrors `commit_via_external_editor`): git blocks on the editor, we show
    /// a waiting notice, then report and refresh.
    pub(crate) fn create_tag_via_external_editor(
        &mut self,
        name: String,
        target: String,
        force: bool,
        git_editor: String,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.set_status("Waiting for tag message…".to_string(), false, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    repo.create_annotated_tag_with_editor(&name, &target, force, &git_editor)
                })
                .await;
            this.update(cx, |this, cx| {
                this.report("Tagged", result, cx);
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_editor_after(
        &mut self,
        mode: CommitMode,
        args: Vec<String>,
        after_submit: CommitAfterSubmit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If the user opted to write ordinary commit messages in their external
        // editor, hand off to an interactive `git commit` instead of the in-app
        // editor. Mid-rebase rewords choose their editor before reaching this
        // helper, because they also have to continue the rebase afterward.
        if after_submit == CommitAfterSubmit::Commit {
            if let Some(git_editor) = self.external_commit_editor() {
                self.commit_via_external_editor(mode, args, git_editor, cx);
                return;
            }
        }
        // Return inserts a newline; Cmd/Ctrl+Return submits (reported as a
        // PressEnter with secondary=true). We use code-editor mode (with the
        // grammar-less "text" language, so no syntax coloring) purely to get its
        // diagnostics layer, which we use to flag the over-50 summary; gutter,
        // line numbers, and folding are turned off so it reads as a plain box.
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("text")
                .submit_on_enter(false)
                .line_number(false)
                .folding(false)
        });
        let sub = cx.subscribe_in(
            &state,
            window,
            |this, _state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.submit_editor(window, cx),
                // Re-wrap the body and refresh the summary-length warning as the
                // message is edited.
                InputEvent::Change => this.on_editor_changed(window, cx),
                _ => {}
            },
        );
        // Focus on the next frame, not now: the keystroke that opened the editor
        // (`c`) is still mid-dispatch, and focusing synchronously would let that
        // character land in the message (see open_picker for the same reasoning).
        let to_focus = state.clone();
        cx.on_next_frame(window, move |_this, window, cx| {
            to_focus.read(cx).focus_handle(cx).focus(window, cx);
        });
        self.screen = Screen::Editor(CommitEditor {
            state: state.clone(),
            mode,
            args,
            after_submit,
            initial: String::new(),
            confirming_cancel: false,
            flash: false,
            diff: Vec::new(),
            diff_scroll: UniformListScrollHandle::new(),
            _sub: sub,
        });
        // Stamp this editor instance so async loads started for it can't write
        // into a different editor opened after it was cancelled.
        let gen = self.next_screen_gen();
        // Amend/reword pre-fill HEAD's message — loaded off the UI thread (the
        // git call must not block the UI), then set into the input if the user
        // hasn't started typing.
        if matches!(mode, CommitMode::Amend | CommitMode::Reword) {
            if let Some(repo) = self.repo.clone() {
                cx.spawn_in(window, async move |this, cx| {
                    let msg = cx
                        .background_executor()
                        .spawn(async move { repo.head_message().unwrap_or_default() })
                        .await;
                    let _ = cx.update(|window, app| {
                        state.update(app, |s, cx| {
                            if s.value().is_empty() {
                                s.set_value(msg.clone(), window, cx);
                            }
                        });
                    });
                    // set_value doesn't emit Change, so update the summary
                    // warning for the pre-filled message ourselves. Also record
                    // HEAD's message as the baseline, so canceling an unedited
                    // amend/reword doesn't prompt to discard.
                    let _ = this.update_in(cx, |this, window, cx| {
                        if !this.screen_gen.is_current(gen) {
                            return; // this editor was closed; don't touch a newer one
                        }
                        if let Some(ed) = this.editor_mut() {
                            ed.initial = msg;
                        }
                        this.on_editor_changed(window, cx);
                    });
                })
                .detach();
            }
        }
        // Preview the relevant diff: the staged change for create/amend, or the
        // reworded commit's own changes for reword. A tag message has no diff to
        // show, so the message editor fills the window instead.
        if !matches!(
            self.editor().map(|e| &e.after_submit),
            Some(CommitAfterSubmit::CreateTag { .. })
        ) {
            self.load_commit_diff(cx);
        }
        cx.notify();
    }

    /// Load the diff to preview in the open editor, in the background, and
    /// flatten it (with syntax highlighting) for read-only display. Create/amend
    /// show the staged diff being committed (or, with `--all`, every tracked
    /// change vs HEAD that the commit will include); reword shows the diff of the
    /// commit it's renaming (HEAD's own changes), since it makes no tree change.
    pub(crate) fn load_commit_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(ed) = self.editor() else {
            return;
        };
        // The caller (open_editor_after) bumped screen_gen for this editor;
        // capture it so a load outlived by its editor can't populate a newer
        // one (whose mode/args — create vs reword vs --all — may differ).
        let gen = self.screen_gen.current();
        let reword = ed.mode == CommitMode::Reword;
        let also_unstaged = ed.args.iter().any(|a| a == "--all");
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    let loaded = if reword {
                        repo.diff_commit("HEAD")
                    } else if also_unstaged {
                        // `--all` records every tracked change vs HEAD, so
                        // preview that — not just the staged side, which would
                        // hide tracked unstaged work the commit will include.
                        repo.diff_tracked_vs_head()
                    } else {
                        repo.diff_all(DiffSource::Staged)
                    };
                    match loaded {
                        Ok(diffs) => {
                            let mapped = diffs
                                .into_iter()
                                .map(|d| {
                                    let (head, tail) =
                                        file_head_tail(&repo.workdir().join(d.display_path()));
                                    let lang =
                                        highlight::detect_language(d.display_path(), &head, &tail);
                                    (d, lang)
                                })
                                .collect::<Vec<_>>();
                            (mapped, None)
                        }
                        Err(e) => (Vec::new(), Some(e.to_string())),
                    }
                })
                .await;
            let (files, error) = files;
            this.update(cx, |this, cx| {
                if !this.screen_gen.is_current(gen) || this.editor().is_none() {
                    return; // this editor closed (or was replaced) before the diff loaded
                }
                if let Some(err) = error {
                    if let Some(ed) = this.editor_mut() {
                        ed.diff = vec![CommitDiffRow::Note(format!("diff unavailable: {err}"))];
                    }
                    cx.notify();
                    return;
                }
                let rows = this.diff_rows(&files, cx);
                if let Some(ed) = this.editor_mut() {
                    ed.diff = rows;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Flatten loaded file diffs (each paired with its detected language) into
    /// displayable rows with syntax highlighting. Shared by the commit editor's
    /// preview and the log's commit-detail view.
    pub(crate) fn diff_rows(
        &self,
        files: &[(FileDiff, Option<&'static str>)],
        cx: &mut Context<Self>,
    ) -> Vec<CommitDiffRow> {
        let default = cx.theme().foreground;
        let (fg, dim) = (self.palette.fg, self.palette.dim);
        let mut rows = Vec::new();
        for (diff, lang) in files {
            rows.push(CommitDiffRow::File {
                change: file_change(diff),
                path: diff.display_path().to_string(),
            });
            let hl = match lang {
                Some(l) if !diff.is_binary => Some(highlight::highlight_diff(diff, l, cx, default)),
                _ => None,
            };
            for (hi, hunk) in diff.hunks.iter().enumerate() {
                rows.push(CommitDiffRow::Hunk(status_label::hunk_header_text(hunk)));
                for (li, line) in hunk.lines.iter().enumerate() {
                    let spans = hl
                        .as_ref()
                        .and_then(|h| h.get(&(hi, li)))
                        .cloned()
                        .unwrap_or_else(|| {
                            let color = if line.kind == LineKind::NoNewline {
                                dim
                            } else {
                                fg
                            };
                            Rc::from(vec![(line.content.clone(), color)])
                        });
                    rows.push(CommitDiffRow::Line {
                        kind: line.kind,
                        spans,
                    });
                }
            }
        }
        rows
    }

    /// Capture-phase handler: Escape cancels the editor. (Enter is consumed by
    /// the Input as a bound action and never reaches here — commit is driven by
    /// the PressEnter subscription instead.)
    pub(crate) fn on_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The vertico picker's query input is focused, so steal navigation /
        // confirm / cancel keys before the input consumes them; everything else
        // (text, backspace) falls through to filter the list.
        if matches!(self.popup, Some(Popup::Picker(_))) {
            let ctrl = event.keystroke.modifiers.control;
            // Emacs minibuffer aliases: C-g cancels, C-n/C-p move the selection.
            let key = match event.keystroke.key.as_str() {
                "g" if ctrl => "escape",
                "n" if ctrl => "down",
                "p" if ctrl => "up",
                k => k,
            };
            match key {
                "up" => {
                    cx.stop_propagation();
                    self.picker_move(-1, cx);
                }
                "down" => {
                    cx.stop_propagation();
                    self.picker_move(1, cx);
                }
                "enter" => {
                    cx.stop_propagation();
                    self.confirm_picker(window, cx);
                }
                "escape" => {
                    cx.stop_propagation();
                    self.cancel_popup(window, cx);
                }
                _ => {}
            }
            return;
        }

        if self.editor().is_none() {
            return;
        }
        // C-g cancels here too; C-n/C-p are left to the Input for cursor motion.
        let key = match event.keystroke.key.as_str() {
            "g" if event.keystroke.modifiers.control => "escape",
            k => k,
        };
        // While the "discard message?" confirmation is up, it owns the keyboard:
        // swallow every key so none reaches the message input (otherwise typing
        // would edit the message behind the prompt). Only y / n / esc act.
        if self.editor().is_some_and(|e| e.confirming_cancel) {
            cx.stop_propagation();
            match key {
                "y" => self.discard_editor(window, cx),
                "n" | "escape" => self.keep_editing(window, cx),
                // Any other key is ignored — flash the prompt so it's clear
                // input is paused and only y/n/esc do anything.
                _ => self.flash_discard_prompt(cx),
            }
            return;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.cancel_editor(window, cx);
        } else if key == "q" && event.keystroke.modifiers.alt {
            // alt-q reflows the body (Emacs fill-paragraph heritage); capture it
            // so the Input doesn't insert the character.
            cx.stop_propagation();
            self.reflow_editor(window, cx);
        }
    }

    /// Cancel the editor — but if there are unsaved edits, ask first rather than
    /// silently dropping the message.
    pub(crate) fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = match self.editor() {
            Some(ed) => ed.state.read(cx).value().trim() != ed.initial.trim(),
            None => return,
        };
        if dirty {
            if let Some(ed) = self.editor_mut() {
                ed.confirming_cancel = true;
                ed.flash = false; // start un-flashed
            }
            cx.notify();
        } else {
            self.discard_editor(window, cx);
        }
    }

    /// Flash the discard confirmation to draw attention to it — invoked when a
    /// key other than y/n/esc is pressed while it's up. A generation-scoped
    /// timer clears the flash, so rapid keypresses keep it lit without an
    /// earlier timer cutting a later flash short.
    pub(crate) fn flash_discard_prompt(&mut self, cx: &mut Context<Self>) {
        if !self.editor().is_some_and(|e| e.confirming_cancel) {
            return;
        }
        if let Some(ed) = self.editor_mut() {
            ed.flash = true;
        }
        let gen = self.confirm_flash_gen.bump();
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(CONFIRM_FLASH_MS))
                .await;
            this.update(cx, |this, cx| {
                if this.confirm_flash_gen.is_current(gen) {
                    if let Some(ed) = this.editor_mut() {
                        ed.flash = false;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Close the editor, discarding its message.
    pub(crate) fn discard_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Status;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the discard confirmation and keep editing.
    pub(crate) fn keep_editing(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.editor_mut() {
            ed.confirming_cancel = false;
        }
        cx.notify();
    }

    pub(crate) fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editor() else {
            return;
        };
        let text = ed.state.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.set_status("Message is empty".to_string(), false, cx);
            return;
        }
        let ed = self.take_editor().unwrap();
        self.focus.focus(window, cx);
        // Drop the trailing newline the submit keystroke inserted.
        let message = text.trim_end().to_string();
        match ed.after_submit {
            CommitAfterSubmit::Commit => self.run_commit(message, ed.mode, ed.args, cx),
            CommitAfterSubmit::ContinueRebase { stopped_sha } => {
                self.run_rebase_reword_commit(message, stopped_sha, window, cx)
            }
            CommitAfterSubmit::CreateTag {
                name,
                target,
                force,
            } => self.run_job(
                "Tagging…",
                "Tagged",
                move |repo| repo.create_annotated_tag(&name, &target, force, &message),
                cx,
            ),
        }
    }

    pub(crate) fn run_commit(
        &mut self,
        message: String,
        mode: CommitMode,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.run_job(
            "Committing…",
            "Committed",
            move |repo| repo.commit(&message, mode, &args),
            cx,
        );
    }
}

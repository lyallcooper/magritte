//! Keyboard input handling for [`StatusView`]: the `on_key` entry point, the
//! prefix-sequence state machine, command dispatch (`run_dispatch` /
//! `invoke_command`), custom-command/shell execution, and the `:` palette.
//! Split out of `main.rs` as a `pub(crate)` `impl StatusView` block.

use std::collections::HashMap;

use gpui::{Context, KeyDownEvent, SharedString, Window};
use magritte_core::{
    transient::{self, Suffix, Transient},
    SequenceKind,
};

use crate::*;

impl StatusView {
    pub(crate) fn on_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While the editor is open the focused Input handles keys; commit/cancel
        // are caught in the capture phase (on_capture_key).
        if self.editor().is_some() {
            return;
        }

        let key = event.keystroke.key.to_lowercase();
        let shift = event.keystroke.modifiers.shift;
        let mut ctrl = event.keystroke.modifiers.control;
        let alt = event.keystroke.modifiers.alt;
        let cmd = event.keystroke.modifiers.platform;

        // C-g is the universal cancel (= Escape) everywhere — Emacs
        // keyboard-quit. Other Emacs motions (`C-n`/`C-p`, `C-x C-c`, …) are now
        // ordinary keymap entries (see the preset binding tables), not normalized here.
        let key = match key.as_str() {
            "g" if ctrl => {
                ctrl = false;
                "escape".to_string()
            }
            _ => key,
        };

        // A sequence is pending: this key continues it. Resolve here — before the
        // per-view branches — so sequences (including `C-x C-c`) work everywhere.
        if self.pending_prefix.is_some() {
            let next = chord(&key, shift, ctrl, alt, cmd);
            self.advance_prefix(&next, window, cx);
            return;
        }

        // While settings is open the focused Select handles keys; we only watch
        // for Esc (when no dropdown menu is open) to close the screen. Tab is
        // delivered via the ToggleFold action.
        if self.settings().is_some() {
            if key == "escape" {
                self.close_settings(window, cx);
            }
            return;
        }

        // Popup keys are case-sensitive (e.g. F pull vs f fetch), so
        // reconstruct the cased key from the shift modifier.
        let cased = chord(&key, shift, false, false, false);

        // A command transient is modal — it captures every key. Pass the full
        // chord (with modifiers) so meta-keys like `C-s` (save switches) work;
        // a plain key's chord is just its cased form, so suffixes are unaffected.
        if matches!(self.popup, Some(Popup::Transient(_))) {
            self.handle_transient_key(&chord(&key, shift, ctrl, alt, cmd), window, cx);
            return;
        }

        // The vertico picker's focused input handles text; navigation, confirm
        // and cancel are caught in the capture phase (on_capture_key). Ignore the
        // rest here so typed characters aren't read as commands.
        if matches!(self.popup, Some(Popup::Picker(_))) {
            return;
        }

        // The `?` dispatch popup is modal (like magit's dispatch): a shown key
        // runs that command, esc/? close it, other keys are ignored. `q` closes
        // help unless the context menu explicitly shows it as a view-local action.
        if let Some(Popup::Dispatch(def)) = &self.popup {
            // (A pending prefix's second key was already resolved above.)
            match cased.as_str() {
                "escape" | "?" | "/" => {
                    self.popup = None;
                    cx.notify();
                }
                "q" if dispatch_has_key(def, "q") => self.run_info_key("q", window, cx),
                "q" => {
                    self.popup = None;
                    cx.notify();
                }
                k if self.is_prefix(k) => self.enter_prefix(k.to_string(), window, cx),
                k if dispatch_has_key(def, k) => self.run_info_key(k, window, cx),
                _ => {}
            }
            return;
        }

        // A pending discard confirmation captures the next key.
        if self.confirm.is_some() {
            if key == "y" {
                self.confirm_yes(window, cx);
            } else {
                self.confirm_no(window, cx);
            }
            return;
        }

        if let Some(kind) = self.sequence_kind() {
            let sequence_prefix = match kind {
                SequenceKind::Rebase => "r",
                SequenceKind::Merge => "m",
                SequenceKind::CherryPick => "A",
                SequenceKind::Revert => match self.config.keymap_preset {
                    config::KeymapPreset::EvilCollection => "_",
                    config::KeymapPreset::Vanilla => "V",
                },
                SequenceKind::Am => "w",
            };
            if cased == sequence_prefix {
                self.open_transient(
                    "",
                    transient::sequence_transient(kind, self.keymap_style()),
                    RemoteTargets::default(),
                    cx,
                );
                return;
            }
        }

        // Command palette via cmd+p / cmd+k — before per-view handlers, so it
        // remains reachable from detail/log screens.
        if cmd && matches!(key.as_str(), "p" | "k") {
            return self.open_command_palette(window, cx);
        }
        if key == "?" || (key == "/" && shift) {
            self.popup = Some(Popup::Dispatch(dispatch_menu_for(self)));
            cx.notify();
            return;
        }

        // The git command-log view takes over the window; esc/q/$ close it, and
        // it scrolls with the usual vi/less keys.
        if self.git_log().is_some() {
            if key == "escape" || key == "q" || key == "$" || (key == "4" && shift) {
                self.close_git_log(window, cx);
                return;
            }
            // `a` toggles showing the UI's own read-only queries.
            if key == "a" {
                self.toggle_git_log_all(window, cx);
                return;
            }
            let page = page_rows(window);
            let len = self.git_log_rows().len();
            // The pager has no cursor, so it scrolls via less-style keys rather
            // than the shared `nav_*`; translate a remapped motion to the key
            // apply_scroll_key understands, so [keymap] still drives it.
            let cased = chord(&key, shift, false, false, false);
            let (skey, sshift) = match self.keymap.get(&cased).map(String::as_str) {
                Some("move-down") => ("j", false),
                Some("move-up") => ("k", false),
                Some("goto-bottom") => ("g", true),
                Some("goto-top") => ("g", false),
                _ => (key.as_str(), shift),
            };
            if let Some(sv) = self.git_log_mut() {
                apply_scroll_key(&sv.scroll, &mut sv.top, len, skey, sshift, ctrl, page);
            }
            cx.notify();
            return;
        }

        // A commit's diff detail (opened from the log) is topmost; esc/q returns
        // to the log, and it scrolls with the usual vi/less keys.
        // The interactive-rebase todo editor: set an action, reorder, then start.
        if self.rebase_todo().is_some() {
            // While the "discard edits?" confirmation is up, capture y / n / esc.
            if self.rebase_todo().is_some_and(|rt| rt.confirming_cancel) {
                match key.as_str() {
                    "y" => self.discard_rebase_todo(window, cx),
                    "n" | "escape" => self.keep_editing_rebase_todo(window, cx),
                    _ => {}
                }
                return;
            }
            if self.try_nav(&key, shift, ctrl, alt, window, cx) {
                return;
            }
            match key.as_str() {
                "escape" | "q" => self.close_rebase_todo(window, cx),
                "enter" => self.run_rebase_todo(window, cx),
                // Move the selected commit up/down (shift+k / shift+j).
                "k" if shift => self.rebase_todo_reorder(-1, cx),
                "j" if shift => self.rebase_todo_reorder(1, cx),
                // Set the action of the commit at point.
                "p" => self.rebase_todo_set_action(RebaseAction::Pick, cx),
                "r" | "w" => self.rebase_todo_set_action(RebaseAction::Reword, cx),
                "e" => self.rebase_todo_set_action(RebaseAction::Edit, cx),
                "s" => self.rebase_todo_set_action(RebaseAction::Squash, cx),
                "f" => self.rebase_todo_set_action(RebaseAction::Fixup, cx),
                "d" | "x" => self.rebase_todo_set_action(RebaseAction::Drop, cx),
                _ => {}
            }
            return;
        }

        if self.commit_view().is_some() {
            if self.try_nav(&key, shift, ctrl, alt, window, cx) {
                return;
            }
            match key.as_str() {
                // Cancel a visual selection first; otherwise leave the view.
                "escape" | "q" => {
                    if self.flat_diff().is_some_and(|fd| fd.visual.is_some()) {
                        if let Some(fd) = self.flat_diff_mut() {
                            fd.visual = None;
                        }
                        cx.notify();
                    } else {
                        self.close_commit_view(window, cx);
                    }
                }
                "v" => self.flat_diff_toggle_visual(cx),
                "a" => self.toggle_commit_details(cx),
                // Copy: evil's `y`; magit-mode-map's `C-w` (the vanilla key).
                "y" if self.is_evil() => self.copy_flat_diff_selection(cx),
                "w" if ctrl => self.copy_flat_diff_selection(cx),
                "c" if cmd => self.copy_flat_diff_selection(cx),
                _ => {}
            }
            return;
        }

        if self.diff_view().is_some() {
            if self.try_nav(&key, shift, ctrl, alt, window, cx) {
                return;
            }
            match key.as_str() {
                "escape" | "q" => {
                    if self.flat_diff().is_some_and(|fd| fd.visual.is_some()) {
                        if let Some(fd) = self.flat_diff_mut() {
                            fd.visual = None;
                        }
                        cx.notify();
                    } else {
                        self.close_diff_view(window, cx);
                    }
                }
                "v" => self.flat_diff_toggle_visual(cx),
                "y" if self.is_evil() => self.copy_flat_diff_selection(cx),
                "w" if ctrl => self.copy_flat_diff_selection(cx),
                "c" if cmd => self.copy_flat_diff_selection(cx),
                _ => {}
            }
            return;
        }

        // The commit-log view: Enter opens the commit; esc/q close; motions move
        // the selection (shared with every cursor view via `try_nav`).
        if self.log().is_some() {
            // In a select mode, Return confirms the commit for the pending
            // action; while browsing it opens the commit's diff.
            let select_rebase = match self.log().map(|l| &l.purpose) {
                Some(LogPurpose::SelectRebaseBase { args }) => Some((args.clone(), false)),
                Some(LogPurpose::SelectRebaseReword { args }) => Some((args.clone(), true)),
                _ => None,
            };
            if self.try_nav(&key, shift, ctrl, alt, window, cx) {
                return;
            }
            let chorded = chord(&key, shift, ctrl, alt, cmd);
            match chorded.as_str() {
                "escape" | "q" => return self.close_log(window, cx),
                // Cmd+Return confirms the pending select (rebase since); plain
                // Return opens the commit's diff — in select mode too, so you can
                // inspect commits before choosing (magit lets you visit from the
                // log-select).
                "cmd-enter" => {
                    if let Some((args, reword)) = select_rebase {
                        if reword {
                            self.reword_past_selected(args, window, cx);
                        } else {
                            self.rebase_since_selected(args, cx);
                        }
                    }
                    return;
                }
                // `r`: rebase interactively since the commit at point (magit's
                // commit-at-point path) — only while browsing, with default args.
                "r" if select_rebase.is_none() => {
                    return self.rebase_since_selected(Vec::new(), cx)
                }
                // Vanilla copy: `C-w` isn't in the log's keymap dispatch, so
                // handle it here alongside the shared verbs.
                "ctrl-w" => return self.copy_log_commit(cx),
                _ => {}
            }
            self.log_commit_key(&chorded, window, cx);
            return;
        }

        // Command palette via cmd+p / cmd+k handled above, before per-view branches.
        // Motions, paging, and the `g` prefix — remappable, applied screen-aware.
        if self.try_nav(&key, shift, ctrl, alt, window, cx) {
            return;
        }
        // Act on the commit/stash at point in a status section (after motions,
        // so j/k/g still work; only the shared point verbs are intercepted —
        // anything else falls through to the keymap).
        let chorded = chord(&key, shift, ctrl, alt, cmd);
        if self.point_commit_key(&chorded, window, cx) {
            return;
        }
        if self.point_stash_key(&chorded, window, cx) {
            return;
        }
        match key.as_str() {
            // Tab toggles a fold (also delivered via the ToggleFold action, since
            // Root binds tab). Kept explicit — and out of the remappable keymap.
            "tab" => self.toggle_fold(cx),
            "escape" => {
                // A running job takes priority: C-g/Esc kills its subprocess.
                // Otherwise cancel a visual selection, else dismiss the
                // status/error banner if one is showing.
                if self.cancel_job(cx) {
                    return;
                }
                if self.selection.visual.take().is_some() || self.toast.message.take().is_some() {
                    cx.notify();
                }
                return;
            }
            // Modifier/popup shortcuts that aren't ordinary commands: Cmd-C yanks
            // before any `c` binding; M-x and an unbound `:`/`;`+shift open the
            // palette; `?`/`/`+shift open Help. Bound symbol keys (`!`, `|`, `$`,
            // vanilla `:`) fall through to the effective keymap below.
            "c" if cmd => return self.invoke_command("yank", window, cx),
            "x" if alt => return self.open_command_palette(window, cx),
            ":" | ";" if key == ":" || shift => {
                if Self::is_dispatch_key(&self.keymap, &cased) {
                    return self.run_dispatch(&cased, window, cx);
                }
                return self.open_command_palette(window, cx);
            }
            // Everything else resolves through the effective keymap (the
            // shift-cased keystroke → command id), so remap/unbind take effect.
            // The plain command keys (`c`, `s`/`S`, `O`, `F`, `enter`, `v`, …)
            // live there now, not as arms above — the single source of dispatch.
            _ => {
                // Resolve on the full chord, so a modifier binding (e.g.
                // `[keymap] "ctrl-d" = "commit"`) dispatches; for a plain/shifted
                // key the chord is just its cased form, so nothing else changes.
                let chord = chord(&key, shift, ctrl, alt, cmd);
                if Self::is_dispatch_key(&self.keymap, &chord) {
                    return self.run_dispatch(&chord, window, cx);
                }
                // An unbound key: tell the user (emacs' "… is undefined"). Only
                // for plain/shifted keys — an unbound key held with cmd/alt/ctrl
                // is usually an OS or editor shortcut we don't model, so a "z is
                // unbound" toast for cmd-z would be misleading.
                if !cmd && !alt && !ctrl {
                    self.report_unbound(&cased, cx);
                }
                return;
            }
        }
        self.scroll
            .scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// Mouse click on a transient suffix: toggle a switch, or invoke an action.
    pub(crate) fn click_suffix(
        &mut self,
        key: SharedString,
        is_switch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_switch {
            if let Some(Popup::Transient(state)) = self.popup.as_mut() {
                let k = key.to_string();
                if !state.active.remove(&k) {
                    state.active.insert(k);
                }
                cx.notify();
            }
        } else {
            self.handle_transient_key(&key, window, cx);
        }
    }

    /// Click on a value-reading option row: prompt for its value, stashing the
    /// transient to reopen after (mirrors pressing the option's `-X` key).
    pub(crate) fn click_option(
        &mut self,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opt = match &self.popup {
            Some(Popup::Transient(s)) => s
                .def
                .option_for(&key)
                .map(|o| (o.key.to_string(), o.description.to_string(), o.completion)),
            _ => None,
        };
        if let Some((k, desc, comp)) = opt {
            if let Some(Popup::Transient(ts)) = self.popup.take() {
                self.open_option_prompt(k, desc, comp, ts, window, cx);
            }
        }
    }

    /// Invoke a `?`-dispatch command (by key press or row click): close the
    /// dispatch menu and run the command, like magit's dispatch transient.
    pub(crate) fn run_dispatch(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        // A keymap-bound command (default or user-remapped), the `:` palette, or
        // a motion. Resolving through the effective keymap is what makes
        // remap/unbind take effect — and binding *any* command id (even a leaf
        // like `branch.delete`) to a key Just Works via `invoke_command`.
        if let Some(id) = self.keymap.get(key).cloned() {
            // Motions resolve here too (registry Navigation commands), applied
            // screen-aware by their `run`.
            self.invoke_command(&id, window, cx);
        } else if key == ":" {
            self.open_command_palette(window, cx);
        }
    }

    /// Invoke a registry [`Command`] by id — the keymap's bridge to the
    /// registry, so the command's behavior lives in exactly one place.
    pub(crate) fn invoke_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cmd) = commands().iter().find(|c| c.id == id) {
            (cmd.run)(self, window, cx);
        } else if let Some(custom) = self.config.commands.iter().find(|c| c.id == id).cloned() {
            self.run_custom_command(custom, window, cx);
        }
    }

    /// Run a user `[[command]]`: substitute its placeholders against the current
    /// selection, confirm if it looks destructive, then run it as a shell command
    /// on the background path.
    pub(crate) fn run_custom_command(
        &mut self,
        cmd: config::CustomCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = match self.expand_placeholders(&cmd.run) {
            Ok(c) => c,
            Err(e) => return self.set_status(e, false, cx),
        };
        if command.trim().is_empty() {
            return;
        }
        if command_is_destructive(&command) {
            self.confirm = Some((
                format!("Run `{command}`?"),
                Confirm::CustomShell {
                    command,
                    refresh: cmd.refresh,
                },
            ));
            cx.notify();
        } else {
            self.run_custom_shell(command, cmd.refresh, cx);
        }
    }

    /// Substitute `{file}`/`{commit}`/`{branch}` in the command against the
    /// current selection, each shell-quoted so a path with spaces stays one word.
    /// `Err` (with why) if a placeholder can't be resolved — e.g. `{file}` with
    /// no file at point.
    pub(crate) fn expand_placeholders(&self, command: &str) -> Result<String, String> {
        let mut s = command.to_string();
        if s.contains("{file}") {
            let path = self
                .path_at_point()
                .ok_or_else(|| "No file at point for {file}".to_string())?;
            s = s.replace("{file}", &shell_words::quote(&path));
        }
        if s.contains("{branch}") {
            let branch = self
                .status
                .as_ref()
                .and_then(|st| st.head.branch.clone())
                .ok_or_else(|| "No current branch for {branch}".to_string())?;
            s = s.replace("{branch}", &shell_words::quote(&branch));
        }
        if s.contains("{commit}") {
            // The commit at point: the log selection, else a status commit row
            // (unpushed/unpulled/recent), else the open commit view.
            let hash = self
                .log()
                .and_then(|l| l.entries.get(l.selected))
                .map(|e| e.hash.clone())
                .or_else(|| self.point_commit().map(|(hash, _, _)| hash))
                .or_else(|| self.commit_view().map(|cv| cv.rev.clone()))
                .ok_or_else(|| "No commit at point for {commit}".to_string())?;
            s = s.replace("{commit}", &shell_words::quote(&hash));
        }
        Ok(s)
    }

    /// The repo-relative path of the file at point (its row, or the file a
    /// hunk/line belongs to), if any.
    pub(crate) fn path_at_point(&self) -> Option<String> {
        match self.rows.get(self.selected)?.target.as_ref()? {
            Target::File(f) => Some(f.path.clone()),
            Target::Hunk { file, .. } | Target::Line { file, .. } => Some(file.path.clone()),
        }
    }

    /// Run a resolved custom command (`sh -c`), surfacing its full output as a
    /// toast and refreshing unless opted out — like the `!` prompt.
    pub(crate) fn run_custom_shell(
        &mut self,
        command: String,
        refresh: bool,
        cx: &mut Context<Self>,
    ) {
        self.run_command_job(
            format!("{command}…"),
            refresh,
            move |repo| repo.run_shell(&command),
            cx,
        );
    }

    /// Classify a keystroke sequence against the effective keymap: a complete
    /// binding, a prefix of one or more longer bindings, or neither.
    pub(crate) fn classify_seq(&self, seq: &str) -> KeyMatch {
        if let Some(id) = self.keymap.get(seq) {
            return KeyMatch::Command(id.clone());
        }
        let lead = format!("{seq} ");
        if self.keymap.keys().any(|k| k.starts_with(&lead)) {
            return KeyMatch::Prefix;
        }
        KeyMatch::Unbound
    }

    /// Whether `key` begins a longer binding — a prefix the next keystroke
    /// continues (it may also be a complete binding on its own; this only asks
    /// whether *more* could follow).
    pub(crate) fn is_prefix(&self, key: &str) -> bool {
        matches!(self.classify_seq(key), KeyMatch::Prefix)
    }

    /// Begin (or extend) a sequence: remember the keys typed so far and show the
    /// lightweight bottom strip. The sequence then waits indefinitely for the
    /// next key; after `which_key_delay_ms` the strip expands into the which-key
    /// list of continuations.
    pub(crate) fn enter_prefix(
        &mut self,
        seq: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gen = self.prefix_gen.bump();
        self.pending_prefix = Some(PendingPrefix {
            seq,
            gen,
            which_key: false,
        });
        cx.notify();
        let delay = Duration::from_millis(self.config.which_key_delay_ms);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |this, _window, cx| {
                // Reveal the which-key list only if this exact sequence is still
                // waiting (a newer prefix or a resolved sequence bumps/clears it).
                let Some(p) = this.pending_prefix.as_mut() else {
                    return;
                };
                if p.gen != gen || p.which_key {
                    return;
                }
                p.which_key = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Feed the next key into the pending sequence. Appends it and re-classifies:
    /// a complete binding runs (closing any dispatch popup), a deeper prefix
    /// keeps waiting, and an unbound sequence reports "… is unbound".
    pub(crate) fn advance_prefix(
        &mut self,
        next: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.pending_prefix.take() else {
            return;
        };
        // Esc / C-g (normalized to "escape") aborts the sequence silently — it's
        // keyboard-quit, not an attempt at a binding, so no "unbound" notice.
        if next == "escape" {
            cx.notify();
            return;
        }
        let seq = format!("{} {next}", p.seq);
        match self.classify_seq(&seq) {
            KeyMatch::Command(id) => {
                self.popup = None;
                self.invoke_command(&id, window, cx);
            }
            KeyMatch::Prefix => self.enter_prefix(seq, window, cx),
            KeyMatch::Unbound => self.report_unbound(&seq, cx),
        }
        cx.notify();
    }

    /// Note that a keystroke sequence isn't bound (magit/emacs' "… is undefined"
    /// echo-area feedback), as a fading notice with the keys shown as keycaps.
    pub(crate) fn report_unbound(&mut self, seq: &str, cx: &mut Context<Self>) {
        self.set_status("is unbound".to_string(), true, cx);
        self.toast.keys = Some(seq.to_string());
    }

    /// Note a command run *from the palette* for its frecency ranking, and
    /// persist it. Only palette runs count: a command you already invoke by key
    /// doesn't need surfacing at the top of the palette.
    pub(crate) fn record_use(&mut self, id: &str) {
        self.usage.record(id);
        config::save_usage(&self.usage);
    }

    /// Open the `:` command palette: the vertico picker over the (enabled)
    /// registry commands, matched by title. Enter runs the chosen command.
    pub(crate) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Order by frecency (most-used-recently first); a stable sort keeps the
        // registry order among never-used commands and ties. The picker's fuzzy
        // ranking takes over once the user types, with this order breaking ties.
        let mut entries: Vec<(String, String)> = all_commands(&self.config)
            .filter(|c| c.palette && (c.enabled)(self))
            .map(|c| (c.id.to_string(), c.title.to_string()))
            .collect();
        entries.sort_by(|a, b| {
            let (sa, sb) = (self.usage.score(&a.0), self.usage.score(&b.0));
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let choices: Vec<String> = entries.into_iter().map(|(_, title)| title).collect();
        self.open_picker(
            PickerAction::RunCommand,
            choices,
            CreateMode::None,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Whether `key` is a single-stroke dispatch key: bound in the effective
    /// keymap (a command), or one of the bare motions `j`/`k`/`G` and the `:`
    /// palette. Multi-stroke entries are handled elsewhere — Tab via the
    /// ToggleFold action, `g r`/`g g`/`g j`/`g k` via the g-prefix — so they're
    /// excluded even if a key like `g r` is bound.
    pub(crate) fn is_dispatch_key(keymap: &HashMap<String, String>, key: &str) -> bool {
        // Only single-keystroke chords reach here (multi-key sequences resolve
        // through the prefix machinery); motions are registry commands too.
        keymap.contains_key(key) || key == ":"
    }

    /// Act-at-point verbs for a status commit row. `key` is in encoded chord
    /// form (`A`, `cmd-c`) — [`Self::on_key`] encodes its modifiers first, and
    /// help-menu clicks arrive already encoded. Returns whether it handled the
    /// key. Commit rows mirror evil-collection-magit: `A`/`_` open the
    /// cherry-pick/revert transients with the commit at point as the default,
    /// while `a`/`-` apply/revert changes without committing (vanilla uses
    /// `V`/`v` for the revert pair).
    fn point_commit_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((hash, short, subject)) = self.point_commit() else {
            return false;
        };
        match key {
            "enter" => self.open_commit(hash, short, subject, cx),
            "A" => self.open_cherry_pick_transient(cx),
            "a" => self.pick_selected(PickOp::CherryApply, window, cx),
            "_" if self.is_evil() => self.open_revert_transient(cx),
            "-" if self.is_evil() => self.pick_selected(PickOp::RevertNoCommit, window, cx),
            "V" if self.is_vanilla() => self.open_revert_transient(cx),
            "v" if self.is_vanilla() => self.pick_selected(PickOp::RevertNoCommit, window, cx),
            "r" => self.invoke_command("rebase", window, cx),
            // Evil-only `y`: vanilla's copy is `C-w`, which resolves through
            // the keymap to yank (copying the full hash for a commit row).
            "y" if self.is_evil() => self.copy_to_clipboard(hash, cx),
            "cmd-c" => self.copy_to_clipboard(hash, cx),
            _ => return false,
        }
        true
    }

    /// Act-at-point verbs for a stash row (chord-form `key`, like
    /// [`Self::point_commit_key`]): `Enter` shows it, `a` applies, `A` pops,
    /// `x` drops (confirmed), `y`/Cmd-C yanks the reference.
    fn point_stash_key(&mut self, key: &str, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((reference, message)) = self.point_stash() else {
            return false;
        };
        match key {
            "enter" => self.open_commit(reference.clone(), reference, message, cx),
            "A" => self.run_stash_action(StashAction::Pop, reference, cx),
            "a" => self.run_stash_action(StashAction::Apply, reference, cx),
            // The delete key follows the preset: evil-collection's x, magit
            // vanilla's k.
            "x" if self.is_evil() => {
                self.confirm = Some((format!("Drop {reference}?"), Confirm::DropStash(reference)));
                cx.notify();
            }
            "k" if self.is_vanilla() => {
                self.confirm = Some((format!("Drop {reference}?"), Confirm::DropStash(reference)));
                cx.notify();
            }
            "y" if self.is_evil() => self.copy_to_clipboard(reference, cx),
            "cmd-c" => self.copy_to_clipboard(reference, cx),
            _ => return false,
        }
        true
    }

    /// Commit-at-point verbs for the log view (chord-form `key`): open the
    /// commit, cherry-pick it (Magit's `A`), revert it (evil `_` / vanilla
    /// `V`), or yank its hash. `r` (rebase since) stays with the callers — the
    /// key handler guards it to browse mode.
    fn log_commit_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match key {
            "enter" => self.open_commit_view(cx),
            "A" => self.pick_selected(PickOp::CherryPick, window, cx),
            "_" if self.is_evil() => self.pick_selected(PickOp::Revert, window, cx),
            "V" if self.is_vanilla() => self.pick_selected(PickOp::Revert, window, cx),
            "y" if self.is_evil() => self.copy_log_commit(cx),
            "cmd-c" => self.copy_log_commit(cx),
            _ => return false,
        }
        true
    }

    /// Close the open picker. If it was prompting for a transient option value,
    /// reopen that transient unchanged rather than dismissing everything.
    pub(crate) fn cancel_popup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Popup::Picker(p)) = self.popup.take() {
            if let Some(ts) = p.resume {
                self.popup = Some(Popup::Transient(*ts));
            }
        }
        cx.notify();
    }

    /// Mouse click on a status row: select it, and toggle its fold if foldable.
    pub(crate) fn click_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.popup.is_some() {
            self.popup = None;
            cx.notify();
            return;
        }
        // A shift-click already set up the extended selection in `on_mouse_down`;
        // don't also toggle the row's fold.
        if self.selection.shift_click {
            self.selection.shift_click = false;
            cx.notify();
            return;
        }
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let foldable = row.fold.is_some();
        if row.selectable {
            self.selected = ix;
        }
        if foldable {
            self.toggle_fold(cx);
        } else {
            cx.notify();
        }
    }

    pub(crate) fn run_info_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        if self.commit_view().is_some() {
            match key {
                "a" => self.toggle_commit_details(cx),
                "v" => self.flat_diff_toggle_visual(cx),
                "y" | "ctrl-w" => self.copy_flat_diff_selection(cx),
                "q" => self.close_commit_view(window, cx),
                _ => self.run_dispatch(key, window, cx),
            }
            return;
        }
        if self.diff_view().is_some() {
            match key {
                "v" => self.flat_diff_toggle_visual(cx),
                "y" | "ctrl-w" => self.copy_flat_diff_selection(cx),
                "q" => self.close_diff_view(window, cx),
                _ => self.run_dispatch(key, window, cx),
            }
            return;
        }
        if self.log().is_some() {
            match key {
                "r" => self.rebase_since_selected(Vec::new(), cx),
                "y" | "ctrl-w" => self.copy_log_commit(cx),
                "q" => self.close_log(window, cx),
                _ => {
                    if !self.log_commit_key(key, window, cx) {
                        self.run_dispatch(key, window, cx);
                    }
                }
            }
            return;
        }
        if self.git_log().is_some() {
            match key {
                "a" => self.toggle_git_log_all(window, cx),
                "q" => self.close_git_log(window, cx),
                _ => self.run_dispatch(key, window, cx),
            }
            return;
        }
        if self.rebase_todo().is_some() {
            match key {
                "q" => self.close_rebase_todo(window, cx),
                _ => self.run_dispatch(key, window, cx),
            }
            return;
        }
        // Menu clicks deliver `ctrl-w` directly (the key path resolves it
        // through the keymap to yank instead), so map it to the point copy
        // before the shared verbs.
        if key == "ctrl-w" {
            if let Some((hash, ..)) = self.point_commit() {
                return self.copy_to_clipboard(hash, cx);
            }
            if let Some((reference, _)) = self.point_stash() {
                return self.copy_to_clipboard(reference, cx);
            }
        }
        if self.point_commit_key(key, window, cx) {
            return;
        }
        if self.point_stash_key(key, window, cx) {
            return;
        }
        self.run_dispatch(key, window, cx);
    }
}

fn dispatch_has_key(def: &Transient, key: &str) -> bool {
    def.groups.iter().any(|group| {
        group
            .suffixes
            .iter()
            .any(|suffix| matches!(suffix, Suffix::Info(info) if info.keys == key))
    })
}

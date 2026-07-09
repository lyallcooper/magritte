//! Keyboard input handling for [`StatusView`]: the `on_key` entry point, the
//! prefix-sequence state machine, command dispatch (`run_dispatch` /
//! `invoke_command`), custom-command/shell execution, and the `:` palette.
//! Split out of `main.rs` as a `pub(crate)` `impl StatusView` block.

use gpui::{Context, KeyDownEvent, SharedString, Window};
use magritte_core::{
    transient::{self, Suffix, Transient},
    HeadInfo, RemoteTargets, SequenceKind,
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

        // A one-shot notice (e.g. "… is unbound") is dismissed by the next
        // keypress, so it doesn't linger over the action the user takes next.
        if self.toast.transient && self.toast.message.is_some() {
            self.clear_status(cx);
        }

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
        // help unless the context menu explicitly shows it as a view-local
        // action. Menu rows can be keyed on modifier chords (vanilla's `ctrl-w`
        // Copy), so match the full chord — a plain key's chord is its cased form.
        if let Some(Popup::Dispatch(def)) = &self.popup {
            // (A pending prefix's second key was already resolved above.)
            let chorded = chord(&key, shift, ctrl, alt, cmd);
            match chorded.as_str() {
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
                // An unbound key dismisses the help and reports it, like pressing
                // it on the underlying screen would.
                k => {
                    self.popup = None;
                    if !cmd && !alt && !ctrl {
                        self.report_unbound(k, cx);
                    }
                    cx.notify();
                }
            }
            return;
        }

        // A pending discard confirmation captures the next key. Modifier
        // chords pass over it (cmd-C copying the prompt text must not read as
        // "no"); any plain key other than `y` declines.
        if self.confirm.is_some() {
            if cmd || alt || ctrl {
                return;
            }
            if key == "y" {
                self.confirm_yes(window, cx);
            } else {
                self.confirm_no(window, cx);
            }
            return;
        }

        // Plain keys only: `cased` has modifiers stripped, so without this
        // guard cmd-R/ctrl-R during a paused rebase would be swallowed here
        // instead of reaching the OS/app shortcut it belongs to.
        if let Some(kind) = self.sequence_kind().filter(|_| !(ctrl || alt || cmd)) {
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
        // remains reachable from detail/log screens. M-x and `:` (`;`+shift)
        // reach it from every screen too: `:` dispatches through the keymap
        // first (vanilla's git-command on the screens that bind it), with
        // `run_dispatch` falling back to the palette everywhere else.
        if cmd && matches!(key.as_str(), "p" | "k") {
            return self.open_command_palette(window, cx);
        }
        if key == "x" && alt && !ctrl && !cmd {
            return self.open_command_palette(window, cx);
        }
        if (key == ":" || (key == ";" && shift)) && !ctrl && !alt && !cmd {
            return self.run_dispatch(&cased, window, cx);
        }
        if key == "?" || (key == "/" && shift) {
            self.popup = Some(Popup::Dispatch(dispatch_menu_for(self)));
            cx.notify();
            return;
        }

        // In evil, `y` is a yank *prefix* (yy/ys/yb/yr) in normal state, but a
        // direct yank of the selection in visual state — evil-collection's
        // visual-map `y`. So when a selection is active, copy immediately rather
        // than starting a sequence.
        if self.is_evil()
            && !shift
            && !ctrl
            && !alt
            && !cmd
            && key == "y"
            && self.has_visual_selection()
        {
            self.copy_at_point(cx);
            return;
        }

        // The git command-log view takes over the window; esc/q/$ close it, and
        // it scrolls with the usual vi/less keys (the shared `pager_key`).
        if self.git_log().is_some() {
            // `$` (also shift-4) closes, mirroring the key that opened the pager.
            if key == "$" || (key == "4" && shift) {
                return self.close_screen(window, cx);
            }
            return self.pager_key(&key, shift, ctrl, alt, cmd, window, cx);
        }

        // The blame view is a pager too (no cursor): `Esc`/`q` close via the
        // registry, motions translate to less-style scrolling.
        if matches!(self.screen, Screen::Blame { .. }) {
            return self.pager_key(&key, shift, ctrl, alt, cmd, window, cx);
        }

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
            self.dispatch_or_report(&key, shift, ctrl, alt, cmd, window, cx);
            return;
        }

        // The commit- and diff-view flat diffs share the same apply-engine verbs
        // (apply/reverse/reverse-index, visual toggle, details) — all in the
        // registry, keyed per-context. `Esc`/`q` (the `close` verb) cancels a
        // visual selection first, then leaves the view.
        if self.commit_view().is_some() || self.diff_view().is_some() {
            self.dispatch_or_report(&key, shift, ctrl, alt, cmd, window, cx);
            return;
        }

        // The commit-log view: every verb (open/select/cherry-pick/revert/
        // reset/rebase-since/relimit) is a registry command scoped to the Log
        // context; motions and copy resolve through the shared dispatch too.
        if self.log().is_some() {
            self.dispatch_or_report(&key, shift, ctrl, alt, cmd, window, cx);
            return;
        }

        // The refs browser: motions move the cursor (skipping headers); Enter
        // checks out the ref at point, the preset delete key removes it.
        if self.refs_view().is_some() {
            self.dispatch_or_report(&key, shift, ctrl, alt, cmd, window, cx);
            return;
        }

        // The worktree browser: motions move the cursor; the registry owns visit,
        // remove, and the add/branch/move creators.
        if self.worktree_view().is_some() {
            self.dispatch_or_report(&key, shift, ctrl, alt, cmd, window, cx);
            return;
        }

        // SPC on a commit/stash row previews it (magit's show-or-scroll-up),
        // rather than paging — a heavily used peek flow. SPC anywhere else falls
        // through to paging (try_nav below). Plain Space only, status screen only.
        if key == "space"
            && !shift
            && !ctrl
            && !alt
            && !cmd
            && matches!(self.screen, Screen::Status)
            && self.preview_at_point(cx)
        {
            return;
        }
        // Motions, paging, and the `g` prefix — remappable, applied screen-aware.
        if self.try_nav(&key, shift, ctrl, alt, cmd, window, cx) {
            return;
        }
        // Act on the commit/stash at point in a status section (after motions, so
        // j/k/g still work): an at-point verb — resolved from the keymap, gated on
        // the target — claims the key before the general keymap / diff-context
        // keys below, so `a` = cherry-apply on a commit but Stage on a file.
        let chorded = chord(&key, shift, ctrl, alt, cmd);
        if let Some(id) = self.resolve_binding(&chorded) {
            if commands().iter().any(|c| c.id == id && c.at_point) {
                self.invoke_command(&id, window, cx);
                return;
            }
        }
        match key.as_str() {
            // Tab toggles a fold (also delivered via the ToggleFold action, since
            // Root binds tab). Kept explicit — and out of the remappable keymap.
            // Shift-Tab falls through so a user binding for it can dispatch.
            "tab" if !shift => self.toggle_fold(cx),
            "escape" if !shift => {
                // Cancel a transient still opening (its config-variable load
                // hasn't landed), so it can't pop up after the quit.
                self.transient_open_gen.bump();
                // A running job takes priority: C-g/Esc kills its subprocess.
                // Otherwise cancel a visual selection, else dismiss the
                // status/error banner if one is showing.
                if self.cancel_job(cx) {
                    return;
                }
                let had_char = self.char_sel.take().is_some();
                if self.selection.visual.take().is_some()
                    || had_char
                    || self.toast.message.take().is_some()
                {
                    cx.notify();
                }
                return;
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
                if Self::is_dispatch_key(self.screen_bindings(), &chord) {
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
                state.toggle_switch(key.as_ref());
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

    /// The single context-scoped dispatcher: resolve `chord` in the active
    /// screen's keymap and run its command (if applicable now), or enter a
    /// prefix. Returns whether it consumed the key. Every screen dispatches
    /// through this one path (`run_info_key` is the `?`-menu click shim over
    /// it), so a key means whatever the registry says for that screen.
    pub(crate) fn dispatch_key(
        &mut self,
        chord: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_prefix(chord) {
            self.enter_prefix(chord.to_string(), window, cx);
            return true;
        }
        let Some(id) = self.resolve_binding(chord) else {
            return false;
        };
        self.invoke_command(&id, window, cx);
        true
    }

    /// Dispatch a keystroke on a secondary screen; if nothing claims it, report
    /// "… is unbound in <view> view". Only for plain/shifted keys — an unbound
    /// key held with cmd/alt/ctrl is usually an OS shortcut we don't model.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_or_report(
        &mut self,
        key: &str,
        shift: bool,
        ctrl: bool,
        alt: bool,
        cmd: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dispatch_key(&chord(key, shift, ctrl, alt, cmd), window, cx) {
            return;
        }
        if !cmd && !alt && !ctrl {
            self.report_unbound(&chord(key, shift, false, false, false), cx);
        }
    }

    /// The command a keystroke resolves to on the current screen: the first of
    /// its candidates (ordered most-specific-first by [`build_keymap`]) whose
    /// `enabled` holds. A candidate scoped out by its target — e.g. cherry-apply
    /// with no commit at point — is skipped so the key falls through to the next
    /// (Stage on a file row). `None` if the key is unbound or all candidates
    /// decline.
    pub(crate) fn resolve_binding(&self, chord: &str) -> Option<String> {
        first_enabled_candidate(self.screen_bindings().get(chord)?, |id| {
            commands()
                .iter()
                .find(|c| c.id == id)
                .is_none_or(|c| (c.enabled)(self))
        })
        .map(str::to_string)
    }

    /// Close the active secondary screen (the `close` command, `Esc`/`q`). In a
    /// flat-diff view, `Esc` first cancels an active visual selection (magit's
    /// two-step). A no-op on the status screen.
    pub(crate) fn close_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .flat_diff()
            .is_some_and(|fd| fd.visual.is_some() || fd.char_sel.is_some_and(|c| !c.is_empty()))
        {
            if let Some(fd) = self.flat_diff_mut() {
                fd.visual = None;
                fd.char_sel = None;
            }
            cx.notify();
            return;
        }
        // Esc first clears a log selection (char or line-wise), then closes.
        if self
            .log()
            .is_some_and(|l| l.char_sel.is_some_and(|c| !c.is_empty()) || l.visual.is_some())
        {
            if let Some(log) = self.log_mut() {
                log.char_sel = None;
                log.visual = None;
            }
            cx.notify();
            return;
        }
        match self.screen_kind() {
            ScreenKind::Log => self.close_log(window, cx),
            ScreenKind::GitLog => self.close_git_log(window, cx),
            ScreenKind::Commit => self.close_commit_view(window, cx),
            ScreenKind::Diff => self.close_diff_view(window, cx),
            ScreenKind::RebaseTodo => self.close_rebase_todo(window, cx),
            ScreenKind::Refs => self.close_refs(window, cx),
            ScreenKind::Worktree => self.close_worktrees(window, cx),
            ScreenKind::Blame => self.close_blame(window, cx),
            ScreenKind::Settings => self.close_settings(window, cx),
            ScreenKind::Status | ScreenKind::Editor => {}
        }
    }

    pub(crate) fn run_dispatch(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        // A keymap-bound command (default or user-remapped), the `:` palette, or
        // a motion. Resolving through the effective keymap is what makes
        // remap/unbind take effect — and binding *any* command id (even a leaf
        // like `branch.delete`) to a key Just Works via `invoke_command`.
        if let Some(id) = self.resolve_binding(key) {
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

    /// The custom-command placeholder names (`{file}`, `{branch}`, …), resolved
    /// by [`placeholder_value`](Self::placeholder_value).
    pub(crate) const PLACEHOLDERS: &'static [&'static str] = &[
        "file",
        "commit",
        "branch",
        "upstream",
        "push-remote",
        "default-branch",
        "default-remote",
    ];

    /// Resolve one placeholder name against the current selection and repo
    /// state; `Err` (with why) when it doesn't apply right now.
    fn placeholder_value(&self, name: &str) -> Result<String, String> {
        let head = |sel: fn(&HeadInfo) -> Option<String>| {
            self.status.as_ref().and_then(|st| sel(&st.head))
        };
        match name {
            "file" => self
                .path_at_point()
                .ok_or_else(|| "No file at point for {file}".to_string()),
            // The commit at point: the log selection, else a status commit row
            // (unpushed/unpulled/recent), else the open commit view.
            "commit" => self
                .log()
                .and_then(|l| l.entries.get(l.selected))
                .map(|e| e.hash.clone())
                .or_else(|| self.point_commit().map(|(hash, _, _)| hash))
                .or_else(|| self.commit_view().map(|cv| cv.rev.clone()))
                .ok_or_else(|| "No commit at point for {commit}".to_string()),
            "branch" => head(|h| h.branch.clone())
                .ok_or_else(|| "No current branch for {branch}".to_string()),
            "upstream" => head(|h| h.upstream.clone())
                .ok_or_else(|| "No upstream configured for {upstream}".to_string()),
            // The resolved push remote, like the push/pull transients: an
            // explicit pushRemote/pushDefault, else the upstream's remote.
            "push-remote" => head(|h| RemoteTargets::from_head(h).push_remote)
                .ok_or_else(|| "No push-remote configured for {push-remote}".to_string()),
            "default-branch" => self
                .default_branch_cached()
                .map(|(_, branch)| branch)
                .ok_or_else(|| "No default branch found for {default-branch}".to_string()),
            // Resolved together with {default-branch}: the remote whose HEAD
            // named it, else the push-remote (when the default branch was only
            // found as a local mainline name) — so the pair can't disagree.
            "default-remote" => self
                .default_branch_cached()
                .and_then(|(remote, _)| remote)
                .or_else(|| head(|h| RemoteTargets::from_head(h).push_remote))
                .ok_or_else(|| "No default remote found for {default-remote}".to_string()),
            _ => Err(format!("unknown placeholder {{{name}}}")),
        }
    }

    /// The memoized `{default-branch}`/`{default-remote}` resolution: it
    /// shells out to git (remote HEADs, mainline probes), and placeholder
    /// expansion runs per displayed label — resolve once and reuse until the
    /// next status refresh clears it.
    fn default_branch_cached(&self) -> Option<(Option<String>, String)> {
        self.default_branch_cache
            .borrow_mut()
            .get_or_insert_with(|| {
                self.repo
                    .as_ref()
                    .and_then(|r| r.default_branch_remote().ok().flatten())
            })
            .clone()
    }

    /// Substitute the [`PLACEHOLDERS`](Self::PLACEHOLDERS) in a command against
    /// the current selection, each shell-quoted so a path with spaces stays one
    /// word. `Err` (with why) if a placeholder can't be resolved — e.g. `{file}`
    /// with no file at point.
    pub(crate) fn expand_placeholders(&self, command: &str) -> Result<String, String> {
        substitute_placeholders(command, |name| {
            self.placeholder_value(name)
                .map(|v| Some(shell_words::quote(&v).into_owned()))
        })
    }

    /// Expand placeholders for a display label (a command title): unquoted, and
    /// an unresolvable placeholder stays literal — a label must always render.
    pub(crate) fn expand_placeholders_display(&self, text: &str) -> String {
        substitute_placeholders(text, |name| Ok(self.placeholder_value(name).ok()))
            .unwrap_or_else(|_| text.to_string())
    }

    /// The configured (raw) title behind a displayed command label: palette and
    /// menu labels are placeholder-expanded, so by-title lookups map the label
    /// back to the `[[command]]` title it was expanded from first.
    pub(crate) fn raw_command_title(&self, label: &str) -> String {
        self.config
            .commands
            .iter()
            .find(|c| c.title.contains('{') && self.expand_placeholders_display(&c.title) == label)
            .map(|c| c.title.clone())
            .unwrap_or_else(|| label.to_string())
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
        // A bound sequence resolves to its first applicable candidate (an
        // at-point verb gated on the target, else the general command).
        if self.screen_bindings().contains_key(seq) {
            if let Some(id) = self.resolve_binding(seq) {
                return KeyMatch::Command(id);
            }
        }
        let lead = format!("{seq} ");
        if self.screen_bindings().keys().any(|k| k.starts_with(&lead)) {
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

    /// Whether an active selection (line-wise visual or a mouse char range) is
    /// present on the current screen — the flat-diff selection in a commit/diff
    /// view, or the status-list selection — so evil's `y` yanks it immediately
    /// instead of starting a prefix.
    pub(crate) fn has_visual_selection(&self) -> bool {
        if let Some(fd) = self.flat_diff() {
            fd.visual.is_some() || fd.char_sel.is_some_and(|c| !c.is_empty())
        } else if let Some(log) = self.log() {
            log.visual.is_some() || log.char_sel.is_some_and(|c| !c.is_empty())
        } else {
            matches!(self.screen, Screen::Status)
                && (self.selection.visual.is_some() || self.char_sel.is_some_and(|c| !c.is_empty()))
        }
    }

    /// Clear the mouse char selection and line-wise visual selection of the
    /// active view (flat-diff, log, or status). Returns whether anything was
    /// cleared. Used by Esc and by a click that lands off any selectable text.
    pub(crate) fn clear_point_selection(&mut self) -> bool {
        if let Some(fd) = self.flat_diff_mut() {
            return fd.visual.take().is_some() | fd.char_sel.take().is_some();
        }
        if let Some(log) = self.log_mut() {
            return log.visual.take().is_some() | log.char_sel.take().is_some();
        }
        self.selection.visual.take().is_some() | self.char_sel.take().is_some()
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
        let message = match self.screen_name() {
            Some(view) => format!("is unbound in {view} view"),
            None => "is unbound".to_string(),
        };
        self.set_status(message, true, cx);
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
        // Each entry carries its search corpus (title + hidden aliases, so
        // "yank" finds "Copy" and "add" finds "Stage") alongside the id (for
        // frecency ordering) and the displayed title.
        // The key/id hints are resolved in this same pass, from each command's
        // *raw* title: mapping the displayed label back through
        // `raw_command_title` per row re-expanded every user command's
        // placeholders per palette row — O(rows × commands) `{default-branch}`
        // resolutions, each a git subprocess, which froze the open for seconds.
        let bindings = self.screen_bindings();
        let kind = self.screen_kind();
        type Entry = (String, String, String, Option<String>, Option<String>);
        let mut entries: Vec<Entry> = all_commands(&self.config)
            .filter(|c| c.palette && (c.enabled)(self))
            // Only commands that dispatch on the current screen (how the `?`
            // menu scopes): a status-scoped act command run from another screen
            // would act on the invisible status cursor. User `[[command]]`s
            // (absent from the registry) are context-free.
            .filter(|c| {
                commands()
                    .iter()
                    .find(|b| b.id == c.id)
                    .is_none_or(|b| b.contexts.contains(kind))
            })
            .map(|c| {
                // User `[[command]]` titles may carry placeholders ({branch},
                // …); show and match them expanded, as they'd read on screen.
                let title = self.expand_placeholders_display(c.title);
                let search = if c.aliases.is_empty() {
                    title.to_lowercase()
                } else {
                    format!("{} {}", title, c.aliases.join(" ")).to_lowercase()
                };
                let keys = commands::command_keys(bindings, &self.config, c.title);
                let id = commands::command_id_for_title(&self.config, c.title);
                (c.id.to_string(), title, search, keys, id)
            })
            .collect();
        entries.sort_by(|a, b| {
            let (sa, sb) = (self.usage.score(&a.0), self.usage.score(&b.0));
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut hints = std::collections::HashMap::new();
        let (choices, search): (Vec<String>, Vec<String>) = entries
            .into_iter()
            .map(|(_, title, search, keys, id)| {
                hints.insert(
                    SharedString::from(title.clone()),
                    (keys.map(SharedString::from), id.map(SharedString::from)),
                );
                (title, search)
            })
            .unzip();
        self.open_picker_searchable(
            PickerAction::RunCommand,
            choices,
            Some(search),
            CreateMode::None,
            Vec::new(),
            window,
            cx,
        );
        if let Some(Popup::Picker(p)) = self.popup.as_ref() {
            *p.hints.borrow_mut() = hints;
        }
    }

    /// Whether `key` is a single-stroke dispatch key: bound in the effective
    /// keymap (a command), or one of the bare motions `j`/`k`/`G` and the `:`
    /// palette. Multi-stroke entries are handled elsewhere — Tab via the
    /// ToggleFold action, `g r`/`g g`/`g j`/`g k` via the g-prefix — so they're
    /// excluded even if a key like `g r` is bound.
    pub(crate) fn is_dispatch_key(keymap: &commands::KeyBindings, key: &str) -> bool {
        // Only single-keystroke chords reach here (multi-key sequences resolve
        // through the prefix machinery); motions are registry commands too.
        keymap.contains_key(key) || key == ":"
    }

    /// Preview the commit or stash at point in the commit view (magit's `SPC`
    /// show-or-scroll): the view overlays the status screen and Escape returns
    /// to the same row. Returns whether there was something to preview. Once the
    /// view is open, `SPC` there scrolls it (the normal paging motion).
    fn preview_at_point(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some((hash, _, subject)) = self.point_commit() {
            self.open_commit(hash, subject, cx);
            return true;
        }
        if let Some((reference, message)) = self.point_stash() {
            self.open_commit(reference, message, cx);
            return true;
        }
        false
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
        // On a secondary screen the `?` popup re-dispatches the chosen key
        // through the same per-context table that drives live input, so the menu
        // and the keyboard always agree.
        if !matches!(self.screen, Screen::Status) {
            self.dispatch_key(key, window, cx);
            return;
        }
        // Status: an at-point verb (gated on the commit/stash at point) claims the
        // key first, then the general keymap / `:` palette — mirroring `on_key`,
        // so the `?` menu and the keyboard dispatch identically.
        if let Some(id) = self.resolve_binding(key) {
            if commands().iter().any(|c| c.id == id && c.at_point) {
                return self.invoke_command(&id, window, cx);
            }
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

/// The first of a key's candidate command ids (ordered most-specific-first by
/// `build_keymap`) whose `enabled` holds — the pure core of
/// [`StatusView::resolve_binding`], separated so the priority/enablement scan
/// is testable without a live view.
pub(crate) fn first_enabled_candidate(
    candidates: &[String],
    enabled: impl Fn(&str) -> bool,
) -> Option<&str> {
    candidates.iter().map(String::as_str).find(|id| enabled(id))
}

/// Replace each `{name}` placeholder in `text` in one left-to-right pass — a
/// substituted value is emitted verbatim, never re-scanned, so a value that
/// itself contains a placeholder token (a file named `{branch}.txt`) can't be
/// re-substituted or break its shell quoting. `resolve` returns the
/// replacement, `Ok(None)` to keep the token literal, or `Err` to abort;
/// anything that isn't a known placeholder stays literal.
fn substitute_placeholders(
    text: &str,
    mut resolve: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let token = StatusView::PLACEHOLDERS.iter().find_map(|name| {
            rest[1..]
                .strip_prefix(name)
                .and_then(|tail| tail.strip_prefix('}'))
                .map(|tail| (*name, tail))
        });
        match token {
            Some((name, tail)) => {
                match resolve(name)? {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = tail;
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::substitute_placeholders;

    #[test]
    fn substitution_is_single_pass() {
        // A substituted value containing a later placeholder token is emitted
        // verbatim, not re-substituted — the quoting around it stays intact.
        let out = substitute_placeholders("cat {file} on {branch}", |name| {
            Ok(Some(match name {
                "file" => "'{branch}.txt'".to_string(),
                "branch" => "main".to_string(),
                other => panic!("unexpected placeholder {other}"),
            }))
        })
        .unwrap();
        assert_eq!(out, "cat '{branch}.txt' on main");
    }

    #[test]
    fn unknown_and_unresolved_tokens_stay_literal() {
        let out = substitute_placeholders("{nope} {branch} {branch", |name| {
            assert_eq!(name, "branch");
            Ok(None)
        })
        .unwrap();
        assert_eq!(out, "{nope} {branch} {branch");
        let err = substitute_placeholders("run {file}", |_| Err("no file".to_string()));
        assert_eq!(err, Err("no file".to_string()));
    }
}

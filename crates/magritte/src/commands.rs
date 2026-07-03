//! The command registry and keymap: the single source of truth for *what
//! commands exist* (the built-in [`commands`] table plus the user's
//! `[[command]]` definitions), how default keys bind to them ([`build_keymap`]),
//! and the `?` dispatch menu / `:` palette metadata. Split out of `main.rs`;
//! the command `run` closures call back into [`StatusView`] methods.

use std::collections::HashMap;

use gpui::{Context, Window};
use magritte_core::transient::{self, Suffix, TitleSpan, Transient};
use magritte_core::RemoteTargets;

use crate::*;

/// The arguments a leaf command runs with: the toggled switches/options, any
/// pathspec limits, the resolved remote targets, and the log commit limit.
/// Gathered from a transient's state, or [`ActionArgs::defaults`] for a
/// palette-fired command (no switches).
pub(crate) struct ActionArgs {
    pub(crate) args: Vec<String>,
    pub(crate) paths: Vec<String>,
    pub(crate) targets: RemoteTargets,
    pub(crate) limit: usize,
}

impl ActionArgs {
    pub(crate) fn defaults(targets: RemoteTargets, limit: usize) -> Self {
        Self {
            args: Vec::new(),
            paths: Vec::new(),
            targets,
            limit,
        }
    }
}

/// Groupings for the command registry — the `?` menu and `:` palette render in
/// this order.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    /// Git porcelain (commit, branch, push, …).
    Commands,
    /// App/chrome commands that aren't git operations (settings, command log).
    Application,
    /// Working-tree edits (stage / unstage / discard).
    Applying,
    /// Always-available essentials (fold, refresh, visual selection).
    Essential,
    /// Cursor motions (move, page, section, edges). Resolved through the keymap
    /// like any command — so they're remappable — but dispatched screen-aware.
    Navigation,
}

impl Category {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Category::Commands => "Commands",
            Category::Application => "Application",
            Category::Applying => "Applying changes",
            Category::Essential => "Essential",
            Category::Navigation => "Navigation",
        }
    }
}

/// A user-invokable command: a stable identity decoupled from the key that
/// triggers it. This is the single source of truth for *what a command does* —
/// the keymap (`on_key` via [`StatusView::run_command`], the `?` dispatch menu,
/// and the `:` command palette all resolve to one of these and call `run`.
/// Argument-taking commands (commit, branch, …) open their own picker/transient
/// from `run`; the registry deliberately doesn't model arguments.
#[derive(Clone, Copy)]
pub(crate) struct Command {
    /// Stable id, e.g. "stage", "branch", "push-upstream". Used by the keymap,
    /// the palette (resolving the chosen title), and tests.
    pub(crate) id: &'static str,
    /// Human label shown in the `?` menu and `:` palette.
    pub(crate) title: &'static str,
    /// Extra search terms for the `:` palette (search-only; never displayed), so
    /// a query that isn't in the magit-flavored title still surfaces the command
    /// — the git verb behind the action ("add" → Stage, "restore" → Discard) or
    /// a plainer synonym. Kept to unambiguous terms: an alias that collides with
    /// another command's identity ("checkout", "reset") is a footgun, not a help.
    pub(crate) aliases: &'static [&'static str],
    /// Which `?`-menu group / palette category it belongs to.
    pub(crate) category: Category,
    /// Default keybinding, as the dispatch menu renders it (e.g. "Z", "g r").
    /// `None` for leaf subcommands reached via a transient or the palette, not a
    /// top-level key.
    pub(crate) key: Option<&'static str>,
    /// Show in the `?` dispatch menu. Mirrors magit's curated dispatch: the
    /// top-level prefixes and direct actions, not every leaf.
    pub(crate) menu: bool,
    /// Offer in the `:` command palette. Mirrors magit's `M-x`: prefixes *and*
    /// the leaf subcommands (e.g. "Push current to upstream").
    pub(crate) palette: bool,
    /// Whether it makes sense to offer right now — the palette filters on this.
    /// (Permissive today; argument-gathering happens in `run`.)
    pub(crate) enabled: fn(&StatusView) -> bool,
    /// For a leaf, the transient suffix it fires — used to show its full key
    /// sequence (prefix + suffix, e.g. `c c`) in the palette. `None` for
    /// top-level prefixes/actions, which advertise their own `key`.
    pub(crate) leaf: Option<transient::Command>,
    /// Perform the command. May open a transient/picker or act immediately.
    pub(crate) run: fn(&mut StatusView, &mut Window, &mut Context<StatusView>),
}

/// The command registry: the one place commands are defined. Pure motions
/// (j/k/gg/G/gj/gk) are not commands and stay in the keymap. Keep keys in sync
/// with the modal handling in `on_key` (shift variants, the `g` prefix); the
/// `dispatch_menu_covers_every_command` test guards menu/registry/dispatch
/// against drift.
pub(crate) fn commands() -> &'static [Command] {
    use transient::Command as Leaf;
    const ALWAYS: fn(&StatusView) -> bool = |_| true;

    // A top-level prefix or direct action: bound to a key, in the `?` menu and
    // the palette.
    macro_rules! top {
        ($id:literal, $title:literal, $cat:expr, $key:literal, $run:expr) => {
            top!($id, $title, $cat, $key, &[], $run)
        };
        ($id:literal, $title:literal, $cat:expr, $key:literal, $aliases:expr, $run:expr) => {
            Command {
                id: $id,
                title: $title,
                aliases: $aliases,
                category: $cat,
                key: Some($key),
                menu: true,
                palette: true,
                enabled: ALWAYS,
                leaf: None,
                run: $run,
            }
        };
    }
    // A motion: bound to a key (so the keymap can remap it) but kept out of the
    // `?` menu and `:` palette. Its `run` is screen-aware.
    macro_rules! nav {
        ($id:literal, $title:literal, $key:literal, $run:expr) => {
            Command {
                id: $id,
                title: $title,
                aliases: &[],
                category: Category::Navigation,
                key: Some($key),
                menu: false,
                palette: false,
                enabled: ALWAYS,
                leaf: None,
                run: $run,
            }
        };
    }
    // A section jump (magit-status-jump): keyless in the registry — vanilla
    // reaches them through the `j` transient, evil through `g`-sequences — but
    // in the palette, offered only when the section is on screen.
    macro_rules! jump {
        ($id:literal, $title:literal, $section:expr) => {
            Command {
                id: $id,
                title: $title,
                aliases: &[],
                category: Category::Navigation,
                key: None,
                menu: false,
                palette: true,
                enabled: |t| {
                    t.rows
                        .iter()
                        .any(|r| matches!(&r.fold, Some(FoldKey::Section(s)) if *s == $section))
                },
                leaf: None,
                run: |t, _w, cx| t.jump_to_section($section, cx),
            }
        };
    }
    // A leaf subcommand (a transient suffix): no top-level key, palette-only —
    // it's surfaced in the `?` menu through its prefix's transient. Firing it
    // runs the action directly with default arguments.
    macro_rules! leaf {
        ($id:literal, $title:literal, $cmd:expr) => {
            leaf!($id, $title, &[], $cmd)
        };
        ($id:literal, $title:literal, $aliases:expr, $cmd:expr) => {
            Command {
                id: $id,
                title: $title,
                aliases: $aliases,
                category: Category::Commands,
                key: None,
                menu: false,
                palette: true,
                enabled: ALWAYS,
                leaf: Some($cmd),
                run: |t, w, cx| t.fire_command_default($cmd, w, cx),
            }
        };
    }

    const C: &[Command] = &[
        // Prefixes (open a transient).
        top!("commit", "Commit", Category::Commands, "c", |t, _w, cx| {
            t.open_transient(
                "commit",
                transient::commit_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("branch", "Branch", Category::Commands, "b", |t, _w, cx| {
            // The branch transient (checkout/create/rename/delete) doesn't use
            // remote targets, so don't resolve them just to open it.
            t.open_transient(
                "branch",
                transient::branch_transient(t.config.keymap_preset.transient_style()),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("tag", "Tag", Category::Commands, "t", |t, _w, cx| {
            t.open_transient(
                "tag",
                transient::tag_transient(t.config.keymap_preset.transient_style()),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("remote", "Remote", Category::Commands, "M", |t, _w, cx| {
            t.open_transient(
                "remote",
                transient::remote_transient(t.config.keymap_preset.transient_style()),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!("stash", "Stash", Category::Commands, "Z", |t, _w, cx| {
            t.open_transient(
                "stash",
                transient::stash_transient(),
                RemoteTargets::default(),
                cx,
            )
        }),
        top!(
            "reset",
            "Reset",
            Category::Commands,
            "O",
            &["roll back"],
            |t, _w, cx| {
                t.open_transient(
                    "reset",
                    transient::reset_transient(),
                    RemoteTargets::default(),
                    cx,
                )
            }
        ),
        top!(
            "git-command",
            "Run command",
            Category::Commands,
            "!",
            |t, w, cx| { t.open_run_git(w, cx) }
        ),
        top!("rebase", "Rebase", Category::Commands, "r", |t, _w, cx| {
            // While a rebase is paused, `r` opens the continue/skip/abort
            // transient (magit's `r r` = continue) rather than starting a new one.
            if matches!(
                t.sequence.as_ref().map(|s| s.kind),
                Some(SequenceKind::Rebase)
            ) {
                t.open_transient(
                    "",
                    transient::sequence_transient(
                        SequenceKind::Rebase,
                        t.config.keymap_preset.transient_style(),
                    ),
                    RemoteTargets::default(),
                    cx,
                );
            } else {
                let rt = t.remote_targets();
                t.open_transient("rebase", transient::rebase_transient(&rt), rt, cx);
            }
        }),
        top!(
            "merge",
            "Merge",
            Category::Commands,
            "m",
            &["combine"],
            |t, _w, cx| {
                // While a merge is in progress, `m` opens its abort action (you
                // finish a merge by committing); don't start another.
                if matches!(
                    t.sequence.as_ref().map(|s| s.kind),
                    Some(SequenceKind::Merge)
                ) {
                    t.open_transient(
                        "",
                        transient::sequence_transient(
                            SequenceKind::Merge,
                            t.config.keymap_preset.transient_style(),
                        ),
                        RemoteTargets::default(),
                        cx,
                    );
                } else {
                    t.open_transient(
                        "merge",
                        transient::merge_transient(),
                        RemoteTargets::default(),
                        cx,
                    );
                }
            }
        ),
        top!(
            "ignore",
            "Ignore",
            Category::Commands,
            "i",
            &["gitignore", "exclude", "skip"],
            |t, _w, cx| {
                t.open_transient(
                    "ignore",
                    transient::ignore_transient(),
                    RemoteTargets::default(),
                    cx,
                )
            }
        ),
        top!(
            "log",
            "Log",
            Category::Commands,
            "l",
            &["history", "commits"],
            |t, _w, cx| {
                t.open_transient(
                    "log",
                    transient::log_transient(),
                    RemoteTargets::default(),
                    cx,
                )
            }
        ),
        top!(
            "diff",
            "Diff",
            Category::Commands,
            "d",
            &["changes", "compare"],
            |t, _w, cx| {
                t.open_transient(
                    "diff",
                    transient::diff_transient(),
                    RemoteTargets::default(),
                    cx,
                )
            }
        ),
        // The refs browser (magit's show-refs). Vanilla binds `y` (magit); evil
        // binds `yr` via its `y` yank family.
        Command {
            id: "show-refs",
            title: "Show refs",
            aliases: &["references", "refs browser"],
            category: Category::Commands,
            key: None,
            menu: true,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |t, _w, cx| t.open_refs(cx),
        },
        top!(
            "push",
            "Push",
            Category::Commands,
            "p",
            &["publish", "upload"],
            |t, _w, cx| {
                let rt = t.remote_targets();
                t.open_transient("push", transient::push_transient(&rt), rt, cx)
            }
        ),
        top!("pull", "Pull", Category::Commands, "F", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient("pull", transient::pull_transient(&rt), rt, cx)
        }),
        top!("fetch", "Fetch", Category::Commands, "f", |t, _w, cx| {
            let rt = t.remote_targets();
            t.open_transient("fetch", transient::fetch_transient(&rt), rt, cx)
        }),
        // Leaf subcommands (palette-only; reached in the `?` menu via their
        // prefix's transient).
        leaf!("commit-create", "Create commit", Leaf::CommitCreate),
        leaf!(
            "commit-amend",
            "Amend commit",
            &["fixup last", "edit last commit"],
            Leaf::CommitAmend
        ),
        leaf!(
            "commit-reword",
            "Reword commit",
            &["rename commit", "edit message", "change message"],
            Leaf::CommitReword
        ),
        leaf!(
            "commit-extend",
            "Extend commit (keep message)",
            Leaf::CommitExtend
        ),
        leaf!(
            "push-pushremote",
            "Push current to push-remote",
            Leaf::PushPushRemote
        ),
        leaf!(
            "push-upstream",
            "Push current to upstream",
            Leaf::PushUpstream
        ),
        leaf!("push-elsewhere", "Push elsewhere", Leaf::PushElsewhere),
        leaf!(
            "pull-pushremote",
            "Pull from push-remote",
            Leaf::PullPushRemote
        ),
        leaf!("pull-upstream", "Pull from upstream", Leaf::PullUpstream),
        leaf!("pull-elsewhere", "Pull elsewhere", Leaf::PullElsewhere),
        leaf!(
            "fetch-pushremote",
            "Fetch push-remote",
            Leaf::FetchPushRemote
        ),
        leaf!("fetch-upstream", "Fetch upstream", Leaf::FetchUpstream),
        leaf!("fetch-all", "Fetch all remotes", Leaf::FetchAll),
        leaf!("fetch-elsewhere", "Fetch elsewhere", Leaf::FetchElsewhere),
        leaf!(
            "branch-checkout",
            "Checkout branch/revision",
            &["switch"],
            Leaf::BranchCheckout
        ),
        leaf!(
            "branch-create-checkout",
            "Create and checkout branch",
            &["checkout -b", "switch -c"],
            Leaf::BranchCreateCheckout
        ),
        leaf!(
            "branch-create",
            "Create branch",
            &["new branch"],
            Leaf::BranchCreate
        ),
        leaf!(
            "branch-rename",
            "Rename branch",
            &["move branch"],
            Leaf::BranchRename
        ),
        leaf!(
            "branch-delete",
            "Delete branch",
            &["remove branch"],
            Leaf::BranchDelete
        ),
        leaf!("tag-create", "Create tag", &["new tag"], Leaf::TagCreate),
        leaf!("tag-delete", "Delete tag", &["remove tag"], Leaf::TagDelete),
        leaf!("remote-add", "Add remote", &["new remote"], Leaf::RemoteAdd),
        leaf!("remote-rename", "Rename remote", Leaf::RemoteRename),
        leaf!(
            "remote-remove",
            "Remove remote",
            &["delete remote"],
            Leaf::RemoteRemove
        ),
        leaf!(
            "stash-push",
            "Stash worktree and index",
            &["save", "stash changes"],
            Leaf::StashPush
        ),
        leaf!(
            "stash-push-all",
            "Stash including untracked",
            Leaf::StashPushAll
        ),
        leaf!("stash-apply", "Apply stash", Leaf::StashApply),
        leaf!("stash-pop", "Pop stash", Leaf::StashPop),
        leaf!(
            "stash-drop",
            "Drop stash",
            &["delete stash", "remove stash"],
            Leaf::StashDrop
        ),
        leaf!("log-current", "Log current", Leaf::LogCurrent),
        leaf!("log-all", "Log all branches", Leaf::LogAll),
        leaf!("log-other", "Log other ref", Leaf::LogOther),
        leaf!("log-reflog", "Reflog", Leaf::LogReflog),
        leaf!("diff-dwim", "Diff smart target", Leaf::DiffDwim),
        leaf!("diff-range", "Diff range", Leaf::DiffRange),
        leaf!("diff-unstaged", "Diff unstaged", Leaf::DiffUnstaged),
        leaf!(
            "diff-staged",
            "Diff staged",
            &["diff cached"],
            Leaf::DiffStaged
        ),
        leaf!("diff-worktree", "Diff worktree", Leaf::DiffWorktree),
        leaf!("diff-commit", "Show commit", Leaf::DiffCommit),
        leaf!(
            "cherry-pick",
            "Cherry-pick commit",
            &["pick"],
            Leaf::CherryPick
        ),
        leaf!(
            "cherry-pick-range",
            "Cherry-pick range",
            Leaf::CherryPickRange
        ),
        leaf!("cherry-apply", "Apply commit", Leaf::CherryApply),
        leaf!(
            "revert",
            "Revert commit",
            &["undo commit"],
            Leaf::RevertCommit
        ),
        leaf!("revert-range", "Revert range", Leaf::RevertRange),
        leaf!(
            "revert-no-commit",
            "Revert changes",
            &["undo changes"],
            Leaf::RevertNoCommit
        ),
        // Application commands.
        top!(
            "settings",
            "Settings",
            Category::Application,
            ",",
            &["preferences", "config", "options"],
            |t, w, cx| { t.open_settings(w, cx) }
        ),
        top!(
            "command-log",
            "Command log",
            Category::Application,
            "$",
            &["process", "git output", "console"],
            |t, _w, cx| { t.open_git_log(cx) }
        ),
        Command {
            id: "check-updates",
            title: "Check for updates",
            aliases: &["upgrade", "new version"],
            category: Category::Application,
            key: None,
            menu: false,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |t, _w, cx| t.check_for_updates(cx),
        },
        // The `?` accelerator opens this too; a registry entry so vanilla's `h`
        // (magit binds both `h` and `?` to the dispatch) and the palette reach it.
        Command {
            id: "help",
            title: "Help",
            aliases: &["dispatch", "keybindings", "shortcuts"],
            category: Category::Application,
            key: None,
            menu: false, // it *is* the menu
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |t, _w, cx| {
                t.popup = Some(Popup::Dispatch(dispatch_menu_for(t)));
                cx.notify();
            },
        },
        // Applying changes.
        top!(
            "stage",
            "Stage",
            Category::Applying,
            "s",
            &["add", "git add"],
            |t, _w, cx| t.act(Op::Stage, cx)
        ),
        top!(
            "unstage",
            "Unstage",
            Category::Applying,
            "u",
            &["remove from index"],
            |t, _w, cx| t.act(Op::Unstage, cx)
        ),
        top!(
            "stage-all",
            "Stage all tracked",
            Category::Applying,
            "S",
            &["add all", "git add all"],
            |t, _w, cx| { t.stage_all_command(cx) }
        ),
        top!(
            "unstage-all",
            "Unstage all",
            Category::Applying,
            "U",
            |t, _w, cx| { t.unstage_all_command(cx) }
        ),
        top!(
            "discard",
            "Discard",
            Category::Applying,
            "x",
            &["restore", "throw away"],
            |t, _w, cx| t.act(Op::Discard, cx)
        ),
        // Essentials.
        top!(
            "open-file",
            "Open file",
            Category::Essential,
            "enter",
            &["edit", "visit"],
            |t, _w, cx| { t.open_at_point(cx) }
        ),
        top!(
            "fold",
            "Fold / unfold",
            Category::Essential,
            "tab",
            &["collapse", "expand", "toggle"],
            |t, _w, cx| { t.toggle_fold(cx) }
        ),
        top!(
            "refresh",
            "Refresh",
            Category::Essential,
            "g r",
            &["reload", "update"],
            |t, _w, cx| {
                t.refresh(cx);
                cx.notify();
            }
        ),
        top!(
            "visual",
            "Visual selection",
            Category::Essential,
            "v",
            &["select", "mark", "region"],
            |t, _w, cx| {
                t.selection.visual = if t.selection.visual.is_some() {
                    None
                } else {
                    Some(t.selected)
                };
                cx.notify();
            }
        ),
        top!(
            "yank",
            "Copy",
            Category::Essential,
            "y",
            &["yank", "clipboard", "copy value"],
            |t, _w, cx| t.copy_at_point(cx)
        ),
        // The buffer's revision (evil `y b`, magit-copy-buffer-revision):
        // palette-only, bound to `yb` in the evil preset.
        Command {
            id: "copy-buffer-revision",
            title: "Copy revision",
            aliases: &["copy hash", "copy sha", "copy commit", "yank revision"],
            category: Category::Essential,
            key: None,
            menu: false,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |t, _w, cx| t.copy_buffer_revision(cx),
        },
        // Motions: resolved through the keymap (so remappable) but applied
        // screen-aware via the `nav_*` helpers. Kept out of the `?` menu and the
        // `:` palette (`menu: false`/`palette: false`) — cursor motions are
        // standard vim/emacs conventions and would only clutter the menu.
        nav!("move-down", "Move down", "j", |t, _w, cx| t.nav_line(1, cx)),
        nav!("move-up", "Move up", "k", |t, _w, cx| t.nav_line(-1, cx)),
        nav!("goto-top", "Top", "g g", |t, _w, cx| t.nav_edge(false, cx)),
        nav!("goto-bottom", "Bottom", "G", |t, _w, cx| t
            .nav_edge(true, cx)),
        // Section motions, magit's two granularities: `next-section` visits
        // every section start (headers, files, commits, hunks — like magit's
        // `n`); the sibling variants stay at the current depth (magit's `M-n`).
        nav!("next-section", "Next section", "ctrl-j", |t, _w, cx| t
            .nav_section(true, cx)),
        nav!("prev-section", "Previous section", "ctrl-k", |t, _w, cx| t
            .nav_section(false, cx)),
        nav!(
            "next-sibling-section",
            "Next sibling section",
            "g j",
            |t, _w, cx| t.nav_section_sibling(true, cx)
        ),
        nav!(
            "prev-sibling-section",
            "Previous sibling section",
            "g k",
            |t, _w, cx| t.nav_section_sibling(false, cx)
        ),
        nav!("section-up", "Parent section", "^", |t, _w, cx| t
            .nav_section_up(cx)),
        // Fold depth (magit's magit-section-show-level-N, applied buffer-wide;
        // M-1..M-4 alias in both presets).
        nav!("show-level-1", "Fold to sections", "1", |t, _w, cx| t
            .nav_show_level(1, cx)),
        nav!("show-level-2", "Fold to files", "2", |t, _w, cx| t
            .nav_show_level(2, cx)),
        nav!("show-level-3", "Fold to hunks", "3", |t, _w, cx| t
            .nav_show_level(3, cx)),
        nav!("show-level-4", "Unfold everything", "4", |t, _w, cx| t
            .nav_show_level(4, cx)),
        // Section jumps (magit-status-jump): the `j` transient in vanilla,
        // direct `g`-sequences in evil (evil-collection's gz/gn/gu/gs/gf*/gp*).
        Command {
            id: "status-jump",
            title: "Jump to section",
            aliases: &[],
            category: Category::Essential,
            key: None, // vanilla binds `j` (magit); evil uses the g-sequences
            menu: true,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |t, _w, cx| {
                t.open_transient(
                    "status-jump",
                    jump_transient(),
                    RemoteTargets::default(),
                    cx,
                )
            },
        },
        jump!("jump-to-stashes", "Jump to stashes", SectionId::Stashes),
        jump!(
            "jump-to-untracked",
            "Jump to untracked files",
            SectionId::Untracked
        ),
        jump!(
            "jump-to-ignored",
            "Jump to ignored files",
            SectionId::Ignored
        ),
        jump!(
            "jump-to-unstaged",
            "Jump to unstaged changes",
            SectionId::Unstaged
        ),
        jump!(
            "jump-to-staged",
            "Jump to staged changes",
            SectionId::Staged
        ),
        jump!(
            "jump-to-unpulled-upstream",
            "Jump to unpulled commits",
            SectionId::Unpulled
        ),
        jump!(
            "jump-to-unpulled-pushremote",
            "Jump to unpulled (push remote)",
            SectionId::UnpulledPushremote
        ),
        jump!(
            "jump-to-unpushed-upstream",
            "Jump to unpushed commits",
            SectionId::Unpushed
        ),
        jump!(
            "jump-to-unpushed-pushremote",
            "Jump to unpushed (push remote)",
            SectionId::UnpushedPushremote
        ),
        nav!("half-page-down", "Half page down", "ctrl-d", |t, w, cx| t
            .nav_page(true, false, w, cx)),
        nav!("half-page-up", "Half page up", "ctrl-u", |t, w, cx| t
            .nav_page(false, false, w, cx)),
        nav!("page-down", "Page down", "ctrl-f", |t, w, cx| t
            .nav_page(true, true, w, cx)),
        nav!("page-up", "Page up", "ctrl-b", |t, w, cx| t
            .nav_page(false, true, w, cx)),
        // Quit (Emacs `C-x C-c`, bound by the preset): no single key, so a
        // literal rather than `top!`. Reachable via the palette too.
        Command {
            id: "quit",
            title: "Quit",
            aliases: &["exit", "close"],
            category: Category::Application,
            key: None,
            menu: false,
            palette: true,
            enabled: ALWAYS,
            leaf: None,
            run: |_t, _w, cx| cx.quit(),
        },
    ];
    C
}

/// Default *secondary* key bindings: aliases layered onto the registry's primary
/// keys in [`build_keymap`] (before the user's `[keymap]`, so they're remappable
/// and unbindable like any default). These keep modifier/arrow/sequence aliases
/// in the one keymap rather than hardcoded in the key handler.
pub(crate) const EVIL_COLLECTION_BINDINGS: &[(&str, &str)] = &[
    // Arrow + Emacs cursor motions.
    ("down", "move-down"),
    ("up", "move-up"),
    ("ctrl-n", "move-down"),
    ("ctrl-p", "move-up"),
    // Paging: full page also on Space.
    ("space", "page-down"),
    // Sibling-section motion — evil-collection's `gj`/`]`/`M-j` (the primary
    // `g j` comes from the registry; `C-j`/`C-k` are the fine-grained motions).
    ("alt-j", "next-sibling-section"),
    ("alt-k", "prev-sibling-section"),
    ("]", "next-sibling-section"),
    ("[", "prev-sibling-section"),
    // Visual line: `V` mirrors `v` (our selection is already line-wise), as in
    // evil-collection-magit.
    ("V", "visual"),
    // magit-mode-map's C-w (copy the value at point) — kept in both presets.
    ("ctrl-w", "yank"),
    // evil-collection-magit's `y` yank family: `y` is a prefix, so copy is
    // `yy`/`ys` (both our context copy — we don't split whole-line from
    // section-value), plus `yb` buffer revision and `yr` show-refs. `Cmd-C`
    // copies directly, without the prefix.
    ("y y", "yank"),
    ("y s", "yank"),
    ("y b", "copy-buffer-revision"),
    ("y r", "show-refs"),
    ("cmd-c", "yank"),
    // Fold-level aliases (magit's M-1..M-4).
    ("alt-1", "show-level-1"),
    ("alt-2", "show-level-2"),
    ("alt-3", "show-level-3"),
    ("alt-4", "show-level-4"),
    // Section jumps: evil-collection's direct `g`-sequences (vanilla gets the
    // `j` transient instead).
    ("g z", "jump-to-stashes"),
    ("g n", "jump-to-untracked"),
    ("g i", "jump-to-ignored"),
    ("g u", "jump-to-unstaged"),
    ("g s", "jump-to-staged"),
    ("g f u", "jump-to-unpulled-upstream"),
    ("g f p", "jump-to-unpulled-pushremote"),
    ("g p u", "jump-to-unpushed-upstream"),
    ("g p p", "jump-to-unpushed-pushremote"),
    // Evil-collection-magit remaps Magit's direct `:` git-command binding to
    // `|`; Magit's `!` run-command transient remains the canonical key.
    ("|", "git-command"),
    // Emacs quit.
    ("ctrl-x ctrl-c", "quit"),
];

pub(crate) const VANILLA_BINDINGS: &[(&str, &str)] = &[
    // Emacs cursor motions stay available because vanilla Magit uses `n`/`p`
    // for section movement, not line movement.
    ("down", "move-down"),
    ("up", "move-up"),
    ("ctrl-n", "move-down"),
    ("ctrl-p", "move-up"),
    // Paging: Space/DEL mirror magit's scroll pair; C-v/M-v are Emacs' own.
    ("space", "page-down"),
    ("backspace", "page-up"),
    ("ctrl-v", "page-down"),
    ("alt-v", "page-up"),
    // Buffer edges (Emacs beginning/end-of-buffer).
    ("alt-<", "goto-top"),
    ("alt->", "goto-bottom"),
    ("n", "next-section"),
    ("p", "prev-section"),
    // Sibling motion, magit's `M-n`/`M-p`.
    ("alt-n", "next-sibling-section"),
    ("alt-p", "prev-sibling-section"),
    // Region selection on set-mark; copy on magit's `magit-copy-section-value`.
    ("ctrl-space", "visual"),
    ("ctrl-w", "yank"),
    ("cmd-c", "yank"),
    // Magit binds both `h` and `?` to the dispatch (`?` is the fixed key).
    ("h", "help"),
    // Fold-level aliases (magit's M-1..M-4).
    ("alt-1", "show-level-1"),
    ("alt-2", "show-level-2"),
    ("alt-3", "show-level-3"),
    ("alt-4", "show-level-4"),
    // Magit's `G` is refresh-all; we have one buffer, so alias plain refresh.
    ("G", "refresh"),
    (":", "git-command"),
    ("Q", "git-command"),
    ("ctrl-x ctrl-c", "quit"),
];

/// The `j` jump menu (magit-status-jump), suffixes keyed as magit keys them —
/// including the two-keystroke `fu`/`fp`/`pu`/`pp`. Each entry dispatches its
/// registry `jump-to-*` command.
pub(crate) fn jump_transient() -> Transient {
    let entry = |key: &str, description: &str, id: &str| {
        Suffix::Custom(transient::Custom {
            key: key.to_string(),
            description: description.to_string(),
            id: id.to_string(),
        })
    };
    Transient {
        title: transient::plain_title("Jump to section"),
        groups: vec![Group {
            title: transient::plain_title("Jump to"),
            suffixes: vec![
                entry("z", "Stashes", "jump-to-stashes"),
                entry("n", "Untracked files", "jump-to-untracked"),
                entry("i", "Ignored files", "jump-to-ignored"),
                entry("u", "Unstaged changes", "jump-to-unstaged"),
                entry("s", "Staged changes", "jump-to-staged"),
                entry("fu", "Unpulled from upstream", "jump-to-unpulled-upstream"),
                entry(
                    "fp",
                    "Unpulled from push remote",
                    "jump-to-unpulled-pushremote",
                ),
                entry("pu", "Unpushed to upstream", "jump-to-unpushed-upstream"),
                entry(
                    "pp",
                    "Unpushed to push remote",
                    "jump-to-unpushed-pushremote",
                ),
            ],
        }],
    }
}

pub(crate) fn default_key_for_command(
    preset: config::KeymapPreset,
    cmd: &Command,
) -> Option<&'static str> {
    use config::KeymapPreset::*;
    match preset {
        // evil-collection-magit makes `y` a yank *prefix* (`yy`/`ys`/`yb`/`yr`),
        // so copy is on `C-w`/`Cmd-C` and the sequences below — never a bare `y`.
        EvilCollection => match cmd.id {
            "yank" => None,
            _ => cmd.key,
        },
        Vanilla => match cmd.id {
            "push" => Some("P"),
            "reset" => Some("X"),
            "stash" => Some("z"),
            "discard" => Some("k"),
            "refresh" => Some("g"),
            "status-jump" => Some("j"),
            "show-refs" => Some("y"),
            // `n`/`p` and `M-n`/`M-p` are aliases below; no Ctrl-j/`g j` in vanilla.
            "next-section" | "prev-section" => None,
            "next-sibling-section" | "prev-sibling-section" => None,
            "move-down" | "move-up" | "goto-top" | "goto-bottom" | "visual" | "yank" => None,
            _ => cmd.key,
        },
    }
}

fn preset_bindings(preset: config::KeymapPreset) -> &'static [(&'static str, &'static str)] {
    match preset {
        config::KeymapPreset::EvilCollection => EVIL_COLLECTION_BINDINGS,
        config::KeymapPreset::Vanilla => VANILLA_BINDINGS,
    }
}

/// Canonical keystroke string for a keypress: word modifier prefixes (`cmd-`,
/// `ctrl-`, `alt-`, in that order) then the key, with a shifted letter
/// uppercased (so `K`, not `shift-k`). One token; multi-key sequences join these
/// with spaces (`ctrl-x ctrl-c`). The prefixes match `kbd::format_keys`, so the
/// display ("Ctrl+x") follows for free.
pub(crate) fn chord(key: &str, shift: bool, ctrl: bool, alt: bool, cmd: bool) -> String {
    let base = if shift {
        match key {
            "1" => "!".to_string(),
            "4" => "$".to_string(),
            "6" => "^".to_string(),
            "-" => "_".to_string(),
            "=" => "+".to_string(),
            "[" => "{".to_string(),
            "]" => "}".to_string(),
            "\\" => "|".to_string(),
            ";" => ":".to_string(),
            "'" => "\"".to_string(),
            "," => "<".to_string(),
            "." => ">".to_string(),
            "/" => "?".to_string(),
            "`" => "~".to_string(),
            _ if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic()) => {
                key.to_uppercase()
            }
            _ => key.to_string(),
        }
    } else {
        key.to_string()
    };
    let mut s = String::new();
    if cmd {
        s.push_str("cmd-");
    }
    if ctrl {
        s.push_str("ctrl-");
    }
    if alt {
        s.push_str("alt-");
    }
    s.push_str(&base);
    s
}

/// Lightweight metadata for any command — built-in or user `[[command]]`.
/// The cross-cutting consumers (keymap/transient validation, the palette,
/// suffix labels) read commands through [`all_commands`], so none of them can
/// silently forget user commands; only dispatch ([`StatusView::invoke_command`])
/// and built-in-specific key logic touch the two kinds directly.
#[derive(Clone, Copy)]
pub(crate) struct CommandInfo<'a> {
    pub(crate) id: &'a str,
    pub(crate) title: &'a str,
    /// Search-only synonyms for the `:` palette (empty for user `[[command]]`s).
    pub(crate) aliases: &'a [&'a str],
    /// Whether it appears in the `:` palette.
    pub(crate) palette: bool,
    /// Whether it's applicable right now.
    pub(crate) enabled: fn(&StatusView) -> bool,
}

/// Every command the user can refer to by id or title: the built-in registry,
/// then the user's `[[command]]` definitions. The single source of truth for
/// "what commands exist" — bind/run targets, the palette, and transient
/// injections all resolve through this.
pub(crate) fn all_commands(config: &config::Config) -> impl Iterator<Item = CommandInfo<'_>> {
    const ALWAYS: fn(&StatusView) -> bool = |_| true;
    commands()
        .iter()
        .map(|c| CommandInfo {
            id: c.id,
            title: c.title,
            aliases: c.aliases,
            palette: c.palette,
            enabled: c.enabled,
        })
        .chain(config.commands.iter().map(|c| CommandInfo {
            id: &c.id,
            title: &c.title,
            aliases: &[],
            palette: true,
            enabled: ALWAYS,
        }))
}

/// The effective keystroke → command-id map: the built-in defaults (every
/// registry command that has a key) overlaid with the user's `[keymap]`. A value
/// of `"unbound"` removes a default binding; an unknown id is skipped with a
/// warning rather than dropped silently. Navigation, command prefixes, and
/// aliases all live in this same map, so preset changes and user overrides flow
/// through one dispatch path.
pub(crate) fn build_keymap(config: &config::Config) -> (HashMap<String, String>, Vec<String>) {
    let mut map: HashMap<String, String> = commands()
        .iter()
        .filter_map(|c| {
            default_key_for_command(config.keymap_preset, c)
                .map(|key| (key.to_string(), c.id.to_string()))
        })
        .collect();
    // Secondary aliases (arrows, Emacs motions, Space, `C-x C-c`) — layered
    // before the user's table so they remap/unbind like any default.
    for (key, id) in preset_bindings(config.keymap_preset) {
        map.insert(key.to_string(), id.to_string());
    }
    let mut warnings = Vec::new();
    // A binding target is valid if it names any command — built-in or user.
    let known = |id: &str| all_commands(config).any(|c| c.id == id);
    for (keystroke, id) in &config.keymap {
        if id == "unbound" {
            map.remove(keystroke);
        } else if !known(id) {
            warnings.push(format!("keymap: unknown command id \"{id}\""));
        } else {
            // Any keystroke sequence is allowed, to any depth — `dispatch`
            // accumulates keys until one resolves to a binding (or to nothing).
            map.insert(keystroke.clone(), id.clone());
        }
    }
    // Validate the user `[[command]]` definitions.
    let mut seen_ids = HashSet::new();
    for c in &config.commands {
        if c.run.trim().is_empty() {
            warnings.push(format!("command \"{}\": empty run", c.id));
        }
        if commands().iter().any(|b| b.id == c.id) {
            warnings.push(format!("command \"{}\": shadows a built-in command", c.id));
        }
        if !seen_ids.insert(c.id.as_str()) {
            warnings.push(format!("command \"{}\": duplicate id", c.id));
        }
    }
    // Validate the `[transient]` suffix injections: the section must name a
    // transient; a `-`-prefixed value is a custom switch (its key must also be
    // dash-prefixed to toggle), anything else names a command. (The injection
    // itself happens in `open_transient`.)
    for (tid, suffixes) in &config.transient {
        if !TRANSIENT_IDS.contains(&tid.as_str()) {
            warnings.push(format!("transient: \"{tid}\" is not a transient"));
            continue;
        }
        for (key, spec) in suffixes {
            // `"key" = "unbound"` removes a built-in suffix; not a command id.
            if spec.is_unbound() {
                continue;
            }
            match spec.kind() {
                config::SuffixKind::Action { id, .. } if !known(id) => {
                    warnings.push(format!("transient.{tid}: unknown command id \"{id}\""));
                }
                config::SuffixKind::Switch { .. } if !key.starts_with('-') => {
                    warnings.push(format!(
                        "transient.{tid}: switch \"{key}\" should be dash-prefixed (e.g. \"-{key}\") to toggle"
                    ));
                }
                _ => {}
            }
        }
    }
    // A sequence is unreachable if a shorter prefix of it is bound to a command:
    // pressing that prefix fires its command, so the rest of the sequence never
    // arrives (exact match wins over waiting). Adding a key *under* such a
    // command — e.g. inside the commit transient — is what `[transient.<id>]` is
    // for, so point there when the shadower is a transient.
    let sequences: Vec<String> = map.keys().filter(|k| k.contains(' ')).cloned().collect();
    for k in sequences {
        let tokens: Vec<&str> = k.split(' ').collect();
        for i in 1..tokens.len() {
            let prefix = tokens[..i].join(" ");
            if let Some(shadower) = map.get(&prefix) {
                let hint = if TRANSIENT_IDS.contains(&shadower.as_str()) {
                    format!("; add it inside that menu with [transient.{shadower}]")
                } else {
                    String::new()
                };
                warnings.push(format!(
                    "keymap: \"{k}\" is unreachable — \"{prefix}\" runs \"{shadower}\"{hint}"
                ));
                break;
            }
        }
    }
    // Validate `[status].sections`: each id must name a real section.
    for id in &config.status.sections {
        if SectionId::from_config_id(id).is_none() {
            warnings.push(format!("status: unknown section \"{id}\""));
        }
    }
    (map, warnings)
}

/// Most output lines a command's toast shows before it's cut off (with a
/// pointer to the `$` log for the rest).
pub(crate) const MAX_TOAST_LINES: usize = 10;

/// The toast text for a finished user command (`!` or `[[command]]`): its output
/// (trimmed stdout, then stderr), or a short fallback when it printed nothing.
/// Output longer than [`MAX_TOAST_LINES`] is cut off with a pointer to the full
/// record in the `$` log — shown with its current key (`log_key`) when bound.
pub(crate) fn command_toast(run: &magritte_core::CommandRun, log_key: Option<&str>) -> String {
    let parts: Vec<&str> = [run.stdout.trim(), run.stderr.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return if run.ok { "done" } else { "command failed" }.to_string();
    }
    let text = parts.join("\n");
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_TOAST_LINES {
        return text;
    }
    let more = lines.len() - MAX_TOAST_LINES;
    let hint = match log_key {
        Some(key) => format!("press {} for the full output", kbd::format_keys(key)),
        None => "open the command log for the full output".to_string(),
    };
    format!(
        "{}\n… {more} more lines ({hint})",
        lines[..MAX_TOAST_LINES].join("\n")
    )
}

/// Whether a custom command looks like it could throw away work — so the
/// frontend confirms first, like the built-in destructive ops. A word-level
/// scan for `clean`, `--hard`, or `--force`/`--force-with-lease`.
pub(crate) fn command_is_destructive(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|w| matches!(w, "clean" | "--hard" | "--force" | "--force-with-lease"))
}

/// The command ids whose `?`/key opens a transient — the valid `[transient.<id>]`
/// sections for suffix injection.
pub(crate) const TRANSIENT_IDS: &[&str] = &[
    "commit", "branch", "tag", "remote", "stash", "reset", "rebase", "merge", "ignore", "log",
    "diff", "push", "pull", "fetch",
];

/// The keystroke sequence to reach the command with this palette title, as
/// space-separated keys: a top-level command's own key (e.g. `p`), or a leaf's
/// full prefix-then-suffix path (e.g. `c c` for "Create commit"). `None` if it
/// has no binding. Lets the `:` palette double as a keymap reference.
pub(crate) fn command_keys(
    keymap: &HashMap<String, String>,
    config: &config::Config,
    title: &str,
) -> Option<String> {
    let Some(cmd) = commands().iter().find(|c| c.title == title) else {
        // Not a built-in: a user `[[command]]`, reached by a `[keymap]` binding
        // or a `[transient.<id>]` injection.
        return user_command_keys(keymap, config, title);
    };
    // A current top-level key — including a leaf bound directly to one via
    // `[keymap]`. Reflects remaps and hides what the user unbound.
    if let Some(key) = current_key(
        keymap,
        cmd.id,
        default_key_for_command(config.keymap_preset, cmd),
    ) {
        return Some(key);
    }
    // Otherwise a leaf reached through its prefix's transient: `<prefix>
    // <suffix>`, with the prefix's *current* key (the suffix is transient-fixed).
    let leaf = cmd.leaf?;
    // Search every transient (via the single `transient_for` source of truth, so
    // adding a leaf under any prefix surfaces its key — no hardcoded list).
    for &prefix_id in TRANSIENT_IDS {
        let Some(t) = transient_for(prefix_id, config.keymap_preset.transient_style()) else {
            continue;
        };
        for group in &t.groups {
            for suffix in &group.suffixes {
                if let Suffix::Action(a) = suffix {
                    if a.command == leaf {
                        let default = commands()
                            .iter()
                            .find(|c| c.id == prefix_id)
                            .and_then(|c| default_key_for_command(config.keymap_preset, c));
                        let prefix_key = current_key(keymap, prefix_id, default)?;
                        return Some(format!("{prefix_key} {}", a.key));
                    }
                }
            }
        }
    }
    None
}

/// The keystroke for a user `[[command]]` (matched by `title`): a direct
/// `[keymap]` binding, else a `[transient.<id>]` injection as `<prefix> <key>`.
/// An injection whose key is shadowed by a built-in suffix is skipped, since
/// `open_transient` drops it (the built-in wins), so it wouldn't actually fire.
pub(crate) fn user_command_keys(
    keymap: &HashMap<String, String>,
    config: &config::Config,
    title: &str,
) -> Option<String> {
    let id = &config.commands.iter().find(|c| c.title == title)?.id;
    if let Some(key) = current_key(keymap, id, None) {
        return Some(key);
    }
    for (tid, suffixes) in &config.transient {
        for (key, spec) in suffixes {
            // Only an action injection runs this command; a switch has no id.
            let config::SuffixKind::Action { id: action_id, .. } = spec.kind() else {
                continue;
            };
            if action_id != id {
                continue;
            }
            if transient_for(tid, config.keymap_preset.transient_style())
                .is_some_and(|t| t.action_for(key).is_some())
            {
                continue; // shadowed by a built-in suffix — the injection is dropped
            }
            let default = commands()
                .iter()
                .find(|c| c.id == tid)
                .and_then(|c| default_key_for_command(config.keymap_preset, c));
            if let Some(prefix_key) = current_key(keymap, tid, default) {
                return Some(format!("{prefix_key} {key}"));
            }
        }
    }
    None
}

/// The plain-text title of a transient group (its text spans joined), for
/// matching a `[transient.<id>]` injection's target section.
pub(crate) fn group_text(g: &Group) -> String {
    g.title
        .iter()
        .filter_map(|s| match s {
            TitleSpan::Text(t) => Some(t.as_str()),
            TitleSpan::Branch(_) => None,
        })
        .collect()
}

/// The built-in transient for a prefix id (the `[transient.<id>]` sections),
/// for resolving an injected suffix's key against its built-ins. The single
/// source of truth for "the transient for this id".
pub(crate) fn transient_for(id: &str, style: transient::KeymapStyle) -> Option<Transient> {
    let rt = RemoteTargets::default();
    Some(match id {
        "commit" => transient::commit_transient(),
        "branch" => transient::branch_transient(style),
        "tag" => transient::tag_transient(style),
        "remote" => transient::remote_transient(style),
        "stash" => transient::stash_transient(),
        "reset" => transient::reset_transient(),
        "rebase" => transient::rebase_transient(&rt),
        "merge" => transient::merge_transient(),
        "ignore" => transient::ignore_transient(),
        "log" => transient::log_transient(),
        "diff" => transient::diff_transient(),
        "push" => transient::push_transient(&rt),
        "pull" => transient::pull_transient(&rt),
        "fetch" => transient::fetch_transient(&rt),
        _ => return None,
    })
}

/// The keystroke currently bound to command `id` in the effective `keymap`,
/// preferring its built-in `default` key when that's still bound to it — so the
/// `?` menu shows remapped keys and hides anything the user unbound.
pub(crate) fn current_key(
    keymap: &HashMap<String, String>,
    id: &str,
    default: Option<&str>,
) -> Option<String> {
    if let Some(def) = default {
        if keymap.get(def).map(String::as_str) == Some(id) {
            return Some(def.to_string());
        }
    }
    keymap
        .iter()
        .filter(|(_, v)| v.as_str() == id)
        .map(|(k, _)| k.clone())
        .min()
}

/// The `?` dispatch menu: a modal command transient (magit's dispatch),
/// generated from the [`commands`] registry (grouped by [`Category`]). Each
/// row shows its *current* key from `keymap` — remaps are reflected, and an
/// unbound command is dropped — and is invoked by that key or a click.
///
/// This menu is the discoverable face of the keymap. The
/// `dispatch_menu_covers_every_command` test cross-checks it against the keys
/// `run_dispatch` actually handles, so a command can't be shown-but-dead or
/// invocable-but-hidden.
pub(crate) fn dispatch_menu(
    keymap: &HashMap<String, String>,
    config: &config::Config,
) -> Transient {
    let group = |cat: Category| Group {
        title: transient::plain_title(cat.title()),
        suffixes: commands()
            .iter()
            .filter(|c| c.category == cat && c.menu)
            .filter_map(|c| {
                // Prefer the preset's default key (e.g. vanilla `g` for
                // refresh), not the registry's evil-collection one.
                let default = default_key_for_command(config.keymap_preset, c);
                let keys = if c.id == "yank" {
                    // Copy has no bare key in evil (`y` is the yank prefix); show
                    // the `yy` family key. Vanilla magit's copy is `C-w`.
                    Some(
                        if matches!(config.keymap_preset, config::KeymapPreset::EvilCollection) {
                            "y y"
                        } else {
                            "ctrl-w"
                        }
                        .to_string(),
                    )
                } else {
                    current_key(keymap, c.id, default)
                };
                keys.map(|keys| {
                    Suffix::Info(transient::Info {
                        keys,
                        description: c.title.to_string(),
                    })
                })
            })
            .collect(),
    };
    // Essential gathers the always-available registry commands plus the `:`
    // palette — itself a meta-affordance (reach any command), not a registry
    // entry, so it's appended here rather than living in `commands()`.
    let mut essential = group(Category::Essential);
    if matches!(config.keymap_preset, config::KeymapPreset::EvilCollection) {
        essential.suffixes.push(Suffix::Info(transient::Info {
            keys: ":".to_string(),
            description: "Command palette".to_string(),
        }));
    }
    let mut menu = Transient {
        title: transient::plain_title("Help"),
        groups: vec![
            group(Category::Commands),
            group(Category::Applying),
            essential,
            group(Category::Application),
        ],
    };
    // User `[[command]]`s that are bound to a key show too, in their configured
    // `section` (default "Commands"), creating that group if it doesn't exist.
    for c in &config.commands {
        let Some(keys) = current_key(keymap, &c.id, None) else {
            continue; // unbound → palette-only, like keyless built-ins
        };
        let info = Suffix::Info(transient::Info {
            keys,
            description: c.title.clone(),
        });
        let section = c.section.as_deref().unwrap_or("Commands");
        match menu.groups.iter_mut().find(|g| group_text(g) == section) {
            Some(g) => g.suffixes.push(info),
            None => menu.groups.push(Group {
                title: transient::plain_title(section),
                suffixes: vec![info],
            }),
        }
    }
    menu
}

pub(crate) fn dispatch_menu_for(view: &StatusView) -> Transient {
    let info = |keys: &str, description: &str| {
        Suffix::Info(transient::Info {
            keys: keys.to_string(),
            description: description.to_string(),
        })
    };
    let group = |title: &str, suffixes: Vec<Suffix>| Group {
        title: transient::plain_title(title),
        suffixes,
    };
    // The copy key differs by preset: evil's `yy` yank family vs magit-mode-map's
    // `C-w` (vanilla magit's `y` is show-refs, so `y`-to-copy would surprise).
    let copy_key = if view.is_evil() { "y y" } else { "ctrl-w" };

    match &view.screen {
        Screen::Commit { view: cv, .. } => Transient {
            title: transient::plain_title("Help"),
            groups: vec![group(
                "Commit detail",
                vec![
                    info(
                        "a",
                        if cv.show_details {
                            "Hide details"
                        } else {
                            "Show details"
                        },
                    ),
                    info("v", "Visual selection"),
                    info(copy_key, "Copy"),
                    info("q", "Back"),
                ],
            )],
        },
        Screen::Diff { .. } => Transient {
            title: transient::plain_title("Help"),
            groups: vec![group(
                "Diff",
                vec![
                    info("v", "Visual selection"),
                    info(copy_key, "Copy"),
                    info("q", "Back"),
                ],
            )],
        },
        Screen::Log(log) => {
            let mut suffixes = vec![info("enter", "Show commit"), info(copy_key, "Copy hash")];
            if matches!(log.purpose, LogPurpose::Browse) {
                let (revert, _reverse) = match view.config.keymap_preset {
                    config::KeymapPreset::EvilCollection => ("_", "-"),
                    config::KeymapPreset::Vanilla => ("V", "v"),
                };
                suffixes.extend([
                    info("A", "Cherry-pick"),
                    info(revert, "Revert"),
                    info("r", "Rebase since"),
                ]);
            }
            suffixes.push(info("q", "Back"));
            Transient {
                title: transient::plain_title("Help"),
                groups: vec![group("Commit at point", suffixes)],
            }
        }
        Screen::GitLog { .. } => Transient {
            title: transient::plain_title("Help"),
            groups: vec![group(
                "Command log",
                vec![info("a", "Toggle queries"), info("q", "Back")],
            )],
        },
        Screen::RebaseTodo(_) => Transient {
            title: transient::plain_title("Help"),
            groups: vec![group(
                "Rebase todo",
                vec![
                    info("enter", "Start rebase"),
                    info("p", "Pick"),
                    info("r", "Reword"),
                    info("e", "Edit"),
                    info("s", "Squash"),
                    info("f", "Fixup"),
                    info("x", "Drop"),
                    info("q", "Cancel"),
                ],
            )],
        },
        _ => {
            let mut menu = dispatch_menu(&view.keymap, &view.config);
            if view
                .rows
                .get(view.selected)
                .and_then(|r| r.target.as_ref())
                .is_none()
            {
                menu.groups
                    .retain(|g| group_text(g) != Category::Applying.title());
            }
            if let Some((_hash, _short, _subject)) = view.point_commit() {
                let (revert, reverse) = match view.config.keymap_preset {
                    config::KeymapPreset::EvilCollection => ("_", "-"),
                    config::KeymapPreset::Vanilla => ("V", "v"),
                };
                menu.groups.insert(
                    0,
                    group(
                        "Commit at point",
                        vec![
                            info("enter", "Show commit"),
                            info(copy_key, "Copy hash"),
                            info("A", "Cherry-pick"),
                            info("a", "Apply changes"),
                            info(revert, "Revert"),
                            info(reverse, "Revert changes"),
                            info("r", "Rebase since"),
                        ],
                    ),
                );
                menu.groups
                    .retain(|g| group_text(g) != Category::Applying.title());
            } else if let Some((_reference, _message)) = view.point_stash() {
                menu.groups.insert(
                    0,
                    group(
                        "Stash at point",
                        vec![
                            info("enter", "Show stash"),
                            info(copy_key, "Copy reference"),
                            info("a", "Apply"),
                            info("A", "Pop"),
                            info(if view.is_evil() { "x" } else { "k" }, "Drop"),
                        ],
                    ),
                );
                menu.groups
                    .retain(|g| group_text(g) != Category::Applying.title());
            }
            menu.groups.retain(|g| !g.suffixes.is_empty());
            menu
        }
    }
}

// --- Command-argument vocabulary shared with the controller ---------------

/// The default ignore pattern for a concrete path at point. Repo-local ignore
/// files get anchored paths (`/foo`) so ignoring a file named `foo` doesn't also
/// ignore every nested `foo`; a subdir `.gitignore` anchors the basename within
/// that subdirectory. This mirrors Magit's `magit-gitignore-read-pattern`,
/// which prefixes the current-file default with `/` for every ignore target.
pub(crate) fn default_ignore_pattern(command: transient::Command, file: Option<&str>) -> String {
    use transient::Command::*;
    match (command, file) {
        (IgnoreSubdir, Some(f)) => Path::new(f)
            .file_name()
            .map(|n| anchored_ignore_path(&n.to_string_lossy()))
            .unwrap_or_default(),
        (IgnoreToplevel | IgnorePrivate | IgnoreGlobal, Some(f)) => anchored_ignore_path(f),
        (_, Some(f)) => f.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn anchored_ignore_path(path: &str) -> String {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}

/// The revision scope for a `git log` invocation.
pub(crate) enum LogScope {
    /// HEAD / the current branch.
    Current,
    /// All refs (`--all`).
    All,
    /// A specific ref.
    Ref(String),
}

/// Assemble a `git log` argument list in the order git requires: flags and
/// options, a commit limit (defaulted when unset), the revision scope, then any
/// pathspecs behind a `--`.
pub(crate) fn build_log_args(
    mut flags: Vec<String>,
    scope: LogScope,
    paths: Vec<String>,
    limit: usize,
) -> Vec<String> {
    if !flags
        .iter()
        .any(|a| a.starts_with("-n") || a.starts_with("--max-count"))
    {
        flags.push(format!("--max-count={limit}"));
    }
    match scope {
        LogScope::Current => flags.push("HEAD".to_string()),
        LogScope::All => flags.push("--all".to_string()),
        LogScope::Ref(r) => flags.push(r),
    }
    if !paths.is_empty() {
        flags.push("--".to_string());
        flags.extend(paths);
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::commands;

    #[test]
    fn aliased_commands_are_in_the_palette() {
        // Aliases only help in the `:` palette, so an alias on a palette-hidden
        // command is dead weight — catch it.
        for c in commands().iter().filter(|c| !c.aliases.is_empty()) {
            assert!(c.palette, "aliased command `{}` isn't in the palette", c.id);
        }
    }

    #[test]
    fn aliases_are_nonempty_and_not_the_title() {
        for c in commands() {
            for a in c.aliases {
                assert!(!a.trim().is_empty(), "`{}` has a blank alias", c.id);
                assert!(
                    !a.eq_ignore_ascii_case(c.title),
                    "`{}` alias `{a}` just repeats its title",
                    c.id
                );
            }
        }
    }
}

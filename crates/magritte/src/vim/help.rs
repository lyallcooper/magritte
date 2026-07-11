//! The `:help` cheat sheet shown over the commit editor in Vim mode.

use crate::*;

/// The `:help` popup: a cheat sheet of the Vim-mode bindings, shown as a
/// dispatch-style transient over the editor (Esc dismisses). Static — the
/// engine's editing keys are fixed; the editor-level commands (commit,
/// cancel, …) take extra `[vim.keymap]` sequences, which aren't listed here.
pub(crate) fn vim_help_menu() -> transient::Transient {
    let info = |keys: &str, description: &str| {
        transient::Suffix::Info(transient::Info {
            keys: keys.to_string(),
            description: description.to_string(),
        })
    };
    let group = |title: &str, suffixes| transient::Group {
        title: transient::plain_title(title),
        suffixes,
    };
    transient::Transient {
        title: transient::plain_title("Vim mode"),
        groups: vec![
            group(
                "Editor",
                vec![
                    info("ZZ · :wq · ,,", "Commit"),
                    info("ZQ · :q · ,k", "Cancel"),
                    info(":q!", "Discard without asking"),
                    info("gq", "Reflow over a motion"),
                ],
            ),
            group(
                "Edit",
                vec![
                    info("d · c · y", "Delete / change / yank"),
                    info("p · P", "Put after / before"),
                    info("> · <", "Indent / dedent"),
                    info("u · ctrl-r", "Undo / redo"),
                    info(".", "Repeat last change"),
                    info("ys · cs · ds", "Surround add / change / delete"),
                ],
            ),
            group(
                "Search & command line",
                vec![
                    info("/ · ?", "Search forward / back"),
                    info("n · N", "Next / previous match"),
                    info(":s/pat/rep/", "Substitute (ranges, g flag)"),
                    info(":N", "Go to line N"),
                ],
            ),
        ],
    }
}

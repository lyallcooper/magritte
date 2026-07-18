//! Vim-mode integration for the commit editor. The pure keystroke→[`Action`]
//! engine lives in `magritte_ui::vim` (re-exported here so `crate::vim::…`
//! paths keep working); `apply` routes gpui keys through it and applies the
//! returned actions, and `help` builds the `:help` cheat sheet.

pub(crate) mod apply;
mod help;

pub(crate) use magritte_ui::vim::*;

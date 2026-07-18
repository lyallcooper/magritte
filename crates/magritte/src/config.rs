//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`), deep-merged with a per-repo
//! `.git/magritte/config.toml` overlay. Carries appearance/font/editor
//! settings, the `[keymap]` and `[transient.*]` overrides, user `[[command]]`s,
//! and the `[status]`/`[fetch]` tables — see docs/config.md for the user-facing
//! reference. Loaded at startup, re-read live on change; in-app saves patch
//! only the fields the Settings screen owns (via `toml_edit`), preserving the
//! user's comments and layout. Also home to the sibling state files: command
//! usage (palette frecency) and saved transient arguments.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use magritte_ui::persist::{atomic_write_text, atomic_write_toml, load_toml_or_default, unix_now};

/// Default theme names for the light and dark slots (our bundled themes; see
/// `BUNDLED_THEMES`).
pub const DEFAULT_LIGHT_THEME: &str = "Selenized Light";
pub const DEFAULT_DARK_THEME: &str = "Selenized Dark";

/// Built-in keymap family. `EvilCollection` is the default because Magritte is
/// keyboard-first and already uses vim-style navigation; `Vanilla` keeps the
/// Magit command keys for users coming from Emacs without evil-collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KeymapPreset {
    // "evil-collection" is accepted for configs written before the rename.
    #[default]
    #[serde(rename = "evil", alias = "evil-collection")]
    EvilCollection,
    Vanilla,
}

impl KeymapPreset {
    pub fn as_str(self) -> &'static str {
        match self {
            KeymapPreset::EvilCollection => "evil",
            KeymapPreset::Vanilla => "vanilla",
        }
    }

    pub fn transient_style(self) -> crate::git_transient::KeymapStyle {
        match self {
            KeymapPreset::EvilCollection => crate::git_transient::KeymapStyle::EvilCollection,
            KeymapPreset::Vanilla => crate::git_transient::KeymapStyle::Vanilla,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "auto" (follow the system), "light", or "dark". Empty = "auto".
    #[serde(skip_serializing_if = "is_default_appearance")]
    pub appearance: String,
    /// Theme used in light mode (registry name). Empty = default.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub light_theme: String,
    /// Theme used in dark mode (registry name). Empty = default.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dark_theme: String,
    /// Monospace font family (code, diffs, tabular columns). Empty = platform
    /// default.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub font: String,
    /// Base font size (px) for the whole UI; rows and the commit editor's
    /// line height scale with it. Clamped to 9-24. Unset = the platform's
    /// standard UI text size (13 on macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<u32>,
    /// Proportional UI font for prose chrome (menus, headings, labels). Empty =
    /// use the monospace `font` everywhere, as before.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ui_font: String,
    /// The app icon variant (macOS Dock/switcher icon) — see
    /// [`crate::app_icon`] for the variants and default. macOS only, and it
    /// sets the running Dock icon, not the bundle's Finder icon.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app_icon: String,
    /// Highlight commit-summary characters past 50 columns in the editor.
    #[serde(skip_serializing_if = "is_true")]
    pub commit_title_ruler: bool,
    /// Auto-hard-wrap the commit body at 72 columns as you type.
    #[serde(skip_serializing_if = "is_true")]
    pub commit_body_wrap: bool,
    /// Modal Vim editing (Normal/Insert/Visual, operators, text objects,
    /// surround) in the in-app commit editor.
    #[serde(skip_serializing_if = "is_false")]
    pub commit_vim_mode: bool,
    /// External GUI editor for "open file" (Return) and the config button.
    /// Either a CLI command (`code -w`, `zed`) or, on macOS, an application
    /// name opened via `open -a` (`Zed`, `Visual Studio Code`). Empty = open in
    /// the OS default app.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub editor: String,
    /// Write commit messages in the external `commit_editor` command (an
    /// interactive `git commit`) instead of Magritte's in-app commit editor.
    #[serde(skip_serializing_if = "is_false")]
    pub commit_in_editor: bool,
    /// Command for writing commit messages in an external editor (used as
    /// `GIT_EDITOR` for an interactive `git commit`), e.g. `zed --wait`,
    /// `code --wait`, or `nvim`. Must block until the message is saved/closed —
    /// the user supplies the appropriate wait flag. Used only when
    /// `commit_in_editor` is set; empty falls back to the in-app editor.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub commit_editor: String,
    /// Keystroke → command-id overrides, applied over the built-in keymap at
    /// startup. The value `"unbound"` removes a default binding. Keystrokes use
    /// the same form the `?` menu shows (e.g. `"K"`, `"g r"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keymap: BTreeMap<String, String>,
    /// Which built-in keymap family to start from before applying `[keymap]`.
    #[serde(default, skip_serializing_if = "is_default_keymap_preset")]
    pub keymap_preset: KeymapPreset,
    /// Extra suffixes to add into a transient, keyed by the transient's command
    /// id (`branch`, `commit`, `push`, …): each inner entry maps a suffix
    /// keystroke to a [`TransientSuffix`] — a command to run, a toggleable git
    /// flag, or a placement-only move of the built-in at that key. Lets users
    /// add e.g. a `b X` → delete-branch action, or a custom switch, inside a
    /// built-in transient. The inner map preserves file order: entries apply
    /// in the order written, so one can place relative to an earlier one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub transient: BTreeMap<String, IndexMap<String, TransientSuffix>>,
    /// Commit-editor Vim mode (`[vim]`): extra key sequences for the
    /// editor-level commands. Only used while `commit_vim_mode` is on.
    #[serde(default, skip_serializing_if = "is_default_vim")]
    pub vim: VimConfig,
    /// How long (ms) after a prefix key is pressed before the which-key popup
    /// of possible continuations appears. The prefix itself waits indefinitely
    /// for the next key; this only delays the help.
    #[serde(
        default = "default_which_key_delay_ms",
        skip_serializing_if = "is_default_which_key_delay_ms"
    )]
    pub which_key_delay_ms: u64,
    /// Re-run `git status` when the window regains focus, so out-of-app changes
    /// show up without a manual refresh. On by default; set false to opt out.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub refresh_on_focus: bool,
    /// Show the nearest tag(s) (a "Tag/Tags" segment) in the title bar. Off by
    /// default; set true to show it.
    #[serde(default, alias = "show_tags", skip_serializing_if = "is_false")]
    pub show_tags_in_title_bar: bool,
    /// Periodically check the public release feed and show a quiet notice when a
    /// newer Magritte is available. On by default; set false to opt out.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub check_for_updates: bool,
    /// Branches considered "published" (magit's `magit-published-branches`):
    /// amending/rewording/rebasing a commit already on one of these warns
    /// before rewriting shared history. A commit counts as on a branch when
    /// it's an ancestor of it; branches absent from the repo are ignored (so
    /// the default names both `origin/main` and `origin/master`). Empty = never
    /// warn.
    #[serde(
        default = "default_published_branches",
        skip_serializing_if = "is_default_published_branches"
    )]
    pub published_branches: Vec<String>,
    /// User-defined commands (`[[command]]`): a shell command surfaced in the
    /// `:` palette and bindable in `[keymap]` by `id`. Skipped when empty so a
    /// saved config doesn't write `command = []` — that's an empty *array*, and
    /// a user later hand-adding a `[[command]]` (array-of-tables) entry would
    /// then hit a TOML type conflict until they deleted the `[]`.
    #[serde(default, rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CustomCommand>,
    /// Status-view section selection and order (`[status]`).
    #[serde(default, skip_serializing_if = "is_default_status")]
    pub status: StatusConfig,
    /// Background auto-fetch (`[fetch]`).
    #[serde(default, skip_serializing_if = "is_default_fetch")]
    pub fetch: FetchConfig,
}

/// Commit-editor Vim mode settings (`[vim]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VimConfig {
    /// Extra key sequences for the editor-level Vim commands (`commit`,
    /// `cancel`, `discard`, `reflow`, `help`), added alongside the built-in
    /// defaults (`ZZ`, `ZQ`, `,q`, `:wq`, …). Sequence steps are normally
    /// space-separated (`"Q enter"`, `"ctrl-x ctrl-c"`); modifier chords use
    /// the global-keymap notation (`"cmd-enter"`). Compact literal sequences
    /// such as `",w"` remain accepted. A sequence whose first key names a
    /// built-in command shadows that key. Merged per entry with a repo overlay,
    /// like `[keymap]`. See docs/config.md.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keymap: BTreeMap<String, String>,
}

/// Background auto-fetch (`[fetch]`). Off by default; when `auto` is on, runs a
/// plain `git fetch` every `interval_minutes` so the unpushed/unpulled counts
/// stay current without a manual fetch. Per-repo overridable like the rest of
/// the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchConfig {
    #[serde(skip_serializing_if = "is_false")]
    pub auto: bool,
    #[serde(skip_serializing_if = "is_default_interval_minutes")]
    pub interval_minutes: u64,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            auto: false,
            interval_minutes: 30,
        }
    }
}

/// The status view's sections and their order (`[status]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusConfig {
    /// Section ids in display order — order is display order, presence includes,
    /// omission hides. Empty falls back to [`DEFAULT_STATUS_SECTIONS`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
    /// How many commits the `recent` section shows.
    #[serde(skip_serializing_if = "is_default_recent_count")]
    pub recent_count: usize,
}

impl Default for StatusConfig {
    fn default() -> Self {
        StatusConfig {
            sections: Vec::new(),
            recent_count: 10,
        }
    }
}

/// The built-in section order, used when `[status].sections` is empty/unset.
pub const DEFAULT_STATUS_SECTIONS: &[&str] = &[
    "untracked",
    "unstaged",
    "staged",
    "stashes",
    "unpulled",
    "unpulled-pushremote",
    "unpushed",
    "unpushed-pushremote",
    "recent",
    // "ignored" is available but off by default.
];

impl StatusConfig {
    /// The effective ordered section ids: the configured list, or the default
    /// when none is set.
    pub fn section_ids(&self) -> Vec<String> {
        if self.sections.is_empty() {
            DEFAULT_STATUS_SECTIONS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            self.sections.clone()
        }
    }
}

/// A user-defined command from a `[[command]]` table: a shell command the
/// palette and keymap can run by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCommand {
    /// Stable id — bound in `[keymap]` and recorded for palette frecency.
    /// Conventionally namespaced (`user.sync`) to avoid clashing with built-ins.
    pub id: String,
    /// Human label shown in the palette. Placeholders (`{branch}`, …) are
    /// expanded for display; one that doesn't resolve stays literal.
    pub title: String,
    /// The shell command to run, e.g. `"git pull --rebase && git push"`. Run via
    /// `sh -c` in the repo root, so it supports `&&`, pipes, and any program —
    /// not just git. The `{file}`, `{commit}`, `{branch}`, `{upstream}`,
    /// `{push-remote}`, `{default-branch}`, and `{default-remote}` placeholders
    /// are substituted (shell-quoted) from the selection and repo at run time.
    pub run: String,
    /// Re-read status after running (default true).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub refresh: bool,
    /// Whether to ask before running. Unset uses the destructive-word scan
    /// (`clean`, `--hard`, `--force`, `--force-with-lease`); `false` runs a
    /// trusted command without asking, `true` forces the prompt on commands
    /// the scan can't see (e.g. a script that deletes things).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
}

/// Where a `[transient.<id>]` entry lands in the menu. `before`/`after` place
/// it next to the suffix invoked by that key (a built-in, or an earlier user
/// entry — magit's `transient-insert-suffix` coordinates); `group` names a
/// section title to append into (created at the end if missing), and is the
/// fallback when the `before`/`after` key isn't in the transient.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    pub group: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl Placement {
    pub const NONE: Placement = Placement {
        group: None,
        before: None,
        after: None,
    };
}

/// A `[transient.<id>]` entry. A bare string is a command id (an action), or —
/// if it starts with `-` — a git flag (a switch). The table forms add a switch
/// description and/or a [`Placement`]; a table with *only* placement keys moves
/// the built-in suffix at that key:
///
/// ```toml
/// [transient.commit]
/// "A" = "commit-amend"                                # action (command id)
/// "-d" = "--depth=1"                                  # switch (bare flag)
/// "-n" = { flag = "--no-verify", description = "Skip hooks" }  # switch + label
/// "-v" = { flag = "--verbose", after = "-s" }         # placed next to a key
/// "X" = { command = "branch-delete", group = "Create" }
/// "F" = { after = "c" }                               # move built-in `F`
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSuffix", into = "RawSuffix")]
pub enum TransientSuffix {
    /// A bare string: a command id, or a `-`-prefixed git flag.
    Bare(String),
    /// A table naming a command to run, optionally placed.
    Action {
        command: String,
        placement: Placement,
    },
    /// A table: a git flag, optionally with a description and placement.
    Switch {
        flag: String,
        description: String,
        placement: Placement,
    },
    /// A placement-only table: move the built-in suffix at this key.
    Move(Placement),
}

/// The on-disk shape of a [`TransientSuffix`] table, validated into the enum:
/// `command` and `flag` are mutually exclusive and pick the kind; neither
/// makes the entry a placement-only move.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawSuffix {
    Bare(String),
    Table(RawSuffixTable),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuffixTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

impl TryFrom<RawSuffix> for TransientSuffix {
    type Error = String;

    fn try_from(raw: RawSuffix) -> Result<Self, String> {
        let t = match raw {
            RawSuffix::Bare(s) => return Ok(TransientSuffix::Bare(s)),
            RawSuffix::Table(t) => t,
        };
        if t.before.is_some() && t.after.is_some() {
            return Err("a transient suffix takes `before` or `after`, not both".into());
        }
        let placement = Placement {
            group: t.group,
            before: t.before,
            after: t.after,
        };
        match (t.command, t.flag) {
            (Some(_), Some(_)) => {
                Err("a transient suffix takes `command` or `flag`, not both".into())
            }
            (Some(command), None) if t.description.is_some() => Err(format!(
                "`description` only applies to a `flag` suffix (command \"{command}\")"
            )),
            (Some(command), None) => Ok(TransientSuffix::Action { command, placement }),
            (None, Some(flag)) => Ok(TransientSuffix::Switch {
                flag,
                description: t.description.unwrap_or_default(),
                placement,
            }),
            (None, None) if placement == Placement::NONE => Err(
                "a transient suffix needs a `command`, a `flag`, or a placement \
                 (`group`/`before`/`after`) to move the built-in at its key"
                    .into(),
            ),
            (None, None) => Ok(TransientSuffix::Move(placement)),
        }
    }
}

impl From<TransientSuffix> for RawSuffix {
    fn from(suffix: TransientSuffix) -> Self {
        let table = |placement: Placement| RawSuffixTable {
            group: placement.group,
            before: placement.before,
            after: placement.after,
            ..RawSuffixTable::default()
        };
        match suffix {
            TransientSuffix::Bare(s) => RawSuffix::Bare(s),
            TransientSuffix::Action { command, placement } => RawSuffix::Table(RawSuffixTable {
                command: Some(command),
                ..table(placement)
            }),
            TransientSuffix::Switch {
                flag,
                description,
                placement,
            } => RawSuffix::Table(RawSuffixTable {
                flag: Some(flag),
                description: (!description.is_empty()).then_some(description),
                ..table(placement)
            }),
            TransientSuffix::Move(placement) => RawSuffix::Table(table(placement)),
        }
    }
}

/// A [`TransientSuffix`] interpreted: an action (command id), a switch (flag +
/// description), or a move of the built-in at its key, each with a
/// [`Placement`]. A bare `-`-prefixed string resolves to a switch.
pub enum SuffixKind<'a> {
    Action {
        id: &'a str,
        placement: &'a Placement,
    },
    Switch {
        flag: &'a str,
        description: &'a str,
        placement: &'a Placement,
    },
    Move(&'a Placement),
}

impl TransientSuffix {
    /// Whether this entry *removes* the built-in suffix at its key — the
    /// keymap-style `"key" = "unbound"` sentinel — rather than adding one.
    pub fn is_unbound(&self) -> bool {
        matches!(self, TransientSuffix::Bare(s) if s == "unbound")
    }

    pub fn kind(&self) -> SuffixKind<'_> {
        match self {
            TransientSuffix::Bare(s) if s.starts_with('-') => SuffixKind::Switch {
                flag: s,
                description: "",
                placement: &Placement::NONE,
            },
            TransientSuffix::Bare(s) => SuffixKind::Action {
                id: s,
                placement: &Placement::NONE,
            },
            TransientSuffix::Action { command, placement } => SuffixKind::Action {
                id: command,
                placement,
            },
            TransientSuffix::Switch {
                flag,
                description,
                placement,
            } => SuffixKind::Switch {
                flag,
                description,
                placement,
            },
            TransientSuffix::Move(placement) => SuffixKind::Move(placement),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_which_key_delay_ms() -> u64 {
    1000
}

fn default_published_branches() -> Vec<String> {
    vec!["origin/main".to_string(), "origin/master".to_string()]
}

// `skip_serializing_if` predicates: a saved config omits keys left at their
// default, so the file stays minimal (and a `command = []` can't break a later
// hand-added `[[command]]`). Each returns true when the field is at its
// default. `save_settings_at` derives its write-or-omit decisions from these
// too (via serde), so they are the only encoding of the defaults.
fn is_default_appearance(s: &str) -> bool {
    // Empty and "auto" both mean "follow the system".
    matches!(s, "" | "auto")
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_default_which_key_delay_ms(n: &u64) -> bool {
    *n == default_which_key_delay_ms()
}
fn is_default_keymap_preset(p: &KeymapPreset) -> bool {
    *p == KeymapPreset::default()
}
fn is_default_recent_count(n: &usize) -> bool {
    *n == StatusConfig::default().recent_count
}
fn is_default_interval_minutes(n: &u64) -> bool {
    *n == FetchConfig::default().interval_minutes
}
fn is_default_status(s: &StatusConfig) -> bool {
    *s == StatusConfig::default()
}
fn is_default_vim(v: &VimConfig) -> bool {
    *v == VimConfig::default()
}
fn is_default_fetch(f: &FetchConfig) -> bool {
    *f == FetchConfig::default()
}
fn is_default_published_branches(v: &[String]) -> bool {
    v == default_published_branches().as_slice()
}

impl Default for Config {
    fn default() -> Self {
        // The commit-editor aids follow the git 50/72 convention out of the box;
        // `#[serde(default)]` also fills these in for configs written before the
        // fields existed, so an upgrade keeps them on.
        Self {
            appearance: String::new(),
            light_theme: String::new(),
            dark_theme: String::new(),
            font: String::new(),
            font_size: None,
            ui_font: String::new(),
            app_icon: String::new(),
            commit_title_ruler: true,
            commit_body_wrap: true,
            commit_vim_mode: false,
            editor: String::new(),
            commit_in_editor: false,
            commit_editor: String::new(),
            keymap: BTreeMap::new(),
            keymap_preset: KeymapPreset::default(),
            transient: BTreeMap::new(),
            vim: VimConfig::default(),
            which_key_delay_ms: default_which_key_delay_ms(),
            refresh_on_focus: true,
            show_tags_in_title_bar: false,
            check_for_updates: true,
            published_branches: default_published_branches(),
            commands: Vec::new(),
            status: StatusConfig::default(),
            fetch: FetchConfig::default(),
        }
    }
}

impl Config {
    pub fn light_theme(&self) -> &str {
        non_empty(&self.light_theme, DEFAULT_LIGHT_THEME)
    }
    pub fn dark_theme(&self) -> &str {
        non_empty(&self.dark_theme, DEFAULT_DARK_THEME)
    }
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

/// Path to the config file, if we can determine a config home.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("magritte").join("config.toml"))
}

/// Load the global config, returning defaults if it's missing or unreadable,
/// plus a warning when a file *exists* yet fails to parse. Equivalent to
/// [`load_merged`] with no repo overlay.
pub fn load_reporting() -> (Config, Option<String>) {
    load_merged(None)
}

/// Resolve the effective config: the global file, with a repo scope's
/// `config.toml` (when present) deep-merged on top — scalars and `[keymap]` /
/// `[transient]` entries the repo sets win, `[[command]]`s concatenate. Same
/// per-file warning semantics as before: an existing-but-unreadable or invalid
/// file is reported and skipped rather than failing the whole load.
pub fn load_merged(repo_config: Option<&Path>) -> (Config, Option<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let global = path()
        .map(|p| read_config_value(&p, &mut warnings))
        .unwrap_or_else(empty_table);
    let mut merged = global.clone();
    let mut overlaid = false;
    if let Some(p) = repo_config {
        if p.exists() {
            let overlay = read_config_value(p, &mut warnings);
            deep_merge(&mut merged, overlay);
            overlaid = true;
        }
    }
    // A type error (serde) fails the whole struct, unlike a semantically-bad
    // value (unknown theme, bad keystroke) which degrades per-field later. When
    // the failure only appears with the repo overlay merged in, fall back to
    // the user's valid global config rather than throwing it all away.
    let config = match merged.try_into() {
        Ok(config) => config,
        Err(e) => {
            warnings.push(format!("Ignoring invalid config: {e}"));
            let global_alone = overlaid.then(|| global.try_into().ok()).flatten();
            global_alone.unwrap_or_default()
        }
    };
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    (config, warning)
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

/// Read a config file as a raw TOML table: an empty table if missing, and a
/// recorded warning if it exists but can't be read or parsed (so the rest of the
/// load proceeds).
fn read_config_value(path: &Path, warnings: &mut Vec<String>) -> toml::Value {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<toml::Value>(&text) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!(
                    "Ignoring invalid config at {}: {e}",
                    path.display()
                ));
                empty_table()
            }
        },
        // A missing file is the normal "use defaults" case; an existing but
        // unreadable one (permissions, etc.) is worth telling the user about.
        Err(e) => {
            if path.exists() {
                warnings.push(format!("Could not read config at {}: {e}", path.display()));
            }
            empty_table()
        }
    }
}

/// Deep-merge `overlay` onto `base` (both TOML tables): tables merge key by key
/// with the overlay winning; the top-level `command` array concatenates with
/// dedup by `id` (so a repo adds or overrides commands); other scalars and
/// arrays are replaced by the overlay. Only the top level treats `command`
/// specially — a nested `command` key (e.g. a `[transient.<id>]` action
/// suffix) is ordinary data and merges like any other value.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, ov) in o {
                if k == "command" {
                    let merged = merge_commands(b.get("command"), ov);
                    b.insert(k, merged);
                } else if let Some(bv) = b.get_mut(&k) {
                    merge_value(bv, ov);
                } else {
                    b.insert(k, ov);
                }
            }
        }
        (slot, o) => *slot = o,
    }
}

/// The recursive arm of [`deep_merge`]: tables merge key by key, everything
/// else is replaced by the overlay.
fn merge_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, ov) in o {
                if let Some(bv) = b.get_mut(&k) {
                    merge_value(bv, ov);
                } else {
                    b.insert(k, ov);
                }
            }
        }
        (slot, o) => *slot = o,
    }
}

/// Concatenate the global and repo `[[command]]` arrays, a repo entry replacing
/// a global one of the same `id` (so a repo adds new commands or overrides
/// existing ones by id, with no spurious duplicate-id warning).
fn merge_commands(base: Option<&toml::Value>, overlay: toml::Value) -> toml::Value {
    let id_of = |c: &toml::Value| c.get("id").and_then(|v| v.as_str()).map(str::to_owned);
    let mut out: Vec<toml::Value> = base
        .and_then(toml::Value::as_array)
        .map(|a| a.to_vec())
        .unwrap_or_default();
    let toml::Value::Array(overlay) = overlay else {
        return toml::Value::Array(out);
    };
    for cmd in overlay {
        if let Some(id) = id_of(&cmd) {
            out.retain(|c| id_of(c).as_deref() != Some(id.as_str()));
        }
        out.push(cmd);
    }
    toml::Value::Array(out)
}

/// Command-usage record for the palette's frecency ranking, persisted next to
/// the config. A single decaying score per command captures both frequency and
/// recency: each use decays the old score by elapsed time, then adds 1, so a
/// command used a lot recently outranks one used more but long ago.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub command: HashMap<String, CommandUse>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CommandUse {
    pub score: f64,
    /// Unix seconds of the last use, for decaying the score on the next one.
    pub last_used: u64,
}

/// The score halves every this-many days of disuse.
const FRECENCY_HALF_LIFE_DAYS: f64 = 30.0;

impl Usage {
    /// The current frecency score for a command id (0 if never used).
    pub fn score(&self, id: &str) -> f64 {
        self.command.get(id).map_or(0.0, |u| u.score)
    }

    /// Record a use now: decay the prior score by how long it's been, then +1.
    pub fn record(&mut self, id: &str) {
        let now = unix_now();
        let entry = self.command.entry(id.to_string()).or_insert(CommandUse {
            score: 0.0,
            last_used: now,
        });
        let days = now.saturating_sub(entry.last_used) as f64 / 86_400.0;
        entry.score = entry.score * 0.5_f64.powf(days / FRECENCY_HALF_LIFE_DAYS) + 1.0;
        entry.last_used = now;
    }
}

/// Path to the usage file (a sibling of the config).
pub fn usage_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("command-usage.toml"))
}

/// Load the persisted command usage, or defaults if missing/unreadable.
pub fn load_usage() -> Usage {
    usage_path()
        .map(|p| load_toml_or_default(&p))
        .unwrap_or_default()
}

/// Persist command usage (atomic). Best-effort.
pub fn save_usage(usage: &Usage) {
    if let Some(path) = usage_path() {
        let _ = atomic_write_toml(&path, usage);
    }
}

/// Saved default argument sets per transient (magit's `transient-save`), keyed
/// by transient command id → the git arguments to pre-apply (`--all`,
/// `--grep=fix`, `-n50`), not the keystrokes that toggle them, so a keybinding
/// remap can't misread a default. Persisted next to the config as
/// `transient-arguments.toml` (e.g. `commit = ["--all", "--signoff"]`).
pub type TransientArguments = BTreeMap<String, Vec<String>>;

pub const TRANSIENT_ARGUMENTS_FILE: &str = "transient-arguments.toml";

/// The magritte settings directory inside a repo's git dir — the repo "scope",
/// a sibling layout to the global config dir (so `config.toml` /
/// `transient-arguments.toml` carry the same formats, just rooted here and
/// overlaid on the global ones). `git_common_dir` is the repo's common git
/// directory, shared across worktrees.
pub fn repo_dir(git_common_dir: &Path) -> PathBuf {
    git_common_dir.join("magritte")
}

/// Path to the global saved transient arguments file (a sibling of the config).
pub fn transient_arguments_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name(TRANSIENT_ARGUMENTS_FILE))
}

pub fn repo_transient_arguments_path(repo_dir: &Path) -> PathBuf {
    repo_dir.join(TRANSIENT_ARGUMENTS_FILE)
}

/// Load the saved transient argument sets from a specific file, or empty if it's
/// missing/unreadable. Used for both scopes (global and a repo's `.git/magritte`).
pub fn load_transient_arguments_at(path: &Path) -> TransientArguments {
    load_toml_or_default(path)
}

/// Persist the saved transient argument sets to a specific file (atomic temp-file
/// + rename), creating its directory as needed. Best-effort.
pub fn save_transient_arguments_at(path: &Path, values: &TransientArguments) {
    let _ = atomic_write_toml(path, values);
}

/// Load the global saved transient argument sets, or empty if missing.
pub fn load_transient_arguments() -> TransientArguments {
    transient_arguments_path()
        .map(|p| load_transient_arguments_at(&p))
        .unwrap_or_default()
}

/// Ensure the global config file exists without formatting or otherwise
/// rewriting an existing file. Used by "Open global config" so merely opening
/// the file doesn't reorder or normalize the user's hand-written TOML.
pub fn ensure_file() -> Option<PathBuf> {
    let path = path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, "");
    }
    Some(path)
}

/// Persist only the top-level fields owned by the Settings screen, preserving
/// the rest of the user's TOML (comments, ordering, custom command tables,
/// transient/keymap sections, etc.). Default-valued keys are omitted per the
/// serde `skip_serializing_if` rules, but unrelated syntax is left untouched.
/// A failure (typically an on-disk file that no longer parses) is returned for
/// the caller to surface — the GUI runs with detached stdio, so stderr is
/// invisible and a silent no-op save would look like it worked.
pub fn save_settings(config: &Config) -> Result<(), String> {
    let Some(path) = path() else {
        return Err("Could not save config: no config directory".to_string());
    };
    save_settings_at(&path, config)
        .map_err(|e| format!("Could not save config ({}): {e}", path.display()))
}

fn save_settings_at(path: &Path, config: &Config) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = if text.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        text.parse::<toml_edit::DocumentMut>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    };

    // Serializing through serde applies the struct's `skip_serializing_if`
    // rules, so a key's presence in this table *is* the is-it-still-default
    // decision — there is no second list of defaults to keep in sync.
    let serialized = toml::Table::try_from(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // The Settings-screen-owned keys, in write order: key, a legacy spelling
    // to migrate away from, and the current value. A `None` value removes the
    // key outright: set_setting's omit only skips writing when the key is
    // absent, and there is no "unset" number to write (font_size).
    type Value = toml_edit::Value;
    let owned: [(&str, Option<&str>, Option<Value>); 17] = [
        ("appearance", None, Some(config.appearance.as_str().into())),
        (
            "light_theme",
            None,
            Some(config.light_theme.as_str().into()),
        ),
        ("dark_theme", None, Some(config.dark_theme.as_str().into())),
        ("font", None, Some(config.font.as_str().into())),
        (
            "font_size",
            None,
            config.font_size.map(|n| Value::from(n as i64)),
        ),
        ("ui_font", None, Some(config.ui_font.as_str().into())),
        ("app_icon", None, Some(config.app_icon.as_str().into())),
        (
            "commit_title_ruler",
            None,
            Some(config.commit_title_ruler.into()),
        ),
        (
            "commit_body_wrap",
            None,
            Some(config.commit_body_wrap.into()),
        ),
        ("commit_vim_mode", None, Some(config.commit_vim_mode.into())),
        ("editor", None, Some(config.editor.as_str().into())),
        (
            "commit_in_editor",
            None,
            Some(config.commit_in_editor.into()),
        ),
        (
            "commit_editor",
            None,
            Some(config.commit_editor.as_str().into()),
        ),
        (
            "keymap_preset",
            None,
            Some(config.keymap_preset.as_str().into()),
        ),
        (
            "refresh_on_focus",
            None,
            Some(config.refresh_on_focus.into()),
        ),
        (
            "show_tags_in_title_bar",
            Some("show_tags"),
            Some(config.show_tags_in_title_bar.into()),
        ),
        (
            "check_for_updates",
            None,
            Some(config.check_for_updates.into()),
        ),
    ];

    for (key, alias, value) in owned {
        if let Some(alias) = alias {
            if !doc.as_table().contains_key(key) {
                if let Some(item) = doc.as_table_mut().remove(alias) {
                    doc[key] = item;
                }
            } else {
                doc.as_table_mut().remove(alias);
            }
        }
        match value {
            Some(value) => set_setting(&mut doc, key, !serialized.contains_key(key), value),
            None => {
                doc.as_table_mut().remove(key);
            }
        }
    }

    atomic_write_text(path, &doc.to_string())
}

fn set_setting(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    omit: bool,
    mut value: toml_edit::Value,
) {
    let old_present = doc.as_table().contains_key(key);
    let old_decor = doc
        .get(key)
        .and_then(|item| item.as_value())
        .map(|value| value.decor().clone());
    if omit && !old_present {
        return;
    }
    if let Some(decor) = old_decor {
        *value.decor_mut() = decor;
    }
    doc[key] = toml_edit::Item::Value(value);
}

#[cfg(test)]
mod tests {
    #[test]
    fn font_size_unset_removes_the_key() {
        let path = std::env::temp_dir().join("magritte-font-size-save-test.toml");
        std::fs::write(&path, "font_size = 16\n").unwrap();

        let mut cfg = Config {
            font_size: Some(15),
            ..Config::default()
        };
        save_settings_at(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("font_size = 15"));

        // Back to the system default: the key is removed, not written as 0
        // (set_setting keeps an existing key, which once wrote a bogus 0).
        cfg.font_size = None;
        save_settings_at(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("font_size"), "saved: {saved}");
        let _ = std::fs::remove_file(path);
    }

    use super::*;

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn example_config_covers_every_value_type() {
        let source = include_str!("../../../docs/config.example.toml");

        // Adopting the example wholesale must not change behavior: every
        // entry is a commented-out demonstration, so the file as written
        // parses to the default configuration.
        let adopted: Config = toml::from_str(source).expect("example should be valid TOML");
        assert_eq!(
            adopted,
            Config::default(),
            "an adopted example must not deviate from the defaults \
             (demonstrations belong in comments)"
        );

        // The coverage checks below run against the uncommented rendering,
        // so the demonstrations are still schema-checked.
        let source = &uncomment_example(source);
        let raw: toml::Table =
            toml::from_str(source).expect("uncommented example should be valid TOML");
        let config: Config = toml::from_str(source)
            .expect("uncommented docs/config.example.toml should match the config schema");
        let (_, warnings) = crate::commands::build_keymap(&config);
        assert!(
            warnings.is_empty(),
            "uncommented example config should not produce warnings: {warnings:?}"
        );

        // These exhaustive patterns are a compile-time schema guard. Adding a
        // field to any config value below requires updating this test and its
        // corresponding key check, so the example cannot silently miss it.
        let Config {
            appearance: _,
            light_theme: _,
            dark_theme: _,
            font: _,
            font_size: _,
            ui_font: _,
            app_icon: _,
            commit_title_ruler: _,
            commit_body_wrap: _,
            commit_vim_mode: _,
            editor: _,
            commit_in_editor: _,
            commit_editor: _,
            keymap: _,
            keymap_preset: _,
            transient: _,
            vim,
            which_key_delay_ms: _,
            refresh_on_focus: _,
            show_tags_in_title_bar: _,
            check_for_updates: _,
            published_branches: _,
            commands,
            status,
            fetch,
        } = &config;
        assert_eq!(
            raw.keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "appearance",
                "app_icon",
                "check_for_updates",
                "command",
                "commit_body_wrap",
                "commit_editor",
                "commit_in_editor",
                "commit_title_ruler",
                "commit_vim_mode",
                "dark_theme",
                "editor",
                "fetch",
                "font",
                "font_size",
                "keymap",
                "keymap_preset",
                "light_theme",
                "published_branches",
                "refresh_on_focus",
                "show_tags_in_title_bar",
                "status",
                "transient",
                "ui_font",
                "vim",
                "which_key_delay_ms",
            ]
            .into_iter()
            .collect(),
            "example should contain every Config field"
        );

        let StatusConfig {
            sections,
            recent_count: _,
        } = status;
        assert_eq!(
            sections
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            crate::SectionId::ALL
                .into_iter()
                .map(crate::SectionId::config_id)
                .collect(),
            "example should list every status section"
        );
        assert_table_keys(&raw, "status", &["recent_count", "sections"]);

        let FetchConfig {
            auto: _,
            interval_minutes: _,
        } = fetch;
        assert_table_keys(&raw, "fetch", &["auto", "interval_minutes"]);

        let VimConfig { keymap: _ } = vim;
        assert_table_keys(
            raw.get("vim").and_then(toml::Value::as_table).unwrap(),
            "keymap",
            &["; w", "Q", "cmd-enter", "g z"],
        );

        assert_eq!(commands.len(), 2);
        for command in commands {
            let CustomCommand {
                id: _,
                title: _,
                run: _,
                refresh: _,
                confirm: _,
            } = command;
        }
        for command in raw["command"].as_array().unwrap() {
            assert_eq!(
                command
                    .as_table()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                ["confirm", "id", "refresh", "run", "title"]
                    .into_iter()
                    .collect(),
                "every command example should contain every CustomCommand field"
            );
        }

        let mut shapes = std::collections::BTreeSet::new();
        let mut placements = std::collections::BTreeSet::new();
        for suffixes in config.transient.values() {
            for suffix in suffixes.values() {
                match suffix {
                    TransientSuffix::Bare(value) if value == "unbound" => {
                        shapes.insert("unbound");
                    }
                    TransientSuffix::Bare(value) if value.starts_with('-') => {
                        shapes.insert("bare-switch");
                    }
                    TransientSuffix::Bare(_) => {
                        shapes.insert("bare-action");
                    }
                    TransientSuffix::Action {
                        command: _,
                        placement,
                    } => {
                        shapes.insert("action-table");
                        record_placement(placement, &mut placements);
                    }
                    TransientSuffix::Switch {
                        flag: _,
                        description,
                        placement,
                    } => {
                        assert!(!description.is_empty());
                        shapes.insert("switch-table");
                        record_placement(placement, &mut placements);
                    }
                    TransientSuffix::Move(placement) => {
                        shapes.insert("move-table");
                        record_placement(placement, &mut placements);
                    }
                }
            }
        }
        assert_eq!(
            shapes,
            [
                "action-table",
                "bare-action",
                "bare-switch",
                "move-table",
                "switch-table",
                "unbound",
            ]
            .into_iter()
            .collect(),
            "example should contain every TransientSuffix shape"
        );
        assert_eq!(
            placements,
            ["after", "before", "group"].into_iter().collect(),
            "example should contain every placement type"
        );
    }

    /// The example with its commented-out demonstrations made live: strips
    /// the `#` off lines that read as TOML (a key = value, a quoted key, a
    /// table header, or an array element/terminator), leaving prose comments
    /// alone. A mis-stripped line fails the caller's parse loudly.
    fn uncomment_example(source: &str) -> String {
        let looks_like_toml = |rest: &str| {
            rest.starts_with('"')
                || rest.starts_with('[')
                || rest.starts_with(']')
                || rest.split_once('=').is_some_and(|(key, _)| {
                    let key = key.trim();
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                })
        };
        source
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                match trimmed.strip_prefix('#') {
                    Some(rest) => {
                        let rest = rest.trim_start();
                        if looks_like_toml(rest) {
                            rest
                        } else {
                            line
                        }
                    }
                    None => line,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_table_keys(table: &toml::Table, key: &str, expected: &[&str]) {
        assert_eq!(
            table[key]
                .as_table()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected.iter().copied().collect(),
            "example table {key} should contain every field"
        );
    }

    fn record_placement<'a>(
        placement: &'a Placement,
        found: &mut std::collections::BTreeSet<&'a str>,
    ) {
        let Placement {
            group,
            before,
            after,
        } = placement;
        if group.is_some() {
            found.insert("group");
        }
        if before.is_some() {
            found.insert("before");
        }
        if after.is_some() {
            found.insert("after");
        }
    }

    #[test]
    fn keymap_preset_parses_current_and_legacy_names() {
        let parse = |s: &str| -> Config { toml::from_str(s).unwrap() };
        assert_eq!(
            parse("keymap_preset = \"evil\"").keymap_preset,
            KeymapPreset::EvilCollection
        );
        assert_eq!(
            parse("keymap_preset = \"vanilla\"").keymap_preset,
            KeymapPreset::Vanilla
        );
        // Accepted for configs written before the rename to "evil".
        assert_eq!(
            parse("keymap_preset = \"evil-collection\"").keymap_preset,
            KeymapPreset::EvilCollection
        );
    }

    #[test]
    fn deep_merge_overlays_scalars_and_tables() {
        let mut base =
            val("dark_theme = \"A\"\nfont = \"F\"\n[keymap]\nx = \"commit\"\nk = \"move-up\"\n");
        let overlay = val("dark_theme = \"B\"\n[keymap]\nx = \"unbound\"\nK = \"branch-delete\"\n");
        deep_merge(&mut base, overlay);
        assert_eq!(base.get("dark_theme").and_then(|v| v.as_str()), Some("B")); // overridden
        assert_eq!(base.get("font").and_then(|v| v.as_str()), Some("F")); // kept
        let km = base.get("keymap").unwrap();
        assert_eq!(km.get("x").and_then(|v| v.as_str()), Some("unbound")); // overridden
        assert_eq!(km.get("k").and_then(|v| v.as_str()), Some("move-up")); // kept from global
        assert_eq!(km.get("K").and_then(|v| v.as_str()), Some("branch-delete"));
        // added by repo
    }

    #[test]
    fn vim_keymap_parses_and_merges_per_entry() {
        let cfg: Config =
            toml::from_str("[vim.keymap]\n\"Q\" = \"cancel\"\n\",w\" = \"commit\"\n").unwrap();
        assert_eq!(cfg.vim.keymap["Q"], "cancel");
        assert_eq!(cfg.vim.keymap[",w"], "commit");
        // Repo overlay: per-entry, like [keymap] — same key wins, new keys add.
        let mut base = val("[vim.keymap]\n\"Q\" = \"cancel\"\n\",w\" = \"commit\"\n");
        let overlay = val("[vim.keymap]\n\"Q\" = \"commit\"\n\"R\" = \"reflow\"\n");
        deep_merge(&mut base, overlay);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.vim.keymap["Q"], "commit");
        assert_eq!(cfg.vim.keymap[",w"], "commit");
        assert_eq!(cfg.vim.keymap["R"], "reflow");
        // Default configs don't serialize an empty [vim] table.
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("[vim"), "empty [vim] omitted:\n{text}");
    }

    #[test]
    fn transient_suffix_parses_all_forms_in_file_order() {
        let cfg: Config = toml::from_str(
            r#"
[transient.commit]
"A" = "commit-amend"
"-d" = "--depth=1"
"-n" = { flag = "--no-verify", description = "Skip hooks", after = "-a" }
"X" = { command = "branch-delete", group = "Create", before = "c" }
"F" = { after = "c" }
"e" = { group = "Extras" }
"#,
        )
        .unwrap();
        let t = &cfg.transient["commit"];
        assert_eq!(t["A"], TransientSuffix::Bare("commit-amend".into()));
        assert!(matches!(&t["-n"], TransientSuffix::Switch { placement, .. }
                if placement.after.as_deref() == Some("-a")));
        assert!(matches!(&t["X"], TransientSuffix::Action { placement, .. }
                if placement.before.as_deref() == Some("c")
                    && placement.group.as_deref() == Some("Create")));
        assert!(matches!(&t["F"], TransientSuffix::Move(p) if p.after.as_deref() == Some("c")));
        assert!(
            matches!(&t["e"], TransientSuffix::Move(p) if p.group.as_deref() == Some("Extras"))
        );
        // Entries keep the order they were written in, not key order.
        let keys: Vec<_> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, ["A", "-d", "-n", "X", "F", "e"]);
    }

    #[test]
    fn transient_suffix_rejects_invalid_tables() {
        let bad = [
            r#""x" = { command = "a", flag = "--b" }"#, // both kinds
            r#""x" = { flag = "--b", before = "a", after = "c" }"#, // both directions
            r#""x" = { command = "a", description = "d" }"#, // description on an action
            r#""x" = {}"#,                              // nothing at all
            r#""x" = { flagg = "--typo" }"#,            // unknown field
        ];
        for entry in bad {
            let src = format!("[transient.commit]\n{entry}\n");
            assert!(
                toml::from_str::<Config>(&src).is_err(),
                "should reject: {entry}"
            );
        }
    }

    #[test]
    fn transient_entries_keep_file_order_through_the_repo_merge() {
        // Non-alphabetical on purpose: file order must survive the
        // `toml::Value` round trip and the deep merge — global order first,
        // a repo override staying in place, repo-new entries appended.
        let mut base = val("[transient.commit]\n\"z\" = \"commit-amend\"\n\"-a\" = \"--all\"\n");
        let overlay =
            val("[transient.commit]\n\"-a\" = \"--allow-empty\"\n\"b\" = \"commit-extend\"\n");
        deep_merge(&mut base, overlay);
        let cfg: Config = base.try_into().unwrap();
        let t = &cfg.transient["commit"];
        let keys: Vec<_> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, ["z", "-a", "b"]);
        assert_eq!(t["-a"], TransientSuffix::Bare("--allow-empty".into()));
    }

    #[test]
    fn transient_suffix_round_trips() {
        let cfg: Config = toml::from_str(
            "[transient.commit]\n\
             \"A\" = \"commit-amend\"\n\
             \"-n\" = { flag = \"--no-verify\", description = \"Skip hooks\", after = \"-a\" }\n\
             \"F\" = { after = \"c\" }\n",
        )
        .unwrap();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg, "round-trips");
    }

    #[test]
    fn settings_save_preserves_unrelated_config_syntax() {
        let path = std::env::temp_dir().join(format!(
            "magritte-settings-save-{}.toml",
            std::process::id()
        ));
        let original = r#"# my config
font = "Mono" # keep this comment
refresh_on_focus = true # explicit default
show_tags_in_title_bar = false # explicit default false

[keymap]
"g x" = "user.sync"

[[command]]
id = "user.sync"
title = "Sync"
run = "git fetch && git push"
"#;
        std::fs::write(&path, original).unwrap();

        let mut cfg: Config = toml::from_str(original).unwrap();
        cfg.font = "JetBrains Mono".to_string();
        cfg.show_tags_in_title_bar = true;
        save_settings_at(&path, &cfg).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# my config"));
        assert!(saved.contains("font = \"JetBrains Mono\" # keep this comment"));
        assert!(saved.contains("refresh_on_focus = true # explicit default"));
        assert!(saved.contains("show_tags_in_title_bar = true # explicit default false"));
        assert!(saved.contains("[keymap]\n\"g x\" = \"user.sync\""));
        assert!(saved.contains("[[command]]\nid = \"user.sync\""));
        assert!(!saved.contains("commit_body_wrap"));
        assert!(!saved.contains("command = []"));
        let saved_value: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(
            saved_value
                .get("show_tags_in_title_bar")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            saved_value
                .get("refresh_on_focus")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(saved_value["command"][0]
            .get("show_tags_in_title_bar")
            .is_none());

        cfg.show_tags_in_title_bar = false;
        cfg.font.clear();
        save_settings_at(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("show_tags_in_title_bar = false # explicit default false"));
        assert!(saved.contains("font = \"\" # keep this comment"));
        assert!(saved.contains("refresh_on_focus = true # explicit default"));
        assert!(saved.contains("[[command]]\nid = \"user.sync\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_save_migrates_old_show_tags_key() {
        let path = std::env::temp_dir().join(format!(
            "magritte-show-tags-migrate-{}.toml",
            std::process::id()
        ));
        let original = "show_tags = false # old spelling\n";
        std::fs::write(&path, original).unwrap();

        let cfg: Config = toml::from_str(original).unwrap();
        assert!(!cfg.show_tags_in_title_bar);
        save_settings_at(&path, &cfg).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("show_tags_in_title_bar = false # old spelling"));
        assert!(!saved.contains("show_tags ="));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_save_round_trips_every_owned_field() {
        let path = std::env::temp_dir().join(format!(
            "magritte-settings-owned-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "").unwrap();

        // Every Settings-owned field non-default: reading the file back must
        // reproduce the config exactly, so a key wrongly treated as default
        // (and left unwritten) fails the equality.
        let cfg = Config {
            appearance: "dark".into(),
            light_theme: "Selenized Light".into(),
            dark_theme: "Selenized Dark".into(),
            font: "Berkeley Mono".into(),
            font_size: Some(15),
            ui_font: "Inter".into(),
            app_icon: "classic".into(),
            commit_title_ruler: false,
            commit_body_wrap: false,
            commit_vim_mode: true,
            editor: "zed".into(),
            commit_in_editor: true,
            commit_editor: "zed --wait".into(),
            keymap_preset: KeymapPreset::Vanilla,
            refresh_on_focus: false,
            show_tags_in_title_bar: true,
            check_for_updates: false,
            ..Config::default()
        };
        save_settings_at(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(toml::from_str::<Config>(&saved).unwrap(), cfg);

        // Saving the same state again does not churn the file.
        save_settings_at(&path, &cfg).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);

        // Everything at its default adds nothing to an empty file — including
        // the settings screen's explicit "auto" appearance.
        std::fs::write(&path, "").unwrap();
        let auto = Config {
            appearance: "auto".into(),
            ..Config::default()
        };
        save_settings_at(&path, &auto).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn status_config_defaults_and_section_ids() {
        let c = StatusConfig::default();
        assert_eq!(c.recent_count, 10);
        assert!(c.sections.is_empty());
        // Empty falls back to the built-in ordered set.
        let default: Vec<String> = DEFAULT_STATUS_SECTIONS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(c.section_ids(), default);
        // A set list is used verbatim (order preserved).
        let c2 = StatusConfig {
            sections: vec!["staged".into(), "recent".into()],
            recent_count: 5,
        };
        assert_eq!(
            c2.section_ids(),
            vec!["staged".to_string(), "recent".to_string()]
        );
    }

    #[test]
    fn status_config_deserializes_with_partial_table() {
        // `[status]` with only recent_count keeps the default recent_count? No —
        // it sets it; the missing `sections` defaults to empty (→ default set).
        let cfg: Config = toml::from_str("[status]\nrecent_count = 3\n").unwrap();
        assert_eq!(cfg.status.recent_count, 3);
        assert!(cfg.status.sections.is_empty());
        // No `[status]` at all → default (recent_count 10).
        let cfg2: Config = toml::from_str("dark_theme = \"X\"\n").unwrap();
        assert_eq!(cfg2.status.recent_count, 10);
    }

    #[test]
    fn deep_merge_concats_commands_dedup_by_id() {
        let mut base = val("[[command]]\nid = \"a\"\ntitle = \"A\"\nrun = \"ga\"\n\
             [[command]]\nid = \"b\"\ntitle = \"B\"\nrun = \"gb\"\n");
        let overlay = val("[[command]]\nid = \"b\"\ntitle = \"B2\"\nrun = \"gb2\"\n\
             [[command]]\nid = \"c\"\ntitle = \"C\"\nrun = \"gc\"\n");
        deep_merge(&mut base, overlay);
        let cmds = base.get("command").and_then(|v| v.as_array()).unwrap();
        let ids: Vec<&str> = cmds
            .iter()
            .map(|c| c.get("id").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]); // b overridden in place, c appended
        let b = cmds
            .iter()
            .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("b"))
            .unwrap();
        assert_eq!(b.get("title").and_then(|v| v.as_str()), Some("B2")); // repo wins
        let parsed: Result<Config, _> = base.try_into();
        assert!(parsed.is_ok(), "merged config deserializes");
    }

    #[test]
    fn deep_merge_leaves_nested_command_keys_alone() {
        // A table-form transient suffix carries a `command` key. When both
        // configs define the same suffix, the overlay must replace it like any
        // nested value — not get mangled by the top-level [[command]] concat.
        let mut base = val("[transient.commit]\n\"A\" = { command = \"commit-amend\" }\n");
        let overlay = val("[transient.commit]\n\"A\" = { command = \"commit-reword\" }\n");
        deep_merge(&mut base, overlay);
        let suffix = &base["transient"]["commit"]["A"];
        assert_eq!(
            suffix.get("command").and_then(|v| v.as_str()),
            Some("commit-reword") // repo wins; still a string, not []
        );
        let parsed: Result<Config, _> = base.try_into();
        assert!(parsed.is_ok(), "merged config deserializes");
    }

    #[test]
    fn empty_commands_are_not_serialized() {
        // A saved default config must not write `command = []`: that's an empty
        // *array*, and a user later hand-adding a `[[command]]` (array-of-tables)
        // entry would then hit a TOML type conflict until they deleted the `[]`.
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(
            !text.contains("command"),
            "default config should not serialize a `command` key:\n{text}"
        );
        // But a config that *has* commands still serializes (and round-trips).
        let mut cfg = Config::default();
        cfg.commands.push(CustomCommand {
            confirm: None,
            id: "amend".into(),
            title: "Amend".into(),
            run: "commit --amend".into(),
            refresh: true,
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("[[command]]"), "non-empty commands serialize");
    }

    #[test]
    fn default_config_omits_default_keys() {
        // Everything at its default → an empty file, so a saved config carries
        // only what the user actually changed.
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(
            text.trim().is_empty(),
            "default config should serialize to nothing, got:\n{text}"
        );

        // Non-default values are written (and round-trip), while their
        // still-default neighbours stay omitted.
        let cfg = Config {
            font: "Berkeley Mono".into(),
            status: StatusConfig {
                recent_count: 25,
                ..StatusConfig::default()
            },
            fetch: FetchConfig {
                interval_minutes: 5,
                ..FetchConfig::default()
            },
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("font = \"Berkeley Mono\""));
        assert!(text.contains("recent_count = 25"));
        assert!(text.contains("interval_minutes = 5"));
        assert!(!text.contains("commit_title_ruler"), "default bool omitted");
        assert!(!text.contains("auto"), "default fetch.auto omitted");
        assert!(!text.contains("sections"), "default empty sections omitted");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg, "round-trips");
    }
}

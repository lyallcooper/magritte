//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`). Currently just the chosen theme and
//! font; written when the settings screen closes, loaded at startup.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default theme names for the light and dark slots (our bundled themes; see
/// `BUNDLED_THEMES`).
pub const DEFAULT_LIGHT_THEME: &str = "Selenized White";
pub const DEFAULT_DARK_THEME: &str = "Selenized Black";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "auto" (follow the system), "light", or "dark". Empty = "auto".
    pub appearance: String,
    /// Theme used in light mode (registry name). Empty = default.
    pub light_theme: String,
    /// Theme used in dark mode (registry name). Empty = default.
    pub dark_theme: String,
    /// Monospace font family (code, diffs, tabular columns). Empty = platform
    /// default.
    pub font: String,
    /// Proportional UI font for prose chrome (menus, headings, labels). Empty =
    /// use the monospace `font` everywhere, as before.
    pub ui_font: String,
    /// Highlight commit-summary characters past 50 columns in the editor.
    pub commit_title_ruler: bool,
    /// Auto-hard-wrap the commit body at 72 columns as you type.
    pub commit_body_wrap: bool,
    /// External GUI editor for "open file" (Return) and the config button.
    /// Either a CLI command (`code -w`, `zed`) or, on macOS, an application
    /// name opened via `open -a` (`Zed`, `Visual Studio Code`). Empty = open in
    /// the OS default app.
    pub editor: String,
    /// Write commit messages in the external `commit_editor` command (an
    /// interactive `git commit`) instead of Magritte's in-app commit editor.
    pub commit_in_editor: bool,
    /// Command for writing commit messages in an external editor (used as
    /// `GIT_EDITOR` for an interactive `git commit`), e.g. `zed --wait`,
    /// `code --wait`, or `nvim`. Must block until the message is saved/closed —
    /// the user supplies the appropriate wait flag. Used only when
    /// `commit_in_editor` is set; empty falls back to the in-app editor.
    pub commit_editor: String,
    /// Keystroke → command-id overrides, applied over the built-in keymap at
    /// startup. The value `"unbound"` removes a default binding. Keystrokes use
    /// the same form the `?` menu shows (e.g. `"K"`, `"g r"`).
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
    /// Extra suffixes to add into a transient, keyed by the transient's command
    /// id (`branch`, `commit`, `push`, …): each inner entry maps a suffix
    /// keystroke to the command id it runs. Lets users add e.g. a `b X` →
    /// delete-branch binding inside the branch transient.
    #[serde(default)]
    pub transient: BTreeMap<String, BTreeMap<String, String>>,
    /// How long (ms) after a prefix key is pressed before the which-key popup
    /// of possible continuations appears. The prefix itself waits indefinitely
    /// for the next key; this only delays the help.
    #[serde(default = "default_which_key_delay_ms")]
    pub which_key_delay_ms: u64,
    /// Re-run `git status` when the window regains focus, so out-of-app changes
    /// show up without a manual refresh. On by default; set false to opt out.
    #[serde(default = "default_true")]
    pub refresh_on_focus: bool,
    /// User-defined commands (`[[command]]`): a shell command surfaced in the
    /// `:` palette and bindable in `[keymap]` by `id`.
    #[serde(default, rename = "command")]
    pub commands: Vec<CustomCommand>,
}

/// A user-defined command from a `[[command]]` table: a shell command the
/// palette and keymap can run by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCommand {
    /// Stable id — bound in `[keymap]` and recorded for palette frecency.
    /// Conventionally namespaced (`user.sync`) to avoid clashing with built-ins.
    pub id: String,
    /// Human label shown in the palette.
    pub title: String,
    /// The shell command to run, e.g. `"git pull --rebase && git push"`. Run via
    /// `sh -c` in the repo root, so it supports `&&`, pipes, and any program —
    /// not just git. The `{file}`, `{commit}`, and `{branch}` placeholders are
    /// substituted (shell-quoted) from the selection at run time.
    pub run: String,
    /// Re-read status after running (default true).
    #[serde(default = "default_true")]
    pub refresh: bool,
}

fn default_true() -> bool {
    true
}

fn default_which_key_delay_ms() -> u64 {
    1000
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
            ui_font: String::new(),
            commit_title_ruler: true,
            commit_body_wrap: true,
            editor: String::new(),
            commit_in_editor: false,
            commit_editor: String::new(),
            keymap: BTreeMap::new(),
            transient: BTreeMap::new(),
            which_key_delay_ms: default_which_key_delay_ms(),
            refresh_on_focus: true,
            commands: Vec::new(),
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

/// Load the config, returning defaults if it's missing or unreadable, plus a
/// warning when the config file *exists* yet fails to parse — so we can tell the
/// user their settings were ignored rather than silently falling back to
/// defaults. A missing/unreadable file is not a warning (defaulting is the
/// intended behavior there).
pub fn load_reporting() -> (Config, Option<String>) {
    let Some(path) = path() else {
        return (Config::default(), None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // A missing file is the normal "use defaults" case; an existing but
        // unreadable one (permissions, etc.) is worth telling the user about.
        Err(e) => {
            let warning = path
                .exists()
                .then(|| format!("Could not read config at {}: {e}", path.display()));
            return (Config::default(), warning);
        }
    };
    match toml::from_str(&text) {
        Ok(config) => (config, None),
        Err(e) => (
            Config::default(),
            Some(format!(
                "Ignoring invalid config at {}: {e}",
                path.display()
            )),
        ),
    }
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Usage {
    /// The current frecency score for a command id (0 if never used).
    pub fn score(&self, id: &str) -> f64 {
        self.command.get(id).map_or(0.0, |u| u.score)
    }

    /// Record a use now: decay the prior score by how long it's been, then +1.
    pub fn record(&mut self, id: &str) {
        let now = now_secs();
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
    let Some(path) = usage_path() else {
        return Usage::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist command usage (atomic temp-file + rename). Best-effort.
pub fn save_usage(usage: &Usage) {
    let Some(path) = usage_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = toml::to_string_pretty(usage) {
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Write the config, creating parent directories as needed. Written
/// atomically (temp file + rename) so an interrupted write can't truncate or
/// corrupt the existing config. Errors are reported but not fatal — settings
/// just won't persist.
pub fn save(config: &Config) {
    let Some(path) = path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("magritte: could not create config dir: {e}");
            return;
        }
    }
    let text = match toml::to_string_pretty(config) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("magritte: could not serialize config: {e}");
            return;
        }
    };
    // Write to a sibling temp file, then atomically rename it into place.
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, text) {
        eprintln!("magritte: could not write config: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("magritte: could not replace config: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

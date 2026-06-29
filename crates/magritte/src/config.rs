//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`). Currently just the chosen theme and
//! font; written when the settings screen closes, loaded at startup.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
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
    /// keystroke to a [`TransientSuffix`] — a command to run, or a toggleable git
    /// flag. Lets users add e.g. a `b X` → delete-branch action, or a custom
    /// switch, inside a built-in transient.
    #[serde(default)]
    pub transient: BTreeMap<String, BTreeMap<String, TransientSuffix>>,
    /// How long (ms) after a prefix key is pressed before the which-key popup
    /// of possible continuations appears. The prefix itself waits indefinitely
    /// for the next key; this only delays the help.
    #[serde(default = "default_which_key_delay_ms")]
    pub which_key_delay_ms: u64,
    /// Re-run `git status` when the window regains focus, so out-of-app changes
    /// show up without a manual refresh. On by default; set false to opt out.
    #[serde(default = "default_true")]
    pub refresh_on_focus: bool,
    /// Show the nearest tag(s) (a "Tag/Tags" segment) in the title bar. On by
    /// default; set false to hide it.
    #[serde(default = "default_true")]
    pub show_tags: bool,
    /// User-defined commands (`[[command]]`): a shell command surfaced in the
    /// `:` palette and bindable in `[keymap]` by `id`.
    #[serde(default, rename = "command")]
    pub commands: Vec<CustomCommand>,
    /// Status-view section selection and order (`[status]`).
    #[serde(default)]
    pub status: StatusConfig,
}

/// The status view's sections and their order (`[status]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusConfig {
    /// Section ids in display order — order is display order, presence includes,
    /// omission hides. Empty falls back to [`DEFAULT_STATUS_SECTIONS`].
    pub sections: Vec<String>,
    /// How many commits the `recent` section shows.
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
            DEFAULT_STATUS_SECTIONS.iter().map(|s| s.to_string()).collect()
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

/// A `[transient.<id>]` injection. A bare string is a command id (an action),
/// or — if it starts with `-` — a git flag (a switch). The table forms add a
/// switch description and/or a target `group` (the section title to place it in;
/// switches default to "Arguments", actions to "Custom"):
///
/// ```toml
/// [transient.commit]
/// "A" = "commit-amend"                                # action (command id)
/// "-d" = "--depth=1"                                  # switch (bare flag)
/// "-n" = { flag = "--no-verify", description = "Skip hooks" }  # switch + label
/// "-v" = { flag = "--verbose", group = "Arguments" }  # placed in a section
/// "X" = { command = "branch-delete", group = "Create" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransientSuffix {
    /// A bare string: a command id, or a `-`-prefixed git flag.
    Bare(String),
    /// A table naming a command to run, optionally in a given section.
    Action {
        command: String,
        #[serde(default)]
        group: Option<String>,
    },
    /// A table: a git flag, optionally with a description and target section.
    Switch {
        flag: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        group: Option<String>,
    },
}

/// A [`TransientSuffix`] interpreted: an action (command id) or a switch (flag +
/// description), each with an optional target `group`. A bare `-`-prefixed
/// string resolves to a switch.
pub enum SuffixKind<'a> {
    Action {
        id: &'a str,
        group: Option<&'a str>,
    },
    Switch {
        flag: &'a str,
        description: &'a str,
        group: Option<&'a str>,
    },
}

impl TransientSuffix {
    pub fn kind(&self) -> SuffixKind<'_> {
        match self {
            TransientSuffix::Bare(s) if s.starts_with('-') => SuffixKind::Switch {
                flag: s,
                description: "",
                group: None,
            },
            TransientSuffix::Bare(s) => SuffixKind::Action {
                id: s,
                group: None,
            },
            TransientSuffix::Action { command, group } => SuffixKind::Action {
                id: command,
                group: group.as_deref(),
            },
            TransientSuffix::Switch {
                flag,
                description,
                group,
            } => SuffixKind::Switch {
                flag,
                description,
                group: group.as_deref(),
            },
        }
    }
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
            show_tags: true,
            commands: Vec::new(),
            status: StatusConfig::default(),
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
    let mut merged = path()
        .map(|p| read_config_value(&p, &mut warnings))
        .unwrap_or_else(empty_table);
    if let Some(p) = repo_config {
        if p.exists() {
            let overlay = read_config_value(p, &mut warnings);
            deep_merge(&mut merged, overlay);
        }
    }
    let parsed: Result<Config, _> = merged.try_into();
    let config = parsed.unwrap_or_else(|e| {
        warnings.push(format!("Ignoring invalid config: {e}"));
        Config::default()
    });
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
                warnings.push(format!("Ignoring invalid config at {}: {e}", path.display()));
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
/// arrays are replaced by the overlay.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, ov) in o {
                if k == "command" {
                    let merged = merge_commands(b.get("command"), ov);
                    b.insert(k, merged);
                } else if let Some(bv) = b.get_mut(&k) {
                    deep_merge(bv, ov);
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

/// Saved default switch sets per transient (magit's `transient-save`), keyed by
/// transient command id → the active switch keys. Persisted next to the config
/// as `transient-switches.toml` (e.g. `commit = ["-a", "-s"]`).
pub type TransientSwitches = BTreeMap<String, Vec<String>>;

/// The magritte settings directory inside a repo's git dir — the repo "scope",
/// a sibling layout to the global config dir (so `config.toml` /
/// `transient-switches.toml` carry the same formats, just rooted here and
/// overlaid on the global ones). `git_common_dir` is the repo's common git
/// directory, shared across worktrees.
pub fn repo_dir(git_common_dir: &Path) -> PathBuf {
    git_common_dir.join("magritte")
}

/// Path to the global saved-transient-switches file (a sibling of the config).
pub fn transient_switches_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("transient-switches.toml"))
}

/// Load the saved transient switch sets from a specific file, or empty if it's
/// missing/unreadable. Used for both scopes (global and a repo's `.git/magritte`).
pub fn load_transient_switches_at(path: &Path) -> TransientSwitches {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist the saved transient switch sets to a specific file (atomic temp-file
/// + rename), creating its directory as needed. Best-effort.
pub fn save_transient_switches_at(path: &Path, values: &TransientSwitches) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = toml::to_string_pretty(values) {
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Load the global saved transient switch sets, or empty if missing.
pub fn load_transient_switches() -> TransientSwitches {
    transient_switches_path()
        .map(|p| load_transient_switches_at(&p))
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn deep_merge_overlays_scalars_and_tables() {
        let mut base = val("dark_theme = \"A\"\nfont = \"F\"\n[keymap]\nx = \"commit\"\nk = \"move-up\"\n");
        let overlay = val("dark_theme = \"B\"\n[keymap]\nx = \"unbound\"\nK = \"branch-delete\"\n");
        deep_merge(&mut base, overlay);
        assert_eq!(base.get("dark_theme").and_then(|v| v.as_str()), Some("B")); // overridden
        assert_eq!(base.get("font").and_then(|v| v.as_str()), Some("F")); // kept
        let km = base.get("keymap").unwrap();
        assert_eq!(km.get("x").and_then(|v| v.as_str()), Some("unbound")); // overridden
        assert_eq!(km.get("k").and_then(|v| v.as_str()), Some("move-up")); // kept from global
        assert_eq!(km.get("K").and_then(|v| v.as_str()), Some("branch-delete")); // added by repo
    }

    #[test]
    fn status_config_defaults_and_section_ids() {
        let c = StatusConfig::default();
        assert_eq!(c.recent_count, 10);
        assert!(c.sections.is_empty());
        // Empty falls back to the built-in ordered set.
        let default: Vec<String> = DEFAULT_STATUS_SECTIONS.iter().map(|s| s.to_string()).collect();
        assert_eq!(c.section_ids(), default);
        // A set list is used verbatim (order preserved).
        let c2 = StatusConfig {
            sections: vec!["staged".into(), "recent".into()],
            recent_count: 5,
        };
        assert_eq!(c2.section_ids(), vec!["staged".to_string(), "recent".to_string()]);
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
        let mut base = val(
            "[[command]]\nid = \"a\"\ntitle = \"A\"\nrun = \"ga\"\n\
             [[command]]\nid = \"b\"\ntitle = \"B\"\nrun = \"gb\"\n",
        );
        let overlay = val(
            "[[command]]\nid = \"b\"\ntitle = \"B2\"\nrun = \"gb2\"\n\
             [[command]]\nid = \"c\"\ntitle = \"C\"\nrun = \"gc\"\n",
        );
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
}

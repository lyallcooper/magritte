//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`). Currently just the chosen theme and
//! font; written when the settings screen closes, loaded at startup.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Default theme names for the light and dark slots (our bundled themes; see
/// `BUNDLED_THEMES`).
pub const DEFAULT_LIGHT_THEME: &str = "GitHub Light";
pub const DEFAULT_DARK_THEME: &str = "GitHub Dark";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "auto" (follow the system), "light", or "dark". Empty = "auto".
    pub appearance: String,
    /// Theme used in light mode (registry name). Empty = default.
    pub light_theme: String,
    /// Theme used in dark mode (registry name). Empty = default.
    pub dark_theme: String,
    /// Monospace font family. Empty = platform default.
    pub font: String,
    /// Highlight commit-summary characters past 50 columns in the editor.
    pub commit_title_ruler: bool,
    /// Auto-hard-wrap the commit body at 72 columns as you type.
    pub commit_body_wrap: bool,
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
            commit_title_ruler: true,
            commit_body_wrap: true,
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

/// Last-modified time of the config file, for change detection. `None` if the
/// file (or config home) doesn't exist.
pub fn mtime() -> Option<SystemTime> {
    let path = path()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Load the config, returning defaults if it's missing or unreadable.
pub fn load() -> Config {
    load_reporting().0
}

/// Like [`load`], but also returns a warning when the config file *exists* yet
/// fails to parse — so we can tell the user their settings were ignored rather
/// than silently falling back to defaults. A missing/unreadable file is not a
/// warning (defaulting is the intended behavior there).
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

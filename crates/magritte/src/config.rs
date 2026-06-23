//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`). Currently just the chosen theme and
//! font; written when the settings screen closes, loaded at startup.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Default theme names for the light and dark slots (gpui-component's built-in
/// neutral themes).
pub const DEFAULT_LIGHT_THEME: &str = "Default Light";
pub const DEFAULT_DARK_THEME: &str = "Default Dark";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Config::default(), None);
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

/// Write the config, creating parent directories as needed. Errors are
/// reported but not fatal — settings just won't persist.
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
    match toml::to_string_pretty(config) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!("magritte: could not write config: {e}");
            }
        }
        Err(e) => eprintln!("magritte: could not serialize config: {e}"),
    }
}

//! Persistent user config: an XDG-style TOML file at
//! `$XDG_CONFIG_HOME/magritte/config.toml` (falling back to
//! `~/.config/magritte/config.toml`). Currently just the chosen theme and
//! font; written when the settings screen closes, loaded at startup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Active theme name (registry name, e.g. "Solarized Light"). Empty = default.
    pub theme: String,
    /// Monospace font family. Empty = platform default.
    pub font: String,
}

/// Path to the config file, if we can determine a config home.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("magritte").join("config.toml"))
}

/// Load the config, returning defaults if it's missing or unreadable.
pub fn load() -> Config {
    let Some(path) = path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
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

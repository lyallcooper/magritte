//! Small typed TOML state files: load helpers and atomic (temp + rename)
//! writes. The app owns the schemas and where the files live; these are the
//! mechanics.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Serialize};

pub fn scoped_path(scope_dir: &Path, file: &str) -> PathBuf {
    scope_dir.join(file)
}

pub fn load_toml_opt<T: DeserializeOwned>(path: &Path) -> Option<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
}

pub fn load_toml_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    load_toml_opt(path).unwrap_or_default()
}

pub fn save_toml<T: Serialize>(path: &Path, value: &T) {
    let _ = atomic_write_toml(path, value);
}

pub fn atomic_write_toml<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write_text(path, &text)
}

pub fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("toml.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Current time as unix seconds; 0 if the clock is somehow before the epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

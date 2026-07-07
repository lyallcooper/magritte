//! App-owned state persistence: small typed TOML files that are not user
//! configuration. Global files live next to `config.toml`; repo/worktree-scoped
//! files live inside the already-discovered `.git/magritte` scope directories.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config;

pub const FOLDS_FILE: &str = "folds.toml";
pub const WINDOW_FILE: &str = "window.toml";

pub fn global_path(file: &str) -> Option<PathBuf> {
    config::path().map(|p| p.with_file_name(file))
}

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

pub(crate) fn atomic_write_toml<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write_text(path, &text)
}

pub(crate) fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
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

/// Per-worktree persisted status fold state. Sections are expanded by default,
/// so only the user's deviations — the collapsed sections, by config id — are
/// stored; everything absent loads expanded.
#[derive(Serialize, Deserialize, Default)]
pub struct FoldState {
    #[serde(default)]
    pub collapsed: Vec<String>,
    /// Whether the commit view's Details section opens expanded — a repo-wide
    /// preference (magit-style: per repo, not per commit). Default collapsed.
    #[serde(default)]
    pub commit_details_expanded: bool,
}

/// Last saved application window placement.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowState {
    #[serde(default)]
    pub mode: WindowMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_uuid: Option<String>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_state_round_trips_and_defaults_empty() {
        let missing = std::env::temp_dir().join("magritte-no-such-folds.toml");
        let _ = std::fs::remove_file(&missing);
        assert!(load_toml_or_default::<FoldState>(&missing)
            .collapsed
            .is_empty());

        let path = std::env::temp_dir().join("magritte-fold-state-test.toml");
        save_toml(
            &path,
            &FoldState {
                collapsed: vec!["staged".into(), "ignored".into()],
                commit_details_expanded: false,
            },
        );
        assert_eq!(
            load_toml_or_default::<FoldState>(&path).collapsed,
            vec!["staged", "ignored"]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn window_state_accepts_old_frame_shape() {
        let state: WindowState = toml::from_str("x = 1\ny = 2\nwidth = 3\nheight = 4\n").unwrap();
        assert_eq!(state.mode, WindowMode::Windowed);
        assert_eq!(state.display_uuid, None);
    }
}

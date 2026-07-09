//! App-owned state persistence: small typed TOML files that are not user
//! configuration. Global files live next to `config.toml`; repo/worktree-scoped
//! files live inside the already-discovered `.git/magritte` scope directories.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config;

pub const FOLDS_FILE: &str = "folds.toml";
pub const WINDOW_FILE: &str = "window.toml";
pub const RECENT_REPOS_FILE: &str = "recent-repos.toml";

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

/// A repository in the recent list and when it was last opened (unix
/// seconds).
#[derive(Serialize, Deserialize, Clone)]
pub struct RecentRepo {
    pub path: PathBuf,
    pub last_used: u64,
}

/// How long a repository stays in the recent list after its last use.
const RECENT_REPOS_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Recently opened repositories, most recent first — the Dock menu's list.
#[derive(Serialize, Deserialize, Default)]
pub struct RecentRepos {
    #[serde(default)]
    pub entries: Vec<RecentRepo>,
    /// Legacy shape (bare paths, no timestamp). Migrated into `entries` on
    /// load and never written back.
    #[serde(default, skip_serializing)]
    pub paths: Vec<PathBuf>,
}

impl RecentRepos {
    /// Load the recent-repos file, migrating any legacy bare paths (each
    /// gets `last_used` set to now, so it gets a fresh 30 days) and pruning
    /// entries past [`RECENT_REPOS_MAX_AGE`]. Callers should use this rather
    /// than `load_toml_or_default` directly, so stale entries drop out on
    /// the next load rather than only on the next save.
    pub fn load(path: &Path) -> RecentRepos {
        let mut recents: RecentRepos = load_toml_or_default(path);
        if !recents.paths.is_empty() {
            let now = unix_now();
            recents
                .entries
                .extend(recents.paths.drain(..).map(|path| RecentRepo {
                    path,
                    last_used: now,
                }));
        }
        let now = unix_now();
        recents
            .entries
            .retain(|e| now.saturating_sub(e.last_used) <= RECENT_REPOS_MAX_AGE.as_secs());
        recents
    }
}

/// Current time as unix seconds; 0 if the clock is somehow before the epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    /// The commit editor's message-box height (px), when the user resized it
    /// away from the default via the drag divider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_editor_height: Option<f32>,
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
                commit_editor_height: None,
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

    #[test]
    fn recent_repos_migrates_legacy_paths() {
        let path = std::env::temp_dir().join("magritte-recent-repos-legacy-test.toml");
        std::fs::write(&path, "paths = [\"/tmp/a\", \"/tmp/b\"]\n").unwrap();

        let recents = RecentRepos::load(&path);
        assert_eq!(recents.entries.len(), 2);
        assert_eq!(recents.entries[0].path, PathBuf::from("/tmp/a"));
        assert_eq!(recents.entries[1].path, PathBuf::from("/tmp/b"));
        assert!(recents.entries.iter().all(|e| e.last_used > 0));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_repos_prunes_entries_older_than_30_days() {
        let path = std::env::temp_dir().join("magritte-recent-repos-prune-test.toml");
        let now = unix_now();
        save_toml(
            &path,
            &RecentRepos {
                entries: vec![
                    RecentRepo {
                        path: PathBuf::from("/tmp/stale"),
                        last_used: now - 31 * 24 * 60 * 60,
                    },
                    RecentRepo {
                        path: PathBuf::from("/tmp/fresh"),
                        last_used: now,
                    },
                ],
                paths: Vec::new(),
            },
        );

        let recents = RecentRepos::load(&path);
        assert_eq!(recents.entries.len(), 1);
        assert_eq!(recents.entries[0].path, PathBuf::from("/tmp/fresh"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_repos_round_trips_new_format() {
        let path = std::env::temp_dir().join("magritte-recent-repos-roundtrip-test.toml");
        let last_used = unix_now();
        save_toml(
            &path,
            &RecentRepos {
                entries: vec![RecentRepo {
                    path: PathBuf::from("/tmp/repo"),
                    last_used,
                }],
                paths: Vec::new(),
            },
        );

        let recents = RecentRepos::load(&path);
        assert_eq!(recents.entries.len(), 1);
        assert_eq!(recents.entries[0].path, PathBuf::from("/tmp/repo"));
        assert_eq!(recents.entries[0].last_used, last_used);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("paths"));

        let _ = std::fs::remove_file(&path);
    }
}

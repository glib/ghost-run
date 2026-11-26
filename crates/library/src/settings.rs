//! The app's own settings, in `$XDG_CONFIG_HOME/ghost-run/settings.toml`.
//! (The Import page keeps its own in `ghost-run/import`.)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Folders the Library lists clips from (searched recursively).
    pub footage_folders: Vec<PathBuf>,
    /// The run database (`timer_db.yaml`).
    pub run_database: Option<PathBuf>,
    /// A Resolve project (`.drp`) new projects start from. Unused until the
    /// Resolve bridge exists.
    pub resolve_template: Option<PathBuf>,
    /// Where overlays are rendered; `None`: an `Overlays` folder beside each clip.
    pub overlay_folder: Option<PathBuf>,
    /// What overlays compare each sector with.
    pub comparison: Comparison,
    /// The Distrobox DaVinci Resolve is installed in (default `davincibox`).
    pub resolve_container: Option<String>,
    /// Past this many GB of local copies of NAS clips, the Library suggests
    /// removing some (default [`LOCAL_COPIES_GB`]).
    pub local_copies_gb: Option<u32>,
}

/// The default for [`AppSettings::local_copies_gb`].
pub const LOCAL_COPIES_GB: u32 = 50;

/// What an overlay compares the run with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// The course's best lap before this run.
    #[default]
    BestLap,
    /// The fastest of each sector before this run, added up.
    Optimal,
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ghost-run")
}

/// `$XDG_CONFIG_HOME/ghost-run` (default `~/.config/ghost-run`).
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME/ghost-run` (default `~/.local/share/ghost-run`).
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

impl AppSettings {
    /// [`Self::local_copies_gb`] in bytes, with its default.
    pub fn local_copies_limit(&self) -> u64 {
        u64::from(self.local_copies_gb.unwrap_or(LOCAL_COPIES_GB)) * 1_000_000_000
    }

    pub fn path() -> PathBuf {
        config_dir().join("settings.toml")
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// A missing file gives the defaults.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Written to a temporary file and renamed, so a crash can't leave half a file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/settings.toml");
        assert_eq!(
            AppSettings::load_from(&path).unwrap(),
            AppSettings::default()
        );
        let settings = AppSettings {
            footage_folders: vec!["/media/a".into(), "/media/b c".into()],
            run_database: Some("/home/x/timer_db.yaml".into()),
            resolve_template: None,
            overlay_folder: Some("/out".into()),
            comparison: Comparison::Optimal,
            resolve_container: Some("resolve".into()),
            local_copies_gb: Some(80),
        };
        settings.save_to(&path).unwrap();
        assert_eq!(AppSettings::load_from(&path).unwrap(), settings);
        // Unknown and missing keys are fine (older or newer files).
        std::fs::write(&path, "footage_folders = [\"/x\"]\nfuture = 1\n").unwrap();
        assert_eq!(
            AppSettings::load_from(&path).unwrap().footage_folders,
            vec![PathBuf::from("/x")]
        );
    }
}

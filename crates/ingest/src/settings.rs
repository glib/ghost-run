use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    /// Stable filesystem type, mount source, and filesystem root. Never mount options/credentials.
    pub expected_mount: String,
    pub shared: bool,
}
impl Destination {
    pub fn select(path: PathBuf, require_cifs: bool) -> Result<Self> {
        let path = fs::canonicalize(path)
            .context("Location unavailable. Select the current mounted folder.")?;
        let mount = crate::storage::mount_for(&path)?;
        ensure!(
            !matches!(
                mount.fs_type.as_str(),
                "nfs" | "nfs4" | "9p" | "ceph" | "afs"
            ),
            "This network filesystem is unsupported. Select local storage or an OS-mounted CIFS share."
        );
        ensure!(
            !mount.fs_type.starts_with("fuse"),
            "GVfs/FUSE is unsupported. Select an OS-mounted CIFS folder."
        );
        let shared = mount.fs_type == "cifs" || mount.fs_type == "smb3";
        ensure!(
            !require_cifs || shared,
            "Archive must be on an OS-mounted CIFS share. Mount the share first, then select its folder."
        );
        Ok(Self {
            path,
            expected_mount: mount.stable(),
            shared,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub source: PathBuf,
    pub archive: Option<Destination>,
    pub editing: Option<Destination>,
    pub high_contrast: bool,
    pub text_scale: f64,
    pub workflow: Workflow,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workflow {
    #[default]
    Ingest,
    Convert,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            source: PathBuf::new(),
            archive: None,
            editing: None,
            high_contrast: false,
            text_scale: 1.0,
            workflow: Workflow::Ingest,
        }
    }
}
/// Import's settings: `~/.config/ghost-run/import` (beside the app's own
/// `settings.toml`, which is a different file).
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("ghost-run/import")
}
/// Import's job journal and lock: `~/.local/share/ghost-run/import`.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("ghost-run/import")
}

/// Moves settings and the journal from where the separate resolve-ingest app
/// kept them (`.../resolve-ingest`), once: only when the old folder exists and
/// the new one doesn't. Call before anything opens either folder.
pub fn move_from_resolve_ingest() -> Result<()> {
    for (var, fallback, new) in [
        ("XDG_CONFIG_HOME", ".config", config_dir()),
        ("XDG_DATA_HOME", ".local/share", data_dir()),
    ] {
        let old = xdg(var, fallback).join("resolve-ingest");
        move_dir(&old, &new)
            .with_context(|| format!("couldn't move {} to {}", old.display(), new.display()))?;
    }
    Ok(())
}

fn move_dir(old: &Path, new: &Path) -> Result<()> {
    if !old.is_dir() || new.exists() {
        return Ok(());
    }
    fs::create_dir_all(new.parent().context("no parent folder")?)?;
    // Same filesystem in practice (both under the home folder): a rename.
    if fs::rename(old, new).is_ok() {
        return Ok(());
    }
    let tmp = new.with_extension("moving");
    let _ = fs::remove_dir_all(&tmp);
    copy_dir(old, &tmp)?;
    fs::rename(&tmp, new)?;
    fs::remove_dir_all(old)?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback)
        })
}
impl Settings {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("settings.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let s: Self = toml::from_str(&fs::read_to_string(path)?)?;
        ensure!(s.version == 1, "Unsupported settings version {}", s.version);
        ensure!(
            s.text_scale.is_finite() && (1.0..=2.0).contains(&s.text_scale),
            "Text scale must be between 1 and 2"
        );
        Ok(s)
    }
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("settings-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(tmp, dir.join("settings.toml"))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_the_old_folder_once() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("resolve-ingest");
        let new = home.path().join("ghost-run/import");
        fs::create_dir_all(old.join("sub")).unwrap();
        fs::write(old.join("settings.toml"), "version = 1").unwrap();
        fs::write(old.join("sub/jobs.sqlite3"), "db").unwrap();

        move_dir(&old, &new).unwrap();
        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(new.join("settings.toml")).unwrap(),
            "version = 1"
        );
        assert_eq!(
            fs::read_to_string(new.join("sub/jobs.sqlite3")).unwrap(),
            "db"
        );

        // An old folder reappearing never replaces the new one.
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("settings.toml"), "version = 9").unwrap();
        move_dir(&old, &new).unwrap();
        assert!(old.exists());
        assert_eq!(
            fs::read_to_string(new.join("settings.toml")).unwrap(),
            "version = 1"
        );

        // Nothing to move: nothing happens.
        move_dir(&home.path().join("absent"), &home.path().join("x")).unwrap();
        assert!(!home.path().join("x").exists());
    }

    #[test]
    fn copies_when_it_cannot_rename() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("old");
        fs::create_dir_all(old.join("a")).unwrap();
        fs::write(old.join("a/f"), "x").unwrap();
        let to = home.path().join("copy");
        copy_dir(&old, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join("a/f")).unwrap(), "x");
    }
}

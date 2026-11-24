//! The library database: one row per clip found in the footage folders.
//! It holds nothing that can't be found again by rescanning, so it can be
//! deleted at any time (`$XDG_DATA_HOME/ghost-run/library.sqlite3`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use crate::probe::MediaInfo;

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub path: PathBuf,
    pub size: u64,
    /// Modification time, ns since the Unix epoch: with `size`, says whether
    /// the file changed since it was probed.
    pub modified_ns: i64,
    /// When it was recorded, by the camera's clock (`YYYY-MM-DD HH:MM:SS`).
    pub recorded: Option<String>,
    /// Where `recorded` came from: `filename`, `metadata` or `modified`.
    pub recorded_source: String,
    /// `None` when ffprobe couldn't read it (`probe_error` says why).
    pub info: Option<MediaInfo>,
    pub probe_error: Option<String>,
    pub proxy: ProxyState,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ProxyState {
    #[default]
    Missing,
    /// Checked against the clip: same frame count, rate and timecode.
    Ready(PathBuf),
    Failed(String),
}

impl Clip {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub struct Library {
    db: Connection,
}

const VERSION: u32 = 1;

impl Library {
    pub fn open_default() -> Result<Self> {
        let dir = crate::settings::data_dir();
        std::fs::create_dir_all(&dir)?;
        Self::open(&dir.join("library.sqlite3"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "{} is from a newer Ghost Run; delete it to rebuild the library",
            path.display()
        );
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS clips (
               path TEXT PRIMARY KEY,
               size INTEGER NOT NULL,
               modified_ns INTEGER NOT NULL,
               recorded TEXT,
               recorded_source TEXT NOT NULL,
               info TEXT,
               probe_error TEXT,
               proxy_state TEXT NOT NULL DEFAULT '',
               proxy_path TEXT,
               proxy_error TEXT
             );",
        )?;
        db.pragma_update(None, "user_version", VERSION)?;
        Ok(Self { db })
    }

    /// Every clip, newest recording first.
    pub fn clips(&self) -> Result<Vec<Clip>> {
        let mut stmt = self.db.prepare(
            "SELECT path, size, modified_ns, recorded, recorded_source, info, probe_error,
                    proxy_state, proxy_path, proxy_error
             FROM clips ORDER BY recorded IS NULL, recorded DESC, path",
        )?;
        let rows = stmt.query_map([], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, path: &Path) -> Result<Option<Clip>> {
        Ok(self
            .db
            .query_row(
                "SELECT path, size, modified_ns, recorded, recorded_source, info, probe_error,
                        proxy_state, proxy_path, proxy_error
                 FROM clips WHERE path = ?1",
                [path_text(path)],
                row,
            )
            .optional()?)
    }

    pub fn upsert(&self, clip: &Clip) -> Result<()> {
        let (state, proxy_path, proxy_error) = match &clip.proxy {
            ProxyState::Missing => ("", None, None),
            ProxyState::Ready(p) => ("ready", Some(path_text(p)), None),
            ProxyState::Failed(e) => ("failed", None, Some(e.clone())),
        };
        self.db.execute(
            "INSERT OR REPLACE INTO clips VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                path_text(&clip.path),
                clip.size as i64,
                clip.modified_ns,
                clip.recorded,
                clip.recorded_source,
                clip.info.as_ref().map(serde_json::to_string).transpose()?,
                clip.probe_error,
                state,
                proxy_path,
                proxy_error
            ],
        )?;
        Ok(())
    }

    pub fn set_proxy(&self, path: &Path, proxy: ProxyState) -> Result<()> {
        if let Some(mut clip) = self.get(path)? {
            clip.proxy = proxy;
            self.upsert(&clip)?;
        }
        Ok(())
    }

    /// Removes clips that aren't in `keep`; returns how many went.
    pub fn retain(&self, keep: &std::collections::HashSet<PathBuf>) -> Result<usize> {
        let gone: Vec<PathBuf> = self
            .clips()?
            .into_iter()
            .map(|c| c.path)
            .filter(|p| !keep.contains(p))
            .collect();
        for path in &gone {
            self.db
                .execute("DELETE FROM clips WHERE path = ?1", [path_text(path)])?;
        }
        Ok(gone.len())
    }
}

/// Paths are stored as text; the scanner skips any that aren't UTF-8.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Clip> {
    let info: Option<String> = r.get(5)?;
    let state: String = r.get(7)?;
    let proxy_path: Option<String> = r.get(8)?;
    let proxy_error: Option<String> = r.get(9)?;
    Ok(Clip {
        path: PathBuf::from(r.get::<_, String>(0)?),
        size: r.get::<_, i64>(1)? as u64,
        modified_ns: r.get(2)?,
        recorded: r.get(3)?,
        recorded_source: r.get(4)?,
        // A row written by a later version with a different shape is just reprobed.
        info: info.and_then(|i| serde_json::from_str(&i).ok()),
        probe_error: r.get(6)?,
        proxy: match (state.as_str(), proxy_path) {
            ("ready", Some(p)) => ProxyState::Ready(PathBuf::from(p)),
            ("failed", _) => ProxyState::Failed(proxy_error.unwrap_or_default()),
            _ => ProxyState::Missing,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(path: &str, recorded: Option<&str>) -> Clip {
        Clip {
            path: path.into(),
            size: 10,
            modified_ns: 1,
            recorded: recorded.map(str::to_owned),
            recorded_source: "filename".into(),
            info: None,
            probe_error: Some("no video stream".into()),
            proxy: ProxyState::Missing,
        }
    }

    #[test]
    fn stores_lists_newest_first_and_forgets_missing_clips() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        lib.upsert(&clip("/v/a.mp4", Some("2026-08-01 10:00:00")))
            .unwrap();
        lib.upsert(&clip("/v/b.mp4", Some("2026-08-02 10:00:00")))
            .unwrap();
        lib.upsert(&clip("/v/c.mp4", None)).unwrap();
        let names: Vec<String> = lib.clips().unwrap().iter().map(Clip::name).collect();
        assert_eq!(names, ["b.mp4", "a.mp4", "c.mp4"]);

        lib.set_proxy(
            Path::new("/v/a.mp4"),
            ProxyState::Ready("/v/Proxy/a.mov".into()),
        )
        .unwrap();
        lib.set_proxy(Path::new("/v/b.mp4"), ProxyState::Failed("boom".into()))
            .unwrap();
        let again = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let a = again.get(Path::new("/v/a.mp4")).unwrap().unwrap();
        assert_eq!(a.proxy, ProxyState::Ready("/v/Proxy/a.mov".into()));
        assert_eq!(a.probe_error.as_deref(), Some("no video stream"));
        let b = again.get(Path::new("/v/b.mp4")).unwrap().unwrap();
        assert_eq!(b.proxy, ProxyState::Failed("boom".into()));

        let keep = [PathBuf::from("/v/a.mp4")].into_iter().collect();
        assert_eq!(again.retain(&keep).unwrap(), 2);
        assert_eq!(again.clips().unwrap().len(), 1);
    }

    #[test]
    fn refuses_a_newer_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.sqlite3");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(Library::open(&path).is_err());
    }
}

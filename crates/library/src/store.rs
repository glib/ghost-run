//! The library database: one row per clip found in the footage folders.
//! It holds nothing that can't be found again by rescanning, so it can be
//! deleted at any time (`$XDG_DATA_HOME/ghost-run/library.sqlite3`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use crate::probe::MediaInfo;

#[derive(Debug, Clone, PartialEq, Default)]
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
    /// The proxy: for an original on the NAS, its local copy's proxy.
    pub proxy: ProxyState,
    /// For an original on the NAS: its local editing copy, while there is one
    /// ([`crate::footage`]).
    pub local: Option<PathBuf>,
    /// For an original on the NAS: the catalog's preview, once made
    /// ([`crate::preview`]).
    pub preview: Option<PathBuf>,
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
    /// The file to read frames from: the proxy (every frame a key frame),
    /// else the catalog's preview, else the local copy, else the clip.
    pub fn frames_from(&self) -> PathBuf {
        if let ProxyState::Ready(proxy) = &self.proxy {
            return proxy.clone();
        }
        self.preview
            .clone()
            .or_else(|| self.local.clone())
            .unwrap_or_else(|| self.path.clone())
    }

    /// The file to edit: the local copy if there is one, else the clip.
    pub fn editable(&self) -> &Path {
        self.local.as_deref().unwrap_or(&self.path)
    }

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

/// 1: clips. 2: marker drafts. 3: course references. 4: rendered overlays.
/// 5: course target times. 6: clips' local copy and preview.
const VERSION: u32 = 6;

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
            "{} is from a newer Ghost Run: update Ghost Run to open it",
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
             );
             CREATE TABLE IF NOT EXISTS drafts (
               path TEXT PRIMARY KEY,
               course_id TEXT NOT NULL,
               frames TEXT NOT NULL,
               confidence TEXT NOT NULL,
               source TEXT NOT NULL,
               updated_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS course_targets (
               course_id TEXT PRIMARY KEY,
               seconds REAL NOT NULL
             );
             CREATE TABLE IF NOT EXISTS course_references (
               course_id TEXT PRIMARY KEY,
               run_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS renders (
               run_id TEXT PRIMARY KEY,
               clip TEXT NOT NULL,
               overlay TEXT NOT NULL,
               first_frame INTEGER NOT NULL,
               timecode TEXT NOT NULL,
               fingerprint TEXT NOT NULL,
               rendered_ms INTEGER NOT NULL
             );",
        )?;
        let has_local: bool = db
            .prepare("SELECT 1 FROM pragma_table_info('clips') WHERE name = 'local'")?
            .exists([])?;
        if !has_local {
            db.execute_batch(
                "ALTER TABLE clips ADD COLUMN local TEXT;
                 ALTER TABLE clips ADD COLUMN preview TEXT;",
            )?;
        }
        db.pragma_update(None, "user_version", VERSION)?;
        Ok(Self { db })
    }

    /// Every clip, newest recording first.
    pub fn clips(&self) -> Result<Vec<Clip>> {
        let mut stmt = self.db.prepare(
            "SELECT path, size, modified_ns, recorded, recorded_source, info, probe_error,
                    proxy_state, proxy_path, proxy_error, local, preview
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
                        proxy_state, proxy_path, proxy_error, local, preview
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
            "INSERT OR REPLACE INTO clips (path, size, modified_ns, recorded, recorded_source,
               info, probe_error, proxy_state, proxy_path, proxy_error, local, preview)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                proxy_error,
                clip.local.as_deref().map(path_text),
                clip.preview.as_deref().map(path_text)
            ],
        )?;
        Ok(())
    }

    pub fn set_preview(&self, path: &Path, preview: Option<PathBuf>) -> Result<()> {
        if let Some(mut clip) = self.get(path)? {
            clip.preview = preview;
            self.upsert(&clip)?;
        }
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

    /// Writes a consistent copy of the whole library to `path`, which must
    /// not exist yet (SQLite's `VACUUM INTO`).
    pub fn copy_to(&self, path: &Path) -> Result<()> {
        self.db
            .execute("VACUUM INTO ?1", [path_text(path)])
            .with_context(|| format!("copying the library to {}", path.display()))?;
        Ok(())
    }
}

/// A run's rendered overlay: where it is and where it lines up with its clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub run_id: String,
    pub clip: PathBuf,
    pub overlay: PathBuf,
    /// The clip frame the overlay's first frame goes over.
    pub first_frame: i64,
    /// The overlay's starting timecode (the clip's at `first_frame`).
    pub timecode: String,
    /// The run's markers when rendered (its fingerprint): if they've changed
    /// since, the overlay is out of date.
    pub fingerprint: String,
    pub rendered_ms: i64,
}

impl Library {
    pub fn rendered(&self, run_id: &str) -> Result<Option<Rendered>> {
        Ok(self
            .db
            .query_row(
                "SELECT run_id, clip, overlay, first_frame, timecode, fingerprint, rendered_ms
                 FROM renders WHERE run_id = ?1",
                [run_id],
                rendered_row,
            )
            .optional()?)
    }

    /// Every rendered overlay, by run id.
    pub fn renders(&self) -> Result<std::collections::HashMap<String, Rendered>> {
        let mut stmt = self.db.prepare(
            "SELECT run_id, clip, overlay, first_frame, timecode, fingerprint, rendered_ms
             FROM renders",
        )?;
        let rows = stmt.query_map([], rendered_row)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|r| (r.run_id.clone(), r))
            .collect())
    }

    pub fn set_rendered(&self, r: &Rendered) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO renders VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                r.run_id,
                path_text(&r.clip),
                path_text(&r.overlay),
                r.first_frame,
                r.timecode,
                r.fingerprint,
                r.rendered_ms
            ],
        )?;
        Ok(())
    }
}

fn rendered_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Rendered> {
    Ok(Rendered {
        run_id: r.get(0)?,
        clip: PathBuf::from(r.get::<_, String>(1)?),
        overlay: PathBuf::from(r.get::<_, String>(2)?),
        first_frame: r.get(3)?,
        timecode: r.get(4)?,
        fingerprint: r.get(5)?,
        rendered_ms: r.get(6)?,
    })
}

/// Markers placed on a clip but not yet committed as a run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Draft {
    pub course_id: String,
    /// Marker name → frame.
    pub frames: std::collections::BTreeMap<String, i64>,
    /// Marker name → auto-markers' confidence (hand-placed markers have none).
    pub confidence: std::collections::BTreeMap<String, f64>,
    /// `hand` or `auto`.
    pub source: String,
}

impl Library {
    pub fn draft(&self, clip: &Path) -> Result<Option<Draft>> {
        let row = self
            .db
            .query_row(
                "SELECT course_id, frames, confidence, source FROM drafts WHERE path = ?1",
                [path_text(clip)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(course_id, frames, confidence, source)| Draft {
            course_id,
            frames: serde_json::from_str(&frames).unwrap_or_default(),
            confidence: serde_json::from_str(&confidence).unwrap_or_default(),
            source,
        }))
    }

    pub fn set_draft(&self, clip: &Path, draft: &Draft) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO drafts VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                path_text(clip),
                draft.course_id,
                serde_json::to_string(&draft.frames)?,
                serde_json::to_string(&draft.confidence)?,
                draft.source,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(())
    }

    pub fn clear_draft(&self, clip: &Path) -> Result<()> {
        self.db
            .execute("DELETE FROM drafts WHERE path = ?1", [path_text(clip)])?;
        Ok(())
    }

    /// The run chosen as `course_id`'s reference for auto-markers, if one was.
    pub fn reference(&self, course_id: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT run_id FROM course_references WHERE course_id = ?1",
                [course_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_reference(&self, course_id: &str, run_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO course_references VALUES (?1, ?2)",
            [course_id, run_id],
        )?;
        Ok(())
    }

    /// The lap time Liam is chasing on a course, if one is set (Tracks).
    pub fn target(&self, course_id: &str) -> Result<Option<f64>> {
        Ok(self
            .db
            .query_row(
                "SELECT seconds FROM course_targets WHERE course_id = ?1",
                [course_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Sets (or with `None` clears) a course's target lap time.
    pub fn set_target(&self, course_id: &str, seconds: Option<f64>) -> Result<()> {
        match seconds {
            Some(s) => self.db.execute(
                "INSERT OR REPLACE INTO course_targets VALUES (?1, ?2)",
                params![course_id, s],
            )?,
            None => self.db.execute(
                "DELETE FROM course_targets WHERE course_id = ?1",
                [course_id],
            )?,
        };
        Ok(())
    }

    /// Clips with a draft, so the Library can show them.
    /// Every clip's draft.
    pub fn drafts(&self) -> Result<std::collections::HashMap<PathBuf, Draft>> {
        let mut stmt = self
            .db
            .prepare("SELECT path, course_id, frames, confidence, source FROM drafts")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Draft {
                    course_id: r.get(1)?,
                    frames: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                    confidence: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    source: r.get(4)?,
                },
            ))
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(p, d)| (PathBuf::from(p), d))
            .collect())
    }
}

impl Draft {
    /// The auto-placed markers worth checking by eye (confidence under
    /// [`crate::auto::CHECK_BELOW`]).
    pub fn to_check(&self) -> usize {
        self.confidence
            .values()
            .filter(|c| **c < crate::auto::CHECK_BELOW)
            .count()
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
        local: r.get::<_, Option<String>>(10)?.map(PathBuf::from),
        preview: r.get::<_, Option<String>>(11)?.map(PathBuf::from),
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
            ..Clip::default()
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
    fn keeps_drafts_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let clip = Path::new("/v/a.mp4");
        assert_eq!(lib.draft(clip).unwrap(), None);
        let draft = Draft {
            course_id: "c".into(),
            frames: [("Start".to_owned(), 10), ("Finish".to_owned(), 99)].into(),
            confidence: [("Start".to_owned(), 0.8)].into(),
            source: "auto".into(),
        };
        lib.set_draft(clip, &draft).unwrap();
        assert_eq!(lib.draft(clip).unwrap(), Some(draft.clone()));
        assert_eq!(lib.drafts().unwrap().get(clip), Some(&draft));
        assert_eq!(draft.to_check(), 0);
        let unsure = Draft {
            confidence: [("Start".to_owned(), 0.8), ("Finish".to_owned(), 0.1)].into(),
            ..draft.clone()
        };
        assert_eq!(unsure.to_check(), 1);
        lib.clear_draft(clip).unwrap();
        assert_eq!(lib.draft(clip).unwrap(), None);
    }

    #[test]
    fn remembers_course_references() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        assert_eq!(lib.reference("c").unwrap(), None);
        lib.set_reference("c", "run_1").unwrap();
        lib.set_reference("c", "run_2").unwrap();
        assert_eq!(lib.reference("c").unwrap().as_deref(), Some("run_2"));
    }

    #[test]
    fn remembers_and_clears_course_targets() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        assert_eq!(lib.target("c").unwrap(), None);
        lib.set_target("c", Some(151.5)).unwrap();
        assert_eq!(lib.target("c").unwrap(), Some(151.5));
        lib.set_target("c", None).unwrap();
        assert_eq!(lib.target("c").unwrap(), None);
    }

    #[test]
    fn remembers_rendered_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        assert_eq!(lib.rendered("r1").unwrap(), None);
        let r = Rendered {
            run_id: "r1".into(),
            clip: "/v/a.mp4".into(),
            overlay: "/v/Overlays/a.overlay.mov".into(),
            first_frame: 30,
            timecode: "01:00:01:00".into(),
            fingerprint: "a.mp4:abc".into(),
            rendered_ms: 5,
        };
        lib.set_rendered(&r).unwrap();
        assert_eq!(lib.rendered("r1").unwrap(), Some(r.clone()));
        assert_eq!(lib.renders().unwrap()["r1"], r);
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

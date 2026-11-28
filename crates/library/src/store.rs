//! The library database (`$XDG_DATA_HOME/ghost-run/library.sqlite3`): one row
//! per clip found in the footage folders, which a rescan finds again, and
//! what only Liam can say about them, which it can't: marker drafts, course
//! references and targets, [`crate::organise`]'s tags, flags,
//! collections and ride names, and [`crate::videos`]. It's copied to the NAS daily
//! ([`crate::catalog_backup`]).
//!
//! What belongs to this machine alone is in a second file beside it
//! (`library.machine.sqlite3`): each clip's local copy, preview and proxy,
//! and the overlays rendered here. That keeps the catalog itself the same
//! for every machine, so it can be shared between them (#69).
//!
//! A clip under the NAS footage root is stored by its path in the root
//! (`2026/2026-11-24/DJI/DJI_0001.MP4`), so machines that mount the NAS in
//! different places agree on it; any other clip by its full path, with the
//! name of the machine it's on ([`crate::machine`]), and only that machine
//! lists it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::db::{Db, Row};
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
    /// The catalog.
    pub(crate) db: Db,
    /// What only this machine has.
    pub(crate) local: Db,
    /// The NAS footage root, which clips under it are stored relative to.
    nas: Option<PathBuf>,
    /// This machine's name ([`crate::machine::name`]).
    machine: String,
}

/// 1: clips. 2: marker drafts. 3: course references. 4: rendered overlays.
/// 5: course target times. 6: clips' local copy and preview. 7: tags, flags,
/// collections and ride names. 8: videos, their clips and usage. 9: cards
/// in videos. 10: features and captions. 11: Strava. 12: this machine's
/// part moved to its own file; clips under the footage root stored by
/// their path in it, others with their machine's name.
const VERSION: u32 = 12;

/// The machine file's: 1, clips' local copy, preview and proxy, and renders.
const MACHINE_VERSION: u32 = 1;

impl Library {
    /// The library in the data folder, with Import's archive folder as the
    /// footage root.
    pub fn open_default() -> Result<Self> {
        let dir = crate::settings::data_dir();
        std::fs::create_dir_all(&dir)?;
        let nas = ingest::settings::Settings::load()
            .ok()
            .and_then(|s| s.archive)
            .map(|a| a.path);
        Self::open_in(&dir.join("library.sqlite3"), nas)
    }

    /// A library with no footage root: every clip stored by its full path.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_in(path, None)
    }

    /// A library whose clips under `nas` are stored by their path in it. Its
    /// machine file is beside it (`<name>.machine.sqlite3`).
    pub fn open_in(path: &Path, nas: Option<PathBuf>) -> Result<Self> {
        Self::open_with(path, &machine_path(path), nas, crate::machine::name())
    }

    /// A library with its machine file at `machine_file`, as machine `machine`.
    pub(crate) fn open_with(
        path: &Path,
        machine_file: &Path,
        nas: Option<PathBuf>,
        machine: String,
    ) -> Result<Self> {
        let db = Db::sqlite(path)?;
        let conn = db.sqlite_conn();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "{} is from a newer Ghost Run: update Ghost Run to open it",
            path.display()
        );
        let local = Db::sqlite(machine_file)?;
        create_machine_tables(&local)?;
        if version < 12 {
            create_tables_before_12(conn)?;
            Self::create_organise_tables(conn)?;
            Self::create_video_tables(conn)?;
            Self::create_feature_tables(conn)?;
            Self::create_strava_tables(conn)?;
            move_machine_part(&db, &local, &machine)?;
        }
        let library = Self {
            db,
            local,
            nas,
            machine,
        };
        if version < 12 {
            library.rekey()?;
        }
        Ok(library)
    }

    /// The text a clip's path is stored as: its path in the footage root, or
    /// the whole path.
    pub(crate) fn stored(&self, path: &Path) -> String {
        if let Some(root) = &self.nas
            && let Ok(inside) = path.strip_prefix(root)
            && !inside.as_os_str().is_empty()
        {
            return path_text(inside);
        }
        path_text(path)
    }

    /// The path a stored clip path stands for on this machine.
    pub(crate) fn unstored(&self, text: &str) -> PathBuf {
        let path = Path::new(text);
        match &self.nas {
            Some(root) if path.is_relative() => root.join(path),
            _ => path.to_owned(),
        }
    }

    /// Whose a stored clip is: everyone's (`""`) under the footage root, else
    /// this machine's.
    fn owner(&self, stored: &str) -> &str {
        if Path::new(stored).is_relative() {
            ""
        } else {
            &self.machine
        }
    }

    /// Stores clips under the footage root by their path in it, where they
    /// were stored whole (from before schema 12, or before the root was set);
    /// returns how many changed. A row already there under the short path
    /// wins over its whole-path twin.
    pub fn rekey(&self) -> Result<usize> {
        let Some(root) = &self.nas else {
            return Ok(0);
        };
        let mut prefix = path_text(root);
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let n = prefix.chars().count() as i64;
        let rest = n + 1;
        let mut changed = 0;
        let tables: [(&Db, &str, &str); 4] = [
            (&self.db, "clips", ", machine = ''"),
            (&self.db, "drafts", ""),
            (&self.db, "video_items", ""),
            (&self.local, "clip_state", ""),
        ];
        for (db, table, extra) in tables {
            db.transaction(|| {
                if table != "video_items" {
                    db.execute(
                        &format!(
                            "DELETE FROM {table} WHERE substr(path, 1, $1) = $2
                               AND substr(path, $3) IN (SELECT path FROM {table})"
                        ),
                        &[&n, &prefix, &rest],
                    )?;
                }
                changed += db.execute(
                    &format!(
                        "UPDATE {table} SET path = substr(path, $3){extra}
                         WHERE substr(path, 1, $1) = $2"
                    ),
                    &[&n, &prefix, &rest],
                )?;
                Ok(())
            })?;
        }
        Ok(changed)
    }

    /// Every clip on the NAS or on this machine, newest recording first.
    pub fn clips(&self) -> Result<Vec<Clip>> {
        let mut states = self.states()?;
        self.db
            .query(
                "SELECT path, size, modified_ns, recorded, recorded_source, info, probe_error
                 FROM clips WHERE machine = '' OR machine = $1
                 ORDER BY recorded IS NULL, recorded DESC, path",
                &[&self.machine],
            )?
            .iter()
            .map(|r| {
                let stored: String = r.get(0)?;
                let state = states.remove(&stored);
                self.clip_row(r, state)
            })
            .collect()
    }

    pub fn get(&self, path: &Path) -> Result<Option<Clip>> {
        let stored = self.stored(path);
        let Some(row) = self.db.query_opt(
            "SELECT path, size, modified_ns, recorded, recorded_source, info, probe_error
             FROM clips WHERE path = $1",
            &[&stored],
        )?
        else {
            return Ok(None);
        };
        let state = self
            .local
            .query_opt(
                "SELECT path, proxy_state, proxy_path, proxy_error, local, preview
                 FROM clip_state WHERE path = $1",
                &[&stored],
            )?
            .map(|r| state_row(&r))
            .transpose()?
            .map(|(_, s)| s);
        self.clip_row(&row, state).map(Some)
    }

    pub fn upsert(&self, clip: &Clip) -> Result<()> {
        let stored = self.stored(&clip.path);
        let info = clip.info.as_ref().map(serde_json::to_string).transpose()?;
        // Unchanged rows aren't written again: a shared catalog sees every
        // machine's scans.
        self.db.execute(
            "INSERT INTO clips (path, size, modified_ns, recorded, recorded_source, info,
               probe_error, machine)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (path) DO UPDATE SET size = excluded.size,
               modified_ns = excluded.modified_ns, recorded = excluded.recorded,
               recorded_source = excluded.recorded_source, info = excluded.info,
               probe_error = excluded.probe_error, machine = excluded.machine
             WHERE clips.size IS DISTINCT FROM excluded.size
               OR clips.modified_ns IS DISTINCT FROM excluded.modified_ns
               OR clips.recorded IS DISTINCT FROM excluded.recorded
               OR clips.recorded_source IS DISTINCT FROM excluded.recorded_source
               OR clips.info IS DISTINCT FROM excluded.info
               OR clips.probe_error IS DISTINCT FROM excluded.probe_error
               OR clips.machine IS DISTINCT FROM excluded.machine",
            &[
                &stored,
                &clip.size,
                &clip.modified_ns,
                &clip.recorded,
                &clip.recorded_source,
                &info,
                &clip.probe_error,
                &self.owner(&stored),
            ],
        )?;
        self.save_state(&stored, clip)
    }

    /// Saves this machine's part of a clip.
    fn save_state(&self, stored: &str, clip: &Clip) -> Result<()> {
        let (state, proxy_path, proxy_error) = match &clip.proxy {
            ProxyState::Missing => ("", None, None),
            ProxyState::Ready(p) => ("ready", Some(path_text(p)), None),
            ProxyState::Failed(e) => ("failed", None, Some(e.clone())),
        };
        self.local.execute(
            "INSERT INTO clip_state (path, proxy_state, proxy_path, proxy_error, local, preview)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (path) DO UPDATE SET proxy_state = excluded.proxy_state,
               proxy_path = excluded.proxy_path, proxy_error = excluded.proxy_error,
               local = excluded.local, preview = excluded.preview",
            &[
                &stored,
                &state,
                &proxy_path,
                &proxy_error,
                &clip.local.as_deref().map(path_text),
                &clip.preview.as_deref().map(path_text),
            ],
        )?;
        Ok(())
    }

    pub fn set_preview(&self, path: &Path, preview: Option<PathBuf>) -> Result<()> {
        if let Some(mut clip) = self.get(path)? {
            clip.preview = preview;
            self.save_state(&self.stored(path), &clip)?;
        }
        Ok(())
    }

    pub fn set_proxy(&self, path: &Path, proxy: ProxyState) -> Result<()> {
        if let Some(mut clip) = self.get(path)? {
            clip.proxy = proxy;
            self.save_state(&self.stored(path), &clip)?;
        }
        Ok(())
    }

    /// Removes clips that aren't in `keep` (of those [`Self::clips`] lists);
    /// returns how many went.
    pub fn retain(&self, keep: &HashSet<PathBuf>) -> Result<usize> {
        let gone: Vec<String> = self
            .clips()?
            .into_iter()
            .filter(|c| !keep.contains(&c.path))
            .map(|c| self.stored(&c.path))
            .collect();
        self.db.transaction(|| {
            for stored in &gone {
                self.db
                    .execute("DELETE FROM clips WHERE path = $1", &[stored])?;
            }
            Ok(())
        })?;
        self.local.transaction(|| {
            for stored in &gone {
                self.local
                    .execute("DELETE FROM clip_state WHERE path = $1", &[stored])?;
            }
            Ok(())
        })?;
        Ok(gone.len())
    }

    /// Writes a consistent copy of the whole library to `path`, which must
    /// not exist yet (SQLite's `VACUUM INTO`).
    pub fn copy_to(&self, path: &Path) -> Result<()> {
        self.db
            .sqlite_conn()
            .execute("VACUUM INTO ?1", [path_text(path)])
            .with_context(|| format!("copying the library to {}", path.display()))?;
        Ok(())
    }

    /// This machine's part of every clip, by stored path.
    fn states(&self) -> Result<HashMap<String, State>> {
        self.local
            .query(
                "SELECT path, proxy_state, proxy_path, proxy_error, local, preview
                 FROM clip_state",
                &[],
            )?
            .iter()
            .map(state_row)
            .collect()
    }

    fn clip_row(&self, r: &Row, state: Option<State>) -> Result<Clip> {
        let info: Option<String> = r.get(5)?;
        let state = state.unwrap_or_default();
        Ok(Clip {
            path: self.unstored(&r.get::<String>(0)?),
            size: r.get(1)?,
            modified_ns: r.get(2)?,
            recorded: r.get(3)?,
            recorded_source: r.get(4)?,
            // A row written by a later version with a different shape is just reprobed.
            info: info.and_then(|i| serde_json::from_str(&i).ok()),
            probe_error: r.get(6)?,
            proxy: state.proxy,
            local: state.local,
            preview: state.preview,
        })
    }
}

/// This machine's part of a clip.
#[derive(Default)]
struct State {
    proxy: ProxyState,
    local: Option<PathBuf>,
    preview: Option<PathBuf>,
}

fn state_row(r: &Row) -> Result<(String, State)> {
    let state: String = r.get(1)?;
    let proxy_path: Option<String> = r.get(2)?;
    let proxy_error: Option<String> = r.get(3)?;
    Ok((
        r.get(0)?,
        State {
            proxy: match (state.as_str(), proxy_path) {
                ("ready", Some(p)) => ProxyState::Ready(PathBuf::from(p)),
                ("failed", _) => ProxyState::Failed(proxy_error.unwrap_or_default()),
                _ => ProxyState::Missing,
            },
            local: r.get::<Option<String>>(4)?.map(PathBuf::from),
            preview: r.get::<Option<String>>(5)?.map(PathBuf::from),
        },
    ))
}

/// The machine file beside a library: `library.sqlite3` →
/// `library.machine.sqlite3`.
pub fn machine_path(library: &Path) -> PathBuf {
    library.with_extension("machine.sqlite3")
}

fn create_machine_tables(local: &Db) -> Result<()> {
    let conn = local.sqlite_conn();
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        version <= MACHINE_VERSION,
        "this machine's library file is from a newer Ghost Run: update Ghost Run to open it"
    );
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clip_state (
           path TEXT PRIMARY KEY,
           proxy_state TEXT NOT NULL DEFAULT '',
           proxy_path TEXT,
           proxy_error TEXT,
           local TEXT,
           preview TEXT
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
    conn.pragma_update(None, "user_version", MACHINE_VERSION)?;
    Ok(())
}

/// Schema 11's own tables, as later ones changed them.
fn create_tables_before_12(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS clips (
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
    Ok(())
}

/// Schema 12: clips' local copy, preview and proxy and the renders go to the
/// machine file (copied first, so an interrupted move is just done again),
/// and every clip is this machine's until [`Library::rekey`] finds it's
/// under the footage root.
fn move_machine_part(db: &Db, local: &Db, machine: &str) -> Result<()> {
    let states = db.query(
        "SELECT path, proxy_state, proxy_path, proxy_error, local, preview FROM clips",
        &[],
    )?;
    let renders = db.query(
        "SELECT run_id, clip, overlay, first_frame, timecode, fingerprint, rendered_ms
         FROM renders",
        &[],
    )?;
    local.transaction(|| {
        for r in &states {
            let v: Vec<crate::db::Value> = (0..6).map(|i| r.get(i)).collect::<Result<_>>()?;
            local.execute(
                "INSERT INTO clip_state VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (path) DO NOTHING",
                &[&v[0], &v[1], &v[2], &v[3], &v[4], &v[5]],
            )?;
        }
        for r in &renders {
            let v: Vec<crate::db::Value> = (0..7).map(|i| r.get(i)).collect::<Result<_>>()?;
            local.execute(
                "INSERT INTO renders VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (run_id) DO NOTHING",
                &[&v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6]],
            )?;
        }
        Ok(())
    })?;
    db.transaction(|| {
        db.sqlite_conn().execute_batch(
            "ALTER TABLE clips DROP COLUMN proxy_state;
             ALTER TABLE clips DROP COLUMN proxy_path;
             ALTER TABLE clips DROP COLUMN proxy_error;
             ALTER TABLE clips DROP COLUMN local;
             ALTER TABLE clips DROP COLUMN preview;
             ALTER TABLE clips ADD COLUMN machine TEXT NOT NULL DEFAULT '';
             DROP TABLE renders;",
        )?;
        db.execute("UPDATE clips SET machine = $1", &[&machine])?;
        db.sqlite_conn()
            .pragma_update(None, "user_version", VERSION)?;
        Ok(())
    })
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

/// Overlays are files on this machine, so they're kept in its own file.
impl Library {
    pub fn rendered(&self, run_id: &str) -> Result<Option<Rendered>> {
        self.local
            .query_opt(
                "SELECT run_id, clip, overlay, first_frame, timecode, fingerprint, rendered_ms
                 FROM renders WHERE run_id = $1",
                &[&run_id],
            )?
            .map(|r| rendered_row(&r))
            .transpose()
    }

    /// Every rendered overlay, by run id.
    pub fn renders(&self) -> Result<HashMap<String, Rendered>> {
        self.local
            .query(
                "SELECT run_id, clip, overlay, first_frame, timecode, fingerprint, rendered_ms
                 FROM renders",
                &[],
            )?
            .iter()
            .map(|r| rendered_row(r).map(|r| (r.run_id.clone(), r)))
            .collect()
    }

    pub fn set_rendered(&self, r: &Rendered) -> Result<()> {
        self.local.execute(
            "INSERT INTO renders (run_id, clip, overlay, first_frame, timecode, fingerprint,
               rendered_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (run_id) DO UPDATE SET clip = excluded.clip,
               overlay = excluded.overlay, first_frame = excluded.first_frame,
               timecode = excluded.timecode, fingerprint = excluded.fingerprint,
               rendered_ms = excluded.rendered_ms",
            &[
                &r.run_id,
                &path_text(&r.clip),
                &path_text(&r.overlay),
                &r.first_frame,
                &r.timecode,
                &r.fingerprint,
                &r.rendered_ms,
            ],
        )?;
        Ok(())
    }
}

fn rendered_row(r: &Row) -> Result<Rendered> {
    Ok(Rendered {
        run_id: r.get(0)?,
        clip: PathBuf::from(r.get::<String>(1)?),
        overlay: PathBuf::from(r.get::<String>(2)?),
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
        self.db
            .query_opt(
                "SELECT course_id, frames, confidence, source FROM drafts WHERE path = $1",
                &[&self.stored(clip)],
            )?
            .map(|r| draft_row(&r, 0))
            .transpose()
    }

    pub fn set_draft(&self, clip: &Path, draft: &Draft) -> Result<()> {
        self.db.execute(
            "INSERT INTO drafts (path, course_id, frames, confidence, source, updated_ms)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (path) DO UPDATE SET course_id = excluded.course_id,
               frames = excluded.frames, confidence = excluded.confidence,
               source = excluded.source, updated_ms = excluded.updated_ms",
            &[
                &self.stored(clip),
                &draft.course_id,
                &serde_json::to_string(&draft.frames)?,
                &serde_json::to_string(&draft.confidence)?,
                &draft.source,
                &chrono::Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub fn clear_draft(&self, clip: &Path) -> Result<()> {
        self.db
            .execute("DELETE FROM drafts WHERE path = $1", &[&self.stored(clip)])?;
        Ok(())
    }

    /// The run chosen as `course_id`'s reference for auto-markers, if one was.
    pub fn reference(&self, course_id: &str) -> Result<Option<String>> {
        self.db
            .query_opt(
                "SELECT run_id FROM course_references WHERE course_id = $1",
                &[&course_id],
            )?
            .map(|r| r.get(0))
            .transpose()
    }

    pub fn set_reference(&self, course_id: &str, run_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO course_references (course_id, run_id) VALUES ($1, $2)
             ON CONFLICT (course_id) DO UPDATE SET run_id = excluded.run_id",
            &[&course_id, &run_id],
        )?;
        Ok(())
    }

    /// The lap time Liam is chasing on a course, if one is set (Tracks).
    pub fn target(&self, course_id: &str) -> Result<Option<f64>> {
        self.db
            .query_opt(
                "SELECT seconds FROM course_targets WHERE course_id = $1",
                &[&course_id],
            )?
            .map(|r| r.get(0))
            .transpose()
    }

    /// Sets (or with `None` clears) a course's target lap time.
    pub fn set_target(&self, course_id: &str, seconds: Option<f64>) -> Result<()> {
        match seconds {
            Some(s) => self.db.execute(
                "INSERT INTO course_targets (course_id, seconds) VALUES ($1, $2)
                 ON CONFLICT (course_id) DO UPDATE SET seconds = excluded.seconds",
                &[&course_id, &s],
            )?,
            None => self.db.execute(
                "DELETE FROM course_targets WHERE course_id = $1",
                &[&course_id],
            )?,
        };
        Ok(())
    }

    /// Every clip's draft.
    pub fn drafts(&self) -> Result<HashMap<PathBuf, Draft>> {
        self.db
            .query(
                "SELECT path, course_id, frames, confidence, source FROM drafts",
                &[],
            )?
            .iter()
            .map(|r| Ok((self.unstored(&r.get::<String>(0)?), draft_row(r, 1)?)))
            .collect()
    }
}

/// A draft from its row's columns `from`.. (course, frames, confidence, source).
fn draft_row(r: &Row, from: usize) -> Result<Draft> {
    Ok(Draft {
        course_id: r.get(from)?,
        frames: serde_json::from_str(&r.get::<String>(from + 1)?).unwrap_or_default(),
        confidence: serde_json::from_str(&r.get::<String>(from + 2)?).unwrap_or_default(),
        source: r.get(from + 3)?,
    })
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
pub(crate) fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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
        assert_eq!(
            again.clips().unwrap()[1].proxy,
            ProxyState::Ready("/v/Proxy/a.mov".into())
        );

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
        lib.set_draft(clip, &unsure).unwrap();
        assert_eq!(lib.draft(clip).unwrap(), Some(unsure));
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
        lib.set_target("c", Some(150.0)).unwrap();
        assert_eq!(lib.target("c").unwrap(), Some(150.0));
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
        let newer = Rendered {
            rendered_ms: 6,
            ..r.clone()
        };
        lib.set_rendered(&newer).unwrap();
        assert_eq!(lib.rendered("r1").unwrap(), Some(newer.clone()));
        assert_eq!(lib.renders().unwrap()["r1"], newer);
    }

    #[test]
    fn refuses_a_newer_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.sqlite3");
        rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(Library::open(&path).is_err());
    }

    /// A schema 11 library: one clip with a proxy, local copy and preview
    /// under the NAS at /nas, one on this machine's disk, a draft and a render.
    fn schema_11(path: &Path) {
        let db = rusqlite::Connection::open(path).unwrap();
        create_tables_before_12(&db).unwrap();
        db.execute_batch(
            "INSERT INTO clips VALUES ('/nas/2026/a.mp4', 10, 1, '2026-08-01 10:00:00',
               'filename', NULL, 'x', 'ready', '/edit/2026/Proxy/a.mov', NULL,
               '/edit/2026/a.mp4.mov', '/cat/a.mp4.10.preview.mov');
             INSERT INTO clips VALUES ('/disk/b.mp4', 20, 2, NULL, 'modified', NULL, 'x',
               'failed', NULL, 'boom', NULL, NULL);
             INSERT INTO drafts VALUES ('/nas/2026/a.mp4', 'c', '{\"Start\":3}', '{}',
               'hand', 1);
             INSERT INTO renders VALUES ('r1', '/edit/2026/a.mp4.mov', '/o/a.overlay.mov',
               0, '01:00:00:00', 'f', 1);
             PRAGMA user_version = 11;",
        )
        .unwrap();
    }

    #[test]
    fn schema_11_moves_this_machines_part_out_and_rekeys_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        schema_11(&path);
        let lib = Library::open_in(&path, Some("/nas".into())).unwrap();
        assert!(dir.path().join("library.machine.sqlite3").is_file());
        let a = lib.get(Path::new("/nas/2026/a.mp4")).unwrap().unwrap();
        assert_eq!(a.proxy, ProxyState::Ready("/edit/2026/Proxy/a.mov".into()));
        assert_eq!(a.local, Some("/edit/2026/a.mp4.mov".into()));
        assert_eq!(a.preview, Some("/cat/a.mp4.10.preview.mov".into()));
        assert_eq!(
            lib.rendered("r1").unwrap().unwrap().overlay,
            Path::new("/o/a.overlay.mov")
        );

        assert_eq!(lib.rekey().unwrap(), 0, "rekeyed when opened");
        let stored: Vec<(String, String)> = lib
            .db
            .query("SELECT path, machine FROM clips ORDER BY path", &[])
            .unwrap()
            .iter()
            .map(|r| (r.get(0).unwrap(), r.get(1).unwrap()))
            .collect();
        let me = crate::machine::name();
        assert_eq!(
            stored,
            [
                ("/disk/b.mp4".to_owned(), me.clone()),
                ("2026/a.mp4".to_owned(), String::new())
            ]
        );
        let clips = lib.clips().unwrap();
        assert_eq!(clips[0].path, Path::new("/nas/2026/a.mp4"));
        assert_eq!(clips[0].local, Some("/edit/2026/a.mp4.mov".into()));
        assert_eq!(clips[1].proxy, ProxyState::Failed("boom".into()));
        assert!(lib.draft(Path::new("/nas/2026/a.mp4")).unwrap().is_some());
        assert!(
            lib.drafts()
                .unwrap()
                .contains_key(Path::new("/nas/2026/a.mp4"))
        );
        // Opened again, it's schema 12 and stays as it is.
        drop(lib);
        let again = Library::open_in(&path, Some("/nas".into())).unwrap();
        assert_eq!(again.clips().unwrap().len(), 2);
    }

    #[test]
    fn machines_share_nas_clips_and_keep_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("library.sqlite3");
        let desk = Library::open_with(
            &shared,
            &dir.path().join("desk.sqlite3"),
            Some("/var/mnt/nas/footage".into()),
            "desk".into(),
        )
        .unwrap();
        let laptop = Library::open_with(
            &shared,
            &dir.path().join("laptop.sqlite3"),
            Some("/media/nas/footage/".into()),
            "laptop".into(),
        )
        .unwrap();
        let on_nas = Clip {
            local: Some("/home/me/edit/a.mp4.mov".into()),
            ..clip(
                "/var/mnt/nas/footage/2026/a.mp4",
                Some("2026-08-01 10:00:00"),
            )
        };
        desk.upsert(&on_nas).unwrap();
        desk.upsert(&clip("/home/me/b.mp4", None)).unwrap();
        laptop.upsert(&clip("/home/me/c.mp4", None)).unwrap();
        let draft = Draft {
            course_id: "c".into(),
            ..Draft::default()
        };
        laptop
            .set_draft(Path::new("/media/nas/footage/2026/a.mp4"), &draft)
            .unwrap();

        let paths = |lib: &Library| -> Vec<PathBuf> {
            lib.clips().unwrap().into_iter().map(|c| c.path).collect()
        };
        assert_eq!(
            paths(&laptop),
            [
                PathBuf::from("/media/nas/footage/2026/a.mp4"),
                "/home/me/c.mp4".into()
            ]
        );
        assert_eq!(
            paths(&desk),
            [
                PathBuf::from("/var/mnt/nas/footage/2026/a.mp4"),
                "/home/me/b.mp4".into()
            ]
        );
        // The local copy is the desk's alone; the draft is everyone's.
        assert_eq!(laptop.clips().unwrap()[0].local, None);
        assert_eq!(desk.clips().unwrap()[0].local, on_nas.local);
        assert_eq!(desk.draft(&on_nas.path).unwrap().unwrap().course_id, "c");
        // The laptop's scan finding only the NAS clip leaves the desk's own.
        let keep = [PathBuf::from("/media/nas/footage/2026/a.mp4")].into();
        assert_eq!(laptop.retain(&keep).unwrap(), 1);
        assert_eq!(paths(&desk).len(), 2);
        assert_eq!(paths(&laptop).len(), 1);
    }
}

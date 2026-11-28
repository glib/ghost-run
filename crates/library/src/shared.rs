//! A catalog shared between machines (#69, decision 0007): the library's
//! tables in a Postgres database of their own (`ghost_run`, on the
//! resolve-db container), instead of this machine's `library.sqlite3`.
//!
//! - Where it is (server, database, user, password) is in
//!   `<data dir>/shared-catalog.json` (0600), never in settings.toml. With
//!   no such file the catalog is this machine's own, as before.
//! - What belongs to one machine stays in its machine file
//!   ([`crate::store::machine_path`]), shared or not.
//! - `library.sqlite3` becomes this machine's copy of the shared catalog
//!   ([`refresh_copy`], whenever the catalog has changed since the last copy:
//!   triggers count changes in the `ghost_run_changes` sequence). When the
//!   server can't be reached the Library opens that copy read-only.
//!   Stopping sharing ([`stop`]) leaves it as this machine's own catalog.
//! - The shared database isn't backed up by Ghost Run: the container is.
//!
//! The queries are the same as for SQLite ([`crate::db`]); only the table
//! definitions here are Postgres's own.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::db::{Db, ToValue, Value};
use crate::store::Library;

/// The schema this Ghost Run writes (the same number as SQLite's).
pub const SCHEMA: u32 = 12;

/// How long connecting may take before the server counts as unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// After failing to connect, how long the copy is used without trying
/// again, so every open doesn't wait out [`CONNECT_TIMEOUT`].
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Where the shared catalog is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 5432,
            database: "ghost_run".into(),
            user: "ghost_run".into(),
            password: String::new(),
        }
    }
}

/// `<data dir>/shared-catalog.json`.
pub fn config_path() -> PathBuf {
    crate::settings::data_dir().join("shared-catalog.json")
}

impl Config {
    /// The saved config, if this machine shares its catalog.
    pub fn load() -> Result<Option<Self>> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("reading {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Written readable by this user only, through a temporary file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    /// `ghost_run on resolve-db.home`.
    pub fn describe(&self) -> String {
        format!("{} on {}", self.database, self.host)
    }

    /// What pooled connections are filed under: everything but the password.
    fn key(&self) -> String {
        format!(
            "{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }

    fn pg_config(&self) -> postgres::Config {
        let mut c = postgres::Config::new();
        c.host(&self.host)
            .port(self.port)
            .dbname(&self.database)
            .user(&self.user)
            .password(&self.password)
            .application_name("ghost-run")
            .connect_timeout(CONNECT_TIMEOUT)
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(60));
        c
    }

    fn check(&self) -> Result<()> {
        ensure!(
            !self.host.trim().is_empty(),
            "the server needs a name or address"
        );
        ensure!(
            !self.database.trim().is_empty(),
            "the database needs a name"
        );
        ensure!(!self.user.trim().is_empty(), "the user needs a name");
        Ok(())
    }
}

/// Idle connections, by [`Config::key`].
static POOL: Mutex<Vec<(String, postgres::Client)>> = Mutex::new(Vec::new());

/// The last failure to connect: the config's key, when, and why.
static DOWN: Mutex<Option<(String, Instant, String)>> = Mutex::new(None);

/// A new connection, its schema made or checked.
fn connect(cfg: &Config) -> Result<postgres::Client> {
    cfg.check()?;
    let mut client = cfg
        .pg_config()
        .connect(postgres::NoTls)
        .with_context(|| format!("connecting to {}", cfg.describe()))?;
    ensure_schema(&mut client)?;
    Ok(client)
}

/// A connection from the pool, else a new one, unless connecting failed
/// within [`RETRY_AFTER`].
fn client(cfg: &Config) -> Result<postgres::Client> {
    let key = cfg.key();
    {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(i) = pool.iter().position(|(k, _)| *k == key) {
            let (_, mut client) = pool.swap_remove(i);
            // One round trip: a connection the server dropped (restarted,
            // or the network went) looks open until it's used.
            if !client.is_closed() && client.is_valid(Duration::from_secs(2)).is_ok() {
                return Ok(client);
            }
        }
    }
    if let Some((k, when, why)) = &*DOWN.lock().unwrap_or_else(|e| e.into_inner())
        && *k == key
        && when.elapsed() < RETRY_AFTER
    {
        bail!("{why}");
    }
    match connect(cfg) {
        Ok(client) => {
            *DOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Ok(client)
        }
        Err(e) => {
            *DOWN.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((key, Instant::now(), format!("{e:#}")));
            Err(e)
        }
    }
}

fn db(cfg: &Config, client: postgres::Client) -> Db {
    Db::postgres(client, &POOL, cfg.key())
}

/// The library: the shared catalog, or this machine's copy of it read-only
/// when the server can't be reached.
pub fn open(cfg: &Config, path: &Path, nas: Option<PathBuf>) -> Result<Library> {
    open_as(cfg, path, nas, crate::machine::name())
}

/// [`open`] as machine `machine`.
pub(crate) fn open_as(
    cfg: &Config,
    path: &Path,
    nas: Option<PathBuf>,
    machine: String,
) -> Result<Library> {
    let machine_file = crate::store::machine_path(path);
    match client(cfg) {
        Ok(client) => Library::assemble(db(cfg, client), &machine_file, nas, machine),
        Err(e) => {
            ensure!(
                path.is_file(),
                "Can't reach the shared catalog ({}), and this machine has no copy of it yet: {e:#}",
                cfg.describe()
            );
            let why = format!(
                "Can't reach the shared catalog ({}): showing this machine's copy from {}, read-only until it's back.",
                cfg.describe(),
                copied_when(path)
            );
            let copy = Db::sqlite_read_only(path, &why)?;
            Library::assemble(copy, &machine_file, nas, machine)
        }
    }
}

/// When this machine's copy was made, as `Sat 28 Nov, 14:05`.
fn copied_when(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%a %-d %b, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "a while ago".into())
}

/// Every shared table and its columns, in the order both schemas have them.
/// The copies ([`copy_in`], [`refresh_copy`]) go by this list, and a test
/// checks it against both schemas.
pub const TABLES: &[(&str, &[&str])] = &[
    (
        "clips",
        &[
            "path",
            "size",
            "modified_ns",
            "recorded",
            "recorded_source",
            "info",
            "probe_error",
            "machine",
        ],
    ),
    (
        "drafts",
        &[
            "path",
            "course_id",
            "frames",
            "confidence",
            "source",
            "updated_ms",
        ],
    ),
    ("course_targets", &["course_id", "seconds"]),
    ("course_references", &["course_id", "run_id"]),
    ("clip_tags", &["clip", "tag"]),
    ("clip_flags", &["clip", "flag"]),
    ("collections", &["id", "name"]),
    ("collection_clips", &["collection", "clip"]),
    ("ride_names", &["clip", "name"]),
    (
        "videos",
        &[
            "id",
            "title",
            "kind",
            "status",
            "project",
            "timeline",
            "link",
            "published",
            "created_ms",
        ],
    ),
    (
        "video_items",
        &[
            "video",
            "position",
            "clip",
            "path",
            "first_frame",
            "frames",
            "timer",
            "card",
            "caption",
        ],
    ),
    (
        "video_usage",
        &["video", "source", "clip", "first_frame", "frames"],
    ),
    ("features", &["id", "name"]),
    ("feature_attempts", &["clip", "feature", "outcome"]),
    (
        "strava_activities",
        &[
            "id",
            "name",
            "sport",
            "start_local",
            "elapsed_seconds",
            "distance_m",
            "climb_m",
        ],
    ),
    ("strava_streams", &["activity", "streams"]),
    (
        "strava_efforts",
        &[
            "activity",
            "segment",
            "name",
            "start_local",
            "elapsed_seconds",
        ],
    ),
];

/// Tables with a generated id, whose sequence follows copied-in rows.
const SERIAL: &[&str] = &["collections", "videos", "features"];

const POSTGRES_TABLES: &str = "
CREATE TABLE IF NOT EXISTS clips (
  path TEXT PRIMARY KEY,
  size BIGINT NOT NULL,
  modified_ns BIGINT NOT NULL,
  recorded TEXT,
  recorded_source TEXT NOT NULL,
  info TEXT,
  probe_error TEXT,
  machine TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS drafts (
  path TEXT PRIMARY KEY,
  course_id TEXT NOT NULL,
  frames TEXT NOT NULL,
  confidence TEXT NOT NULL,
  source TEXT NOT NULL,
  updated_ms BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS course_targets (
  course_id TEXT PRIMARY KEY,
  seconds DOUBLE PRECISION NOT NULL
);
CREATE TABLE IF NOT EXISTS course_references (
  course_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clip_tags (
  clip TEXT NOT NULL,
  tag TEXT NOT NULL,
  PRIMARY KEY (clip, tag)
);
CREATE TABLE IF NOT EXISTS clip_flags (
  clip TEXT PRIMARY KEY,
  flag TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS collections (
  id BIGSERIAL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS collection_clips (
  collection BIGINT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
  clip TEXT NOT NULL,
  PRIMARY KEY (collection, clip)
);
CREATE TABLE IF NOT EXISTS ride_names (
  clip TEXT PRIMARY KEY,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS videos (
  id BIGSERIAL PRIMARY KEY,
  title TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'clips',
  status TEXT NOT NULL DEFAULT 'planned',
  project TEXT,
  timeline TEXT,
  link TEXT,
  published TEXT,
  created_ms BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS video_items (
  video BIGINT NOT NULL,
  position BIGINT NOT NULL,
  clip TEXT NOT NULL,
  path TEXT NOT NULL,
  first_frame BIGINT NOT NULL,
  frames BIGINT NOT NULL,
  timer TEXT,
  card TEXT,
  caption TEXT,
  PRIMARY KEY (video, position)
);
CREATE TABLE IF NOT EXISTS video_usage (
  video BIGINT NOT NULL,
  source TEXT NOT NULL,
  clip TEXT NOT NULL,
  first_frame BIGINT NOT NULL,
  frames BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS features (
  id BIGSERIAL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS feature_attempts (
  clip TEXT PRIMARY KEY,
  feature BIGINT NOT NULL REFERENCES features(id) ON DELETE CASCADE,
  outcome TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS strava_activities (
  id BIGINT PRIMARY KEY,
  name TEXT NOT NULL,
  sport TEXT NOT NULL,
  start_local TEXT NOT NULL,
  elapsed_seconds BIGINT NOT NULL,
  distance_m DOUBLE PRECISION NOT NULL,
  climb_m DOUBLE PRECISION NOT NULL
);
CREATE TABLE IF NOT EXISTS strava_streams (
  activity BIGINT PRIMARY KEY,
  streams TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS strava_efforts (
  activity BIGINT NOT NULL,
  segment BIGINT NOT NULL,
  name TEXT NOT NULL,
  start_local TEXT NOT NULL,
  elapsed_seconds DOUBLE PRECISION NOT NULL
);
CREATE SEQUENCE IF NOT EXISTS ghost_run_changes;
CREATE OR REPLACE FUNCTION ghost_run_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  PERFORM nextval('ghost_run_changes');
  RETURN NULL;
END
$$;
";

/// An arbitrary number, held while one machine makes or fills the schema.
const SCHEMA_LOCK: i64 = 0x6768_6f73_7472;

/// Makes the tables if they aren't there; refuses a newer schema.
fn ensure_schema(client: &mut postgres::Client) -> Result<()> {
    client.batch_execute(
        "CREATE TABLE IF NOT EXISTS ghost_run_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )?;
    let found = schema_of(client)?;
    if let Some(v) = found {
        ensure!(
            v <= SCHEMA,
            "the shared catalog is from a newer Ghost Run (schema {v}): update Ghost Run here"
        );
        if v == SCHEMA {
            return Ok(());
        }
    }
    let mut tx = client.transaction()?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])?;
    tx.batch_execute(POSTGRES_TABLES)?;
    for (table, _) in TABLES {
        // Row triggers, so an upsert that changes nothing isn't a change.
        tx.batch_execute(&format!(
            "DROP TRIGGER IF EXISTS ghost_run_changed ON {table};
             CREATE TRIGGER ghost_run_changed AFTER INSERT OR UPDATE OR DELETE ON {table}
               FOR EACH ROW EXECUTE PROCEDURE ghost_run_changed();"
        ))?;
    }
    tx.execute(
        "INSERT INTO ghost_run_meta (key, value) VALUES ('schema', $1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        &[&SCHEMA.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

fn schema_of(client: &mut postgres::Client) -> Result<Option<u32>> {
    let row = client.query_opt("SELECT value FROM ghost_run_meta WHERE key = 'schema'", &[])?;
    row.map(|r| {
        let v: String = r.get(0);
        v.parse::<u32>()
            .with_context(|| format!("the shared catalog's schema is {v:?}"))
    })
    .transpose()
}

/// What's in a catalog, to say what a switch would do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub clips: i64,
    pub drafts: i64,
    pub tagged: i64,
    pub videos: i64,
    /// Which machine filled it, and when (a shared catalog only).
    pub filled_by: Option<(String, String)>,
}

impl Summary {
    fn of(db: &Db) -> Result<Self> {
        let count = |sql: &str| db.query_one::<i64>(sql, &[]);
        Ok(Self {
            clips: count("SELECT COUNT(*) FROM clips")?,
            drafts: count("SELECT COUNT(*) FROM drafts")?,
            tagged: count("SELECT COUNT(DISTINCT clip) FROM clip_tags")?,
            videos: count("SELECT COUNT(*) FROM videos")?,
            filled_by: None,
        })
    }

    /// Whether there's nothing in it.
    pub fn is_empty(&self) -> bool {
        self.clips == 0 && self.drafts == 0 && self.tagged == 0 && self.videos == 0
    }

    /// `412 clips, 3 drafts, 40 tagged, 2 videos`.
    pub fn text(&self) -> String {
        format!(
            "{} clips, {} drafts, {} tagged, {} videos",
            self.clips, self.drafts, self.tagged, self.videos
        )
    }
}

/// What the shared catalog holds: `None` if no machine has filled it yet.
/// Connects afresh (not from the pool), so it's also the connection test.
pub fn remote(cfg: &Config) -> Result<Option<Summary>> {
    let mut client = connect(cfg)?;
    let filled = client
        .query_opt(
            "SELECT a.value, b.value FROM ghost_run_meta a, ghost_run_meta b
             WHERE a.key = 'filled_by' AND b.key = 'filled_at'",
            &[],
        )?
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)));
    let db = db(cfg, client);
    let summary = Summary::of(&db)?;
    Ok(match filled {
        Some(by) => Some(Summary {
            filled_by: Some(by),
            ..summary
        }),
        None if summary.is_empty() => None,
        None => Some(summary),
    })
}

/// What this machine's own catalog holds.
pub fn local(path: &Path, nas: Option<PathBuf>) -> Result<Summary> {
    if !path.is_file() {
        return Ok(Summary::default());
    }
    Summary::of(&Library::open_in(path, nas)?.db)
}

/// Copies every shared table from `from` to `to` (inside the caller's
/// transactions).
fn copy_tables(from: &Db, to: &Db) -> Result<usize> {
    let mut rows = 0;
    for (table, columns) in TABLES {
        let list = columns.join(", ");
        let marks: Vec<String> = (1..=columns.len()).map(|i| format!("${i}")).collect();
        let insert = format!("INSERT INTO {table} ({list}) VALUES ({})", marks.join(", "));
        for row in from.query(&format!("SELECT {list} FROM {table}"), &[])? {
            let values: Vec<Value> = (0..columns.len())
                .map(|i| row.get::<Value>(i))
                .collect::<Result<_>>()?;
            let params: Vec<&dyn ToValue> = values.iter().map(|v| v as &dyn ToValue).collect();
            to.execute(&insert, &params)
                .with_context(|| format!("copying {table}"))?;
            rows += 1;
        }
    }
    Ok(rows)
}

/// Fills the empty shared catalog from this machine's own (`path`, migrated
/// and its NAS clips rekeyed first). Refused if another machine got there
/// first.
fn copy_in(cfg: &Config, path: &Path, nas: Option<PathBuf>) -> Result<usize> {
    let own = Library::open_in(path, nas)?;
    let shared = db(cfg, connect(cfg)?);
    shared.transaction(|| {
        shared.query("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK])?;
        let filled = shared
            .query_opt(
                "SELECT value FROM ghost_run_meta WHERE key = 'filled_by'",
                &[],
            )?
            .is_some();
        ensure!(
            !filled && Summary::of(&shared)?.is_empty(),
            "another machine has filled the shared catalog already: use it instead"
        );
        let rows = copy_tables(&own.db, &shared)?;
        for table in SERIAL {
            shared.query(
                &format!(
                    "SELECT setval(pg_get_serial_sequence('{table}', 'id'),
                       COALESCE((SELECT MAX(id) FROM {table}), 0) + 1, false)"
                ),
                &[],
            )?;
        }
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        for (key, value) in [("filled_by", crate::machine::name()), ("filled_at", now)] {
            shared.execute(
                "INSERT INTO ghost_run_meta (key, value) VALUES ($1, $2)",
                &[&key, &value],
            )?;
        }
        Ok(rows)
    })
}

/// The change count this machine's copy was made at, per config.
static COPIED_AT: Mutex<Option<(String, i64)>> = Mutex::new(None);

/// One copy at a time.
static COPYING: Mutex<()> = Mutex::new(());

/// Makes `path` (`library.sqlite3`) a copy of the shared catalog, if it has
/// changed since the last copy this run made (always with `force`).
/// Returns whether it copied. The copy is made beside it, then written into
/// `path` in one transaction by SQLite's online backup, so the app's open
/// connections to it can stay open and a reader never sees half of one;
/// it's an ordinary library, read-only only while it's opened as the
/// stand-in.
pub fn refresh_copy(cfg: &Config, path: &Path, force: bool) -> Result<bool> {
    let _one = COPYING.lock().unwrap_or_else(|e| e.into_inner());
    let shared = db(cfg, client(cfg)?);
    shared.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")?;
    let result = (|| -> Result<bool> {
        let changes: i64 = shared.query_one(
            "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM ghost_run_changes",
            &[],
        )?;
        let key = cfg.key();
        if !force
            && path.is_file()
            && *COPIED_AT.lock().unwrap_or_else(|e| e.into_inner()) == Some((key.clone(), changes))
        {
            return Ok(false);
        }
        let dir = path.parent().context("the library has no folder")?;
        let tmp = dir.join(format!(".library-copy-{}.sqlite3", std::process::id()));
        let tmp_machine = crate::store::machine_path(&tmp);
        remove_sqlite(&tmp);
        remove_sqlite(&tmp_machine);
        // An empty library of this schema, made the way any library is.
        drop(Library::open_with(
            &tmp,
            &tmp_machine,
            None,
            crate::machine::name(),
        )?);
        remove_sqlite(&tmp_machine);
        let copy = Db::sqlite(&tmp)?;
        copy.batch("PRAGMA journal_mode=DELETE")?;
        copy.transaction(|| copy_tables(&shared, &copy))?;
        drop(copy);
        let written = write_over(&tmp, path);
        remove_sqlite(&tmp);
        written.with_context(|| format!("writing the copy into {}", path.display()))?;
        *COPIED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, changes));
        Ok(true)
    })();
    let _ = shared.batch("COMMIT");
    result
}

/// Removes a SQLite file and its journals.
fn remove_sqlite(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// How long writing the copy waits for the app's own writes to `path`.
const WRITE_WAIT: Duration = Duration::from_secs(30);

/// Replaces everything in the SQLite database `to` with `from`'s, in one
/// write transaction (SQLite's online backup). Other connections to `to`
/// may stay open, as the app's do: they wait for it like for any write, and
/// see the new contents after. Never renames or removes `to` or its
/// journals, which would pull them out from under those connections.
fn write_over(from: &Path, to: &Path) -> Result<()> {
    use rusqlite::backup::{Backup, StepResult};
    let page_size = |c: &rusqlite::Connection| -> Result<i64> {
        Ok(c.query_row("PRAGMA page_size", [], |r| r.get(0))?)
    };
    let mut dst = rusqlite::Connection::open(to)?;
    dst.busy_timeout(WRITE_WAIT)?;
    {
        // A backup into a WAL database needs the same page size.
        let src = rusqlite::Connection::open(from)?;
        let want = page_size(&dst)?;
        if page_size(&src)? != want {
            src.execute_batch(&format!("PRAGMA page_size = {want}; VACUUM;"))?;
        }
    }
    let src =
        rusqlite::Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let backup = Backup::new(&src, &mut dst)?;
    let until = std::time::Instant::now() + WRITE_WAIT;
    loop {
        match backup.step(-1)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            _ => {
                ensure!(
                    std::time::Instant::now() < until,
                    "it stayed busy for {} s (is a scan writing to it?)",
                    WRITE_WAIT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Keeps a copy of the SQLite database `path` at `to` (`VACUUM INTO`, one
/// read transaction: the app's connections to `path` may stay open).
fn keep_copy_of(path: &Path, to: &Path) -> Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(WRITE_WAIT)?;
    conn.execute("VACUUM INTO ?1", [to.to_string_lossy()])
        .with_context(|| format!("copying {} to {}", path.display(), to.display()))?;
    Ok(())
}

/// What [`start`] did.
#[derive(Debug, PartialEq)]
pub enum Started {
    /// This machine's catalog filled the shared one (rows copied).
    CopiedIn(usize),
    /// The shared catalog was there already; this machine's own was put
    /// aside under this name.
    Joined(Option<PathBuf>),
}

/// Starts sharing: fills the shared catalog from this machine's own if
/// `copy_in` (it must be empty), else uses what's there (it must have been
/// filled), keeping a copy of this machine's own catalog as
/// `library-before-shared-<date>.sqlite3`. Then saves the config and makes
/// this machine's copy in `library.sqlite3` (`path`). Nothing here needs the
/// app to close its own connections to `path`. Once the shared catalog is
/// filled or joined, the config is saved even if making the copy fails: the
/// copy is made again within [`refresh_copy`]'s next round.
pub fn start(
    cfg: &Config,
    path: &Path,
    nas: Option<PathBuf>,
    copy_in: bool,
    config_file: &Path,
) -> Result<Started> {
    let filled = remote(cfg)?;
    let started = if copy_in {
        ensure!(
            filled.is_none(),
            "the shared catalog has been filled already: use it instead"
        );
        let rows = if path.is_file() {
            self::copy_in(cfg, path, nas)?
        } else {
            0
        };
        Started::CopiedIn(rows)
    } else {
        ensure!(
            filled.is_some(),
            "the shared catalog is empty: fill it from this machine's catalog instead"
        );
        let aside = if path.is_file() {
            let today = chrono::Local::now().format("%Y-%m-%d");
            let mut aside = path.with_file_name(format!("library-before-shared-{today}.sqlite3"));
            let mut n = 2;
            while aside.exists() {
                aside = path.with_file_name(format!("library-before-shared-{today}-{n}.sqlite3"));
                n += 1;
            }
            keep_copy_of(path, &aside)?;
            Some(aside)
        } else {
            None
        };
        Started::Joined(aside)
    };
    *DOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    cfg.save_to(config_file)?;
    if let Err(e) = refresh_copy(cfg, path, true) {
        eprintln!("Shared catalog: this machine's copy wasn't made yet: {e:#}");
    }
    Ok(started)
}

/// Stops sharing: this machine's copy of the shared catalog (refreshed
/// first if the server can be reached) becomes its own catalog again.
/// Returns whether the copy is up to date.
pub fn stop(cfg: &Config, path: &Path, config_file: &Path) -> Result<bool> {
    let fresh = refresh_copy(cfg, path, false).is_ok();
    std::fs::remove_file(config_file)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
        .with_context(|| format!("removing {}", config_file.display()))?;
    Ok(fresh)
}

/// Whether the shared catalog can be reached right now (from the pool, or
/// connecting), for the Settings status line.
pub fn reachable(cfg: &Config) -> Result<()> {
    drop(db(cfg, client(cfg)?));
    Ok(())
}

/// Forgets a failure to connect, so the next open tries again at once.
pub fn retry_now() {
    *DOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Tests that need a Postgres server: `GHOST_RUN_TEST_POSTGRES` holds a
/// connection string for a user that may create databases
/// (`host=localhost port=5432 user=postgres password=postgres`). Without it
/// they're skipped, unless `REQUIRE_POSTGRES` is set (CI).
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A fresh, empty database for one test, and its config.
    pub fn database(name: &str) -> Option<Config> {
        let Ok(conn) = std::env::var("GHOST_RUN_TEST_POSTGRES") else {
            assert!(
                std::env::var_os("REQUIRE_POSTGRES").is_none(),
                "REQUIRE_POSTGRES is set but GHOST_RUN_TEST_POSTGRES isn't"
            );
            eprintln!("skipped: no GHOST_RUN_TEST_POSTGRES");
            return None;
        };
        let admin: postgres::Config = conn.parse().unwrap();
        let mut client = admin.connect(postgres::NoTls).unwrap();
        let db = format!("ghost_run_test_{name}_{}", std::process::id());
        // One statement per call: several in one run as a transaction,
        // which these can't be in.
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS {db}"))
            .unwrap();
        client
            .batch_execute(&format!("CREATE DATABASE {db}"))
            .unwrap();
        let host = match admin.get_hosts().first() {
            Some(postgres::config::Host::Tcp(h)) => h.clone(),
            _ => "localhost".into(),
        };
        Some(Config {
            host,
            port: admin.get_ports().first().copied().unwrap_or(5432),
            database: db,
            user: admin.get_user().unwrap_or("postgres").into(),
            password: admin
                .get_password()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Clip, Draft, ProxyState};

    fn clip(path: &str, recorded: &str) -> Clip {
        Clip {
            path: path.into(),
            size: 10,
            modified_ns: 1,
            recorded: Some(recorded.into()),
            recorded_source: "filename".into(),
            ..Clip::default()
        }
    }

    /// The column lists match both schemas exactly.
    #[test]
    fn the_table_list_matches_both_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let conn = lib.db.sqlite_conn().unwrap();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut listed: Vec<String> = TABLES.iter().map(|(t, _)| t.to_string()).collect();
        tables.sort();
        listed.sort();
        assert_eq!(tables, listed);
        for (table, columns) in TABLES {
            let found: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(found, *columns, "{table} in SQLite");
        }
        let Some(cfg) = testing::database("tables") else {
            return;
        };
        let mut client = connect(&cfg).unwrap();
        for (table, columns) in TABLES {
            let found: Vec<String> = client
                .query(
                    "SELECT column_name::text FROM information_schema.columns
                     WHERE table_name = $1 ORDER BY ordinal_position",
                    &[table],
                )
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(found, *columns, "{table} in Postgres");
        }
    }

    #[test]
    fn config_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/shared-catalog.json");
        assert_eq!(Config::load_from(&path).unwrap(), None);
        let cfg = Config {
            host: "resolve-db.home".into(),
            password: "s3cret".into(),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), Some(cfg.clone()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(cfg.describe(), "ghost_run on resolve-db.home");
        assert!(!cfg.key().contains("s3cret"));
    }

    #[test]
    fn an_unreachable_server_opens_the_copy_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        // Nothing listens on port 1 (refused at once).
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 1,
            ..Config::default()
        };
        let err = open(&cfg, &path, None).err().unwrap();
        assert!(format!("{err:#}").contains("no copy of it yet"), "{err:#}");
        {
            let own = Library::open(&path).unwrap();
            own.upsert(&clip("/v/a.mp4", "2026-08-01 10:00:00"))
                .unwrap();
        }
        let lib = open(&cfg, &path, None).unwrap();
        assert!(lib.read_only().unwrap().contains("read-only"));
        assert_eq!(lib.clips().unwrap().len(), 1);
        let err = lib.add_tag(&["a.mp4.10".into()], "x").err().unwrap();
        assert!(format!("{err}").contains("Can't reach the shared catalog"));
        // This machine's part is still saved.
        lib.set_proxy(
            Path::new("/v/a.mp4"),
            ProxyState::Ready("/v/Proxy/a.mov".into()),
        )
        .unwrap();
        lib.upsert(&clip("/v/b.mp4", "2026-08-02 10:00:00"))
            .unwrap();
        assert_eq!(
            lib.clips().unwrap().len(),
            1,
            "new clips wait for the server"
        );
        assert_eq!(
            lib.clips().unwrap()[0].proxy,
            ProxyState::Ready("/v/Proxy/a.mov".into())
        );
    }

    /// Every other catalog call, on Postgres.
    #[test]
    fn the_rest_of_the_catalog_works_on_postgres() {
        use crate::strava::{Activity, Effort, Streams};
        use crate::videos::{Item, Use};
        let Some(cfg) = testing::database("rest") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        let lib = open_as(&cfg, &path, Some("/nas".into()), "desk".into()).unwrap();
        assert!(lib.is_shared());

        // Clips: whole-path rows under the root are rekeyed; retain.
        lib.db
            .execute(
                "INSERT INTO clips (path, size, modified_ns, recorded_source, machine)
                 VALUES ('/nas/old.mp4', 1, 1, 'modified', 'desk')",
                &[],
            )
            .unwrap();
        assert_eq!(lib.rekey().unwrap(), 1);
        assert_eq!(lib.clips().unwrap()[0].path, Path::new("/nas/old.mp4"));
        lib.upsert(&clip("/home/me/new.mp4", "2026-08-01 10:00:00"))
            .unwrap();
        let keep = [PathBuf::from("/home/me/new.mp4")].into();
        assert_eq!(lib.retain(&keep).unwrap(), 1);

        // References, rides, collections and features, deleted again.
        lib.set_reference("c", "r1").unwrap();
        lib.set_reference("c", "r2").unwrap();
        assert_eq!(lib.reference("c").unwrap().as_deref(), Some("r2"));
        let ride = crate::organise::Ride {
            id: "a.mp4.1".into(),
            start: Default::default(),
            end: Default::default(),
            clips: vec!["a.mp4.1".into(), "b.mp4.1".into()],
            name: None,
        };
        lib.name_ride(&ride, "Squamish").unwrap();
        lib.name_ride(&ride, "Squamish day").unwrap();
        assert_eq!(lib.catalog().unwrap().ride_names["a.mp4.1"], "Squamish day");
        let race = lib.collection("Race").unwrap();
        lib.add_to_collection(race, &["a.mp4.1".into()]).unwrap();
        lib.add_to_collection(race, &["a.mp4.1".into()]).unwrap();
        lib.remove_from_collection(race, &["a.mp4.1".into()])
            .unwrap();
        lib.rename_collection(race, "Races").unwrap();
        lib.delete_collection(race).unwrap();
        let drop_ = lib.feature("Drop").unwrap();
        lib.set_attempts(
            drop_,
            &["a.mp4.1".into()],
            crate::features::Outcome::Crashed,
        )
        .unwrap();
        lib.remove_attempts(&["a.mp4.1".into()]).unwrap();
        lib.rename_feature(drop_, "Big drop").unwrap();
        lib.delete_feature(drop_).unwrap();
        lib.remove_tag(&["a.mp4.1".into()], "x").unwrap();
        lib.set_flag(&["a.mp4.1".into()], None).unwrap();
        let c = lib.catalog().unwrap();
        assert!(c.collections.is_empty() && c.features.is_empty());

        // Videos: items with a card and a stored path, usage, delete.
        let v = lib.new_video("Trail", "trail").unwrap();
        let mut video = lib.video(v).unwrap().unwrap();
        video.title = "Trail day".into();
        video.published = Some("2026-11-28".into());
        lib.save_video(&video).unwrap();
        let item = Item {
            clip: "a.mp4.1".into(),
            path: "/nas/2026/a.mp4".into(),
            first_frame: 3,
            frames: 90,
            timer: Some("r1".into()),
            caption: Some("hi".into()),
            ..Item::default()
        };
        lib.set_items(v, std::slice::from_ref(&item)).unwrap();
        assert_eq!(lib.add_items(v, std::slice::from_ref(&item)).unwrap(), 0);
        let got = lib.video(v).unwrap().unwrap();
        assert_eq!(got.title, "Trail day");
        assert_eq!(got.items, [item]);
        let stored: String = lib
            .db
            .query_one("SELECT path FROM video_items", &[])
            .unwrap();
        assert_eq!(stored, "2026/a.mp4");
        let uses = [Use {
            video: v,
            clip: "a.mp4.1".into(),
            first_frame: 3,
            frames: 90,
        }];
        lib.set_usage(v, "built", &uses).unwrap();
        lib.set_usage(v, "timeline", &uses).unwrap();
        assert_eq!(lib.read_back().unwrap(), [v].into());
        assert_eq!(lib.usage().unwrap()[&v].len(), 1);
        lib.delete_video(v).unwrap();
        assert!(lib.videos().unwrap().is_empty());

        // Strava.
        let start = chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        let activity = Activity {
            id: 9_000_000_001,
            name: "Morning ride".into(),
            sport: "MountainBikeRide".into(),
            start_local: start,
            elapsed_seconds: 3600,
            distance_m: 12_345.5,
            climb_m: 456.0,
        };
        lib.set_strava_activity(&activity).unwrap();
        lib.set_strava_activity(&activity).unwrap();
        let streams = Streams {
            time: vec![0.0, 1.0],
            ..Streams::default()
        };
        let effort = Effort {
            activity: activity.id,
            segment: 77,
            name: "Seg".into(),
            start_local: start,
            elapsed_seconds: 61.5,
        };
        lib.set_strava_detail(activity.id, &streams, std::slice::from_ref(&effort))
            .unwrap();
        assert_eq!(lib.strava_activities().unwrap(), [activity]);
        assert_eq!(lib.strava_detailed().unwrap(), [9_000_000_001]);
        assert_eq!(lib.strava_streams(9_000_000_001).unwrap(), Some(streams));
        assert_eq!(lib.strava_index().unwrap().efforts, [effort]);
        lib.clear_strava().unwrap();
        assert!(lib.strava_activities().unwrap().is_empty());
    }

    /// The whole story against a real server: the desk fills the shared
    /// catalog, the laptop joins it, both see each other's edits, the
    /// laptop's copy keeps up, and stopping leaves a working library.
    #[test]
    fn desk_fills_it_laptop_joins_and_both_edit() {
        let Some(cfg) = testing::database("story") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let desk_dir = dir.path().join("desk");
        let laptop_dir = dir.path().join("laptop");
        std::fs::create_dir_all(&desk_dir).unwrap();
        std::fs::create_dir_all(&laptop_dir).unwrap();
        let desk_path = desk_dir.join("library.sqlite3");
        let laptop_path = laptop_dir.join("library.sqlite3");
        let nas = Some(PathBuf::from("/nas"));
        {
            let own = Library::open_in(&desk_path, nas.clone()).unwrap();
            own.upsert(&clip("/nas/2026/a.mp4", "2026-08-01 10:00:00"))
                .unwrap();
            own.upsert(&clip("/home/me/b.mp4", "2026-08-02 10:00:00"))
                .unwrap();
            own.add_tag(&["a.mp4.10".into()], "wet").unwrap();
            own.collection("Race").unwrap();
            let v = own.new_video("First", "clips").unwrap();
            assert_eq!(v, 1);
            own.set_draft(
                Path::new("/nas/2026/a.mp4"),
                &Draft {
                    course_id: "c".into(),
                    ..Draft::default()
                },
            )
            .unwrap();
        }
        {
            let own = Library::open(&laptop_path).unwrap();
            own.add_tag(&["x.mp4.1".into()], "laptop only").unwrap();
        }
        let desk_cfg_file = desk_dir.join("shared-catalog.json");
        let laptop_cfg_file = laptop_dir.join("shared-catalog.json");
        // The app has each machine's own catalog open while it starts
        // sharing (the Library page, thumbnails, previews): starting mustn't
        // need it to itself.
        let desk_app = Library::open_in(&desk_path, nas.clone()).unwrap();
        let laptop_app = Library::open(&laptop_path).unwrap();
        assert_eq!(desk_app.clips().unwrap().len(), 2);
        assert_eq!(laptop_app.clips().unwrap().len(), 0);

        assert_eq!(remote(&cfg).unwrap(), None);
        // Joining an empty catalog is refused; so is filling it twice.
        assert!(start(&cfg, &laptop_path, None, false, &laptop_cfg_file).is_err());
        let started = start(&cfg, &desk_path, nas.clone(), true, &desk_cfg_file).unwrap();
        assert!(
            matches!(started, Started::CopiedIn(n) if n >= 5),
            "{started:?}"
        );
        assert!(desk_cfg_file.is_file());
        let filled = remote(&cfg).unwrap().unwrap();
        assert_eq!(
            (filled.clips, filled.drafts, filled.tagged, filled.videos),
            (2, 1, 1, 1)
        );
        assert!(start(&cfg, &laptop_path, None, true, &laptop_cfg_file).is_err());

        let started = start(&cfg, &laptop_path, None, false, &laptop_cfg_file).unwrap();
        let Started::Joined(Some(aside)) = started else {
            panic!("{started:?}");
        };
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("library-before-shared-")
        );
        // The laptop's own catalog was kept aside; its copy is the shared one.
        let kept = Library::open(&aside).unwrap();
        assert_eq!(kept.catalog().unwrap().tags_of("x.mp4.1").unwrap().len(), 1);
        let copy = Library::open(&laptop_path).unwrap();
        assert!(copy.catalog().unwrap().tags_of("x.mp4.1").is_none());
        assert_eq!(copy.videos().unwrap().len(), 1);
        drop((kept, copy));
        // Connections opened before (as the app's are) see the copy too.
        assert!(laptop_app.catalog().unwrap().tags_of("x.mp4.1").is_none());
        drop((desk_app, laptop_app));
        let laptop_nas = Some(PathBuf::from("/media/nas"));

        let desk = open(&cfg, &desk_path, nas.clone()).unwrap();
        let laptop = open_as(&cfg, &laptop_path, laptop_nas.clone(), "laptop".into()).unwrap();
        assert!(desk.is_shared() && desk.read_only().is_none());
        // The NAS clip under each machine's own mount; the desk's own clip
        // only on the desk.
        let paths = |lib: &Library| -> Vec<PathBuf> {
            lib.clips().unwrap().into_iter().map(|c| c.path).collect()
        };
        assert_eq!(paths(&laptop), [PathBuf::from("/media/nas/2026/a.mp4")]);
        assert_eq!(paths(&desk).len(), 2);
        assert!(
            laptop
                .draft(Path::new("/media/nas/2026/a.mp4"))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            laptop.catalog().unwrap().tags_of("a.mp4.10").unwrap().len(),
            1
        );
        // New ids carry on after the copied ones.
        assert_eq!(laptop.new_video("Second", "clips").unwrap(), 2);
        assert_eq!(
            laptop.collection("race").unwrap(),
            desk.collection("Race").unwrap()
        );
        laptop.add_tag(&["a.mp4.10".into()], "laptop tag").unwrap();
        laptop.set_target("c", Some(99.5)).unwrap();
        laptop
            .set_flag(&["a.mp4.10".into()], Some(crate::organise::Flag::Pick))
            .unwrap();
        let f = laptop.feature("Drop").unwrap();
        laptop
            .set_attempts(f, &["a.mp4.10".into()], crate::features::Outcome::Landed)
            .unwrap();
        assert_eq!(desk.target("c").unwrap(), Some(99.5));
        assert_eq!(desk.videos().unwrap().len(), 2);
        assert_eq!(desk.features().unwrap()[0].attempts.len(), 1);
        // A scan's unchanged upsert isn't a change; a real one is.
        assert!(refresh_copy(&cfg, &laptop_path, false).unwrap());
        assert!(!refresh_copy(&cfg, &laptop_path, false).unwrap());
        desk.upsert(&clip("/nas/2026/a.mp4", "2026-08-01 10:00:00"))
            .unwrap();
        assert!(!refresh_copy(&cfg, &laptop_path, false).unwrap());
        desk.set_target("c", Some(98.0)).unwrap();
        assert!(refresh_copy(&cfg, &laptop_path, false).unwrap());
        drop((desk, laptop));

        // The laptop stops sharing: its copy is its own library again, with
        // everything in it.
        assert!(stop(&cfg, &laptop_path, &laptop_cfg_file).unwrap());
        assert!(!laptop_cfg_file.exists());
        let own = Library::open_in(&laptop_path, laptop_nas).unwrap();
        assert_eq!(own.target("c").unwrap(), Some(98.0));
        assert_eq!(own.videos().unwrap().len(), 2);
        own.add_tag(&["a.mp4.10".into()], "after").unwrap();
    }
}

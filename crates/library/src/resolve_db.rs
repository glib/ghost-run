//! DaVinci Resolve's projects, read straight from its project database
//! (decision 0008): Resolve keeps its project library in Postgres on
//! resolve-db, the server the shared catalog is on, so Ghost Run can list
//! every project, its timelines and the files each timeline uses without
//! Resolve running, and say which of them it has the footage for.
//!
//! Read only, always: the `ghost_run` role is given `SELECT` on Resolve's
//! database by `scripts/catalog-db-setup.sh` and nothing more. Resolve owns
//! that schema and doesn't document it; what's read is the part community
//! tools have worked out (checked against Resolve 19):
//!
//! `SM_Project` → `Sm2Timeline` (by `SM_Project_id`) → `Sm2Sequence` (by
//! `Sm2Timeline_id`) → `Sm2TiTrack` (its `Sequence`; `Type` 0 video, 1 audio)
//! → `Sm2TiItem` (by `Sm2TiTrack_id`: `MediaFilePath`, `In` the first source
//! frame, `Duration` in frames).
//!
//! Everything is cast to text in SQL and parsed here, so a column whose type
//! changes between Resolve versions doesn't break the read. When the tables
//! aren't laid out like this, [`read`] says so and the Videos page falls back
//! to asking Resolve itself (the bridge).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::footage::Roots;
use crate::relink::camera_name;
use crate::shared::Config;
use crate::store::Clip;
use crate::videos::OnTimeline;

/// A Resolve project.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Project {
    pub name: String,
    /// When Resolve last saved it, in seconds since the Unix epoch.
    pub modified: Option<i64>,
    pub timelines: Vec<Timeline>,
}

/// One of a project's timelines.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Timeline {
    pub name: String,
    pub modified: Option<i64>,
    /// Its items with a file, video tracks first, in track and record order.
    pub items: Vec<TimelineItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineItem {
    pub path: PathBuf,
    /// On a video track (else audio).
    pub video: bool,
    /// The first frame of the file used.
    pub first_frame: i64,
    pub frames: i64,
}

impl Timeline {
    /// Each file it uses, once, in the order first used.
    pub fn files(&self) -> Vec<&Path> {
        let mut seen = HashSet::new();
        self.items
            .iter()
            .map(|i| i.path.as_path())
            .filter(|p| seen.insert(*p))
            .collect()
    }

    /// Its video items, as the bridge's `read_timeline` gives them.
    pub fn on_timeline(&self) -> Vec<OnTimeline> {
        self.items
            .iter()
            .filter(|i| i.video)
            .map(|i| OnTimeline {
                path: i.path.clone(),
                first_frame: i.first_frame,
                frames: i.frames,
            })
            .collect()
    }
}

/// The databases on the server that look like Resolve's (they have
/// `SM_Project`) and that `cfg`'s user may read; `cfg.database` is where to
/// connect first (the shared catalog's).
pub fn databases(cfg: &Config) -> Result<Vec<String>> {
    let mut client = connect(cfg)?;
    let names: Vec<String> = client
        .query(
            "SELECT datname FROM pg_database
             WHERE NOT datistemplate AND datallowconn
               AND has_database_privilege(datname, 'CONNECT')
             ORDER BY datname",
            &[],
        )?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut found = Vec::new();
    for name in names {
        if name == cfg.database {
            continue;
        }
        let other = Config {
            database: name.clone(),
            ..cfg.clone()
        };
        let Ok(mut c) = connect(&other) else {
            continue;
        };
        if has_table(&mut c, "SM_Project").unwrap_or(false) {
            found.push(name);
        }
    }
    Ok(found)
}

/// Resolve's database to read: `wanted` if set, else the only one
/// [`databases`] finds.
pub fn pick(cfg: &Config, wanted: Option<&str>) -> Result<Config> {
    if let Some(name) = wanted.map(str::trim).filter(|n| !n.is_empty()) {
        return Ok(Config {
            database: name.to_owned(),
            ..cfg.clone()
        });
    }
    let found = databases(cfg)?;
    match found.as_slice() {
        [one] => Ok(Config {
            database: one.clone(),
            ..cfg.clone()
        }),
        [] => bail!(
            "no Resolve project database on {} that Ghost Run may read: run scripts/catalog-db-setup.sh --resolve on the server",
            cfg.host
        ),
        many => bail!(
            "several Resolve databases on {} ({}): pick one in Settings → DaVinci Resolve",
            cfg.host,
            many.join(", ")
        ),
    }
}

fn connect(cfg: &Config) -> Result<postgres::Client> {
    let mut c = postgres::Config::new();
    c.host(&cfg.host)
        .port(cfg.port)
        .dbname(&cfg.database)
        .user(&cfg.user)
        .password(&cfg.password)
        .application_name("ghost-run (read only)")
        .connect_timeout(crate::shared::CONNECT_TIMEOUT);
    let mut client = c
        .connect(postgres::NoTls)
        .with_context(|| format!("connecting to {}", cfg.describe()))?;
    // Belt and braces: the role can only read, and so can this session.
    client.batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")?;
    Ok(client)
}

fn has_table(client: &mut postgres::Client, table: &str) -> Result<bool> {
    let row = client.query_one("SELECT to_regclass(quote_ident($1)) IS NOT NULL", &[&table])?;
    Ok(row.get(0))
}

fn columns(client: &mut postgres::Client, table: &str) -> Result<HashSet<String>> {
    Ok(client
        .query(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = current_schema() AND table_name = $1",
            &[&table],
        )?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// The tables and columns [`read`] needs, each `table.column`.
pub const NEEDED: &[(&str, &[&str])] = &[
    (
        "SM_Project",
        &["SM_Project_id", "ProjectName", "LastModTimeInSecs"],
    ),
    (
        "Sm2Timeline",
        &["Sm2Timeline_id", "Name", "SM_Project_id", "ModTimeInSecs"],
    ),
    ("Sm2Sequence", &["Sm2Sequence_id", "Sm2Timeline_id"]),
    ("Sm2TiTrack", &["Sm2TiTrack_id", "Type", "Sequence"]),
    (
        "Sm2TiItem",
        &["Sm2TiTrack_id", "MediaFilePath", "In", "Duration", "Start"],
    ),
];

/// What's missing of [`NEEDED`], as `table.column` (empty: all there).
pub fn missing(client: &mut postgres::Client) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (table, cols) in NEEDED {
        let have = columns(client, table)?;
        if have.is_empty() {
            out.push((*table).to_owned());
            continue;
        }
        out.extend(
            cols.iter()
                .filter(|c| !have.contains(**c))
                .map(|c| format!("{table}.{c}")),
        );
    }
    Ok(out)
}

/// Every project in Resolve's database `cfg`, newest first, with its
/// timelines (newest first) and the files they use.
pub fn read(cfg: &Config) -> Result<Vec<Project>> {
    let mut client = connect(cfg)?;
    let gone = missing(&mut client)?;
    if !gone.is_empty() {
        bail!(
            "Resolve's database isn't laid out as Ghost Run expects (a newer Resolve?): no {}",
            gone.join(", ")
        );
    }
    let rows = client.query(
        r#"SELECT p."SM_Project_id"::text, p."ProjectName"::text, p."LastModTimeInSecs"::text,
                  t."Sm2Timeline_id"::text, t."Name"::text, t."ModTimeInSecs"::text,
                  tr."Type"::text, i."MediaFilePath"::text, i."In"::text,
                  i."Duration"::text, i."Start"::text
           FROM "SM_Project" p
           LEFT JOIN "Sm2Timeline" t ON t."SM_Project_id" = p."SM_Project_id"
           LEFT JOIN "Sm2Sequence" s ON s."Sm2Timeline_id" = t."Sm2Timeline_id"
           LEFT JOIN "Sm2TiTrack" tr ON tr."Sequence" = s."Sm2Sequence_id"
           LEFT JOIN "Sm2TiItem" i ON i."Sm2TiTrack_id" = tr."Sm2TiTrack_id"
                AND coalesce(i."MediaFilePath", '') <> ''"#,
        &[],
    )?;
    let mut projects: Vec<Project> = Vec::new();
    let mut project_at: HashMap<String, usize> = HashMap::new();
    let mut timeline_at: HashMap<String, (usize, usize)> = HashMap::new();
    // Each timeline's items, by (project, timeline), with (audio, record
    // start) to sort them by.
    type Sortable = (bool, i64, TimelineItem);
    let mut order: HashMap<(usize, usize), Vec<Sortable>> = HashMap::new();
    for row in &rows {
        let get = |i: usize| row.get::<_, Option<String>>(i);
        let Some(pid) = get(0) else { continue };
        let p = *project_at.entry(pid).or_insert_with(|| {
            projects.push(Project {
                name: get(1).unwrap_or_default(),
                modified: get(2).and_then(|s| number(&s)),
                timelines: Vec::new(),
            });
            projects.len() - 1
        });
        let Some(tid) = get(3) else { continue };
        let (p, t) = *timeline_at.entry(tid).or_insert_with(|| {
            projects[p].timelines.push(Timeline {
                name: get(4).unwrap_or_default(),
                modified: get(5).and_then(|s| number(&s)),
                items: Vec::new(),
            });
            (p, projects[p].timelines.len() - 1)
        });
        let Some(path) = get(7) else { continue };
        let video = get(6).and_then(|s| number(&s)) == Some(0);
        let item = TimelineItem {
            path: PathBuf::from(path),
            video,
            first_frame: get(8).and_then(|s| number(&s)).unwrap_or(0),
            frames: get(9).and_then(|s| number(&s)).unwrap_or(0),
        };
        let start = get(10).and_then(|s| number(&s)).unwrap_or(0);
        order.entry((p, t)).or_default().push((!video, start, item));
    }
    for ((p, t), mut items) in order {
        items.sort_by_key(|(audio, start, _)| (*audio, *start));
        projects[p].timelines[t].items = items.into_iter().map(|(_, _, i)| i).collect();
    }
    for p in &mut projects {
        p.timelines
            .sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    }
    projects.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    Ok(projects)
}

/// A number Resolve wrote as text (`"262"`, `"262.0"`).
fn number(s: &str) -> Option<i64> {
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| {
        s.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| f.round() as i64)
    })
}

/// Finds the catalogued clip for a file a timeline names, quickly enough to
/// do for every timeline: by its path, its local copy's path, the original
/// of a local copy, else a camera name only one clip has. (No file system
/// lookups: [`crate::relink::find`] does the careful version for the one
/// project being brought back.)
pub struct Matcher<'a> {
    roots: &'a Roots,
    by_path: HashMap<&'a Path, &'a Clip>,
    by_copy: HashMap<PathBuf, &'a Clip>,
    by_name: HashMap<String, Vec<&'a Clip>>,
}

impl<'a> Matcher<'a> {
    pub fn new(clips: &'a [Clip], roots: &'a Roots) -> Self {
        let mut m = Self {
            roots,
            by_path: HashMap::new(),
            by_copy: HashMap::new(),
            by_name: HashMap::new(),
        };
        for c in clips {
            m.by_path.insert(c.path.as_path(), c);
            if let Some(local) = &c.local {
                m.by_copy.insert(local.clone(), c);
            }
            if let Some(copy) = roots.local_copy(&c.path) {
                m.by_copy.insert(copy, c);
            }
            m.by_name.entry(camera_name(&c.path)).or_default().push(c);
        }
        m
    }

    pub fn find(&self, path: &Path) -> Option<&'a Clip> {
        if let Some(c) = self.by_path.get(path).or_else(|| self.by_copy.get(path)) {
            return Some(c);
        }
        if let Some(c) = self
            .roots
            .original_of(path)
            .and_then(|o| self.by_path.get(o.as_path()).copied())
        {
            return Some(c);
        }
        match self.by_name.get(&camera_name(path)).map(Vec::as_slice) {
            Some([one]) => Some(one),
            _ => None,
        }
    }
}

/// How much of a timeline's footage this machine has.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Footage {
    /// Files it uses (Ghost Run's overlays and captions left out).
    pub files: usize,
    /// Of those, the ones whose file is here as the timeline names it.
    pub here: usize,
    /// Catalogued clips not here: on the NAS, to copy down.
    pub on_nas: usize,
    /// Catalogued, here, but not where the timeline looks (to relink).
    pub elsewhere: usize,
    /// Neither here nor in the catalog (music, logos, footage never
    /// catalogued).
    pub unknown: usize,
}

impl Footage {
    /// Ready to open: every file is where the timeline looks for it.
    pub fn ready(&self) -> bool {
        self.here == self.files
    }

    /// A few words for a list row.
    pub fn text(&self) -> String {
        if self.files == 0 {
            return "no footage".into();
        }
        if self.ready() {
            return "footage here".into();
        }
        let mut parts = Vec::new();
        if self.on_nas > 0 {
            parts.push(format!("{} on the NAS", self.on_nas));
        }
        if self.elsewhere > 0 {
            parts.push(format!("{} to relink", self.elsewhere));
        }
        if self.unknown > 0 {
            parts.push(format!("{} missing", self.unknown));
        }
        parts.join(" · ")
    }
}

/// Whether Ghost Run made the file (an overlay, a caption, a card).
fn ours(path: &Path) -> bool {
    camera_name(path).ends_with(".overlay.mov") || path.starts_with(crate::settings::data_dir())
}

/// How much of `timeline`'s footage is here; `exists` says whether a path is
/// a file (the file system, but not in tests).
pub fn footage(timeline: &Timeline, m: &Matcher, exists: &dyn Fn(&Path) -> bool) -> Footage {
    let mut f = Footage::default();
    for path in timeline.files() {
        if ours(path) {
            continue;
        }
        f.files += 1;
        if exists(path) {
            f.here += 1;
            continue;
        }
        match m.find(path) {
            Some(c) => {
                let local = m.roots.local_copy(&c.path).or_else(|| c.local.clone());
                let copy_here = local.as_deref().is_some_and(exists);
                if m.roots.on_nas(&c.path) && !copy_here {
                    f.on_nas += 1;
                } else {
                    f.elsewhere += 1;
                }
            }
            None => f.unknown += 1,
        }
    }
    f
}

/// The clips (by [`crate::organise::key`]) a timeline uses.
pub fn clip_keys(timeline: &Timeline, m: &Matcher) -> HashSet<String> {
    timeline
        .files()
        .into_iter()
        .filter_map(|p| m.find(p))
        .map(crate::organise::key)
        .collect()
}

/// The clips any timeline uses whose project Resolve saved at or after
/// `since` (seconds), with the project names: footage someone is probably
/// still editing, whose local copies shouldn't be removed. `skip` names the
/// (project, timeline) pairs to leave out (published videos').
pub fn in_use(
    projects: &[Project],
    m: &Matcher,
    since: i64,
    skip: &HashSet<(String, String)>,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for p in projects {
        if p.modified.unwrap_or(i64::MAX) < since {
            continue;
        }
        for t in &p.timelines {
            if skip.contains(&(p.name.clone(), t.name.clone())) {
                continue;
            }
            for key in clip_keys(t, m) {
                let names = out.entry(key).or_default();
                if !names.contains(&p.name) {
                    names.push(p.name.clone());
                }
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    /// Makes the tables [`super::read`] reads in an empty database, laid out
    /// as Resolve 19's (only the columns used, and a few others), and fills
    /// them: `(project, modified, [(timeline, [(video, path, in, duration,
    /// start)])])`.
    #[allow(clippy::type_complexity)]
    pub fn fill(
        client: &mut postgres::Client,
        projects: &[(&str, i64, &[(&str, &[(bool, &str, i64, i64, i64)])])],
    ) {
        client
            .batch_execute(
                r#"CREATE TABLE "SM_Project" ("SM_Project_id" uuid PRIMARY KEY,
                     "ProjectName" varchar, "LastModTimeInSecs" bigint, "LockId" uuid);
                   CREATE TABLE "Sm2Timeline" ("Sm2Timeline_id" uuid PRIMARY KEY,
                     "Name" varchar, "SM_Project_id" uuid, "ModTimeInSecs" bigint,
                     "CreateTimeInSecs" bigint);
                   CREATE TABLE "Sm2Sequence" ("Sm2Sequence_id" uuid PRIMARY KEY,
                     "Sm2Timeline_id" uuid, "FrameRate" bytea);
                   CREATE TABLE "Sm2TiTrack" ("Sm2TiTrack_id" uuid PRIMARY KEY,
                     "Type" integer, "Sequence" uuid, "UserDefinedName" varchar);
                   CREATE TABLE "Sm2TiItem" ("Sm2TiItem_id" uuid PRIMARY KEY,
                     "Name" varchar, "MediaFilePath" varchar, "In" varchar,
                     "Start" varchar, "Duration" varchar, "Sm2TiTrack_id" uuid,
                     "MarkersBA" bytea);"#,
            )
            .unwrap();
        let mut n = 0u32;
        let mut id = || {
            n += 1;
            format!("00000000-0000-0000-0000-{n:012}")
        };
        for (project, modified, timelines) in projects {
            let pid = id();
            client
                .execute(
                    r#"INSERT INTO "SM_Project" VALUES ($1::text::uuid, $2, $3, NULL)"#,
                    &[&pid, project, modified],
                )
                .unwrap();
            for (timeline, items) in timelines.iter() {
                let (tid, sid) = (id(), id());
                client
                    .execute(
                        r#"INSERT INTO "Sm2Timeline" VALUES ($1::text::uuid, $2, $3::text::uuid, $4, 0)"#,
                        &[&tid, timeline, &pid, modified],
                    )
                    .unwrap();
                client
                    .execute(
                        r#"INSERT INTO "Sm2Sequence" VALUES ($1::text::uuid, $2::text::uuid, NULL)"#,
                        &[&sid, &tid],
                    )
                    .unwrap();
                let (v, a) = (id(), id());
                for (track, kind) in [(&v, 0i32), (&a, 1)] {
                    client
                        .execute(
                            r#"INSERT INTO "Sm2TiTrack" VALUES ($1::text::uuid, $2, $3::text::uuid, '')"#,
                            &[track, &kind, &sid],
                        )
                        .unwrap();
                }
                for (video, path, first, frames, start) in items.iter() {
                    let iid = id();
                    let track = if *video { &v } else { &a };
                    client
                        .execute(
                            r#"INSERT INTO "Sm2TiItem" VALUES ($1::text::uuid, 'x', $2, $3, $4, $5, $6::text::uuid, NULL)"#,
                            &[
                                &iid,
                                path,
                                &first.to_string(),
                                &start.to_string(),
                                &frames.to_string(),
                                track,
                            ],
                        )
                        .unwrap();
                }
            }
        }
        // A title or generator: no file.
        client
            .batch_execute(
                r#"INSERT INTO "Sm2TiItem" ("Sm2TiItem_id", "Name", "MediaFilePath", "Sm2TiTrack_id")
                   SELECT gen_random_uuid(), 'Text+', '', "Sm2TiTrack_id" FROM "Sm2TiTrack" LIMIT 1"#,
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::testing::database;

    fn clip(path: &str) -> Clip {
        Clip {
            path: path.into(),
            size: 100,
            ..Clip::default()
        }
    }

    #[test]
    fn reads_projects_timelines_and_their_files() {
        let Some(cfg) = database("resolve_read") else {
            return;
        };
        let mut client = connect_rw(&cfg);
        testing::fill(
            &mut client,
            &[
                (
                    "Old edit",
                    100,
                    &[("Timeline 1", &[(true, "/old/GX010001.MP4", 0, 50, 86400)])],
                ),
                (
                    "Lower A-line",
                    200,
                    &[
                        (
                            "YouTube",
                            &[
                                (true, "/f/b.MP4.mov", 10, 20, 86420),
                                (true, "/f/a.MP4.mov", 262, 33, 86400),
                                (false, "/f/a.MP4.mov", 262, 33, 86400),
                                (false, "/music/song.wav", 0, 900, 86400),
                            ],
                        ),
                        ("Empty", &[]),
                    ],
                ),
            ],
        );
        let projects = read(&cfg).unwrap();
        assert_eq!(
            projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["Lower A-line", "Old edit"]
        );
        let youtube = projects[0]
            .timelines
            .iter()
            .find(|t| t.name == "YouTube")
            .unwrap();
        assert_eq!(
            youtube.files(),
            [
                Path::new("/f/a.MP4.mov"),
                Path::new("/f/b.MP4.mov"),
                Path::new("/music/song.wav")
            ]
        );
        assert_eq!(
            youtube.on_timeline(),
            [
                OnTimeline {
                    path: "/f/a.MP4.mov".into(),
                    first_frame: 262,
                    frames: 33
                },
                OnTimeline {
                    path: "/f/b.MP4.mov".into(),
                    first_frame: 10,
                    frames: 20
                },
            ]
        );
        assert!(
            projects[0]
                .timelines
                .iter()
                .any(|t| t.name == "Empty" && t.items.is_empty())
        );
        assert_eq!(projects[1].modified, Some(100));
    }

    #[test]
    fn says_what_is_missing_from_an_unexpected_layout() {
        let Some(cfg) = database("resolve_layout") else {
            return;
        };
        let mut client = connect_rw(&cfg);
        client
            .batch_execute(
                r#"CREATE TABLE "SM_Project" ("SM_Project_id" uuid, "ProjectName" varchar)"#,
            )
            .unwrap();
        let e = read(&cfg).unwrap_err().to_string();
        assert!(e.contains("SM_Project.LastModTimeInSecs"), "{e}");
        assert!(e.contains("Sm2TiItem"), "{e}");
    }

    #[test]
    fn finds_resolve_databases() {
        let Some(cfg) = database("resolve_find") else {
            return;
        };
        let Some(other) = database("resolve_find_other") else {
            return;
        };
        let mut client = connect_rw(&other);
        testing::fill(&mut client, &[]);
        let found = databases(&cfg).unwrap();
        assert!(found.contains(&other.database), "{found:?}");
        assert!(!found.contains(&cfg.database));
        assert_eq!(pick(&cfg, Some(" named ")).unwrap().database, "named");
    }

    fn connect_rw(cfg: &Config) -> postgres::Client {
        let mut c = postgres::Config::new();
        c.host(&cfg.host)
            .port(cfg.port)
            .dbname(&cfg.database)
            .user(&cfg.user)
            .password(&cfg.password);
        c.connect(postgres::NoTls).unwrap()
    }

    #[test]
    fn matches_files_to_clips_without_the_file_system() {
        let roots = Roots {
            nas: Some("/nas/footage".into()),
            nas_online: true,
            local: Some("/home/me/footage".into()),
            ..Roots::default()
        };
        let clips = vec![
            clip("/nas/footage/2026/2026-06-16/GX010042.MP4"),
            clip("/nas/footage/2026/2026-06-17/GX010001.MP4"),
            clip("/nas/footage/2026/2026-06-18/GX010001.MP4"),
            clip("/home/me/other/solo.mp4"),
        ];
        let m = Matcher::new(&clips, &roots);
        let copy = roots.local_copy(&clips[0].path).unwrap();
        assert_eq!(m.find(&copy).unwrap().path, clips[0].path);
        assert_eq!(m.find(&clips[0].path).unwrap().path, clips[0].path);
        // Under an old name: the camera name, when only one clip has it.
        assert_eq!(
            m.find(Path::new("/old/DATE/UUID/clips/f-~47~58010042.MP4.mov"))
                .map(|c| &c.path),
            Some(&clips[0].path)
        );
        // Two clips share the name: no guess.
        assert!(m.find(Path::new("/old/GX010001.MP4")).is_none());

        let timeline = Timeline {
            name: "t".into(),
            modified: None,
            items: [
                copy.to_str().unwrap(),
                "/home/me/other/solo.mp4",
                "/old/GX010001.MP4",
                "/home/me/footage/x.overlay.mov",
            ]
            .iter()
            .map(|p| TimelineItem {
                path: p.into(),
                video: true,
                first_frame: 0,
                frames: 1,
            })
            .collect(),
        };
        let here: HashSet<PathBuf> = ["/home/me/other/solo.mp4".into()].into();
        let f = footage(&timeline, &m, &|p| here.contains(p));
        assert_eq!(
            f,
            Footage {
                files: 3,
                here: 1,
                on_nas: 1,
                elsewhere: 0,
                unknown: 1
            }
        );
        assert_eq!(f.text(), "1 on the NAS · 1 missing");
        assert!(!f.ready());

        let projects = vec![
            Project {
                name: "Now".into(),
                modified: Some(1000),
                timelines: vec![timeline.clone()],
            },
            Project {
                name: "Long ago".into(),
                modified: Some(10),
                timelines: vec![timeline.clone()],
            },
            Project {
                name: "Published".into(),
                modified: Some(1000),
                timelines: vec![timeline],
            },
        ];
        let skip = [("Published".to_owned(), "t".to_owned())].into();
        let used = in_use(&projects, &m, 500, &skip);
        assert_eq!(
            used.get(&crate::organise::key(&clips[0])),
            Some(&vec!["Now".to_owned()])
        );
        assert_eq!(used.len(), 2);
    }
}

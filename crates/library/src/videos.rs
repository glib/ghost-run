//! Videos (decision 0005): a video is one Resolve timeline, the one rendered
//! for YouTube. Ghost Run keeps its title, kind, status (planned → in Resolve
//! → published, with the link and date typed in by hand), the clips that go
//! in it, in order, with the part of each and whether its timer overlay goes
//! on, and its **usage**: the clip ranges in it, recorded when Ghost Run
//! builds it (`built`) and, later, read back from the final timeline
//! (`timeline`), which then counts instead.
//!
//! Clips are kept by key ([`crate::organise::key`]) and by path as last seen,
//! so a video still finds a clip that moved on the NAS after a rescan.
//!
//! A timeline made by hand can be linked as a video: what the bridge reads
//! from it is matched to catalogued clips ([`match_timeline`]). What clips
//! are used by ([`clip_uses`]) drives the Library's Unused / In a video /
//! Released views, and publishing offers to remove the local copies no
//! unfinished video needs ([`freeable`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};

use crate::store::{Clip, Library};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Planned,
    InResolve,
    Published,
}

impl Status {
    pub const ALL: [Status; 3] = [Status::Planned, Status::InResolve, Status::Published];

    pub fn text(self) -> &'static str {
        match self {
            Status::Planned => "planned",
            Status::InResolve => "in_resolve",
            Status::Published => "published",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Planned => "Planned",
            Status::InResolve => "In Resolve",
            Status::Published => "Published",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "in_resolve" => Status::InResolve,
            "published" => Status::Published,
            _ => Status::Planned,
        }
    }
}

/// A clip in a video: which part, and whether its run's timer goes on.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    /// The clip's key.
    pub clip: String,
    /// Where the clip was when it was added (or last found).
    pub path: PathBuf,
    /// The first frame of the clip used, and how many.
    pub first_frame: i64,
    pub frames: i64,
    /// The run whose timer overlay goes over it, if any.
    pub timer: Option<String>,
}

impl Item {
    /// The whole of `clip`, with no timer.
    pub fn whole(clip: &Clip) -> Self {
        Item {
            clip: crate::organise::key(clip),
            path: clip.path.clone(),
            first_frame: 0,
            frames: frames(clip),
            timer: None,
        }
    }
}

/// How many frames `clip` has, as catalogued.
pub fn frames(clip: &Clip) -> i64 {
    clip.info.as_ref().map_or(0, |i| {
        i.frames
            .or_else(|| i.duration_seconds.map(|d| (d * i.fps()).round() as i64))
            .unwrap_or(0)
    })
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Video {
    pub id: i64,
    pub title: String,
    /// `clips` (chosen clips), or a kind's name.
    pub kind: String,
    pub status: Status,
    /// The Resolve project and timeline it was built as, or linked to.
    pub project: Option<String>,
    pub timeline: Option<String>,
    pub link: Option<String>,
    /// When it was published (`YYYY-MM-DD`).
    pub published: Option<String>,
    pub items: Vec<Item>,
}

/// A stretch of a clip used in a video.
#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    pub video: i64,
    pub clip: String,
    pub first_frame: i64,
    pub frames: i64,
}

/// A clip on a Resolve timeline, as the bridge reads it: the file, and the
/// part of it used.
#[derive(Debug, Clone, PartialEq)]
pub struct OnTimeline {
    pub path: PathBuf,
    pub first_frame: i64,
    pub frames: i64,
}

/// Which catalogued clip each timeline item is (by path, its local copy's
/// path, the same file under another spelling, or a file name only one clip
/// has), as items in timeline order with no timer; and the names of the
/// files that aren't in the catalog. Overlays are left out.
pub fn match_timeline(clips: &[Clip], found: &[OnTimeline]) -> (Vec<Item>, Vec<String>) {
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    let (mut items, mut missing) = (Vec::new(), Vec::new());
    for f in found {
        let name = original_name(&f.path);
        if name.ends_with(".overlay.mov") {
            continue;
        }
        let here = canonical(&f.path);
        let by_path = clips.iter().find(|c| {
            c.path == f.path
                || c.local.as_deref() == Some(f.path.as_path())
                || (here.is_some()
                    && [Some(c.path.as_path()), c.local.as_deref()]
                        .into_iter()
                        .flatten()
                        .any(|p| canonical(p) == here))
        });
        let by_name = || {
            let mut named = clips.iter().filter(|c| original_name(&c.path) == name);
            match (named.next(), named.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        match by_path.or_else(by_name) {
            Some(c) => items.push(Item {
                first_frame: f.first_frame,
                frames: f.frames,
                ..Item::whole(c)
            }),
            None => missing.push(
                f.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        }
    }
    (items, missing)
}

/// A file's name in lower case, an editing copy's (`clip.MP4.mov`) as its
/// original's.
fn original_name(path: &Path) -> String {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    match name.strip_suffix(".mov") {
        Some(original) if original.contains('.') && !original.ends_with(".overlay") => {
            original.to_owned()
        }
        _ => name,
    }
}

/// Which kinds of video a clip is in.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Used {
    /// In one not published yet (planned or in its usage).
    pub unpublished: bool,
    /// In a published one's final cut.
    pub published: bool,
}

/// The clips a video counts as using: its items, and its usage, except that
/// a published video with usage counts only that (its final cut).
fn keys_of<'a>(v: &'a Video, usage: &'a HashMap<i64, Vec<Use>>) -> HashSet<&'a str> {
    let used: HashSet<&str> = usage
        .get(&v.id)
        .into_iter()
        .flatten()
        .map(|u| u.clip.as_str())
        .collect();
    if v.status == Status::Published && !used.is_empty() {
        return used;
    }
    used.into_iter()
        .chain(v.items.iter().map(|i| i.clip.as_str()))
        .collect()
}

/// Each clip key's videos, for the Library's views.
pub fn clip_uses(videos: &[Video], usage: &HashMap<i64, Vec<Use>>) -> HashMap<String, Used> {
    let mut out: HashMap<String, Used> = HashMap::new();
    for v in videos {
        for key in keys_of(v, usage) {
            let e = out.entry(key.to_owned()).or_default();
            if v.status == Status::Published {
                e.published = true;
            } else {
                e.unpublished = true;
            }
        }
    }
    out
}

/// The clips of `video` (all it was made from) that no other unpublished
/// video needs: their local copies can go once it's published.
pub fn freeable(video: i64, videos: &[Video], usage: &HashMap<i64, Vec<Use>>) -> HashSet<String> {
    let Some(v) = videos.iter().find(|v| v.id == video) else {
        return HashSet::new();
    };
    let needed: HashSet<&str> = videos
        .iter()
        .filter(|o| o.id != video && o.status != Status::Published)
        .flat_map(|o| keys_of(o, usage))
        .collect();
    usage
        .get(&video)
        .into_iter()
        .flatten()
        .map(|u| u.clip.as_str())
        .chain(v.items.iter().map(|i| i.clip.as_str()))
        .filter(|k| !needed.contains(k))
        .map(str::to_owned)
        .collect()
}

impl Library {
    pub(crate) fn create_video_tables(db: &rusqlite::Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS videos (
               id INTEGER PRIMARY KEY,
               title TEXT NOT NULL,
               kind TEXT NOT NULL DEFAULT 'clips',
               status TEXT NOT NULL DEFAULT 'planned',
               project TEXT,
               timeline TEXT,
               link TEXT,
               published TEXT,
               created_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS video_items (
               video INTEGER NOT NULL,
               position INTEGER NOT NULL,
               clip TEXT NOT NULL,
               path TEXT NOT NULL,
               first_frame INTEGER NOT NULL,
               frames INTEGER NOT NULL,
               timer TEXT,
               PRIMARY KEY (video, position)
             );
             CREATE TABLE IF NOT EXISTS video_usage (
               video INTEGER NOT NULL,
               source TEXT NOT NULL,
               clip TEXT NOT NULL,
               first_frame INTEGER NOT NULL,
               frames INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Every video, newest first.
    pub fn videos(&self) -> Result<Vec<Video>> {
        let mut stmt = self
            .db
            .prepare("SELECT id FROM videos ORDER BY created_ms DESC, id DESC")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| self.video(id)?.context("a video went missing"))
            .collect()
    }

    pub fn video(&self, id: i64) -> Result<Option<Video>> {
        let video = self
            .db
            .query_row(
                "SELECT title, kind, status, project, timeline, link, published
                 FROM videos WHERE id = ?1",
                [id],
                |r| {
                    Ok(Video {
                        id,
                        title: r.get(0)?,
                        kind: r.get(1)?,
                        status: Status::parse(&r.get::<_, String>(2)?),
                        project: r.get(3)?,
                        timeline: r.get(4)?,
                        link: r.get(5)?,
                        published: r.get(6)?,
                        items: Vec::new(),
                    })
                },
            )
            .optional()?;
        let Some(mut video) = video else {
            return Ok(None);
        };
        let mut stmt = self.db.prepare(
            "SELECT clip, path, first_frame, frames, timer FROM video_items
             WHERE video = ?1 ORDER BY position",
        )?;
        video.items = stmt
            .query_map([id], |r| {
                Ok(Item {
                    clip: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    first_frame: r.get(2)?,
                    frames: r.get(3)?,
                    timer: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(video))
    }

    /// A new planned video; returns its id.
    pub fn new_video(&self, title: &str, kind: &str) -> Result<i64> {
        let title = title.trim();
        anyhow::ensure!(!title.is_empty(), "a video needs a title");
        self.db.execute(
            "INSERT INTO videos (title, kind, created_ms) VALUES (?1, ?2, ?3)",
            params![title, kind, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Saves a video's title, status, Resolve names, link and date (not its
    /// items: [`Library::set_items`]).
    pub fn save_video(&self, v: &Video) -> Result<()> {
        anyhow::ensure!(!v.title.trim().is_empty(), "a video needs a title");
        self.db.execute(
            "UPDATE videos SET title = ?2, kind = ?3, status = ?4, project = ?5,
               timeline = ?6, link = ?7, published = ?8
             WHERE id = ?1",
            params![
                v.id,
                v.title.trim(),
                v.kind,
                v.status.text(),
                v.project,
                v.timeline,
                v.link,
                v.published
            ],
        )?;
        Ok(())
    }

    /// Replaces a video's clips.
    pub fn set_items(&self, video: i64, items: &[Item]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM video_items WHERE video = ?1", [video])?;
        for (position, i) in items.iter().enumerate() {
            tx.execute(
                "INSERT INTO video_items VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    video,
                    position as i64,
                    i.clip,
                    i.path.to_string_lossy(),
                    i.first_frame,
                    i.frames,
                    i.timer
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Adds clips to the end of a video, skipping ones already in it.
    pub fn add_items(&self, video: i64, items: &[Item]) -> Result<usize> {
        let mut all = self
            .video(video)?
            .context("that video isn't in the catalog any more")?
            .items;
        let before = all.len();
        for i in items {
            if !all.iter().any(|a| a.clip == i.clip) {
                all.push(i.clone());
            }
        }
        self.set_items(video, &all)?;
        Ok(all.len() - before)
    }

    pub fn delete_video(&self, id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM video_usage WHERE video = ?1", [id])?;
        tx.execute("DELETE FROM video_items WHERE video = ?1", [id])?;
        tx.execute("DELETE FROM videos WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a video's usage from `source` (`built` or `timeline`).
    pub fn set_usage(&self, video: i64, source: &str, uses: &[Use]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM video_usage WHERE video = ?1 AND source = ?2",
            params![video, source],
        )?;
        for u in uses {
            tx.execute(
                "INSERT INTO video_usage VALUES (?1, ?2, ?3, ?4, ?5)",
                params![video, source, u.clip, u.first_frame, u.frames],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The videos whose usage was read back from their timeline.
    pub fn read_back(&self) -> Result<HashSet<i64>> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT video FROM video_usage WHERE source = 'timeline'")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        Ok(ids)
    }

    /// Each video's usage: what was read from its timeline if that's been
    /// done, else what was built.
    pub fn usage(&self) -> Result<HashMap<i64, Vec<Use>>> {
        let mut stmt = self
            .db
            .prepare("SELECT video, source, clip, first_frame, frames FROM video_usage")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    Use {
                        video: r.get(0)?,
                        clip: r.get(2)?,
                        first_frame: r.get(3)?,
                        frames: r.get(4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let read: std::collections::HashSet<i64> = rows
            .iter()
            .filter(|(s, _)| s == "timeline")
            .map(|(_, u)| u.video)
            .collect();
        let mut by_video: HashMap<i64, Vec<Use>> = HashMap::new();
        for (source, u) in rows {
            let wanted = if read.contains(&u.video) {
                "timeline"
            } else {
                "built"
            };
            if source == wanted {
                by_video.entry(u.video).or_default().push(u);
            }
        }
        Ok(by_video)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(path: &str, local: Option<&str>) -> Clip {
        Clip {
            path: path.into(),
            local: local.map(Into::into),
            size: 7,
            ..Clip::default()
        }
    }

    #[test]
    fn timeline_items_are_matched_to_catalogued_clips() {
        let clips = [
            clip("/nas/a/DJI_0001.MP4", Some("/edit/a/DJI_0001.MP4.mov")),
            clip("/nas/a/DJI_0002.MP4", None),
            clip("/nas/b/GX0003.MP4", None),
            clip("/nas/c/GX0003.MP4", None),
        ];
        let on = |path: &str, first, frames| OnTimeline {
            path: path.into(),
            first_frame: first,
            frames,
        };
        let (items, missing) = match_timeline(
            &clips,
            &[
                on("/edit/a/DJI_0001.MP4.mov", 30, 200),
                on("/var/home/me/nas/a/DJI_0002.MP4", 0, 90),
                on("/edit/a/Overlays/DJI_0001.overlay.mov", 0, 200),
                on("/somewhere/GX0003.MP4", 0, 10),
                on("/edit/DJI_0001.MP4.mov", 300, 60),
                on("/music/song.wav", 0, 10),
            ],
        );
        let got: Vec<(&str, i64, i64)> = items
            .iter()
            .map(|i| (i.clip.as_str(), i.first_frame, i.frames))
            .collect();
        assert_eq!(
            got,
            [
                ("dji_0001.mp4.7", 30, 200),
                ("dji_0002.mp4.7", 0, 90),
                ("dji_0001.mp4.7", 300, 60)
            ]
        );
        assert_eq!(items[0].path, PathBuf::from("/nas/a/DJI_0001.MP4"));
        // Two clips share GX0003.MP4: not guessed.
        assert_eq!(missing, ["GX0003.MP4", "song.wav"]);
    }

    #[test]
    fn uses_and_what_publishing_frees() {
        let video = |id, status, clips: &[&str]| Video {
            id,
            status,
            items: clips.iter().map(|c| item(c, None)).collect(),
            ..Video::default()
        };
        let videos = [
            video(1, Status::Published, &["a", "b", "c"]),
            video(2, Status::InResolve, &["c", "d"]),
            video(3, Status::Published, &["e"]),
        ];
        // Video 1's final cut kept only a and b.
        let usage = HashMap::from([(
            1,
            ["a", "b"]
                .map(|c| Use {
                    video: 1,
                    clip: c.into(),
                    first_frame: 0,
                    frames: 10,
                })
                .to_vec(),
        )]);
        let uses = clip_uses(&videos, &usage);
        assert_eq!(
            uses["a"],
            Used {
                unpublished: false,
                published: true
            }
        );
        assert!(!uses["c"].published && uses["c"].unpublished);
        assert!(uses["e"].published);
        assert!(!uses.contains_key("z"));
        let mut freed: Vec<String> = freeable(1, &videos, &usage).into_iter().collect();
        freed.sort();
        assert_eq!(freed, ["a", "b"]);
        assert!(freeable(9, &videos, &usage).is_empty());
    }

    fn item(clip: &str, timer: Option<&str>) -> Item {
        Item {
            clip: clip.into(),
            path: format!("/nas/{clip}").into(),
            first_frame: 0,
            frames: 300,
            timer: timer.map(Into::into),
        }
    }

    #[test]
    fn keeps_videos_their_clips_and_usage() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        assert!(lib.new_video("  ", "clips").is_err());
        let a = lib.new_video("Whistler top to bottom", "clips").unwrap();
        let b = lib.new_video("A-Line progression", "speed").unwrap();
        lib.set_items(a, &[item("x.mp4.1", Some("r1")), item("y.mp4.2", None)])
            .unwrap();
        assert_eq!(
            lib.add_items(a, &[item("y.mp4.2", None), item("z.mp4.3", None)])
                .unwrap(),
            1
        );
        let mut v = lib.video(a).unwrap().unwrap();
        assert_eq!(
            v.items.iter().map(|i| i.clip.as_str()).collect::<Vec<_>>(),
            ["x.mp4.1", "y.mp4.2", "z.mp4.3"]
        );
        assert_eq!(v.items[0].timer.as_deref(), Some("r1"));
        v.status = Status::Published;
        v.link = Some("https://youtu.be/abc".into());
        v.published = Some("2026-11-27".into());
        lib.save_video(&v).unwrap();
        let titles: Vec<String> = lib.videos().unwrap().into_iter().map(|v| v.title).collect();
        assert_eq!(titles, ["A-Line progression", "Whistler top to bottom"]);
        assert_eq!(lib.video(a).unwrap().unwrap().status, Status::Published);

        let used = |clip: &str, first, frames| Use {
            video: a,
            clip: clip.into(),
            first_frame: first,
            frames,
        };
        lib.set_usage(
            a,
            "built",
            &[used("x.mp4.1", 0, 300), used("y.mp4.2", 0, 300)],
        )
        .unwrap();
        assert_eq!(lib.usage().unwrap()[&a].len(), 2);
        // What the final timeline kept counts instead.
        lib.set_usage(a, "timeline", &[used("x.mp4.1", 40, 100)])
            .unwrap();
        assert_eq!(lib.usage().unwrap()[&a], [used("x.mp4.1", 40, 100)]);

        lib.delete_video(a).unwrap();
        assert!(lib.video(a).unwrap().is_none());
        assert!(lib.usage().unwrap().is_empty());
        assert_eq!(lib.videos().unwrap().len(), 1);
        assert_eq!(lib.video(b).unwrap().unwrap().kind, "speed");
    }
}

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

use std::collections::HashMap;
use std::path::PathBuf;

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

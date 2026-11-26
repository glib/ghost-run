//! What only Liam can say about the footage (decision 0005's catalog): rides
//! (clips grouped by when they were recorded, with a name), tags, pick/reject
//! flags and collections across rides.
//!
//! These are kept by each clip's **key** (its file name and size, as the
//! catalog's previews are), not its path, so they stay with a clip that moves
//! on the NAS. Rides aren't stored: they're worked out from recording times
//! each time, and only their names are kept, against the ride's clips.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;
use chrono::{Duration, NaiveDateTime};
use rusqlite::{OptionalExtension, params};

use crate::store::{Clip, Library};

/// A gap longer than this between one clip's end and the next one's start
/// starts a new ride.
pub const RIDE_GAP: Duration = Duration::hours(3);

/// A clip's key: its lower-case file name and size.
pub fn key(clip: &Clip) -> String {
    format!("{}.{}", clip.name().to_lowercase(), clip.size)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Pick,
    Reject,
}

impl Flag {
    fn text(self) -> &'static str {
        match self {
            Flag::Pick => "pick",
            Flag::Reject => "reject",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "pick" => Some(Flag::Pick),
            "reject" => Some(Flag::Reject),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    /// Its clips' keys.
    pub clips: HashSet<String>,
}

/// Everything here, read at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub tags: HashMap<String, BTreeSet<String>>,
    pub flags: HashMap<String, Flag>,
    /// By name.
    pub collections: Vec<Collection>,
    /// Clip key → the name of the ride it's in (kept on one clip per ride).
    pub ride_names: HashMap<String, String>,
    /// By name (`crate::features`).
    pub features: Vec<crate::features::Feature>,
}

impl Catalog {
    /// Every tag in use, sorted, with how many clips have it.
    pub fn all_tags(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for tags in self.tags.values() {
            for t in tags {
                *counts.entry(t).or_default() += 1;
            }
        }
        let mut all: Vec<(String, usize)> =
            counts.into_iter().map(|(t, n)| (t.to_owned(), n)).collect();
        all.sort_by_key(|(t, _)| t.to_lowercase());
        all
    }

    pub fn tags_of(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.tags.get(key).filter(|t| !t.is_empty())
    }
}

/// Clips recorded close together.
#[derive(Debug, Clone, PartialEq)]
pub struct Ride {
    /// Its first clip's key: what the ride is known by in the panel.
    pub id: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// Keys, in recording order.
    pub clips: Vec<String>,
    /// As named, if it has been.
    pub name: Option<String>,
}

impl Ride {
    /// Its name, else when it was: "Sun 24 Nov 2026, 09:30".
    pub fn title(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.start.format("%a %-d %b %Y, %H:%M").to_string())
    }
}

fn recorded(clip: &Clip) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(clip.recorded.as_deref()?, "%Y-%m-%d %H:%M:%S").ok()
}

fn seconds(clip: &Clip) -> f64 {
    clip.info.as_ref().map_or(0.0, |i| {
        i.duration_seconds
            .or_else(|| i.frames.map(|f| f as f64 / i.fps().max(1.0)))
            .unwrap_or(0.0)
    })
}

/// The rides `clips` make, newest first. Clips with no recording time are in
/// none.
pub fn rides(clips: &[Clip], names: &HashMap<String, String>) -> Vec<Ride> {
    let mut timed: Vec<(NaiveDateTime, NaiveDateTime, String)> = clips
        .iter()
        .filter_map(|c| {
            let start = recorded(c)?;
            let end = start + Duration::milliseconds((seconds(c) * 1000.0) as i64);
            Some((start, end, key(c)))
        })
        .collect();
    timed.sort();
    let mut rides: Vec<Ride> = Vec::new();
    for (start, end, key) in timed {
        match rides.last_mut() {
            Some(ride) if start - ride.end <= RIDE_GAP => {
                ride.end = ride.end.max(end);
                ride.clips.push(key);
            }
            _ => rides.push(Ride {
                id: key.clone(),
                start,
                end,
                clips: vec![key],
                name: None,
            }),
        }
    }
    for ride in &mut rides {
        ride.name = ride.clips.iter().find_map(|k| names.get(k)).cloned();
    }
    rides.reverse();
    rides
}

impl Library {
    pub(crate) fn create_organise_tables(db: &rusqlite::Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS clip_tags (
               clip TEXT NOT NULL,
               tag TEXT NOT NULL,
               PRIMARY KEY (clip, tag)
             );
             CREATE TABLE IF NOT EXISTS clip_flags (
               clip TEXT PRIMARY KEY,
               flag TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS collections (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS collection_clips (
               collection INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
               clip TEXT NOT NULL,
               PRIMARY KEY (collection, clip)
             );
             CREATE TABLE IF NOT EXISTS ride_names (
               clip TEXT PRIMARY KEY,
               name TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    pub fn catalog(&self) -> Result<Catalog> {
        let mut c = Catalog::default();
        let mut stmt = self.db.prepare("SELECT clip, tag FROM clip_tags")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
            let (clip, tag) = row?;
            c.tags.entry(clip).or_default().insert(tag);
        }
        let mut stmt = self.db.prepare("SELECT clip, flag FROM clip_flags")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (clip, flag) = row?;
            if let Some(flag) = Flag::parse(&flag) {
                c.flags.insert(clip, flag);
            }
        }
        let mut stmt = self
            .db
            .prepare("SELECT id, name FROM collections ORDER BY name COLLATE NOCASE")?;
        let collections = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, name) in collections {
            let mut stmt = self
                .db
                .prepare("SELECT clip FROM collection_clips WHERE collection = ?1")?;
            let clips = stmt
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            c.collections.push(Collection { id, name, clips });
        }
        let mut stmt = self.db.prepare("SELECT clip, name FROM ride_names")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
            let (clip, name) = row?;
            c.ride_names.insert(clip, name);
        }
        c.features = self.features()?;
        Ok(c)
    }

    /// Flags these clips, or clears their flag.
    pub fn set_flag(&self, clips: &[String], flag: Option<Flag>) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            match flag {
                Some(f) => tx.execute(
                    "INSERT OR REPLACE INTO clip_flags VALUES (?1, ?2)",
                    params![clip, f.text()],
                )?,
                None => tx.execute("DELETE FROM clip_flags WHERE clip = ?1", [clip])?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_tag(&self, clips: &[String], tag: &str) -> Result<()> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute(
                "INSERT OR IGNORE INTO clip_tags VALUES (?1, ?2)",
                params![clip, tag],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_tag(&self, clips: &[String], tag: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute(
                "DELETE FROM clip_tags WHERE clip = ?1 AND tag = ?2",
                params![clip, tag],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The collection called `name` (any case), made if there's none.
    pub fn collection(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "a collection needs a name");
        let found: Option<i64> = self
            .db
            .query_row(
                "SELECT id FROM collections WHERE name = ?1 COLLATE NOCASE",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            return Ok(id);
        }
        self.db
            .execute("INSERT INTO collections (name) VALUES (?1)", [name])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn rename_collection(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "a collection needs a name");
        self.db.execute(
            "UPDATE collections SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_collection(&self, id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM collection_clips WHERE collection = ?1", [id])?;
        tx.execute("DELETE FROM collections WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_to_collection(&self, id: i64, clips: &[String]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute(
                "INSERT OR IGNORE INTO collection_clips VALUES (?1, ?2)",
                params![id, clip],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_from_collection(&self, id: i64, clips: &[String]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute(
                "DELETE FROM collection_clips WHERE collection = ?1 AND clip = ?2",
                params![id, clip],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Names the ride made of these clips (an empty name clears it). The name
    /// is kept on its first clip only.
    pub fn name_ride(&self, ride: &Ride, name: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in &ride.clips {
            tx.execute("DELETE FROM ride_names WHERE clip = ?1", [clip])?;
        }
        if !name.trim().is_empty() {
            tx.execute(
                "INSERT INTO ride_names VALUES (?1, ?2)",
                params![ride.clips[0], name.trim()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MediaInfo;

    fn clip(name: &str, recorded: &str, seconds: f64) -> Clip {
        Clip {
            path: format!("/nas/{name}").into(),
            size: 100,
            recorded: Some(recorded.into()),
            info: Some(MediaInfo {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                fps_num: 30,
                fps_den: 1,
                frames: None,
                duration_seconds: Some(seconds),
                timecode: None,
                rotation: 0,
                creation_time: None,
                has_audio: true,
            }),
            ..Clip::default()
        }
    }

    #[test]
    fn rides_split_at_a_three_hour_gap_newest_first() {
        let clips = [
            clip("A.MP4", "2026-11-24 09:00:00", 600.0),
            clip("B.MP4", "2026-11-24 12:00:00", 60.0), // 2h50 after A ends
            clip("C.MP4", "2026-11-24 15:01:01", 60.0), // 3h00m01 after B ends
            clip("D.MP4", "2026-11-25 08:00:00", 60.0),
            Clip {
                recorded: None,
                ..clip("E.MP4", "", 1.0)
            },
        ];
        let names = [("c.mp4.100".to_owned(), "Afternoon laps".to_owned())].into();
        let rides = rides(&clips, &names);
        let groups: Vec<Vec<&str>> = rides
            .iter()
            .map(|r| r.clips.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            groups,
            [
                vec!["d.mp4.100"],
                vec!["c.mp4.100"],
                vec!["a.mp4.100", "b.mp4.100"]
            ]
        );
        assert_eq!(rides[1].title(), "Afternoon laps");
        assert_eq!(rides[2].title(), "Tue 24 Nov 2026, 09:00");
        assert_eq!(rides[2].id, "a.mp4.100");
    }

    #[test]
    fn keeps_tags_flags_collections_and_ride_names() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let ab = ["a.mp4.1".to_owned(), "b.mp4.2".to_owned()];
        lib.add_tag(&ab, "gear: Fox helmet").unwrap();
        lib.add_tag(&ab[..1], "wet").unwrap();
        lib.add_tag(&ab[..1], "  ").unwrap();
        lib.remove_tag(&ab[1..], "gear: Fox helmet").unwrap();
        lib.set_flag(&ab, Some(Flag::Pick)).unwrap();
        lib.set_flag(&ab[1..], Some(Flag::Reject)).unwrap();
        let race = lib.collection("Race build-up").unwrap();
        assert_eq!(lib.collection("race BUILD-up").unwrap(), race);
        lib.add_to_collection(race, &ab).unwrap();
        lib.remove_from_collection(race, &ab[..1]).unwrap();
        let ride = Ride {
            id: ab[0].clone(),
            start: NaiveDateTime::default(),
            end: NaiveDateTime::default(),
            clips: ab.to_vec(),
            name: None,
        };
        lib.name_ride(&ride, "Squamish").unwrap();
        lib.name_ride(&ride, "Squamish day").unwrap();

        let c = lib.catalog().unwrap();
        assert_eq!(
            c.tags_of("a.mp4.1").unwrap().iter().collect::<Vec<_>>(),
            ["gear: Fox helmet", "wet"]
        );
        assert_eq!(c.tags_of("b.mp4.2"), None);
        assert_eq!(
            c.all_tags(),
            [("gear: Fox helmet".to_owned(), 1), ("wet".to_owned(), 1)]
        );
        assert_eq!(c.flags["a.mp4.1"], Flag::Pick);
        assert_eq!(c.flags["b.mp4.2"], Flag::Reject);
        assert_eq!(c.collections.len(), 1);
        assert_eq!(
            c.collections[0].clips,
            HashSet::from(["b.mp4.2".to_owned()])
        );
        assert_eq!(c.ride_names.len(), 1);
        assert_eq!(c.ride_names["a.mp4.1"], "Squamish day");

        lib.set_flag(&ab, None).unwrap();
        lib.rename_collection(race, "Race").unwrap();
        lib.delete_collection(race).unwrap();
        let c = lib.catalog().unwrap();
        assert!(c.flags.is_empty() && c.collections.is_empty());
    }
}

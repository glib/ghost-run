//! Features (decision 0005's feature progression): a spot on a trail, a jump
//! or a drop, and the clips that are attempts at it, each landed, crashed or
//! not said. Kept by clip key, like tags (`crate::organise`), so they stay
//! with a clip that moves on the NAS. A clip is an attempt at one feature at
//! most.

use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

use crate::store::Library;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Landed,
    Crashed,
    /// Not said (yet).
    Unknown,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Outcome::Landed => "landed",
            Outcome::Crashed => "crashed",
            Outcome::Unknown => "",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "landed" => Outcome::Landed,
            "crashed" => Outcome::Crashed,
            _ => Outcome::Unknown,
        }
    }

    /// "Landed", "Crashed", or nothing.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Landed => "Landed",
            Outcome::Crashed => "Crashed",
            Outcome::Unknown => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// The clip's key.
    pub clip: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub id: i64,
    pub name: String,
    /// In no particular order: kinds put them in the order recorded.
    pub attempts: Vec<Attempt>,
}

impl Feature {
    pub fn attempt(&self, clip: &str) -> Option<&Attempt> {
        self.attempts.iter().find(|a| a.clip == clip)
    }
}

impl Library {
    pub(crate) fn create_feature_tables(db: &rusqlite::Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS features (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS feature_attempts (
               clip TEXT PRIMARY KEY,
               feature INTEGER NOT NULL REFERENCES features(id) ON DELETE CASCADE,
               outcome TEXT NOT NULL DEFAULT ''
             );",
        )?;
        Ok(())
    }

    /// Every feature, by name, with its attempts.
    pub fn features(&self) -> Result<Vec<Feature>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, name FROM features ORDER BY name COLLATE NOCASE")?;
        let mut features = stmt
            .query_map([], |r| {
                Ok(Feature {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    attempts: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = self
            .db
            .prepare("SELECT clip, feature, outcome FROM feature_attempts ORDER BY clip")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (clip, feature, outcome) = row?;
            if let Some(f) = features.iter_mut().find(|f| f.id == feature) {
                f.attempts.push(Attempt {
                    clip,
                    outcome: Outcome::parse(&outcome),
                });
            }
        }
        Ok(features)
    }

    /// The feature called `name` (any case), made if it's new; its id.
    pub fn feature(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        ensure!(!name.is_empty(), "a feature needs a name");
        let found: Option<i64> = self
            .db
            .query_row(
                "SELECT id FROM features WHERE name = ?1 COLLATE NOCASE",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            return Ok(id);
        }
        self.db
            .execute("INSERT INTO features (name) VALUES (?1)", [name])?;
        Ok(self.db.last_insert_rowid())
    }

    /// Makes these clips attempts at `feature` with this outcome (moving them
    /// from another feature if they were attempts at one).
    pub fn set_attempts(&self, feature: i64, clips: &[String], outcome: Outcome) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute(
                "INSERT INTO feature_attempts (clip, feature, outcome) VALUES (?1, ?2, ?3)
                 ON CONFLICT (clip) DO UPDATE SET feature = ?2, outcome = ?3",
                params![clip, feature, outcome.text()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// These clips aren't attempts at anything.
    pub fn remove_attempts(&self, clips: &[String]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for clip in clips {
            tx.execute("DELETE FROM feature_attempts WHERE clip = ?1", [clip])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rename_feature(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        ensure!(!name.is_empty(), "a feature needs a name");
        self.db.execute(
            "UPDATE features SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Deletes a feature; its clips stay in the Library.
    pub fn delete_feature(&self, id: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM feature_attempts WHERE feature = ?1", [id])?;
        tx.execute("DELETE FROM features WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_belong_to_one_feature_each() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let drop = lib.feature("Big drop").unwrap();
        assert_eq!(lib.feature(" big DROP ").unwrap(), drop);
        let gap = lib.feature("Road gap").unwrap();
        let ab = ["a.mp4.1".to_owned(), "b.mp4.2".to_owned()];
        lib.set_attempts(drop, &ab, Outcome::Crashed).unwrap();
        lib.set_attempts(drop, &ab[1..], Outcome::Landed).unwrap();
        lib.set_attempts(gap, &["c.mp4.3".to_owned()], Outcome::Unknown)
            .unwrap();
        let f = lib.features().unwrap();
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["Big drop", "Road gap"]
        );
        assert_eq!(f[0].attempt("a.mp4.1").unwrap().outcome, Outcome::Crashed);
        assert_eq!(f[0].attempt("b.mp4.2").unwrap().outcome, Outcome::Landed);

        // Moving one to the other feature.
        lib.set_attempts(gap, &ab[..1], Outcome::Landed).unwrap();
        lib.remove_attempts(&ab[1..]).unwrap();
        let f = lib.features().unwrap();
        assert!(f[0].attempts.is_empty());
        assert_eq!(f[1].attempts.len(), 2);

        lib.rename_feature(gap, "Road gap, big line").unwrap();
        lib.delete_feature(gap).unwrap();
        let f = lib.features().unwrap();
        assert_eq!(f.len(), 1);
        assert!(lib.feature("").is_err());
    }
}

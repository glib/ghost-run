//! Saving a database the app has edited, carefully: never over changes made
//! by someone else since it was loaded (the Resolve script writes the same
//! file), never into an invalid database, and always with a backup of what
//! was there.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::database::{DatabaseError, TimerDatabase};
use crate::validation::validate_database;

/// How many backups to keep beside the database.
pub const KEEP_BACKUPS: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error(
        "{0} changed since it was loaded (the Resolve script, or another copy of the app?); reload it and make the change again"
    )]
    ChangedOnDisk(String),
    #[error("the change would make the database invalid: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("could not back up the database: {0}")]
    Backup(std::io::Error),
    #[error(transparent)]
    Database(#[from] DatabaseError),
}

/// A database file and what it held when it was loaded.
#[derive(Debug, Clone)]
pub struct DatabaseFile {
    pub path: PathBuf,
    /// Hash of the file's bytes when loaded; `None` if it didn't exist.
    loaded: Option<String>,
    /// Problems the database already had (not blamed on an edit).
    known_errors: Vec<String>,
}

fn read_hash(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(format!("{:x}", Sha256::digest(bytes)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl DatabaseFile {
    pub fn load(path: impl Into<PathBuf>) -> Result<(Self, TimerDatabase), DatabaseError> {
        let path = path.into();
        let loaded = read_hash(&path).map_err(|e| DatabaseError::Write {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        let db = TimerDatabase::load(&path)?;
        let known_errors = validate_database(&db);
        Ok((
            Self {
                path,
                loaded,
                known_errors,
            },
            db,
        ))
    }

    /// Whether the file is still as it was loaded.
    pub fn unchanged(&self) -> bool {
        read_hash(&self.path).ok().as_ref() == Some(&self.loaded)
    }

    /// Writes `db` over the file, backing up the old one first.
    pub fn save(&mut self, db: &TimerDatabase) -> Result<(), SaveError> {
        if !self.unchanged() {
            return Err(SaveError::ChangedOnDisk(self.path.display().to_string()));
        }
        let new: Vec<String> = validate_database(db)
            .into_iter()
            .filter(|e| !self.known_errors.contains(e))
            .collect();
        if !new.is_empty() {
            return Err(SaveError::Invalid(new));
        }
        if self.loaded.is_some() {
            self.backup().map_err(SaveError::Backup)?;
        }
        db.save(&self.path)?;
        self.loaded = read_hash(&self.path).ok().flatten();
        self.known_errors = validate_database(db);
        Ok(())
    }

    /// `<dir>/<name>.backups/`, where the copies go.
    pub fn backup_dir(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".backups");
        self.path.with_file_name(name)
    }

    fn backup(&self) -> std::io::Result<()> {
        let dir = self.backup_dir();
        std::fs::create_dir_all(&dir)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
        std::fs::copy(&self.path, dir.join(format!("{stamp}.yaml")))?;
        let mut backups: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
            .collect();
        backups.sort();
        let excess = backups.len().saturating_sub(KEEP_BACKUPS);
        for old in &backups[..excess] {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit;

    const DB: &str =
        "schema_version: 2\ncourses:\n- id: a\n  name: A\n  sector_count: 1\nruns: []\n";

    #[test]
    fn saves_with_backups_and_refuses_lost_updates_and_invalid_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timer_db.yaml");
        std::fs::write(&path, DB).unwrap();
        let (mut file, mut db) = DatabaseFile::load(&path).unwrap();

        edit::add_course(&mut db, "B", 2).unwrap();
        file.save(&db).unwrap();
        assert!(TimerDatabase::load(&path).unwrap().courses.len() == 2);
        let backups: Vec<_> = std::fs::read_dir(file.backup_dir()).unwrap().collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].as_ref().unwrap().path()).unwrap(),
            DB
        );

        // Someone else writes the file: our next save must not overwrite it.
        std::fs::write(&path, DB).unwrap();
        edit::add_course(&mut db, "C", 1).unwrap();
        assert!(matches!(file.save(&db), Err(SaveError::ChangedOnDisk(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DB);

        // Reloaded, an edit that breaks the database is refused.
        let (mut file, mut db) = DatabaseFile::load(&path).unwrap();
        db.courses[0].sector_count = 0;
        assert!(matches!(file.save(&db), Err(SaveError::Invalid(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DB);
    }

    #[test]
    fn keeps_a_limited_number_of_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.yaml");
        std::fs::write(&path, DB).unwrap();
        let (mut file, db) = DatabaseFile::load(&path).unwrap();
        for _ in 0..KEEP_BACKUPS + 3 {
            file.save(&db).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            std::fs::read_dir(file.backup_dir()).unwrap().count(),
            KEEP_BACKUPS
        );
    }

    #[test]
    fn a_missing_file_is_created_without_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.yaml");
        let (mut file, mut db) = DatabaseFile::load(&path).unwrap();
        edit::add_course(&mut db, "A", 1).unwrap();
        file.save(&db).unwrap();
        assert!(path.exists() && !file.backup_dir().exists());
    }
}

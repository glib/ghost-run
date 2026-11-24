//! The YAML run database (schema v2; v1 files load and are saved as v2).
//! Shared with the Python tool, so it reads and writes the same files.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde_norway::{Mapping, Value};

use crate::models::{Course, RecordError, RunRecord, SCHEMA_VERSION};
use crate::py;
use crate::yaml;

pub const SUPPORTED_SCHEMA_VERSIONS: [i64; 2] = [1, SCHEMA_VERSION];

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("could not read database {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("could not parse database {path}: {message}")]
    Parse { path: String, message: String },
    #[error("database {path} must contain a YAML mapping")]
    NotMapping { path: String },
    #[error("unsupported schema_version {found}; expected one of 1, 2")]
    UnsupportedSchema { found: String },
    #[error("invalid database {path}: {message}")]
    Invalid { path: String, message: String },
    #[error("could not write database {path}: {message}")]
    Write { path: String, message: String },
    #[error("course not found: {0}")]
    CourseNotFound(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimerDatabase {
    pub courses: Vec<Course>,
    pub runs: Vec<RunRecord>,
}

impl TimerDatabase {
    /// Loads a database. A missing file is an empty database.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let path = path.as_ref();
        let shown = path.display().to_string();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(DatabaseError::Read {
                    path: shown,
                    source,
                });
            }
        };
        Self::from_yaml(&text, &shown)
    }

    /// Parses database text; `path` is only used in error messages.
    pub fn from_yaml(text: &str, path: &str) -> Result<Self, DatabaseError> {
        let raw: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_norway::from_str(text).map_err(|e| DatabaseError::Parse {
                path: path.into(),
                message: e.to_string(),
            })?
        };
        let raw = match raw {
            // PyYAML gives None for an empty document; Python then uses `{}`.
            Value::Null => Mapping::new(),
            Value::Mapping(m) => m,
            _ => return Err(DatabaseError::NotMapping { path: path.into() }),
        };

        // Python's `version in {1, 2}`: numerically equal (`2.0` and `true` count), not strings.
        let version = raw.get("schema_version");
        let numeric = match version {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        };
        if !numeric.is_some_and(|v| SUPPORTED_SCHEMA_VERSIONS.iter().any(|s| *s as f64 == v)) {
            return Err(DatabaseError::UnsupportedSchema {
                found: py::repr(version),
            });
        }

        let invalid = |e: RecordError| DatabaseError::Invalid {
            path: path.into(),
            message: e.to_string(),
        };
        let list = |key: &str| -> Result<Vec<Value>, DatabaseError> {
            match raw.get(key) {
                None => Ok(Vec::new()),
                Some(Value::Sequence(items)) => Ok(items.clone()),
                Some(other) => Err(DatabaseError::Invalid {
                    path: path.into(),
                    message: format!("{key} must be a list, not {}", py::repr(Some(other))),
                }),
            }
        };
        let courses = list("courses")?
            .iter()
            .map(Course::from_value)
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        let runs = list("runs")?
            .iter()
            .map(RunRecord::from_value)
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        Ok(Self { courses, runs })
    }

    /// The database as YAML text, in the Python tool's layout and key order.
    pub fn to_yaml(&self) -> String {
        let mut root = Mapping::new();
        root.insert("schema_version".into(), SCHEMA_VERSION.into());
        root.insert(
            "courses".into(),
            Value::Sequence(self.courses.iter().map(Course::to_value).collect()),
        );
        root.insert(
            "runs".into(),
            Value::Sequence(self.runs.iter().map(RunRecord::to_value).collect()),
        );
        yaml::emit(&Value::Mapping(root))
    }

    /// Saves atomically: writes `<name>.tmp` next to the file, then renames it
    /// over the target. The temporary file is removed if anything fails.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), DatabaseError> {
        let path = path.as_ref();
        let shown = path.display().to_string();
        let fail = |e: std::io::Error| DatabaseError::Write {
            path: shown.clone(),
            message: e.to_string(),
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(fail)?;
        }
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let result = (|| {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(self.to_yaml().as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(fail(e));
        }
        Ok(())
    }

    pub fn course_by_id(&self, course_id: &str) -> Result<&Course, DatabaseError> {
        self.courses
            .iter()
            .find(|c| c.id == course_id)
            .ok_or_else(|| DatabaseError::CourseNotFound(course_id.into()))
    }

    /// Replaces the run with the same id, or appends it.
    pub fn upsert_run(&mut self, run: RunRecord) {
        match self.runs.iter_mut().find(|r| r.id == run.id) {
            Some(existing) => *existing = run,
            None => self.runs.push(run),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::clip_fingerprint;
    use crate::models::MarkerFrames;

    fn course() -> Course {
        Course {
            id: "course".into(),
            name: "Course".into(),
            sector_count: 2,
        }
    }

    fn frames() -> MarkerFrames {
        [("Start", 0), ("S1", 100), ("Finish", 300)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect()
    }

    fn db() -> TimerDatabase {
        let mut run = RunRecord::new(
            "run_a",
            "course",
            "2026-05-31",
            "GX010123.MP4",
            100.0,
            frames(),
        );
        run.fingerprint = Some(clip_fingerprint("GX010123.MP4", &frames()));
        run.committed_at = Some("2026-05-31T10:00:00Z".into());
        TimerDatabase {
            courses: vec![course()],
            runs: vec![run],
        }
    }

    #[test]
    fn round_trip_keeps_courses_and_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timer_db.yaml");
        db().save(&path).unwrap();
        let loaded = TimerDatabase::load(&path).unwrap();
        assert_eq!(loaded.courses, [course()]);
        assert_eq!(loaded.runs[0].marker_frames, frames());
        assert_eq!(
            loaded.runs[0].fingerprint.as_deref(),
            Some(clip_fingerprint("GX010123.MP4", &frames()).as_str())
        );
        assert!(!dir.path().join("timer_db.yaml.tmp").exists());
    }

    #[test]
    fn loads_v1_and_saves_v2_with_capture_fallback() {
        let v1 = "schema_version: 1\ncourses:\n- id: course\n  name: Course\n  sector_count: 2\nruns:\n- id: run_a\n  course_id: course\n  date: '2026-05-31'\n  filename: GX010123.MP4\n  source_fps: 100.0\n  marker_frames:\n    Finish: 300\n    S1: 100\n    Start: 0\n";
        let database = TimerDatabase::from_yaml(v1, "db").unwrap();
        assert_eq!(
            database.runs[0].capture_time.as_deref(),
            Some("2026-05-31T23:59:59Z")
        );
        assert_eq!(
            database.runs[0].capture_time_source.as_deref(),
            Some("date_fallback")
        );
        let saved = database.to_yaml();
        assert!(saved.contains("schema_version: 2"), "{saved}");
        assert!(
            saved.contains("capture_time: '2026-05-31T23:59:59Z'"),
            "{saved}"
        );
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            TimerDatabase::load(dir.path().join("nope.yaml")).unwrap(),
            TimerDatabase::default()
        );
    }

    #[test]
    fn rejects_bad_files() {
        let err = |text: &str| {
            TimerDatabase::from_yaml(text, "db")
                .unwrap_err()
                .to_string()
        };
        assert!(err("- not\n- a\n- mapping\n").contains("must contain a YAML mapping"));
        assert!(err("schema_version: [unterminated\n").contains("could not parse database"));
        assert_eq!(
            err("courses: []\n"),
            "unsupported schema_version None; expected one of 1, 2"
        );
        assert_eq!(
            err("schema_version: '2'\n"),
            "unsupported schema_version '2'; expected one of 1, 2"
        );
        assert_eq!(
            err(""),
            "unsupported schema_version None; expected one of 1, 2"
        );
        assert!(err("schema_version: 2\nruns:\n- id: r\n").starts_with("invalid database db:"));
    }

    #[test]
    fn save_failure_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // The target is a directory, so the final rename fails.
        let path = dir.path().join("timer_db.yaml");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "x").unwrap();
        assert!(matches!(db().save(&path), Err(DatabaseError::Write { .. })));
        assert!(!dir.path().join("timer_db.yaml.tmp").exists());
    }
}

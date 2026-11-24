//! Committing a clip's markers as a run, as the Python tool's "Commit new
//! run" / "Update existing run" do: the same id scheme, capture time, date,
//! fingerprint and fields, so either tool can carry on from the other.

use std::path::Path;

use crate::capture_time::{
    self, FILENAME_TIMESTAMP_SOURCE, FILESYSTEM_CREATED_SOURCE, from_dji_filename,
};
use crate::database::TimerDatabase;
use crate::markers::{SnapshotError, parse_marker_snapshot};
use crate::matching::clip_fingerprint;
use crate::models::{MarkerFrames, RawMarker, RunRecord};
use crate::timing::{TimingError, compute_timing};

/// The clip being timed.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipInfo {
    pub filename: String,
    pub source_path: Option<String>,
    pub source_fps: f64,
    /// UTC ISO capture time, if known, and where it came from.
    pub capture_time: Option<String>,
    pub capture_time_source: Option<String>,
}

impl ClipInfo {
    /// The clip at `path`: capture time from a DJI file name (the camera's
    /// clock read as this computer's zone), else the file's ctime, as the
    /// Python tool and `rtimer` do. An editing copy (`clip.MP4.mov`) is known
    /// by its original's name, as Resolve's runs are.
    pub fn for_file(path: &Path, source_fps: f64) -> Self {
        let filename = original_name(
            &path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let (capture_time, source) = match from_dji_filename(&filename, &chrono::Local) {
            Some(t) => (Some(t), Some(FILENAME_TIMESTAMP_SOURCE)),
            None => match changed_time(path) {
                Some(t) => (Some(t), Some(FILESYSTEM_CREATED_SOURCE)),
                None => (None, None),
            },
        };
        Self {
            filename,
            source_path: Some(path.display().to_string()),
            source_fps,
            capture_time,
            capture_time_source: source.map(str::to_owned),
        }
    }
}

/// `clip.MP4.mov` → `clip.MP4`; other names as they are.
pub fn original_name(name: &str) -> String {
    match name
        .strip_suffix(".mov")
        .or_else(|| name.strip_suffix(".MOV"))
    {
        Some(original) if original.contains('.') => original.to_owned(),
        _ => name.to_owned(),
    }
}

fn changed_time(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let seconds = std::os::unix::fs::MetadataExt::ctime(&meta);
    #[cfg(not(unix))]
    let seconds = meta
        .created()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(
        chrono::DateTime::from_timestamp(seconds, 0)?
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("no course {0}")]
    NoCourse(String),
    #[error(transparent)]
    Markers(#[from] SnapshotError),
    #[error(transparent)]
    Timing(#[from] TimingError),
}

/// The stored run for this clip: one with the same source path, else the
/// same file name, on any course (the earliest committed if several).
pub fn existing_run<'a>(db: &'a TimerDatabase, clip: &ClipInfo) -> Option<&'a RunRecord> {
    let same_path = |r: &&RunRecord| {
        clip.source_path.is_some() && r.source_path.as_deref() == clip.source_path.as_deref()
    };
    db.runs
        .iter()
        .find(same_path)
        .or_else(|| db.runs.iter().find(|r| r.filename == clip.filename))
}

/// Writes the markers into `db` as a committed run, updating the clip's
/// existing run if it has one. Returns the run's id.
pub fn commit(
    db: &mut TimerDatabase,
    course_id: &str,
    clip: &ClipInfo,
    markers: &MarkerFrames,
    now_utc: &str,
) -> Result<String, CommitError> {
    let course = db
        .courses
        .iter()
        .find(|c| c.id == course_id)
        .ok_or_else(|| CommitError::NoCourse(course_id.into()))?
        .clone();
    let raw: Vec<RawMarker> = markers
        .iter()
        .map(|(n, f)| RawMarker::new(n.clone(), *f))
        .collect();
    let snapshot = parse_marker_snapshot(&raw, &course)?;
    compute_timing(&snapshot, &course, clip.source_fps)?;
    let fingerprint = clip_fingerprint(&clip.filename, &snapshot.frames);
    let captured = clip
        .capture_time
        .as_deref()
        .and_then(|t| capture_time::normalize(t).ok().flatten());

    if let Some(existing) = existing_run(db, clip).cloned() {
        let mut run = existing.clone();
        run.course_id = course.id.clone();
        run.filename = clip.filename.clone();
        run.source_fps = clip.source_fps;
        run.marker_frames = snapshot.frames.clone();
        // A real capture time replaces a missing or made-up one.
        if (run.capture_time.is_none()
            || run.capture_time_source.as_deref() == Some(capture_time::DATE_FALLBACK_SOURCE))
            && let Some(t) = &captured
        {
            run.capture_time = Some(t.clone());
            run.capture_time_source = clip.capture_time_source.clone();
            run.date = t[..10].to_owned();
        }
        run.source_path = clip.source_path.clone().or(existing.source_path);
        run.fingerprint = Some(fingerprint);
        run.committed = true;
        run.committed_at = Some(now_utc.to_owned());
        let id = run.id.clone();
        db.upsert_run(run);
        return Ok(id);
    }

    let date = match &captured {
        Some(t) => t[..10].to_owned(),
        None => chrono::Local::now().format("%Y-%m-%d").to_string(),
    };
    let capture = captured
        .clone()
        .or_else(|| capture_time::from_date(&date).ok().flatten());
    let source = match (&captured, &capture) {
        (Some(_), _) => clip.capture_time_source.clone(),
        (None, Some(_)) => Some(capture_time::DATE_FALLBACK_SOURCE.to_owned()),
        _ => None,
    };
    let id = next_run_id(db, &date);
    let mut run = RunRecord::new(
        id.clone(),
        course.id.clone(),
        date,
        clip.filename.clone(),
        clip.source_fps,
        snapshot.frames.clone(),
    );
    run.capture_time = capture;
    run.capture_time_source = source;
    run.source_path = clip.source_path.clone();
    run.fingerprint = Some(fingerprint);
    run.committed = true;
    run.ignored = false;
    run.committed_at = Some(now_utc.to_owned());
    db.runs.push(run);
    Ok(id)
}

/// `run_YYYY_MM_DD_NNN`, the first number not taken.
pub fn next_run_id(db: &TimerDatabase, date: &str) -> String {
    let stem = format!("run_{}", date.replace('-', "_"));
    (1..)
        .map(|i| format!("{stem}_{i:03}"))
        .find(|id| !db.runs.iter().any(|r| &r.id == id))
        .expect("an unused id")
}

/// Now, as the Python tool writes `committed_at`.
pub fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Course;

    fn frames(s1: i64, finish: i64) -> MarkerFrames {
        [("Start", 10), ("S1", s1), ("Finish", finish)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect()
    }

    fn db() -> TimerDatabase {
        TimerDatabase {
            courses: vec![Course {
                id: "c".into(),
                name: "C".into(),
                sector_count: 2,
            }],
            runs: vec![],
        }
    }

    fn clip(name: &str) -> ClipInfo {
        ClipInfo {
            filename: name.into(),
            source_path: Some(format!("/v/{name}")),
            source_fps: 29.97,
            capture_time: Some("2026-08-17T14:52:36Z".into()),
            capture_time_source: Some(FILENAME_TIMESTAMP_SOURCE.into()),
        }
    }

    #[test]
    fn commits_a_new_run_like_the_python_tool() {
        let mut d = db();
        let id = commit(
            &mut d,
            "c",
            &clip("a.MP4"),
            &frames(100, 200),
            "2026-11-24T10:00:00Z",
        )
        .unwrap();
        assert_eq!(id, "run_2026_08_17_001");
        let run = &d.runs[0];
        assert_eq!(run.date, "2026-08-17");
        assert_eq!(run.capture_time.as_deref(), Some("2026-08-17T14:52:36Z"));
        assert_eq!(
            run.capture_time_source.as_deref(),
            Some("filename_timestamp")
        );
        assert_eq!(
            run.fingerprint,
            Some(clip_fingerprint("a.MP4", &frames(100, 200)))
        );
        assert!(run.committed && !run.ignored);
        assert_eq!(run.committed_at.as_deref(), Some("2026-11-24T10:00:00Z"));
        assert!(crate::validation::validate_database(&d).is_empty());

        let id2 = commit(&mut d, "c", &clip("b.MP4"), &frames(100, 200), "t").unwrap();
        assert_eq!(id2, "run_2026_08_17_002");
    }

    #[test]
    fn updates_the_clips_existing_run() {
        let mut d = db();
        let id = commit(&mut d, "c", &clip("a.MP4"), &frames(100, 200), "t1").unwrap();
        d.runs[0].ignored = true;
        let again = commit(&mut d, "c", &clip("a.MP4"), &frames(120, 250), "t2").unwrap();
        assert_eq!(again, id);
        assert_eq!(d.runs.len(), 1);
        assert_eq!(d.runs[0].marker_frames, frames(120, 250));
        assert!(d.runs[0].ignored, "kept");
        assert_eq!(d.runs[0].committed_at.as_deref(), Some("t2"));
    }

    #[test]
    fn editing_copies_are_known_by_the_original() {
        assert_eq!(original_name("DJI_1.MP4.mov"), "DJI_1.MP4");
        assert_eq!(original_name("run.mov"), "run.mov");
        assert_eq!(original_name("a.MP4"), "a.MP4");
    }

    #[test]
    fn refuses_markers_that_dont_fit() {
        let mut d = db();
        let bad: MarkerFrames = [("Start", 10), ("Finish", 20)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect();
        assert!(commit(&mut d, "c", &clip("a.MP4"), &bad, "t").is_err());
        assert!(commit(&mut d, "c", &clip("a.MP4"), &frames(300, 200), "t").is_err());
        assert!(commit(&mut d, "zz", &clip("a.MP4"), &frames(100, 200), "t").is_err());
        assert!(d.runs.is_empty());
    }

    #[test]
    fn a_clip_without_a_time_is_dated_today_with_a_fallback_capture_time() {
        let mut d = db();
        let mut c = clip("a.MP4");
        c.capture_time = None;
        c.capture_time_source = None;
        commit(&mut d, "c", &c, &frames(100, 200), "t").unwrap();
        assert_eq!(
            d.runs[0].capture_time_source.as_deref(),
            Some("date_fallback")
        );
        assert!(crate::validation::validate_database(&d).is_empty());
    }
}

//! Database consistency checks (`validate-db`). Messages and their order match
//! the Python tool.

use std::collections::HashMap;

use crate::capture_time::{self, CAPTURE_TIME_SOURCES};
use crate::database::TimerDatabase;
use crate::markers::{SnapshotError, parse_marker_snapshot, raw_markers};
use crate::timing::compute_timing;

/// Ids that appear more than once, in order of first appearance.
fn duplicates<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut order = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for id in ids {
        let count = counts.entry(id).or_default();
        if *count == 0 {
            order.push(id);
        }
        *count += 1;
    }
    order.into_iter().filter(|id| counts[id] > 1).collect()
}

pub fn validate_database(database: &TimerDatabase) -> Vec<String> {
    let mut errors = Vec::new();

    for id in duplicates(database.courses.iter().map(|c| c.id.as_str())) {
        errors.push(format!("duplicate course id: {id}"));
    }
    // Later courses with the same id win, as in Python's dict.
    let courses: HashMap<&str, _> = database
        .courses
        .iter()
        .map(|c| (c.id.as_str(), c))
        .collect();
    for course in &database.courses {
        if course.id.trim().is_empty() {
            errors.push("course id is required".into());
        }
        if course.name.trim().is_empty() {
            errors.push(format!("course {} name is required", course.id));
        }
        if course.sector_count < 1 {
            errors.push(format!(
                "course {} sector_count must be at least 1",
                course.id
            ));
        }
    }

    for id in duplicates(database.runs.iter().map(|r| r.id.as_str())) {
        errors.push(format!("duplicate run id: {id}"));
    }

    for run in &database.runs {
        let Some(course) = courses.get(run.course_id.as_str()) else {
            errors.push(format!(
                "run {} references missing course {}",
                run.id, run.course_id
            ));
            continue;
        };
        if run.source_fps.is_nan() || run.source_fps <= 0.0 {
            errors.push(format!("run {} source_fps must be greater than 0", run.id));
            continue;
        }
        if let Some(source) = run.capture_time_source.as_deref()
            && !CAPTURE_TIME_SOURCES.contains(&source)
        {
            errors.push(format!(
                "run {} capture_time_source must be one of {}",
                run.id,
                CAPTURE_TIME_SOURCES.join(", ")
            ));
        }
        if let Some(t) = run.capture_time.as_deref()
            && let Err(e) = capture_time::normalize(t)
        {
            errors.push(format!("run {} capture_time is invalid: {e}", run.id));
        }
        match parse_marker_snapshot(&raw_markers(&run.marker_frames), course) {
            Err(SnapshotError::Markers(e)) => {
                errors.extend(e.errors.iter().map(|m| format!("run {}: {m}", run.id)));
            }
            Err(e) => errors.push(format!("run {}: {e}", run.id)),
            Ok(snapshot) => {
                if let Err(e) = compute_timing(&snapshot, course, run.source_fps) {
                    errors.push(format!("run {}: {e}", run.id));
                }
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Course, MarkerFrames, RunRecord};

    fn frames(items: &[(&str, i64)]) -> MarkerFrames {
        items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect()
    }

    fn course(id: &str, name: &str) -> Course {
        Course {
            id: id.into(),
            name: name.into(),
            sector_count: 2,
        }
    }

    fn run(id: &str, course_id: &str, fps: f64, f: MarkerFrames) -> RunRecord {
        RunRecord::new(id, course_id, "2026-05-31", "GX010123.MP4", fps, f)
    }

    #[test]
    fn reports_consistency_errors() {
        let a = frames(&[("Start", 0), ("S1", 100), ("Finish", 300)]);
        let valid = TimerDatabase {
            courses: vec![course("course", "Course")],
            runs: vec![run("run_a", "course", 100.0, a.clone())],
        };
        assert!(validate_database(&valid).is_empty());

        let invalid = TimerDatabase {
            courses: vec![
                course("course", "Course"),
                course("course", "Duplicate"),
                course("", "Blank ID"),
                course("blank_name", " "),
            ],
            runs: vec![
                run("run_a", "course", 100.0, a.clone()),
                run(
                    "run_a",
                    "course",
                    100.0,
                    frames(&[("Start", 0), ("Finish", 100)]),
                ),
                run("missing_course", "missing", 100.0, a.clone()),
                run("bad_fps", "course", 0.0, a),
            ],
        };
        assert_eq!(
            validate_database(&invalid),
            [
                "duplicate course id: course",
                "course id is required",
                "course blank_name name is required",
                "duplicate run id: run_a",
                "run run_a: missing marker S1",
                "run missing_course references missing course missing",
                "run bad_fps source_fps must be greater than 0",
            ]
        );
    }

    #[test]
    fn rejects_unknown_capture_time_source() {
        let mut r = run(
            "r",
            "course",
            100.0,
            frames(&[("Start", 0), ("S1", 1), ("Finish", 2)]),
        );
        r.capture_time_source = Some("guess".into());
        let db = TimerDatabase {
            courses: vec![course("course", "Course")],
            runs: vec![r],
        };
        assert_eq!(
            validate_database(&db),
            [
                "run r capture_time_source must be one of date_fallback, filename_timestamp, filesystem_created, manual"
            ]
        );
    }
}

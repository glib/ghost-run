//! Marker validation: a course with N sectors needs `Start`, `S1`…`S(N-1)` and
//! `Finish`, once each, in increasing frame order. Other markers are ignored.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::models::{Course, MarkerFrames, MarkerSnapshot, RawMarker};

static SECTOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^S([1-9][0-9]*)$").expect("valid regex"));

/// All problems found in a clip's markers, in the order the Python tool lists them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", .errors.join("; "))]
pub struct MarkerValidationError {
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("sector_count must be at least 1")]
pub struct InvalidSectorCount;

pub fn expected_marker_names(course: &Course) -> Result<Vec<String>, InvalidSectorCount> {
    if course.sector_count < 1 {
        return Err(InvalidSectorCount);
    }
    let mut names = vec!["Start".to_owned()];
    names.extend((1..course.sector_count).map(|i| format!("S{i}")));
    names.push("Finish".to_owned());
    Ok(names)
}

fn is_timing_marker(name: &str) -> bool {
    name == "Start" || name == "Finish" || SECTOR_RE.is_match(name)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Markers(#[from] MarkerValidationError),
    #[error(transparent)]
    SectorCount(#[from] InvalidSectorCount),
}

/// Validates a clip's markers for `course` and returns its timing frames.
pub fn parse_marker_snapshot(
    markers: &[RawMarker],
    course: &Course,
) -> Result<MarkerSnapshot, SnapshotError> {
    let expected = expected_marker_names(course)?;
    let timing: Vec<&RawMarker> = markers
        .iter()
        .filter(|m| is_timing_marker(&m.name))
        .collect();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for m in &timing {
        *counts.entry(m.name.as_str()).or_default() += 1;
    }

    let mut errors = Vec::new();
    for name in &expected {
        match counts.get(name.as_str()).copied().unwrap_or(0) {
            0 => errors.push(format!("missing marker {name}")),
            1 => {}
            _ => errors.push(format!("duplicate marker {name}")),
        }
    }
    let mut unexpected: Vec<&str> = counts
        .keys()
        .copied()
        .filter(|n| !expected.iter().any(|e| e == n))
        .collect();
    unexpected.sort_by_key(|n| sort_key(n));
    errors.extend(unexpected.iter().map(|n| format!("unexpected marker {n}")));
    if !errors.is_empty() {
        return Err(MarkerValidationError { errors }.into());
    }

    let frames: HashMap<&str, i64> = timing.iter().map(|m| (m.name.as_str(), m.frame)).collect();
    for pair in expected.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        let (pf, nf) = (frames[prev.as_str()], frames[next.as_str()]);
        if nf <= pf {
            errors.push(format!(
                "marker {next} frame {nf} must be after {prev} frame {pf}"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(MarkerValidationError { errors }.into());
    }

    Ok(MarkerSnapshot {
        frames: expected
            .iter()
            .map(|n| (n.clone(), frames[n.as_str()]))
            .collect::<MarkerFrames>(),
    })
}

/// Start, then sectors by number, then Finish, then anything else by name.
fn sort_key(name: &str) -> (u8, u64, String) {
    if name == "Start" {
        return (0, 0, String::new());
    }
    if let Some(c) = SECTOR_RE.captures(name) {
        return (1, c[1].parse().unwrap_or(u64::MAX), String::new());
    }
    if name == "Finish" {
        return (2, 0, String::new());
    }
    (3, 0, name.to_owned())
}

/// Markers from a stored run, for re-validating it.
pub fn raw_markers(frames: &MarkerFrames) -> Vec<RawMarker> {
    frames
        .iter()
        .map(|(n, f)| RawMarker::new(n.clone(), *f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn course() -> Course {
        Course {
            id: "course".into(),
            name: "Course".into(),
            sector_count: 4,
        }
    }

    fn markers(items: &[(&str, i64)]) -> Vec<RawMarker> {
        items.iter().map(|(n, f)| RawMarker::new(*n, *f)).collect()
    }

    fn errors(items: &[(&str, i64)]) -> Vec<String> {
        match parse_marker_snapshot(&markers(items), &course()) {
            Err(SnapshotError::Markers(e)) => e.errors,
            other => panic!("expected marker errors, got {other:?}"),
        }
    }

    #[test]
    fn missing_start() {
        let e = errors(&[("S1", 200), ("S2", 350), ("S3", 500), ("Finish", 700)]);
        assert!(e.contains(&"missing marker Start".to_owned()));
    }

    #[test]
    fn duplicate_marker() {
        let e = errors(&[
            ("Start", 100),
            ("S1", 200),
            ("S1", 210),
            ("S2", 350),
            ("S3", 500),
            ("Finish", 700),
        ]);
        assert!(e.contains(&"duplicate marker S1".to_owned()));
    }

    #[test]
    fn extra_sector_marker() {
        let e = errors(&[
            ("Start", 100),
            ("S1", 200),
            ("S2", 350),
            ("S3", 500),
            ("S4", 650),
            ("Finish", 700),
        ]);
        assert!(e.contains(&"unexpected marker S4".to_owned()));
    }

    #[test]
    fn out_of_order_marker() {
        let e = errors(&[
            ("Start", 100),
            ("S1", 200),
            ("S2", 190),
            ("S3", 500),
            ("Finish", 700),
        ]);
        assert!(e.contains(&"marker S2 frame 190 must be after S1 frame 200".to_owned()));
    }

    #[test]
    fn unexpected_markers_sorted_numerically_and_non_timing_ignored() {
        let e = errors(&[
            ("Start", 0),
            ("S1", 1),
            ("S2", 2),
            ("S3", 3),
            ("S10", 4),
            ("S4", 5),
            ("Lunch", 6),
            ("Finish", 7),
        ]);
        assert_eq!(e, ["unexpected marker S4", "unexpected marker S10"]);
    }
}

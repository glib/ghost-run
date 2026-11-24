//! Sector and lap times from marker frames: `seconds = frame_delta / source_fps`.

use crate::markers::{InvalidSectorCount, expected_marker_names};
use crate::models::{Course, MarkerSnapshot, SectorTiming, TimingResult};
use crate::py::round_half_even;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimingError {
    #[error("source_fps must be greater than 0")]
    InvalidFps,
    #[error("snapshot missing markers: {0}")]
    MissingMarkers(String),
    #[error("sector {0} duration must be positive")]
    NonPositiveSector(i64),
    #[error("lap duration must be positive")]
    NonPositiveLap,
    #[error(transparent)]
    SectorCount(#[from] InvalidSectorCount),
}

pub fn compute_timing(
    snapshot: &MarkerSnapshot,
    course: &Course,
    source_fps: f64,
) -> Result<TimingResult, TimingError> {
    // NaN is rejected too (Python lets it through and divides by it).
    if source_fps.is_nan() || source_fps <= 0.0 {
        return Err(TimingError::InvalidFps);
    }
    let expected = expected_marker_names(course)?;
    let missing: Vec<&str> = expected
        .iter()
        .filter(|n| !snapshot.frames.contains_key(n.as_str()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(TimingError::MissingMarkers(missing.join(", ")));
    }

    let frame = |name: &str| snapshot.frames[name];
    let mut sectors = Vec::with_capacity(course.sector_count as usize);
    for sector in 1..=course.sector_count {
        let start_marker = if sector == 1 {
            "Start".to_owned()
        } else {
            format!("S{}", sector - 1)
        };
        let end_marker = if sector == course.sector_count {
            "Finish".to_owned()
        } else {
            format!("S{sector}")
        };
        let (start_frame, end_frame) = (frame(&start_marker), frame(&end_marker));
        let duration_frames = end_frame - start_frame;
        if duration_frames <= 0 {
            return Err(TimingError::NonPositiveSector(sector));
        }
        sectors.push(SectorTiming {
            sector,
            start_marker,
            end_marker,
            start_frame,
            end_frame,
            duration_frames,
            duration_seconds: duration_frames as f64 / source_fps,
        });
    }

    let lap_frames = frame("Finish") - frame("Start");
    if lap_frames <= 0 {
        return Err(TimingError::NonPositiveLap);
    }
    Ok(TimingResult {
        sectors,
        lap_frames,
        lap_seconds: lap_frames as f64 / source_fps,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("duration cannot be negative")]
pub struct NegativeDuration;

/// `M:SS.mmm`, milliseconds rounded half-to-even (as Python's `round`).
pub fn format_duration(seconds: f64) -> Result<String, NegativeDuration> {
    if seconds < 0.0 {
        return Err(NegativeDuration);
    }
    let total_ms = round_half_even(seconds * 1000.0);
    let (minutes, rem) = (total_ms / 60_000, total_ms % 60_000);
    Ok(format!("{minutes}:{:02}.{:03}", rem / 1000, rem % 1000))
}

/// `+S.mmm` / `-S.mmm`; zero is `+0.000`.
pub fn format_delta(seconds: f64) -> String {
    let total_ms = round_half_even(seconds.abs() * 1000.0);
    let sign = if seconds < 0.0 { '-' } else { '+' };
    format!("{sign}{}.{:03}", total_ms / 1000, total_ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::parse_marker_snapshot;
    use crate::models::RawMarker;

    fn course() -> Course {
        Course {
            id: "course".into(),
            name: "Course".into(),
            sector_count: 4,
        }
    }

    fn snapshot(items: &[(&str, i64)]) -> MarkerSnapshot {
        let markers: Vec<RawMarker> = items.iter().map(|(n, f)| RawMarker::new(*n, *f)).collect();
        parse_marker_snapshot(&markers, &course()).unwrap()
    }

    #[test]
    fn four_sector_course() {
        let s = snapshot(&[
            ("Start", 100),
            ("S1", 200),
            ("S2", 350),
            ("S3", 500),
            ("Finish", 700),
        ]);
        let t = compute_timing(&s, &course(), 100.0).unwrap();
        let frames: Vec<i64> = t.sectors.iter().map(|s| s.duration_frames).collect();
        assert_eq!(frames, [100, 150, 150, 200]);
        assert_eq!(t.lap_frames, 600);
        assert_eq!(format_duration(t.lap_seconds).unwrap(), "0:06.000");
    }

    #[test]
    fn fractional_fps_and_delta_formatting() {
        let s = snapshot(&[
            ("Start", 0),
            ("S1", 30),
            ("S2", 60),
            ("S3", 90),
            ("Finish", 120),
        ]);
        let t = compute_timing(&s, &course(), 29.97).unwrap();
        assert_eq!(format_duration(t.lap_seconds).unwrap(), "0:04.004");
        assert_eq!(format_delta(1.204), "+1.204");
        assert_eq!(format_delta(-0.532), "-0.532");
        assert_eq!(format_delta(0.0), "+0.000");
    }

    #[test]
    fn rejects_bad_fps_and_negative_durations() {
        let s = snapshot(&[
            ("Start", 0),
            ("S1", 30),
            ("S2", 60),
            ("S3", 90),
            ("Finish", 120),
        ]);
        assert_eq!(
            compute_timing(&s, &course(), 0.0),
            Err(TimingError::InvalidFps)
        );
        assert_eq!(
            compute_timing(&s, &course(), f64::NAN),
            Err(TimingError::InvalidFps)
        );
        assert_eq!(format_duration(-0.1), Err(NegativeDuration));
    }

    #[test]
    fn formats_minutes_and_rounds_half_even() {
        assert_eq!(format_duration(125.4567).unwrap(), "2:05.457");
        // 0.0625 s is exactly representable: 62.5 ms rounds to the even 62.
        assert_eq!(format_duration(0.0625).unwrap(), "0:00.062");
        assert_eq!(format_delta(-0.0625), "-0.062");
    }
}

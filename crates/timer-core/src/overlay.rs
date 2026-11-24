//! What the timer overlay shows for a clip: the live timer, a row per sector
//! and the lap, each with its difference from the comparison (the best lap, or
//! the optimal lap made of the fastest sectors), and the best/optimal lap.
//!
//! Comparisons are chronological: a clip is compared only with runs captured
//! before it, and never with its own stored run.

use crate::capture_time::{self, InvalidCaptureTime};
use crate::database::{DatabaseError, TimerDatabase};
use crate::markers::{SnapshotError, parse_marker_snapshot};
use crate::matching::{find_matching_run, marker_snapshot_hash};
use crate::models::{MarkerFrames, RawMarker, RunRecord};
use crate::stats::{CourseStats, StatsOptions, compute_course_stats};
use crate::timing::{TimingError, compute_timing, format_delta, format_duration};

/// Prefix of the overlay's name in Resolve (`Resolve Timer - course - run`).
pub const GENERATED_NAME_PREFIX: &str = "Resolve Timer";

/// The clip being timed, as read from the timeline (or the CLI's arguments).
#[derive(Debug, Clone, Default)]
pub struct SelectedRun {
    pub course_id: String,
    pub filename: String,
    pub source_fps: f64,
    pub markers: Vec<RawMarker>,
    pub clip_id: Option<String>,
    pub run_date: Option<String>,
    pub capture_time: Option<String>,
    pub capture_time_source: Option<String>,
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonMode {
    BestLap,
    Optimal,
}

impl ComparisonMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BestLap => "best_lap",
            Self::Optimal => "optimal",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayPayload {
    pub course_id: String,
    /// The stored run this clip matches, if any.
    pub run_id: Option<String>,
    pub start_frame: i64,
    pub finish_frame: i64,
    pub source_fps: f64,
    pub marker_frames: MarkerFrames,
    /// The comparison's time for each sector (`None`: nothing to compare with).
    pub sector_reference_seconds: Vec<Option<f64>>,
    pub best_lap_seconds: Option<f64>,
    pub optimal_lap_seconds: Option<f64>,
    pub comparison_mode: ComparisonMode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayRow {
    /// `S1`..`Sn`, then `LAP`.
    pub label: String,
    pub duration_seconds: f64,
    /// Negative is faster than the comparison.
    pub delta_seconds: Option<f64>,
    /// The clip frame the row's time is known at (its end marker).
    pub reveal_frame: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error(transparent)]
    Database(#[from] DatabaseError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Timing(#[from] TimingError),
    #[error(transparent)]
    CaptureTime(#[from] InvalidCaptureTime),
}

/// Builds the overlay for `selected`, compared with `database`'s runs.
pub fn overlay_payload(
    database: &TimerDatabase,
    selected: &SelectedRun,
    mode: ComparisonMode,
) -> Result<OverlayPayload, OverlayError> {
    let course = database.course_by_id(&selected.course_id)?;
    let snapshot = parse_marker_snapshot(&selected.markers, course)?;
    let timing = compute_timing(&snapshot, course, selected.source_fps)?;
    let matching = find_matching_run(
        &database.runs,
        &course.id,
        &selected.filename,
        &snapshot.frames,
        selected.clip_id.as_deref(),
    );
    let as_of = selected_effective_capture_time(selected, matching)?;
    let stats = compute_course_stats(
        course,
        &database.runs,
        StatsOptions {
            exclude_run_id: matching.map(|r| r.id.as_str()),
            as_of_capture_time: as_of.as_deref(),
        },
    );
    let count = course.sector_count.max(0) as usize;
    let (best_sectors, best_lap) = best_lap_references(&stats, count);
    let (optimal_sectors, optimal_lap) = optimal_references(&stats, course.sector_count);
    debug_assert_eq!(
        timing.lap_frames,
        snapshot.frames["Finish"] - snapshot.frames["Start"]
    );
    Ok(OverlayPayload {
        course_id: course.id.clone(),
        run_id: matching.map(|r| r.id.clone()),
        start_frame: snapshot.frames["Start"],
        finish_frame: snapshot.frames["Finish"],
        source_fps: selected.source_fps,
        sector_reference_seconds: match mode {
            ComparisonMode::BestLap => best_sectors,
            ComparisonMode::Optimal => optimal_sectors,
        },
        marker_frames: snapshot.frames,
        best_lap_seconds: best_lap,
        optimal_lap_seconds: optimal_lap,
        comparison_mode: mode,
    })
}

fn best_lap_references(stats: &CourseStats<'_>, count: usize) -> (Vec<Option<f64>>, Option<f64>) {
    match &stats.best_lap {
        None => (vec![None; count], None),
        Some(best) => (
            best.timing
                .sectors
                .iter()
                .map(|s| Some(s.duration_seconds))
                .collect(),
            Some(best.timing.lap_seconds),
        ),
    }
}

fn optimal_references(
    stats: &CourseStats<'_>,
    sector_count: i64,
) -> (Vec<Option<f64>>, Option<f64>) {
    let sectors = (1..=sector_count)
        .map(|n| {
            stats
                .fastest_sectors
                .iter()
                .rev() // a later entry wins, as when Python builds a dict
                .find(|s| s.sector == n)
                .map(|s| s.duration_seconds)
        })
        .collect();
    (sectors, stats.optimal_seconds)
}

/// The capture time the clip is compared as of: its own, else its stored
/// run's, else the end of its date.
fn selected_effective_capture_time(
    selected: &SelectedRun,
    existing: Option<&RunRecord>,
) -> Result<Option<String>, InvalidCaptureTime> {
    let given = |s: &Option<String>| s.as_deref().is_some_and(|v| !v.is_empty());
    if !given(&selected.capture_time)
        && !given(&selected.run_date)
        && let Some(run) = existing
    {
        return Ok(capture_time::effective(
            run.capture_time.as_deref(),
            &run.date,
        ));
    }
    if let Some(t) = capture_time::normalize(selected.capture_time.as_deref().unwrap_or(""))? {
        return Ok(Some(t));
    }
    capture_time::from_date(&selected_run_date(selected, existing)?)
}

fn selected_run_date(
    selected: &SelectedRun,
    existing: Option<&RunRecord>,
) -> Result<String, InvalidCaptureTime> {
    if let Some(d) = selected.run_date.as_deref().filter(|d| !d.is_empty()) {
        return Ok(d.to_owned());
    }
    if let Some(t) = capture_time::normalize(selected.capture_time.as_deref().unwrap_or(""))? {
        return Ok(t[..10].to_owned());
    }
    if let Some(run) = existing {
        return Ok(run.date.clone());
    }
    Ok(chrono::Local::now().date_naive().to_string())
}

impl OverlayPayload {
    pub fn sector_count(&self) -> usize {
        self.sector_reference_seconds.len()
    }

    pub fn lap_seconds(&self) -> f64 {
        (self.finish_frame - self.start_frame) as f64 / self.source_fps
    }

    /// The reference the lap row is compared with.
    pub fn lap_reference_seconds(&self) -> Option<f64> {
        match self.comparison_mode {
            ComparisonMode::BestLap => self.best_lap_seconds,
            ComparisonMode::Optimal => self.optimal_lap_seconds,
        }
    }

    /// `Resolve Timer - <course> - <run id, else marker hash>`.
    pub fn generated_name(&self) -> String {
        let identity = match self.run_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => marker_snapshot_hash(&self.marker_frames),
        };
        format!("{GENERATED_NAME_PREFIX} - {} - {identity}", self.course_id)
    }

    /// One row per sector, then the lap.
    pub fn rows(&self) -> Vec<OverlayRow> {
        let count = self.sector_count();
        let frame = |name: &str| self.marker_frames[name];
        let mut rows: Vec<OverlayRow> = (1..=count)
            .map(|sector| {
                let start = if sector == 1 {
                    "Start".to_owned()
                } else {
                    format!("S{}", sector - 1)
                };
                let end = if sector == count {
                    "Finish".to_owned()
                } else {
                    format!("S{sector}")
                };
                let duration = (frame(&end) - frame(&start)) as f64 / self.source_fps;
                OverlayRow {
                    label: format!("S{sector}"),
                    duration_seconds: duration,
                    delta_seconds: self.sector_reference_seconds[sector - 1].map(|r| duration - r),
                    reveal_frame: frame(&end),
                }
            })
            .collect();
        let lap = self.lap_seconds();
        rows.push(OverlayRow {
            label: "LAP".into(),
            duration_seconds: lap,
            delta_seconds: self.lap_reference_seconds().map(|r| lap - r),
            reveal_frame: self.finish_frame,
        });
        rows
    }

    /// The overlay as text, as it reads at the finish (`overlay-text`).
    pub fn final_text(&self) -> String {
        let mut lines = vec![format!("LIVE        {}", duration(self.lap_seconds()))];
        for row in self.rows() {
            lines.push(format!(
                "{:<11} {:>8}   {:>7}",
                row.label,
                duration(row.duration_seconds),
                delta_text(row.delta_seconds)
            ));
        }
        if let Some(best) = self.best_lap_seconds {
            lines.push(format!("BEST        {}", duration(best)));
        }
        if let Some(optimal) = self.optimal_lap_seconds {
            lines.push(format!("OPTIMAL     {}", duration(optimal)));
        }
        lines.join("\n")
    }

    /// Seconds on the live timer at clip frame `frame`: zero before the start,
    /// held at the lap time after the finish.
    pub fn elapsed_seconds(&self, frame: i64) -> f64 {
        (frame.min(self.finish_frame) - self.start_frame).max(0) as f64 / self.source_fps
    }
}

/// `M:SS.mmm`; durations here are never negative (the markers are validated).
pub fn duration(seconds: f64) -> String {
    format_duration(seconds.max(0.0)).expect("non-negative")
}

/// `+S.mmm` / `-S.mmm`, or `--.---` with nothing to compare with.
pub fn delta_text(delta: Option<f64>) -> String {
    delta.map_or_else(|| "--.---".into(), format_delta)
}

/// RGBA, 0–1: grey with nothing to compare, green faster, red slower, amber level.
pub fn delta_color(delta: Option<f64>) -> [f32; 4] {
    match delta {
        None => [0.72, 0.75, 0.8, 1.0],
        Some(d) if d < 0.0 => [0.2, 1.0, 0.45, 1.0],
        Some(d) if d > 0.0 => [1.0, 0.3, 0.25, 1.0],
        Some(_) => [1.0, 0.76, 0.18, 1.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::clip_fingerprint;
    use crate::models::Course;

    fn course() -> Course {
        Course {
            id: "course".into(),
            name: "Course".into(),
            sector_count: 2,
        }
    }

    fn stored(id: &str, capture: &str, items: &[(&str, i64)]) -> RunRecord {
        let frames: MarkerFrames = items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect();
        let mut r = RunRecord::new(id, "course", &capture[..10], "GX010001.MP4", 100.0, frames);
        r.fingerprint = Some(clip_fingerprint(&r.filename, &r.marker_frames));
        r.capture_time = Some(capture.into());
        r
    }

    fn selected(items: &[(&str, i64)], capture: &str) -> SelectedRun {
        SelectedRun {
            course_id: "course".into(),
            filename: "GX010123.MP4".into(),
            source_fps: 100.0,
            markers: items.iter().map(|(n, f)| RawMarker::new(*n, *f)).collect(),
            capture_time: Some(capture.into()),
            ..Default::default()
        }
    }

    fn db() -> TimerDatabase {
        TimerDatabase {
            courses: vec![course()],
            runs: vec![
                // Best lap 3.0 s (1.0 + 2.0); fastest S2 is from `b`.
                stored(
                    "a",
                    "2026-05-01T10:00:00Z",
                    &[("Start", 0), ("S1", 100), ("Finish", 300)],
                ),
                stored(
                    "b",
                    "2026-05-02T10:00:00Z",
                    &[("Start", 0), ("S1", 150), ("Finish", 320)],
                ),
                // Captured after the clip: not compared with.
                stored(
                    "later",
                    "2026-07-01T10:00:00Z",
                    &[("Start", 0), ("S1", 10), ("Finish", 20)],
                ),
            ],
        }
    }

    #[test]
    fn compares_with_earlier_runs_only() {
        let clip = selected(
            &[("Start", 50), ("S1", 140), ("Finish", 360)],
            "2026-06-01T10:00:00Z",
        );
        let best = overlay_payload(&db(), &clip, ComparisonMode::BestLap).unwrap();
        assert_eq!(best.run_id, None);
        assert_eq!(best.sector_reference_seconds, [Some(1.0), Some(2.0)]);
        assert_eq!(best.best_lap_seconds, Some(3.0));
        assert_eq!(best.optimal_lap_seconds, Some(2.7));
        assert_eq!(
            best.final_text(),
            "LIVE        0:03.100\n\
             S1          0:00.900    -0.100\n\
             S2          0:02.200    +0.200\n\
             LAP         0:03.100    +0.100\n\
             BEST        0:03.000\n\
             OPTIMAL     0:02.700"
        );

        let optimal = overlay_payload(&db(), &clip, ComparisonMode::Optimal).unwrap();
        assert_eq!(optimal.sector_reference_seconds, [Some(1.0), Some(1.7)]);
        let lap = optimal.rows().pop().unwrap();
        assert!((lap.delta_seconds.unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(lap.reveal_frame, 360);
    }

    #[test]
    fn a_stored_clip_is_not_compared_with_itself() {
        let mut clip = selected(
            &[("Start", 0), ("S1", 150), ("Finish", 320)],
            "2026-05-02T10:00:00Z",
        );
        clip.filename = "GX010001.MP4".into();
        let p = overlay_payload(&db(), &clip, ComparisonMode::BestLap).unwrap();
        assert_eq!(p.run_id.as_deref(), Some("b"));
        assert_eq!(p.generated_name(), "Resolve Timer - course - b");
        assert_eq!(p.best_lap_seconds, Some(3.0));
    }

    #[test]
    fn nothing_to_compare_with() {
        let clip = selected(
            &[("Start", 0), ("S1", 100), ("Finish", 300)],
            "2026-01-01T00:00:00Z",
        );
        let p = overlay_payload(&db(), &clip, ComparisonMode::BestLap).unwrap();
        assert_eq!(p.sector_reference_seconds, [None, None]);
        assert!(p.final_text().contains("S1          0:01.000    --.---"));
        assert!(!p.final_text().contains("BEST"));
        assert_eq!(delta_color(None), [0.72, 0.75, 0.8, 1.0]);
        assert!(p.generated_name().starts_with("Resolve Timer - course - "));
    }

    #[test]
    fn live_timer_holds_before_start_and_after_finish() {
        let clip = selected(
            &[("Start", 100), ("S1", 200), ("Finish", 400)],
            "2026-01-01T00:00:00Z",
        );
        let p = overlay_payload(&db(), &clip, ComparisonMode::BestLap).unwrap();
        assert_eq!(p.elapsed_seconds(0), 0.0);
        assert_eq!(p.elapsed_seconds(250), 1.5);
        assert_eq!(p.elapsed_seconds(10_000), 3.0);
    }
}

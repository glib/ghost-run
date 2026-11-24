//! The timing of markers still being placed: each sector whose two ends are
//! set, the lap once Start and Finish are, and how they compare with the
//! course's best, so the marker editor can show it as you go.

use crate::markers::expected_marker_names;
use crate::models::{Course, MarkerFrames, RunRecord};
use crate::stats::{StatsOptions, compute_course_stats};

#[derive(Debug, Clone, PartialEq)]
pub struct SectorPreview {
    pub sector: i64,
    /// Seconds, once both ends are set and in order.
    pub seconds: Option<f64>,
    /// Both ends set, but the end isn't after the start.
    pub out_of_order: bool,
    /// Against the same sector of the best lap.
    pub delta_best: Option<f64>,
    /// Faster than (or as fast as) the course's fastest time for this sector.
    pub fastest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// The course's markers in order: Start, S1…, Finish.
    pub names: Vec<String>,
    /// The first marker not yet set.
    pub next: Option<String>,
    pub sectors: Vec<SectorPreview>,
    pub lap_seconds: Option<f64>,
    pub lap_out_of_order: bool,
    pub best_lap_seconds: Option<f64>,
    pub delta_best: Option<f64>,
    /// Where the lap would rank among the course's counted runs, of how many.
    pub rank: Option<(usize, usize)>,
    /// Every marker set and in order: it can be committed.
    pub complete: bool,
}

/// Previews `markers` on `course`, against `runs` (leaving out
/// `this_run`, the clip's own stored run).
pub fn preview(
    course: &Course,
    runs: &[RunRecord],
    markers: &MarkerFrames,
    fps: f64,
    this_run: Option<&str>,
) -> Preview {
    let names = expected_marker_names(course).unwrap_or_default();
    let stats = compute_course_stats(
        course,
        runs,
        StatsOptions {
            exclude_run_id: this_run,
            as_of_capture_time: None,
        },
    );
    let best = stats.best_lap.as_ref();
    let fps_ok = fps.is_finite() && fps > 0.0;
    let span = |a: &str, b: &str| -> (Option<f64>, bool) {
        match (markers.get(a), markers.get(b)) {
            (Some(s), Some(e)) if fps_ok && e > s => (Some((e - s) as f64 / fps), false),
            (Some(_), Some(_)) => (None, true),
            _ => (None, false),
        }
    };

    let mut sectors = Vec::new();
    for (i, pair) in names.windows(2).enumerate() {
        let (seconds, out_of_order) = span(&pair[0], &pair[1]);
        let best_sector = best.and_then(|b| b.timing.sectors.get(i));
        let fastest_sector = stats.fastest_sectors.get(i);
        sectors.push(SectorPreview {
            sector: i as i64 + 1,
            seconds,
            out_of_order,
            delta_best: seconds
                .zip(best_sector)
                .map(|(s, b)| s - b.duration_seconds),
            fastest: seconds
                .zip(fastest_sector)
                .is_some_and(|(s, f)| s <= f.duration_seconds),
        });
    }
    let (lap_seconds, lap_out_of_order) = span("Start", "Finish");
    let best_lap_seconds = best.map(|b| b.timing.lap_seconds);
    let complete = !names.is_empty()
        && names.iter().all(|n| markers.contains_key(n))
        && sectors.iter().all(|s| s.seconds.is_some());
    let rank = lap_seconds.filter(|_| complete).map(|lap| {
        let faster = stats
            .eligible_runs
            .iter()
            .filter(|r| r.timing.lap_seconds < lap)
            .count();
        (faster + 1, stats.eligible_runs.len() + 1)
    });
    Preview {
        next: names.iter().find(|n| !markers.contains_key(*n)).cloned(),
        names,
        sectors,
        lap_seconds,
        lap_out_of_order,
        best_lap_seconds,
        delta_best: lap_seconds.zip(best_lap_seconds).map(|(l, b)| l - b),
        rank,
        complete,
    }
}

/// Markers that belong on `course` (others are dropped when the course changes).
pub fn keep_for_course(markers: &MarkerFrames, course: &Course) -> MarkerFrames {
    let names = expected_marker_names(course).unwrap_or_default();
    markers
        .iter()
        .filter(|(n, _)| names.contains(n))
        .map(|(n, f)| (n.clone(), *f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn course() -> Course {
        Course {
            id: "c".into(),
            name: "C".into(),
            sector_count: 2,
        }
    }

    fn frames(items: &[(&str, i64)]) -> MarkerFrames {
        items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect()
    }

    fn run(id: &str, s1: i64, finish: i64) -> RunRecord {
        let mut r = RunRecord::new(
            id,
            "c",
            "2026-08-17",
            format!("{id}.MP4"),
            10.0,
            frames(&[("Start", 0), ("S1", s1), ("Finish", finish)]),
        );
        r.committed = true;
        r
    }

    #[test]
    fn shows_sectors_as_they_are_placed() {
        let runs = [run("a", 100, 300), run("b", 120, 280)];
        let p = preview(
            &course(),
            &runs,
            &frames(&[("Start", 0), ("S1", 90)]),
            10.0,
            None,
        );
        assert_eq!(p.names, ["Start", "S1", "Finish"]);
        assert_eq!(p.next.as_deref(), Some("Finish"));
        assert_eq!(p.sectors[0].seconds, Some(9.0));
        // Best lap is b (28 s); its sector 1 took 12 s.
        assert_eq!(p.sectors[0].delta_best, Some(-3.0));
        assert!(p.sectors[0].fastest);
        assert_eq!(p.sectors[1].seconds, None);
        assert_eq!(p.lap_seconds, None);
        assert!(!p.complete && p.rank.is_none());
    }

    #[test]
    fn ranks_a_complete_lap_and_leaves_its_own_run_out() {
        let runs = [run("a", 100, 300), run("b", 120, 280)];
        let m = frames(&[("Start", 10), ("S1", 120), ("Finish", 300)]);
        let p = preview(&course(), &runs, &m, 10.0, None);
        assert!(p.complete);
        assert_eq!(p.lap_seconds, Some(29.0));
        assert_eq!(p.best_lap_seconds, Some(28.0));
        assert_eq!(p.delta_best, Some(1.0));
        assert_eq!(p.rank, Some((2, 3)));
        assert!(!p.sectors[1].fastest);

        let p = preview(&course(), &runs, &m, 10.0, Some("b"));
        assert_eq!(p.best_lap_seconds, Some(30.0));
        assert_eq!(p.rank, Some((1, 2)));
    }

    #[test]
    fn markers_out_of_order_are_flagged_not_timed() {
        let m = frames(&[("Start", 50), ("S1", 40), ("Finish", 30)]);
        let p = preview(&course(), &[], &m, 10.0, None);
        assert!(p.sectors[0].out_of_order && p.lap_out_of_order);
        assert!(!p.complete);
        assert_eq!(p.best_lap_seconds, None);
    }

    #[test]
    fn changing_course_keeps_only_its_markers() {
        let m = frames(&[("Start", 1), ("S1", 2), ("S2", 3), ("Finish", 4)]);
        assert_eq!(
            keep_for_course(&m, &course()),
            frames(&[("Start", 1), ("S1", 2), ("Finish", 4)])
        );
    }
}

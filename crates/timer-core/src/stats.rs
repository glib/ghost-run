//! Course statistics: best lap, fastest sector each, and the optimal lap (the
//! sum of the fastest sectors), over committed, non-ignored, valid runs.

use crate::capture_time;
use crate::markers::{parse_marker_snapshot, raw_markers};
use crate::matching::run_order_time;
use crate::models::{Course, RunRecord, TimingResult};
use crate::timing::compute_timing;

#[derive(Debug, Clone)]
pub struct RunTiming<'a> {
    pub run: &'a RunRecord,
    pub timing: TimingResult,
}

#[derive(Debug, Clone)]
pub struct FastestSector<'a> {
    pub sector: i64,
    pub run: &'a RunRecord,
    pub duration_seconds: f64,
    pub duration_frames: i64,
}

#[derive(Debug, Clone)]
pub struct CourseStats<'a> {
    /// Eligible runs in comparison order (capture time, then id).
    pub eligible_runs: Vec<RunTiming<'a>>,
    /// The fastest lap; the earliest run wins a tie.
    pub best_lap: Option<RunTiming<'a>>,
    pub fastest_sectors: Vec<FastestSector<'a>>,
    pub optimal_seconds: Option<f64>,
    pub optimal_frames: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StatsOptions<'o> {
    /// Leave this run out (e.g. the run being previewed).
    pub exclude_run_id: Option<&'o str>,
    /// Only count runs captured before this time (chronological comparisons).
    pub as_of_capture_time: Option<&'o str>,
}

pub fn compute_course_stats<'a>(
    course: &Course,
    runs: &'a [RunRecord],
    options: StatsOptions<'_>,
) -> CourseStats<'a> {
    let as_of = options
        .as_of_capture_time
        .and_then(|t| capture_time::normalize(t).ok().flatten());
    let mut eligible: Vec<RunTiming<'a>> = runs
        .iter()
        .filter(|run| options.exclude_run_id != Some(run.id.as_str()))
        .filter(|run| run.course_id == course.id && run.committed && !run.ignored)
        .filter(|run| {
            let captured = capture_time::effective(run.capture_time.as_deref(), &run.date);
            match (&as_of, captured) {
                (Some(as_of), Some(captured)) => captured < *as_of,
                _ => true,
            }
        })
        .filter_map(|run| {
            let snapshot = parse_marker_snapshot(&raw_markers(&run.marker_frames), course).ok()?;
            let timing = compute_timing(&snapshot, course, run.source_fps).ok()?;
            Some(RunTiming { run, timing })
        })
        .collect();
    eligible.sort_by(|a, b| {
        (run_order_time(a.run), &a.run.id).cmp(&(run_order_time(b.run), &b.run.id))
    });

    // `min_by` keeps the first of equal elements, like Python's `min()`.
    let best_lap = eligible
        .iter()
        .min_by(|a, b| a.timing.lap_seconds.total_cmp(&b.timing.lap_seconds))
        .cloned();

    let mut fastest_sectors = Vec::new();
    for index in 0..course.sector_count.max(0) as usize {
        let candidates = eligible
            .iter()
            .map(|item| (item, &item.timing.sectors[index]));
        let best = candidates.min_by(|(ia, sa), (ib, sb)| {
            sa.duration_seconds
                .total_cmp(&sb.duration_seconds)
                .then_with(|| run_order_time(ia.run).cmp(&run_order_time(ib.run)))
                .then_with(|| ia.run.id.cmp(&ib.run.id))
        });
        if let Some((item, sector)) = best {
            fastest_sectors.push(FastestSector {
                sector: sector.sector,
                run: item.run,
                duration_seconds: sector.duration_seconds,
                duration_frames: sector.duration_frames,
            });
        }
    }

    let (optimal_seconds, optimal_frames) = if fastest_sectors.is_empty() {
        (None, None)
    } else {
        // Summed left to right from zero, as Python's `sum()`, so the float result is identical.
        let seconds = fastest_sectors
            .iter()
            .fold(0.0, |acc, s| acc + s.duration_seconds);
        (
            Some(seconds),
            Some(fastest_sectors.iter().map(|s| s.duration_frames).sum()),
        )
    };

    CourseStats {
        eligible_runs: eligible,
        best_lap,
        fastest_sectors,
        optimal_seconds,
        optimal_frames,
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

    fn frames(items: &[(&str, i64)]) -> MarkerFrames {
        items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect()
    }

    fn run(id: &str, f: MarkerFrames) -> RunRecord {
        let mut r = RunRecord::new(id, "course", "2026-05-31", "GX010123.MP4", 100.0, f);
        r.fingerprint = Some(clip_fingerprint(&r.filename, &r.marker_frames));
        r.committed_at = Some("2026-05-31T10:00:00Z".into());
        r
    }

    fn a() -> MarkerFrames {
        frames(&[("Start", 0), ("S1", 100), ("Finish", 300)])
    }
    fn b() -> MarkerFrames {
        frames(&[("Start", 0), ("S1", 80), ("Finish", 280)])
    }

    fn ids(items: &[RunTiming<'_>]) -> Vec<String> {
        items.iter().map(|i| i.run.id.clone()).collect()
    }

    #[test]
    fn only_committed_non_ignored_valid_runs() {
        let mut uncommitted = run("run_b", b());
        uncommitted.committed = false;
        let mut ignored = run("run_c", b());
        ignored.ignored = true;
        let runs = [
            run("run_a", a()),
            uncommitted,
            ignored,
            run("run_d", frames(&[("Start", 0), ("Finish", 100)])),
        ];
        let stats = compute_course_stats(&course(), &runs, StatsOptions::default());
        assert_eq!(ids(&stats.eligible_runs), ["run_a"]);
        assert_eq!(stats.best_lap.unwrap().run.id, "run_a");
        assert_eq!(stats.optimal_seconds, Some(3.0));
    }

    #[test]
    fn best_lap_tie_goes_to_the_earliest_run() {
        // Same date (so same fallback capture time); the id breaks the order.
        let runs = [run("late", a()), run("early", a())];
        let stats = compute_course_stats(&course(), &runs, StatsOptions::default());
        assert_eq!(stats.best_lap.unwrap().run.id, "early");
    }

    #[test]
    fn can_exclude_a_run() {
        let runs = [run("run_a", a()), run("run_b", b())];
        let stats = compute_course_stats(
            &course(),
            &runs,
            StatsOptions {
                exclude_run_id: Some("run_b"),
                ..Default::default()
            },
        );
        assert_eq!(ids(&stats.eligible_runs), ["run_a"]);
        assert!(!runs[1].ignored);
    }

    #[test]
    fn optimal_mixes_fastest_sectors_from_different_runs() {
        let runs = [
            run(
                "run_a",
                frames(&[("Start", 0), ("S1", 100), ("Finish", 300)]),
            ),
            run(
                "run_b",
                frames(&[("Start", 0), ("S1", 80), ("Finish", 320)]),
            ),
        ];
        let stats = compute_course_stats(&course(), &runs, StatsOptions::default());
        let sector_runs: Vec<&str> = stats
            .fastest_sectors
            .iter()
            .map(|s| s.run.id.as_str())
            .collect();
        assert_eq!(sector_runs, ["run_b", "run_a"]);
        assert_eq!(stats.optimal_seconds, Some(2.8));
    }

    #[test]
    fn as_of_keeps_only_earlier_runs() {
        let mut early = run("early", a());
        early.capture_time = Some("2026-05-30T10:00:00Z".into());
        let mut late = run("late", b());
        late.capture_time = Some("2026-06-01T10:00:00Z".into());
        let runs = [early, late];
        let options = StatsOptions {
            as_of_capture_time: Some("2026-06-01T10:00:00Z"),
            ..Default::default()
        };
        assert_eq!(
            ids(&compute_course_stats(&course(), &runs, options).eligible_runs),
            ["early"]
        );
    }
}

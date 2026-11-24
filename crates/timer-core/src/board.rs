//! A course's leaderboard: every run on it, ranked by lap, with sector times,
//! the gap to the best lap, and why a run doesn't count if it doesn't. Built on
//! [`compute_course_stats`], so "best" and "optimal" mean what they mean
//! everywhere else.

use crate::capture_time;
use crate::markers::{parse_marker_snapshot, raw_markers};
use crate::models::{Course, RunRecord};
use crate::stats::{StatsOptions, compute_course_stats};
use crate::timing::compute_timing;

#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    /// Counts towards best and optimal laps.
    Counted,
    Ignored,
    NotCommitted,
    /// The markers don't time this course (say why).
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardRow {
    pub run_id: String,
    /// When it was ridden: capture time if known (`YYYY-MM-DD HH:MM`), else the date.
    pub when: String,
    pub filename: String,
    /// 1 = fastest; only counted runs are ranked.
    pub rank: Option<usize>,
    pub lap_seconds: Option<f64>,
    /// Lap minus the best lap (0 for the best).
    pub delta_best: Option<f64>,
    pub sectors: Vec<Option<f64>>,
    /// Per sector: this run holds the fastest time.
    pub fastest: Vec<bool>,
    pub status: RunStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CourseBoard {
    pub course: Course,
    /// Counted runs fastest first, then the rest, newest first.
    pub rows: Vec<BoardRow>,
    pub best_lap: Option<(String, f64)>,
    pub optimal_seconds: Option<f64>,
}

pub fn course_board(course: &Course, runs: &[RunRecord]) -> CourseBoard {
    let stats = compute_course_stats(course, runs, StatsOptions::default());
    let sectors = course.sector_count.max(0) as usize;
    let best = stats.best_lap.as_ref().map(|b| b.timing.lap_seconds);
    let mut rows: Vec<BoardRow> = runs
        .iter()
        .filter(|r| r.course_id == course.id)
        .map(|run| {
            let timing = parse_marker_snapshot(&raw_markers(&run.marker_frames), course)
                .map_err(|e| e.to_string())
                .and_then(|s| {
                    compute_timing(&s, course, run.source_fps).map_err(|e| e.to_string())
                });
            let status = match &timing {
                Err(e) => RunStatus::Invalid(e.clone()),
                Ok(_) if !run.committed => RunStatus::NotCommitted,
                Ok(_) if run.ignored => RunStatus::Ignored,
                Ok(_) => RunStatus::Counted,
            };
            let timing = timing.ok();
            let sector_times: Vec<Option<f64>> = (0..sectors)
                .map(|i| timing.as_ref().map(|t| t.sectors[i].duration_seconds))
                .collect();
            let fastest = (0..sectors)
                .map(|i| {
                    status == RunStatus::Counted
                        && stats
                            .fastest_sectors
                            .get(i)
                            .is_some_and(|f| f.run.id == run.id)
                })
                .collect();
            let lap = timing.as_ref().map(|t| t.lap_seconds);
            BoardRow {
                run_id: run.id.clone(),
                when: when(run),
                filename: run.filename.clone(),
                rank: None,
                lap_seconds: lap,
                delta_best: lap.zip(best).map(|(l, b)| l - b),
                sectors: sector_times,
                fastest,
                status,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let counted = |r: &BoardRow| r.status != RunStatus::Counted;
        counted(a)
            .cmp(&counted(b))
            .then_with(
                || match (a.status == RunStatus::Counted, a.lap_seconds, b.lap_seconds) {
                    (true, Some(x), Some(y)) => x.total_cmp(&y).then_with(|| a.when.cmp(&b.when)),
                    _ => b.when.cmp(&a.when),
                },
            )
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    let mut rank = 0;
    for row in &mut rows {
        if row.status == RunStatus::Counted {
            rank += 1;
            row.rank = Some(rank);
        }
    }
    CourseBoard {
        course: course.clone(),
        rows,
        best_lap: stats
            .best_lap
            .as_ref()
            .map(|b| (b.run.id.clone(), b.timing.lap_seconds)),
        optimal_seconds: stats.optimal_seconds,
    }
}

fn when(run: &RunRecord) -> String {
    // A capture time made up from the date (end of that day) says nothing more.
    if run.capture_time_source.as_deref() == Some("date_fallback") {
        return run.date.clone();
    }
    match run
        .capture_time
        .as_deref()
        .and_then(|t| capture_time::normalize(t).ok().flatten())
    {
        // Normalised capture times are UTC ISO (`2026-08-17T14:52:36Z`); show
        // them in this computer's zone, to the minute.
        Some(t) => chrono::DateTime::parse_from_rfc3339(&t)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or(t),
        None => run.date.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MarkerFrames;

    fn frames(s1: i64, finish: i64) -> MarkerFrames {
        [("Start", 0), ("S1", s1), ("Finish", finish)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect()
    }

    fn run(id: &str, date: &str, f: MarkerFrames) -> RunRecord {
        let mut r = RunRecord::new(id, "c", date, format!("{id}.MP4"), 10.0, f);
        r.committed = true;
        r
    }

    #[test]
    fn ranks_counted_runs_and_explains_the_rest() {
        let course = Course {
            id: "c".into(),
            name: "C".into(),
            sector_count: 2,
        };
        let mut ignored = run("d", "2026-06-04", frames(10, 20));
        ignored.ignored = true;
        let mut other = run("e", "2026-06-05", frames(10, 20));
        other.course_id = "elsewhere".into();
        let runs = vec![
            run("a", "2026-06-01", frames(100, 300)), // 10 + 20 s
            run("b", "2026-06-02", frames(90, 310)),  // 9 + 22 s: faster S1
            run("c", "2026-06-03", frames(300, 100)), // markers out of order
            ignored,
            other,
        ];
        let board = course_board(&course, &runs);
        let ids: Vec<&str> = board.rows.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "d", "c"]);
        assert_eq!(board.best_lap, Some(("a".into(), 30.0)));
        assert_eq!(board.optimal_seconds, Some(29.0));

        let a = &board.rows[0];
        assert_eq!(
            (a.rank, a.lap_seconds, a.delta_best),
            (Some(1), Some(30.0), Some(0.0))
        );
        assert_eq!(a.sectors, [Some(10.0), Some(20.0)]);
        assert_eq!(a.fastest, [false, true]);
        let b = &board.rows[1];
        assert_eq!((b.rank, b.delta_best), (Some(2), Some(1.0)));
        assert_eq!(b.fastest, [true, false]);
        assert_eq!(board.rows[2].status, RunStatus::Ignored);
        assert_eq!(board.rows[2].rank, None);
        assert!(matches!(board.rows[3].status, RunStatus::Invalid(_)));
        assert_eq!(board.rows[3].lap_seconds, None);
    }
}

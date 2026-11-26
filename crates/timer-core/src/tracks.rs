//! Tracks: which courses are ridden most lately, and how each one is going —
//! the personal best and how long it has stood, the latest run against it,
//! every counted run over time, and the sector losing most against the
//! fastest of each. Built on [`course_board`], so a run counts here exactly
//! when it counts on the leaderboard.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::board::{CourseBoard, RunStatus, course_board};
use crate::database::TimerDatabase;
use crate::matching::run_order_time;
use crate::models::{Course, RunRecord};

/// "Lately": runs in this many days count towards a course's order.
pub const RECENT_DAYS: i64 = 56;
/// Sector gaps average this many of the latest counted runs.
pub const RECENT_RUNS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Best {
    pub run_id: String,
    pub lap_seconds: f64,
    pub when: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CourseSummary {
    pub course: Course,
    /// Every run on it, counted or not.
    pub runs: usize,
    /// Runs in the last [`RECENT_DAYS`].
    pub recent_runs: usize,
    pub last_ridden: Option<DateTime<Utc>>,
    pub best: Option<Best>,
    /// The latest counted run's lap minus the best (0 when it is the best).
    pub latest_gap: Option<f64>,
}

/// A counted run on the trend chart.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub run_id: String,
    pub when: DateTime<Utc>,
    pub lap_seconds: f64,
    /// Faster than every run before it.
    pub new_best: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectorGap {
    /// 1-based.
    pub sector: usize,
    pub fastest: f64,
    /// Mean of the latest [`RECENT_RUNS`] counted runs.
    pub recent_mean: f64,
    pub gap: f64,
}

/// When a run was ridden, as its capture time (or date) in UTC.
pub fn ridden_at(run: &RunRecord) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&run_order_time(run))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Every course, ridden most lately first (then most recently ridden, then
/// by name).
pub fn ridden_most(db: &TimerDatabase, now: DateTime<Utc>) -> Vec<CourseSummary> {
    let since = now - chrono::Duration::days(RECENT_DAYS);
    let mut out: Vec<CourseSummary> = db
        .courses
        .iter()
        .map(|course| {
            let runs: Vec<&RunRecord> = db
                .runs
                .iter()
                .filter(|r| r.course_id == course.id)
                .collect();
            let times: Vec<DateTime<Utc>> = runs.iter().filter_map(|r| ridden_at(r)).collect();
            let board = course_board(course, &db.runs);
            let points = history(&board, &db.runs);
            let best = board.best_lap.as_ref().map(|(id, lap)| Best {
                run_id: id.clone(),
                lap_seconds: *lap,
                when: db.runs.iter().find(|r| &r.id == id).and_then(ridden_at),
            });
            let latest_gap = points
                .last()
                .zip(best.as_ref())
                .map(|(p, b)| p.lap_seconds - b.lap_seconds);
            CourseSummary {
                course: course.clone(),
                runs: runs.len(),
                recent_runs: times.iter().filter(|t| **t >= since).count(),
                last_ridden: times.iter().max().copied(),
                best,
                latest_gap,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.recent_runs
            .cmp(&a.recent_runs)
            .then(b.last_ridden.cmp(&a.last_ridden))
            .then_with(|| a.course.name.cmp(&b.course.name))
    });
    out
}

/// A course's counted runs in the order ridden, marking each new best.
pub fn history(board: &CourseBoard, runs: &[RunRecord]) -> Vec<Point> {
    let by_id: HashMap<&str, &RunRecord> = runs.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut points: Vec<Point> = board
        .rows
        .iter()
        .filter(|r| r.status == RunStatus::Counted)
        .filter_map(|row| {
            Some(Point {
                run_id: row.run_id.clone(),
                when: ridden_at(by_id.get(row.run_id.as_str())?)?,
                lap_seconds: row.lap_seconds?,
                new_best: false,
            })
        })
        .collect();
    points.sort_by(|a, b| a.when.cmp(&b.when).then_with(|| a.run_id.cmp(&b.run_id)));
    let mut best = f64::INFINITY;
    for p in &mut points {
        if p.lap_seconds < best {
            best = p.lap_seconds;
            p.new_best = true;
        }
    }
    points
}

/// Each sector's fastest counted time against the mean of the latest
/// [`RECENT_RUNS`] counted runs, biggest gap first.
pub fn sector_gaps(board: &CourseBoard, runs: &[RunRecord]) -> Vec<SectorGap> {
    let points = history(board, runs);
    let latest: Vec<&str> = points
        .iter()
        .rev()
        .take(RECENT_RUNS)
        .map(|p| p.run_id.as_str())
        .collect();
    let counted: Vec<_> = board
        .rows
        .iter()
        .filter(|r| r.status == RunStatus::Counted)
        .collect();
    let sectors = board.course.sector_count.max(0) as usize;
    let mut gaps: Vec<SectorGap> = (0..sectors)
        .filter_map(|i| {
            let fastest = counted
                .iter()
                .filter_map(|r| r.sectors.get(i).copied().flatten())
                .min_by(f64::total_cmp)?;
            let recent: Vec<f64> = counted
                .iter()
                .filter(|r| latest.contains(&r.run_id.as_str()))
                .filter_map(|r| r.sectors.get(i).copied().flatten())
                .collect();
            if recent.is_empty() {
                return None;
            }
            let recent_mean = recent.iter().sum::<f64>() / recent.len() as f64;
            Some(SectorGap {
                sector: i + 1,
                fastest,
                recent_mean,
                gap: recent_mean - fastest,
            })
        })
        .collect();
    gaps.sort_by(|a, b| b.gap.total_cmp(&a.gap).then(a.sector.cmp(&b.sector)));
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MarkerFrames;

    fn course(id: &str, name: &str) -> Course {
        Course {
            id: id.into(),
            name: name.into(),
            sector_count: 2,
        }
    }

    /// A run at 10 fps: sector 1 then sector 2, in seconds.
    fn run(id: &str, course: &str, when: &str, s1: i64, s2: i64) -> RunRecord {
        let frames: MarkerFrames = [("Start", 0), ("S1", s1 * 10), ("Finish", (s1 + s2) * 10)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect();
        let mut r = RunRecord::new(id, course, &when[..10], format!("{id}.MP4"), 10.0, frames);
        r.capture_time = Some(when.into());
        r
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn db(runs: Vec<RunRecord>) -> TimerDatabase {
        TimerDatabase {
            courses: vec![
                course("a", "Alpha"),
                course("b", "Bravo"),
                course("c", "Charlie"),
            ],
            runs,
        }
    }

    #[test]
    fn courses_ridden_most_lately_come_first() {
        let d = db(vec![
            // Alpha: many runs, but long ago.
            run("a1", "a", "2026-01-01T10:00:00Z", 30, 30),
            run("a2", "a", "2026-01-02T10:00:00Z", 30, 30),
            run("a3", "a", "2026-01-03T10:00:00Z", 30, 30),
            // Bravo: two lately.
            run("b1", "b", "2026-08-01T10:00:00Z", 40, 40),
            run("b2", "b", "2026-08-20T10:00:00Z", 39, 42),
            // Charlie: one lately.
            run("c1", "c", "2026-08-10T10:00:00Z", 20, 20),
        ]);
        let order = ridden_most(&d, at("2026-08-26T12:00:00Z"));
        let names: Vec<&str> = order.iter().map(|s| s.course.name.as_str()).collect();
        assert_eq!(names, ["Bravo", "Charlie", "Alpha"]);
        let bravo = &order[0];
        assert_eq!((bravo.runs, bravo.recent_runs), (2, 2));
        assert_eq!(bravo.best.as_ref().unwrap().run_id, "b1");
        assert_eq!(
            bravo.best.as_ref().unwrap().when,
            Some(at("2026-08-01T10:00:00Z"))
        );
        assert_eq!(bravo.latest_gap, Some(1.0));
        assert_eq!(bravo.last_ridden, Some(at("2026-08-20T10:00:00Z")));
    }

    #[test]
    fn history_marks_each_new_best_in_the_order_ridden() {
        let d = db(vec![
            run("3", "a", "2026-08-03T10:00:00Z", 29, 30),
            run("1", "a", "2026-08-01T10:00:00Z", 31, 31),
            run("2", "a", "2026-08-02T10:00:00Z", 32, 32),
            run("4", "a", "2026-08-04T10:00:00Z", 28, 30),
        ]);
        let board = course_board(&d.courses[0], &d.runs);
        let p = history(&board, &d.runs);
        let marks: Vec<(&str, bool)> = p.iter().map(|p| (p.run_id.as_str(), p.new_best)).collect();
        assert_eq!(marks, [("1", true), ("2", false), ("3", true), ("4", true)]);
    }

    #[test]
    fn the_sector_losing_most_lately_comes_first() {
        let mut runs = vec![
            // An old run holds both fastest sectors.
            run("old", "a", "2026-01-01T10:00:00Z", 20, 30),
        ];
        for (i, day) in (10..16).enumerate() {
            // Sector 1 about a second off, sector 2 about three.
            runs.push(run(
                &format!("r{i}"),
                "a",
                &format!("2026-08-{day}T10:00:00Z"),
                21,
                33,
            ));
        }
        let d = db(runs);
        let board = course_board(&d.courses[0], &d.runs);
        let gaps = sector_gaps(&board, &d.runs);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].sector, 2);
        assert_eq!(
            (gaps[0].fastest, gaps[0].recent_mean, gaps[0].gap),
            (30.0, 33.0, 3.0)
        );
        assert_eq!(gaps[1].sector, 1);
        assert_eq!(gaps[1].gap, 1.0);
    }
}

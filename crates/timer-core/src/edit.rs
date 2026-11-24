//! Changes the app makes to a database: moving runs to another course,
//! ignoring them, deleting them, adding and renaming courses. Each one either
//! applies whole or fails without changing anything, and none can leave a run
//! whose markers don't fit its course.

use std::collections::HashSet;

use crate::database::TimerDatabase;
use crate::models::Course;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EditError {
    #[error("no course {0}")]
    NoCourse(String),
    #[error("no run {0}")]
    NoRun(String),
    #[error(
        "{run} has markers for {has} sectors, but {course} has {wants}: place its markers again for that course instead"
    )]
    SectorMismatch {
        run: String,
        course: String,
        has: usize,
        wants: i64,
    },
    #[error("a course needs a name")]
    NoName,
    #[error("a course needs at least one sector")]
    NoSectors,
    #[error("there is already a course called {0}")]
    DuplicateName(String),
}

fn check_runs(db: &TimerDatabase, ids: &[String]) -> Result<(), EditError> {
    let known: HashSet<&str> = db.runs.iter().map(|r| r.id.as_str()).collect();
    match ids.iter().find(|id| !known.contains(id.as_str())) {
        Some(id) => Err(EditError::NoRun(id.clone())),
        None => Ok(()),
    }
}

/// Moves runs to another course. Their markers stay as they are, so the new
/// course must have the same number of sectors.
pub fn move_runs(db: &mut TimerDatabase, ids: &[String], course_id: &str) -> Result<(), EditError> {
    check_runs(db, ids)?;
    let course = db
        .courses
        .iter()
        .find(|c| c.id == course_id)
        .ok_or_else(|| EditError::NoCourse(course_id.into()))?
        .clone();
    for run in db.runs.iter().filter(|r| ids.contains(&r.id)) {
        // Start and Finish, plus one marker between each pair of sectors.
        let has = run.marker_frames.len().saturating_sub(1);
        if has as i64 != course.sector_count {
            return Err(EditError::SectorMismatch {
                run: run.id.clone(),
                course: course.name.clone(),
                has,
                wants: course.sector_count,
            });
        }
    }
    for run in db.runs.iter_mut().filter(|r| ids.contains(&r.id)) {
        run.course_id = course.id.clone();
    }
    Ok(())
}

pub fn set_ignored(db: &mut TimerDatabase, ids: &[String], ignored: bool) -> Result<(), EditError> {
    check_runs(db, ids)?;
    for run in db.runs.iter_mut().filter(|r| ids.contains(&r.id)) {
        run.ignored = ignored;
    }
    Ok(())
}

pub fn delete_runs(db: &mut TimerDatabase, ids: &[String]) -> Result<(), EditError> {
    check_runs(db, ids)?;
    db.runs.retain(|r| !ids.contains(&r.id));
    Ok(())
}

/// Adds a course and returns its id: the name in lower case with underscores
/// (`Lower Whistler A-Line` → `lower_whistler_a_line`), made unique.
pub fn add_course(
    db: &mut TimerDatabase,
    name: &str,
    sector_count: i64,
) -> Result<String, EditError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EditError::NoName);
    }
    if sector_count < 1 {
        return Err(EditError::NoSectors);
    }
    if db.courses.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
        return Err(EditError::DuplicateName(name.into()));
    }
    let mut base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    while base.contains("__") {
        base = base.replace("__", "_");
    }
    let base = match base.trim_matches('_') {
        "" => "course".to_owned(),
        b => b.to_owned(),
    };
    let mut id = base.clone();
    let mut n = 2;
    while db.courses.iter().any(|c| c.id == id) {
        id = format!("{base}_{n}");
        n += 1;
    }
    db.courses.push(Course {
        id: id.clone(),
        name: name.into(),
        sector_count,
    });
    Ok(id)
}

/// Renames a course. Its id stays, so runs and overlays keep pointing at it.
pub fn rename_course(db: &mut TimerDatabase, course_id: &str, name: &str) -> Result<(), EditError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EditError::NoName);
    }
    if db
        .courses
        .iter()
        .any(|c| c.id != course_id && c.name.eq_ignore_ascii_case(name))
    {
        return Err(EditError::DuplicateName(name.into()));
    }
    let course = db
        .courses
        .iter_mut()
        .find(|c| c.id == course_id)
        .ok_or_else(|| EditError::NoCourse(course_id.into()))?;
    course.name = name.into();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MarkerFrames, RunRecord};

    fn db() -> TimerDatabase {
        let frames = |names: &[&str]| -> MarkerFrames {
            names
                .iter()
                .enumerate()
                .map(|(i, n)| ((*n).to_owned(), i as i64 * 100))
                .collect()
        };
        let course = |id: &str, n| Course {
            id: id.into(),
            name: id.to_uppercase(),
            sector_count: n,
        };
        TimerDatabase {
            courses: vec![course("a", 2), course("b", 2), course("c", 3)],
            runs: vec![
                RunRecord::new(
                    "r1",
                    "a",
                    "2026-06-01",
                    "1.MP4",
                    30.0,
                    frames(&["Start", "S1", "Finish"]),
                ),
                RunRecord::new(
                    "r2",
                    "a",
                    "2026-06-02",
                    "2.MP4",
                    30.0,
                    frames(&["Start", "S1", "Finish"]),
                ),
            ],
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn moves_runs_only_to_a_course_their_markers_fit() {
        let mut d = db();
        move_runs(&mut d, &ids(&["r1", "r2"]), "b").unwrap();
        assert!(d.runs.iter().all(|r| r.course_id == "b"));
        let before = d.clone();
        let err = move_runs(&mut d, &ids(&["r1"]), "c").unwrap_err();
        assert!(matches!(
            err,
            EditError::SectorMismatch {
                has: 2,
                wants: 3,
                ..
            }
        ));
        assert_eq!(
            move_runs(&mut d, &ids(&["r1"]), "zz"),
            Err(EditError::NoCourse("zz".into()))
        );
        assert_eq!(
            move_runs(&mut d, &ids(&["r1", "nope"]), "a"),
            Err(EditError::NoRun("nope".into()))
        );
        assert_eq!(d, before, "failed edits change nothing");
        assert!(crate::validation::validate_database(&d).is_empty());
    }

    #[test]
    fn ignores_and_deletes() {
        let mut d = db();
        set_ignored(&mut d, &ids(&["r2"]), true).unwrap();
        assert!(d.runs[1].ignored && !d.runs[0].ignored);
        delete_runs(&mut d, &ids(&["r1"])).unwrap();
        assert_eq!(d.runs.len(), 1);
    }

    #[test]
    fn adds_and_renames_courses() {
        let mut d = db();
        assert_eq!(
            add_course(&mut d, " Lower Whistler A-Line ", 4).unwrap(),
            "lower_whistler_a_line"
        );
        assert_eq!(
            add_course(&mut d, "a", 1).unwrap_err(),
            EditError::DuplicateName("a".into())
        );
        assert_eq!(add_course(&mut d, "A!", 1).unwrap(), "a_2");
        assert_eq!(add_course(&mut d, "", 1).unwrap_err(), EditError::NoName);
        assert_eq!(
            add_course(&mut d, "x", 0).unwrap_err(),
            EditError::NoSectors
        );
        rename_course(&mut d, "a", "Upper A").unwrap();
        assert_eq!(d.courses[0].name, "Upper A");
        assert_eq!(
            rename_course(&mut d, "b", "upper a").unwrap_err(),
            EditError::DuplicateName("upper a".into())
        );
        assert!(crate::validation::validate_database(&d).is_empty());
    }
}

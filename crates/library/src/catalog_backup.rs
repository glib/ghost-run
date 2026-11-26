//! A daily copy of the catalog (the library database) and the run database
//! on the NAS, in `<archive>/.ghost-run/backups/`: once footage lives only on
//! the NAS, the catalog holds what can't be found again by rescanning (0005).
//! One copy a day, written under a hidden temporary name and renamed, and the
//! newest [`KEEP`] of each kind kept; only files this module names are ever
//! removed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;

use crate::store::Library;

/// How many daily copies of each file are kept.
pub const KEEP: usize = 30;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    /// Files written this time (none when today's copies exist).
    pub written: Vec<PathBuf>,
    /// Old copies removed.
    pub removed: usize,
}

/// The backup folder for an archive.
pub fn folder(archive: &Path) -> PathBuf {
    archive.join(".ghost-run").join("backups")
}

/// Backs up the library and, if given, the run database into `archive`'s
/// backup folder for `today`, unless today's copies are there already.
pub fn run(
    library: &Library,
    run_database: Option<&Path>,
    archive: &Path,
    today: NaiveDate,
) -> Result<Report> {
    anyhow::ensure!(
        archive.is_dir(),
        "the NAS archive {} isn't there (is the share mounted?)",
        archive.display()
    );
    let dir = folder(archive);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut report = Report::default();
    let library_copy = dir.join(format!("library-{today}.sqlite3"));
    if !library_copy.exists() {
        let tmp = dir.join(format!(".library-{today}.partial"));
        let _ = std::fs::remove_file(&tmp);
        library.copy_to(&tmp)?;
        std::fs::rename(&tmp, &library_copy)?;
        report.written.push(library_copy);
    }
    if let Some(db) = run_database.filter(|p| p.is_file()) {
        let copy = dir.join(format!("timer_db-{today}.yaml"));
        if !copy.exists() {
            let tmp = dir.join(format!(".timer_db-{today}.partial"));
            std::fs::copy(db, &tmp).with_context(|| format!("copying {}", db.display()))?;
            std::fs::rename(&tmp, &copy)?;
            report.written.push(copy);
        }
    }
    for (prefix, suffix) in [("library-", ".sqlite3"), ("timer_db-", ".yaml")] {
        report.removed += prune(&dir, prefix, suffix)?;
    }
    Ok(report)
}

/// Removes all but the newest [`KEEP`] `<prefix>YYYY-MM-DD<suffix>` files.
fn prune(dir: &Path, prefix: &str, suffix: &str) -> Result<usize> {
    let mut dated: Vec<(NaiveDate, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some((NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?, e.path()))
        })
        .collect();
    dated.sort();
    let extra = dated.len().saturating_sub(KEEP);
    for (_, path) in &dated[..extra] {
        std::fs::remove_file(path)?;
    }
    Ok(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_copy_a_day_and_the_newest_thirty_kept() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("nas");
        std::fs::create_dir(&archive).unwrap();
        let library = Library::open(&d.path().join("library.sqlite3")).unwrap();
        let runs = d.path().join("timer_db.yaml");
        std::fs::write(&runs, "schema_version: 2\n").unwrap();
        let day =
            |n: i64| NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(n as u64);

        let first = run(&library, Some(&runs), &archive, day(0)).unwrap();
        assert_eq!(first.written.len(), 2);
        // A copy opens as a library.
        Library::open(&first.written[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.written[1]).unwrap(),
            "schema_version: 2\n"
        );
        // Again the same day: nothing new.
        assert_eq!(
            run(&library, Some(&runs), &archive, day(0)).unwrap(),
            Report::default()
        );

        let unrelated = folder(&archive).join("notes.txt");
        std::fs::write(&unrelated, "mine").unwrap();
        for n in 1..=31 {
            run(&library, Some(&runs), &archive, day(n)).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(folder(&archive))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2 * KEEP + 1);
        assert_eq!(names[0], "library-2026-01-03.sqlite3");
        assert!(names.contains(&"notes.txt".to_owned()));
        assert!(names.contains(&"timer_db-2026-02-01.yaml".to_owned()));
    }

    #[test]
    fn a_missing_share_is_an_error_not_a_new_folder() {
        let d = tempfile::tempdir().unwrap();
        let library = Library::open(&d.path().join("library.sqlite3")).unwrap();
        let archive = d.path().join("not-mounted");
        let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(run(&library, None, &archive, today).is_err());
        assert!(!archive.exists());
    }
}

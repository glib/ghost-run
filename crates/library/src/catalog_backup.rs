//! A daily copy of the catalog (the library database) and the run database
//! on the NAS, in `<archive>/.ghost-run/backups/`: once footage lives only on
//! the NAS, the catalog holds what can't be found again by rescanning (0005).
//! One copy a day, the newest [`KEEP`] of each kind kept; only files this
//! module names are ever removed.
//!
//! SQLite never writes to the share: its locks aren't reliable over SMB, and
//! a `VACUUM INTO` straight onto a CIFS mount was seen to hang on a 0-byte
//! file (#54). The library is copied into a local staging folder first; then
//! plain file copies go to the share under a hidden temporary name, are
//! synced, read back and compared, and renamed. Each file is tried on its
//! own, so one failure doesn't skip the other, and the share's part has a
//! time limit, so a hung mount is reported rather than waited on for ever.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;

use crate::store::Library;

/// How many daily copies of each file are kept.
pub const KEEP: usize = 30;

/// How long the copies onto the share may take.
pub const SHARE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    /// Files written this time (none when today's copies exist).
    pub written: Vec<PathBuf>,
    /// Old copies removed.
    pub removed: usize,
    /// What couldn't be backed up, and why.
    pub failed: Vec<String>,
}

/// The backup folder for an archive.
pub fn folder(archive: &Path) -> PathBuf {
    archive.join(".ghost-run").join("backups")
}

/// Backs up the library and, if given, the run database into `archive`'s
/// backup folder for `today`, unless today's copies are there already.
/// `staging` is a local folder the library is copied into first. Errors only
/// when the share isn't there; each file's failure is in the report.
pub fn run(
    library: &Library,
    run_database: Option<&Path>,
    archive: &Path,
    today: NaiveDate,
    staging: &Path,
) -> Result<Report> {
    run_with(
        library,
        run_database,
        archive,
        today,
        staging,
        SHARE_TIMEOUT,
    )
}

fn run_with(
    library: &Library,
    run_database: Option<&Path>,
    archive: &Path,
    today: NaiveDate,
    staging: &Path,
    timeout: Duration,
) -> Result<Report> {
    ensure!(
        archive.is_dir(),
        "the NAS archive {} isn't there (is the share mounted?)",
        archive.display()
    );
    let dir = folder(archive);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut report = Report::default();
    // (local file, name on the share)
    let mut copies: Vec<(PathBuf, String)> = Vec::new();
    let library_name = format!("library-{today}.sqlite3");
    if !dir.join(&library_name).exists() {
        let staged = staging.join(&library_name);
        let made = std::fs::create_dir_all(staging)
            .with_context(|| format!("creating {}", staging.display()))
            .and_then(|()| {
                let _ = std::fs::remove_file(&staged);
                library.copy_to(&staged)
            });
        match made {
            Ok(()) => copies.push((staged, library_name)),
            Err(e) => report.failed.push(format!("the library: {e:#}")),
        }
    }
    if let Some(db) = run_database.filter(|p| p.is_file()) {
        let name = format!("timer_db-{today}.yaml");
        if !dir.join(&name).exists() {
            copies.push((db.to_owned(), name));
        }
    }

    // The share's part, on a thread of its own with a time limit.
    let names: Vec<String> = copies.iter().map(|(_, n)| n.clone()).collect();
    let (tx, rx) = mpsc::channel();
    let share = dir.clone();
    std::thread::spawn(move || {
        for (src, name) in copies {
            let result = copy_checked(&src, &share, &name);
            if tx.send((name, result)).is_err() {
                return;
            }
        }
    });
    let deadline = std::time::Instant::now() + timeout;
    let mut done = 0;
    while done < names.len() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((name, result)) => {
                done += 1;
                match result {
                    Ok(path) => report.written.push(path),
                    Err(e) => report.failed.push(format!("{name}: {e:#}")),
                }
            }
            Err(_) => {
                report.failed.push(format!(
                    "{}: the share didn't finish writing within {} s",
                    names[done..].join(", "),
                    timeout.as_secs()
                ));
                break;
            }
        }
    }
    let _ = std::fs::remove_file(staging.join(format!("library-{today}.sqlite3")));
    if report.failed.is_empty() {
        for (prefix, suffix) in [("library-", ".sqlite3"), ("timer_db-", ".yaml")] {
            report.removed += prune(&dir, prefix, suffix)?;
        }
        remove_partials(&dir)?;
    }
    Ok(report)
}

/// Copies `src` into `dir` as `name` with plain file writes: a hidden
/// temporary name, synced, read back and compared, then renamed.
fn copy_checked(src: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let tmp = dir.join(format!(".{name}.partial"));
    let target = dir.join(name);
    let _ = std::fs::remove_file(&tmp);
    let written = (|| -> Result<()> {
        let mut from = File::open(src).with_context(|| format!("reading {}", src.display()))?;
        let mut to = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        std::io::copy(&mut from, &mut to)?;
        to.flush()?;
        to.sync_all()?;
        drop(to);
        ensure!(same_bytes(src, &tmp)?, "the copy read back differently");
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| target)
}

fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// Removes temporary copies earlier runs left (`.library-<date>.partial` from
/// before #54, `.<name>.partial` since), once today's copies are in.
fn remove_partials(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = [".library-", ".timer_db-"]
            .iter()
            .any(|p| name.starts_with(p));
        if ours && name.ends_with(".partial") {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
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

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Days::new(n as u64)
    }

    #[test]
    fn one_copy_a_day_and_the_newest_thirty_kept() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("nas");
        std::fs::create_dir(&archive).unwrap();
        let staging = d.path().join("staging");
        let library = Library::open(&d.path().join("library.sqlite3")).unwrap();
        let runs = d.path().join("timer_db.yaml");
        std::fs::write(&runs, "schema_version: 2\n").unwrap();

        let first = run(&library, Some(&runs), &archive, day(0), &staging).unwrap();
        assert_eq!(first.written.len(), 2, "{:?}", first.failed);
        // A copy opens as a library.
        Library::open(&first.written[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.written[1]).unwrap(),
            "schema_version: 2\n"
        );
        // Only the two files reached the share (no SQLite journal or
        // temporary files), and nothing is left in staging.
        let mut on_share: Vec<String> = std::fs::read_dir(folder(&archive))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        on_share.sort();
        assert_eq!(
            on_share,
            ["library-2026-01-01.sqlite3", "timer_db-2026-01-01.yaml"]
        );
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        // Again the same day: nothing new.
        assert_eq!(
            run(&library, Some(&runs), &archive, day(0), &staging).unwrap(),
            Report::default()
        );

        let unrelated = folder(&archive).join("notes.txt");
        std::fs::write(&unrelated, "mine").unwrap();
        // A hung run's leftover (#54) goes once a backup succeeds.
        let leftover = folder(&archive).join(".library-2026-01-01.partial");
        std::fs::write(&leftover, "").unwrap();
        for n in 1..=31 {
            run(&library, Some(&runs), &archive, day(n), &staging).unwrap();
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
        assert!(!leftover.exists());
    }

    #[test]
    fn the_run_database_is_copied_when_the_library_fails() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("nas");
        std::fs::create_dir(&archive).unwrap();
        // Staging can't be made: a file is in the way.
        let staging = d.path().join("staging");
        std::fs::write(&staging, "").unwrap();
        let library = Library::open(&d.path().join("library.sqlite3")).unwrap();
        let runs = d.path().join("timer_db.yaml");
        std::fs::write(&runs, "schema_version: 2\n").unwrap();
        let report = run(&library, Some(&runs), &archive, day(0), &staging).unwrap();
        assert_eq!(
            report.written,
            [folder(&archive).join("timer_db-2026-01-01.yaml")]
        );
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].starts_with("the library"));
    }

    #[test]
    fn a_missing_share_is_an_error_not_a_new_folder() {
        let d = tempfile::tempdir().unwrap();
        let library = Library::open(&d.path().join("library.sqlite3")).unwrap();
        let archive = d.path().join("not-mounted");
        assert!(run(&library, None, &archive, day(0), d.path()).is_err());
        assert!(!archive.exists());
    }

    #[test]
    fn copies_are_checked_before_they_count() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a");
        std::fs::write(&src, vec![7u8; 200_000]).unwrap();
        let path = copy_checked(&src, d.path(), "b").unwrap();
        assert!(same_bytes(&src, &path).unwrap());
        std::fs::write(&path, vec![7u8; 199_999]).unwrap();
        assert!(!same_bytes(&src, &path).unwrap());
        assert!(copy_checked(&d.path().join("missing"), d.path(), "c").is_err());
        assert!(!d.path().join(".c.partial").exists());
    }
}

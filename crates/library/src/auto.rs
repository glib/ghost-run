//! Auto-markers in the app: each course has a reference run (one chosen by
//! hand, else its best lap whose clip can be found); a clip's markers are
//! placed by lining its footage up with the reference's
//! ([`timer_align::align`]), and the course a clip is from is the one whose
//! reference matches it best.
//!
//! Footage is read from proxies when there are any (they have the same frames
//! as the originals and decode far faster), and the small frames read are
//! cached, so a reference is decoded once however many clips use it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use timer_align::align;
use timer_align::video::read_frames;
use timer_core::TimerDatabase;
use timer_core::commit::original_name;
use timer_core::markers::{expected_marker_names, parse_marker_snapshot, raw_markers};
use timer_core::models::{Course, MarkerFrames, RunRecord};
use timer_core::stats::{StatsOptions, compute_course_stats};

use crate::store::{Clip, Library, ProxyState};

/// A placed marker less sure than this is worth checking by eye.
pub const CHECK_BELOW: f64 = 0.3;
/// A match this poor or worse means the run probably isn't in the clip.
pub const POOR_MATCH: f64 = 0.5;

/// A course's reference: a run with its markers, and the footage to read.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub course_id: String,
    pub run_id: String,
    pub lap_seconds: f64,
    /// The file read: the run's clip's proxy if it has one, else the clip.
    pub footage: PathBuf,
    /// Marker name → frame, in order along the course.
    pub markers: Vec<(String, i64)>,
    /// Chosen by hand (rather than the best lap with footage).
    pub chosen: bool,
}

/// Markers placed in a clip from a course's reference.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub course_id: String,
    pub reference_run: String,
    pub markers: MarkerFrames,
    pub confidence: BTreeMap<String, f64>,
    /// 0 is identical footage, 1 unrelated (see [`POOR_MATCH`]).
    pub match_cost: f64,
}

impl Placement {
    /// Markers worth checking by eye, in course order.
    pub fn to_check(&self) -> Vec<String> {
        let mut names: Vec<(&i64, &String)> = self
            .confidence
            .iter()
            .filter(|(_, c)| **c < CHECK_BELOW)
            .filter_map(|(n, _)| self.markers.get(n).map(|f| (f, n)))
            .collect();
        names.sort();
        names.into_iter().map(|(_, n)| n.clone()).collect()
    }

    pub fn quality(&self) -> &'static str {
        match self.match_cost {
            c if c < 0.3 => "good",
            c if c < POOR_MATCH => "fair",
            _ => "poor",
        }
    }
}

/// The file to read a clip's frames from: its proxy if it has one.
pub fn footage(clip: &Clip) -> PathBuf {
    match &clip.proxy {
        ProxyState::Ready(proxy) => proxy.clone(),
        _ => clip.path.clone(),
    }
}

/// The library clip a stored run was timed on: its source path if that's
/// still there, else a clip with the run's file name.
///
/// Paths are also compared after resolving links: on Fedora Atomic `/home`
/// links to `/var/home`, so a run timed at `/home/…/clip.mov` is the library's
/// `/var/home/…/clip.mov`, and its proxy should be used.
pub fn run_clip(run: &RunRecord, clips: &[Clip]) -> Option<Clip> {
    if let Some(path) = run.source_path.as_deref().filter(|p| !p.is_empty()) {
        let path = Path::new(path);
        if let Some(clip) = clips.iter().find(|c| c.path == path) {
            return Some(clip.clone());
        }
        if let Ok(real) = path.canonicalize()
            && let Some(clip) = clips
                .iter()
                .find(|c| c.path.canonicalize().is_ok_and(|p| p == real))
        {
            return Some(clip.clone());
        }
        if path.is_file() {
            return Some(Clip {
                path: path.to_owned(),
                size: 0,
                modified_ns: 0,
                recorded: None,
                recorded_source: String::new(),
                info: None,
                probe_error: None,
                proxy: ProxyState::Missing,
            });
        }
    }
    clips
        .iter()
        .find(|c| original_name(&c.name()) == run.filename)
        .cloned()
}

/// `course`'s reference: the run chosen for it if its footage can be found,
/// else the fastest counted run whose footage can be.
pub fn reference(
    db: &TimerDatabase,
    course: &Course,
    library: &Library,
    clips: &[Clip],
) -> Result<Option<Reference>> {
    let chosen = library.reference(&course.id)?;
    let stats = compute_course_stats(course, &db.runs, StatsOptions::default());
    let mut runs: Vec<(&RunRecord, f64)> = stats
        .eligible_runs
        .iter()
        .map(|r| (r.run, r.timing.lap_seconds))
        .collect();
    runs.sort_by(|a, b| a.1.total_cmp(&b.1));
    if let Some(id) = &chosen
        && let Some(i) = runs.iter().position(|(r, _)| &r.id == id)
    {
        let first = runs.remove(i);
        runs.insert(0, first);
    }
    let names = expected_marker_names(course)?;
    for (run, lap) in runs {
        let Some(clip) = run_clip(run, clips) else {
            continue;
        };
        let Ok(snapshot) = parse_marker_snapshot(&raw_markers(&run.marker_frames), course) else {
            continue;
        };
        return Ok(Some(Reference {
            course_id: course.id.clone(),
            run_id: run.id.clone(),
            lap_seconds: lap,
            footage: footage(&clip),
            markers: names
                .iter()
                .map(|n| (n.clone(), snapshot.frames[n.as_str()]))
                .collect(),
            chosen: chosen.as_deref() == Some(run.id.as_str()),
        }));
    }
    Ok(None)
}

/// Every course's reference, for telling which course a clip is from.
pub fn references(db: &TimerDatabase, library: &Library, clips: &[Clip]) -> Result<Vec<Reference>> {
    let mut out = Vec::new();
    for course in &db.courses {
        if let Some(r) = reference(db, course, library, clips)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// Places `reference`'s markers in the clip whose frames are in `footage`.
/// `cache`: where decoded frames are kept for next time.
pub fn place(reference: &Reference, footage: &Path, cache: Option<&Path>) -> Result<Placement> {
    let reference_frames = read_frames(&reference.footage, cache).with_context(|| {
        format!(
            "reading the reference run {} ({})",
            reference.run_id,
            reference.footage.display()
        )
    })?;
    let clip_frames = read_frames(footage, cache)?;
    let alignment = align(&reference_frames, &reference.markers, &clip_frames)?;
    Ok(Placement {
        course_id: reference.course_id.clone(),
        reference_run: reference.run_id.clone(),
        markers: alignment
            .markers
            .iter()
            .map(|m| (m.name.clone(), m.frame))
            .collect(),
        confidence: alignment
            .markers
            .iter()
            .map(|m| (m.name.clone(), m.confidence))
            .collect(),
        match_cost: alignment.match_cost,
    })
}

/// Places the clip against every reference; best match first. References
/// that can't be used (footage gone, clip too short) are left out.
pub fn match_courses(
    references: &[Reference],
    footage: &Path,
    cache: Option<&Path>,
) -> Result<Vec<Placement>> {
    if references.is_empty() {
        bail!(
            "no course has a run to match against yet: time one run on each course by hand first"
        );
    }
    let mut placements = Vec::new();
    let mut first_error = None;
    for reference in references {
        match place(reference, footage, cache) {
            Ok(p) => placements.push(p),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    if placements.is_empty() {
        return Err(first_error.expect("at least one reference"));
    }
    placements.sort_by(|a, b| a.match_cost.total_cmp(&b.match_cost));
    Ok(placements)
}

/// Where decoded frames are cached: the same place as `rtimer auto-markers`.
pub fn cache_dir() -> Option<PathBuf> {
    timer_align::video::default_cache_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip_at(path: &Path) -> Clip {
        Clip {
            path: path.to_owned(),
            size: 1,
            modified_ns: 1,
            recorded: None,
            recorded_source: String::new(),
            info: None,
            probe_error: None,
            proxy: ProxyState::Ready(path.with_extension("proxy.mov")),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_run_timed_through_a_linked_folder_finds_the_library_clip() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("var-home");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("a.mov"), b"x").unwrap();
        let link = dir.path().join("home");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut run = RunRecord::new(
            "r",
            "c",
            "2026-01-01",
            "other.mov",
            30.0,
            MarkerFrames::new(),
        );
        run.source_path = Some(link.join("a.mov").display().to_string());
        let clips = [clip_at(&real.join("b.mov")), clip_at(&real.join("a.mov"))];
        let found = run_clip(&run, &clips).unwrap();
        assert_eq!(found, clips[1]);
        assert_eq!(footage(&found), real.join("a.proxy.mov"));
    }

    #[test]
    fn markers_to_check_are_listed_in_course_order() {
        let p = Placement {
            course_id: "c".into(),
            reference_run: "r".into(),
            markers: [("Start", 10), ("S1", 50), ("Finish", 90)]
                .iter()
                .map(|(n, f)| ((*n).to_owned(), *f))
                .collect(),
            confidence: [("Start", 0.1), ("S1", 0.9), ("Finish", 0.2)]
                .iter()
                .map(|(n, c)| ((*n).to_owned(), *c))
                .collect(),
            match_cost: 0.2,
        };
        assert_eq!(p.to_check(), ["Start", "Finish"]);
        assert_eq!(p.quality(), "good");
    }
}

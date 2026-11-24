//! `rtimer auto-markers`: places a new clip's markers from a reference run
//! and writes them as a marker CSV (`name,frame,confidence`), ready for
//! `overlay-text`, `render` or checking by eye.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use timer_align::video::{default_cache_dir, read_frames};
use timer_align::{Alignment, align};
use timer_core::markers::{expected_marker_names, parse_marker_snapshot, raw_markers};
use timer_core::models::{Course, MarkerSnapshot};
use timer_core::timing::{compute_timing, format_delta};
use timer_core::{RawMarker, TimerDatabase};

use crate::selected::read_marker_csv;

#[derive(Args)]
pub struct AutoMarkersArgs {
    /// Stored run whose markers to copy
    #[arg(
        long,
        required_unless_present = "reference_markers",
        conflicts_with = "reference_markers"
    )]
    reference_run: Option<String>,
    /// Marker CSV (name,frame) for the reference clip, instead of a stored run
    #[arg(long, requires = "course")]
    reference_markers: Option<String>,
    /// The reference run's clip (default: the stored run's source_path)
    #[arg(long)]
    reference_clip: Option<PathBuf>,
    /// Course ID (default: the reference run's)
    #[arg(long)]
    course: Option<String>,
    /// The clip to place markers in
    #[arg(long)]
    clip: PathBuf,
    /// Write the marker CSV here instead of to standard output
    #[arg(long)]
    out: Option<PathBuf>,
    /// Where decoded clips are kept for next time (default: your cache folder)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Don't read or write the cache
    #[arg(long)]
    no_cache: bool,
}

/// Confidence below this is flagged for checking by eye.
const CHECK_BELOW: f64 = 0.3;

pub fn run(db: &TimerDatabase, args: &AutoMarkersArgs) -> Result<(), Box<dyn std::error::Error>> {
    let (course, reference, reference_clip, reference_fps) = reference(db, args)?;
    let names = expected_marker_names(course)?;
    let markers: Vec<(String, i64)> = names
        .iter()
        .map(|n| (n.clone(), reference.frames[n.as_str()]))
        .collect();

    let cache = if args.no_cache {
        None
    } else {
        args.cache_dir.clone().or_else(default_cache_dir)
    };
    let began = Instant::now();
    eprintln!("Reading {} ...", reference_clip.display());
    let reference_frames = read_frames(&reference_clip, cache.as_deref())?;
    eprintln!("Reading {} ...", args.clip.display());
    let clip_frames = read_frames(&args.clip, cache.as_deref())?;
    let read_secs = began.elapsed().as_secs_f64();
    let began = Instant::now();
    let alignment = align(&reference_frames, &markers, &clip_frames)?;
    eprintln!(
        "Read in {read_secs:.1} s, matched in {:.1} s.",
        began.elapsed().as_secs_f64()
    );

    let csv = marker_csv(&alignment);
    match &args.out {
        Some(path) => std::fs::write(path, &csv)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?,
        None => print!("{csv}"),
    }
    eprint!(
        "{}",
        report(
            course,
            &alignment,
            &reference,
            reference_fps.unwrap_or(reference_frames.fps),
            clip_frames.fps
        )
    );
    Ok(())
}

type Reference<'a> = (&'a Course, MarkerSnapshot, PathBuf, Option<f64>);

/// The course, the reference markers, its clip, and the stored run's fps.
fn reference<'a>(
    db: &'a TimerDatabase,
    args: &AutoMarkersArgs,
) -> Result<Reference<'a>, Box<dyn std::error::Error>> {
    if let Some(run_id) = &args.reference_run {
        let run = db
            .runs
            .iter()
            .find(|r| &r.id == run_id)
            .ok_or_else(|| format!("run not found: {run_id}"))?;
        if let Some(course) = &args.course
            && course != &run.course_id
        {
            return Err(
                format!("run {run_id} is on course {}, not {course}", run.course_id).into(),
            );
        }
        let course = db.course_by_id(&run.course_id)?;
        let snapshot = parse_marker_snapshot(&raw_markers(&run.marker_frames), course)?;
        let clip = match (&args.reference_clip, run.source_path.as_deref()) {
            (Some(clip), _) => clip.clone(),
            (None, Some(path)) if !path.is_empty() => PathBuf::from(path),
            _ => {
                return Err(format!(
                    "run {run_id} has no source_path; give its clip with --reference-clip"
                )
                .into());
            }
        };
        return Ok((course, snapshot, clip, Some(run.source_fps)));
    }
    let course_id = args.course.as_deref().expect("clap requires --course");
    let course = db.course_by_id(course_id)?;
    let csv = args
        .reference_markers
        .as_deref()
        .expect("clap requires one");
    let markers: Vec<RawMarker> = read_marker_csv(csv)?;
    let snapshot = parse_marker_snapshot(&markers, course)?;
    let clip = args
        .reference_clip
        .clone()
        .ok_or("--reference-markers needs --reference-clip")?;
    Ok((course, snapshot, clip, None))
}

fn marker_csv(alignment: &Alignment) -> String {
    let mut out = String::from("name,frame,confidence\n");
    for m in &alignment.markers {
        let _ = writeln!(out, "{},{},{:.2}", m.name, m.frame, m.confidence);
    }
    out
}

/// A readable summary: how well the clip matched, each marker, and the
/// sector times against the reference's.
fn report(
    course: &Course,
    alignment: &Alignment,
    reference: &MarkerSnapshot,
    reference_fps: f64,
    clip_fps: f64,
) -> String {
    let mut out = String::new();
    let quality = match alignment.match_cost {
        c if c < 0.3 => "good",
        c if c < 0.5 => "fair",
        _ => "poor: is this run in the clip?",
    };
    let _ = writeln!(
        out,
        "Match {:.2} ({quality}; 0 is identical footage, 1 unrelated)",
        alignment.match_cost
    );
    for m in &alignment.markers {
        let flag = if m.confidence < CHECK_BELOW {
            "  <- check"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  {:<8} frame {:>7}  confidence {:.2}{flag}",
            m.name, m.frame, m.confidence
        );
    }
    let placed: Vec<RawMarker> = alignment
        .markers
        .iter()
        .map(|m| RawMarker::new(m.name.clone(), m.frame))
        .collect();
    let timing = parse_marker_snapshot(&placed, course)
        .map_err(|e| e.to_string())
        .and_then(|s| compute_timing(&s, course, clip_fps).map_err(|e| e.to_string()));
    let reference = compute_timing(reference, course, reference_fps);
    match (timing, reference) {
        (Ok(t), Ok(r)) => {
            let _ = writeln!(out, "Sectors (against the reference run):");
            for (s, rs) in t.sectors.iter().zip(&r.sectors) {
                let _ = writeln!(
                    out,
                    "  S{:<7} {:>9.3} s  {}",
                    s.sector,
                    s.duration_seconds,
                    format_delta(s.duration_seconds - rs.duration_seconds)
                );
            }
            let _ = writeln!(
                out,
                "  LAP      {:>9.3} s  {}",
                t.lap_seconds,
                format_delta(t.lap_seconds - r.lap_seconds)
            );
        }
        (Err(e), _) => {
            let _ = writeln!(out, "The placed markers don't make a valid run: {e}");
        }
        (_, Err(e)) => {
            let _ = writeln!(out, "The reference run isn't valid: {e}");
        }
    }
    out
}

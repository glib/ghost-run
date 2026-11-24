//! Auto-markers end to end on generated footage: a clip that is the
//! reference course's footage cut a second later gets the reference's markers
//! a second earlier, and is told apart from another course's footage.

use std::path::Path;
use std::process::Command;

use library::Library;
use library::auto::{match_courses, place, reference, references};
use timer_core::TimerDatabase;
use timer_core::models::{Course, MarkerFrames, RunRecord};

fn have_ffmpeg() -> bool {
    let ok = Command::new("ffmpeg").arg("-version").output().is_ok();
    assert!(
        ok || std::env::var_os("REQUIRE_FFMPEG").is_none(),
        "ffmpeg is required (REQUIRE_FFMPEG is set)"
    );
    ok
}

fn make(path: &Path, source: &str, filter: &str) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", source])
        .args([
            "-vf", filter, "-c:v", "libx264", "-g", "30", "-pix_fmt", "yuv420p",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

fn frames(items: &[(&str, i64)]) -> MarkerFrames {
    items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect()
}

fn run(id: &str, course: &str, clip: &Path, markers: MarkerFrames) -> RunRecord {
    let mut r = RunRecord::new(
        id,
        course,
        "2026-08-17",
        clip.file_name().unwrap().to_string_lossy(),
        30.0,
        markers,
    );
    r.source_path = Some(clip.display().to_string());
    r.committed = true;
    r
}

#[test]
fn places_markers_and_tells_courses_apart() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ridge = dir.path().join("ridge.mp4");
    let other = dir.path().join("other.mp4");
    let new = dir.path().join("new.mp4");
    make(&ridge, "testsrc2=s=320x180:r=30:d=8", "null");
    make(
        &other,
        "life=s=320x180:r=30:mold=10:ratio=0.2:seed=7:life_color=#00ff00",
        "trim=end_frame=240",
    );
    // The ridge footage again, starting 30 frames later.
    make(
        &new,
        "testsrc2=s=320x180:r=30:d=8",
        "trim=start_frame=30,setpts=PTS-STARTPTS",
    );

    let db = TimerDatabase {
        courses: vec![
            Course {
                id: "ridge".into(),
                name: "Ridge".into(),
                sector_count: 2,
            },
            Course {
                id: "other".into(),
                name: "Other".into(),
                sector_count: 1,
            },
        ],
        runs: vec![
            run(
                "r1",
                "ridge",
                &ridge,
                frames(&[("Start", 60), ("S1", 120), ("Finish", 180)]),
            ),
            run(
                "o1",
                "other",
                &other,
                frames(&[("Start", 30), ("Finish", 200)]),
            ),
        ],
    };
    let library = Library::open(&dir.path().join("l.sqlite3")).unwrap();
    let cache = dir.path().join("cache");

    let r = reference(&db, &db.courses[0], &library, &[])
        .unwrap()
        .unwrap();
    assert_eq!(r.run_id, "r1");
    assert!(!r.chosen);
    let placed = place(&r, &new, Some(&cache)).unwrap();
    assert_eq!(placed.course_id, "ridge");
    for (name, want) in [("Start", 30), ("S1", 90), ("Finish", 150)] {
        let got = placed.markers[name];
        assert!((got - want).abs() <= 1, "{name}: {got}, expected {want}");
    }
    assert_eq!(placed.quality(), "good", "cost {}", placed.match_cost);

    let all = references(&db, &library, &[]).unwrap();
    assert_eq!(all.len(), 2);
    let ranked = match_courses(&all, &new, Some(&cache)).unwrap();
    assert_eq!(ranked[0].course_id, "ridge");
    assert!(ranked.len() < 2 || ranked[1].match_cost > ranked[0].match_cost);

    // A run chosen by hand is the reference, even if it isn't the fastest.
    library.set_reference("ridge", "r1").unwrap();
    assert!(
        reference(&db, &db.courses[0], &library, &[])
            .unwrap()
            .unwrap()
            .chosen
    );
}

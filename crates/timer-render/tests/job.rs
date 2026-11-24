//! Rendering through `job::render`: progress reaches the end, the result has
//! the clip's timecode, and a stopped render leaves nothing behind.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use timer_core::TimerDatabase;
use timer_core::models::{Course, RawMarker};
use timer_core::overlay::{ComparisonMode, OverlayPayload, SelectedRun, overlay_payload};
use timer_render::encode::{SourceInfo, probe};
use timer_render::job::{self, RenderError, Spec};

fn have_ffmpeg() -> bool {
    let ok = Command::new("ffmpeg").arg("-version").output().is_ok();
    assert!(
        ok || std::env::var_os("REQUIRE_FFMPEG").is_none(),
        "ffmpeg is required (REQUIRE_FFMPEG is set)"
    );
    ok
}

fn payload() -> OverlayPayload {
    let db = TimerDatabase {
        courses: vec![Course {
            id: "c".into(),
            name: "C".into(),
            sector_count: 1,
        }],
        runs: vec![],
    };
    let selected = SelectedRun {
        course_id: "c".into(),
        filename: "clip.mp4".into(),
        source_fps: 30.0,
        markers: vec![RawMarker::new("Start", 30), RawMarker::new("Finish", 300)],
        clip_id: None,
        run_date: Some("2026-08-17".into()),
        capture_time: None,
        capture_time_source: None,
        source_path: None,
    };
    overlay_payload(&db, &selected, ComparisonMode::BestLap).unwrap()
}

fn source(dir: &Path) -> SourceInfo {
    let clip = dir.join("clip.mp4");
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x180:r=30:d=12",
        ])
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-timecode",
            "01:00:00:00",
        ])
        .arg(&clip)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    probe(&clip).unwrap()
}

#[test]
fn renders_with_progress_and_stops_cleanly() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let info = source(dir.path());
    let payload = payload();
    let out = dir.path().join("clip.overlay.mov");
    let spec = Spec {
        jobs: Some(2),
        threads: Some(1),
        ..Spec::new(&payload, &out, Some(&info))
    };

    let seen = Mutex::new(Vec::new());
    let report = job::render(
        &spec,
        &|done, total| seen.lock().unwrap().push((done, total)),
        &AtomicBool::new(false),
    )
    .unwrap();
    // 1 s before Start to 5 s after Finish: frames 0..=359 (the clip ends there).
    assert_eq!((report.first, report.last), (0, 359));
    assert_eq!(report.timecode, "01:00:00:00");
    assert_eq!(seen.lock().unwrap().last(), Some(&(360, 360)));
    assert!(out.exists());
    assert!(!job::partial_path(&out).exists());

    // Stopped part way: an error, and neither the file nor its partial.
    std::fs::remove_file(&out).unwrap();
    let cancel = AtomicBool::new(false);
    let result = job::render(
        &spec,
        &|done, _| {
            if done >= 40 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(RenderError::Cancelled)), "{result:?}");
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["clip.mp4"], "left behind: {left:?}");
}

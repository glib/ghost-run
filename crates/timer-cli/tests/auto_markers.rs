//! `rtimer auto-markers` end to end on encoded video: a made-up "trail"
//! filmed twice (different speed, frame rate and exposure). Needs ffmpeg;
//! skipped without it unless `REQUIRE_FFMPEG` is set (as in CI).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn have_ffmpeg() -> bool {
    let found = Command::new("ffmpeg").arg("-version").output().is_ok();
    if !found && std::env::var_os("REQUIRE_FFMPEG").is_some() {
        panic!("REQUIRE_FFMPEG is set but ffmpeg isn't on PATH");
    }
    found
}

const WORLD_W: usize = 900;
const WORLD_H: usize = 40;
const SCALE: f64 = 5.0; // video pixels per world pixel
const W: usize = 320;
const H: usize = 180;

/// Smooth random texture: the trail.
fn world() -> Vec<f32> {
    let mut state = 12345u64;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 10_000) as f32 / 10_000.0
    };
    let mut out = vec![0.0; WORLD_W * WORLD_H];
    for (scale, amp) in [(16usize, 1.0f32), (5, 0.5), (2, 0.25)] {
        let gw = WORLD_W / scale + 2;
        let grid: Vec<f32> = (0..gw * (WORLD_H / scale + 2)).map(|_| rand()).collect();
        for y in 0..WORLD_H {
            for x in 0..WORLD_W {
                let (fx, fy) = (x as f32 / scale as f32, y as f32 / scale as f32);
                let (x0, y0, tx, ty) = (fx as usize, fy as usize, fx.fract(), fy.fract());
                let g = |a: usize, b: usize| grid[b * gw + a];
                let top = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
                let bottom = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
                out[y * WORLD_W + x] += amp * (top * (1.0 - ty) + bottom * ty);
            }
        }
    }
    out
}

/// View positions (world x of the left edge) per frame for one ride.
fn ride(fps: f64, wait: f64, speed: impl Fn(f64) -> f64) -> Vec<f64> {
    let mut out = vec![20.0; (wait * fps) as usize];
    let (mut p, mut t) = (20.0, 0.0);
    while p < 800.0 {
        out.push(p);
        p += speed(t) / fps;
        t += 1.0 / fps;
    }
    out.extend(std::iter::repeat_n(800.0, (wait * fps) as usize));
    out
}

fn encode(path: &Path, world: &[f32], positions: &[f64], fps: u32, gain: f32, offset: f32) {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "-s",
            &format!("{W}x{H}"),
        ])
        .args(["-framerate", &fps.to_string(), "-i", "-"])
        .args([
            "-c:v",
            "libx264",
            "-crf",
            "18",
            "-g",
            &(fps / 2).to_string(),
        ])
        .args(["-pix_fmt", "yuv420p"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut frame = vec![0u8; W * H];
    for &p in positions {
        for row in 0..H {
            for col in 0..W {
                let wx = p + col as f64 / SCALE;
                let wy = 4.0 + row as f64 / SCALE;
                let (x0, y0) = (wx as usize, wy as usize);
                let (tx, ty) = (wx.fract() as f32, wy.fract() as f32);
                let g =
                    |a: usize, b: usize| world[b.min(WORLD_H - 1) * WORLD_W + a.min(WORLD_W - 1)];
                let top = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
                let bottom = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
                let v = (top * (1.0 - ty) + bottom * ty) * 90.0 * gain + offset;
                frame[row * W + col] = v.clamp(0.0, 255.0) as u8;
            }
        }
        stdin.write_all(&frame).unwrap();
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

fn crossing(positions: &[f64], x: f64) -> i64 {
    positions.iter().position(|p| *p >= x).unwrap() as i64
}

fn rtimer(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rtimer"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn places_markers_from_a_reference_run() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let world = world();
    let gates = [("Start", 100.0), ("S1", 400.0), ("Finish", 700.0)];

    let ref_pos = ride(60.0, 1.0, |t| 60.0 + 15.0 * (t * 0.8).sin());
    let ref_clip = dir.path().join("reference.mp4");
    encode(&ref_clip, &world, &ref_pos, 60, 1.0, 40.0);
    let new_pos = ride(30.0, 3.0, |t| 45.0 + 20.0 * (t * 0.5).cos());
    let new_clip = dir.path().join("new.mp4");
    encode(&new_clip, &world, &new_pos, 30, 0.75, 25.0);

    let frames: Vec<String> = gates
        .iter()
        .map(|(n, x)| format!("{n}: {}", crossing(&ref_pos, *x)))
        .collect();
    let db = dir.path().join("timer_db.yaml");
    std::fs::write(
        &db,
        format!(
            "schema_version: 2\ncourses:\n- id: trail\n  name: Trail\n  sector_count: 2\nruns:\n\
             - id: ref\n  course_id: trail\n  date: '2026-06-01'\n  filename: reference.mp4\n  \
             source_fps: 60.0\n  source_path: '{}'\n  marker_frames: {{{}}}\n",
            ref_clip.display(),
            frames.join(", ")
        ),
    )
    .unwrap();

    let out = dir.path().join("markers.csv");
    let cache = dir.path().join("cache");
    let args = [
        "auto-markers",
        "--reference-run",
        "ref",
        "--clip",
        new_clip.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--cache-dir",
        cache.to_str().unwrap(),
    ];
    let result = rtimer(&db, &args);
    let report = String::from_utf8_lossy(&result.stderr).into_owned();
    assert!(result.status.success(), "{report}");
    assert!(report.contains("(good;"), "{report}");
    eprintln!("{report}");

    let csv = std::fs::read_to_string(&out).unwrap();
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("name,frame,confidence"));
    for ((name, x), line) in gates.iter().zip(lines) {
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields[0], *name, "{csv}");
        let frame: i64 = fields[1].parse().unwrap();
        let truth = crossing(&new_pos, *x);
        assert!(
            (frame - truth).abs() <= 1,
            "{name}: {frame} vs {truth}\n{report}"
        );
    }

    // Both clips were cached; a second run reads them from there.
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 2);
    assert!(rtimer(&db, &args).status.success());

    // The CSV feeds the overlay commands.
    let text = rtimer(
        &db,
        &[
            "overlay-text",
            "--course",
            "trail",
            "--markers",
            out.to_str().unwrap(),
            "--filename",
            "new.mp4",
            "--fps",
            "30",
        ],
    );
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    assert!(String::from_utf8_lossy(&text.stdout).starts_with("LIVE"));

    // Without a source path or clip, it says what's missing.
    let missing = rtimer(
        &db,
        &[
            "auto-markers",
            "--reference-markers",
            out.to_str().unwrap(),
            "--course",
            "trail",
            "--clip",
            new_clip.to_str().unwrap(),
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--reference-clip"));
}

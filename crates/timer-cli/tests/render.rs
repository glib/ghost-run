//! `rtimer render` end to end: a small overlay file, checked with ffprobe.
//! Needs ffmpeg; skipped without it unless `REQUIRE_FFMPEG` is set (as in CI).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn have_ffmpeg() -> bool {
    let found = Command::new("ffmpeg").arg("-version").output().is_ok()
        && Command::new("ffprobe").arg("-version").output().is_ok();
    if !found && std::env::var_os("REQUIRE_FFMPEG").is_some() {
        panic!("REQUIRE_FFMPEG is set but ffmpeg/ffprobe aren't on PATH");
    }
    found
}

fn rtimer(args: &[&str]) -> String {
    let root = root();
    let out = Command::new(env!("CARGO_BIN_EXE_rtimer"))
        .arg("--db")
        .arg(root.join("examples/timer_db.yaml"))
        .args(["render", "--course", "lower_whistler_a_line", "--markers"])
        .arg(root.join("examples/markers.csv"))
        .args(["--filename", "GX010999.MP4", "--fps", "59.94"])
        .args(["--date", "2026-06-01"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn probe(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_tag_string,pix_fmt,width,height,r_frame_rate,nb_frames:stream_tags=timecode")
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn renders_a_transparent_prores_file_with_timecode() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mov = dir.path().join("overlay.mov");
    let mov_arg = mov.to_str().unwrap();
    // Markers: Start 100 .. Finish 700; one second of pre-roll, no hold.
    let stdout = rtimer(&["--out", mov_arg, "--size", "320x180", "--hold", "0"]);
    assert!(stdout.contains("starts at clip frame 40"), "{stdout}");
    let info = probe(&mov);
    for line in [
        "codec_tag_string=ap4h",
        "width=320",
        "height=180",
        "r_frame_rate=60000/1001",
        "nb_frames=661",
        "TAG:timecode=00:00:00:40",
    ] {
        assert!(info.contains(line), "missing {line} in\n{info}");
    }
    assert!(info.contains("pix_fmt=yuva444p"), "{info}");

    // Over that file as the clip: its size, rate and timecode carry over,
    // and the overlay stops at the clip's last frame.
    let again = dir.path().join("again.mov");
    let stdout = rtimer(&[
        "--out",
        again.to_str().unwrap(),
        "--source",
        mov_arg,
        "--pre-roll",
        "0",
    ]);
    assert!(stdout.contains("timecode 00:00:02:20"), "{stdout}"); // 00:00:00:40 + 100
    let info = probe(&again);
    assert!(info.contains("width=320"), "{info}");
    assert!(info.contains("nb_frames=561"), "{info}"); // frames 100..=660

    // A --fps that isn't the clip's rate would make the timer run fast or slow.
    let out = Command::new(env!("CARGO_BIN_EXE_rtimer"))
        .arg("--db")
        .arg(root().join("examples/timer_db.yaml"))
        .args(["render", "--course", "lower_whistler_a_line", "--markers"])
        .arg(root().join("examples/markers.csv"))
        .args([
            "--filename",
            "x.MP4",
            "--fps",
            "29.97",
            "--source",
            mov_arg,
            "--out",
        ])
        .arg(dir.path().join("wrong.mov"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Use --fps 59.94"), "{err}");
    assert!(!dir.path().join("wrong.mov").exists());

    let png = dir.path().join("finish.png");
    rtimer(&[
        "--out",
        png.to_str().unwrap(),
        "--size",
        "320x180",
        "--still",
        "700",
    ]);
    assert!(probe(&png).contains("width=320"));
}

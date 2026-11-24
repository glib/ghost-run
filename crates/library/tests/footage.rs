//! Scanning and proxies on real (tiny, generated) clips. Needs ffmpeg; skipped
//! without it unless `REQUIRE_FFMPEG` is set (as in CI).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;

use library::scan::{Progress, scan};
use library::{Cancel, Library, ProxyState, proxy};

fn have_ffmpeg() -> bool {
    let found = Command::new("ffmpeg").arg("-version").output().is_ok();
    if !found && std::env::var_os("REQUIRE_FFMPEG").is_some() {
        panic!("REQUIRE_FFMPEG is set but ffmpeg isn't on PATH");
    }
    found
}

/// A 2 s test clip: 29.97 fps with drop-frame timecode, audio, and the
/// upside-down rotation flag DJI cameras write.
fn clip(path: &Path) {
    let tmp = path.with_extension("tmp.mp4");
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=640x360:r=30000/1001:d=2",
        ])
        .args(["-f", "lavfi", "-i", "sine=f=440:d=2", "-shortest"])
        .args([
            "-c:v",
            "libx264",
            "-g",
            "15",
            "-c:a",
            "aac",
            "-timecode",
            "01:02:03;04",
        ])
        .arg(&tmp)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-display_rotation", "180", "-i"])
        .arg(&tmp)
        .args(["-c", "copy"])
        .arg(path)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    std::fs::remove_file(tmp).unwrap();
}

#[test]
fn scans_makes_proxies_and_notices_changes() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let footage = dir.path().join("footage");
    std::fs::create_dir_all(footage.join("day2")).unwrap();
    let a = footage.join("DJI_20260817075236_0012_D.MP4");
    let b = footage.join("day2/run.mp4");
    clip(&a);
    clip(&b);
    std::fs::write(footage.join("broken.mp4"), b"not a video").unwrap();
    let library = Library::open(&dir.path().join("library.sqlite3")).unwrap();
    let cancel = Cancel::default();
    let folders = vec![footage.clone()];

    let mut probing = 0;
    let report = scan(&folders, &library, &cancel, |p| {
        if let Progress::Probing { .. } = p {
            probing += 1
        }
    })
    .unwrap();
    assert_eq!((report.found, report.probed, report.unreadable), (3, 3, 1));
    assert_eq!(probing, 3);
    let clips = library.clips().unwrap();
    // The unreadable file is dated by its modification time (today), so first.
    let names: Vec<String> = clips.iter().map(|c| c.name()).collect();
    assert_eq!(
        names,
        ["broken.mp4", "run.mp4", "DJI_20260817075236_0012_D.MP4"]
    );
    let first = clips.iter().find(|c| c.path == a).unwrap();
    assert_eq!(first.recorded.as_deref(), Some("2026-08-17 07:52:36"));
    assert_eq!(first.recorded_source, "filename");
    let info = first.info.clone().unwrap();
    assert_eq!((info.width, info.height, info.rotation), (640, 360, -180));
    assert_eq!(info.timecode.as_deref(), Some("01:02:03;04"));
    assert!(info.has_audio);
    let broken = clips.iter().find(|c| c.name() == "broken.mp4").unwrap();
    assert!(broken.info.is_none() && broken.probe_error.is_some());

    // Unchanged files aren't probed again.
    let again = scan(&folders, &library, &cancel, |_| {}).unwrap();
    assert_eq!(again.probed, 0);

    // A proxy: half size, same frames, rate, timecode and rotation.
    let mut last = 0.0;
    let made = proxy::make(&a, &info, &cancel, |f| last = f).unwrap();
    assert_eq!(made, footage.join("Proxy/DJI_20260817075236_0012_D.mov"));
    assert_eq!(last, 1.0);
    let p = library::probe::probe(&made).unwrap();
    assert_eq!((p.width, p.height), (320, 180));
    assert_eq!(p.codec, "prores");
    proxy::verify(&a, &info, &made).unwrap();
    assert!(
        !std::fs::read_dir(footage.join("Proxy")).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("partial"))
    );

    // A scan finds the proxy, checks it, and doesn't list it as footage.
    let report = scan(&folders, &library, &cancel, |_| {}).unwrap();
    assert_eq!(report.found, 3);
    assert_eq!(
        library.get(&a).unwrap().unwrap().proxy,
        ProxyState::Ready(made.clone())
    );

    // A proxy that doesn't match (here, another clip's frames cut short) fails.
    let other = footage.join("day2/Proxy/run.mov");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&made)
        .args(["-frames:v", "10", "-c", "copy"])
        .arg(&other)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    scan(&folders, &library, &cancel, |_| {}).unwrap();
    assert!(matches!(
        library.get(&b).unwrap().unwrap().proxy,
        ProxyState::Failed(_)
    ));

    // Cancelled: no proxy and no partial file left behind.
    let b_info = library.get(&b).unwrap().unwrap().info.unwrap();
    std::fs::remove_file(&other).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(proxy::make(&b, &b_info, &cancel, |_| {}).is_err());
    assert_eq!(
        std::fs::read_dir(other.parent().unwrap()).unwrap().count(),
        0
    );
    cancel.store(false, Ordering::Relaxed);

    // Deleted clips leave the library; a changed one is probed again.
    std::fs::remove_file(footage.join("broken.mp4")).unwrap();
    clip(&b);
    let report = scan(&folders, &library, &cancel, |_| {}).unwrap();
    assert_eq!((report.found, report.removed, report.probed), (2, 1, 1));
}

/// Frames read by number match ffmpeg's own frame-exact selection, from the
/// original (long-GOP H.264) and from its proxy.
#[test]
fn reads_exact_frames_by_number() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("DJI_20260817075236_0012_D.MP4");
    clip(&path);
    let info = library::probe::probe(&path).unwrap();
    let made = proxy::make(&path, &info, &Cancel::default(), |_| {}).unwrap();
    let proxy_info = library::probe::probe(&made).unwrap();
    for (file, info) in [(&path, &info), (&made, &proxy_info)] {
        let (w, h) = library::frames::display_size(info, 90);
        for n in [0i64, 1, 14, 15, 31, 58] {
            let frame = library::frames::read_frame(file, info, n, 90).unwrap();
            assert_eq!((frame.width, frame.height), (w, h));
            let reference = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(file)
                .args([
                    "-vf",
                    &format!("select=eq(n\\,{n}),scale={w}:{h}:flags=bilinear"),
                ])
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
                .output()
                .unwrap()
                .stdout;
            assert_eq!(frame.rgba, reference, "frame {n} of {}", file.display());
        }
        assert!(library::frames::read_frame(file, info, 500, 90).is_err());
    }
}

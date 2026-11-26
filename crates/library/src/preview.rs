//! The catalog's own copy of a clip (decision 0005), kept on this machine for
//! clips whose original is on the NAS: a small preview (about 540 lines
//! tall, H.264 with a key frame each second) that timing, auto-place and
//! thumbnails read instead of the full-size file, and 16 kHz mono audio for
//! suggesting edits later. About 2% of a 4K original.
//!
//! Like a proxy, the preview is the clip frame for frame (same frame count,
//! rate, timecode and rotation flag), so frame numbers mean the same in both.
//! Files are named by the original's name and size, so they stay matched
//! when the original moves on the NAS:
//! `$XDG_DATA_HOME/ghost-run/catalog/<name>.<size>.preview.mov` and `.wav`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::probe::{self, MediaInfo};
use crate::proxy;
use crate::store::Clip;
use crate::{Cancel, cancelled};

/// Preview height (or less, for smaller clips).
pub const HEIGHT: u32 = 540;

pub fn dir() -> PathBuf {
    crate::settings::data_dir().join("catalog")
}

fn base(clip: &Path, size: u64) -> String {
    let name = clip.file_name().unwrap_or_default().to_string_lossy();
    format!("{name}.{size}")
}

/// Where the preview of the clip (its original's path and size) goes.
pub fn preview_path(dir: &Path, clip: &Path, size: u64) -> PathBuf {
    dir.join(format!("{}.preview.mov", base(clip, size)))
}

/// Where its audio goes.
pub fn audio_path(dir: &Path, clip: &Path, size: u64) -> PathBuf {
    dir.join(format!("{}.wav", base(clip, size)))
}

/// The preview's size: [`HEIGHT`] lines of the stored (unrotated) frame, or
/// the clip's own size if that's smaller; even numbers.
pub fn preview_size(info: &MediaInfo) -> (u32, u32) {
    let short = info.height.min(info.width);
    if short <= HEIGHT {
        return (info.width / 2 * 2, info.height / 2 * 2);
    }
    let scale = f64::from(HEIGHT) / f64::from(short);
    let even = |v: u32| ((f64::from(v) * scale).round() as u32 / 2 * 2).max(2);
    (even(info.width), even(info.height))
}

/// Checks `preview` is a preview of the clip: same frames, rate, timecode and
/// rotation, at the preview size.
pub fn verify(clip_path: &Path, clip: &MediaInfo, preview: &Path) -> Result<()> {
    let p = probe::probe(preview)?;
    ensure!(
        (p.width, p.height) == preview_size(clip),
        "preview is {}x{}, expected {}x{}",
        p.width,
        p.height,
        preview_size(clip).0,
        preview_size(clip).1
    );
    ensure!(
        (p.fps_num, p.fps_den) == (clip.fps_num, clip.fps_den),
        "preview runs at {}/{} fps, the clip at {}/{}",
        p.fps_num,
        p.fps_den,
        clip.fps_num,
        clip.fps_den
    );
    ensure!(
        p.timecode == clip.timecode,
        "preview timecode {:?} doesn't match {:?}",
        p.timecode,
        clip.timecode
    );
    ensure!(
        p.rotation == clip.rotation,
        "preview rotation {} doesn't match {}",
        p.rotation,
        clip.rotation
    );
    let (want, got) = (probe::frames(clip_path, clip)?, probe::frames(preview, &p)?);
    ensure!(want == got, "preview has {got} frames, the clip {want}");
    Ok(())
}

/// Makes the preview and audio of `clip` (an original on the NAS), reading
/// its local copy if it has one (the same frames), else the original. Written
/// to partial files first, checked, then moved into place. Returns the
/// preview's path.
pub fn make(
    dir: &Path,
    clip: &Clip,
    threads: usize,
    cancel: &Cancel,
    mut progress: impl FnMut(f64),
) -> Result<PathBuf> {
    let (original, size, source) = (clip.path.as_path(), clip.size, clip.editable());
    let info = clip.info.as_ref().context("the clip couldn't be read")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let out = preview_path(dir, original, size);
    let wav = audio_path(dir, original, size);
    let partial = dir.join(format!(".{}.partial.mov", base(original, size)));
    let partial_wav = dir.join(format!(".{}.partial.wav", base(original, size)));
    let total = probe::frames(source, info).ok().filter(|n| *n > 0);
    let (w, h) = preview_size(info);
    let threads_arg = threads.to_string();
    let gop = info.fps().round().max(1.0).to_string();

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]);
    if threads > 0 {
        cmd.args(["-threads", &threads_arg]);
    }
    cmd.arg("-noautorotate").arg("-i").arg(source);
    // The preview: video only, a key frame a second so a frame is never far.
    cmd.args(["-map", "0:v:0", "-an"])
        .args(["-vf", &format!("scale={w}:{h}")])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "26"])
        .args(["-pix_fmt", "yuv420p", "-g", &gop])
        .args(["-fps_mode", "passthrough"]);
    if threads > 0 {
        cmd.args(["-threads", &threads_arg]);
    }
    if let Some(tc) = &info.timecode {
        cmd.args(["-timecode", tc]);
    }
    cmd.arg(&partial);
    if info.has_audio {
        cmd.args(["-map", "0:a:0", "-vn", "-ac", "1", "-ar", "16000"])
            .args(["-c:a", "pcm_s16le"])
            .arg(&partial_wav);
    }
    cmd.args(["-progress", "pipe:1", "-nostats"]);
    let clean = || {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(&partial_wav);
    };
    if let Err(e) = proxy::run(cmd, total, cancel, &mut progress) {
        clean();
        return Err(e);
    }
    let checked = (|| -> Result<()> {
        if info.rotation != 0 && probe::probe(&partial).is_ok_and(|p| p.rotation != info.rotation) {
            proxy::set_rotation(&partial, info).context("setting the preview's rotation")?;
        }
        verify(source, info, &partial).context("the preview didn't match the clip")
    })();
    if let Err(e) = checked {
        clean();
        return Err(e);
    }
    if cancelled(cancel) {
        clean();
        anyhow::bail!("cancelled");
    }
    if info.has_audio {
        std::fs::rename(&partial_wav, &wav)?;
    }
    std::fs::rename(&partial, &out).with_context(|| format!("writing {}", out.display()))?;
    progress(1.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32) -> MediaInfo {
        MediaInfo {
            codec: "hevc".into(),
            width: w,
            height: h,
            fps_num: 60,
            fps_den: 1,
            frames: None,
            duration_seconds: None,
            timecode: None,
            rotation: 0,
            creation_time: None,
            has_audio: true,
        }
    }

    #[test]
    fn previews_are_540_lines_and_named_by_the_original() {
        assert_eq!(preview_size(&info(3840, 2160)), (960, 540));
        // Portrait stored frames: the short side is 540.
        assert_eq!(preview_size(&info(2160, 3840)), (540, 960));
        assert_eq!(preview_size(&info(2704, 1520)), (960, 540));
        // Small clips stay their size.
        assert_eq!(preview_size(&info(640, 360)), (640, 360));
        let dir = Path::new("/c");
        let clip = Path::new("/nas/2026/2026-11-24/DJI_1.MP4");
        assert_eq!(
            preview_path(dir, clip, 1234),
            PathBuf::from("/c/DJI_1.MP4.1234.preview.mov")
        );
        assert_eq!(
            audio_path(dir, clip, 1234),
            PathBuf::from("/c/DJI_1.MP4.1234.wav")
        );
    }

    #[test]
    fn makes_a_matching_preview_and_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("DJI_20261124093012_0001_D.MP4");
        let ok = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=1280x720:rate=30:duration=2")
            .args(["-f", "lavfi", "-i", "sine=duration=2"])
            .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        let info = probe::probe(&src).unwrap();
        let size = std::fs::metadata(&src).unwrap().len();
        let dir = tmp.path().join("catalog");
        let clip = Clip {
            path: src.clone(),
            size,
            info: Some(info),
            ..Clip::default()
        };
        let out = make(&dir, &clip, 0, &Cancel::default(), |_| {}).unwrap();
        assert_eq!(out, preview_path(&dir, &src, size));
        assert!(audio_path(&dir, &src, size).exists());
        let p = probe::probe(&out).unwrap();
        assert_eq!((p.width, p.height), (960, 540));
    }
}

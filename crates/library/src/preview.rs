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
//!
//! Previews are also kept on the NAS, in `<footage root>/.ghost-run/previews/`
//! under the same names, so each is made once for every machine
//! ([`ensure`]): a machine uses its own copy, else fetches the NAS's, else
//! makes one and puts it there too. Its own copies are a cache of the ones
//! it used, so timing still works away from the NAS.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::footage::Roots;
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

/// Where previews are kept on the NAS, for every machine.
pub fn shared_dir(roots: &Roots) -> Option<PathBuf> {
    roots
        .nas
        .as_ref()
        .map(|nas| nas.join(".ghost-run").join("previews"))
}

/// The clip's preview to read: this machine's, else the NAS's (while it's
/// mounted).
pub fn find(roots: &Roots, clip: &Path, size: u64) -> Option<PathBuf> {
    let local = preview_path(&roots.catalog, clip, size);
    if local.is_file() {
        return Some(local);
    }
    shared_dir(roots)
        .filter(|_| roots.nas_online)
        .map(|dir| preview_path(&dir, clip, size))
        .filter(|p| p.is_file())
}

/// Whether the clip has a preview on this machine.
pub fn here(roots: &Roots, clip: &Path, size: u64) -> bool {
    preview_path(&roots.catalog, clip, size).is_file()
}

/// How [`ensure`] got a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Got {
    /// This machine had it.
    Here,
    /// Copied from the NAS.
    Fetched,
    /// Made now (and put on the NAS).
    Made,
}

/// The clip's preview on this machine: the one it has, else a copy of the
/// NAS's, else one made now (from the local copy, else the original), which
/// is then put on the NAS for other machines. Returns its path.
pub fn ensure(
    roots: &Roots,
    clip: &Clip,
    threads: usize,
    cancel: &Cancel,
    progress: impl FnMut(f64),
) -> Result<(PathBuf, Got)> {
    let local = preview_path(&roots.catalog, &clip.path, clip.size);
    if local.is_file() {
        return Ok((local, Got::Here));
    }
    if let Some(dir) = shared_dir(roots).filter(|_| roots.nas_online) {
        let shared = preview_path(&dir, &clip.path, clip.size);
        if shared.is_file() {
            std::fs::create_dir_all(&roots.catalog)?;
            let wav = audio_path(&dir, &clip.path, clip.size);
            if wav.is_file() {
                copy_into_place(&wav, &audio_path(&roots.catalog, &clip.path, clip.size))?;
            }
            copy_into_place(&shared, &local)?;
            return Ok((local, Got::Fetched));
        }
    }
    let made = make(&roots.catalog, clip, threads, cancel, progress)?;
    // Best effort: a machine that can't write to the share still has its own.
    let _ = share(roots, clip);
    Ok((made, Got::Made))
}

/// Puts this machine's preview of the clip on the NAS if it isn't there
/// yet; whether it copied one.
pub fn share(roots: &Roots, clip: &Clip) -> Result<bool> {
    let Some(dir) = shared_dir(roots).filter(|_| roots.nas_online) else {
        return Ok(false);
    };
    let local = preview_path(&roots.catalog, &clip.path, clip.size);
    let shared = preview_path(&dir, &clip.path, clip.size);
    if !local.is_file() || shared.is_file() {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let wav = audio_path(&roots.catalog, &clip.path, clip.size);
    if wav.is_file() {
        copy_into_place(&wav, &audio_path(&dir, &clip.path, clip.size))?;
    }
    copy_into_place(&local, &shared)?;
    Ok(true)
}

/// Copies `from` to a hidden partial file beside `to`, then renames it into
/// place, so a half-copied file is never taken for a preview.
fn copy_into_place(from: &Path, to: &Path) -> Result<()> {
    let dir = to.parent().context("no folder")?;
    let partial = dir.join(format!(
        ".{}.partial",
        to.file_name().unwrap_or_default().to_string_lossy()
    ));
    let copied = std::fs::copy(from, &partial)
        .with_context(|| format!("copying {} to {}", from.display(), dir.display()))
        .and_then(|_| {
            std::fs::rename(&partial, to).with_context(|| format!("writing {}", to.display()))
        });
    if copied.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    copied
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

    #[test]
    fn previews_are_made_once_and_shared_through_the_nas() {
        let tmp = tempfile::tempdir().unwrap();
        let nas = tmp.path().join("nas/footage");
        let src = nas.join("2026/2026-11-24/DJI_1.MP4");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        let ok = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=640x360:rate=30:duration=1")
            .args(["-f", "lavfi", "-i", "sine=duration=1"])
            .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        let clip = Clip {
            path: src.clone(),
            size: std::fs::metadata(&src).unwrap().len(),
            info: Some(probe::probe(&src).unwrap()),
            ..Clip::default()
        };
        let machine = |name: &str| Roots {
            nas: Some(nas.clone()),
            nas_online: true,
            local: Some(tmp.path().join(name).join("edit")),
            catalog: tmp.path().join(name).join("catalog"),
        };
        let (desktop, laptop) = (machine("desktop"), machine("laptop"));
        let cancel = Cancel::default();
        assert_eq!(find(&laptop, &src, clip.size), None);

        let (made, got) = ensure(&desktop, &clip, 0, &cancel, |_| {}).unwrap();
        assert_eq!(got, Got::Made);
        let shared = preview_path(&nas.join(".ghost-run/previews"), &src, clip.size);
        assert!(shared.is_file() && made.is_file());
        assert!(audio_path(&nas.join(".ghost-run/previews"), &src, clip.size).is_file());
        // The laptop reads the NAS's until it fetches its own.
        assert_eq!(find(&laptop, &src, clip.size), Some(shared.clone()));
        assert!(!here(&laptop, &src, clip.size));
        let (fetched, got) = ensure(&laptop, &clip, 0, &cancel, |_| {}).unwrap();
        assert_eq!(got, Got::Fetched);
        assert_eq!(fetched, preview_path(&laptop.catalog, &src, clip.size));
        assert!(audio_path(&laptop.catalog, &src, clip.size).is_file());
        assert_eq!(
            ensure(&laptop, &clip, 0, &cancel, |_| {}).unwrap().1,
            Got::Here
        );
        // Away from the NAS, its own copy is still found.
        let away = Roots {
            nas_online: false,
            ..laptop.clone()
        };
        assert_eq!(find(&away, &src, clip.size), Some(fetched));

        // A preview made before sharing existed goes up when shared.
        std::fs::remove_file(&shared).unwrap();
        assert!(share(&desktop, &clip).unwrap());
        assert!(shared.is_file());
        assert!(!share(&desktop, &clip).unwrap());
    }
}

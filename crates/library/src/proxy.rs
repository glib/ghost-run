//! Proxies: a half-resolution ProRes 422 Proxy copy of each clip, in a `Proxy`
//! folder beside it (`clips/Proxy/<name>.mov`), where Resolve looks for proxy
//! media. Every frame is a key frame, so the frame viewer can jump anywhere at
//! once, and auto-markers can read it instead of decoding 4K HEVC.
//!
//! A proxy must be the clip frame for frame: the same frame count, frame rate
//! and timecode, and the same rotation flag (frames are not turned upright;
//! like the clip, the player does that). [`verify`] checks this, and a proxy
//! that fails is made again.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::probe::{self, MediaInfo};
use crate::{Cancel, cancelled};

pub const FOLDER: &str = "Proxy";

/// `<clip folder>/Proxy/<clip name without its last extension>.mov`.
pub fn proxy_path(clip: &Path) -> PathBuf {
    let stem = clip
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    clip.parent()
        .unwrap_or(Path::new("."))
        .join(FOLDER)
        .join(format!("{stem}.mov"))
}

/// Half the clip's size, rounded down to even numbers.
pub fn proxy_size(info: &MediaInfo) -> (u32, u32) {
    (info.width / 4 * 2, info.height / 4 * 2)
}

/// Checks `proxy` is a proxy of the clip described by `clip` (at `clip_path`).
pub fn verify(clip_path: &Path, clip: &MediaInfo, proxy: &Path) -> Result<()> {
    let p = probe::probe(proxy)?;
    ensure!(
        (p.width, p.height) == proxy_size(clip),
        "proxy is {}x{}, expected {}x{}",
        p.width,
        p.height,
        proxy_size(clip).0,
        proxy_size(clip).1
    );
    ensure!(
        (p.fps_num, p.fps_den) == (clip.fps_num, clip.fps_den),
        "proxy runs at {}/{} fps, the clip at {}/{}",
        p.fps_num,
        p.fps_den,
        clip.fps_num,
        clip.fps_den
    );
    ensure!(
        p.timecode == clip.timecode,
        "proxy timecode {:?} doesn't match the clip's {:?}",
        p.timecode,
        clip.timecode
    );
    ensure!(
        p.rotation == clip.rotation,
        "proxy rotation {} doesn't match the clip's {}",
        p.rotation,
        clip.rotation
    );
    let (want, got) = (probe::frames(clip_path, clip)?, probe::frames(proxy, &p)?);
    ensure!(want == got, "proxy has {got} frames, the clip {want}");
    Ok(())
}

/// Makes and checks the clip's proxy; `progress` gets 0.0–1.0 as it goes.
/// It's written to a hidden partial file first, so a cancelled or failed
/// encode never leaves a proxy Resolve would pick up.
pub fn make(
    clip_path: &Path,
    clip: &MediaInfo,
    cancel: &Cancel,
    mut progress: impl FnMut(f64),
) -> Result<PathBuf> {
    let out = proxy_path(clip_path);
    let dir = out.parent().expect("proxy path has a folder");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let partial = dir.join(format!(
        ".{}.partial.mov",
        out.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let total = probe::frames(clip_path, clip).ok().filter(|n| *n > 0);
    let (w, h) = proxy_size(clip);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        // Keep the frames as stored, with the rotation flag carried over.
        .arg("-noautorotate")
        .arg("-i")
        .arg(clip_path)
        .args(["-map", "0:v:0", "-map", "0:a?"])
        .args(["-vf", &format!("scale={w}:{h}")])
        .args(["-c:v", "prores_aw", "-profile:v", "0", "-vendor", "apl0"])
        .args(["-c:a", "pcm_s16le"])
        // One output frame per input frame, so frame numbers match the clip's.
        .args(["-fps_mode", "passthrough"])
        .args(["-progress", "pipe:1", "-nostats"]);
    if let Some(tc) = &clip.timecode {
        cmd.args(["-timecode", tc]);
    }
    // If the app dies mid-encode, ffmpeg goes with it rather than running on.
    // SAFETY: prctl is async-signal-safe and touches no memory of ours.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = cmd
        .arg(&partial)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run ffmpeg (is FFmpeg installed?)")?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    let mut stopped = false;
    for line in BufReader::new(child.stdout.take().expect("piped stdout")).lines() {
        let Ok(line) = line else { break };
        if cancelled(cancel) {
            let _ = child.kill();
            stopped = true;
            break;
        }
        if let (Some(frame), Some(total)) = (line.strip_prefix("frame="), total)
            && let Ok(frame) = frame.trim().parse::<f64>()
        {
            progress((frame / total as f64).min(1.0));
        }
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    let fail = |message: String| {
        let _ = std::fs::remove_file(&partial);
        Err(anyhow::anyhow!(message))
    };
    if stopped || cancelled(cancel) {
        return fail("cancelled".into());
    }
    if !status.success() {
        return fail(format!("ffmpeg failed: {}", errors.trim()));
    }
    if let Err(e) = verify(clip_path, clip, &partial) {
        return fail(format!("the new proxy didn't match the clip: {e:#}"));
    }
    std::fs::rename(&partial, &out).with_context(|| format!("writing {}", out.display()))?;
    progress(1.0);
    Ok(out)
}

/// Whether `path` is inside a proxy folder or is one of our partial files.
pub fn is_proxy_or_partial(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.ends_with(".partial.mov")
        || path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|d| d == FOLDER)
}

/// Removes a stale partial left by a crash, if there is one.
pub fn clean_partial(clip_path: &Path) {
    let out = proxy_path(clip_path);
    if let Some(dir) = out.parent() {
        let partial = dir.join(format!(
            ".{}.partial.mov",
            out.file_stem().unwrap_or_default().to_string_lossy()
        ));
        let _ = std::fs::remove_file(partial);
    }
}

/// Fails unless ffmpeg has the encoder proxies need.
pub fn check_ffmpeg() -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("could not run ffmpeg (is FFmpeg installed?)")?;
    if !String::from_utf8_lossy(&out.stdout).contains("prores_aw") {
        bail!("this FFmpeg has no ProRes encoder (prores_aw)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies_go_in_a_proxy_folder_beside_the_clip() {
        assert_eq!(
            proxy_path(Path::new("/v/DJI_0012_D.MP4")),
            PathBuf::from("/v/Proxy/DJI_0012_D.mov")
        );
        // An editing copy keeps the original's name in its own.
        assert_eq!(
            proxy_path(Path::new("/v/DJI_0012_D.MP4.mov")),
            PathBuf::from("/v/Proxy/DJI_0012_D.MP4.mov")
        );
        assert!(is_proxy_or_partial(Path::new("/v/Proxy/x.mov")));
        assert!(is_proxy_or_partial(Path::new("/v/.x.partial.mov")));
        assert!(!is_proxy_or_partial(Path::new("/v/x.mov")));
    }
}

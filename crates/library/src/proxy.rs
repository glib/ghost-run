//! Proxies: a half-resolution ProRes 422 Proxy copy of each clip, in a `Proxy`
//! folder beside it (`clips/Proxy/<name>.mov`), where Resolve looks for proxy
//! media. Every frame is a key frame, so the frame viewer can jump anywhere at
//! once, and auto-markers can read it instead of decoding 4K HEVC.
//!
//! A proxy must be the clip frame for frame: the same frame count, frame rate
//! and timecode, and the same rotation flag (frames are not turned upright;
//! like the clip, the player does that). [`verify`] checks this, and a proxy
//! that fails is made again.
//!
//! One ffmpeg can't keep a big machine busy (its scaler is the slow step,
//! and a proxy is a light encode), so several clips are made at once:
//! [`plan`] picks how many, and how many threads each gets.

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

/// How many proxies to make at once, and the threads each ffmpeg gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub jobs: usize,
    pub threads: usize,
}

/// Threads one proxy job keeps busy well (decode, scale and ProRes).
const THREADS_PER_JOB: usize = 4;
/// At most this many at once: past it, the disk is the limit.
const MAX_JOBS: usize = 8;
/// Measured for a 4K 10-bit HEVC clip: about 420 MB a job plus 46 MB a
/// decoder thread (602 MB at 4 threads, 1154 MB at 16).
const JOB_MB_4K: u64 = 420;
const THREAD_MB_4K: u64 = 46;

/// The plan for `clips` clips up to `width × height` on this machine.
pub fn plan(width: u32, height: u32, clips: usize) -> Plan {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    plan_for(width, height, clips, cores, available_memory())
}

/// Spreads `cores` over jobs of about [`THREADS_PER_JOB`] threads, as many
/// as there are clips, within half the `available` memory.
pub fn plan_for(
    width: u32,
    height: u32,
    clips: usize,
    cores: usize,
    available: Option<u64>,
) -> Plan {
    let cores = cores.max(1);
    let area = (f64::from(width) * f64::from(height) / (3840.0 * 2160.0)).max(0.05);
    let mb = |threads: usize| -> u64 {
        ((JOB_MB_4K + THREAD_MB_4K * threads as u64) as f64 * area) as u64 * 1024 * 1024
    };
    let budget = available.map_or(u64::MAX, |a| a / 2);
    let mut jobs = (cores / THREADS_PER_JOB)
        .clamp(1, MAX_JOBS)
        .min(clips.max(1));
    while jobs > 1 && jobs as u64 * mb(cores / jobs) > budget {
        jobs -= 1;
    }
    Plan {
        jobs,
        threads: (cores / jobs).max(1),
    }
}

/// `MemAvailable` from `/proc/meminfo`, in bytes.
fn available_memory() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Makes and checks the clip's proxy; `progress` gets 0.0–1.0 as it goes.
/// It's written to a hidden partial file first, so a cancelled or failed
/// encode never leaves a proxy Resolve would pick up.
pub fn make(
    clip_path: &Path,
    clip: &MediaInfo,
    cancel: &Cancel,
    progress: impl FnMut(f64),
) -> Result<PathBuf> {
    make_with(clip_path, clip, 0, cancel, progress)
}

/// [`make`] with `threads` for each of ffmpeg's decoder, scaler and encoder
/// (`0`: ffmpeg's own choice, and a one-thread scaler).
pub fn make_with(
    clip_path: &Path,
    clip: &MediaInfo,
    threads: usize,
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

    let threads_arg = threads.to_string();
    let scale = if threads > 0 {
        format!("scale={w}:{h}:threads={threads}")
    } else {
        format!("scale={w}:{h}")
    };
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]);
    if threads > 0 {
        cmd.args(["-threads", &threads_arg]);
    }
    // Keep the frames as stored, with the rotation flag carried over.
    cmd.arg("-noautorotate")
        .arg("-i")
        .arg(clip_path)
        .args(["-map", "0:v:0", "-map", "0:a?"])
        .args(["-vf", &scale]);
    if threads > 0 {
        cmd.args(["-threads", &threads_arg]);
    }
    cmd.args(["-c:v", "prores_aw", "-profile:v", "0", "-vendor", "apl0"])
        .args(["-c:a", "pcm_s16le"])
        // One output frame per input frame, so frame numbers match the clip's.
        .args(["-fps_mode", "passthrough"])
        .args(["-progress", "pipe:1", "-nostats"]);
    if let Some(tc) = &clip.timecode {
        cmd.args(["-timecode", tc]);
    }
    cmd.arg(&partial);
    if let Err(e) = run(cmd, total, cancel, &mut progress) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let fail = |message: String| {
        let _ = std::fs::remove_file(&partial);
        Err(anyhow::anyhow!(message))
    };
    // FFmpeg 7 carries the rotation flag through a re-encode with
    // -noautorotate; FFmpeg 8 drops it. Put it back with a lossless remux.
    if clip.rotation != 0
        && probe::probe(&partial).is_ok_and(|p| p.rotation != clip.rotation)
        && let Err(e) = set_rotation(&partial, clip)
    {
        return fail(format!("could not set the proxy's rotation: {e:#}"));
    }
    if let Err(e) = verify(clip_path, clip, &partial) {
        return fail(format!("the new proxy didn't match the clip: {e:#}"));
    }
    std::fs::rename(&partial, &out).with_context(|| format!("writing {}", out.display()))?;
    progress(1.0);
    Ok(out)
}

/// Runs an ffmpeg command that writes `-progress pipe:1`, reporting the share
/// of `total` frames done; kills it when `cancel` is set. Errors are
/// "cancelled" or ffmpeg's own message.
pub(crate) fn run(
    mut cmd: Command,
    total: Option<i64>,
    cancel: &Cancel,
    progress: &mut dyn FnMut(f64),
) -> Result<()> {
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
    if stopped || cancelled(cancel) {
        bail!("cancelled");
    }
    if !status.success() {
        bail!("ffmpeg failed: {}", errors.trim());
    }
    Ok(())
}

/// Rewrites `partial` with the clip's rotation flag (stream copy, so quick).
pub(crate) fn set_rotation(partial: &Path, clip: &MediaInfo) -> Result<()> {
    let tmp = partial.with_extension("rotating.mov");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        // ffprobe's rotation and this option use the same angle convention.
        .args(["-display_rotation", &clip.rotation.to_string()])
        .arg("-i")
        .arg(partial)
        .args(["-map", "0:v", "-map", "0:a?", "-c", "copy"]);
    if let Some(tc) = &clip.timecode {
        cmd.args(["-timecode", tc]);
    }
    let out = cmd.arg(&tmp).output().context("could not run ffmpeg")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    std::fs::rename(&tmp, partial)?;
    Ok(())
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
    fn plans_jobs_by_cores_clips_and_memory() {
        const GB: u64 = 1 << 30;
        let plan = |cores, clips, gb: Option<u64>| {
            let p = plan_for(3840, 2160, clips, cores, gb.map(|g| g * GB));
            (p.jobs, p.threads)
        };
        // A 4-core box: one job with every core.
        assert_eq!(plan(4, 10, Some(6)), (1, 4));
        // 32 threads, plenty of memory: eight jobs of four.
        assert_eq!(plan(32, 20, Some(32)), (8, 4));
        // Only three clips: three jobs, sharing the cores.
        assert_eq!(plan(32, 3, Some(32)), (3, 10));
        // Little memory: fewer jobs (a 4 GB budget fits six of about 650 MB).
        assert_eq!(plan(32, 20, Some(8)), (6, 5));
        assert_eq!(plan(32, 20, Some(1)), (1, 32));
        // Unknown memory: by cores.
        assert_eq!(plan(16, 20, None), (4, 4));
        // Smaller clips take less memory.
        let hd = plan_for(1920, 1080, 20, 32, Some(8 * GB));
        assert_eq!((hd.jobs, hd.threads), (8, 4));
    }

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

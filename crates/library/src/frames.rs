//! Single frames for the frame viewer, by frame number, decoded with ffmpeg
//! and turned upright as the clip's rotation flag says. Fast from a proxy
//! (every frame is a key frame); from a long-GOP original it decodes from the
//! previous key frame, so it's slower but still exact.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::probe::MediaInfo;

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub index: i64,
    pub width: u32,
    pub height: u32,
    /// RGBA, `width * 4` bytes a row.
    pub rgba: Vec<u8>,
}

/// The size a frame is shown at: upright, at most `max_height` tall, even.
pub fn display_size(info: &MediaInfo, max_height: u32) -> (u32, u32) {
    let (w, h) = if info.rotation.rem_euclid(180) == 90 {
        (info.height, info.width)
    } else {
        (info.width, info.height)
    };
    let out_h = h.min(max_height.max(2)) / 2 * 2;
    let out_w =
        ((f64::from(w) * f64::from(out_h) / f64::from(h.max(1))).round() as u32 / 2 * 2).max(2);
    (out_w, out_h)
}

/// Frame `index` (0 = the first) of the video at `path`, described by `info`.
pub fn read_frame(path: &Path, info: &MediaInfo, index: i64, max_height: u32) -> Result<Frame> {
    ensure!(index >= 0, "no frame {index}");
    let (w, h) = display_size(info, max_height);
    // Seek a quarter frame before the frame's start: ffmpeg then decodes from
    // the key frame before and returns the first frame at or after that point.
    let seconds = ((index as f64 - 0.25) / info.fps()).max(0.0);
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error"])
        .args(["-ss", &format!("{seconds:.6}")])
        .arg("-i")
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-an"])
        .args(["-vf", &format!("scale={w}:{h}:flags=bilinear")])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .output()
        .context("could not run ffmpeg (is FFmpeg installed?)")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let want = (w * h * 4) as usize;
    ensure!(
        output.stdout.len() >= want,
        "frame {index} is past the end of the clip"
    );
    let mut rgba = output.stdout;
    rgba.truncate(want);
    Ok(Frame {
        index,
        width: w,
        height: h,
        rgba,
    })
}

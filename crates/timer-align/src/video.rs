//! Reading a clip as small greyscale frames with ffmpeg. Decoding a long 4K
//! clip takes a while, so the result is cached by the file's path, size and
//! modification time (a reference clip is read once, however many runs use it).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};
use timer_render::encode::{EncodeError, probe};

use crate::Frames;

pub const WIDTH: usize = 64;
pub const HEIGHT: usize = 36;
/// Bump when the frames read change, so old cache entries aren't used.
const CACHE_VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"RTFRAMES";

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error(transparent)]
    Probe(#[from] EncodeError),
    #[error("could not run ffmpeg: {0} (is ffmpeg installed and on PATH?)")]
    Spawn(std::io::Error),
    #[error("ffmpeg could not read {path}: {message}")]
    Decode { path: String, message: String },
}

/// Every frame of `path`, 64 × 36 greyscale, numbered as in the clip.
/// `cache`: a directory to keep and reuse decoded clips in.
pub fn read_frames(path: &Path, cache: Option<&Path>) -> Result<Frames, VideoError> {
    let info = probe(path)?;
    let fps = info.rate.fps();
    let cache_file = cache.and_then(|dir| cache_path(dir, path));
    if let Some(file) = &cache_file
        && let Some(frames) = load(file, fps)
    {
        return Ok(frames);
    }
    let frames = decode(path, fps)?;
    if let Some(file) = &cache_file {
        // A cache that can't be written only costs time next run.
        let _ = save(file, &frames);
    }
    Ok(frames)
}

fn decode(path: &Path, fps: f64) -> Result<Frames, VideoError> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        // A GPU decoder if there is one (much faster for 4K); else software.
        .args(["-hwaccel", "auto", "-i"])
        .arg(path)
        .args(["-an", "-sn", "-dn", "-map", "0:v:0"])
        .args([
            "-vf",
            &format!("scale={WIDTH}:{HEIGHT}:flags=area,format=gray"),
        ])
        // One output frame per decoded frame, so frame numbers match the clip's.
        .args(["-fps_mode", "passthrough", "-f", "rawvideo", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(VideoError::Spawn)?;
    // Read stderr alongside, so a chatty ffmpeg can't fill the pipe and stall.
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut data = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_end(&mut data);
    let status = child.wait().map_err(VideoError::Spawn)?;
    let errors = errors.join().unwrap_or_default();
    if read.is_err() || !status.success() {
        return Err(VideoError::Decode {
            path: path.display().to_string(),
            message: errors.trim().to_owned(),
        });
    }
    data.truncate(data.len() / (WIDTH * HEIGHT) * (WIDTH * HEIGHT));
    Ok(Frames {
        width: WIDTH,
        height: HEIGHT,
        fps,
        data,
    })
}

/// `<dir>/<hash of path, size, mtime>.frames`, or `None` if the file can't be read.
fn cache_path(dir: &Path, path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let mut hash = Sha256::new();
    hash.update(absolute.to_string_lossy().as_bytes());
    hash.update(meta.len().to_le_bytes());
    hash.update(modified.to_le_bytes());
    hash.update(CACHE_VERSION.to_le_bytes());
    let digest: String = hash
        .finalize()
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(dir.join(format!("{digest}.frames")))
}

fn save(file: &Path, frames: &Frames) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(frames.data.len() + 24);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(frames.width as u32).to_le_bytes());
    bytes.extend_from_slice(&(frames.height as u32).to_le_bytes());
    bytes.extend_from_slice(&frames.fps.to_le_bytes());
    bytes.extend_from_slice(&frames.data);
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, file)
}

fn load(file: &Path, fps: f64) -> Option<Frames> {
    let bytes = std::fs::read(file).ok()?;
    if bytes.len() < 24 || &bytes[..8] != MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let height = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let stored_fps = f64::from_le_bytes(bytes[16..24].try_into().ok()?);
    if width != WIDTH || height != HEIGHT || stored_fps != fps {
        return None;
    }
    Some(Frames {
        width,
        height,
        fps,
        data: bytes[24..].to_vec(),
    })
}

/// The default cache directory: `rtimer` in the user's cache folder.
pub fn default_cache_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
    };
    base.map(|b| b.join("rtimer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_and_rejects_other_rates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/x.frames");
        let frames = Frames {
            width: WIDTH,
            height: HEIGHT,
            fps: 59.94,
            data: (0..WIDTH * HEIGHT * 3).map(|i| i as u8).collect(),
        };
        save(&file, &frames).unwrap();
        let back = load(&file, 59.94).unwrap();
        assert_eq!(back.data, frames.data);
        assert_eq!(back.len(), 3);
        assert!(load(&file, 29.97).is_none());
    }

    #[test]
    fn cache_key_follows_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, b"one").unwrap();
        let a = cache_path(dir.path(), &clip).unwrap();
        std::fs::write(&clip, b"longer").unwrap();
        let b = cache_path(dir.path(), &clip).unwrap();
        assert_ne!(a, b);
        assert!(cache_path(dir.path(), &dir.path().join("missing.mp4")).is_none());
    }
}

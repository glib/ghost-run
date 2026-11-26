//! Small thumbnails of clips for the Import page: one frame a second in (or
//! the first, for a shorter clip), upright, 90 pixels high, as PNG, cached in
//! `$XDG_CACHE_HOME/ghost-run/thumbs` by path, size and modification time so a
//! card put back in doesn't decode its clips again.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub const HEIGHT: u32 = 90;

/// `$XDG_CACHE_HOME/ghost-run/thumbs` (default `~/.cache/…`).
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ghost-run")
        .join("thumbs")
}

/// The clip's thumbnail, made (with ffmpeg) if it isn't cached yet.
pub fn thumbnail(clip: &Path, cache: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(clip).with_context(|| format!("reading {}", clip.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    clip.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().hash(&mut hasher);
    let out = cache.join(format!("{:016x}.png", hasher.finish()));
    if out.exists() {
        return Ok(out);
    }
    std::fs::create_dir_all(cache)?;
    let tmp = cache.join(format!(".{:016x}.partial.png", hasher.finish()));
    // A second in, else the first frame: -ss past the end gives nothing.
    for seek in ["1", "0"] {
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-ss", seek, "-i"])
            .arg(clip)
            .args([
                "-frames:v",
                "1",
                "-vf",
                &format!("scale=-2:{HEIGHT}"),
                "-f",
                "image2",
                "-c:v",
                "png",
            ])
            .arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running ffmpeg")?;
        if status.success() && tmp.metadata().is_ok_and(|m| m.len() > 0) {
            std::fs::rename(&tmp, &out)?;
            return Ok(out);
        }
    }
    let _ = std::fs::remove_file(&tmp);
    bail!("no frame could be read from {}", clip.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_are_made_once_and_cached() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            assert!(
                std::env::var_os("REQUIRE_FFMPEG").is_none(),
                "ffmpeg is required"
            );
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let clip = d.path().join("clip.mp4");
        let ok = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=10",
                "-t",
                "0.5",
            ])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let cache = d.path().join("cache");
        let first = thumbnail(&clip, &cache).unwrap();
        let bytes = std::fs::read(&first).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        // Cached: the same file, not made again.
        let modified = first.metadata().unwrap().modified().unwrap();
        let again = thumbnail(&clip, &cache).unwrap();
        assert_eq!(again, first);
        assert_eq!(again.metadata().unwrap().modified().unwrap(), modified);
    }
}

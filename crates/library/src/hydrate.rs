//! Local copies of NAS clips (decision 0005): **hydrate** pulls an original
//! down as an editing copy (the video stream as it is, the audio as 24-bit
//! PCM, which Resolve on Linux needs, in a MOV) at its fixed local path
//! ([`Roots::local_copy`]); its proxy is then made beside it. **Dehydrate**
//! removes the copy and its proxy again, only while the original is on the
//! NAS, so nothing removed can't be made again. Rendered overlays are kept:
//! they take a while to render and don't depend on the copy.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::footage::Roots;
use crate::store::Clip;
use crate::{Cancel, probe, proxy};

/// Makes `clip`'s local copy (an original on the NAS) if it has none;
/// returns its path. `progress` gets 0.0–1.0.
pub fn hydrate(
    clip: &Clip,
    roots: &Roots,
    cancel: &Cancel,
    mut progress: impl FnMut(f64),
) -> Result<PathBuf> {
    let target = roots
        .local_copy(&clip.path)
        .context("only clips on the NAS have local copies (and the local folder must be set)")?;
    if target.is_file() {
        return Ok(target);
    }
    ensure!(
        roots.nas_online,
        "the NAS isn't mounted, so {} can't be copied from it",
        clip.name()
    );
    let info = clip.info.as_ref().context("the clip couldn't be read")?;
    let dir = target.parent().context("no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let partial = dir.join(format!(
        ".{}.partial.mov",
        target.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .arg("-noautorotate")
        .arg("-i")
        .arg(&clip.path)
        .args(["-map", "0:v:0", "-map", "0:a?"])
        .args(["-map_metadata", "0", "-c:v", "copy", "-c:a", "pcm_s24le"])
        .args(["-progress", "pipe:1", "-nostats", "-f", "mov"])
        .arg(&partial);
    let total = probe::frames(&clip.path, info).ok().filter(|n| *n > 0);
    if let Err(e) = proxy::run(cmd, total, cancel, &mut progress) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let checked = probe::probe(&partial).and_then(|copy| {
        let (want, got) = (
            probe::frames(&clip.path, info)?,
            probe::frames(&partial, &copy)?,
        );
        ensure!(
            want == got,
            "the copy has {got} frames, the original {want}"
        );
        ensure!(
            copy.rotation == info.rotation && copy.timecode == info.timecode,
            "the copy's rotation or timecode doesn't match the original's"
        );
        Ok(())
    });
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&partial);
        bail!("the local copy didn't match the original: {e:#}");
    }
    std::fs::rename(&partial, &target).with_context(|| format!("writing {}", target.display()))?;
    progress(1.0);
    Ok(target)
}

/// Removes `clip`'s local copy and its proxy, once the original is checked to
/// be on the NAS now (mounted, there, the size catalogued). Returns the bytes
/// freed.
pub fn dehydrate(clip: &Clip, roots: &Roots) -> Result<u64> {
    let Some(local) = roots.local_copy(&clip.path).filter(|p| p.is_file()) else {
        return Ok(0);
    };
    ensure!(
        roots.nas_online,
        "the NAS isn't mounted: local copies are only removed while the original can be checked"
    );
    let on_nas = std::fs::metadata(&clip.path)
        .with_context(|| format!("the original isn't on the NAS: {}", clip.path.display()))?;
    ensure!(
        on_nas.len() == clip.size,
        "the original on the NAS isn't the size catalogued ({} bytes, not {}): rescan first",
        on_nas.len(),
        clip.size
    );
    let mut freed = 0;
    for file in [proxy::proxy_path(&local), local] {
        if let Ok(meta) = std::fs::metadata(&file) {
            std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
            freed += meta.len();
        }
    }
    Ok(freed)
}

/// What [`settle`] did with a newly imported clip's local copy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Settled {
    /// Kept, with its proxy made, to edit now.
    Kept,
    /// Removed, freeing these bytes.
    Removed(u64),
}

/// After an import (decision 0005): makes the catalog's preview of a clip
/// just verified on the NAS from its local copy, while that's still here,
/// then either keeps the copy (making its proxy) or removes it.
/// `progress` gets the step ("preview", "proxy") and 0.0–1.0.
pub fn settle(
    clip: &Clip,
    roots: &Roots,
    keep: bool,
    cancel: &Cancel,
    mut progress: impl FnMut(&str, f64),
) -> Result<Settled> {
    let info = clip.info.as_ref().context("the clip couldn't be read")?;
    let local = clip
        .local
        .as_deref()
        .context("the clip has no local copy")?;
    let preview = crate::preview::preview_path(&roots.catalog, &clip.path, clip.size);
    if !preview.is_file() {
        crate::preview::make(&roots.catalog, clip, 0, cancel, |f| progress("preview", f))
            .context("making its preview")?;
    }
    if keep {
        if !proxy::proxy_path(local).is_file() {
            proxy::make(local, info, cancel, |f| progress("proxy", f))
                .context("making its proxy")?;
        }
        return Ok(Settled::Kept);
    }
    Ok(Settled::Removed(dehydrate(clip, roots)?))
}

/// The bytes the local copies of NAS clips take, proxies included.
pub fn local_bytes<'a>(clips: impl IntoIterator<Item = &'a Clip>, roots: &Roots) -> u64 {
    clips
        .into_iter()
        .filter(|c| roots.on_nas(&c.path))
        .filter_map(|c| c.local.as_deref())
        .flat_map(|local| [local.to_path_buf(), proxy::proxy_path(local)])
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_down_and_removes_only_while_the_original_is_on_the_nas() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots {
            nas: Some(tmp.path().join("nas")),
            nas_online: true,
            local: Some(tmp.path().join("edit")),
            catalog: tmp.path().join("catalog"),
        };
        let original = tmp
            .path()
            .join("nas/2026/2026-11-24/DJI_20261124093012_0001_D.MP4");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        let ok = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x180:rate=30:duration=1")
            .args(["-f", "lavfi", "-i", "sine=duration=1"])
            .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&original)
            .status()
            .unwrap();
        assert!(ok.success());
        let mut clip = Clip {
            path: original.clone(),
            size: std::fs::metadata(&original).unwrap().len(),
            info: Some(probe::probe(&original).unwrap()),
            ..Clip::default()
        };
        let local = hydrate(&clip, &roots, &Cancel::default(), |_| {}).unwrap();
        assert_eq!(
            local,
            tmp.path()
                .join("edit/2026/2026-11-24/DJI_20261124093012_0001_D.MP4.mov")
        );
        let copy = probe::probe(&local).unwrap();
        assert!(copy.has_audio);
        clip.local = Some(local.clone());
        assert!(local_bytes([&clip], &roots) > 0);

        // Offline, or a changed original: nothing is removed.
        let offline = Roots {
            nas_online: false,
            ..roots.clone()
        };
        assert!(dehydrate(&clip, &offline).is_err());
        let wrong = Clip {
            size: clip.size + 1,
            ..clip.clone()
        };
        assert!(dehydrate(&wrong, &roots).is_err());
        assert!(local.exists());

        let freed = dehydrate(&clip, &roots).unwrap();
        assert!(freed > 0);
        assert!(!local.exists());
        assert!(original.exists());

        // After an import: the preview is made from the copy, then the copy
        // is kept with its proxy, or removed.
        let local = hydrate(&clip, &roots, &Cancel::default(), |_| {}).unwrap();
        let kept = settle(&clip, &roots, true, &Cancel::default(), |_, _| {}).unwrap();
        assert_eq!(kept, Settled::Kept);
        assert!(proxy::proxy_path(&local).is_file());
        let preview = crate::preview::preview_path(&roots.catalog, &clip.path, clip.size);
        assert!(preview.is_file());
        let removed = settle(&clip, &roots, false, &Cancel::default(), |_, _| {}).unwrap();
        assert!(matches!(removed, Settled::Removed(n) if n > 0));
        assert!(!local.exists() && !proxy::proxy_path(&local).exists());
        assert!(preview.is_file() && original.exists());
    }
}

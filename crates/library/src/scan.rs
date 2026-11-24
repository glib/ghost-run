//! Finding clips: walk the footage folders, probe what's new or changed, note
//! proxies that already exist, and forget clips that are gone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use walkdir::WalkDir;

use crate::probe::{self, MediaInfo};
use crate::proxy;
use crate::store::{Clip, Library, ProxyState};
use crate::{Cancel, cancelled};

/// Video files by extension (any case).
const VIDEO: &[&str] = &["mp4", "mov", "m4v", "mxf", "mkv", "avi", "mts", "m2ts"];

/// Folders that hold copies, caches or backups rather than footage. Resolve
/// writes the last four; `Proxy` is where proxies go, `Overlays` overlays.
const SKIP_DIRS: &[&str] = &[
    proxy::FOLDER,
    crate::overlays::FOLDER,
    "CacheClip",
    "ProxyMedia",
    ".gallery",
    "Resolve Project Backups",
];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScanReport {
    pub found: usize,
    /// Probed because they were new or changed.
    pub probed: usize,
    /// ffprobe couldn't read them.
    pub unreadable: usize,
    /// Gone from the folders, so removed from the library.
    pub removed: usize,
}

/// What a scan is doing, for a progress line.
pub enum Progress<'a> {
    Looking(usize),
    Probing {
        done: usize,
        total: usize,
        path: &'a Path,
    },
}

/// The video files under `folders`. An original with an editing copy beside it
/// (`clip.MP4` and `clip.MP4.mov`, as Import's "convert alongside" makes) is
/// left out: the copy is the one to edit, time and make a proxy of.
pub fn find(folders: &[PathBuf], cancel: &Cancel) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for folder in folders {
        let walk = WalkDir::new(folder).follow_links(false).into_iter();
        for entry in walk.filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0
                || !(name.starts_with('.')
                    || (e.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref())))
        }) {
            if cancelled(cancel) {
                return found;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !entry.file_type().is_file()
                || path.to_str().is_none()
                || proxy::is_proxy_or_partial(path)
                || crate::overlays::is_overlay(path)
            {
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if VIDEO.contains(&ext.as_str()) {
                found.push(path.to_owned());
            }
        }
    }
    let all: HashSet<PathBuf> = found.iter().cloned().collect();
    found.retain(|p| {
        let mut copy = p.as_os_str().to_owned();
        copy.push(".mov");
        !all.contains(Path::new(&copy))
    });
    found.sort();
    found.dedup();
    found
}

pub fn scan(
    folders: &[PathBuf],
    library: &Library,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<ScanReport> {
    progress(Progress::Looking(0));
    let paths = find(folders, cancel);
    let mut report = ScanReport {
        found: paths.len(),
        ..Default::default()
    };
    for (i, path) in paths.iter().enumerate() {
        if cancelled(cancel) {
            return Ok(report);
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        let known = library.get(path)?;
        let mut clip = match known {
            Some(c) if c.size == meta.len() && c.modified_ns == modified_ns => c,
            _ => {
                progress(Progress::Probing {
                    done: i,
                    total: paths.len(),
                    path,
                });
                report.probed += 1;
                let probed = probe::probe(path);
                let (recorded, source) = recorded(path, probed.as_ref().ok(), modified_ns);
                Clip {
                    path: path.clone(),
                    size: meta.len(),
                    modified_ns,
                    recorded,
                    recorded_source: source.into(),
                    probe_error: probed.as_ref().err().map(|e| format!("{e:#}")),
                    info: probed.ok(),
                    proxy: ProxyState::Missing,
                }
            }
        };
        if clip.info.is_none() {
            report.unreadable += 1;
        }
        clip.proxy = proxy_state(&clip);
        library.upsert(&clip)?;
    }
    if !cancelled(cancel) {
        let keep: HashSet<PathBuf> = paths.into_iter().collect();
        report.removed = library.retain(&keep)?;
    }
    Ok(report)
}

/// The proxy's state after a scan: a recorded proxy that has gone missing is
/// missing; one that has appeared (made by Resolve, or before the library
/// existed) is checked before it counts.
fn proxy_state(clip: &Clip) -> ProxyState {
    let path = proxy::proxy_path(&clip.path);
    match (&clip.proxy, &clip.info) {
        (ProxyState::Ready(p), _) if p.exists() => clip.proxy.clone(),
        (_, Some(info)) if path.exists() => match proxy::verify(&clip.path, info, &path) {
            Ok(()) => ProxyState::Ready(path),
            Err(e) => ProxyState::Failed(format!("existing proxy doesn't match: {e:#}")),
        },
        (ProxyState::Failed(e), _) => ProxyState::Failed(e.clone()),
        _ => ProxyState::Missing,
    }
}

/// When it was recorded, by the camera's clock: from the file name (DJI's
/// `YYYYMMDDHHMMSS`, or `YYYYMMDD_HHMMSS`), else the container's creation
/// time, else the file's modification time.
fn recorded(
    path: &Path,
    info: Option<&MediaInfo>,
    modified_ns: i64,
) -> (Option<String>, &'static str) {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stamp =
        regex::Regex::new(r"(?:^|[^0-9])(20\d{6})_?(\d{6})(?:[^0-9]|$)").expect("valid regex");
    if let Some(c) = stamp.captures(&name)
        && let Ok(t) =
            chrono::NaiveDateTime::parse_from_str(&format!("{}{}", &c[1], &c[2]), "%Y%m%d%H%M%S")
    {
        return (Some(t.format("%Y-%m-%d %H:%M:%S").to_string()), "filename");
    }
    if let Some(t) = info
        .and_then(|i| i.creation_time.as_deref())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    {
        // Cameras write their local clock and call it UTC; show it as written.
        return (
            Some(t.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()),
            "metadata",
        );
    }
    let modified = chrono::DateTime::from_timestamp_nanos(modified_ns)
        .with_timezone(&chrono::Local)
        .naive_local();
    (
        Some(modified.format("%Y-%m-%d %H:%M:%S").to_string()),
        "modified",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_footage_but_not_proxies_caches_or_originals_with_copies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in [
            "a/DJI_1.MP4",
            "a/DJI_2.MP4",
            "a/DJI_2.MP4.mov",
            "a/Proxy/DJI_1.mov",
            "a/.DJI_3.partial.mov",
            "a/CacheClip/x.mov",
            "a/Overlays/DJI_1.overlay.mov",
            "a/elsewhere/DJI_9.overlay.mov",
            "a/.hidden/y.mp4",
            "a/notes.txt",
            "b/GX010150.mp4",
        ] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let found = find(&[root.join("a"), root.join("b")], &Cancel::default());
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["a/DJI_1.MP4", "a/DJI_2.MP4.mov", "b/GX010150.mp4"]);
    }

    #[test]
    fn recording_time_prefers_the_file_name() {
        let (t, from) = recorded(Path::new("/v/DJI_20260817075236_0012_D.MP4"), None, 0);
        assert_eq!(
            (t.as_deref(), from),
            (Some("2026-08-17 07:52:36"), "filename")
        );
        let (t, from) = recorded(Path::new("/v/VID_20260601_101500.mp4"), None, 0);
        assert_eq!(
            (t.as_deref(), from),
            (Some("2026-06-01 10:15:00"), "filename")
        );
        let info = MediaInfo {
            codec: "h264".into(),
            width: 2,
            height: 2,
            fps_num: 30,
            fps_den: 1,
            frames: None,
            duration_seconds: None,
            timecode: None,
            rotation: 0,
            creation_time: Some("2026-05-31T09:08:07.000000Z".into()),
            has_audio: false,
        };
        let (t, from) = recorded(Path::new("/v/GX010150.MP4"), Some(&info), 0);
        assert_eq!(
            (t.as_deref(), from),
            (Some("2026-05-31 09:08:07"), "metadata")
        );
        assert_eq!(recorded(Path::new("/v/x.mp4"), None, 0).1, "modified");
    }
}

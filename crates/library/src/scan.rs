//! Finding clips: walk the footage folders, probe what's new or changed, note
//! proxies that already exist, and forget clips that are gone.
//!
//! An original on the NAS ([`Roots`]) is listed once: its local editing copy,
//! if there is one, is noted on it rather than listed beside it, along with
//! the catalog's preview. While the NAS isn't mounted, its clips stay as last
//! catalogued.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use walkdir::WalkDir;

use crate::footage::Roots;
use crate::probe::{self, MediaInfo};
use crate::store::{Clip, Library, ProxyState};
use crate::{Cancel, cancelled};
use crate::{preview, proxy};

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

/// Scans `folders` (which should include the NAS footage root while it's
/// mounted, and the local root).
pub fn scan(
    folders: &[PathBuf],
    roots: &Roots,
    library: &Library,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<ScanReport> {
    progress(Progress::Looking(0));
    library.rekey()?;
    let mut paths = find(folders, cancel);
    // While the NAS is offline, its clips stay as last catalogued. So do
    // clips stored under a footage root this machine hasn't set (a shared
    // catalog's, #69): it can't look for them.
    let offline: Vec<Clip> = library
        .clips()?
        .into_iter()
        .filter(|c| (!roots.nas_online && roots.on_nas(&c.path)) || c.path.is_relative())
        .collect();
    // A local copy is listed as its original, when that's in the Library.
    let originals: HashSet<PathBuf> = paths
        .iter()
        .chain(offline.iter().map(|c| &c.path))
        .filter(|p| roots.on_nas(p))
        .cloned()
        .collect();
    paths.retain(|p| roots.original_of(p).is_none_or(|o| !originals.contains(&o)));
    for mut clip in offline.iter().filter(|c| roots.on_nas(&c.path)).cloned() {
        place(&mut clip, roots);
        library.upsert(&clip)?;
    }
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
            Some(c) if c.size == meta.len() && same_time(&c, modified_ns, roots) => c,
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
                    ..Clip::default()
                }
            }
        };
        if clip.info.is_none() {
            report.unreadable += 1;
        }
        place(&mut clip, roots);
        library.upsert(&clip)?;
    }
    if !cancelled(cancel) {
        let keep: HashSet<PathBuf> = paths
            .into_iter()
            .chain(offline.into_iter().map(|c| c.path))
            .collect();
        report.removed = library.retain(&keep)?;
    }
    Ok(report)
}

/// Whether a catalogued clip's modification time is still `modified_ns`. On
/// the NAS, within [`NAS_TIME_SLACK_NS`]: machines sharing a catalog see the
/// same file through their own mounts, which needn't agree to the
/// nanosecond, and each would otherwise reprobe what the other wrote.
fn same_time(clip: &Clip, modified_ns: i64, roots: &Roots) -> bool {
    clip.modified_ns == modified_ns
        || (roots.on_nas(&clip.path) && clip.modified_ns.abs_diff(modified_ns) < NAS_TIME_SLACK_NS)
}

/// See [`same_time`].
pub const NAS_TIME_SLACK_NS: u64 = 2_000_000_000;

/// Notes a clip's local copy, preview and proxy as they are on disk now.
pub fn place(clip: &mut Clip, roots: &Roots) {
    if roots.on_nas(&clip.path) {
        clip.local = roots.local_copy(&clip.path).filter(|p| p.is_file());
        clip.preview = preview::find(roots, &clip.path, clip.size);
    } else {
        clip.local = None;
        clip.preview = None;
    }
    clip.proxy = proxy_state(clip, roots);
}

/// The proxy's state after a scan: a recorded proxy that has gone missing is
/// missing; one that has appeared (made by Resolve, or before the library
/// existed) is checked before it counts. An original on the NAS has a proxy
/// only through its local copy.
fn proxy_state(clip: &Clip, roots: &Roots) -> ProxyState {
    let source = match (&clip.local, roots.on_nas(&clip.path)) {
        (Some(local), _) => local.clone(),
        (None, true) => return ProxyState::Missing,
        (None, false) => clip.path.clone(),
    };
    let path = proxy::proxy_path(&source);
    match (&clip.proxy, &clip.info) {
        (ProxyState::Ready(p), _) if p.exists() && *p == path => clip.proxy.clone(),
        (_, Some(info)) if path.exists() => match proxy::verify(&source, info, &path) {
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
    fn machines_sharing_a_catalog_keep_each_others_clips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in ["nas/2026/DJI_1.MP4", "desk/DJI_2.MP4", "laptop/DJI_3.MP4"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        // The laptop mounts the same share somewhere else.
        std::os::unix::fs::symlink(root.join("nas"), root.join("mnt")).unwrap();
        let roots = |nas: &str, local: &str| Roots {
            nas: Some(root.join(nas)),
            nas_online: true,
            local: Some(root.join(local)),
            catalog: root.join("catalog"),
        };
        let shared = root.join("library.sqlite3");
        let open = |machine: &str, nas: &str| {
            Library::open_with(
                &shared,
                &root.join(format!("{machine}.sqlite3")),
                Some(root.join(nas)),
                machine.into(),
            )
            .unwrap()
        };
        let desk = open("desk", "nas");
        let laptop = open("laptop", "mnt");
        let none = Cancel::default();
        let first = scan(
            &[root.join("nas"), root.join("desk")],
            &roots("nas", "desk"),
            &desk,
            &none,
            |_| {},
        )
        .unwrap();
        assert_eq!(first.probed, 2);
        let second = scan(
            &[root.join("mnt"), root.join("laptop")],
            &roots("mnt", "laptop"),
            &laptop,
            &none,
            |_| {},
        )
        .unwrap();
        // The NAS clip was known already; nothing of the desk's went.
        assert_eq!((second.probed, second.removed), (1, 0));
        let names = |lib: &Library| -> Vec<String> {
            let mut n: Vec<String> = lib
                .clips()
                .unwrap()
                .iter()
                .map(|c| c.path.strip_prefix(root).unwrap().display().to_string())
                .collect();
            n.sort();
            n
        };
        assert_eq!(names(&desk), ["desk/DJI_2.MP4", "nas/2026/DJI_1.MP4"]);
        assert_eq!(names(&laptop), ["laptop/DJI_3.MP4", "mnt/2026/DJI_1.MP4"]);

        // A machine without the footage root set can't see the NAS clip, and
        // doesn't forget it either.
        let bare =
            Library::open_with(&shared, &root.join("bare.sqlite3"), None, "bare".into()).unwrap();
        let report = scan(&[], &Roots::default(), &bare, &none, |_| {}).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(names(&desk).len(), 2);
    }

    #[test]
    fn nas_originals_are_listed_once_with_their_local_copy_and_kept_while_offline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let roots = Roots {
            nas: Some(root.join("nas")),
            nas_online: true,
            local: Some(root.join("edit")),
            catalog: root.join("catalog"),
        };
        for f in [
            "nas/2026/2026-11-24/DJI_1.MP4",
            "nas/2026/2026-11-24/DJI_2.MP4",
            "edit/2026/2026-11-24/DJI_1.MP4.mov",
            "edit/other/GX0101.MP4",
            "catalog/DJI_2.MP4.1.preview.mov",
        ] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let library = Library::open(&root.join("library.sqlite3")).unwrap();
        let all = [root.join("nas"), root.join("edit")];
        scan(&all, &roots, &library, &Cancel::default(), |_| {}).unwrap();
        let clips = library.clips().unwrap();
        let mut listed: Vec<(String, bool, bool)> = clips
            .iter()
            .map(|c| {
                (
                    c.path.strip_prefix(root).unwrap().display().to_string(),
                    c.local.is_some(),
                    c.preview.is_some(),
                )
            })
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("edit/other/GX0101.MP4".into(), false, false),
                ("nas/2026/2026-11-24/DJI_1.MP4".into(), true, false),
                ("nas/2026/2026-11-24/DJI_2.MP4".into(), false, true),
            ]
        );
        let one = clips.iter().find(|c| c.name() == "DJI_1.MP4").unwrap();
        assert_eq!(
            one.editable(),
            root.join("edit/2026/2026-11-24/DJI_1.MP4.mov")
        );

        // The share goes offline (scanned as an empty folder, or not at all):
        // its clips stay, and the local copy is still folded into its original.
        let offline = Roots {
            nas_online: false,
            ..roots.clone()
        };
        scan(
            &[root.join("edit")],
            &offline,
            &library,
            &Cancel::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(library.clips().unwrap().len(), 3);

        // The local copy is removed: the original stays, without one.
        std::fs::remove_file(root.join("edit/2026/2026-11-24/DJI_1.MP4.mov")).unwrap();
        scan(&all, &roots, &library, &Cancel::default(), |_| {}).unwrap();
        let one = library
            .get(&root.join("nas/2026/2026-11-24/DJI_1.MP4"))
            .unwrap()
            .unwrap();
        assert_eq!(one.local, None);
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

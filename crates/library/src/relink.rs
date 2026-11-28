//! Bringing back the footage of a Resolve project whose files have gone
//! (decision 0005): projects made before the footage moved to the NAS point
//! at files that are no longer there. Each file the project uses is matched
//! to a catalogued clip ([`find`]); a NAS clip is then copied here
//! ([`crate::hydrate`]) and Resolve pointed at the copy, a clip still on this
//! machine at the clip itself ([`plan`]).

use std::path::{Path, PathBuf};

use crate::footage::Roots;
use crate::store::Clip;

/// A file a Resolve project uses, as the bridge's `list_media` reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolFile {
    pub path: PathBuf,
    /// Its length in frames, as Resolve remembers it (kept while offline).
    pub frames: Option<i64>,
    /// Its start timecode, as Resolve remembers it.
    pub timecode: Option<String>,
    /// Whether Resolve's side can open the file now.
    pub online: bool,
}

/// A file's name as the camera wrote it, in lower case: an editing copy's
/// (`clip.MP4.mov`) as its original's, and the escapes undone that imports
/// put in names (`~hh` for bytes SMB can't store; batches before the
/// day-folder layout also escaped capitals and added `f-`).
pub fn camera_name(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let name = unescape(name.strip_prefix("f-").unwrap_or(&name)).to_lowercase();
    match name.strip_suffix(".mov") {
        Some(original) if original.contains('.') && !original.ends_with(".overlay") => {
            original.to_owned()
        }
        _ => name,
    }
}

/// `~hh` byte escapes undone; anything that isn't one is kept as written.
fn unescape(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'~', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| name.to_owned())
}

/// Whether `a` and `b` are the same file under two spellings.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// The catalogued clip a project's file is: by its path, its local copy's
/// path (whether or not the copy is still here), the same file under another
/// spelling, or else its camera name, narrowed by length and timecode when
/// Resolve remembers them. `Err` says why none, in a few words.
pub fn find<'a>(clips: &'a [Clip], roots: &Roots, file: &PoolFile) -> Result<&'a Clip, String> {
    let original = roots.original_of(&file.path);
    if let Some(c) = clips.iter().find(|c| {
        c.path == file.path
            || c.local.as_deref() == Some(file.path.as_path())
            || original.as_deref() == Some(c.path.as_path())
            || roots.local_copy(&c.path).as_deref() == Some(file.path.as_path())
    }) {
        return Ok(c);
    }
    if let Some(c) = clips.iter().find(|c| {
        [Some(c.path.as_path()), c.local.as_deref()]
            .into_iter()
            .flatten()
            .any(|p| same_file(p, &file.path))
    }) {
        return Ok(c);
    }
    let name = camera_name(&file.path);
    let named: Vec<&Clip> = clips
        .iter()
        .filter(|c| camera_name(&c.path) == name)
        .collect();
    if named.is_empty() {
        return Err("not in the Library".into());
    }
    let fits: Vec<&Clip> = named
        .iter()
        .copied()
        .filter(
            |c| match (file.frames, c.info.as_ref().and_then(|i| i.frames)) {
                (Some(want), Some(got)) => (want - got).abs() <= 1,
                _ => true,
            },
        )
        .filter(|c| {
            match (
                file.timecode.as_deref(),
                c.info.as_ref().and_then(|i| i.timecode.as_deref()),
            ) {
                (Some(want), Some(got)) if !want.is_empty() => same_timecode(want, got),
                _ => true,
            }
        })
        .collect();
    // A file only on this machine and its NAS original can both be listed;
    // the NAS one is the one to keep using.
    let on_nas: Vec<&Clip> = fits
        .iter()
        .copied()
        .filter(|c| roots.on_nas(&c.path))
        .collect();
    match (fits.as_slice(), on_nas.as_slice()) {
        ([one], _) | (_, [one]) => Ok(one),
        ([], _) => Err(if named.len() == 1 {
            "a clip of that name is in the Library, but not the same length".into()
        } else {
            format!(
                "{} clips of that name are in the Library, none the same length",
                named.len()
            )
        }),
        (many, _) => Err(format!(
            "{} clips of that name and length are in the Library",
            many.len()
        )),
    }
}

/// Timecodes compared ignoring the drop-frame mark (`;` or `:` before the
/// frames).
fn same_timecode(a: &str, b: &str) -> bool {
    a.replace(';', ":") == b.replace(';', ":")
}

/// What to do for one of a project's files that can't be opened.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The file the project names now.
    pub old: PathBuf,
    pub clip: Clip,
    /// Whether the clip has to be copied down from the NAS first.
    pub copy: bool,
}

impl Step {
    /// The file to point Resolve at: a NAS clip's local copy, else the clip.
    pub fn target(&self, roots: &Roots) -> PathBuf {
        roots
            .local_copy(&self.clip.path)
            .unwrap_or_else(|| self.clip.path.clone())
    }
}

/// What bringing a project's footage back would do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Files Resolve can open already.
    pub online: usize,
    /// Each file not found, and why.
    pub missing: Vec<(String, String)>,
}

impl Plan {
    /// The clips to copy down from the NAS.
    pub fn to_copy(&self) -> impl Iterator<Item = &Clip> {
        self.steps.iter().filter(|s| s.copy).map(|s| &s.clip)
    }

    /// About how much room the copies and their proxies take.
    pub fn bytes(&self) -> u64 {
        self.to_copy().map(copy_bytes).sum()
    }
}

/// About the room a clip's local copy and proxy take: the original's size
/// (the video is copied as it is) plus a ProRes 422 Proxy at half size.
pub fn copy_bytes(clip: &Clip) -> u64 {
    let proxy = clip.info.as_ref().map_or(0.0, |i| {
        let pixels = f64::from(i.width / 2) * f64::from(i.height / 2);
        // ProRes 422 Proxy runs at about 0.7 bits a pixel.
        pixels * i.fps() * 0.7 / 8.0 * i.duration_seconds.unwrap_or(0.0)
    });
    clip.size + proxy as u64
}

/// For each file a project uses that can't be opened now: the clip it is
/// and whether it must be copied here first. Ghost Run's own overlays are
/// left out, and a file used twice is counted once.
pub fn plan(clips: &[Clip], roots: &Roots, files: &[PoolFile]) -> Plan {
    let mut out = Plan::default();
    let mut seen = std::collections::HashSet::new();
    for file in files {
        if !seen.insert(file.path.clone()) || camera_name(&file.path).ends_with(".overlay.mov") {
            continue;
        }
        if file.online {
            out.online += 1;
            continue;
        }
        let name = file
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match find(clips, roots, file) {
            Ok(clip) if roots.on_nas(&clip.path) => {
                let here = roots.local_copy(&clip.path).is_some_and(|p| p.is_file());
                if !here && !roots.nas_online {
                    out.missing
                        .push((name, "on the NAS, which isn't mounted".into()));
                } else if !here && clip.info.is_none() {
                    out.missing
                        .push((name, "on the NAS, but it couldn't be read".into()));
                } else {
                    out.steps.push(Step {
                        old: file.path.clone(),
                        clip: clip.clone(),
                        copy: !here,
                    });
                }
            }
            Ok(clip) if clip.path.is_file() => out.steps.push(Step {
                old: file.path.clone(),
                clip: clip.clone(),
                copy: false,
            }),
            Ok(_) => out
                .missing
                .push((name, "in the Library, but its file is gone".into())),
            Err(why) => out.missing.push((name, why)),
        }
    }
    out
}

/// Free bytes on the filesystem holding `path` (or its nearest existing
/// parent).
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let dir = path.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: a NUL-terminated path and a statvfs buffer to fill.
    let ok = unsafe { libc::statvfs(c.as_ptr(), stat.as_mut_ptr()) } == 0;
    // SAFETY: statvfs filled it in when it returned 0.
    ok.then(|| unsafe { stat.assume_init() })
        .map(|s| s.f_bavail * s.f_frsize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::MediaInfo;

    fn clip(path: &str, frames: i64, timecode: &str) -> Clip {
        Clip {
            path: path.into(),
            size: 1_000,
            info: Some(MediaInfo {
                width: 3840,
                height: 2160,
                fps_num: 30,
                fps_den: 1,
                frames: Some(frames),
                duration_seconds: Some(frames as f64 / 30.0),
                timecode: Some(timecode.into()),
                ..MediaInfo::default()
            }),
            ..Clip::default()
        }
    }

    fn file(path: &str, frames: Option<i64>, timecode: Option<&str>) -> PoolFile {
        PoolFile {
            path: path.into(),
            frames,
            timecode: timecode.map(Into::into),
            online: false,
        }
    }

    #[test]
    fn camera_names_undo_the_escapes() {
        let name = |p: &str| camera_name(Path::new(p));
        assert_eq!(name("/a/GX010123.MP4"), "gx010123.mp4");
        assert_eq!(name("/a/GX010123.MP4.mov"), "gx010123.mp4");
        // Old batches: f- and every capital escaped.
        assert_eq!(
            name("/e/2026-08-26/abc/clips/f-~47~58010123.~4d~50~34.mov"),
            "gx010123.mp4"
        );
        assert_eq!(name("/a/what~3f.MP4"), "what?.mp4");
        assert_eq!(name("/a/x~zz.mp4"), "x~zz.mp4");
        assert_eq!(name("/a/run.overlay.mov"), "run.overlay.mov");
        assert_eq!(name("/a/plain.mov"), "plain.mov");
    }

    #[test]
    fn files_are_found_by_path_then_name_length_and_timecode() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots {
            nas: Some("/nas/footage".into()),
            nas_online: true,
            local: Some(tmp.path().join("edit")),
            catalog: tmp.path().join("catalog"),
        };
        let edit = |rel: &str| tmp.path().join("edit").join(rel);
        let clips = [
            clip(
                "/nas/footage/2026/2026-06-16/DJI_0001.MP4",
                900,
                "10:00:00:00",
            ),
            // A camera that numbers its files again each year.
            clip(
                "/nas/footage/2025/2025-06-01/GX010123.MP4",
                600,
                "09:00:00:00",
            ),
            clip(
                "/nas/footage/2026/2026-06-16/GX010123.MP4",
                1200,
                "11:00:00:00",
            ),
            clip(
                "/nas/footage/2026/2026-06-17/GX010200.MP4",
                300,
                "12:00:00:00",
            ),
            clip(
                "/nas/footage/2026/2026-06-18/GX010200.MP4",
                300,
                "12:00:00:00",
            ),
        ];
        let found = |f: PoolFile| find(&clips, &roots, &f).map(|c| c.path.clone());
        let nas = |rel: &str| Ok(PathBuf::from("/nas/footage").join(rel));
        // Its local copy's fixed path, though the copy is gone.
        assert_eq!(
            found(file(
                edit("2026/2026-06-16/DJI_0001.MP4.mov").to_str().unwrap(),
                None,
                None
            )),
            nas("2026/2026-06-16/DJI_0001.MP4")
        );
        // An old escaped editing copy, told apart by its length.
        assert_eq!(
            found(file(
                "/old/2026-06-16/u/clips/f-~47~58010123.~4d~50~34.mov",
                Some(1200),
                None
            )),
            nas("2026/2026-06-16/GX010123.MP4")
        );
        // A raw file, told apart by its timecode (drop-frame spelling).
        assert_eq!(
            found(file("/old/GX010123.MP4", None, Some("09;00;00;00"))),
            nas("2025/2025-06-01/GX010123.MP4")
        );
        // Off by a frame still counts.
        assert_eq!(
            found(file("/old/DJI_0001.MP4", Some(901), None)),
            nas("2026/2026-06-16/DJI_0001.MP4")
        );
        assert_eq!(
            found(file("/old/GX010123.MP4", None, None)).unwrap_err(),
            "2 clips of that name and length are in the Library"
        );
        assert_eq!(
            found(file("/old/GX010200.MP4", Some(300), None)).unwrap_err(),
            "2 clips of that name and length are in the Library"
        );
        assert_eq!(
            found(file("/old/DJI_0001.MP4", Some(450), None)).unwrap_err(),
            "a clip of that name is in the Library, but not the same length"
        );
        assert_eq!(
            found(file("/old/song.wav", None, None)).unwrap_err(),
            "not in the Library"
        );
    }

    #[test]
    fn a_plan_copies_what_isnt_here_and_skips_what_resolve_can_open() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots {
            nas: Some("/nas/footage".into()),
            nas_online: true,
            local: Some(tmp.path().join("edit")),
            catalog: tmp.path().join("catalog"),
        };
        let here = tmp.path().join("edit/2026/2026-06-16/B.MP4.mov");
        std::fs::create_dir_all(here.parent().unwrap()).unwrap();
        std::fs::write(&here, b"copy").unwrap();
        let only_local = tmp.path().join("footage/C.MP4");
        std::fs::create_dir_all(only_local.parent().unwrap()).unwrap();
        std::fs::write(&only_local, b"clip").unwrap();
        let clips = [
            clip("/nas/footage/2026/2026-06-16/A.MP4", 300, "00:00:00:00"),
            clip("/nas/footage/2026/2026-06-16/B.MP4", 300, "00:00:00:00"),
            clip(only_local.to_str().unwrap(), 300, "00:00:00:00"),
        ];
        let online = PoolFile {
            online: true,
            ..file("/x/D.MP4", None, None)
        };
        let plan = plan(
            &clips,
            &roots,
            &[
                file("/old/A.MP4", None, None),
                file("/old/A.MP4", None, None),
                file("/old/B.MP4", None, None),
                file("/old/C.MP4", None, None),
                file("/old/Overlays/A.overlay.mov", None, None),
                file("/old/E.MP4", None, None),
                online,
            ],
        );
        let steps: Vec<(&str, bool)> = plan
            .steps
            .iter()
            .map(|s| (s.clip.path.file_name().unwrap().to_str().unwrap(), s.copy))
            .collect();
        assert_eq!(steps, [("A.MP4", true), ("B.MP4", false), ("C.MP4", false)]);
        assert_eq!(plan.steps[1].target(&roots), here);
        assert_eq!(plan.steps[2].target(&roots), only_local);
        assert_eq!(plan.online, 1);
        assert_eq!(
            plan.missing,
            [("E.MP4".to_owned(), "not in the Library".to_owned())]
        );
        // A's 1 kB plus a half-size ProRes Proxy for 10 s at 30 fps.
        assert_eq!(
            plan.bytes(),
            1_000 + (1920.0 * 1080.0 * 30.0 * 0.7 / 8.0 * 10.0) as u64
        );

        let offline = Roots {
            nas_online: false,
            ..roots.clone()
        };
        let plan = super::plan(&clips, &offline, &[file("/old/A.MP4", None, None)]);
        assert!(plan.steps.is_empty());
        assert_eq!(plan.missing[0].1, "on the NAS, which isn't mounted");
        assert!(free_bytes(tmp.path()).unwrap() > 0);
    }
}

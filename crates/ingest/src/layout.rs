//! Where an import puts each file: `<YYYY>/<YYYY-MM-DD>/<name as the camera
//! wrote it>` under the archive (and the same path plus `.mov` under the
//! editing destination), the day by the camera's clock. A name already taken
//! that day, ignoring case (SMB shares fold case), goes in a numbered folder
//! (`2/`, `3/`…); a sidecar goes wherever its video goes. Nothing is renamed
//! or overwritten. Batches made before this layout used
//! `<UTC date>/<batch id>/originals/<escaped path>` and are still read.
//! See `docs/decisions/0005-workspaces-and-hydrated-footage.md`.

use crate::{catalog, model::Candidate};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Local, NaiveDate};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

/// The highest numbered folder tried before giving up on a name.
const MAX_FOLDER: u32 = 99;

/// The day a file was recorded, by the camera's clock: from its name or its
/// embedded date (cameras write their own clock there); a file with neither
/// goes by its modification time on this machine's clock.
pub fn day(c: &Candidate) -> NaiveDate {
    let recorded = catalog::recorded(c);
    if recorded.origin.starts_with("File modification") {
        DateTime::from_timestamp(c.identity.mtime, 0)
            .map(|t| t.with_timezone(&Local).date_naive())
            .unwrap_or(recorded.date.date())
    } else {
        recorded.date.date()
    }
}

/// The file name as the camera wrote it, with only what SMB can't store
/// escaped as `~hh` (and `~` itself, so it reads back): `\ : * ? " < > |`,
/// control characters, and a trailing space or dot.
pub fn smb_name(name: &str) -> Result<String> {
    ensure!(!name.is_empty(), "Empty file name");
    let mut out = String::new();
    let last = name.chars().count() - 1;
    for (i, ch) in name.chars().enumerate() {
        let unsafe_char = matches!(
            ch,
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '~'
        ) || ch.is_control()
            || (i == last && matches!(ch, ' ' | '.'));
        if unsafe_char {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("~{b:02x}"));
            }
        } else {
            out.push(ch);
        }
    }
    ensure!(
        out.len() <= 220,
        "File name too long for the archive: {name}. Rename a copy of it before importing."
    );
    Ok(out)
}

/// Archive paths for the selected files, in their order. `taken` says whether
/// a path (relative to the archive) is already used there or, as an editing
/// copy, under the editing destination.
pub fn archive_paths(
    all: &[Candidate],
    selected: &[&Candidate],
    mut taken: impl FnMut(&Path) -> Result<bool>,
) -> Result<Vec<PathBuf>> {
    let days: HashMap<&Path, NaiveDate> =
        all.iter().map(|c| (c.relative.as_path(), day(c))).collect();
    // A video and its sidecars move together, under the video's day.
    let mut groups: Vec<(&Path, Vec<usize>)> = Vec::new();
    for (i, c) in selected.iter().enumerate() {
        let key = c.sidecar_for.as_deref().unwrap_or(&c.relative);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    let mut used: HashSet<String> = HashSet::new();
    let mut out = vec![PathBuf::new(); selected.len()];
    for (key, members) in groups {
        let day = days
            .get(key)
            .copied()
            .unwrap_or_else(|| day(selected[members[0]]));
        let folder = PathBuf::from(day.format("%Y").to_string()).join(day.to_string());
        let names = members
            .iter()
            .map(|&i| {
                let name = selected[i]
                    .relative
                    .file_name()
                    .context("Missing file name")?
                    .to_str()
                    .context("File names must be UTF-8")?;
                smb_name(name)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut placed = false;
        for n in 1..=MAX_FOLDER {
            let dir = if n == 1 {
                folder.clone()
            } else {
                folder.join(n.to_string())
            };
            let paths: Vec<PathBuf> = names.iter().map(|name| dir.join(name)).collect();
            let mut free = true;
            for p in &paths {
                if used.contains(&fold(p)) || taken(p)? {
                    free = false;
                    break;
                }
            }
            if free {
                for (p, &i) in paths.into_iter().zip(&members) {
                    used.insert(fold(&p));
                    out[i] = p;
                }
                placed = true;
                break;
            }
        }
        if !placed {
            bail!(
                "{} is already in the archive {MAX_FOLDER} times for {day}. Check the archive folder.",
                names[0]
            );
        }
    }
    Ok(out)
}

/// The editing copy for an archived original: the same path with `.mov`
/// added; for a batch made before this layout, its `clips/` twin.
pub fn editing_path(archive_path: &Path) -> Result<PathBuf> {
    let name = archive_path
        .file_name()
        .context("Missing file name")?
        .to_string_lossy()
        .into_owned();
    let parts: Vec<Component> = archive_path.components().collect();
    if parts.len() >= 4 && parts[2].as_os_str() == "originals" {
        let base: PathBuf = parts[..2].iter().collect();
        let relative: PathBuf = parts[3..].iter().collect();
        return Ok(base
            .join("clips")
            .join(relative)
            .with_file_name(format!("{name}.mov")));
    }
    Ok(archive_path.with_file_name(format!("{name}.mov")))
}

fn fold(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Identity;

    fn candidate(name: &str, sidecar_for: Option<&str>) -> Candidate {
        Candidate {
            relative: name.into(),
            identity: Identity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime: 0,
                mtime_ns: 0,
                ctime: 0,
                ctime_ns: 0,
            },
            probe: None,
            probe_error: None,
            selected: true,
            editing: true,
            sidecar_for: sidecar_for.map(PathBuf::from),
        }
    }

    fn paths(all: &[Candidate], taken: &[&str]) -> Vec<String> {
        let selected: Vec<&Candidate> = all.iter().collect();
        let taken: HashSet<String> = taken.iter().map(|t| t.to_lowercase()).collect();
        archive_paths(all, &selected, |p| Ok(taken.contains(&fold(p))))
            .unwrap()
            .into_iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn files_go_in_their_recording_day_with_their_own_names() {
        let all = [
            candidate("DCIM/100MEDIA/DJI_20260826184226_0027_D.MP4", None),
            candidate("DCIM/100MEDIA/DJI_20260827070000_0001_D.MP4", None),
        ];
        assert_eq!(
            paths(&all, &[]),
            [
                "2026/2026-08-26/DJI_20260826184226_0027_D.MP4",
                "2026/2026-08-27/DJI_20260827070000_0001_D.MP4"
            ]
        );
    }

    #[test]
    fn a_taken_name_goes_in_a_numbered_folder_ignoring_case() {
        let all = [
            candidate("A/DJI_20260826184226_0027_D.MP4", None),
            candidate("B/dji_20260826184226_0027_d.mp4", None),
        ];
        assert_eq!(
            paths(&all, &["2026/2026-08-26/2/DJI_20260826184226_0027_D.MP4"]),
            [
                "2026/2026-08-26/DJI_20260826184226_0027_D.MP4",
                "2026/2026-08-26/3/dji_20260826184226_0027_d.mp4"
            ]
        );
    }

    #[test]
    fn sidecars_follow_their_video() {
        let video = "DCIM/DJI_20260826184226_0027_D.MP4";
        let all = [
            candidate(video, None),
            // No date in its own name: it still goes with the video.
            candidate("DCIM/DJI_0027.SRT", Some(video)),
        ];
        assert_eq!(
            paths(&all, &["2026/2026-08-26/DJI_0027.SRT"]),
            [
                "2026/2026-08-26/2/DJI_20260826184226_0027_D.MP4",
                "2026/2026-08-26/2/DJI_0027.SRT"
            ]
        );
    }

    #[test]
    fn only_what_smb_cannot_store_is_escaped() {
        assert_eq!(smb_name("GX010123.MP4").unwrap(), "GX010123.MP4");
        assert_eq!(smb_name("Ride é 1.mp4").unwrap(), "Ride é 1.mp4");
        assert_eq!(smb_name("a:b?~.").unwrap(), "a~3ab~3f~7e~2e");
        assert!(smb_name(&"a".repeat(221)).is_err());
    }

    #[test]
    fn editing_copies_sit_at_the_same_path_plus_mov() {
        assert_eq!(
            editing_path(Path::new("2026/2026-08-26/2/GX010123.MP4")).unwrap(),
            Path::new("2026/2026-08-26/2/GX010123.MP4.mov")
        );
        // Batches from before this layout.
        assert_eq!(
            editing_path(Path::new(
                "2026-08-26/abc/originals/f-dcim/f-~47x.~4d~50~34"
            ))
            .unwrap(),
            Path::new("2026-08-26/abc/clips/f-dcim/f-~47x.~4d~50~34.mov")
        );
    }
}

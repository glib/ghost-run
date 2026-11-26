//! Camera cards the system has mounted: a removable filesystem under a
//! `media` folder (`/run/media/<user>/<label>`, `/media/<user>/<label>`) with
//! a `DCIM` folder at its top, the folder every camera writes. Found from the
//! mount table, so a card is recognised however often it's reinserted, and
//! whatever path it lands on.

use std::path::{Path, PathBuf};

use crate::storage::parse_mounts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// Where it's mounted.
    pub path: PathBuf,
    /// Its label (the mount folder's name).
    pub name: String,
}

/// The cards mounted now.
pub fn find() -> Vec<Card> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    parse(&text, has_dcim)
}

/// The cards in a mount table; `camera` says whether a mount point holds a
/// camera's folders.
pub fn parse(mountinfo: &str, camera: impl Fn(&Path) -> bool) -> Vec<Card> {
    let mut cards: Vec<Card> = parse_mounts(mountinfo)
        .into_iter()
        .filter(|m| {
            matches!(
                m.fs_type.as_str(),
                "vfat" | "exfat" | "ntfs" | "ntfs3" | "fuseblk" | "msdos"
            )
        })
        .filter(|m| m.point.components().any(|c| c.as_os_str() == "media"))
        .filter(|m| camera(&m.point))
        .map(|m| Card {
            name: m
                .point
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: m.point,
        })
        .collect();
    cards.sort_by(|a, b| a.path.cmp(&b.path));
    cards.dedup();
    cards
}

fn has_dcim(point: &Path) -> bool {
    std::fs::read_dir(point).is_ok_and(|entries| {
        entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().eq_ignore_ascii_case("DCIM") && e.path().is_dir())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_removable_camera_cards_under_media() {
        let table = "\
36 1 8:2 / / rw - btrfs /dev/sda2 rw
90 36 179:1 / /run/media/liam/OSMO\\040ACTION rw,nosuid - exfat /dev/mmcblk0p1 rw
91 36 8:17 / /run/media/liam/BACKUP rw - exfat /dev/sdb1 rw
92 36 0:50 / /mnt/nas rw - cifs //nas/footage rw
93 36 8:33 / /media/liam/GOPRO rw - vfat /dev/sdc1 rw
";
        let cards = parse(table, |p| !p.ends_with("BACKUP"));
        assert_eq!(
            cards,
            [
                Card {
                    path: "/media/liam/GOPRO".into(),
                    name: "GOPRO".into()
                },
                Card {
                    path: "/run/media/liam/OSMO ACTION".into(),
                    name: "OSMO ACTION".into()
                },
            ]
        );
    }
}

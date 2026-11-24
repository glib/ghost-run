//! Where rendered overlays go, and telling them apart from footage.
//!
//! A clip's overlay is `<stem>.overlay.mov`, in the overlay folder chosen in
//! Settings, else in an `Overlays` folder beside the clip. Scans skip both, so
//! overlays never show up as clips.

use std::path::{Path, PathBuf};

/// The folder beside each clip that overlays go in by default.
pub const FOLDER: &str = "Overlays";
const SUFFIX: &str = ".overlay.mov";

/// Where `clip`'s overlay goes: in `folder` if one is chosen, else beside it.
pub fn overlay_path(clip: &Path, folder: Option<&Path>) -> PathBuf {
    let stem = clip.file_stem().unwrap_or_default().to_string_lossy();
    let dir = match folder {
        Some(f) => f.to_owned(),
        None => clip.parent().unwrap_or(Path::new("")).join(FOLDER),
    };
    dir.join(format!("{stem}{SUFFIX}"))
}

/// Whether `path` is a rendered overlay.
pub fn is_overlay(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(SUFFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlays_go_beside_the_clip_unless_a_folder_is_chosen() {
        let clip = Path::new("/v/day1/DJI_0001.MP4");
        assert_eq!(
            overlay_path(clip, None),
            Path::new("/v/day1/Overlays/DJI_0001.overlay.mov")
        );
        assert_eq!(
            overlay_path(clip, Some(Path::new("/out"))),
            Path::new("/out/DJI_0001.overlay.mov")
        );
        // An editing copy keeps the original's extension in the name.
        assert_eq!(
            overlay_path(Path::new("/v/a.MP4.mov"), None),
            Path::new("/v/Overlays/a.MP4.overlay.mov")
        );
        assert!(is_overlay(Path::new("/v/Overlays/a.MP4.overlay.mov")));
        assert!(!is_overlay(Path::new("/v/a.MP4.mov")));
    }
}

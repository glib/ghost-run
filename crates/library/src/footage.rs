//! Where footage lives (decision 0005): each original only on the NAS, under
//! the **footage root** (Import's archive folder), and, while it's being
//! edited, a local editing copy at a fixed place under the **local root**
//! (Import's editing folder): the same path with `.mov` added
//! (`ingest::layout::editing_path`), its proxy in a `Proxy` folder beside it.
//! The Library lists the original once, with its local copy if there is one.

use std::path::{Component, Path, PathBuf};

/// The two roots, as set in Import's settings. Either may be unset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Roots {
    /// The footage root on the NAS.
    pub nas: Option<PathBuf>,
    /// Whether the NAS is mounted as set up. When it isn't, its clips stay in
    /// the Library as last catalogued rather than vanishing.
    pub nas_online: bool,
    /// Where local editing copies go.
    pub local: Option<PathBuf>,
    /// Where the catalog's previews are ([`crate::preview::dir`]).
    pub catalog: PathBuf,
}

impl Roots {
    /// Whether `path` is an original on the NAS.
    pub fn on_nas(&self, path: &Path) -> bool {
        self.nas.as_ref().is_some_and(|root| path.starts_with(root))
    }

    /// Where `original`'s local editing copy goes (whether or not it's there):
    /// `None` for a clip not on the NAS, or with no local root set.
    pub fn local_copy(&self, original: &Path) -> Option<PathBuf> {
        let relative = original.strip_prefix(self.nas.as_ref()?).ok()?;
        let copy = ingest::layout::editing_path(relative).ok()?;
        Some(self.local.as_ref()?.join(copy))
    }

    /// The NAS original a local editing copy was made from (whether or not
    /// it's there): `None` for any other file.
    pub fn original_of(&self, copy: &Path) -> Option<PathBuf> {
        let relative = copy.strip_prefix(self.local.as_ref()?).ok()?;
        let name = relative.file_name()?.to_str()?;
        let original = name.strip_suffix(".mov")?;
        if !original.contains('.') {
            return None;
        }
        // Batches before the day-folder layout: DATE/UUID/clips/… → originals/….
        let parts: Vec<Component> = relative.components().collect();
        let relative: PathBuf = if parts.len() >= 4 && parts[2].as_os_str() == "clips" {
            parts[..2]
                .iter()
                .map(|c| c.as_os_str())
                .chain(std::iter::once("originals".as_ref()))
                .chain(parts[3..].iter().map(|c| c.as_os_str()))
                .collect()
        } else {
            relative.to_path_buf()
        };
        Some(self.nas.as_ref()?.join(relative.with_file_name(original)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Roots {
        Roots {
            nas: Some("/nas/Footage".into()),
            nas_online: true,
            local: Some("/home/me/Editing".into()),
            catalog: "/c".into(),
        }
    }

    #[test]
    fn originals_and_their_local_copies_map_both_ways() {
        let r = roots();
        let original = Path::new("/nas/Footage/2026/2026-11-24/DJI_1.MP4");
        let copy = Path::new("/home/me/Editing/2026/2026-11-24/DJI_1.MP4.mov");
        assert!(r.on_nas(original));
        assert!(!r.on_nas(copy));
        assert_eq!(r.local_copy(original).as_deref(), Some(copy));
        assert_eq!(r.original_of(copy).as_deref(), Some(original));
        // The old layout's twin folders.
        let old = Path::new("/nas/Footage/2026-08-26/abc/originals/f-x.MP4");
        let old_copy = Path::new("/home/me/Editing/2026-08-26/abc/clips/f-x.MP4.mov");
        assert_eq!(r.local_copy(old).as_deref(), Some(old_copy));
        assert_eq!(r.original_of(old_copy).as_deref(), Some(old));
        // Anything else isn't one.
        assert_eq!(r.original_of(Path::new("/home/me/Editing/notes.mov")), None);
        assert_eq!(r.original_of(Path::new("/elsewhere/DJI_1.MP4.mov")), None);
        assert_eq!(r.local_copy(Path::new("/elsewhere/DJI_1.MP4")), None);
    }
}

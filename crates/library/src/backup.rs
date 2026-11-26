//! Whether each clip's original is safely on the NAS, from the Import page's
//! journal (`ingest::jobs::archived`): an editing copy the journal made, or an
//! original it archived, counts once its archive copy is verified and still
//! there. Everything else is local only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An original the import journal verified in the archive (absolute paths).
#[derive(Debug, Clone, PartialEq)]
pub struct Archived {
    pub original: PathBuf,
    pub editing: Option<PathBuf>,
    pub verified_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Backup {
    /// Not imported through Ghost Run: no verified copy on the NAS.
    LocalOnly,
    /// An editing copy whose original is verified on the NAS.
    OnNas {
        original: PathBuf,
        verified_at: String,
    },
    /// The verified original itself (a NAS folder in the Library).
    IsOriginal { verified_at: String },
    /// Verified once, but the original isn't there now (moved, deleted, or
    /// the share isn't mounted).
    Missing { original: PathBuf },
}

impl Backup {
    pub fn is_safe(&self) -> bool {
        matches!(self, Self::OnNas { .. } | Self::IsOriginal { .. })
    }
}

/// Each clip's backup state. `exists` says whether an archived original is
/// still there (a stat on the share).
pub fn states<'a>(
    clips: impl IntoIterator<Item = &'a Path>,
    archived: &[Archived],
    exists: impl Fn(&Path) -> bool,
) -> HashMap<PathBuf, Backup> {
    let mut by_path: HashMap<&Path, &Archived> = HashMap::new();
    for a in archived {
        by_path.insert(&a.original, a);
        if let Some(e) = &a.editing {
            by_path.insert(e, a);
        }
    }
    clips
        .into_iter()
        .map(|clip| {
            let state = match by_path.get(clip) {
                None => Backup::LocalOnly,
                Some(a) if !exists(&a.original) => Backup::Missing {
                    original: a.original.clone(),
                },
                Some(a) if a.original == clip => Backup::IsOriginal {
                    verified_at: a.verified_at.clone(),
                },
                Some(a) => Backup::OnNas {
                    original: a.original.clone(),
                    verified_at: a.verified_at.clone(),
                },
            };
            (clip.to_path_buf(), state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_are_safe_only_while_their_verified_original_is_there() {
        let archived = [Archived {
            original: "/nas/2026/2026-08-26/A.MP4".into(),
            editing: Some("/edit/2026/2026-08-26/A.MP4.mov".into()),
            verified_at: "2026-08-26T20:00:00Z".into(),
        }];
        let clips = [
            Path::new("/edit/2026/2026-08-26/A.MP4.mov"),
            Path::new("/nas/2026/2026-08-26/A.MP4"),
            Path::new("/edit/other.mp4"),
        ];
        let s = states(clips, &archived, |_| true);
        assert_eq!(
            s[clips[0]],
            Backup::OnNas {
                original: "/nas/2026/2026-08-26/A.MP4".into(),
                verified_at: "2026-08-26T20:00:00Z".into()
            }
        );
        assert!(s[clips[1]].is_safe());
        assert_eq!(s[clips[2]], Backup::LocalOnly);
        let gone = states(clips, &archived, |_| false);
        assert!(matches!(gone[clips[0]], Backup::Missing { .. }));
        assert!(!gone[clips[0]].is_safe());
    }
}

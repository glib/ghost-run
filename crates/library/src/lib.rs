//! Ghost Run's footage library: the clips found in your footage folders, what
//! ffprobe says about them, and their proxies. Headless, so the app's Library
//! page is a thin view over it and everything here is tested without a display.
//!
//! - [`settings`]: the app's own settings (footage folders, run database).
//! - [`store`]: the SQLite library (`$XDG_DATA_HOME/ghost-run/library.sqlite3`).
//! - [`scan`]: walking the folders, probing new or changed files.
//! - [`proxy`]: half-resolution ProRes Proxy copies in a `Proxy` folder beside
//!   each clip, frame for frame and timecode for timecode the same as the clip,
//!   shared by Resolve, the frame viewer and auto-markers.
//! - [`overlays`]: where rendered overlays go.
//! - [`backup`]: whether each clip's original is verified on the NAS.
//! - [`catalog_backup`]: a daily copy of the catalog and run database on the NAS.
//! - [`auto`]: placing a clip's markers from the course's reference run, and
//!   telling which course a clip is.

pub mod auto;
pub mod backup;
pub mod catalog_backup;
pub mod frames;
pub mod overlays;
pub mod probe;
pub mod proxy;
pub mod scan;
pub mod settings;
pub mod store;
pub mod thumbs;

pub use probe::MediaInfo;
pub use store::{Clip, Draft, Library, ProxyState, Rendered};

/// Set to stop a scan or a proxy encode at the next check.
pub type Cancel = std::sync::Arc<std::sync::atomic::AtomicBool>;

pub(crate) fn cancelled(cancel: &Cancel) -> bool {
    cancel.load(std::sync::atomic::Ordering::Relaxed)
}

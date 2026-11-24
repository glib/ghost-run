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

pub mod frames;
pub mod probe;
pub mod proxy;
pub mod scan;
pub mod settings;
pub mod store;

pub use probe::MediaInfo;
pub use store::{Clip, Draft, Library, ProxyState};

/// Set to stop a scan or a proxy encode at the next check.
pub type Cancel = std::sync::Arc<std::sync::atomic::AtomicBool>;

pub(crate) fn cancelled(cancel: &Cancel) -> bool {
    cancel.load(std::sync::atomic::Ordering::Relaxed)
}

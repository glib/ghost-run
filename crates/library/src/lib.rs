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
//! - [`footage`]: the NAS footage root and local editing copies.
//! - [`hydrate`]: local copies of NAS clips, made and removed.
//! - [`preview`]: the catalog's small preview and audio of NAS clips.
//! - [`organise`]: rides, tags, flags and collections.
//! - [`overlays`]: where rendered overlays go.
//! - [`backup`]: whether each clip's original is verified on the NAS.
//! - [`catalog_backup`]: a daily copy of the catalog and run database on the NAS.
//! - [`videos`]: videos, the clips in them, and what they used.
//! - [`update`]: updates from Ghost Run's GitHub releases.
//! - [`auto`]: placing a clip's markers from the course's reference run, and
//!   telling which course a clip is.

pub mod auto;
pub mod backup;
pub mod brand;
pub mod catalog_backup;
pub mod db;
pub mod features;
pub mod footage;
pub mod frames;
pub mod hydrate;
pub mod kinds;
pub mod machine;
pub mod organise;
pub mod overlays;
pub mod power;
pub mod preview;
pub mod probe;
pub mod proxy;
pub mod relink;
pub mod resolve_db;
pub mod scan;
pub mod settings;
pub mod shared;
pub mod store;
pub mod strava;
pub mod thumbs;
pub mod update;
pub mod videos;

pub use probe::MediaInfo;
pub use store::{Clip, Draft, Library, ProxyState, Rendered};

/// Set to stop a scan or a proxy encode at the next check.
pub type Cancel = std::sync::Arc<std::sync::atomic::AtomicBool>;

pub(crate) fn cancelled(cancel: &Cancel) -> bool {
    cancel.load(std::sync::atomic::Ordering::Relaxed)
}

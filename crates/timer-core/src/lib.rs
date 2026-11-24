//! Run timing for Resolve Timer: courses, runs and the YAML database they live
//! in (schema v2, shared with the Python tool), sector/lap timing, matching a
//! clip to its stored run, and course statistics (best and optimal laps).
//!
//! Behaviour follows the Python implementation in `src/resolve_timer/` exactly;
//! `scripts/parity.sh` checks the two CLIs give the same output.

pub mod board;
pub mod capture_time;
pub mod commit;
pub mod database;
pub mod edit;
pub mod file;
pub mod markers;
pub mod matching;
pub mod models;
pub mod overlay;
pub mod preview;
pub mod py;
pub mod stats;
pub mod timing;
pub mod validation;
mod yaml;

pub use database::{DatabaseError, TimerDatabase};
pub use models::{
    Course, MarkerFrames, MarkerSnapshot, RawMarker, RunRecord, SectorTiming, TimingResult,
};

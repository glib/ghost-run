//! Run timing for Resolve Timer: courses, runs and the YAML database they live
//! in (schema v2), sector/lap timing, matching a clip to its stored run, and
//! course statistics (best and optimal laps).
//!
//! It was ported from the Python tool (retired in decision 0008) to behave
//! exactly the same, PyYAML's file layout and Python's rounding included, so
//! existing databases read and write as before. `scripts/golden.sh` holds the
//! CLI to the outputs recorded when the two last matched.

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
pub mod tracks;
pub mod validation;
mod yaml;

pub use database::{DatabaseError, TimerDatabase};
pub use models::{
    Course, MarkerFrames, MarkerSnapshot, RawMarker, RunRecord, SectorTiming, TimingResult,
};

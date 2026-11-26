#[cfg(not(target_os = "linux"))]
compile_error!("Ghost Run currently supports Linux only");

pub mod catalog;
pub mod diagnostics;
pub mod discovery;
pub mod errors;
pub mod jobs;
pub mod layout;
pub mod media;
pub mod model;
pub mod pipeline;
pub mod settings;
pub mod storage;
pub mod verification;

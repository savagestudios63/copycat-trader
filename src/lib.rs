//! Library surface of the copycat-trader crate — exposes modules so they can
//! be exercised by integration tests under `tests/`. The binary entry point
//! lives in `src/main.rs` and uses these modules directly.

pub mod config;
pub mod db;
pub mod decoder;
pub mod engine;
pub mod executor;
pub mod geyser;
pub mod risk;
pub mod tui;
pub mod types;

//! CLI command pipelines (the I/O-bearing orchestration layer). Pure logic
//! lives in `merge`, `overrides`, `export`; this layer wires DB + HTTP + config.

pub mod cf_sync;
pub mod list;
pub mod show;
pub mod ssh;
pub mod sync;

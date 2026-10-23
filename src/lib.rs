//! bettergrep: file lineage, per-folder history and system change monitoring for Windows.
//!
//! The database schema and the `config.json` and `.bettergrep.json` formats are
//! documented in `docs/`.

#[cfg(not(windows))]
compile_error!("bettergrep targets Windows only");

pub mod autostart;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod drive;
pub mod fileinfo;
pub mod folder_index;
pub mod grep;
pub mod log;
pub mod mcp;
pub mod operations;
pub mod paths;
pub mod query;
pub mod registry;
pub mod store;
pub mod trace;
pub mod tracker;
pub mod watcher;
pub mod win;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

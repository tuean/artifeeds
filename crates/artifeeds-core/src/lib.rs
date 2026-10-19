//! Artifeeds — feed ingestion core.
//!
//! Layering (each layer only depends on the ones above it):
//!
//! ```text
//!   config   ── parsing + validation + hot reload of many external config files
//!   text     ── zero-dependency, allocation-light HTML → text normalisation
//!   model    ── shared plain-old-data types (articles, stats, snapshots)
//!   http     ── one pooled client, conditional GET, global + per-host concurrency
//!   parse    ── feed-rs → RawEntry, bounded by size/entry budgets
//!   dedup    ── hot-path hash sets that stop duplicates before they reach SQLite
//!   store    ── SQLite (WAL) with a single writer thread and batched transactions
//!   sched    ── adaptive polling policy (backoff, jitter, quiet-feed stretching)
//!   engine   ── orchestration: scheduler loop → fetch tasks → ingest → snapshot
//! ```
//!
//! Design invariants that keep it fast and small:
//!
//! 1. **The UI never takes a lock.** [`engine::Engine`] publishes immutable
//!    [`model::Snapshot`] values through an `ArcSwap`; readers pay one atomic load.
//! 2. **Nothing unbounded.** Every queue (writer queue, ingest channel, in-flight
//!    fetch set, in-memory article window, per-fetch body size) has a hard cap.
//! 3. **Bytes on the wire are the expensive part.** Conditional requests plus
//!    adaptive intervals mean a quiet feed costs one 304 and nothing else.
//! 4. **Strings are shared, not copied.** Interned `Arc<str>` fields everywhere.

pub mod archive;
pub mod config;
pub mod dedup;
pub mod dedupe;
pub mod discover;
pub mod engine;
pub mod http;
pub mod metrics;
pub mod model;
pub mod parse;
pub mod sched;
pub mod store;
pub mod suggest;
pub mod text;
pub mod zhihu;

pub use config::{CompiledConfig, ConfigStore, Settings, SourceConfig, SourceSpec};
pub use engine::{Engine, UiCommand};
pub use model::{Article, SourceId, Snapshot, Stats};
pub use store::Store;

/// Sensible default location for the database on each platform.
pub fn default_data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("ARTIFEEDS_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new(&home).join("Library/Application Support/Artifeeds")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = std::env::var("XDG_DATA_HOME")
            .unwrap_or_else(|_| format!("{home}/.local/share"));
        std::path::Path::new(&base).join("artifeeds")
    }
}

/// Sensible default location for `feeds.toml`.
pub fn default_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ARTIFEEDS_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new(&home).join("Library/Application Support/Artifeeds/feeds.toml")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = std::env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| format!("{home}/.config"));
        std::path::Path::new(&base).join("artifeeds/feeds.toml")
    }
}

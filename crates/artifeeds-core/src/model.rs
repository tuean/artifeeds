//! Shared data types. Cheap to clone: every string is an interned `Arc<str>`.

use std::sync::Arc;

/// Dense in-memory index of a configured source. Rebuilt on every config reload
/// (it is a `Vec` index, never persisted).
pub type SourceId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Idle,
    Fetching,
    Ok,
    NotModified,
    Backoff,
    Error,
    Disabled,
}

impl Health {
    #[inline]
    pub fn dot(self) -> &'static str {
        match self {
            Health::Idle => "○",
            Health::Fetching => "◐",
            Health::Ok => "●",
            Health::NotModified => "◍",
            Health::Backoff => "◔",
            Health::Error => "✕",
            Health::Disabled => "–",
        }
    }

    #[inline]
    pub fn label(self) -> &'static str {
        match self {
            Health::Idle => "idle",
            Health::Fetching => "fetching",
            Health::Ok => "ok",
            Health::NotModified => "304",
            Health::Backoff => "backoff",
            Health::Error => "error",
            Health::Disabled => "off",
        }
    }
}

/// One article as rendered by the UI. `age` is precomputed at snapshot build
/// time so the render path performs zero formatting work.
#[derive(Debug, Clone)]
pub struct Article {
    pub id: i64,
    pub source: SourceId,
    pub db_source: i64,
    pub uid_hash: u64,
    pub content_hash: u64,
    pub title: Arc<str>,
    pub link: Arc<str>,
    pub author: Option<Arc<str>>,
    pub source_title: Arc<str>,
    pub group: Arc<str>,
    pub published: i64,
    pub fetched: i64,
    pub preview: Arc<str>,
    pub has_content: bool,
    pub read: bool,
    pub starred: bool,
    pub age: Arc<str>,
    /// Set once the original page has been saved locally (SingleFile-style).
    pub archive_path: Option<Arc<str>>,
    pub archive_bytes: u32,
    /// When this was last opened, and how far it was read (0..1). This is the
    /// reading record: it survives restarts because it lives in the database.
    pub read_at: i64,
    pub progress: f32,
}

impl Article {
    /// Keyword filter run against the already-in-memory window. Operates on
    /// lowercase haystack built once per article (`title` + `preview`).
    #[inline]
    pub fn matches(&self, needle_lower: &str) -> bool {
        contains_ci(&self.title, needle_lower) || contains_ci(&self.preview, needle_lower)
    }
}

#[inline]
pub fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.is_ascii() {
        // ASCII fast path avoids allocating a lowercased copy.
        let h = haystack.as_bytes();
        let n = needle_lower.as_bytes();
        if n.len() > h.len() {
            return false;
        }
        h.windows(n.len())
            .any(|w| w.eq_ignore_ascii_case(n))
    } else {
        haystack.to_lowercase().contains(needle_lower)
    }
}

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: SourceId,
    pub db_id: i64,
    pub title: Arc<str>,
    pub group: Arc<str>,
    /// Labels from the configuration (`tags = [...]`, or the upstream OPML/CSV
    /// tags). Interned and shared, so the sidebar and the filters cost nothing.
    pub tags: Arc<[Arc<str>]>,
    pub host: Arc<str>,
    pub unread: u32,
    pub total: u32,
    pub health: Health,
    /// The reader marked this subscription as one of their own. Not a config
    /// field: it belongs to the reader, not to the list the feed came from.
    pub featured: bool,
    pub interval_secs: u64,
    pub effective_interval_secs: u64,
    pub last_fetch: i64,
    pub next_due_secs: i64,
    pub last_error: Option<Arc<str>>,
    pub consecutive_errors: u32,
    pub avg_latency_ms: u32,
    pub etag: bool,
}

#[derive(Debug, Clone)]
pub struct GroupRow {
    pub name: Arc<str>,
    pub sources: u32,
    pub unread: u32,
    pub total: u32,
}

/// A tag and everything filed under it.
#[derive(Debug, Clone)]
pub struct TagRow {
    pub name: Arc<str>,
    pub sources: u32,
    pub unread: u32,
    pub total: u32,
}

/// Rolling counters. All are cheap `u64`s updated with relaxed atomics.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub requests: u64,
    pub responses_304: u64,
    pub responses_200: u64,
    pub errors: u64,
    pub items_new: u64,
    pub items_seen: u64,
    pub dupes_dropped: u64,
    pub bytes_in: u64,
    pub bytes_saved_304: u64,
    pub in_flight: u64,
    pub queue_depth: u64,
    pub sources_total: u64,
    pub sources_enabled: u64,
    pub sources_due: u64,
    pub avg_latency_ms: u32,
    pub p95_latency_ms: u32,
    pub last_batch_ms: u32,
    pub db_bytes: u64,
    /// Size of the in-memory window — **not** the size of the current view, so
    /// the sidebar keeps describing the library while a search narrows the list.
    pub mem_articles: u64,
    /// Everything the database holds, which is more than the window: the window
    /// is a recency slice, the database is the history.
    pub db_articles: u64,
    pub db_unread: u64,
    pub window_unread: u64,
    pub window_starred: u64,
    pub config_files: u64,
    pub config_problems: u64,
    pub config_reloads: u64,
    pub uptime_secs: u64,
    pub rss_kib: u64,
    pub cpu_hint_pct: u32,
}

/// Immutable view of everything the UI needs. Published through an `ArcSwap`;
/// reading it costs one atomic load and never blocks the ingestion path.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub generation: u64,
    pub built_at: i64,
    pub articles: Arc<[Arc<Article>]>,
    pub sources: Arc<[SourceRow]>,
    pub groups: Arc<[GroupRow]>,
    /// Tags aggregated across feeds, most-used first.
    pub tags: Arc<[TagRow]>,
    pub stats: Stats,
    pub problems: Arc<[Arc<str>]>,
}

impl Snapshot {
    pub fn unread_total(&self) -> u32 {
        self.articles.iter().filter(|a| !a.read).count() as u32
    }
}

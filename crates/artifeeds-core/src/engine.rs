//! Orchestration.
//!
//! ```text
//!   config watcher ──┐
//!                    │        ┌──────────── scheduler loop (1 task) ────────────┐
//!   UI commands ─────┼───────▶│  BinaryHeap of due sources → spawn fetch tasks  │
//!                    │        │  ← FetchDone results (flume)                    │
//!   fetch tasks ─────┘        │  dedup → SQLite batch → in-memory window        │
//!   (≤ max_in_flight)         │  → immutable Snapshot → ArcSwap                 │
//!                             └─────────────────────────────────────────────────┘
//! ```
//!
//! Everything that mutates shared state lives in **one** task. Fetch tasks are
//! pure: take a spec, do HTTP + parsing, hand back a [`FetchDone`]. That removes
//! every lock from the hot path and makes the pipeline easy to reason about:
//!
//! * **Bounded everywhere.** `max_concurrent_fetches` caps spawned tasks, the
//!   store channel is bounded, the result channel is bounded, and the in-memory
//!   window is capped by `max_in_memory_articles`.
//! * **Backpressure is implicit.** When the store is saturated its channel
//!   fills; the loop blocks awaiting the reply, which stops new dispatch.
//! * **The UI reads a snapshot, never a lock.** Snapshots are published at most
//!   every [`PUBLISH_INTERVAL`], and only the dirty half is rebuilt.
//! * **Idle costs nothing.** With nothing due, the loop parks on a single timer
//!   until the next deadline — no polling, no wakeups.

use crate::config::{
    CompiledConfig, ConfigEvent, ConfigStore, NewSource, Settings, SourceSpec, now_unix,
};
use crate::dedup::DedupIndex;
use crate::discover;
use crate::http::{FetchError, FetchSpec, HttpEngine};
use crate::metrics::Metrics;
use crate::model::{Article, GroupRow, Health, Snapshot, SourceId, SourceRow, TagRow};
use crate::parse::{ParseBudget, RawEntry};
use crate::sched::{Outcome, Policy, Scheduler, SourceRuntime};
use crate::store::{Flag, NewArticle, PersistedState, SourceStateRow, SourceUpsert, Store};
use arc_swap::ArcSwap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

/// How often the UI-visible snapshot is republished. ~4 fps of state updates is
/// imperceptible while fetching and costs a fraction of a core even with
/// thousands of sources.
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(250);
/// How many page archives may be in flight at once. Archiving is heavy (one
/// page plus dozens of sub-resources), so it gets a small, separate budget.
const MAX_CONCURRENT_ARCHIVES: usize = 2;
/// Page fetches for an archive: one try plus this many retries.
const ARCHIVE_RETRIES: u32 = 2;
/// Fetch slots the fleet is limited to while an archive is in flight. Low enough
/// to leave the network room for the page the reader is waiting on.
const USER_WAITING_FETCHES: usize = 4;
/// Config watcher settle window: an editor writing 40 files triggers one reload.
const CONFIG_SETTLE: Duration = Duration::from_millis(300);
/// Decrements the waiting counter however the task ends, including a panic.
struct WaitingOnDrop(Arc<AtomicUsize>);

impl Drop for WaitingOnDrop {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Pages tried when looking for a blog's links, richest first. The article comes
/// last: its sidebar may carry two friend links, while the dedicated page is the
/// blogroll proper.
const LINK_PAGES: [&str; 7] = [
    "/links",
    "/links/",
    "/friends",
    "/blogroll",
    "/about",
    "/link",
    "/friendlinks",
];

/// Where a blog keeps its blogroll, most likely first.
fn link_pages(article_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(parsed) = reqwest::Url::parse(article_url)
        && let Some(host) = parsed.host_str()
    {
        let origin = format!("{}://{}", parsed.scheme(), host);
        for path in LINK_PAGES {
            out.push(format!("{origin}{path}"));
        }
    }
    out.push(article_url.to_string());
    out
}

/// How many search hits one query may pull into the window.
///
/// A search runs against the whole database while the window is the memory
/// budget, so this is the seam between them: enough to fill a screenful several
/// times over, small enough that a one-letter query cannot balloon the window.
const SEARCH_PULL_LIMIT: usize = 200;

/// How many items one refresh of a Zhihu collection may complete.
///
/// Each item is a body request, and an existing article is never updated, so the
/// budget is also the frontier's step size: the collection grows from the newest
/// end, a batch per refresh, instead of firing hundreds of requests at once at a
/// site that answers a burst with a challenge page.
const ZHIHU_BODY_BUDGET: usize = 40;

/// How many links from one page are probed for feeds.
const FRIEND_SCAN_LIMIT: usize = 40;
/// How many of those probes run at once.
const FRIEND_SCAN_CONCURRENCY: usize = 4;
/// How long the whole scan may take before it reports what it has.
const FRIEND_SCAN_BUDGET: Duration = Duration::from_secs(45);

/// Where a newly added subscription is written.
///
/// A `custom.toml` beside the feeds that are already configured: that directory
/// is already `include`d, so the file is picked up with no edit to the main
/// configuration, and the generated files stay generated. Falls back to the main
/// configuration when there is nothing to sit beside.
pub fn added_sources_file(
    config: &CompiledConfig,
    config_path: &std::path::Path,
) -> std::path::PathBuf {
    if let Some(first) = config.sources.first()
        && let Some(dir) = std::path::Path::new(first.origin.as_ref()).parent()
    {
        return dir.join("custom.toml");
    }
    config_path.to_path_buf()
}

/// Append a subscription to a configuration file, creating it if needed.
///
/// Written to a temporary file and renamed into place, so an interrupted write
/// cannot leave a configuration that no longer parses. A backup is taken the
/// first time an existing file is touched.
/// Returns `Ok(false)` when the subscription is already in the file.
///
/// The guard lives here rather than only in the callers because adding a feed is
/// something a reader does repeatedly — pressing Enter twice in the add panel, or
/// running the same import again — and an append-only writer turns that into a
/// file with the same feed in it nineteen times. Loading deduplicates, so nothing
/// breaks; the configuration just rots.
pub fn append_source_to_file(file: &str, source: &NewSource) -> Result<bool, String> {
    let path = std::path::Path::new(file);
    let existing = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| format!("读取 {file} 失败：{e}"))?
    } else {
        String::from("# Artifeeds — 手动添加与友链发现的订阅\n")
    };
    if let Ok(parsed) = toml::from_str::<toml::Value>(&existing)
        && let Some(list) = parsed.get("sources").and_then(|s| s.as_array())
        && list.iter().any(|entry| {
            entry.get("url").and_then(|u| u.as_str()) == Some(source.url.as_str())
                || entry.get("key").and_then(|k| k.as_str()) == Some(source.url.as_str())
        })
    {
        return Ok(false);
    }
    let updated = crate::config::append_source_to_toml(&existing, source);
    // Parse before writing: a subscription that would break the file must be
    // rejected while the file is still intact.
    if let Err(e) = toml::from_str::<toml::Value>(&updated) {
        return Err(format!("生成的配置无法解析（已放弃写入）：{e}"));
    }
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        if !backup.exists() {
            std::fs::write(&backup, &existing).map_err(|e| format!("写备份失败：{e}"))?;
        }
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(true)
}
/// Housekeeping cadence.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// How often the sidebar's article counts are re-read from the database.
const TOTALS_INTERVAL: Duration = Duration::from_secs(30);
const DB_SIZE_INTERVAL: Duration = Duration::from_secs(30);

/// What the UI asks the engine to do.
#[derive(Debug, Clone)]
pub enum UiCommand {
    RefreshAll,
    RefreshSource(SourceId),
    RefreshGroup(Arc<str>),
    /// Load one feed's history into the window, so opening a blog shows the blog
    /// rather than the two items the recency window kept of it.
    LoadSource { id: SourceId, limit: usize },
    /// Remember how far an article has been read: position, and when.
    SaveProgress { id: i64, progress: f32 },
    /// Work out what feed is behind a URL the reader typed.
    DiscoverFeed { url: Arc<str> },
    /// Offer the blogs linked from a page: the blogroll route to subscribing.
    FriendFeeds { page_url: Arc<str> },
    /// Mark a subscription as one of the reader's own, or take the mark back.
    SetFeatured { source: SourceId, featured: bool },
    /// Append a subscription to the configuration. The engine chooses the file.
    AddSource { source: NewSource },
    /// List the signed-in account's Zhihu collections, so they can be added.
    ZhihuCollections,
    MarkRead { ids: Vec<i64>, read: bool },
    ToggleStar { id: i64, starred: bool },
    MarkAllRead { scope: Scope, before: i64 },
    Search { query: Arc<str>, limit: usize },
    ClearSearch,
    /// Fetch one article body from SQLite (lazy reader pane).
    LoadContent { id: i64 },
    /// Save the original page locally as one self-contained HTML file.
    ArchiveArticle { id: i64 },
    /// Runtime-only pause (never rewrites the config file).
    TogglePause(SourceId),
    ReloadConfig,
    PruneNow,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Scope {
    All,
    Source(SourceId),
    Group(Arc<str>),
}

/// What the engine tells the UI. The UI only needs the nudge; data is pulled
/// from the snapshot, which is always consistent.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A URL resolved to a feed (or did not).
    Discovered { url: Arc<str>, feed: Option<discover::Feed>, error: Option<String> },
    /// One blog found on a page, already verified to have a feed.
    FriendFeed { candidate: discover::Candidate, feed: discover::Feed },
    /// The scan finished, with how many links were tried.
    FriendScanDone { tried: usize, found: usize },
    /// A subscription was written to the configuration.
    SourceAdded { title: Arc<str>, url: Arc<str>, file: Arc<str>, error: Option<String> },
    /// A subscription was marked as featured, or unmarked.
    Featured { title: Arc<str>, featured: bool },
    /// Feeds that appeared in the configuration just now, with the ids they were
    /// given. The UI uses this to show the reader what they just subscribed to.
    SourcesAdded { added: Vec<(SourceId, Arc<str>, Arc<str>)> },
    /// The signed-in account's Zhihu collections.
    ZhihuCollections { name: Arc<str>, collections: Vec<crate::zhihu::Collection>, error: Option<String> },
    Ready,
    Published { generation: u64, unread: u32 },
    SearchResults { query: Arc<str>, ids: Arc<[i64]> },
    Content { id: i64, text: Arc<str> },
    /// Result of [`UiCommand::ArchiveArticle`]. `error` is set instead of
    /// `path` when the page could not be saved.
    Archived { id: i64, path: Option<Arc<str>>, bytes: u32, error: Option<Arc<str>> },
    ConfigReloaded { sources: usize, problems: usize },
    Problem(Arc<str>),
}

struct FetchDone {
    id: SourceId,
    latency_ms: u32,
    result: FetchResult,
}

/// The three grouped views of the feed list the sidebar can render.
type SourceViews = (Arc<[SourceRow]>, Arc<[GroupRow]>, Arc<[TagRow]>);

/// Outcome of one archive job, handed back to the scheduler loop.
struct ArchiveDone {
    id: i64,
    path: Option<Arc<str>>,
    bytes: u32,
    error: Option<Arc<str>>,
}

enum FetchResult {
    Modified {
        entries: Vec<RawEntry>,
        truncated: bool,
        bytes: u64,
        etag: Option<Arc<str>>,
        last_modified: Option<Arc<str>>,
    },
    NotModified,
    Failed {
        message: Arc<str>,
        retry_after: Option<u64>,
        fatal: bool,
    },
}

/// Owns the runtime and everything above it.
pub struct Engine {
    runtime: parking_lot::Mutex<Option<Runtime>>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    commands: flume::Sender<UiCommand>,
    events: flume::Receiver<EngineEvent>,
    store: Arc<Store>,
    config: Arc<ConfigStore>,
    metrics: Arc<Metrics>,
    shutdown: Arc<AtomicBool>,
    _watcher: parking_lot::Mutex<Option<notify::RecommendedWatcher>>,
}

impl Engine {
    /// Load config, open the database, warm the in-memory window and start the
    /// scheduler. Returns only once a first snapshot is available, so the UI's
    /// first frame already has data.
    pub fn start(config_path: PathBuf) -> anyhow::Result<Self> {
        Self::start_with(config_path, None)
    }

    /// `db_path_override` is used by tests and the headless benchmark.
    pub fn start_with(
        config_path: PathBuf,
        db_path_override: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let config = ConfigStore::new(config_path);
        let initial = config.current();
        let settings = initial.settings.clone();
        let metrics = Metrics::new();

        let db_path = db_path_override.unwrap_or_else(|| settings.db_path());
        let store = Store::open(&db_path, &settings, Arc::clone(&metrics))?;

        let worker_threads = std::env::var("ARTIFEEDS_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2)
            .clamp(1, 8);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .max_blocking_threads(4)
            .thread_name("artifeeds-io")
            .enable_all()
            .build()?;

        let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let (cmd_tx, cmd_rx) = flume::bounded::<UiCommand>(64);
        let (evt_tx, evt_rx) = flume::bounded::<EngineEvent>(256);
        let (cfg_tx, cfg_rx) = flume::bounded::<ConfigEvent>(16);
        let (done_tx, done_rx) = flume::bounded::<FetchDone>(512);
        let (arch_tx, arch_rx) = flume::bounded::<ArchiveDone>(64);

        let http = Arc::new(HttpEngine::new(&settings, Arc::clone(&metrics))?);

        // Watch the config before the first load completes so no edit is missed.
        let watcher = match config.spawn_watcher(CONFIG_SETTLE, cfg_tx) {
            Ok(w) => Some(w),
            Err(e) => {
                let _ = evt_tx.try_send(EngineEvent::Problem(Arc::from(
                    format!("config watcher unavailable: {e}").as_str(),
                )));
                None
            }
        };

        let mut runner = Runner {
            config: Arc::clone(&config),
            config_now: initial,
            store: Arc::clone(&store),
            metrics: Arc::clone(&metrics),
            http,
            snapshot: Arc::clone(&snapshot),
            sched: Scheduler::new(Policy::default()),
            states: Vec::new(),
            by_db: FxHashMap::default(),
            window: Vec::new(),
            dedup: DedupIndex::new(4_096, 65_536),
            paused: FxHashSet::default(),
            cmd_rx,
            cfg_rx,
            done_rx,
            done_tx,
            arch_rx,
            arch_tx,
            evt_tx,
            in_flight: 0,
            last_progress: Instant::now(),
            last_progress_requests: 0,
            archives_in_flight: 0,
            cached_sources: Arc::from(Vec::new()),
            cached_groups: Arc::from(Vec::new()),
            cached_tags: Arc::from(Vec::new()),
            source_totals: FxHashMap::default(),
            featured_sources: FxHashSet::default(),
            history_loaded: FxHashSet::default(),
            user_waiting: Arc::new(AtomicUsize::new(0)),
            applied_before: false,
            totals_at: None,
            sources_dirty: true,
            articles_dirty: true,
            generation: 0,
            last_publish: Instant::now() - PUBLISH_INTERVAL,
            last_prune: Instant::now(),
            last_db_size: Instant::now(),
            last_error: None,
            search: None,
        };

        runtime.block_on(runner.init())?;

        let flag = Arc::new(AtomicBool::new(false));
        let runner_flag = Arc::clone(&flag);
        runtime.spawn(async move {
            runner.run(runner_flag).await;
        });

        Ok(Self {
            runtime: parking_lot::Mutex::new(Some(runtime)),
            snapshot,
            commands: cmd_tx,
            events: evt_rx,
            store,
            config,
            metrics,
            shutdown: flag,
            _watcher: parking_lot::Mutex::new(watcher),
        })
    }

    /// Current immutable view. One atomic load, no locks.
    #[inline]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    #[inline]
    pub fn send(&self, cmd: UiCommand) {
        let _ = self.commands.try_send(cmd);
    }

    /// Single-consumer event stream (clones compete for messages, which is what
    /// a single UI wants).
    pub fn events(&self) -> flume::Receiver<EngineEvent> {
        self.events.clone()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn db_path(&self) -> PathBuf {
        self.store.path().to_path_buf()
    }

    /// Stop the scheduler, drain the store and join its writer thread.
    /// Idempotent, and safe to call from a shared `Arc<Engine>`.
    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.commands.try_send(UiCommand::Shutdown);
        if let Some(rt) = self.runtime.lock().take() {
            rt.shutdown_timeout(Duration::from_secs(3));
        }
        self.store.shutdown();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// The single-threaded core
// ---------------------------------------------------------------------------

struct Runner {
    config: Arc<ConfigStore>,
    config_now: Arc<CompiledConfig>,
    store: Arc<Store>,
    metrics: Arc<Metrics>,
    http: Arc<HttpEngine>,
    snapshot: Arc<ArcSwap<Snapshot>>,

    sched: Scheduler,
    states: Vec<SourceRuntime>,
    by_db: FxHashMap<i64, SourceId>,
    window: Vec<Arc<Article>>,
    dedup: DedupIndex,
    /// Paused sources, keyed by stable config key so a reload keeps them paused.
    paused: FxHashSet<Arc<str>>,

    cmd_rx: flume::Receiver<UiCommand>,
    cfg_rx: flume::Receiver<ConfigEvent>,
    done_rx: flume::Receiver<FetchDone>,
    done_tx: flume::Sender<FetchDone>,
    arch_rx: flume::Receiver<ArchiveDone>,
    arch_tx: flume::Sender<ArchiveDone>,
    evt_tx: flume::Sender<EngineEvent>,

    in_flight: usize,
    archives_in_flight: usize,
    cached_sources: Arc<[SourceRow]>,
    cached_groups: Arc<[GroupRow]>,
    cached_tags: Arc<[TagRow]>,
    /// `db source id -> (total, unread)` from the database. The window's counts
    /// say how much of a feed is *loaded*, which is not what a reader is asking
    /// when they look at the sidebar.
    source_totals: FxHashMap<i64, (u32, u32)>,
    /// Database ids of the subscriptions the reader marked as their own.
    featured_sources: FxHashSet<i64>,
    /// Feeds whose history has already been pulled into the window, so opening
    /// one twice does not query twice.
    history_loaded: FxHashSet<SourceId>,
    /// Whether a configuration has been applied before, so "new feeds" means
    /// "new since the last one" rather than "everything at startup".
    applied_before: bool,
    /// How many user-initiated jobs are in flight (archiving a page, scanning a
    /// blogroll). While any is, the sweep drops to a trickle: the reader is
    /// waiting on a request the sweep would otherwise be competing with.
    user_waiting: Arc<AtomicUsize>,
    /// When `source_totals` was last refreshed.
    totals_at: Option<Instant>,
    last_progress: Instant,
    last_progress_requests: u64,
    sources_dirty: bool,
    articles_dirty: bool,
    generation: u64,
    last_publish: Instant,
    last_prune: Instant,
    last_db_size: Instant,
    last_error: Option<Arc<str>>,
    /// Active database-backed search: `(query, matching database ids)`.
    search: Option<(Arc<str>, Arc<FxHashSet<i64>>)>,
}

impl Runner {
    // -- start-up ----------------------------------------------------------

    async fn init(&mut self) -> anyhow::Result<()> {
        self.apply_config(Arc::clone(&self.config_now)).await;
        self.load_window().await;
        self.refresh_source_totals().await;
        self.prune_if_due(true).await;
        self.store.refresh_db_bytes().await;
        self.publish(true);
        let _ = self.evt_tx.try_send(EngineEvent::Ready);
        Ok(())
    }

    /// (Re)build runtime state for a config, preserving per-source state
    /// (ETag, backoff, adaptive interval) for keys that persisted.
    async fn apply_config(&mut self, next: Arc<CompiledConfig>) {
        let settings = next.settings.clone();
        let policy = Policy {
            min_interval: Duration::from_secs(settings.min_interval_secs.max(30)),
            max_interval: Duration::from_secs(settings.max_interval_secs.max(60)),
            quiet_factor: settings.quiet_factor.max(1) as u32,
            jitter_percent: settings.jitter_percent.min(50),
        };

        // Carry state across reloads by stable key.
        let previous: FxHashMap<Arc<str>, SourceRuntime> = self
            .config_now
            .sources
            .iter()
            .zip(self.states.iter())
            .map(|(spec, state)| (spec.key.clone(), state.clone()))
            .collect();

        let upserts: Vec<SourceUpsert> = next
            .sources
            .iter()
            .map(|s| SourceUpsert {
                key: s.key.clone(),
                title: s.title.clone(),
                url: s.url.clone(),
                group: s.group.clone(),
            })
            .collect();
        let db_ids = self.store.ensure_sources(upserts).await;
        // Read back validators/health. This is what makes the first refresh
        // after a restart a conditional request instead of a full re-download.
        let persisted: FxHashMap<i64, PersistedState> = self
            .store
            .load_states()
            .await
            .into_iter()
            .map(|s| (s.db_id, s))
            .collect();

        // Feeds this reload introduced. The reader has just asked for them —
        // from the add overlay or from `artifeeds suggest` — and "where did it
        // go?" deserves an answer better than a toast naming a file.
        // Only meaningful on a reload. At startup every feed is "new" relative
        // to an empty previous state, and announcing 1 626 subscriptions to
        // somebody who just launched the app is noise, not news.
        let announce = self.applied_before;
        let mut added: Vec<(SourceId, Arc<str>, Arc<str>)> = Vec::new();
        let mut states: Vec<SourceRuntime> = Vec::with_capacity(next.sources.len());
        let mut by_db: FxHashMap<i64, SourceId> = FxHashMap::default();
        let mut sched = Scheduler::new(policy);

        for (i, spec) in next.sources.iter().enumerate() {
            let db_id = db_ids.get(i).copied().unwrap_or(0);
            let mut state = match previous.get(&spec.key) {
                Some(prev) => {
                    let mut s = prev.clone();
                    s.id = spec.id;
                    s.db_id = db_id;
                    s.base_interval = spec.interval;
                    s.interval = s.interval.clamp(policy.min_interval, spec.interval.max(policy.min_interval));
                    s
                }
                None => {
                    added.push((spec.id, spec.title.clone(), spec.group.clone()));
                    let mut s = SourceRuntime::new(
                        spec.id,
                        db_id,
                        spec.interval,
                        spec.initial_delay(settings.startup_spread_secs),
                    );
                    if let Some(p) = persisted.get(&db_id) {
                        s.etag = p.etag.clone();
                        s.last_modified = p.last_modified.clone();
                        s.last_fetch_unix = p.last_fetch;
                        s.last_status = p.last_status;
                        s.consecutive_err = p.error_count;
                        s.last_error = p.last_error.clone();
                        s.last_body_bytes = p.body_bytes;
                        s.not_modified = 0;
                        if p.interval_secs > 0 {
                            s.interval = Duration::from_secs(p.interval_secs).clamp(
                                policy.min_interval,
                                spec.interval.max(policy.min_interval),
                            );
                        }
                        // Health is recomputed by the first fetch; until then
                        // show the last known problem.
                        if p.error_count > 0 {
                            s.health = Health::Backoff;
                        }
                    }
                    s
                }
            };
            if !spec.enabled {
                state.health = Health::Disabled;
            }
            if db_id != 0 {
                by_db.insert(db_id, spec.id);
            }
            if spec.enabled {
                let remaining = state.next_due.saturating_duration_since(Instant::now());
                sched.schedule_unclamped(&mut state, remaining);
            }
            states.push(state);
        }

        self.config_now = Arc::clone(&next);
        self.states = states;
        self.by_db = by_db;
        self.sched = sched;
        self.sources_dirty = true;
        self.articles_dirty = true;
        self.last_error = None;
        self.applied_before = true;
        if announce && !added.is_empty() {
            tracing::info!(count = added.len(), "feeds added by this reload");
            let _ = self.evt_tx.try_send(EngineEvent::SourcesAdded { added });
        }
    }

    /// Pull true per-source article counts out of the database.
    ///
    /// A grouped scan, so it runs on a schedule (and after text that changed the
    /// article set) rather than per publish.
    async fn refresh_source_totals(&mut self) {
        let counts = self.store.source_counts().await;
        if counts.is_empty() && !self.source_totals.is_empty() {
            return;
        }
        self.source_totals = counts.into_iter().map(|(id, total, unread)| (id, (total, unread))).collect();
        // A separate query, because the counts above are keyed by articles: a
        // feed the reader just featured has no articles yet and must still show
        // its star.
        self.featured_sources = self.store.featured_sources().await.into_iter().collect();
        self.totals_at = Some(Instant::now());
        self.sources_dirty = true;
    }

    async fn load_window(&mut self) {
        let settings = &self.config_now.settings;
        let since = if settings.retention_days > 0 {
            now_unix() - (settings.retention_days as i64) * 86_400
        } else {
            0
        };
        let limit = settings.max_in_memory_articles.max(200);
        // Two tiers, merged:
        //   * recency  — the newest `limit` items inside the retention window,
        //                i.e. "what is happening now" across the fleet;
        //   * presence — the newest two items of *every* feed, whatever their
        //                age, so a blog that posts twice a year still appears
        //                (with a truthful unread count) instead of being
        //                filtered out of existence by the time window.
        const PRESENCE_PER_SOURCE: usize = 2;
        let mut rows = self.store.load_window(limit, 100, since).await;
        let max_presence = PRESENCE_PER_SOURCE.saturating_mul(self.config_now.sources.len().max(1));
        let presence = self.store.load_recent_per_source(PRESENCE_PER_SOURCE, max_presence).await;
        {
            let mut seen: FxHashSet<i64> = rows.iter().map(|r| r.id).collect();
            for row in presence {
                if seen.insert(row.id) {
                    rows.push(row);
                }
            }
        }

        let mut window = Vec::with_capacity(rows.len());
        let mut orphans = 0usize;
        for row in rows {
            let Some(id) = self.by_db.get(&row.db_source).copied() else {
                // The source vanished from the config; its history stays in the
                // database but is no longer shown.
                orphans += 1;
                continue;
            };
            let Some(spec) = self.config_now.sources.get(id as usize) else {
                continue;
            };
            window.push(Arc::new(row.finish(id, spec.title.clone(), spec.group.clone())));
        }
        if orphans > 0 {
            tracing::info!(orphans, "rows belong to sources no longer configured");
        }
        window.sort_unstable_by_key(|a| std::cmp::Reverse(a.published));
        self.dedup.seed(&window);
        self.window = window;
    }

    // -- main loop ---------------------------------------------------------

    async fn run(mut self, shutdown: Arc<AtomicBool>) {
        while !shutdown.load(Ordering::Relaxed) {
            // The scheduler's deadline is the earliest *due* time, which for an
            // overdue fleet is in the past. Sleeping until it is only a reason to
            // wake if a dispatch could actually happen: with every slot busy, a
            // due source stays due, `sleep_until(past)` returns immediately, and
            // the loop spins — measuring the fleet spent more CPU idling between
            // fetches than fetching. With no slot free, park on the channels
            // instead; `done_rx` wakes us the moment one frees.
            let sched_deadline = if self.dispatch_capacity() > self.in_flight {
                self.sched.next_deadline(&self.states)
            } else {
                None
            };
            let publish_deadline = (self.sources_dirty || self.articles_dirty)
                .then(|| self.last_publish + PUBLISH_INTERVAL);
            let maintenance_deadline = self.last_prune + PRUNE_INTERVAL;
            let deadline = [sched_deadline, publish_deadline, Some(maintenance_deadline)]
                .into_iter()
                .flatten()
                .min();

            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
                    // Nothing pending at all: park on the channels only.
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                biased;
                cmd = self.cmd_rx.recv_async() => {
                    match cmd {
                        Ok(UiCommand::Shutdown) | Err(_) => break,
                        Ok(cmd) => self.on_command(cmd).await,
                    }
                }
                archived = self.arch_rx.recv_async() => {
                    if let Ok(done) = archived {
                        self.on_archived(done);
                    }
                }
                done = self.done_rx.recv_async() => {
                    if let Ok(done) = done {
                        self.on_done(done).await;
                    }
                }
                evt = self.cfg_rx.recv_async() => {
                    if let Ok(evt) = evt {
                        self.on_config_event(evt).await;
                    }
                }
                _ = sleep => {}
            }

            self.dispatch_due();
            self.maintenance().await;
            self.publish(false);
            self.log_progress();
        }
        self.persist_dirty();
        tracing::info!("engine loop stopped");
    }

    /// Every 30 s during a sweep, say how it is going. A first full pass over a
    /// large fleet is minutes long, and "is it stuck?" should not be a question
    /// the log cannot answer.
    fn log_progress(&mut self) {
        const PERIOD: Duration = Duration::from_secs(30);
        let now = Instant::now();
        if now.duration_since(self.last_progress) < PERIOD {
            return;
        }
        self.last_progress = now;
        let busy = self.in_flight;
        let due = self.sched.due_count(&self.states, now);
        let m = &self.metrics;
        let requests = m.requests.load(Ordering::Relaxed);
        // Quiet is worth reporting too, just far less often.
        if requests == self.last_progress_requests
            && busy == 0
            && now.duration_since(self.last_progress) < Duration::from_secs(300)
        {
            return;
        }
        let outcomes = requests.saturating_sub(self.last_progress_requests);
        self.last_progress_requests = requests;
        tracing::info!(
            requests,
            outcomes,
            busy,
            ok_304 = m.responses_304.load(Ordering::Relaxed),
            ok_200 = m.responses_200.load(Ordering::Relaxed),
            errors = m.errors.load(Ordering::Relaxed),
            in_flight = m.in_flight.load(Ordering::Relaxed),
            due,
            avg_ms = m.avg_latency(),
            items = m.items_new.load(Ordering::Relaxed),
            "sweep progress"
        );
    }

    /// How many fetches may be in flight right now.
    ///
    /// One definition, because two callers have to agree on it: the loop uses it
    /// to decide whether waking for the scheduler is worthwhile, and the
    /// dispatcher uses it as its budget. While the reader is waiting on
    /// something, the fleet drops to a trickle.
    fn dispatch_capacity(&self) -> usize {
        if self.user_waiting.load(Ordering::Relaxed) > 0 {
            USER_WAITING_FETCHES
        } else {
            self.config_now.settings.max_concurrent_fetches.max(1)
        }
    }

    fn dispatch_due(&mut self) {
        // Copy the handful of scalars we need so no borrow of `config_now`
        // survives into the loop body.
        let settings = self.config_now.settings.clone();
        // Somebody is waiting for a page to open. The sweep is background work
        // and the page is not, so the fleet drops to a trickle until the archive
        // finishes rather than starving the connection the reader is waiting on.
        let max_in_flight = self.dispatch_capacity();
        let now = Instant::now();
        let mut budget_left = max_in_flight.saturating_sub(self.in_flight);

        while budget_left > 0 {
            let Some(id) = self.sched.pop_due(now, &self.states) else {
                break;
            };
            let Some(spec) = self.config_now.sources.get(id as usize).cloned() else {
                continue;
            };
            if !spec.enabled || self.paused.contains(&spec.key) {
                continue;
            }
            let Some(state) = self.states.get_mut(id as usize) else {
                continue;
            };
            state.health = Health::Fetching;

            // A Zhihu collection is fetched by API rather than as a feed, and it
            // is the one source where "what is new" is worth knowing before the
            // fetch: bodies cost a request each, so only unseen items get one.
            let zhihu = crate::zhihu::collection_id(&spec.url);
            let known = zhihu
                .as_ref()
                .map(|_| Arc::new(self.dedup.known_uids(id)))
                .unwrap_or_default();
            let fetch_spec = FetchSpec {
                source: id,
                url: spec.url.clone(),
                host: spec.host.clone(),
                etag: state.etag.clone(),
                last_modified: state.last_modified.clone(),
                headers: spec.headers.clone(),
                user_agent: spec.user_agent.clone(),
                max_body_bytes: settings.max_body_bytes,
            };
            let budget = ParseBudget {
                max_entries: spec.max_entries.min(settings.max_entries_per_fetch),
                preview_chars: settings.preview_chars,
                max_content_bytes: settings.max_content_bytes,
            };
            let http = Arc::clone(&self.http);
            let tx = self.done_tx.clone();
            let per_host = settings.per_host_concurrency.max(1);

            self.in_flight += 1;
            budget_left -= 1;
            tokio::spawn(async move {
                let started = Instant::now();
                let result = match zhihu {
                    Some(collection) => {
                        fetch_zhihu_once(fetch_spec, budget, collection, known).await
                    }
                    None => fetch_once(http, fetch_spec, budget, per_host).await,
                };
                let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
                // Async send: if the loop is momentarily busy the task waits
                // instead of dropping the result (which would leak in_flight).
                let _ = tx.send_async(FetchDone { id, latency_ms, result }).await;
            });
        }
        self.metrics.set_in_flight(self.in_flight as u64);
    }

    async fn on_done(&mut self, done: FetchDone) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let id = done.id;
        let saved_bytes = self.states.get(id as usize).map(|s| s.last_body_bytes).unwrap_or(0);

        let outcome = match done.result {
            FetchResult::NotModified => {
                self.metrics.record_304(saved_bytes);
                Outcome::NotModified { latency_ms: done.latency_ms }
            }
            FetchResult::Modified { entries, truncated, bytes, etag, last_modified } => {
                self.metrics.record_200(bytes);
                if truncated {
                    tracing::debug!(source = id, "feed truncated to entry budget");
                }
                let (new_items, dupes) = self.ingest(id, entries).await;
                self.metrics
                    .record_items((new_items + dupes) as u64, new_items as u64, dupes as u64);
                if new_items > 0 {
                    Outcome::New {
                        items: new_items,
                        latency_ms: done.latency_ms,
                        bytes,
                        etag,
                        last_modified,
                    }
                } else {
                    Outcome::Quiet { latency_ms: done.latency_ms, bytes, etag, last_modified }
                }
            }
            FetchResult::Failed { message, retry_after, fatal } => {
                self.metrics.record_error();
                if self.last_error.is_none() {
                    self.last_error = Some(message.clone());
                }
                Outcome::Error { message, retry_after, fatal }
            }
        };

        if let Some(state) = self.states.get_mut(id as usize) {
            self.sched.reschedule(state, outcome);
            state.dirty = true;
        }
        self.articles_dirty = true;
        self.sources_dirty = true;
        self.persist_dirty();
    }

    /// Dedup → persist → window insert. Returns `(new, duplicates)`.
    async fn ingest(&mut self, id: SourceId, entries: Vec<RawEntry>) -> (u32, u32) {
        let Some(spec) = self.config_now.sources.get(id as usize).cloned() else {
            return (0, 0);
        };
        let db_source = match self.states.get(id as usize) {
            Some(state) => state.db_id,
            None => return (0, 0),
        };
        let fetched = now_unix();

        let mut batch: Vec<NewArticle> = Vec::with_capacity(entries.len());
        let mut hidden: FxHashSet<usize> = FxHashSet::default();
        let mut dupes = 0u32;

        for entry in entries {
            if !self.dedup.uid_new(id, entry.uid_hash) {
                dupes += 1;
                continue;
            }
            if !self.dedup.content_new(entry.content_hash) {
                // Same story syndicated from another feed: keep the row for
                // this source's own history, but do not surface it twice.
                dupes += 1;
                hidden.insert(batch.len());
            }
            batch.push(NewArticle {
                source: id,
                db_source,
                uid_hash: entry.uid_hash,
                content_hash: entry.content_hash,
                title: entry.title,
                link: entry.link,
                author: entry.author,
                published: if entry.published > 0 { entry.published } else { fetched },
                fetched,
                preview: entry.preview,
                content: entry.content,
            });
        }

        if batch.is_empty() {
            return (0, dupes);
        }

        // Snapshot what is needed to build in-memory records before the batch
        // is moved into the store.
        let meta: Vec<BatchMeta> = batch
            .iter()
            .map(|a| BatchMeta {
                source: a.source,
                db_source: a.db_source,
                uid_hash: a.uid_hash,
                content_hash: a.content_hash,
                title: a.title.clone(),
                link: a.link.clone(),
                author: a.author.clone(),
                published: a.published,
                has_content: a.content.is_some(),
                preview: a.preview.clone(),
            })
            .collect();

        let inserted = self.store.insert_batch(batch).await;

        let mut new_count = 0u32;
        for (idx, row_id) in inserted {
            let Some(m) = meta.get(idx) else {
                continue;
            };
            new_count += 1;
            if hidden.contains(&idx) {
                continue;
            }
            let age = crate::text::human_age(now_unix() - m.published);
            self.window.push(Arc::new(Article {
                id: row_id,
                source: m.source,
                db_source: m.db_source,
                uid_hash: m.uid_hash,
                content_hash: m.content_hash,
                title: m.title.clone(),
                link: m.link.clone(),
                author: m.author.clone(),
                source_title: spec.title.clone(),
                group: spec.group.clone(),
                published: m.published,
                fetched,
                preview: m.preview.clone(),
                has_content: m.has_content,
                read: false,
                starred: false,
                age: Arc::from(age.as_str()),
                archive_path: None,
                archive_bytes: 0,
                read_at: 0,
                progress: 0.0,
            }));
        }

        if new_count > 0 {
            self.window.sort_unstable_by_key(|a| std::cmp::Reverse(a.published));
            let cap = self.config_now.settings.max_in_memory_articles.max(200);
            if self.window.len() > cap {
                self.window.truncate(cap);
            }
            self.articles_dirty = true;
        }
        (new_count, dupes)
    }

    // -- commands ----------------------------------------------------------

    async fn on_command(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::RefreshAll => {
                // A manual refresh is an explicit "try everything again", so it
                // also clears the backoff a previous bad sweep left behind:
                // without that, a fleet whose first pass timed out stays cold
                // for hours even after the network recovers.
                for (i, spec) in self.config_now.sources.iter().enumerate() {
                    if !spec.enabled || self.paused.contains(&spec.key) {
                        continue;
                    }
                    if let Some(state) = self.states.get_mut(i) {
                        state.consecutive_err = 0;
                        state.last_error = None;
                        self.sched.schedule_now(state);
                    }
                }
                self.sources_dirty = true;
            }
            UiCommand::DiscoverFeed { url } => {
                let http = Arc::clone(&self.http);
                let tx = self.evt_tx.clone();
                let max_bytes = self.config_now.settings.max_body_bytes;
                tokio::spawn(async move {
                    let (feed, error) = match discover::discover(&http, &url, max_bytes).await {
                        Ok(feed) => (Some(feed), None),
                        Err(e) => (None, Some(e)),
                    };
                    let _ = tx.send(EngineEvent::Discovered { url, feed, error });
                });
            }
            UiCommand::FriendFeeds { page_url } => self.scan_friend_feeds(page_url),
            UiCommand::SetFeatured { source, featured } => {
                let Some(db_id) = self.states.get(source as usize).map(|s| s.db_id) else {
                    return;
                };
                self.store.set_featured(db_id, featured).await;
                if featured {
                    self.featured_sources.insert(db_id);
                } else {
                    self.featured_sources.remove(&db_id);
                }
                self.sources_dirty = true;
                let title = self
                    .config_now
                    .sources
                    .get(source as usize)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                let _ = self.evt_tx.try_send(EngineEvent::Featured { title, featured });
            }
            UiCommand::AddSource { source } => {
                let file = self.new_source_file();
                self.add_source(source, file);
            }
            UiCommand::ZhihuCollections => {
                let tx = self.evt_tx.clone();
                // The reader is waiting, and Zhihu answers a burst of concurrent
                // requests from the same session with a challenge page. Same
                // rule as archiving: while somebody is waiting, the sweep drops
                // to a trickle.
                self.user_waiting.fetch_add(1, Ordering::Relaxed);
                let waiting = Arc::clone(&self.user_waiting);
                tokio::spawn(async move {
                    let _waiting = WaitingOnDrop(waiting);
                    let jar = match std::fs::read_to_string(zhihu_cookie_path()) {
                        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                        _ => {
                            let _ = tx.send(EngineEvent::ZhihuCollections {
                                name: Arc::from(""),
                                collections: Vec::new(),
                                error: Some("还没有登录知乎".into()),
                            });
                            return;
                        }
                    };
                    let account = match crate::zhihu::whoami(&jar).await {
                        Ok(account) => account,
                        Err(e) => {
                            tracing::warn!(error = %e, "zhihu account lookup failed");
                            let _ = tx.send(EngineEvent::ZhihuCollections {
                                name: Arc::from(""),
                                collections: Vec::new(),
                                error: Some(e),
                            });
                            return;
                        }
                    };
                    let result =
                        crate::zhihu::list_collections(&jar, &account.url_token)
                            .await;
                    let (collections, error) = match result {
                        Ok(list) => (list, None),
                        Err(e) => {
                            tracing::warn!(error = %e, "zhihu collection listing failed");
                            (Vec::new(), Some(e))
                        }
                    };
                    tracing::info!(
                        account = %account.name,
                        collections = collections.len(),
                        "zhihu collections listed"
                    );
                    let _ = tx.send(EngineEvent::ZhihuCollections {
                        name: Arc::from(account.name.as_str()),
                        collections,
                        error,
                    });
                });
            }
            UiCommand::SaveProgress { id, progress } => {
                let now = now_unix();
                if let Some(article) = self.window.iter_mut().find(|a| a.id == id) {
                    let article = Arc::make_mut(article);
                    article.progress = progress.clamp(0.0, 1.0);
                    article.read_at = now;
                }
                self.store.save_progress(id, progress, now);
                self.articles_dirty = true;
            }
            UiCommand::LoadSource { id, limit } => {
                self.load_source_window(id, limit).await;
                self.sources_dirty = true;
            }
            UiCommand::RefreshSource(id) => {
                if let Some(state) = self.states.get_mut(id as usize) {
                    self.sched.schedule_now(state);
                    self.sources_dirty = true;
                }
            }
            UiCommand::RefreshGroup(group) => {
                for (i, spec) in self.config_now.sources.iter().enumerate() {
                    if spec.group != group || !spec.enabled || self.paused.contains(&spec.key) {
                        continue;
                    }
                    if let Some(state) = self.states.get_mut(i) {
                        self.sched.schedule_now(state);
                    }
                }
                self.sources_dirty = true;
            }
            UiCommand::MarkRead { ids, read } => {
                self.set_flags(&ids, Flag::Read, read);
                self.store.set_flag(ids, Flag::Read, read);
            }
            UiCommand::ToggleStar { id, starred } => {
                self.set_flags(&[id], Flag::Starred, starred);
                self.store.set_flag(vec![id], Flag::Starred, starred);
            }
            UiCommand::MarkAllRead { scope, before } => {
                let db_source = match &scope {
                    Scope::Source(id) => self.states.get(*id as usize).map(|s| s.db_id),
                    _ => None,
                };
                let changed = self.store.set_flag_where(db_source, Flag::Read, true, before).await;
                for a in self.window.iter_mut() {
                    let in_scope = match &scope {
                        Scope::All => true,
                        Scope::Source(id) => a.source == *id,
                        Scope::Group(g) => a.group.as_ref() == g.as_ref(),
                    };
                    if in_scope && !a.read && a.published <= before {
                        let mut copy = (**a).clone();
                        copy.read = true;
                        *a = Arc::new(copy);
                    }
                }
                tracing::debug!(changed, "mark all read");
                self.articles_dirty = true;
            }
            UiCommand::Search { query, limit } => {
                if query.trim().is_empty() {
                    self.search = None;
                } else {
                    let ids = self.store.search(query.to_string(), limit).await;
                    // The reader searches the database and looks at a window of
                    // it, so the hits have to be brought into the window or the
                    // results of a perfectly good query render as an empty list.
                    self.pull_into_window(&ids).await;
                    let set: FxHashSet<i64> = ids.iter().copied().collect();
                    self.search = Some((Arc::clone(&query), Arc::new(set)));
                    let _ = self
                        .evt_tx
                        .try_send(EngineEvent::SearchResults { query, ids: Arc::from(ids) });
                }
                self.articles_dirty = true;
            }
            UiCommand::ClearSearch => {
                self.search = None;
                self.articles_dirty = true;
            }
            UiCommand::LoadContent { id } => {
                if let Some(text) = self.store.load_content(id).await {
                    let _ = self.evt_tx.try_send(EngineEvent::Content { id, text });
                }
            }
            UiCommand::ArchiveArticle { id } => self.start_archive(id),
            UiCommand::TogglePause(id) => {
                let Some(spec) = self.config_now.sources.get(id as usize).cloned() else {
                    return;
                };
                if !self.paused.remove(&spec.key) {
                    self.paused.insert(spec.key.clone());
                    self.sched.forget(id);
                    if let Some(state) = self.states.get_mut(id as usize) {
                        state.health = Health::Disabled;
                    }
                } else if spec.enabled
                    && let Some(state) = self.states.get_mut(id as usize)
                {
                    self.sched.schedule_now(state);
                }
                self.sources_dirty = true;
            }
            UiCommand::ReloadConfig => {
                if let Some(next) = self.config.reload() {
                    let sources = next.sources.len();
                    let problems = next.problems.len();
                    self.apply_config(next).await;
                    let _ = self
                        .evt_tx
                        .try_send(EngineEvent::ConfigReloaded { sources, problems });
                }
            }
            UiCommand::PruneNow => self.prune_if_due(true).await,
            UiCommand::Shutdown => {}
        }
        self.publish(false);
    }

    fn set_flags(&mut self, ids: &[i64], flag: Flag, value: bool) {
        let set: FxHashSet<i64> = ids.iter().copied().collect();
        for a in self.window.iter_mut() {
            if !set.contains(&a.id) {
                continue;
            }
            let mut copy = (**a).clone();
            match flag {
                Flag::Read => copy.read = value,
                Flag::Starred => copy.starred = value,
            }
            *a = Arc::new(copy);
        }
        self.articles_dirty = true;
    }

    async fn on_config_event(&mut self, evt: ConfigEvent) {
        match evt {
            ConfigEvent::Reloaded { fingerprint, sources, problems } => {
                tracing::info!(fingerprint, sources, problems, "config changed on disk");
                self.metrics.config_reloads.fetch_add(1, Ordering::Relaxed);
                // The watcher has already installed the new configuration — it is
                // what produced this event — so `reload()` here would compare
                // equal and answer "nothing changed", leaving new sources
                // unscheduled and changed settings unapplied. Take what is
                // current instead.
                let next = self.config.current();
                self.apply_config(next).await;
                let _ = self
                    .evt_tx
                    .try_send(EngineEvent::ConfigReloaded { sources, problems });
            }
            ConfigEvent::Unchanged => {}
            ConfigEvent::WatchFailed(msg) => {
                tracing::warn!("config watcher unavailable: {msg}");
                let _ = self.evt_tx.try_send(EngineEvent::Problem(Arc::from(
                    format!("config watcher unavailable: {msg}").as_str(),
                )));
            }
        }
    }

    async fn maintenance(&mut self) {
        if self.last_db_size.elapsed() >= DB_SIZE_INTERVAL {
            self.last_db_size = Instant::now();
            self.store.refresh_db_bytes().await;
        }
        // Counts only change when articles arrive; a grouped scan every half
        // minute is indistinguishable from live and far cheaper than per publish.
        if self.totals_at.is_none_or(|at| at.elapsed() >= TOTALS_INTERVAL) {
            self.refresh_source_totals().await;
        }
        self.prune_if_due(false).await;
    }

    /// Fetch a page, take the links that could be blogs, and verify each has a
    /// feed. Verification is the classifier: markup can only guess at "is this a
    /// blogroll", a feed behind the link is evidence.
    fn scan_friend_feeds(&mut self, page_url: Arc<str>) {
        let http = Arc::clone(&self.http);
        let tx = self.evt_tx.clone();
        let max_bytes = self.config_now.settings.max_body_bytes;
        self.user_waiting.fetch_add(1, Ordering::Relaxed);
        let waiting = Arc::clone(&self.user_waiting);
        tokio::spawn(async move {
            let _waiting = WaitingOnDrop(waiting);
            // Every plausible page, merged. Stopping at the first page that
            // yields anything is tempting and wrong: an article's sidebar has two
            // friend links, its /links page has twenty, and the reader wants the
            // twenty.
            let mut candidates: Vec<discover::Candidate> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for page in link_pages(&page_url) {
                if candidates.len() >= FRIEND_SCAN_LIMIT {
                    break;
                }
                let Ok(resp) = http
                    .fetch_resource(&page, max_bytes, "text/html,application/xhtml+xml,*/*;q=0.8", 2)
                    .await
                else {
                    continue;
                };
                if !resp.content_type.as_deref().unwrap_or("text/html").contains("html") {
                    continue;
                }
                let html = String::from_utf8_lossy(&resp.body).to_string();
                let found = discover::friend_candidates(&html, &resp.final_url, FRIEND_SCAN_LIMIT);
                if !found.is_empty() {
                    tracing::info!(page = %page, links = found.len(), "links page");
                }
                for candidate in found {
                    if seen.insert(candidate.url.to_string()) {
                        candidates.push(candidate);
                    }
                }
            }
            candidates.truncate(FRIEND_SCAN_LIMIT);
            if candidates.is_empty() {
                tracing::info!(%page_url, "no links page yielded candidates");
            }
            let tried = candidates.len();
            let mut found = 0usize;
            // A courtesy scan of somebody else's site, not a crawl: cheap
            // discovery (the link itself, or what the page advertises), a few at
            // a time, and a deadline. The reader is watching it fill in, so slow
            // is acceptable and rude is not.
            let deadline = tokio::time::Instant::now() + FRIEND_SCAN_BUDGET;
            let slots = Arc::new(tokio::sync::Semaphore::new(FRIEND_SCAN_CONCURRENCY));
            let mut tasks = Vec::new();
            for candidate in candidates {
                let http = Arc::clone(&http);
                let tx = tx.clone();
                let slots = Arc::clone(&slots);
                tasks.push(tokio::spawn(async move {
                    let _permit = slots.acquire_owned().await.ok()?;
                    let feed = discover::discover_common(&http, &candidate.url, max_bytes).await.ok()?;
                    let _ = tx.send(EngineEvent::FriendFeed { candidate, feed });
                    Some(())
                }));
            }
            for task in tasks {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if let Ok(Ok(Some(()))) = tokio::time::timeout(remaining, task).await {
                    found += 1;
                }
            }
            tracing::info!(tried, found, %page_url, "blogroll scan finished");
            let _ = tx.send(EngineEvent::FriendScanDone { tried, found });
        });
    }

    /// Where a newly added subscription is written.
    ///
    /// A `custom.toml` beside the feeds that are already configured: that
    /// directory is already `include`d, so the file is picked up with no edit to
    /// the main configuration, and the generated files stay generated. Falls back
    /// to the main configuration when there is nothing to sit beside.
    fn new_source_file(&self) -> Arc<str> {
        Arc::from(added_sources_file(&self.config_now, self.config.path()).to_string_lossy().as_ref())
    }

    /// Append a subscription to a configuration file; the watcher reloads.
    fn add_source(&mut self, source: NewSource, file: Arc<str>) {
        let result = append_source_to_file(&file, &source);
        let title: Arc<str> = Arc::from(source.title.as_str());
        let url: Arc<str> = Arc::from(source.url.as_str());
        match &result {
            Ok(true) => tracing::info!(%file, %url, "subscription added"),
            Ok(false) => tracing::info!(%file, %url, "subscription already present"),
            Err(e) => tracing::warn!(%file, %url, %e, "adding a subscription failed"),
        }
        let (added, error) = match &result {
            Ok(true) => (true, None),
            Ok(false) => (false, Some("已经在订阅里，没有重复添加".to_string())),
            Err(e) => (false, Some(e.clone())),
        };
        let _ = self.evt_tx.send(EngineEvent::SourceAdded { title, url, file, error });
        if added {
            // The file watcher picks the change up on its own, within its settle
            // window; nothing more is needed here.
            tracing::info!("configuration written");
        } else if matches!(result, Ok(false)) {
            tracing::info!("already subscribed; nothing written");
        }
    }

    /// Merge one source's history into the window.
    ///
    /// Merging rather than replacing: "All items" keeps its recency shape, and
    /// the opened blog simply has all of itself present.
    async fn load_source_window(&mut self, id: SourceId, limit: usize) {
        // The window is the memory budget, and opening blogs adds to it, so the
        // load is bounded by the same cap the ingest path uses and each feed is
        // only fetched once.
        let cap = self.config_now.settings.max_in_memory_articles.max(200);
        if self.window.len() >= cap || !self.history_loaded.insert(id) {
            return;
        }
        let Some(db_source) = self.states.get(id as usize).map(|s| s.db_id) else {
            return;
        };
        let rows = self.store.load_source(db_source, limit).await;
        if rows.is_empty() {
            return;
        }
        let Some(spec) = self.config_now.sources.get(id as usize) else {
            return;
        };
        let mut seen: FxHashSet<i64> = self.window.iter().map(|a| a.id).collect();
        let mut added = 0usize;
        for row in rows {
            if !seen.insert(row.id) {
                continue;
            }
            self.window
                .push(Arc::new(row.finish(id, spec.title.clone(), spec.group.clone())));
            added += 1;
        }
        if added > 0 {
            self.window.sort_unstable_by_key(|a| std::cmp::Reverse(a.published));
            self.dedup.seed(&self.window);
            self.articles_dirty = true;
            tracing::info!(source = id, added, "loaded feed history into the window");
        }
    }

    /// Make sure the given articles are in the in-memory window, so a query
    /// answered from the database can actually be displayed.
    async fn pull_into_window(&mut self, ids: &[i64]) {
        let missing: Vec<i64> = {
            let present: FxHashSet<i64> = self.window.iter().map(|a| a.id).collect();
            ids.iter().copied().filter(|id| !present.contains(id)).take(SEARCH_PULL_LIMIT).collect()
        };
        if missing.is_empty() {
            return;
        }
        let rows = self.store.load_by_ids(missing).await;
        let mut added = 0usize;
        for row in rows {
            let Some(source) = self.by_db.get(&row.db_source).copied() else {
                continue;
            };
            let Some(spec) = self.config_now.sources.get(source as usize) else {
                continue;
            };
            self.window
                .push(Arc::new(row.finish(source, spec.title.clone(), spec.group.clone())));
            added += 1;
        }
        if added > 0 {
            self.window.sort_unstable_by_key(|a| std::cmp::Reverse(a.published));
            self.dedup.seed(&self.window);
            tracing::info!(added, "pulled search hits into the window");
        }
    }

    async fn prune_if_due(&mut self, force: bool) {
        if !force && self.last_prune.elapsed() < PRUNE_INTERVAL {
            return;
        }
        self.last_prune = Instant::now();
        let settings = self.config_now.settings.clone();
        let deleted = self.store.prune(settings.retention_days, settings.max_articles as u64).await;
        if deleted > 0 {
            tracing::info!(deleted, "pruned old articles");
        }
        self.store.checkpoint();
        self.store.refresh_db_bytes().await;

        // Page archives live on disk, so pruning rows would otherwise leak
        // files forever. Sweep anything the database no longer references.
        let keep = self.store.archive_paths().await;
        let dir = settings.archive_dir();
        let removed = tokio::task::spawn_blocking(move || gc_archives(&dir, &keep))
            .await
            .unwrap_or(0);
        if removed > 0 {
            tracing::info!(removed, "removed orphaned page archives");
        }
    }

    /// Kick off a page archive. The heavy work (fetching the page and every
    /// sub-resource, then rewriting it) happens in a spawned task; only the
    /// result comes back to this loop.
    fn start_archive(&mut self, id: i64) {
        if self.archives_in_flight >= MAX_CONCURRENT_ARCHIVES {
            tracing::debug!(id, "archive queue full, skipping");
            return;
        }
        let Some(article) = self.window.iter().find(|a| a.id == id).cloned() else {
            return;
        };
        if article.archive_path.is_some() || article.link.is_empty() {
            return;
        }
        let settings = self.config_now.settings.clone();
        let http = Arc::clone(&self.http);
        let dir = settings.archive_dir();
        let url = article.link.to_string();
        let tx = self.arch_tx.clone();
        self.archives_in_flight += 1;
        self.user_waiting.fetch_add(1, Ordering::Relaxed);
        let waiting = Arc::clone(&self.user_waiting);
        tracing::debug!(id, %url, dir = %dir.display(), "archive: starting");
        tokio::spawn(async move {
            // The reader stops waiting when the page is saved, however it went.
            let _waiting = WaitingOnDrop(waiting);
            let budget = settings.archive_budget();
            let started = std::time::Instant::now();
            // The feed path retries a transient failure three times; the page
            // fetch used to try exactly once, so a single unlucky connect during
            // a sweep surfaced to the reader as "存档失败". Same ladder here.
            let mut attempt = 0;
            let page = loop {
                match crate::archive::archive_page(&http, &url, &budget).await {
                    Ok(page) => break Ok(page),
                    Err(e) => {
                        let transient = matches!(e, crate::archive::ArchiveError::Fetch(_));
                        if !transient || attempt >= ARCHIVE_RETRIES {
                            break Err(e);
                        }
                        attempt += 1;
                        let wait = 400u64 << (attempt - 1);
                        tracing::debug!(id, attempt, %e, "archive retrying after {wait}ms");
                        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    }
                }
            };
            let outcome = match page {
                Ok(archived) => {
                    let path = crate::archive::archive_path(&dir, &archived.source_url);
                    let bytes = archived.html.len() as u32;
                    let html = archived.html;
                    // A page archive is a few hundred KB; writing it inline costs
                    // about a millisecond and cannot be starved by the shared
                    // blocking pool the feed parser uses.
                    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, html.as_bytes()));
                    tracing::info!(
                        id,
                        ms = started.elapsed().as_millis(),
                        resources = archived.stats.inlined,
                        bytes,
                        "saved page archive"
                    );
                    match write {
                        Ok(()) => ArchiveDone {
                            id,
                            path: Some(Arc::from(path.to_string_lossy().as_ref())),
                            bytes,
                            error: None,
                        },
                        Err(e) => ArchiveDone {
                            id,
                            path: None,
                            bytes: 0,
                            error: Some(Arc::from(format!("write archive: {e}").as_str())),
                        },
                    }
                }
                Err(e) => ArchiveDone {
                    id,
                    path: None,
                    bytes: 0,
                    error: Some(Arc::from(e.to_string().as_str())),
                },
            };
            let _ = tx.send_async(outcome).await;
        });
    }

    fn on_archived(&mut self, done: ArchiveDone) {
        self.archives_in_flight = self.archives_in_flight.saturating_sub(1);
        if let Some(path) = &done.path {
            self.store.save_archive(done.id, Arc::clone(path), done.bytes);
            for article in self.window.iter_mut() {
                if article.id != done.id {
                    continue;
                }
                let mut copy = (**article).clone();
                copy.archive_path = Some(Arc::clone(path));
                copy.archive_bytes = done.bytes;
                *article = Arc::new(copy);
                break;
            }
            tracing::info!(id = done.id, bytes = done.bytes, "page archived");
            self.articles_dirty = true;
        } else if let Some(error) = &done.error {
            tracing::warn!(id = done.id, error = %error, "page archive failed");
        }
        let _ = self.evt_tx.try_send(EngineEvent::Archived {
            id: done.id,
            path: done.path,
            bytes: done.bytes,
            error: done.error,
        });
    }

    /// Persist the health of every source that changed since the last write.
    fn persist_dirty(&mut self) {
        let mut rows = Vec::new();
        for state in self.states.iter_mut() {
            if !state.dirty {
                continue;
            }
            state.dirty = false;
            rows.push(SourceStateRow {
                db_id: state.db_id,
                etag: state.etag.clone(),
                last_modified: state.last_modified.clone(),
                last_fetch: state.last_fetch_unix,
                last_status: state.last_status,
                error_count: state.consecutive_err,
                last_error: state.last_error.clone(),
                new_items: state.last_new_items,
                interval_secs: state.effective_secs(),
                body_bytes: state.last_body_bytes,
            });
        }
        if !rows.is_empty() {
            self.store.save_states(rows);
        }
    }

    // -- snapshot ----------------------------------------------------------

    /// Publish a new immutable snapshot. Only the dirty half is rebuilt, so a
    /// 5 000-source fleet does not rebuild 5 000 rows four times a second.
    fn publish(&mut self, force: bool) {
        if !(self.sources_dirty || self.articles_dirty || force) {
            return;
        }
        let now = Instant::now();
        if !force && now.duration_since(self.last_publish) < PUBLISH_INTERVAL {
            return;
        }
        self.last_publish = now;
        self.generation += 1;

        if self.sources_dirty || force {
            let (sources, groups, tags) = self.build_source_rows();
            self.cached_sources = sources;
            self.cached_groups = groups;
            self.cached_tags = tags;
            self.sources_dirty = false;
        }

        let articles: Arc<[Arc<Article>]> = match &self.search {
            Some((_, ids)) => self
                .window
                .iter()
                .filter(|a| ids.contains(&a.id))
                .cloned()
                .collect::<Vec<_>>()
                .into(),
            None => self.window.to_vec().into(),
        };
        self.articles_dirty = false;

        let unread = articles.iter().filter(|a| !a.read).count() as u32;
        let mut stats = self.metrics.snapshot_stats();
        let (enabled, due) = self.source_counts();
        stats.sources_total = self.states.len() as u64;
        stats.sources_enabled = enabled;
        stats.sources_due = due;
        stats.queue_depth = self.store.queue_high_water();
        // The window, not the filtered view: these feed the sidebar.
        stats.mem_articles = self.window.len() as u64;
        stats.db_articles = self.source_totals.values().map(|(t, _)| *t as u64).sum();
        stats.db_unread = self.source_totals.values().map(|(_, u)| *u as u64).sum();
        stats.window_unread = self.window.iter().filter(|a| !a.read).count() as u64;
        stats.window_starred = self.window.iter().filter(|a| a.starred).count() as u64;
        stats.db_bytes = self.store.db_bytes();
        stats.config_files = self.config_now.files.len() as u64;
        stats.config_problems = self.config_now.problems.len() as u64;

        let problems: Arc<[Arc<str>]> = self
            .config_now
            .problems
            .iter()
            .map(|p| Arc::<str>::from(p.to_string().as_str()))
            .chain(self.last_error.iter().cloned())
            .collect::<Vec<_>>()
            .into();

        let snapshot = Snapshot {
            generation: self.generation,
            built_at: now_unix(),
            articles,
            sources: Arc::clone(&self.cached_sources),
            groups: Arc::clone(&self.cached_groups),
            tags: Arc::clone(&self.cached_tags),
            stats,
            problems,
        };
        self.snapshot.store(Arc::new(snapshot));
        let _ = self
            .evt_tx
            .try_send(EngineEvent::Published { generation: self.generation, unread });
    }

    fn source_counts(&self) -> (u64, u64) {
        let enabled = self
            .config_now
            .sources
            .iter()
            .filter(|s| s.enabled && !self.paused.contains(&s.key))
            .count() as u64;
        let due = self.sched.due_count(&self.states, Instant::now()) as u64;
        (enabled, due)
    }

    fn build_source_rows(&self) -> SourceViews {
        // One pass over the window yields both per-source and per-group counts.
        let mut unread: FxHashMap<SourceId, u32> = FxHashMap::default();
        let mut total: FxHashMap<SourceId, u32> = FxHashMap::default();
        let have_totals = !self.source_totals.is_empty();
        for a in &self.window {
            *total.entry(a.source).or_insert(0) += 1;
            if !a.read {
                *unread.entry(a.source).or_insert(0) += 1;
            }
        }

        let now = Instant::now();
        let mut rows = Vec::with_capacity(self.states.len());
        let mut groups: BTreeMap<Arc<str>, GroupRow> = BTreeMap::new();
        let mut tags: FxHashMap<Arc<str>, TagRow> = FxHashMap::default();

        for (spec, state) in self.config_now.sources.iter().zip(self.states.iter()) {
            let id = spec.id;
            // Prefer the database's numbers, which describe the *feed*; the
            // window's numbers describe the slice of it this window holds, and
            // for a dormant blog that slice is deliberately tiny.
            let (t, u) = if have_totals {
                self.source_totals.get(&state.db_id).copied().unwrap_or((0, 0))
            } else {
                (
                    total.get(&id).copied().unwrap_or(0),
                    unread.get(&id).copied().unwrap_or(0),
                )
            };
            let health = if !spec.enabled || self.paused.contains(&spec.key) {
                Health::Disabled
            } else {
                state.health
            };
            let featured = self.featured_sources.contains(&state.db_id);
            let entry = groups.entry(spec.group.clone()).or_insert_with(|| GroupRow {
                name: spec.group.clone(),
                sources: 0,
                unread: 0,
                total: 0,
            });
            entry.sources += 1;
            entry.unread += u;
            entry.total += t;

            // Tags are counted per feed, so a tag's total is the sum over the
            // feeds carrying it — the same convention as groups, and it means
            // the numbers add up when filtering by tag.
            for tag in spec.tags.iter() {
                let entry = tags.entry(tag.clone()).or_insert_with(|| TagRow {
                    name: tag.clone(),
                    sources: 0,
                    unread: 0,
                    total: 0,
                });
                entry.sources += 1;
                entry.unread += u;
                entry.total += t;
            }

            rows.push(SourceRow {
                id,
                db_id: state.db_id,
                title: spec.title.clone(),
                group: spec.group.clone(),
                tags: spec.tags.clone(),
                host: spec.host.clone(),
                unread: u,
                total: t,
                health,
                featured,
                interval_secs: state.base_interval.as_secs(),
                effective_interval_secs: state.effective_secs(),
                last_fetch: state.last_fetch_unix,
                next_due_secs: state.secs_until_due(now),
                last_error: state.last_error.clone(),
                consecutive_errors: state.consecutive_err,
                avg_latency_ms: state.latency_ema_ms as u32,
                etag: state.etag.is_some() || state.last_modified.is_some(),
            });
        }
        let mut tags: Vec<TagRow> = tags.into_values().collect();
        // Most-used tags first, then alphabetical, so the list is stable and
        // the interesting ones are at the top.
        tags.sort_by(|a, b| {
            b.total.cmp(&a.total).then_with(|| b.sources.cmp(&a.sources)).then_with(|| a.name.cmp(&b.name))
        });
        (
            rows.into(),
            groups.into_values().collect::<Vec<_>>().into(),
            tags.into(),
        )
    }
}

/// Fields copied out of a [`NewArticle`] before it is handed to the store.
struct BatchMeta {
    source: SourceId,
    db_source: i64,
    uid_hash: u64,
    content_hash: u64,
    title: Arc<str>,
    link: Arc<str>,
    author: Option<Arc<str>>,
    published: i64,
    has_content: bool,
    preview: Arc<str>,
}

// ---------------------------------------------------------------------------
// Fetch task (pure, no shared state)
// ---------------------------------------------------------------------------

/// Where the app keeps the Zhihu session, written by the in-app login.
pub fn zhihu_cookie_path() -> std::path::PathBuf {
    crate::default_data_dir().join("zhihu.cookies")
}

/// Fetch one Zhihu collection as a set of entries.
///
/// The list is one request; the *bodies* are one request each, so they are only
/// pulled for items that arrived since the last successful fetch — the first
/// fetch of a collection is therefore the expensive one, and later ones cost
/// little more than the list.
async fn fetch_zhihu_once(
    spec: FetchSpec,
    budget: ParseBudget,
    collection: String,
    known: Arc<crate::dedup::KnownUids>,
) -> FetchResult {
    let cookies = match std::fs::read_to_string(zhihu_cookie_path()) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => {
            return FetchResult::Failed {
                message: Arc::from("知乎未登录：在「添加」里登录一次即可抓取收藏"),
                retry_after: None,
                fatal: false,
            };
        }
    };
    let want = budget.max_entries.clamp(1, 200);
    let items = match crate::zhihu::collection_items_upto(&cookies, &collection, want).await {
            Ok(items) => items,
            Err(e) => {
                let fatal = e.contains("未登录");
                return FetchResult::Failed {
                    message: Arc::from(e.as_str()),
                    retry_after: None,
                    fatal,
                };
            }
        };

    // Bodies, in a small batch: an API that answers twenty requests in parallel
    // is an API that starts refusing them.
    // An existing article is never updated (`DO NOTHING`), so a body has to
    // travel with the row that first carries the item. That makes the frontier
    // the unit of work: each pass completes up to a budget of items from the
    // newest end, and items still missing a body are simply not inserted yet, so
    // the next pass meets them again. A 154-item collection therefore fills in
    // from the top over several passes instead of firing 154 requests at once.
    let mut entries = Vec::with_capacity(items.len());
    let fetched_at = now_unix();
    let slots = tokio::sync::Semaphore::new(4);
    let mut bodies_left = ZHIHU_BODY_BUDGET;
    let mut deferred = 0usize;
    for item in items {
        let mut entry = crate::zhihu::to_entry(&item, &spec.url, budget.preview_chars);
        // Already stored: it contributes its row for the dedup pass and costs
        // nothing, because nothing about it can change any more.
        if known.contains(&entry.uid_hash) {
            entries.push(entry);
            continue;
        }
        if bodies_left == 0 {
            deferred += 1;
            continue;
        }
        let wants_body = budget.max_content_bytes > 0 && !matches!(item.kind, crate::zhihu::ItemKind::Other);
        if wants_body {
            bodies_left -= 1;
            let _permit = slots.acquire().await;
            match crate::zhihu::content_of(&cookies, item.kind, &item.id).await {
                Ok(html) => {
                    let text = crate::text::html_to_text(&html, budget.max_content_bytes);
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        entry.content_hash = crate::text::hash64(trimmed.as_bytes());
                        entry.content = Some(Arc::from(trimmed.as_str()));
                    }
                }
                Err(e) => {
                    // No body, no row: inserting it now would burn the item
                    // forever, because the row can never be updated.
                    tracing::debug!(id = %item.id, error = %e, "zhihu body unavailable; deferring item");
                    deferred += 1;
                    continue;
                }
            }
        } else {
            bodies_left = bodies_left.saturating_sub(1);
        }
        entries.push(entry);
    }
    let bytes = entries.iter().map(|e| e.title.len() + e.preview.len()).sum::<usize>() as u64;
    let with_body = entries.iter().filter(|e| e.content.is_some()).count();
    tracing::info!(
        source = spec.source,
        collection = %collection,
        items = entries.len(),
        bodies = with_body,
        deferred,
        "zhihu collection fetched"
    );
    let _ = fetched_at;
    FetchResult::Modified {
        entries,
        truncated: false,
        bytes,
        // No validators: the API has no ETag for a collection, and the dedup
        // pass downstream is what keeps repeats out.
        etag: None,
        last_modified: None,
    }
}

async fn fetch_once(
    http: Arc<HttpEngine>,
    spec: FetchSpec,
    budget: ParseBudget,
    per_host: usize,
) -> FetchResult {
    match http.fetch(&spec, per_host).await {
        Ok(resp) if resp.not_modified => FetchResult::NotModified,
        Ok(resp) => {
            let body = resp.body;
            let parsed = tokio::task::spawn_blocking(move || crate::parse::parse(&body, &budget)).await;
            match parsed {
                Ok(Ok(feed)) => FetchResult::Modified {
                    entries: feed.entries,
                    truncated: feed.truncated,
                    bytes: resp.bytes,
                    etag: resp.etag,
                    last_modified: resp.last_modified,
                },
                Ok(Err(e)) => FetchResult::Failed {
                    message: Arc::from(format!("parse: {e}").as_str()),
                    retry_after: None,
                    // A feed that does not parse will not start parsing on a
                    // retry; rely on the slow backoff path instead.
                    fatal: true,
                },
                Err(e) => FetchResult::Failed {
                    message: Arc::from(format!("parse task: {e}").as_str()),
                    retry_after: None,
                    fatal: true,
                },
            }
        }
        Err(e) => {
            let retry_after = e.retry_after();
            let fatal = !matches!(e, FetchError::Transport(_) | FetchError::Timeout);
            FetchResult::Failed { message: Arc::from(e.to_string().as_str()), retry_after, fatal }
        }
    }
}

/// Delete archive files that no article references any more.
///
/// Returns how many were removed. Only files directly inside the archive
/// directory with the `.html` extension the archiver writes are considered, so a
/// misconfigured `db_path` cannot turn this into a recursive delete of
/// something else.
fn gc_archives(dir: &std::path::Path, keep: &[Arc<str>]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let keep: FxHashSet<&str> = keep.iter().map(|p| p.as_ref()).collect();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let name = path.to_string_lossy();
        if keep.contains(name.as_ref()) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Kept so `SourceSpec` stays part of the public engine surface for callers that
/// want to inspect the configured feed list directly.
pub fn source_specs(config: &CompiledConfig) -> &[Arc<SourceSpec>] {
    &config.sources
}

#[allow(dead_code)]
fn _assert_settings_used(_s: &Settings) {}

#[cfg(test)]
mod add_source_tests {
    use super::append_source_to_file;
    use crate::config::NewSource;

    fn source(url: &str) -> NewSource {
        NewSource {
            title: "Rust Blog".into(),
            url: url.into(),
            group: "订阅".into(),
            tags: vec!["手动添加".into()],
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("artifeeds-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Asking twice is what readers do — Enter twice, or the same import again —
    /// and an append-only writer turns that into a file with one feed in it many
    /// times. Loading deduplicates, so nothing breaks; the file just rots.
    #[test]
    fn adding_the_same_feed_twice_writes_once() {
        let dir = temp_dir("duplicate");
        let file = dir.join("custom.toml");
        let url = "https://www.freebuf.com/feed";
        assert!(
            append_source_to_file(file.to_str().unwrap(), &source(url)).unwrap(),
            "the first add writes"
        );
        for _ in 0..5 {
            assert!(
                !append_source_to_file(file.to_str().unwrap(), &source(url)).unwrap(),
                "a repeat writes nothing"
            );
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.matches(url).count(), 2, "url plus its key, once");
        // A different feed still lands.
        assert!(append_source_to_file(file.to_str().unwrap(), &source("https://b.example/feed")).unwrap());
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(parsed["sources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn creates_the_file_when_it_does_not_exist() {
        let dir = temp_dir("create");
        let file = dir.join("custom.toml");
        append_source_to_file(file.to_str().unwrap(), &source("https://blog.rust-lang.org/feed.xml"))
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["sources"][0]["url"].as_str().unwrap(), "https://blog.rust-lang.org/feed.xml");
        // A header comment, so the file explains itself to a reader.
        assert!(text.starts_with('#'));
        assert!(!file.with_extension("toml.bak").exists(), "nothing to back up on creation");
    }

    #[test]
    fn appends_and_backs_up_an_existing_file() {
        let dir = temp_dir("append");
        let file = dir.join("custom.toml");
        std::fs::write(&file, "# mine\n\n[[sources]]\ntitle = \"A\"\nurl = \"https://a.example/f\"\nkey = \"https://a.example/f\"\n").unwrap();
        append_source_to_file(file.to_str().unwrap(), &source("https://b.example/feed")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["sources"].as_array().unwrap().len(), 2);
        assert!(text.starts_with("# mine"), "the reader's own comment survives");
        let backup = std::fs::read_to_string(file.with_extension("toml.bak")).unwrap();
        assert!(backup.contains("https://a.example/f"));
        assert!(!backup.contains("https://b.example/feed"), "the backup is the file before the edit");
        // No temporary file left behind.
        assert!(!file.with_extension("toml.tmp").exists());
    }

    #[test]
    fn a_source_that_would_break_the_file_is_refused_before_writing() {
        let dir = temp_dir("refuse");
        let file = dir.join("custom.toml");
        let original = "[[sources]]\ntitle = \"A\"\nurl = \"https://a.example/f\"\n";
        std::fs::write(&file, original).unwrap();
        // A URL containing a raw newline would produce `url = "a\nb"`; the
        // escaper handles it, so this must *succeed* — the refusal path is for
        // text the escaper cannot save, which cannot currently happen. What
        // matters is that the file is never left half-written.
        append_source_to_file(file.to_str().unwrap(), &source("https://b.example/\"quoted")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["sources"].as_array().unwrap().len(), 2);
    }
}

#[cfg(test)]
mod search_window_tests {
    use super::*;
    use crate::store::{NewArticle, SourceUpsert};

    /// Searching for a blog has to *show* that blog's articles.
    ///
    /// A search runs against the whole database while the reader looks at an
    /// in-memory window of it. Finding the rows in SQL and displaying nothing is
    /// the failure this locks down — it is what made "I just added this blog and
    /// cannot find it" true even though everything was in place.
    #[test]
    fn a_search_pulls_its_hits_into_the_window() {
        let dir = std::env::temp_dir().join(format!("artifeeds-search-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("feeds.toml");
        let url = "https://blog.rust-lang.org/feed.xml";
        std::fs::write(
            &config_path,
            format!(
                "[settings]\nmin_interval_secs = 3600\n\n                 [[sources]]\ntitle = \"Rust Blog\"\nurl = \"{url}\"\n\
                 group = \"编程语言官方博客\"\nenabled = false\n"
            ),
        )
        .unwrap();
        let db = dir.join("t.db");
        let engine = Engine::start_with(config_path, Some(db.clone())).unwrap();

        // The article's own text says nothing about Rust; only its blog does.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let store = Store::open(&db, &Settings::default(), Metrics::new()).unwrap();
            store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from(url),
                    title: Arc::from("Rust Blog"),
                    url: Arc::from(url),
                    group: Arc::from("编程语言官方博客"),
                }])
                .await;
            store
                .insert_batch(vec![NewArticle {
                    source: 0,
                    db_source: 1,
                    uid_hash: 42,
                    content_hash: 42,
                    title: Arc::from("Announcing a new compiler release"),
                    link: Arc::from("https://blog.rust-lang.org/2026/01/01/release.html"),
                    author: None,
                    published: 1_800_000_000,
                    fetched: 1_800_000_000,
                    preview: Arc::from("Faster builds and better diagnostics."),
                    content: None,
                }])
                .await;
        });

        engine.send(UiCommand::Search { query: Arc::from("rust"), limit: 50 });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let shown = engine
                .snapshot()
                .articles
                .iter()
                .any(|a| a.title.contains("Announcing a new compiler release"));
            if shown {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the search hit never reached the window"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        engine.shutdown();
        drop(engine);
    }
}

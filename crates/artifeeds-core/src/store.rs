//! SQLite persistence.
//!
//! ## Shape
//!
//! Exactly **one** thread owns the write connection and every command goes
//! through a bounded channel to it. That is deliberate:
//!
//! * SQLite serialises writers anyway, so a pool would only add lock
//!   contention and `SQLITE_BUSY` retries.
//! * One writer means one prepared-statement cache, one page cache, one
//!   transaction at a time — the cheapest possible configuration.
//! * Callers never block a runtime thread: all public methods are `async` and
//!   await a reply over a `flume` channel.
//!
//! ## Cost controls
//!
//! * **Batched transactions.** One fetch produces one transaction, not one
//!   insert per item. With WAL + `synchronous=NORMAL` that turns N fsyncs into
//!   a single small commit.
//! * **`INSERT … ON CONFLICT DO NOTHING RETURNING id`** dedupes and returns ids
//!   in one statement — no read-modify-write, no extra round trip.
//! * **Pruning.** Read, unstarred articles older than `retention_days` are
//!   deleted in chunks, so the database cannot grow without bound.
//! * **Periodic WAL checkpointing** keeps the `-wal` file from ballooning on a
//!   long-running session.

use crate::config::Settings;
use crate::metrics::Metrics;
use crate::model::{Article, SourceId};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A source as it must exist in the database.
#[derive(Debug, Clone)]
pub struct SourceUpsert {
    pub key: Arc<str>,
    pub title: Arc<str>,
    pub url: Arc<str>,
    pub group: Arc<str>,
}

/// One article to persist. `source` is the in-memory dense id, echoed back on
/// insert so the engine can map database ids to runtime sources.
#[derive(Debug, Clone)]
pub struct NewArticle {
    pub source: SourceId,
    pub db_source: i64,
    pub uid_hash: u64,
    pub content_hash: u64,
    pub title: Arc<str>,
    pub link: Arc<str>,
    pub author: Option<Arc<str>>,
    pub published: i64,
    pub fetched: i64,
    pub preview: Arc<str>,
    pub content: Option<Arc<str>>,
}

/// A row loaded from the database, before the engine maps source ids.
#[derive(Debug, Clone)]
pub struct StoredArticle {
    pub id: i64,
    pub db_source: i64,
    pub uid_hash: u64,
    pub content_hash: u64,
    pub title: Arc<str>,
    pub link: Arc<str>,
    pub author: Option<Arc<str>>,
    pub published: i64,
    pub fetched: i64,
    pub preview: Arc<str>,
    pub has_content: bool,
    pub read: bool,
    pub starred: bool,
    pub archive_path: Option<Arc<str>>,
    pub archive_bytes: u32,
    /// When the reader last opened it, and how far they got (0..1).
    pub read_at: i64,
    pub progress: f32,
}

impl StoredArticle {
    /// Materialise into the shared model once the source is known.
    pub fn finish(self, source: SourceId, source_title: Arc<str>, group: Arc<str>) -> Article {
        let age = crate::text::human_age(crate::config::now_unix() - self.published);
        Article {
            id: self.id,
            source,
            db_source: self.db_source,
            uid_hash: self.uid_hash,
            content_hash: self.content_hash,
            title: self.title,
            link: self.link,
            author: self.author,
            source_title,
            group,
            published: self.published,
            fetched: self.fetched,
            preview: self.preview,
            has_content: self.has_content,
            read: self.read,
            starred: self.starred,
            age: Arc::from(age.as_str()),
            archive_path: self.archive_path,
            archive_bytes: self.archive_bytes,
            read_at: self.read_at,
            progress: self.progress,
        }
    }
}

/// Source state read back from the database at startup.
///
/// `etag` / `last_modified` are the important fields: they are what make the
/// first refresh after a restart a conditional request, so an unchanged feed
/// costs a 304 instead of re-downloading its whole body.
#[derive(Debug, Clone, Default)]
pub struct PersistedState {
    pub db_id: i64,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    pub last_fetch: i64,
    pub last_status: i32,
    pub error_count: u32,
    pub last_error: Option<Arc<str>>,
    pub interval_secs: u64,
    /// Size of the last full body, used to estimate what 304s saved.
    pub body_bytes: u64,
}

/// Persisted health of one source.
#[derive(Debug, Clone)]
pub struct SourceStateRow {
    pub db_id: i64,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    pub last_fetch: i64,
    pub last_status: i32,
    pub error_count: u32,
    pub last_error: Option<Arc<str>>,
    pub new_items: u32,
    pub interval_secs: u64,
    pub body_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Read,
    Starred,
}

enum Cmd {
    EnsureSources {
        items: Vec<SourceUpsert>,
        reply: flume::Sender<Vec<i64>>,
    },
    LoadWindow {
        limit: usize,
        per_source: usize,
        since: i64,
        reply: flume::Sender<Vec<StoredArticle>>,
    },
    LoadPerSource {
        per_source: usize,
        max_rows: usize,
        reply: flume::Sender<Vec<StoredArticle>>,
    },
    /// Every article of one source, newest first — no time window at all.
    SetFeatured {
        db_source: i64,
        featured: bool,
        reply: flume::Sender<()>,
    },
    LoadByIds {
        ids: Vec<i64>,
        reply: flume::Sender<Vec<StoredArticle>>,
    },
    LoadSource {
        db_source: i64,
        limit: usize,
        reply: flume::Sender<Vec<StoredArticle>>,
    },
    /// Remember how far an article has been read, and when.
    SaveProgress {
        id: i64,
        progress: f32,
        read_at: i64,
    },
    /// `(db source id, total, unread)` for every source that has articles.
    SourceCounts {
        reply: flume::Sender<Vec<(i64, u32, u32)>>,
    },
    FeaturedSources {
        reply: flume::Sender<Vec<i64>>,
    },
    Insert {
        items: Vec<NewArticle>,
        reply: flume::Sender<Vec<(usize, i64)>>,
    },
    SetFlag {
        ids: Vec<i64>,
        flag: Flag,
        value: bool,
    },
    FlagWhere {
        db_source: Option<i64>,
        flag: Flag,
        value: bool,
        before: i64,
        reply: flume::Sender<usize>,
    },
    DeleteIds {
        ids: Vec<i64>,
    },
    SaveArchive {
        id: i64,
        path: Arc<str>,
        bytes: u32,
    },
    SaveStates {
        rows: Vec<SourceStateRow>,
    },
    Prune {
        retention_days: u32,
        max_rows: u64,
        reply: flume::Sender<usize>,
    },
    Search {
        query: String,
        limit: usize,
        reply: flume::Sender<Vec<i64>>,
    },
    LoadContent {
        id: i64,
        reply: flume::Sender<Option<Arc<str>>>,
    },
    Counts {
        reply: flume::Sender<Vec<(i64, u32, u32)>>,
    },
    LoadStates {
        reply: flume::Sender<Vec<PersistedState>>,
    },
    ArchivePaths {
        reply: flume::Sender<Vec<Arc<str>>>,
    },
    DbBytes {
        reply: flume::Sender<u64>,
    },
    Checkpoint,
    Shutdown,
}

pub struct Store {
    tx: flume::Sender<Cmd>,
    path: PathBuf,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Latest known database size (main + `-wal` + `-shm`), refreshed by the
    /// writer thread and on demand.
    db_bytes: Arc<AtomicU64>,
    closed: AtomicBool,
    metrics: Arc<Metrics>,
    queue_high_water: AtomicU64,
}

impl Store {
    /// Open (creating if needed) and start the writer thread.
    pub fn open(path: &Path, settings: &Settings, metrics: Arc<Metrics>) -> anyhow::Result<Arc<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (tx, rx) = flume::bounded::<Cmd>(256);
        let conn = open_connection(path, settings)?;

        let store_path = path.to_path_buf();
        let fts = settings.full_text_search;
        let db_bytes = Arc::new(AtomicU64::new(file_bytes(path)));
        let bytes_for_thread = Arc::clone(&db_bytes);
        let metrics_for_thread = Arc::clone(&metrics);
        let handle = std::thread::Builder::new()
            .name("artifeeds-store".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let mut writer = Writer::new(conn, fts, store_path, metrics_for_thread, bytes_for_thread);
                writer.run(rx);
            })?;

        let store = Arc::new(Self {
            tx,
            path: path.to_path_buf(),
            handle: Mutex::new(Some(handle)),
            db_bytes,
            closed: AtomicBool::new(false),
            metrics,
            queue_high_water: AtomicU64::new(0),
        });
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn db_bytes(&self) -> u64 {
        self.db_bytes.load(Ordering::Relaxed)
    }

    pub fn queue_high_water(&self) -> u64 {
        self.queue_high_water.load(Ordering::Relaxed)
    }

    fn send(&self, cmd: Cmd) -> bool {
        let depth = self.tx.len() as u64 + 1;
        self.queue_high_water.fetch_max(depth, Ordering::Relaxed);
        if self.tx.send(cmd).is_err() {
            tracing::error!("store thread is gone; command dropped");
            return false;
        }
        true
    }

    /// Register/refresh the configured sources and get their database ids,
    /// positionally matching `items`.
    pub async fn ensure_sources(&self, items: Vec<SourceUpsert>) -> Vec<i64> {
        if items.is_empty() {
            return Vec::new();
        }
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::EnsureSources { items, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Load the most recent articles, capped both globally and per source.
    pub async fn load_window(
        &self,
        limit: usize,
        per_source: usize,
        since: i64,
    ) -> Vec<StoredArticle> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadWindow { limit, per_source, since, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// The newest `per_source` articles of **every** source, with no global
    /// time or recency filter.
    ///
    /// This is what keeps a fleet of largely dormant blogs usable: the recency
    /// window above would drop a blog whose last post is six months old, so the
    /// reader would show nothing at all for it (and an unread count of zero).
    /// `max_rows` must be `per_source * configured_sources`, so nothing is
    /// silently truncated — the query is deliberately unordered.
    pub async fn load_recent_per_source(
        &self,
        per_source: usize,
        max_rows: usize,
    ) -> Vec<StoredArticle> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadPerSource { per_source, max_rows, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// One source's articles, newest first.
    ///
    /// The in-memory window is a recency view of the whole fleet, and deliberately
    /// holds only a couple of items per dormant feed. That is the right shape for
    /// "what is new", and the wrong shape for "show me this blog": selecting a
    /// feed asks for its history, so it is read from the database.
    pub async fn load_source(&self, db_source: i64, limit: usize) -> Vec<StoredArticle> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadSource { db_source, limit, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// True article counts per source, straight from the database.
    ///
    /// The window's own counts are an artefact of how much of each feed it
    /// happens to hold — a dormant blog would report `2/2` while holding sixty
    /// articles — so the sidebar is fed from here instead.
    pub async fn featured_sources(&self) -> Vec<i64> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::FeaturedSources { reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    pub async fn source_counts(&self) -> Vec<(i64, u32, u32)> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::SourceCounts { reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Insert a batch inside a single transaction. Returns `(batch index, id)`
    /// for rows that were actually new.
    pub async fn insert_batch(&self, items: Vec<NewArticle>) -> Vec<(usize, i64)> {
        if items.is_empty() {
            return Vec::new();
        }
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::Insert { items, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Fire-and-forget: progress is not worth blocking a frame on.
    pub fn save_progress(&self, id: i64, progress: f32, read_at: i64) {
        self.send(Cmd::SaveProgress { id, progress, read_at });
    }

    pub fn set_flag(&self, ids: Vec<i64>, flag: Flag, value: bool) {
        if ids.is_empty() {
            return;
        }
        self.send(Cmd::SetFlag { ids, flag, value });
    }

    pub async fn set_flag_where(
        &self,
        db_source: Option<i64>,
        flag: Flag,
        value: bool,
        before: i64,
    ) -> usize {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::FlagWhere { db_source, flag, value, before, reply }) {
            return 0;
        }
        rx.recv_async().await.unwrap_or(0)
    }

    /// Record that an article's original page now lives at `path` on disk.
    pub fn save_archive(&self, id: i64, path: Arc<str>, bytes: u32) {
        self.send(Cmd::SaveArchive { id, path, bytes });
    }

    pub fn delete_ids(&self, ids: Vec<i64>) {
        if ids.is_empty() {
            return;
        }
        self.send(Cmd::DeleteIds { ids });
    }

    pub fn save_states(&self, rows: Vec<SourceStateRow>) {
        if rows.is_empty() {
            return;
        }
        self.send(Cmd::SaveStates { rows });
    }

    pub async fn prune(&self, retention_days: u32, max_rows: u64) -> usize {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::Prune { retention_days, max_rows, reply }) {
            return 0;
        }
        rx.recv_async().await.unwrap_or(0)
    }

    pub async fn search(&self, query: String, limit: usize) -> Vec<i64> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::Search { query, limit, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Mark a subscription as one of the reader's own, or take the mark away.
    pub async fn set_featured(&self, db_source: i64, featured: bool) {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::SetFeatured { db_source, featured, reply }) {
            return;
        }
        let _ = rx.recv_async().await;
    }

    /// Specific articles by id, in the order asked for.
    ///
    /// A search answers from the whole database while the reader looks at an
    /// in-memory window of it, so the hits have to be fetched before they can be
    /// shown — otherwise searching for a blog finds its articles in SQL and
    /// displays nothing.
    pub async fn load_by_ids(&self, ids: Vec<i64>) -> Vec<StoredArticle> {
        if ids.is_empty() {
            return Vec::new();
        }
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadByIds { ids, reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Full text of one article, loaded on demand so the in-memory window can
    /// stay small. Falls back to the preview when no body was stored.
    pub async fn load_content(&self, id: i64) -> Option<Arc<str>> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadContent { id, reply }) {
            return None;
        }
        rx.recv_async().await.ok().flatten()
    }

    pub async fn counts(&self) -> Vec<(i64, u32, u32)> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::Counts { reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Every archive file the database still references. Used to garbage
    /// collect files whose article was pruned.
    pub async fn archive_paths(&self) -> Vec<Arc<str>> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::ArchivePaths { reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    /// Validators and health for every known source, indexed by database id.
    pub async fn load_states(&self) -> Vec<PersistedState> {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::LoadStates { reply }) {
            return Vec::new();
        }
        rx.recv_async().await.unwrap_or_default()
    }

    pub async fn refresh_db_bytes(&self) -> u64 {
        let (reply, rx) = flume::bounded(1);
        if !self.send(Cmd::DbBytes { reply }) {
            return self.db_bytes.load(Ordering::Relaxed);
        }
        let n = rx.recv_async().await.unwrap_or(0);
        self.db_bytes.store(n, Ordering::Relaxed);
        n
    }

    pub fn checkpoint(&self) {
        self.send(Cmd::Checkpoint);
    }

    /// Stop the writer thread. Idempotent.
    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(handle) = self.handle.lock().take() {
            let _ = handle.join();
        }
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_connection(path: &Path, settings: &Settings) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -16000;
         PRAGMA mmap_size = 134217728;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 2000;",
    )?;
    migrate(&conn, settings)?;
    Ok(conn)
}

const SCHEMA_VERSION: i64 = 5;

fn migrate(conn: &Connection, settings: &Settings) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sources (
            id            INTEGER PRIMARY KEY,
            key           TEXT    NOT NULL UNIQUE,
            title         TEXT    NOT NULL DEFAULT '',
            url           TEXT    NOT NULL DEFAULT '',
            group_name    TEXT    NOT NULL DEFAULT '',
            etag          TEXT,
            last_modified TEXT,
            last_fetch    INTEGER NOT NULL DEFAULT 0,
            last_status   INTEGER NOT NULL DEFAULT 0,
            error_count   INTEGER NOT NULL DEFAULT 0,
            last_error    TEXT,
            new_items     INTEGER NOT NULL DEFAULT 0,
            interval_secs INTEGER NOT NULL DEFAULT 0,
            body_bytes    INTEGER NOT NULL DEFAULT 0,
            -- The reader's own mark, so it survives a re-import of the list the
            -- feed came from: an OPML replace must not un-feature anything.
            featured      INTEGER NOT NULL DEFAULT 0,
            added_at      INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS articles (
            id           INTEGER PRIMARY KEY,
            source_id    INTEGER NOT NULL,
            uid_hash     INTEGER NOT NULL,
            content_hash INTEGER NOT NULL,
            title        TEXT    NOT NULL,
            link         TEXT    NOT NULL DEFAULT '',
            author       TEXT,
            published    INTEGER NOT NULL,
            fetched      INTEGER NOT NULL,
            preview      TEXT    NOT NULL DEFAULT '',
            content      TEXT,
            read         INTEGER NOT NULL DEFAULT 0,
            starred      INTEGER NOT NULL DEFAULT 0,
            archive_path  TEXT,
            archived_at   INTEGER NOT NULL DEFAULT 0,
            archive_bytes INTEGER NOT NULL DEFAULT 0,
            read_at       INTEGER NOT NULL DEFAULT 0,
            progress      REAL    NOT NULL DEFAULT 0
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_articles_uid  ON articles(source_id, uid_hash);
         CREATE INDEX IF NOT EXISTS idx_articles_pub         ON articles(published DESC);
         CREATE INDEX IF NOT EXISTS idx_articles_src_pub     ON articles(source_id, published DESC);
         CREATE INDEX IF NOT EXISTS idx_articles_unread      ON articles(read, published DESC);
         CREATE INDEX IF NOT EXISTS idx_articles_hash        ON articles(content_hash);",
    )?;

    if settings.full_text_search {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts USING fts5(
                title, preview, content='articles', content_rowid='id', tokenize='unicode61'
             );
             CREATE TRIGGER IF NOT EXISTS articles_fts_ai AFTER INSERT ON articles BEGIN
                INSERT INTO articles_fts(rowid, title, preview) VALUES (new.id, new.title, new.preview);
             END;
             CREATE TRIGGER IF NOT EXISTS articles_fts_ad AFTER DELETE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, preview)
                VALUES ('delete', old.id, old.title, old.preview);
             END;
             CREATE TRIGGER IF NOT EXISTS articles_fts_au AFTER UPDATE OF title, preview ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, preview)
                VALUES ('delete', old.id, old.title, old.preview);
                INSERT INTO articles_fts(rowid, title, preview) VALUES (new.id, new.title, new.preview);
             END;",
        )?;
    }

    // Migrations. New databases get the current shape from the batch above;
    // existing ones are patched in place.
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 2 && !has_column(conn, "sources", "body_bytes")? {
        conn.execute_batch("ALTER TABLE sources ADD COLUMN body_bytes INTEGER NOT NULL DEFAULT 0;")?;
    }
    // v3: self-contained page archives.
    if !has_column(conn, "articles", "archive_path")? {
        conn.execute_batch(
            "ALTER TABLE articles ADD COLUMN archive_path TEXT;
             ALTER TABLE articles ADD COLUMN archived_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE articles ADD COLUMN archive_bytes INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    // v4: reading record — when it was last opened, and how far it got.
    if !has_column(conn, "articles", "read_at")? {
        conn.execute_batch(
            "ALTER TABLE articles ADD COLUMN read_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE articles ADD COLUMN progress REAL NOT NULL DEFAULT 0;
             CREATE INDEX IF NOT EXISTS idx_articles_read_at ON articles(read_at DESC);",
        )?;
    }
    // v5: a subscription the reader marked as one of their own.
    if !has_column(conn, "sources", "featured")? {
        conn.execute_batch("ALTER TABLE sources ADD COLUMN featured INTEGER NOT NULL DEFAULT 0;")?;
    }
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

struct Writer {
    conn: Connection,
    fts: bool,
    path: PathBuf,
    metrics: Arc<Metrics>,
    db_bytes: Arc<AtomicU64>,
    batches: u64,
    inserted_since_prune: u64,
    last_checkpoint: std::time::Instant,
}

impl Writer {
    fn new(
        conn: Connection,
        fts: bool,
        path: PathBuf,
        metrics: Arc<Metrics>,
        db_bytes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            conn,
            fts,
            path,
            metrics,
            db_bytes,
            batches: 0,
            inserted_since_prune: 0,
            last_checkpoint: std::time::Instant::now(),
        }
    }

    fn run(&mut self, rx: flume::Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::EnsureSources { items, reply } => {
                    let _ = reply.send(self.ensure_sources(&items));
                }
                Cmd::SetFeatured { db_source, featured, reply } => {
                    if let Err(e) = self.conn.execute(
                        "UPDATE sources SET featured = ?2 WHERE id = ?1",
                        rusqlite::params![db_source, featured as i64],
                    ) {
                        tracing::warn!("set_featured failed: {e}");
                    }
                    let _ = reply.send(());
                }
                Cmd::LoadByIds { ids, reply } => {
                    let _ = reply.send(self.load_by_ids_rows(&ids));
                }
                Cmd::LoadWindow { limit, per_source, since, reply } => {
                    let _ = reply.send(self.load_window(limit, per_source, since));
                }
                Cmd::LoadPerSource { per_source, max_rows, reply } => {
                    let _ = reply.send(self.load_recent_per_source(per_source, max_rows));
                }
                Cmd::LoadSource { db_source, limit, reply } => {
                    let _ = reply.send(self.load_source_rows(db_source, limit));
                }
                Cmd::SaveProgress { id, progress, read_at } => {
                    self.save_progress(id, progress, read_at);
                }
                Cmd::FeaturedSources { reply } => {
                    let _ = reply.send(self.featured_rows());
                }
                Cmd::SourceCounts { reply } => {
                    let _ = reply.send(self.source_counts_rows());
                }
                Cmd::Insert { items, reply } => {
                    let ids = self.insert(&items);
                    let _ = reply.send(ids);
                }
                Cmd::SetFlag { ids, flag, value } => self.set_flag(&ids, flag, value),
                Cmd::FlagWhere { db_source, flag, value, before, reply } => {
                    let _ = reply.send(self.flag_where(db_source, flag, value, before));
                }
                Cmd::DeleteIds { ids } => self.delete_ids(&ids),
                Cmd::SaveArchive { id, path, bytes } => self.save_archive(id, &path, bytes),
                Cmd::SaveStates { rows } => self.save_states(&rows),
                Cmd::Prune { retention_days, max_rows, reply } => {
                    let _ = reply.send(self.prune(retention_days, max_rows));
                }
                Cmd::Search { query, limit, reply } => {
                    let _ = reply.send(self.search(&query, limit));
                }
                Cmd::LoadContent { id, reply } => {
                    let _ = reply.send(self.load_content(id));
                }
                Cmd::Counts { reply } => {
                    let _ = reply.send(self.counts());
                }
                Cmd::LoadStates { reply } => {
                    let _ = reply.send(self.load_states());
                }
                Cmd::ArchivePaths { reply } => {
                    let _ = reply.send(self.archive_paths());
                }
                Cmd::DbBytes { reply } => {
                    let _ = reply.send(self.db_bytes());
                }
                Cmd::Checkpoint => {
                    let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
                }
                Cmd::Shutdown => {
                    let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;");
                    break;
                }
            }
            self.maybe_maintenance();
        }
    }

    fn ensure_sources(&mut self, items: &[SourceUpsert]) -> Vec<i64> {
        let now = crate::config::now_unix();
        let tx = match self.conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("ensure_sources: begin failed: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(items.len());
        {
            let mut stmt = match tx.prepare_cached(
                "INSERT INTO sources(key, title, url, group_name, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key) DO UPDATE SET title = excluded.title,
                                                url = excluded.url,
                                                group_name = excluded.group_name
                 RETURNING id",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("ensure_sources: prepare failed: {e}");
                    return Vec::new();
                }
            };
            for item in items {
                match stmt.query_row(
                    rusqlite::params![item.key.as_ref(), item.title.as_ref(), item.url.as_ref(), item.group.as_ref(), now],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(id) => out.push(id),
                    Err(e) => {
                        tracing::warn!("ensure_sources failed for {}: {e}", item.key);
                        out.push(0);
                    }
                }
            }
        }
        if let Err(e) = tx.commit() {
            tracing::error!("ensure_sources commit failed: {e}");
        }
        out
    }

    /// Persist a reading position. Called on a timer while reading and once on
    /// leaving, so a crash costs at most a few seconds of progress.
    fn save_progress(&mut self, id: i64, progress: f32, read_at: i64) {
        if let Err(e) = self.conn.execute(
            "UPDATE articles SET progress = ?2, read_at = ?3 WHERE id = ?1",
            rusqlite::params![id, progress.clamp(0.0, 1.0) as f64, read_at],
        ) {
            tracing::warn!("save_progress failed: {e}");
        }
    }

    /// Every article of one source, newest first.
    /// The columns a stored article needs, in the order `map_stored` reads them.
    const STORED_COLUMNS: &'static str = "id, source_id, uid_hash, content_hash, title, link, author,
                                  published, fetched, preview, content IS NOT NULL, read, starred,
                                  archive_path, archive_bytes, read_at, progress, read_at, progress";

    fn load_by_ids_rows(&mut self, ids: &[i64]) -> Vec<StoredArticle> {
        // A search limit is small by construction, and the ids come from a query
        // this process just ran, so they are safe to inline as placeholders.
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {} FROM articles WHERE id IN ({placeholders})",
            Self::STORED_COLUMNS
        );
        let mut stmt = match self.conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("load_by_ids prepare failed: {e}");
                return Vec::new();
            }
        };
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        match stmt.query_map(params.as_slice(), map_stored) {
            Ok(iter) => {
                let mut rows: Vec<StoredArticle> = iter.filter_map(|r| r.ok()).collect();
                // `IN` does not preserve order; the caller ranked them.
                let rank: std::collections::HashMap<i64, usize> =
                    ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
                rows.sort_unstable_by_key(|r| rank.get(&r.id).copied().unwrap_or(usize::MAX));
                rows
            }
            Err(e) => {
                tracing::error!("load_by_ids failed: {e}");
                Vec::new()
            }
        }
    }

    fn load_source_rows(&mut self, db_source: i64, limit: usize) -> Vec<StoredArticle> {
        const SQL: &str = "SELECT id, source_id, uid_hash, content_hash, title, link, author,
                                  published, fetched, preview, content IS NOT NULL, read, starred,
                                  archive_path, archive_bytes, read_at, progress, read_at, progress
                           FROM articles WHERE source_id = ?1
                           ORDER BY published DESC LIMIT ?2";
        let mut stmt = match self.conn.prepare_cached(SQL) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("load_source prepare failed: {e}");
                return Vec::new();
            }
        };
        match stmt.query_map(rusqlite::params![db_source, limit as i64], map_stored) {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::error!("load_source failed: {e}");
                Vec::new()
            }
        }
    }

    /// Article totals per source. One grouped scan; the caller runs it on a
    /// schedule rather than per frame.
    /// The feeds the reader marked as their own.
    ///
    /// Deliberately not folded into the counts: those are grouped by *articles*,
    /// so a feed that has none yet — a subscription added a minute ago — would
    /// not appear, and its star would vanish until the first article arrived.
    fn featured_rows(&mut self) -> Vec<i64> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self.conn.prepare_cached("SELECT id FROM sources WHERE featured != 0")
            && let Ok(rows) = stmt.query_map([], |r| r.get::<_, i64>(0))
        {
            out.extend(rows.flatten());
        }
        out
    }

    fn source_counts_rows(&mut self) -> Vec<(i64, u32, u32)> {
        const SQL: &str = "SELECT source_id, COUNT(*), SUM(read = 0) FROM articles GROUP BY source_id";
        let mut stmt = match self.conn.prepare_cached(SQL) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("source_counts prepare failed: {e}");
                return Vec::new();
            }
        };
        match stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?, row.get::<_, u32>(2)?))
        }) {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::error!("source_counts failed: {e}");
                Vec::new()
            }
        }
    }

    fn load_window(&mut self, limit: usize, per_source: usize, since: i64) -> Vec<StoredArticle> {
        // Per-source cap via a window function keeps one firehose feed from
        // dominating the in-memory window.
        let sql = "SELECT id, source_id, uid_hash, content_hash, title, link, author,
                          published, fetched, preview, content IS NOT NULL, read, starred, archive_path, archive_bytes, read_at, progress
                   FROM (
                       SELECT a.*, ROW_NUMBER() OVER (PARTITION BY a.source_id ORDER BY a.published DESC) AS rn
                       FROM articles a
                       WHERE a.published >= ?2
                   )
                   WHERE rn <= ?3
                   ORDER BY published DESC
                   LIMIT ?1";
        let mut stmt = match self.conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("load_window prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![limit as i64, since, per_source as i64], map_stored);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::error!("load_window failed: {e}");
                Vec::new()
            }
        }
    }

    fn load_recent_per_source(&mut self, per_source: usize, max_rows: usize) -> Vec<StoredArticle> {
        let sql = "SELECT id, source_id, uid_hash, content_hash, title, link, author,
                          published, fetched, preview, content IS NOT NULL, read, starred, archive_path, archive_bytes, read_at, progress
                   FROM (
                       SELECT a.*, ROW_NUMBER() OVER (PARTITION BY a.source_id ORDER BY a.published DESC) AS rn
                       FROM articles a
                   )
                   WHERE rn <= ?1
                   LIMIT ?2";
        let mut stmt = match self.conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("load_recent_per_source prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(rusqlite::params![per_source as i64, max_rows as i64], map_stored);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(e) => {
                tracing::error!("load_recent_per_source failed: {e}");
                Vec::new()
            }
        }
    }

    /// One transaction for the whole batch.
    fn insert(&mut self, items: &[NewArticle]) -> Vec<(usize, i64)> {
        let started = std::time::Instant::now();
        let tx = match self.conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("insert: begin failed: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(items.len());
        {
            let mut stmt = match tx.prepare_cached(
                "INSERT INTO articles (source_id, uid_hash, content_hash, title, link, author,
                                       published, fetched, preview, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(source_id, uid_hash) DO NOTHING
                 RETURNING id",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("insert: prepare failed: {e}");
                    return Vec::new();
                }
            };
            for (idx, item) in items.iter().enumerate() {
                let res: Result<i64, _> = stmt.query_row(
                    rusqlite::params![
                        item.db_source,
                        item.uid_hash as i64,
                        item.content_hash as i64,
                        item.title.as_ref(),
                        item.link.as_ref(),
                        item.author.as_deref(),
                        item.published,
                        item.fetched,
                        item.preview.as_ref(),
                        item.content.as_deref(),
                    ],
                    |r| r.get(0),
                );
                match res {
                    Ok(id) => out.push((idx, id)),
                    // Conflict: the row already exists. Not an error.
                    Err(rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(e) => tracing::warn!("insert failed: {e}"),
                }
            }
        }
        if let Err(e) = tx.commit() {
            tracing::error!("insert commit failed: {e}");
        }
        self.batches += 1;
        self.inserted_since_prune += out.len() as u64;
        self.metrics.record_batch(started.elapsed().as_millis() as u32);
        out
    }

    fn set_flag(&mut self, ids: &[i64], flag: Flag, value: bool) {
        let sql = match flag {
            Flag::Read => "UPDATE articles SET read = ?1 WHERE id = ?2",
            Flag::Starred => "UPDATE articles SET starred = ?1 WHERE id = ?2",
        };
        let tx = match self.conn.transaction() {
            Ok(t) => t,
            Err(_) => return,
        };
        {
            let Ok(mut stmt) = tx.prepare_cached(sql) else {
                return;
            };
            for id in ids {
                let _ = stmt.execute(rusqlite::params![value as i64, id]);
            }
        }
        let _ = tx.commit();
    }

    fn flag_where(&mut self, db_source: Option<i64>, flag: Flag, value: bool, before: i64) -> usize {
        let (col, extra) = match flag {
            Flag::Read => ("read", " AND read = 0"),
            Flag::Starred => ("starred", ""),
        };
        let sql = match db_source {
            Some(_) => format!("UPDATE articles SET {col} = ?1 WHERE source_id = ?2 AND published <= ?3{extra}"),
            None => format!("UPDATE articles SET {col} = ?1 WHERE published <= ?3{extra}"),
        };
        let changed = match db_source {
            Some(src) => self.conn.execute(&sql, rusqlite::params![value as i64, src, before]),
            None => self.conn.execute(&sql, rusqlite::params![value as i64, before]),
        };
        changed.unwrap_or(0)
    }

    fn save_archive(&mut self, id: i64, path: &str, bytes: u32) {
        let changed = self.conn.execute(
            "UPDATE articles SET archive_path = ?2, archived_at = ?3, archive_bytes = ?4 WHERE id = ?1",
            rusqlite::params![id, path, crate::config::now_unix(), bytes as i64],
        );
        if let Err(e) = changed {
            tracing::warn!("save_archive failed: {e}");
        }
    }

    fn delete_ids(&mut self, ids: &[i64]) {
        let tx = match self.conn.transaction() {
            Ok(t) => t,
            Err(_) => return,
        };
        {
            let Ok(mut stmt) = tx.prepare_cached("DELETE FROM articles WHERE id = ?1") else {
                return;
            };
            for id in ids {
                let _ = stmt.execute(rusqlite::params![id]);
            }
        }
        let _ = tx.commit();
    }

    fn save_states(&mut self, rows: &[SourceStateRow]) {
        let tx = match self.conn.transaction() {
            Ok(t) => t,
            Err(_) => return,
        };
        {
            let Ok(mut stmt) = tx.prepare_cached(
                "UPDATE sources SET etag = ?2, last_modified = ?3, last_fetch = ?4, last_status = ?5,
                                    error_count = ?6, last_error = ?7, new_items = ?8, interval_secs = ?9,
                                    body_bytes = ?10
                 WHERE id = ?1",
            ) else {
                return;
            };
            for row in rows {
                let _ = stmt.execute(rusqlite::params![
                    row.db_id,
                    row.etag.as_deref(),
                    row.last_modified.as_deref(),
                    row.last_fetch,
                    row.last_status,
                    row.error_count,
                    row.last_error.as_deref(),
                    row.new_items,
                    row.interval_secs as i64,
                    row.body_bytes as i64,
                ]);
            }
        }
        let _ = tx.commit();
    }

    /// Delete read, unstarred articles older than the retention window, then
    /// enforce a hard row cap (oldest first) so a very large fleet still fits.
    fn prune(&mut self, retention_days: u32, max_rows: u64) -> usize {
        let mut deleted = 0usize;
        if retention_days > 0 {
            let cutoff = crate::config::now_unix() - (retention_days as i64) * 86_400;
            // Chunked so a first-time prune on a huge database does not hold
            // one enormous write transaction.
            loop {
                let n = self
                    .conn
                    .execute(
                        "DELETE FROM articles WHERE id IN (
                             SELECT id FROM articles
                             WHERE read = 1 AND starred = 0 AND published < ?1
                             LIMIT 5000)",
                        rusqlite::params![cutoff],
                    )
                    .unwrap_or(0);
                deleted += n;
                if n < 5000 {
                    break;
                }
            }
        }
        if max_rows > 0 {
            let total: u64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as u64;
            if total > max_rows {
                let excess = (total - max_rows) as i64;
                deleted += self
                    .conn
                    .execute(
                        "DELETE FROM articles WHERE id IN (
                             SELECT id FROM articles WHERE starred = 0
                             ORDER BY published ASC LIMIT ?1)",
                        rusqlite::params![excess],
                    )
                    .unwrap_or(0);
            }
        }
        if deleted > 0 {
            let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
        deleted
    }

    fn search(&mut self, query: &str, limit: usize) -> Vec<i64> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        if self.fts {
            let fts_query = build_fts_query(trimmed);
            let mut stmt = match self
                .conn
                .prepare_cached("SELECT rowid FROM articles_fts WHERE articles_fts MATCH ?1 ORDER BY rank LIMIT ?2")
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("search prepare failed: {e}");
                    return Vec::new();
                }
            };
            let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |r| r.get::<_, i64>(0));
            let mut ids: Vec<i64> = match rows {
                Ok(it) => it.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    tracing::warn!("search failed: {e}");
                    Vec::new()
                }
            };
            // A subscription is a thing the reader looks for by name, and its
            // name is not in the text of its articles: searching for a blog you
            // just added has to find that blog's posts, or adding one feels like
            // it did nothing.
            if let Ok(mut stmt) = self.conn.prepare_cached(
                "SELECT a.id FROM articles a
                 WHERE a.source_id IN (SELECT id FROM sources WHERE lower(title) LIKE ?1 OR lower(url) LIKE ?1)
                 ORDER BY a.published DESC LIMIT ?2",
            ) {
                let like = format!("%{}%", trimmed.to_lowercase());
                if let Ok(it) = stmt.query_map(rusqlite::params![like, limit as i64], |r| r.get::<_, i64>(0)) {
                    let seen: std::collections::HashSet<i64> = ids.iter().copied().collect();
                    for id in it.flatten() {
                        if !seen.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
            }
            return ids;
        }
        let like = format!("%{}%", trimmed.to_lowercase());
        let mut stmt = match self
            .conn
            .prepare_cached("SELECT id FROM articles WHERE lower(title) LIKE ?1 OR lower(preview) LIKE ?1 ORDER BY published DESC LIMIT ?2")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![like, limit as i64], |r| r.get::<_, i64>(0));
        rows.map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    /// Full body for the reader pane; `content` when present, else `preview`.
    fn load_content(&mut self, id: i64) -> Option<Arc<str>> {
        let res = self.conn.query_row(
            "SELECT COALESCE(content, preview) FROM articles WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get::<_, String>(0),
        );
        match res {
            Ok(text) if !text.is_empty() => Some(Arc::from(text.as_str())),
            _ => None,
        }
    }

    fn archive_paths(&mut self) -> Vec<Arc<str>> {
        let mut stmt = match self
            .conn
            .prepare_cached("SELECT archive_path FROM articles WHERE archive_path IS NOT NULL")
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("archive_paths prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        rows.map(|it| it.filter_map(|r| r.ok()).map(|s| Arc::from(s.as_str())).collect())
            .unwrap_or_default()
    }

    fn load_states(&mut self) -> Vec<PersistedState> {
        let mut stmt = match self.conn.prepare_cached(
            "SELECT id, etag, last_modified, last_fetch, last_status, error_count,
                    last_error, interval_secs, body_bytes
             FROM sources",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("load_states prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok(PersistedState {
                db_id: row.get(0)?,
                etag: row.get::<_, Option<String>>(1)?.map(|s| Arc::from(s.as_str())),
                last_modified: row.get::<_, Option<String>>(2)?.map(|s| Arc::from(s.as_str())),
                last_fetch: row.get(3)?,
                last_status: row.get(4)?,
                error_count: row.get::<_, i64>(5)? as u32,
                last_error: row.get::<_, Option<String>>(6)?.map(|s| Arc::from(s.as_str())),
                interval_secs: row.get::<_, i64>(7)?.max(0) as u64,
                body_bytes: row.get::<_, i64>(8)?.max(0) as u64,
            })
        });
        rows.map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    fn counts(&mut self) -> Vec<(i64, u32, u32)> {
        let mut stmt = match self.conn.prepare_cached(
            "SELECT source_id, SUM(CASE WHEN read = 0 THEN 1 ELSE 0 END), COUNT(*) FROM articles GROUP BY source_id",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32, r.get::<_, i64>(2)? as u32))
        });
        rows.map(|it| it.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    fn db_bytes(&self) -> u64 {
        file_bytes(&self.path)
    }

    /// Cheap housekeeping: keep the WAL small and publish the database size so
    /// the status bar can show it without touching the filesystem per frame.
    fn maybe_maintenance(&mut self) {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_checkpoint) < std::time::Duration::from_secs(120) {
            return;
        }
        self.last_checkpoint = now;
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        self.db_bytes.store(self.db_bytes(), Ordering::Relaxed);
        self.batches = self.batches.saturating_add(0);
    }
}

/// Size of the database plus its WAL/SHM sidecars.
fn file_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let p = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", path.display()))
        };
        if let Ok(meta) = std::fs::metadata(&p) {
            total += meta.len();
        }
    }
    total
}

/// Shared row → [`StoredArticle`] mapping for the window loaders. The column
/// list is fixed by both queries.
fn map_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredArticle> {
    Ok(StoredArticle {
        id: row.get(0)?,
        db_source: row.get(1)?,
        uid_hash: row.get::<_, i64>(2)? as u64,
        content_hash: row.get::<_, i64>(3)? as u64,
        title: Arc::from(row.get::<_, String>(4)?.as_str()),
        link: Arc::from(row.get::<_, String>(5)?.as_str()),
        author: row.get::<_, Option<String>>(6)?.map(|s| Arc::from(s.as_str())),
        published: row.get(7)?,
        fetched: row.get(8)?,
        preview: Arc::from(row.get::<_, String>(9)?.as_str()),
        has_content: row.get::<_, i64>(10)? != 0,
        read: row.get::<_, i64>(11)? != 0,
        starred: row.get::<_, i64>(12)? != 0,
        archive_path: row.get::<_, Option<String>>(13)?.map(|s| Arc::from(s.as_str())),
        archive_bytes: row.get::<_, i64>(14)?.max(0) as u32,
        read_at: row.get::<_, i64>(15)?,
        progress: row.get::<_, f64>(16)? as f32,
    })
}

/// Turn free text into an FTS5 MATCH expression: every token becomes a quoted
/// prefix term, implicitly AND-ed. Quoting neutralises FTS operators typed by
/// the user (`AND`, `*`, `"`), which would otherwise be syntax errors.
pub fn build_fts_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for token in input.split_whitespace() {
        let cleaned: String = token.chars().filter(|c| !matches!(c, '"' | '\'' | '*' | '(' | ')')).collect();
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" AND ");
        }
        out.push('"');
        out.push_str(&cleaned);
        out.push_str("\"*");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    fn tmp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("artifeeds-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test.db")
    }

    fn new_article(src: i64, uid: u64, title: &str, published: i64) -> NewArticle {
        NewArticle {
            source: 0,
            db_source: src,
            uid_hash: uid,
            content_hash: uid.wrapping_mul(31),
            title: Arc::from(title),
            link: Arc::from(format!("https://e.com/{uid}").as_str()),
            author: None,
            published,
            fetched: published,
            preview: Arc::from(format!("preview {title}").as_str()),
            content: None,
        }
    }

    #[test]
    fn round_trip_insert_dedup_and_flags() {
        let path = tmp_db("roundtrip");
        let settings = Settings::default();
        let metrics = Metrics::new();
        let store = Store::open(&path, &settings, metrics).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let ids = store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from("k1"),
                    title: Arc::from("Source One"),
                    url: Arc::from("https://e.com/feed"),
                    group: Arc::from("G"),
                }])
                .await;
            assert_eq!(ids.len(), 1);
            let db_id = ids[0];
            assert!(db_id > 0);

            // Idempotent.
            let again = store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from("k1"),
                    title: Arc::from("Renamed"),
                    url: Arc::from("https://e.com/feed"),
                    group: Arc::from("G"),
                }])
                .await;
            assert_eq!(again[0], db_id);

            let batch = vec![
                new_article(db_id, 1, "one", 100),
                new_article(db_id, 2, "two", 200),
                new_article(db_id, 1, "one again", 300), // duplicate uid
            ];
            let inserted = store.insert_batch(batch).await;
            assert_eq!(inserted.len(), 2, "duplicate uid must be ignored");

            // Re-inserting the same batch inserts nothing.
            let dup = store
                .insert_batch(vec![new_article(db_id, 1, "one", 100), new_article(db_id, 2, "two", 200)])
                .await;
            assert!(dup.is_empty());

            let window = store.load_window(100, 50, 0).await;
            assert_eq!(window.len(), 2);
            assert_eq!(window[0].title.as_ref(), "two", "newest first");

            let new_id = window[0].id;
            store.set_flag(vec![new_id], Flag::Read, true);
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let window = store.load_window(100, 50, 0).await;
            assert!(window.iter().find(|a| a.id == new_id).unwrap().read);

            let counts = store.counts().await;
            assert_eq!(counts.len(), 1);
            assert_eq!(counts[0].1, 1, "one unread left");

            let bytes = store.refresh_db_bytes().await;
            assert!(bytes > 0);
        });
        store.shutdown();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn search_and_prune() {
        let path = tmp_db("search");
        let settings = Settings::default();
        let store = Store::open(&path, &settings, Metrics::new()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db_id = store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from("k"),
                    title: Arc::from("S"),
                    url: Arc::from("https://e.com/f"),
                    group: Arc::from("G"),
                }])
                .await[0];
            let old = crate::config::now_unix() - 400 * 86_400;
            let mut batch = vec![
                new_article(db_id, 10, "Rust async runtimes", 1_000),
                new_article(db_id, 11, "Python packaging", 1_001),
                new_article(db_id, 12, "old news", old),
            ];
            // Mark the old one read so pruning can remove it.
            let inserted = store.insert_batch(std::mem::take(&mut batch)).await;
            let old_id = inserted
                .iter()
                .find(|(i, _)| *i == 2)
                .map(|(_, id)| *id)
                .unwrap();
            store.set_flag(vec![old_id], Flag::Read, true);
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;

            let hits = store.search("rust".into(), 50).await;
            assert_eq!(hits.len(), 1);
            let hits = store.search("pack".into(), 50).await;
            assert_eq!(hits.len(), 1, "prefix search");
            // FTS operators typed by a user are quoted into literal terms, so
            // "OR" becomes a search term that matches nothing here rather than
            // changing the query semantics.
            let hits = store.search("rust OR python".into(), 50).await;
            assert_eq!(hits.len(), 0, "operators are neutralised, not interpreted");
            let hits = store.search("rust python".into(), 50).await;
            assert_eq!(hits.len(), 0, "multiple terms are AND-ed");

            let pruned = store.prune(120, 0).await;
            assert_eq!(pruned, 1);
        });
        store.shutdown();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn validators_survive_a_restart() {
        let path = tmp_db("restart");
        let settings = Settings::default();
        let store = Store::open(&path, &settings, Metrics::new()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db_id = store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from("k"),
                    title: Arc::from("S"),
                    url: Arc::from("https://e.com/f"),
                    group: Arc::from("G"),
                }])
                .await[0];
            store.save_states(vec![SourceStateRow {
                db_id,
                etag: Some(Arc::from("\"abc\"")),
                last_modified: Some(Arc::from("Wed, 02 Oct 2024 12:00:00 GMT")),
                last_fetch: 1_700_000_000,
                last_status: 200,
                error_count: 3,
                last_error: Some(Arc::from("boom")),
                new_items: 7,
                interval_secs: 3600,
                body_bytes: 12_345,
            }]);
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let states = store.load_states().await;
            assert_eq!(states.len(), 1);
            assert_eq!(states[0].etag.as_deref(), Some("\"abc\""));
            assert_eq!(states[0].error_count, 3);
            assert_eq!(states[0].interval_secs, 3600);
            assert_eq!(states[0].body_bytes, 12_345);
            assert_eq!(states[0].db_id, db_id);
        });
        store.shutdown();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn fts_query_builder_is_safe() {
        assert_eq!(build_fts_query("hello world"), "\"hello\"* AND \"world\"*");
        assert_eq!(build_fts_query("a\"b*(c)"), "\"abc\"*");
        assert_eq!(build_fts_query("   "), "");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::config::Settings;

    /// A subscription is looked up by its own name, and that name is not in the
    /// text of its articles — searching for a blog you just added has to find
    /// its posts, or adding one looks like it did nothing.
    #[test]
    fn a_search_finds_a_blog_by_its_name() {
        let dir = std::env::temp_dir().join(format!("artifeeds-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db"), &Settings::default(), Metrics::new()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let ids = store
                .ensure_sources(vec![SourceUpsert {
                    key: Arc::from("https://blog.rust-lang.org/feed.xml"),
                    title: Arc::from("Rust Blog"),
                    url: Arc::from("https://blog.rust-lang.org/feed.xml"),
                    group: Arc::from("编程语言官方博客"),
                }])
                .await;
            let db_id = ids[0];
            let article = NewArticle {
                source: 0,
                db_source: db_id,
                uid_hash: 7,
                content_hash: 7,
                title: Arc::from("Announcing a new compiler release"),
                link: Arc::from("https://blog.rust-lang.org/2026/01/01/release.html"),
                author: None,
                published: 1_800_000_000,
                fetched: 1_800_000_000,
                preview: Arc::from("Faster builds and better errors."),
                content: None,
            };
            let inserted = store.insert_batch(vec![article.clone()]).await;
            assert_eq!(inserted.len(), 1);

            // Neither the title nor the preview says "rust"; the *blog* is named
            // Rust Blog, and that is what the reader types.
            let hits = store.search("rust".to_string(), 50).await;
            assert!(!hits.is_empty(), "the blog's name should find its articles");

            // An unrelated word still finds nothing.
            assert!(store.search("javascript".to_string(), 50).await.is_empty());

            // And text search keeps working.
            assert!(!store.search("compiler".to_string(), 50).await.is_empty());
        });
    }
}

#[cfg(test)]
mod featured_tests {
    use super::*;
    use crate::config::Settings;

    /// The featured mark is the reader's, so it lives in the database and
    /// survives whatever happens to the file the feed was configured in.
    #[test]
    fn the_featured_mark_round_trips() {
        let dir = std::env::temp_dir().join(format!("artifeeds-featured-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db"), &Settings::default(), Metrics::new()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ids = store
                .ensure_sources(vec![
                    SourceUpsert {
                        key: Arc::from("k1"),
                        title: Arc::from("One"),
                        url: Arc::from("https://one.example/feed"),
                        group: Arc::from("G"),
                    },
                    SourceUpsert {
                        key: Arc::from("k2"),
                        title: Arc::from("Two"),
                        url: Arc::from("https://two.example/feed"),
                        group: Arc::from("G"),
                    },
                ])
                .await;
            let (first, second) = (ids[0], ids[1]);

            // Nothing is featured to begin with.
            assert!(store.featured_sources().await.is_empty());

            // Note: neither feed has articles yet, and the mark must still be
            // reported — that is the case a counts-driven query gets wrong.
            store.set_featured(first, true).await;
            let featured = store.featured_sources().await;
            assert_eq!(featured, vec![first], "the featured feed is reported");
            assert!(!featured.contains(&second));

            // ...and taking it back works, which is what `*` does twice.
            store.set_featured(first, false).await;
            assert!(store.featured_sources().await.is_empty());
        });
    }
}

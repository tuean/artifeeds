//! Configuration: schema, loader, validator, compiler and hot-reloader.
//!
//! ## Why this is its own layer
//!
//! "Lots of external configuration" is the stated workload, so the loader is
//! built for *thousands* of sources spread over *hundreds* of files:
//!
//! * **Fault isolation.** A malformed file produces a [`ConfigProblem`] and is
//!   skipped; every other file still loads. One typo never takes the app down.
//! * **Composition.** `[[sources]]` in the root file, plus `include = [...]`
//!   entries that may point at a file *or a whole directory* (all `*.toml`,
//!   `*.opml`, `*.xml` inside are picked up). OPML exports from other readers
//!   therefore drop straight in.
//! * **Compile once.** Files are parsed into `SourceConfig`, then *compiled*
//!   into [`CompiledConfig`]: dense `SourceId`s, interned `Arc<str>` fields,
//!   `FxHashMap` indexes for O(1) lookup by key or by URL hash. The UI and the
//!   network layer never touch the raw TOML again.
//! * **Reload is a diff, not a restart.** Every source carries a 64-bit
//!   fingerprint. After a reload the engine compares key→fingerprint maps and
//!   only touches genuinely added/removed/changed sources, preserving ETags,
//!   backoff state and adaptive intervals for everything else.
//! * **Watching is debounced and cheap.** One `notify` watcher over the distinct
//!   parent directories (not one watcher per file), with a 300 ms settle window
//!   so an editor writing 40 files triggers exactly one reload.

use crate::model::SourceId;
use crate::text::{hash64, spread};
use crate::default_data_dir;
use arc_swap::ArcSwap;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A non-fatal problem with one input file. Shown in the UI status bar.
#[derive(Debug, Clone)]
pub struct ConfigProblem {
    pub file: Arc<str>,
    pub message: Arc<str>,
}

impl std::fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.message)
    }
}

// ---------------------------------------------------------------------------
// Raw (on-disk) schema
// ---------------------------------------------------------------------------

fn d_true() -> bool {
    true
}
fn d_user_agent() -> String {
    concat!(
        "Artifeeds/",
        env!("CARGO_PKG_VERSION"),
        " (+https://github.com/local/artifeeds; feed reader)"
    )
    .into()
}

/// Global tunables. Every knob that influences resource use lives here so an
/// operator with 5 000 feeds can dial the footprint without touching code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// SQLite file. `~` is expanded.
    pub db_path: Option<String>,
    pub user_agent: String,
    pub accept_language: String,
    /// Concurrent in-flight HTTP requests across all sources.
    pub global_concurrency: usize,
    /// Concurrent in-flight requests per host (politeness + avoids a single
    /// slow host from eating the global budget).
    pub per_host_concurrency: usize,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    /// Minimum spacing between two requests to the same host. Keeps a fleet of
    /// feeds that live on one domain (Blogspot, Medium, a company blog) polite.
    pub host_min_gap_ms: u64,
    /// Never poll a source faster than this, whatever the config says.
    pub min_interval_secs: u64,
    /// Never poll a source slower than this, even after long quiet periods.
    pub max_interval_secs: u64,
    pub default_interval_secs: u64,
    /// ±% randomised on every scheduled wake-up to spread load.
    pub jitter_percent: u8,
    /// Sources are staggered across this window on startup (and after a reload
    /// adds new ones). Long enough to avoid a stampede, short enough that the
    /// reader is populated within seconds of launch.
    pub startup_spread_secs: u64,
    /// Entries considered per fetch (feeds with 10 000 items are truncated).
    pub max_entries_per_fetch: usize,
    /// Hard cap on a single response body; the connection is dropped past it.
    pub max_body_bytes: u64,
    pub preview_chars: usize,
    pub max_content_bytes: usize,
    /// Delete read articles older than this (0 = keep forever).
    pub retention_days: u32,
    /// Hard cap on stored articles; the oldest unstarred rows are dropped past
    /// it (0 = unlimited). Keeps a 5 000-feed database bounded.
    pub max_articles: u32,
    /// Cap on the in-memory article window; the DB keeps everything.
    ///
    /// This is the knob that trades memory for completeness: at ~750 bytes per
    /// article the 20 000 default is roughly 15 MiB, which is what lets a
    /// 1 400-feed fleet keep a meaningful unread count per feed.
    pub max_in_memory_articles: usize,
    pub max_concurrent_fetches: usize,
    pub proxy: Option<String>,
    /// Extra headers sent with every request.
    pub headers: BTreeMap<String, String>,
    /// Stretch the interval of feeds that keep returning 304 (up to `quiet_factor`×).
    pub quiet_factor: u8,
    /// Enable the SQLite FTS5 index (costs a little write amplification).
    pub full_text_search: bool,
    /// Save the original page locally the first time an article is opened in
    /// reading mode (SingleFile-style: CSS, images and fonts inlined into one
    /// self-contained HTML file). Archived pages then render offline.
    pub archive_on_open: bool,
    /// Cap on the archived document (the page plus everything inlined into it).
    pub archive_max_bytes: u64,
    /// Cap on any single sub-resource inlined into an archive.
    pub archive_max_resource_bytes: u64,
    /// Cap on how many sub-resources one page may pull in.
    pub archive_max_resources: usize,
    /// Verbose engine logging.
    pub verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: None,
            user_agent: d_user_agent(),
            accept_language: "en,zh;q=0.9,*;q=0.5".into(),
            global_concurrency: 16,
            per_host_concurrency: 2,
            request_timeout_secs: 20,
            connect_timeout_secs: 5,
            host_min_gap_ms: 250,
            min_interval_secs: 300,
            max_interval_secs: 86_400,
            default_interval_secs: 1_800,
            jitter_percent: 10,
            startup_spread_secs: 25,
            max_entries_per_fetch: 200,
            max_body_bytes: 16 * 1024 * 1024,
            preview_chars: 420,
            max_content_bytes: 96 * 1024,
            retention_days: 120,
            max_articles: 200_000,
            max_in_memory_articles: 20_000,
            max_concurrent_fetches: 24,
            proxy: None,
            headers: BTreeMap::new(),
            quiet_factor: 4,
            full_text_search: true,
            archive_on_open: true,
            archive_max_bytes: 12 * 1024 * 1024,
            archive_max_resource_bytes: 2 * 1024 * 1024,
            archive_max_resources: 80,
            verbose: false,
        }
    }
}

impl Settings {
    /// Where self-contained page archives are written. Kept beside the database
    /// so the whole application state is one directory.
    pub fn archive_dir(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("archives"))
            .unwrap_or_else(|| PathBuf::from("archives"))
    }

    pub fn archive_budget(&self) -> crate::archive::ArchiveBudget {
        crate::archive::ArchiveBudget {
            max_page_bytes: self.archive_max_bytes,
            max_resource_bytes: self.archive_max_resource_bytes,
            max_total_bytes: self.archive_max_bytes,
            max_resources: self.archive_max_resources,
            per_host: self.per_host_concurrency.max(1),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        match &self.db_path {
            Some(p) => expand_tilde(p),
            None => default_data_dir().join("artifeeds.db"),
        }
    }
}

/// Per-source overrides. Every field is optional so the same struct can express
/// `[group_defaults.News]` and a concrete `[[sources]]` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialSource {
    pub title: Option<String>,
    pub url: Option<String>,
    pub enabled: Option<bool>,
    pub group: Option<String>,
    pub tags: Option<Vec<String>>,
    pub interval_secs: Option<u64>,
    pub headers: Option<BTreeMap<String, String>>,
    pub user_agent: Option<String>,
    pub max_entries: Option<usize>,
    pub key: Option<String>,
}

/// A `[[sources]]` entry. `url` is the only required field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub max_entries: Option<usize>,
    /// Stable identity. Defaults to the normalised URL; set it explicitly when a
    /// URL changes (moved blog) so history and read-state survive.
    #[serde(default)]
    pub key: Option<String>,
    /// Which file this entry came from. Not part of the schema.
    #[serde(skip)]
    pub origin_hint: Option<String>,
}

/// One parsed input file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub settings: Settings,
    /// `true` when the file actually carried a `[settings]` table. Included
    /// files may only contribute sources; the root file wins for settings.
    #[serde(skip)]
    pub has_settings: bool,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    /// Files or directories, resolved relative to this file.
    #[serde(default)]
    pub include: Vec<String>,
    /// Interval/headers/UA applied to every source in this file (and includes).
    #[serde(default)]
    pub defaults: PartialSource,
    /// Per-group overrides: `[group_defaults.News] interval_secs = 600`.
    #[serde(default)]
    pub group_defaults: BTreeMap<String, PartialSource>,
    #[serde(default)]
    pub opml: Vec<String>,
    /// Subscriptions the reader removed, by URL or key.
    ///
    /// Generated import files (`tech.toml`, an OPML from `suggest`) are
    /// machine-owned and get rewritten whenever they are regenerated or the app
    /// is rebuilt. A deletion recorded only inside one of those would come back
    /// with the next build, so it is recorded here, in the file the reader owns.
    #[serde(default)]
    pub removed: Vec<String>,
}

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

/// A fully resolved, immutable source description.
#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub id: SourceId,
    pub key: Arc<str>,
    pub url: Arc<str>,
    pub host: Arc<str>,
    pub title: Arc<str>,
    pub group: Arc<str>,
    pub tags: Arc<[Arc<str>]>,
    pub interval: Duration,
    pub headers: Arc<[(Arc<str>, Arc<str>)]>,
    pub user_agent: Option<Arc<str>>,
    pub max_entries: usize,
    pub enabled: bool,
    pub url_hash: u64,
    /// Changes whenever anything that affects fetching changes.
    pub fingerprint: u64,
    /// Stable hash used to spread first poll across the interval.
    pub spread: u64,
    pub origin: Arc<str>,
}

impl SourceSpec {
    /// First wake-up delay: staggered across a short bootstrap window so 3 000
    /// feeds do not all fire in the same second after launch, but the window is
    /// short enough that the reader fills within seconds rather than minutes.
    pub fn initial_delay(&self, startup_spread_secs: u64) -> Duration {
        let span = startup_spread_secs.max(1).min(self.interval.as_secs().max(1));
        let offset = (self.spread as f64 / 1000.0) * span as f64;
        Duration::from_secs_f64(offset.clamp(0.0, span as f64))
    }
}

/// Fully compiled configuration. Cheap to clone (one `Arc` copy of the slice).
#[derive(Debug, Clone)]
pub struct CompiledConfig {
    pub settings: Settings,
    pub sources: Arc<[Arc<SourceSpec>]>,
    pub by_key: FxHashMap<Arc<str>, SourceId>,
    pub by_url_hash: FxHashMap<u64, SourceId>,
    /// Distinct files that produced this config (used for watching + reporting).
    pub files: Arc<[PathBuf]>,
    pub watch_dirs: Arc<[PathBuf]>,
    pub problems: Arc<[ConfigProblem]>,
    /// Hash of the whole compiled set; unchanged ⇒ reload is a no-op.
    pub fingerprint: u64,
    pub loaded_at: i64,
}

impl CompiledConfig {
    pub fn source(&self, id: SourceId) -> Option<&Arc<SourceSpec>> {
        self.sources.get(id as usize)
    }

    pub fn enabled_count(&self) -> usize {
        self.sources.iter().filter(|s| s.enabled).count()
    }

    /// Number of distinct groups. Computed on demand (cheap: one pass over the
    /// already-interned `Arc<str>` groups).
    pub fn groups_count(&self) -> usize {
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        for s in self.sources.iter() {
            seen.insert(s.group.as_ref());
        }
        seen.len()
    }

    /// Key-level diff between two compiled configs. The engine uses this to
    /// apply a reload incrementally instead of rebuilding all runtime state.
    pub fn delta_keys(&self, next: &CompiledConfig) -> ConfigDelta {
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        for (key, old_id) in &self.by_key {
            match next.by_key.get(key) {
                None => removed.push(*old_id),
                Some(new_id) => {
                    let a = &self.sources[*old_id as usize];
                    let b = &next.sources[*new_id as usize];
                    if a.fingerprint != b.fingerprint {
                        changed.push((*old_id, *new_id));
                    }
                }
            }
        }
        for (key, new_id) in &next.by_key {
            if !self.by_key.contains_key(key) {
                added.push(*new_id);
            }
        }
        ConfigDelta { added, removed, changed }
    }
}

/// Which sources a reload touched, expressed in old/new dense ids.
#[derive(Debug, Clone, Default)]
pub struct ConfigDelta {
    pub added: Vec<SourceId>,
    pub removed: Vec<SourceId>,
    pub changed: Vec<(SourceId, SourceId)>,
}

impl ConfigDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Toml,
    Opml,
}

fn classify(path: &Path) -> Option<InputKind> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "toml" => Some(InputKind::Toml),
        "opml" | "xml" => Some(InputKind::Opml),
        _ => None,
    }
}

/// Outcome of a load. Problems are advisory: the config still loads.
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub config: Arc<CompiledConfig>,
    pub problems: Vec<ConfigProblem>,
}

/// Read, compose and compile the configuration rooted at `path`.
pub fn load(path: &Path) -> LoadOutcome {
    let mut problems = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut watch_dirs: Vec<PathBuf> = Vec::new();
    let mut seen_files: FxHashSet<PathBuf> = FxHashSet::default();

    let mut sources: Vec<SourceConfig> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    // `defaults` / `group_defaults` accumulate across files; later files win.
    let mut defaults = PartialSource::default();
    let mut group_defaults: BTreeMap<String, PartialSource> = BTreeMap::new();
    let mut settings: Option<Settings> = None;

    // Breadth-first over the include graph, cycle-safe.
    let mut queue: Vec<(PathBuf, bool)> = vec![(path.to_path_buf(), true)];
    while let Some((file, is_root)) = queue.pop() {
        let canonical = file.canonicalize().unwrap_or_else(|_| file.clone());
        if !seen_files.insert(canonical.clone()) {
            continue;
        }
        if let Some(dir) = file.parent() {
            let dir = dir.to_path_buf();
            if !watch_dirs.contains(&dir) {
                watch_dirs.push(dir);
            }
        }
        files.push(file.clone());
        let Some(text) = read_text(&file, &mut problems) else {
            continue;
        };
        match classify(&file) {
            Some(InputKind::Opml) => {
                let (parsed, skipped) = parse_opml(&text, &file);
                if skipped > 0 {
                    problems.push(problem(&file, format!("{skipped} OPML outlines had no xmlUrl")));
                }
                sources.extend(parsed);
            }
            _ => {
                let parsed: FileConfig = match toml::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        problems.push(problem(&file, explain_toml_error(&text, &e.to_string())));
                        continue;
                    }
                };
                // Only the root file is authoritative for `[settings]`; an
                // included file may supply them when the root has none.
                if is_root || (settings.is_none() && text.contains("[settings]")) {
                    settings = Some(parsed.settings.clone());
                }
                merge_partial(&mut defaults, &parsed.defaults);
                for (g, p) in &parsed.group_defaults {
                    match group_defaults.get_mut(g) {
                        Some(existing) => merge_partial(existing, p),
                        None => {
                            group_defaults.insert(g.clone(), p.clone());
                        }
                    }
                }
                for mut s in parsed.sources {
                    s.set_origin(&file);
                    sources.push(s);
                }
                removed.extend(parsed.removed.iter().cloned());
                let base = file.parent().unwrap_or(Path::new("."));
                for inc in parsed.include.iter().chain(parsed.opml.iter()) {
                    for resolved in resolve_include(base, inc, &mut problems) {
                        queue.push((resolved, false));
                    }
                }
            }
        }
    }

    let settings = settings.unwrap_or_default();
    let config = compile(
        settings,
        sources,
        &removed,
        &defaults,
        &group_defaults,
        files,
        watch_dirs,
        &mut problems,
    );
    LoadOutcome { config: Arc::new(config), problems }
}

impl ConfigStore {
    /// Where a source lives: the `url` that identifies it inside the file, and
    /// the file itself. Used by the delete path, which edits that file as text.
    pub fn source_origin(&self, id: SourceId) -> Option<(Arc<str>, Arc<str>)> {
        let config = self.current();
        let spec = config.sources.get(id as usize)?;
        Some((Arc::clone(&spec.url), Arc::clone(&spec.origin)))
    }
}

impl SourceConfig {
    fn set_origin(&mut self, file: &Path) {
        self.origin_hint = Some(file.to_string_lossy().into_owned());
    }
    fn origin_str(&self) -> Arc<str> {
        match &self.origin_hint {
            Some(s) => Arc::from(s.as_str()),
            None => Arc::from("<inline>"),
        }
    }
}

/// Turn a TOML parse error into something actionable.
///
/// The single most common mistake in this format is putting `include` (or any
/// other document-level key) *after* a `[table]` header, where TOML assigns it
/// to that table. The raw error is correct but cryptic, so add the hint.
fn explain_toml_error(text: &str, raw: &str) -> String {
    if raw.contains("unknown field") && text.contains("include") {
        let first_table = text.find('[').unwrap_or(usize::MAX);
        let include_before = text[..first_table.min(text.len())].contains("include");
        if !include_before {
            return format!(
                "{raw}\n  hint: document-level keys such as `include` must appear                  before the first [table] header in TOML."
            );
        }
    }
    raw.to_string()
}

fn problem(file: &Path, message: impl Into<String>) -> ConfigProblem {
    ConfigProblem {
        file: Arc::from(file.to_string_lossy().as_ref()),
        message: Arc::from(message.into().into_boxed_str()),
    }
}

fn read_text(file: &Path, problems: &mut Vec<ConfigProblem>) -> Option<String> {
    match std::fs::read_to_string(file) {
        Ok(t) => Some(t),
        Err(e) => {
            problems.push(problem(file, format!("cannot read: {e}")));
            None
        }
    }
}

fn resolve_include(base: &Path, raw: &str, problems: &mut Vec<ConfigProblem>) -> Vec<PathBuf> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        base.join(raw)
    };
    if candidate.is_dir() {
        let mut found: Vec<PathBuf> = match std::fs::read_dir(&candidate) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && classify(p).is_some())
                .collect(),
            Err(e) => {
                problems.push(problem(&candidate, format!("cannot list directory: {e}")));
                return Vec::new();
            }
        };
        found.sort();
        found
    } else if candidate.is_file() {
        vec![candidate]
    } else {
        problems.push(problem(&candidate, "include target not found"));
        Vec::new()
    }
}

fn merge_partial(dst: &mut PartialSource, src: &PartialSource) {
    macro_rules! take {
        ($($f:ident),*) => { $( if src.$f.is_some() { dst.$f = src.$f.clone(); } )* };
    }
    take!(title, url, enabled, group, tags, interval_secs, headers, user_agent, max_entries);
}

// ---------------------------------------------------------------------------
// OPML
// ---------------------------------------------------------------------------

/// Minimal, allocation-light OPML reader. OPML in the wild is `<outline>` tags
/// with `xmlUrl`/`title`/`text` attributes — a full XML tree is unnecessary.
pub fn parse_opml(text: &str, file: &Path) -> (Vec<SourceConfig>, usize) {
    let origin = file.to_string_lossy().into_owned();
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut cursor = 0usize;
    // OPML organises subscriptions by nesting: a folder has no `xmlUrl` and its
    // children inherit it as their group. Without this an export loses exactly
    // the structure its author took the trouble to make — the AI folder in
    // `awesome-rsshub-routes` being the obvious case.
    let mut folders: Vec<String> = Vec::new();
    while let Some(rel) = text[cursor..].find("<outline") {
        let start = cursor + rel;
        let Some(end) = text[start..].find('>').map(|e| start + e) else {
            break;
        };
        let tag = &text[start..end];
        cursor = end + 1;
        let self_closing = tag.trim_end().ends_with('/');
        let xml_url = attr(tag, "xmlUrl").or_else(|| attr(tag, "xmlurl"));
        let Some(url) = xml_url else {
            // No `xmlUrl` means either a folder — which is structure, not
            // breakage — or an outline with nothing in it at all, which is worth
            // reporting.
            let label = attr(tag, "title")
                .or_else(|| attr(tag, "text"))
                .map(|t| decode_attr(&t).trim().to_string())
                .filter(|t| !t.is_empty());
            if label.is_none() {
                skipped += 1;
            }
            if !self_closing {
                folders.push(label.unwrap_or_default());
            }
            continue;
        };
        if url.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let title = attr(tag, "title")
            .or_else(|| attr(tag, "text"))
            .map(|t| decode_attr(&t));
        let mut s = SourceConfig {
            url: decode_attr(&url),
            title,
            enabled: true,
            // `category` wins when present; otherwise the nearest non-empty
            // folder above this outline is the group.
            group: attr(tag, "category")
                .map(|c| decode_attr(&c))
                .or_else(|| folders.iter().rev().find(|f| !f.is_empty()).cloned()),
            tags: Vec::new(),
            interval_secs: None,
            headers: BTreeMap::new(),
            user_agent: None,
            max_entries: None,
            key: None,
            origin_hint: None,
        };
        s.origin_hint = Some(origin.clone());
        out.push(s);
        if !self_closing {
            // A subscription can also be a folder; it still has to be popped.
            folders.push(String::new());
        }
    }
    // Close tags pop whatever is open, in document order.
    for tag in text.split("</outline>").skip(1) {
        let _ = tag;
        folders.pop();
    }
    (out, skipped)
}

/// Remove one subscription from an OPML file's text, by its `xmlUrl`.
///
/// The counterpart of [`remove_source_from_toml`] for the other format the
/// loader reads. Only self-closing feed outlines are removed: an outline with
/// children is a *folder*, and deleting it would take its subscriptions with it,
/// so the caller is told nothing could be done instead.
pub fn remove_source_from_opml(text: &str, url: &str) -> Option<String> {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("<outline") {
        let start = search + rel;
        let end = text[start..].find('>').map(|offset| start + offset)?;
        let tag = &text[start..=end];
        let listed = attr(tag, "xmlUrl")
            .or_else(|| attr(tag, "xmlurl"))
            .map(|value| decode_attr(&value));
        // Compared the way the loader compares: the file may spell the address
        // with a trailing slash, in a different case, or with an entity, and the
        // loader normalises all of that away — so removal has to as well, or it
        // reports "not found" for a subscription that is plainly there.
        if listed
            .as_deref()
            .is_some_and(|value| normalize_url(value) == normalize_url(url))
        {
            if !tag.trim_end().ends_with("/>") {
                return None;
            }
            // Take the whole line when the outline has one to itself, so removing
            // a feed does not leave a blank line behind; otherwise just the tag.
            let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = text[end..].find('\n').map(|i| end + i + 1).unwrap_or(text.len());
            let alone_on_its_line = text[line_start..start].trim().is_empty()
                && text[end + 1..line_end].trim().is_empty();
            let (cut_from, cut_to) = if alone_on_its_line {
                (line_start, line_end)
            } else {
                (start, end + 1)
            };
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..cut_from]);
            out.push_str(&text[cut_to..]);
            return Some(out);
        }
        search = end + 1;
    }
    None
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(rel) = tag[from..].find(name) {
        let at = from + rel;
        // must be preceded by whitespace and followed by '='
        let prev_ok = at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace();
        let after = &tag[at + name.len()..];
        let after_trim = after.trim_start();
        if prev_ok && after_trim.starts_with('=') {
            let rest = after_trim[1..].trim_start();
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let body = &rest[1..];
                let end = body.find(quote)?;
                return Some(body[..end].to_string());
            }
        }
        from = at + name.len();
    }
    None
}

fn decode_attr(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    crate::text::html_to_text_into(&s.replace('<', "&lt;"), &mut out, 4096);
    out
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn compile(
    settings: Settings,
    raw_sources: Vec<SourceConfig>,
    removed: &[String],
    defaults: &PartialSource,
    group_defaults: &BTreeMap<String, PartialSource>,
    files: Vec<PathBuf>,
    watch_dirs: Vec<PathBuf>,
    problems: &mut Vec<ConfigProblem>,
) -> CompiledConfig {
    let mut specs: Vec<Arc<SourceSpec>> = Vec::with_capacity(raw_sources.len());
    let mut by_key: FxHashMap<Arc<str>, SourceId> = FxHashMap::default();
    let mut by_url_hash: FxHashMap<u64, SourceId> = FxHashMap::default();
    let mut group_index: FxHashMap<Arc<str>, u32> = FxHashMap::default();

    let min_i = settings.min_interval_secs.max(30);
    let max_i = settings.max_interval_secs.max(min_i);

    // The reader's removals, checked before anything else so a regenerated
    // import file cannot resurrect a subscription they deleted.
    let removed_urls: std::collections::HashSet<String> =
        removed.iter().map(|url| normalize_url(url.trim())).collect();
    let removed_keys: std::collections::HashSet<&str> =
        removed.iter().map(|url| url.trim()).collect();

    for raw in raw_sources.iter() {
        let url = raw.url.trim();
        if removed_urls.contains(&normalize_url(url))
            || raw.key.as_deref().map(str::trim).is_some_and(|k| removed_keys.contains(k))
        {
            tracing::debug!(dropped = %url, "subscription removed by the reader");
            continue;
        }
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            problems.push(ConfigProblem {
                file: Arc::from(raw.origin_str().as_ref()),
                message: Arc::from(format!("skipped source (url must be http/https): {url:?}")),
            });
            continue;
        }
        let normalized = normalize_url(url);
        let url_hash = hash64(normalized.as_bytes());
        if let Some(existing) = by_url_hash.get(&url_hash) {
            // Not a problem to report: an imported OPML routinely lists the same
            // feed under two folders, and two of the reader's own files may name
            // it once each. Keeping the first is the resolution, and it is not
            // something anybody can act on — so it goes to the log, not to the
            // status bar.
            tracing::debug!(
                kept = %specs[*existing as usize].url,
                dropped = %raw.url,
                "duplicate feed url; keeping the first"
            );
            continue;
        }
        let key: Arc<str> = match raw.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => Arc::from(k),
            None => Arc::from(normalized.as_str()),
        };
        if by_key.contains_key(&key) {
            problems.push(ConfigProblem {
                file: Arc::from(raw.origin_str().as_ref()),
                message: Arc::from(format!("duplicate source key {key:?}, skipped")),
            });
            continue;
        }

        let group_src = raw
            .group
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(|g| group_defaults.get(g));

        let pick = |f: fn(&PartialSource) -> Option<u64>| -> Option<u64> {
            raw.interval_secs
                .or_else(|| group_src.flatten().and_then(f))
                .or_else(|| f(defaults))
        };
        let interval =
            pick(|p| p.interval_secs).unwrap_or(settings.default_interval_secs).clamp(min_i, max_i);

        let mut headers: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        for map in [
            defaults.headers.as_ref(),
            group_src.flatten().and_then(|p| p.headers.as_ref()),
            Some(&raw.headers),
        ]
        .into_iter()
        .flatten()
        {
            for (k, v) in map {
                let k = k.to_ascii_lowercase();
                if let Some(slot) = headers.iter_mut().find(|(hk, _)| hk.as_ref() == k) {
                    slot.1 = Arc::from(v.as_str());
                } else {
                    headers.push((Arc::from(k.as_str()), Arc::from(v.as_str())));
                }
            }
        }
        let user_agent = raw
            .user_agent
            .clone()
            .or_else(|| group_src.flatten().and_then(|p| p.user_agent.clone()))
            .or_else(|| defaults.user_agent.clone())
            .map(|s| Arc::from(s.as_str()));
        let max_entries = raw
            .max_entries
            .or(group_src.flatten().and_then(|p| p.max_entries))
            .or(defaults.max_entries)
            .unwrap_or(settings.max_entries_per_fetch);
        let tags: Arc<[Arc<str>]> = raw
            .tags
            .iter()
            .chain(group_src.flatten().and_then(|p| p.tags.as_ref()).into_iter().flatten())
            .chain(defaults.tags.iter().flatten())
            .map(|t| Arc::<str>::from(t.trim()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .into();

        let group: Arc<str> = match raw.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
            Some(g) => Arc::from(g),
            None => {
                // Fall back to the hostname so ungrouped sources still cluster.
                let host = host_of(&normalized);
                Arc::from(host.as_str())
            }
        };
        group_index.entry(group.clone()).or_insert(0);
        let title: Arc<str> = match raw.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Arc::from(t),
            None => Arc::from(host_of(&normalized).as_str()),
        };

        let mut fp = hash64(key.as_bytes());
        for chunk in [
            normalized.as_str(),
            title.as_ref(),
            group.as_ref(),
            user_agent.as_deref().unwrap_or(""),
        ] {
            fp = fp.rotate_left(7) ^ hash64(chunk.as_bytes());
        }
        fp = fp.rotate_left(7) ^ interval;
        fp = fp.rotate_left(7) ^ (raw.enabled as u64);
        fp = fp.rotate_left(7) ^ max_entries as u64;
        for (k, v) in &headers {
            fp = fp.rotate_left(7) ^ hash64(k.as_bytes()) ^ hash64(v.as_bytes());
        }
        for t in tags.iter() {
            fp = fp.rotate_left(7) ^ hash64(t.as_bytes());
        }

        by_key.insert(key.clone(), specs.len() as SourceId);
        by_url_hash.insert(url_hash, specs.len() as SourceId);
        specs.push(Arc::new(SourceSpec {
            id: specs.len() as SourceId,
            key,
            url: Arc::from(normalized.as_str()),
            host: Arc::from(host_of(&normalized).as_str()),
            title,
            group,
            tags,
            interval: Duration::from_secs(interval),
            headers: headers.into(),
            user_agent,
            max_entries,
            enabled: raw.enabled,
            url_hash,
            fingerprint: fp,
            spread: spread(&normalized),
            origin: raw.origin_str(),
        }));
    }

    let mut overall: u64 = hash64(b"artifeeds-config-v1");
    for s in &specs {
        overall = overall.rotate_left(5) ^ s.fingerprint;
    }
    overall = overall.rotate_left(5) ^ hash64(settings.user_agent.as_bytes());
    overall = overall.rotate_left(5) ^ settings.global_concurrency as u64;

    // Dense ids equal the index in `specs` by construction (see `specs.len()`
    // above); nothing to renumber.

    CompiledConfig {
        settings,
        sources: specs.into(),
        by_key,
        by_url_hash,
        files: files.into(),
        watch_dirs: watch_dirs.into(),
        problems: problems.clone().into(),
        fingerprint: overall,
        loaded_at: now_unix(),
    }
}

/// Lowercase the scheme/host, drop the fragment, drop a trailing slash, and
/// strip the tracking parameters that make the same feed appear twice.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => (String::new(), trimmed),
    };
    let (authority, path_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = authority.to_ascii_lowercase();
    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_query, None),
    };
    let mut path = path.to_string();
    while path.ends_with('/') && path.len() > 1 {
        path.pop();
    }
    if path.is_empty() {
        path.push('/');
    }
    let query = query.map(|q| {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|kv| {
                let k = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
                !matches!(
                    k.as_str(),
                    "utm_source" | "utm_medium" | "utm_campaign" | "utm_term" | "utm_content" | "fbclid" | "gclid"
                )
            })
            .collect();
        kept.join("&")
    });
    match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("{scheme}://{authority}{path}?{q}"),
        None => format!("{scheme}://{authority}{path}"),
    }
}

/// A feed being added: what to write into the configuration.
#[derive(Debug, Clone)]
pub struct NewSource {
    pub title: String,
    pub url: String,
    pub group: String,
    pub tags: Vec<String>,
}

/// Append a `[[sources]]` block to a TOML file's text.
///
/// Text again, for the same reason removal is: these files are generated with
/// comments and aligned keys, and a round-trip through a TOML serialiser would
/// discard all of it. The URL doubles as the `key`, because that is what the
/// loader uses to identify a source.
pub fn append_source_to_toml(text: &str, source: &NewSource) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("[[sources]]\n");
    out.push_str(&format!("title = {}\n", toml_string(&source.title)));
    out.push_str(&format!("url   = {}\n", toml_string(&source.url)));
    out.push_str(&format!("group = {}\n", toml_string(&source.group)));
    if !source.tags.is_empty() {
        let tags: Vec<String> = source.tags.iter().map(|t| toml_string(t)).collect();
        out.push_str(&format!("tags  = [{}]\n", tags.join(", ")));
    }
    out.push_str(&format!("key   = {}\n", toml_string(&source.url)));
    out
}

/// A TOML basic string: escaped, which matters because titles contain quotes and
/// backslashes often enough to produce a file that no longer parses.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Remove one `[[sources]]` block from a TOML file's text, by its `url`.
///
/// Edited as text rather than re-serialised, because these files are generated
/// with comments and aligned keys and a round-trip through `toml::to_string`
/// would throw all of that away. Returns `None` when no block carries that URL,
/// so a caller cannot silently rewrite a file it did not understand.
pub fn remove_source_from_toml(text: &str, url: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let is_header = |line: &str| {
        let t = line.trim_start();
        t.starts_with("[[") || t.starts_with('[')
    };
    let url_of = |line: &str| -> Option<String> {
        let t = line.trim();
        let rest = t.strip_prefix("url")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let value = rest.trim_end();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))?;
        Some(value.to_string())
    };

    let want = normalize_url(url);
    let hit = lines
        .iter()
        .position(|line| url_of(line).is_some_and(|u| normalize_url(&u) == want))?;
    // The block starts at its own `[[sources]]` header...
    let start = lines[..hit].iter().rposition(|line| line.trim() == "[[sources]]")?;
    // ...and ends at the next header of any kind.
    let mut end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| is_header(line))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    // Swallow the blank line that separated it from the next block, so deleting
    // a feed does not leave a growing gap behind.
    while end < lines.len() && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut out: Vec<&str> = lines[..start].to_vec();
    let tail = &lines[end..];
    // The block was preceded by a blank line, and the tail still carries the one
    // that separated it from the next block: keep exactly one, not two.
    if out.last().is_some_and(|l| l.trim().is_empty())
        && tail.first().is_some_and(|l| l.trim().is_empty())
    {
        out.pop();
    }
    out.extend_from_slice(tail);
    let mut result = out.join("\n");
    // Removing the last block can leave the blank line that separated it: a file
    // ends with one newline, not a blank line.
    while result.ends_with("\n\n") {
        result.pop();
    }
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_start_matches("www.").to_ascii_lowercase()
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(p)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ConfigStore: current config + hot reload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ConfigEvent {
    /// A reload produced a different fingerprint.
    Reloaded { fingerprint: u64, sources: usize, problems: usize },
    /// A reload happened but the content was identical.
    Unchanged,
    /// The watcher could not be installed; the app keeps running.
    WatchFailed(Arc<str>),
}

/// Holds the live configuration and reloads it in the background.
pub struct ConfigStore {
    path: PathBuf,
    current: ArcSwap<CompiledConfig>,
    problems: ArcSwap<Vec<ConfigProblem>>,
    reloads: AtomicU64,
}

impl ConfigStore {
    /// Load once; never fails — a broken config yields an empty, valid config
    /// plus problems so the UI can show what went wrong.
    pub fn new(path: impl Into<PathBuf>) -> Arc<Self> {
        let path = path.into();
        let outcome = if path.exists() {
            load(&path)
        } else {
            LoadOutcome {
                config: Arc::new(empty_config(&path)),
                problems: vec![problem(&path, "config file not found (using defaults)")],
            }
        };
        Arc::new(Self {
            path,
            current: ArcSwap::from(outcome.config),
            problems: ArcSwap::from_pointee(outcome.problems),
            reloads: AtomicU64::new(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    pub fn load_config(&self) -> arc_swap::Guard<Arc<CompiledConfig>> {
        self.current.load()
    }

    pub fn current(&self) -> Arc<CompiledConfig> {
        self.current.load_full()
    }

    pub fn problems(&self) -> Arc<Vec<ConfigProblem>> {
        Arc::new(self.problems.load().as_ref().clone())
    }

    pub fn reload_count(&self) -> u64 {
        self.reloads.load(Ordering::Relaxed)
    }

    /// Re-read from disk. Returns `Some` only when something actually changed.
    pub fn reload(&self) -> Option<Arc<CompiledConfig>> {
        if !self.path.exists() {
            return None;
        }
        let outcome = load(&self.path);
        let next = outcome.config;
        let prev = self.current.load();
        self.problems.store(Arc::new(outcome.problems));
        if prev.fingerprint == next.fingerprint {
            return None;
        }
        self.current.store(next.clone());
        self.reloads.fetch_add(1, Ordering::Relaxed);
        Some(next)
    }

    /// Install a debounced watcher. The callback runs on a dedicated thread and
    /// receives exactly one event per settling burst of filesystem activity.
    pub fn spawn_watcher(
        self: &Arc<Self>,
        settle: Duration,
        tx: flume::Sender<ConfigEvent>,
    ) -> std::io::Result<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};

        // Watch the file itself plus every directory that contributed sources,
        // so newly added include files are picked up too.
        let config = self.current();
        let mut roots: Vec<PathBuf> = config.watch_dirs.to_vec();
        let mut watch_dirs = Vec::new();
        for r in roots.drain(..) {
            if r.exists() && !watch_dirs.contains(&r) {
                watch_dirs.push(r);
            }
        }
        if let Some(parent) = self.path.parent()
            && parent.exists()
            && !watch_dirs.contains(&parent.to_path_buf())
        {
            watch_dirs.push(parent.to_path_buf());
        }

        let (raw_tx, raw_rx) = flume::unbounded::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
                )
            {
                let _ = raw_tx.try_send(());
            }
        })
        .map_err(std::io::Error::other)?;
        for dir in &watch_dirs {
            let mode = RecursiveMode::NonRecursive;
            if let Err(e) = watcher.watch(dir, mode) {
                let _ = tx.send(ConfigEvent::WatchFailed(Arc::from(e.to_string().as_str())));
            }
        }

        let store = Arc::clone(self);
        std::thread::Builder::new()
            .name("artifeeds-config-watch".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                while raw_rx.recv().is_ok() {
                    // Debounce: keep draining until the storm stops.
                    loop {
                        match raw_rx.recv_timeout(settle) {
                            Ok(()) => continue,
                            Err(flume::RecvTimeoutError::Timeout) => break,
                            Err(flume::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    let before = store.current().fingerprint;
                    match store.reload() {
                        Some(cfg) => {
                            let _ = tx.send(ConfigEvent::Reloaded {
                                fingerprint: cfg.fingerprint,
                                sources: cfg.sources.len(),
                                problems: cfg.problems.len(),
                            });
                        }
                        None if store.current().fingerprint == before => {
                            let _ = tx.send(ConfigEvent::Unchanged);
                        }
                        None => {}
                    }
                }
            })
            .map_err(std::io::Error::other)?;
        Ok(watcher)
    }
}

fn empty_config(path: &Path) -> CompiledConfig {
    CompiledConfig {
        settings: Settings::default(),
        sources: Arc::from(Vec::new()),
        by_key: FxHashMap::default(),
        by_url_hash: FxHashMap::default(),
        files: Arc::from(vec![path.to_path_buf()]),
        watch_dirs: path.parent().map(|p| vec![p.to_path_buf()]).unwrap_or_default().into(),
        problems: Arc::from(Vec::new()),
        fingerprint: hash64(b"empty"),
        loaded_at: now_unix(),
    }
}

/// Write a commented starter config if none exists yet.
pub fn write_scaffold(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, SCAFFOLD)
}

pub const SCAFFOLD: &str = r#"# Artifeeds configuration.
#
# Split large fleets over multiple files and pull them in with `include`:
#   include = ["feeds/*.toml", "conf.d", "subscriptions.opml"]
# A directory include picks up every *.toml / *.opml / *.xml inside it.

[settings]
global_concurrency   = 16      # in-flight requests, all sources
per_host_concurrency = 2       # in-flight requests per hostname
default_interval_secs = 1800   # 30 min
min_interval_secs     = 300    # floor, honours nothing faster
max_interval_secs     = 86400  # ceiling, even for dead feeds
startup_spread_secs   = 25      # stagger the first poll across this window
max_in_memory_articles = 20000 # UI window; SQLite keeps everything
retention_days         = 120   # prune read articles after this

[[sources]]
title = "Rust Blog"
url   = "https://blog.rust-lang.org/feed.xml"
group = "Rust"
tags  = ["rust", "official"]
interval_secs = 900

[[sources]]
title = "Hacker News (front page)"
url   = "https://hnrss.org/frontpage"
group = "News"
interval_secs = 600

[[sources]]
title = "LWN"
url   = "https://lwn.net/headlines/rss"
group = "News"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn compiles_with_defaults_and_groups() {
        let dir = std::env::temp_dir().join(format!("af-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = write(
            &dir,
            "feeds.toml",
            r#"
[settings]
default_interval_secs = 900
min_interval_secs = 600

[defaults]
headers = { "accept" = "application/rss+xml" }

[group_defaults.News]
interval_secs = 300

[[sources]]
title = "A"
url = "https://Example.com/feed?utm_source=x"
group = "News"

[[sources]]
title = "dup-of-A"
url = "https://example.com/feed"
group = "News"

[[sources]]
title = "bad"
url = "ftp://nope"
"#,
        );
        let out = load(&root);
        let cfg = out.config;
        // ftp:// rejected, and the two URLs normalise to the same feed.
        assert_eq!(cfg.sources.len(), 1, "problems: {:?}", cfg.problems);
        let s = &cfg.sources[0];
        assert_eq!(s.url.as_ref(), "https://example.com/feed");
        assert_eq!(s.host.as_ref(), "example.com");
        // group default of 300s is clamped up to min_interval 600s
        assert_eq!(s.interval.as_secs(), 600);
        // URL-level dedup keeps the first occurrence.
        assert_eq!(s.title.as_ref(), "A");
        assert!(s.headers.iter().any(|(k, v)| k.as_ref() == "accept" && v.as_ref() == "application/rss+xml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn includes_directories_and_opml() {
        let dir = std::env::temp_dir().join(format!("af-inc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = write(
            &dir,
            "feeds.toml",
            r#"
include = ["conf.d", "subs.opml"]

[[sources]]
title = "root"
url = "https://a.example/feed"
"#,
        );
        write(
            &dir,
            "conf.d/extra.toml",
            "[[sources]]\ntitle='extra'\nurl='https://b.example/feed'\n",
        );
        write(
            &dir,
            "subs.opml",
            r#"<?xml version="1.0"?><opml><body>
            <outline text="Tech" title="Tech">
              <outline type="rss" text="C" title="C" xmlUrl="https://c.example/rss" htmlUrl="https://c.example"/>
            </outline></body></opml>"#,
        );
        let out = load(&root);
        assert_eq!(out.config.sources.len(), 3, "problems: {:?}", out.config.problems);
        assert!(out.config.by_key.contains_key("https://c.example/rss"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_is_isolated() {
        let dir = std::env::temp_dir().join(format!("af-brk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = write(
            &dir,
            "feeds.toml",
            "include = [\"conf.d\"]\n[[sources]]\nurl='https://ok.example/feed'\n",
        );
        write(&dir, "conf.d/bad.toml", "this is not toml =====");
        let out = load(&root);
        assert_eq!(out.config.sources.len(), 1);
        assert!(!out.problems.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delta_detects_changes() {
        let dir = std::env::temp_dir().join(format!("af-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = write(
            &dir,
            "feeds.toml",
            "[[sources]]\nurl='https://a.example/feed'\n[[sources]]\nurl='https://b.example/feed'\n",
        );
        let first = load(&root).config;
        write(
            &dir,
            "feeds.toml",
            "[[sources]]\nurl='https://a.example/feed'\ninterval_secs=120\n[[sources]]\nurl='https://c.example/feed'\n",
        );
        let second = load(&root).config;
        let delta = first.delta_keys(&second);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.changed.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn url_normalisation() {
        assert_eq!(normalize_url("HTTPS://Example.COM/Feed/"), "https://example.com/Feed");
        assert_eq!(normalize_url("https://e.com/f?b=2&utm_source=t&a=1"), "https://e.com/f?b=2&a=1");
        assert_eq!(host_of("https://www.example.com:8443/feed"), "example.com");
    }
}

#[cfg(test)]
mod remove_tests {
    use super::remove_source_from_toml;

    const FEEDS: &str = r#"# header comment
[[sources]]
title = "A"
url   = "https://a.example/feed"
group = "技术"

[[sources]]
title = "B"
url   = "https://b.example/feed"
group = "技术"

[[sources]]
title = "C"
url   = "https://c.example/feed"
group = "生活"
"#;

    #[test]
    fn removes_the_middle_block_and_leaves_the_rest_verbatim() {
        let out = remove_source_from_toml(FEEDS, "https://b.example/feed").unwrap();
        assert!(!out.contains("B"), "{out}");
        assert!(out.contains("# header comment"));
        assert!(out.contains("https://a.example/feed"));
        assert!(out.contains("https://c.example/feed"));
        // No accumulating blank gap where the block used to be.
        assert!(!out.contains("\n\n\n"), "{out:?}");
        // Still parseable, and with exactly the expected sources.
        let cfg: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(cfg["sources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn removes_first_and_last_blocks() {
        let first = remove_source_from_toml(FEEDS, "https://a.example/feed").unwrap();
        assert!(first.starts_with("# header comment"));
        assert!(!first.contains("https://a.example/feed"));
        let last = remove_source_from_toml(FEEDS, "https://c.example/feed").unwrap();
        assert!(last.contains("https://b.example/feed"));
        assert!(!last.contains("https://c.example/feed"));
        assert!(last.ends_with('\n'));
    }

    #[test]
    fn unknown_url_changes_nothing() {
        assert!(remove_source_from_toml(FEEDS, "https://nope.example/feed").is_none());
    }

    #[test]
    fn stops_at_the_next_table() {
        let text = "[settings]\ninterval = 900\n\n[[sources]]\ntitle = \"A\"\nurl = \"https://a.example/f\"\n\n[[sources]]\ntitle = \"B\"\nurl = \"https://b.example/f\"\n";
        let out = remove_source_from_toml(text, "https://a.example/f").unwrap();
        assert!(out.contains("[settings]"));
        assert!(out.contains("interval = 900"));
        assert!(out.contains("https://b.example/f"));
    }

    #[test]
    fn single_quoted_urls_are_recognised() {
        let text = "[[sources]]\ntitle = 'A'\nurl = 'https://a.example/f'\n";
        assert!(remove_source_from_toml(text, "https://a.example/f").is_some());
    }
}

#[cfg(test)]
mod append_tests {
    use super::{NewSource, append_source_to_toml, remove_source_from_toml};

    fn source() -> NewSource {
        NewSource {
            title: "少数派的诚实".into(),
            url: "https://cavill.site/feed.xml".into(),
            group: "技术".into(),
            tags: vec!["随笔".into()],
        }
    }

    #[test]
    fn appended_text_still_parses_and_round_trips_through_removal() {
        let text = "# header\n\n[[sources]]\ntitle = \"A\"\nurl = \"https://a.example/f\"\n";
        let added = append_source_to_toml(text, &source());
        let parsed: toml::Value = toml::from_str(&added).unwrap();
        let sources = parsed["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1]["url"].as_str().unwrap(), "https://cavill.site/feed.xml");
        assert_eq!(sources[1]["key"].as_str().unwrap(), "https://cavill.site/feed.xml");
        assert_eq!(sources[1]["tags"].as_array().unwrap().len(), 1);
        // The original file is untouched, comments and all.
        assert!(added.starts_with("# header"));

        // ...and removing what was just added gives the original back.
        let removed = remove_source_from_toml(&added, "https://cavill.site/feed.xml").unwrap();
        assert_eq!(removed, text);
    }

    #[test]
    fn hostile_titles_are_escaped() {
        let nasty = NewSource {
            title: "He said \"hi\" \\ back\\slash".into(),
            url: "https://x.example/feed".into(),
            group: "g".into(),
            tags: vec!["a\"b".into()],
        };
        let added = append_source_to_toml("", &nasty);
        let parsed: toml::Value = toml::from_str(&added).unwrap();
        assert_eq!(parsed["sources"][0]["title"].as_str().unwrap(), "He said \"hi\" \\ back\\slash");
        assert_eq!(parsed["sources"][0]["tags"][0].as_str().unwrap(), "a\"b");
    }

    #[test]
    fn appending_to_an_empty_file_needs_no_leading_blank() {
        let added = append_source_to_toml("", &source());
        assert!(added.starts_with("[[sources]]"));
        assert!(added.ends_with('\n'));
    }
}

#[cfg(test)]
mod opml_removal_tests {
    use super::remove_source_from_opml;

    const FILE: &str = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline text="AI 专题" title="AI 专题">
      <outline type="rss" text="OpenAI" title="OpenAI" xmlUrl="https://openai.com/news/rss.xml"/>
      <outline type="rss" text="DeepMind" title="DeepMind" xmlUrl="https://deepmind.google/blog/rss.xml"/>
    </outline>
    <outline type="rss" text="单个" title="单个" xmlUrl="https://solo.example/feed"/>
  </body>
</opml>
"#;

    #[test]
    fn removes_one_feed_and_leaves_the_rest_byte_identical() {
        let out = remove_source_from_opml(FILE, "https://deepmind.google/blog/rss.xml").unwrap();
        assert!(!out.contains("deepmind"), "the feed is gone");
        assert!(out.contains("https://openai.com/news/rss.xml"), "its sibling stays");
        assert!(out.contains("https://solo.example/feed"));
        assert!(out.contains(r#"text="AI 专题""#), "the folder stays");
        assert_eq!(out.lines().count(), FILE.lines().count() - 1, "no blank line left behind");
        // Everything else is untouched, character for character.
        let stripped: String = out.lines().filter(|l| !l.contains("deepmind")).collect();
        let original: String = FILE.lines().filter(|l| !l.contains("deepmind")).collect();
        assert_eq!(stripped, original);
    }

    #[test]
    fn a_folder_is_refused_rather_than_emptied() {
        // The folder outline has no xmlUrl, so asking to remove it by url finds
        // nothing at all — and asking with a feed that is not there is None.
        assert!(remove_source_from_opml(FILE, "https://nonexistent.example/feed").is_none());
        // A *folder* that carries an xmlUrl and children is refused.
        let nested = r#"<opml><body>
    <outline text="A" xmlUrl="https://a.example/feed">
      <outline text="B" xmlUrl="https://b.example/feed"/>
    </outline>
    </body></opml>"#;
        assert!(remove_source_from_opml(nested, "https://a.example/feed").is_none());
        // ...while its child is removable.
        assert!(remove_source_from_opml(nested, "https://b.example/feed").is_some());
    }
}

/// The URLs in a `removed = [...]` list, wherever the list sits in the file.
fn collect_removed(text: &str) -> Vec<String> {
    let Some(start) = text
        .lines()
        .position(|line| line.trim_start().starts_with("removed") && line.contains('='))
        .map(|index| text.lines().take(index).map(|l| l.len() + 1).sum::<usize>())
    else {
        return Vec::new();
    };
    let Some(open) = text[start..].find('[').map(|i| start + i) else {
        return Vec::new();
    };
    let Some(close) = text[open..].find(']').map(|i| open + i) else {
        return Vec::new();
    };
    text[open..close]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Record that the reader removed a subscription, so no regenerated import file
/// can bring it back.
///
/// Returns `Ok(false)` when it was already recorded.
pub fn append_removed_to_config(path: &Path, url: &str) -> Result<bool, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("空的订阅地址".into());
    }
    let original = std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let mut urls = collect_removed(&original);
    if urls.iter().any(|existing| existing == url) {
        return Ok(false);
    }
    urls.push(url.to_string());
    let line = format!(
        "removed = [{}]",
        urls.iter().map(|u| format!("{u:?}")).collect::<Vec<_>>().join(", ")
    );

    let updated = match original
        .lines()
        .position(|l| l.trim_start().starts_with("removed") && l.contains('='))
    {
        // Rewrite the existing list as one line: simpler to read, and simpler to
        // append to next time.
        Some(index) => {
            let lines: Vec<&str> = original.lines().collect();
            let start: usize = lines[..index].iter().map(|l| l.len() + 1).sum();
            let close = original[start..]
                .find(']')
                .map(|i| start + i + 1)
                .ok_or_else(|| "removed 列表没有闭合的 ]".to_string())?;
            format!("{}{}{}", &original[..start], line, &original[close..])
        }
        // A top-level key has to come before the first table header: after
        // `[[sources]]` it would belong to that source.
        None => {
            let insert_at = original
                .lines()
                .position(|l| l.trim_start().starts_with('['))
                .map(|index| original.lines().take(index).map(|l| l.len() + 1).sum::<usize>())
                .unwrap_or(original.len());
            let mut out = String::with_capacity(original.len() + line.len() + 1);
            out.push_str(&original[..insert_at]);
            out.push_str("# 已删除的订阅：记在这里，这样重新生成导入文件也不会把它们带回来。\n");
            out.push_str(&line);
            out.push('\n');
            out.push_str(&original[insert_at..]);
            out
        }
    };

    let backup = path.with_extension("toml.bak");
    if !backup.exists() {
        std::fs::write(&backup, &original).map_err(|e| format!("写备份失败：{e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(true)
}

#[cfg(test)]
mod removed_tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("artifeeds-removed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A generated import file is machine-owned: the app rebuilds it, the
    /// importer rewrites it. A deletion kept only inside one of those comes back,
    /// which is the bug this exists to prevent.
    #[test]
    fn a_removed_subscription_is_not_loaded_even_when_the_file_still_lists_it() {
        let dir = temp_dir("filter");
        std::fs::write(
            dir.join("imported.toml"),
            "[[sources]]\ntitle = \"Keep\"\nurl = \"https://keep.example/feed\"\n\n\
             [[sources]]\ntitle = \"Drop\"\nurl = \"http://proxy.example/ruanyifeng\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("feeds.toml"),
            format!(
                "include = [\"{}\"]\nremoved = [\"http://proxy.example/ruanyifeng\"]\n",
                dir.display()
            ),
        )
        .unwrap();

        let outcome = load(&dir.join("feeds.toml"));
        let urls: Vec<&str> = outcome.config.sources.iter().map(|s| s.url.as_ref()).collect();
        assert_eq!(urls, vec!["https://keep.example/feed"]);
        assert!(outcome.problems.is_empty(), "a removal is not a problem");
    }

    #[test]
    fn recording_a_removal_is_idempotent_and_survives_a_table_header() {
        let dir = temp_dir("append");
        let config = dir.join("feeds.toml");
        std::fs::write(
            &config,
            "[settings]\n\n[[sources]]\ntitle = \"Mine\"\nurl = \"https://mine.example/feed\"\n",
        )
        .unwrap();

        assert!(append_removed_to_config(&config, "https://a.example/feed").unwrap());
        assert!(!append_removed_to_config(&config, "https://a.example/feed").unwrap(), "recorded once");
        assert!(append_removed_to_config(&config, "https://b.example/feed").unwrap());

        let text = std::fs::read_to_string(&config).unwrap();
        // The key must be top-level: after a table header it would belong to the
        // source, and the loader would never see it.
        let removed_at = text.find("removed = [").unwrap();
        let first_table = text.find('[').unwrap();
        assert!(removed_at <= first_table, "removed is written before the first table:\n{text}");
        assert!(text.contains("a.example") && text.contains("b.example"));
        // ...and the file still parses.
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["removed"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["sources"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod removal_matching_tests {
    use super::*;

    /// The file's spelling and the loaded URL are not the same string: the loader
    /// normalises a trailing slash away, and an OPML may write an entity. Removal
    /// compares the way the loader does, so "not found" cannot happen for a
    /// subscription the reader can see in the list.
    #[test]
    fn removal_matches_through_normalisation() {
        let opml = r#"<opml><body>
    <outline type="rss" text="Spotify" title="Spotify" xmlUrl="http://labs.spotify.com/feed/"/>
    </body></opml>"#;
        assert!(
            remove_source_from_opml(opml, "http://labs.spotify.com/feed").is_some(),
            "trailing slash"
        );

        // Scheme and host are case-insensitive, a path is not: this is the same
        // subscription spelled differently, while `/Feed` and `/feed` are not.
        let toml_text = "[[sources]]\ntitle = \"S\"\nurl   = \"HTTP://Example.com/feed/\"\nkey = \"k\"\n";
        assert!(
            remove_source_from_toml(toml_text, "http://example.com/feed").is_some(),
            "scheme and host case, plus a trailing slash"
        );
        // ...and it still refuses a subscription that is genuinely not there.
        assert!(remove_source_from_toml(toml_text, "http://other.example/feed").is_none());
    }
}

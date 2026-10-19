//! The GPUI desktop shell.
//!
//! ## Rendering contract
//!
//! `render` is called on every frame (~60 fps while interacting), so it does
//! three things and nothing else:
//!
//! 1. reads the current immutable [`Snapshot`] (one atomic load),
//! 2. renders **only** the rows the viewport can see (`uniform_list`),
//! 3. returns an element tree.
//!
//! Everything expensive happens elsewhere: filtering happens when the snapshot
//! generation or the scope changes, status text is formatted once per snapshot,
//! and article bodies are loaded lazily from SQLite only for the selected item.

use artifeeds_core::discover;
use crate::theme;
use crate::type_scale::{self, Typography};
#[cfg(target_os = "macos")]
use crate::webview::{self, WebView};
use artifeeds_core::engine::{Engine, EngineEvent, UiCommand};
use artifeeds_core::model::{Article, Health, Snapshot, SourceId};
use artifeeds_core::text::human_bytes;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::Sizable;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::*;
use gpui_kit::*;

gpui_kit::actions!(artifeeds, [FocusArticleSearch, FocusSidebarFilter]);
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SIDEBAR_W: f32 = 258.0;
/// Fixed width of the trailing count column in the sidebar, and of the age
/// column in the list: numbers and ages then line up instead of wandering.
const SIDE_COUNT_W: f32 = 52.0;
/// The sidebar's marker column — health dot or fold arrow.
const SIDE_MARKER_W: f32 = 12.0;
/// Padding at the right end of a sidebar row, and the width of each of the two
/// actions that live there. The hit zones are computed from these, so the
/// glyphs and the zones cannot drift apart.
const SIDE_RAIL_W: f32 = 10.0;
const SIDE_ACTION_W: f32 = 17.0;
/// A row is laid out, right to left, as `[label][★][✕][count]`, separated by the
/// row's own `gap`. The hit zones are derived from that layout, gaps included —
/// the first version measured them from the row's right edge and pointed at the
/// count column, and the second forgot the gaps, which left only the right-hand
/// sliver of each glyph clickable. Measured against a screenshot with a ruler:
/// ★ `[150, 167]`, ✕ `[173, 190]`, count `[196, 248]`.
const SIDE_GAP: f32 = 6.0;
const SIDE_COUNT_X: f32 = SIDEBAR_W - SIDE_RAIL_W - SIDE_COUNT_W;
const SIDE_DELETE_X: f32 = SIDE_COUNT_X - SIDE_GAP - SIDE_ACTION_W;
const SIDE_FEATURE_X: f32 = SIDE_DELETE_X - SIDE_GAP - SIDE_ACTION_W;
/// A few points of slack on the label side of each glyph: aiming at an 11-pixel
/// star is not a skill test.
const SIDE_ACTION_SLACK: f32 = 5.0;
const ARTICLE_AGE_W: f32 = 40.0;
// CJK fallback fonts have taller metrics than the Latin default, so every row
// sets an explicit line height and the row is sized from that: three lines of
// 20/18/18 px plus gaps and padding, without clipping.
/// How much of a feed is pulled in when it is opened. Enough to cover any feed
/// in this fleet several times over, small enough that opening fifty blogs is
/// still only tens of thousands of rows.
const SOURCE_HISTORY: usize = 400;

/// How often the page is asked where it is scrolled to. Each ask is a JavaScript
/// evaluation, so it is not a per-frame question.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// How often the reading position is written to the database while reading.
const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a transient message stays on screen.
const TOAST_TTL: Duration = Duration::from_millis(4_000);

/// Rows are sized from the type scale rather than from magic numbers: a row is
/// its title, an optional summary, a caption and the padding between them.
const ARTICLE_ROW_H: f32 = 88.0;
/// Which unread article to draw.
///
/// Pure so the one awkward rule in it can be tested without a window: the same
/// article is never drawn twice in a row while there is a choice. That is
/// enforced by drawing from a pool that excludes it, not by re-rolling — a
/// re-roll only makes a repeat unlikely, and "unlikely" is what a reader
/// experiences as a stuck key.
///
/// `roll(n)` is expected to return a value below `n`; it is clamped anyway,
/// because a caller that gets that wrong should not panic.
fn draw_unread(
    candidates: &[i64],
    last: Option<i64>,
    mut roll: impl FnMut(usize) -> usize,
) -> Option<i64> {
    if candidates.is_empty() {
        return None;
    }
    let mut pool: Vec<i64> = candidates
        .iter()
        .copied()
        .filter(|id| Some(*id) != last)
        .collect();
    if pool.is_empty() {
        // Exactly one candidate, and it is the one just read: read it again
        // rather than appear broken.
        pool = candidates.to_vec();
    }
    Some(pool[roll(pool.len()).min(pool.len() - 1)])
}

/// The row's right-hand margin that means "only this blog".
const ARTICLE_JUMP_W: f32 = 22.0;

/// How wide the reading-progress hairline may grow in a list row.
const ARTICLE_PROGRESS_W: f32 = 180.0;
const SIDE_ROW_H: f32 = 28.0;
/// Reading mode: the text column is centred and never wider than this. Long
/// lines are the main readability problem in a full-width window.
const READ_COL_MAX: f32 = 780.0;
/// Horizontal padding inside a text column, both modes.
const READ_PAD: f32 = 40.0;
/// Line height of the reading-mode body — the `PROSE` role's leading, so a
/// virtualised row and its text box agree exactly.
const READ_LINE_H: f32 = 24.0;
/// Height of the reading-mode toolbar; the embedded page fills what is left.
const READ_TOOLBAR_H: f32 = 44.0;
/// Width of one half-width column at `text_sm`, in px, with margin: CJK glyphs
/// advance ~14 px, so a half column is ~7.4 px and the extra keeps the longest
/// line inside the article column instead of clipping it.
const HALF_PX_COL: f32 = 8.0;
const MAX_READER_LINES: usize = 6_000;
/// Debounce before a query is sent to SQLite FTS.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// What the article list is currently filtered to.
#[derive(Clone, PartialEq, Eq)]
enum Scope {
    All,
    Unread,
    Starred,
    /// Everything that has been opened, most recently read first.
    History,
    Group(Arc<str>),
    Tag(Arc<str>),
    Source(SourceId),
}

impl Scope {

    fn label(&self) -> String {
        match self {
            Scope::All => "All items".into(),
            Scope::Unread => "Unread".into(),
            Scope::Starred => "Starred".into(),
            Scope::History => "History".into(),
            Scope::Group(g) => g.to_string(),
            Scope::Tag(t) => format!("#{t}"),
            Scope::Source(_) => "Source".into(),
        }
    }
}

/// How reading mode presents the article.
/// A deletion waiting for confirmation: one feed or twenty, same dialog.
struct PendingDelete {
    items: Vec<(SourceId, SharedString, SharedString)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    /// The original web page, rendered by an embedded WKWebView: the site's own
    /// CSS, fonts, images and layout, untouched.
    Original,
    /// The distilled plain text this app extracted at ingest time. Instant,
    /// offline, no scripts, no trackers — and the fallback when the original
    /// page cannot be loaded.
    Text,
}

impl ReadMode {
    fn label(self) -> &'static str {
        match self {
            ReadMode::Original => "原文",
            ReadMode::Text => "纯文本",
        }
    }
}

/// Which dimension the sidebar is organised along.
///
/// With a few thousand feeds the two questions a reader actually has are
/// "what is new in this category" and "what is new on this blog". Both are one
/// keypress apart instead of being forced into a single taxonomy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SideDim {
    /// Categories as sections, feeds nested underneath.
    Group,
    /// Every feed is a top-level entry, sorted and sectioned A–Z.
    Blog,
    /// Tags as sections — the labels the feed list itself carries.
    Tag,
}

impl SideDim {
    fn label(self) -> &'static str {
        match self {
            SideDim::Group => "按分类",
            SideDim::Blog => "按博客",
            SideDim::Tag => "按标签",
        }
    }

    fn next(self) -> Self {
        match self {
            SideDim::Blog => SideDim::Tag,
            SideDim::Tag => SideDim::Group,
            SideDim::Group => SideDim::Blog,
        }
    }
}

/// One row in the (virtualised) sidebar. A row is either a section header
/// (not clickable), a group header, or a scope/source entry.
struct SideRow {
    scope: Option<Scope>,
    label: SharedString,
    subtitle: SharedString,
    /// Pre-formatted trailing count (e.g. `3/25`), and whether to highlight it.
    count_label: SharedString,
    count_accent: bool,
    depth: u8,
    health: Option<Health>,
    is_group: bool,
    is_section: bool,
    /// Set on group/tag headers: the key this section folds under.
    collapse_key: Option<Arc<str>>,
    collapsed: bool,
}

pub struct App {
    engine: Arc<Engine>,
    snapshot: Arc<Snapshot>,

    /// Currently displayed articles (after scope filtering). Rebuilt only when
    /// the snapshot generation or the scope changes.
    list: Vec<Arc<Article>>,
    sidebar: Vec<SideRow>,
    scope: Scope,
    /// Sidebar organisation: by category or one row per blog.
    dim: SideDim,
    /// Hide blogs with no articles in the window instead of listing them last.
    hide_empty_blogs: bool,
    /// Folded sidebar sections, keyed `g:<group>` / `z:<section>`.
    ///
    /// Tags use their own set (`expanded_tags`) because their default is the
    /// opposite: a hundred tags each holding dozens of feeds is a list you want
    /// to read as an overview first, so a tag starts folded and remembers being
    /// opened. Groups and the empty-feeds section start open.
    collapsed: FxHashSet<Arc<str>>,
    /// Tags the reader has explicitly opened.
    expanded_tags: FxHashSet<Arc<str>>,
    /// `source id -> tags`, rebuilt with the snapshot: the sidebar and the tag
    /// filter both need it and neither should walk the config per frame.
    tags_by_source: Vec<Arc<[Arc<str>]>>,
    selected: usize,

    status_text: SharedString,
    problem_text: Option<SharedString>,

    query: String,
    search_focus: bool,
    /// When the current page request started, so the log can answer "how long
    /// did 原文 take?" instead of it being a matter of opinion.
    page_started_at: Option<Instant>,
    /// Set once the page has been revealed, so "fully loaded" is logged exactly
    /// once per navigation.
    page_loaded_at: Option<Instant>,
    /// The search field is a GPUI Kit `Input`: text editing, selection,
    /// clipboard and IME composition all come from the component library
    /// instead of being re-implemented here.
    search_input: Entity<InputState>,
    /// The sidebar's own filter, for finding a blog by name in a list of 1 400.
    side_input: Entity<InputState>,
    /// Text of `side_input` as typed, and its lower-cased form, which is what
    /// filtering compares against.
    side_filter_raw: String,
    side_filter: String,
    /// Curation mode: sidebar clicks tick subscriptions instead of scoping, so a
    /// batch from a freshly imported list can be featured or deleted together.
    curating: bool,
    picked: FxHashSet<SourceId>,
    /// A blog deletion waiting for confirmation: `(source id, title, file)`.
    confirming_delete: Option<PendingDelete>,
    /// A short-lived message (deleted a feed, jumped to a blog), with the moment
    /// it stops being interesting.
    toast: Option<(SharedString, Instant)>,
    /// The article being read, and how far, so the reading record can be flushed
    /// on a timer and on the way out instead of on every scroll tick.
    progress_of: Option<(i64, f32)>,
    progress_saved_at: Option<Instant>,
    /// A reading position waiting for the page to finish loading.
    restore_pending: Option<f32>,
    /// The "add a subscription" overlay: a URL field and the blogs found on the
    /// current blog's links page.
    add_open: bool,
    add_input: Entity<InputState>,
    add_status: SharedString,
    add_found: Vec<(discover::Candidate, discover::Feed)>,
    add_selected: FxHashSet<Arc<str>>,
    add_cursor: usize,
    add_scanning: bool,
    /// What the add overlay is offering, so `⏎` knows which group to file the
    /// result under and the heading can say where it came from.
    add_heading: SharedString,
    add_group: String,
    /// The Zhihu sign-in: a browser on zhihu.com inside the app, because the
    /// collection API only answers an authenticated request, and this way the
    /// session is never lifted out of another browser.
    login: bool,
    login_note: SharedString,
    /// Set when Zhihu refuses the saved session, so the next attempt shows the
    /// sign-in page instead of asking the same rejected cookies again.
    zhihu_need_login: bool,
    /// Set while waiting for the web view to hand back its cookie jar.
    cookie_wait: bool,
    /// When the page was last asked where it is scrolled to.
    progress_polled_at: Option<Instant>,
    pending_query: Option<String>,

    /// `(article id, wrapped lines)` for the reader pane.
    content: Option<(i64, Vec<SharedString>)>,
    /// The unwrapped body, kept so a window resize can re-wrap it.
    content_raw: Option<(i64, Arc<str>)>,
    /// Show traditional-script text as simplified. A display choice: what is
    /// stored stays exactly what the feed published.
    simplify: bool,
    /// The last article the random picker landed on, so pressing it again does
    /// not look like it did nothing.
    last_random: Option<i64>,
    /// Current wrap budget in half-width columns.
    reader_cols: usize,
    /// Reading mode: the article takes over the whole window.
    reading: bool,
    /// Reading mode presentation, and the embedded page when it is in use.
    read_mode: ReadMode,
    #[cfg(target_os = "macos")]
    web: Option<WebView>,
    /// Non-fatal message about the embedded page (load failure, etc).
    web_notice: Option<SharedString>,
    /// Set when a page should be loaded on the next frame: either a remote URL
    /// or a locally archived file (`file` flag).
    pending_url: Option<(Arc<str>, bool)>,
    /// True while a task is polling for the embedded page to report in.
    page_polling: bool,
    /// Whether the embedded page has somewhere to go back to, and where it is
    /// now (it changes when the reader follows an in-page link).
    page_can_back: bool,
    page_url: Option<SharedString>,
    /// True while this article's original page is being saved locally.
    archiving: bool,
    /// What we know about the current article's local archive.
    archive_note: Option<SharedString>,
    /// From the configuration: archive a page the first time it is opened.
    archive_on_open: bool,
    /// Last time the embedded page's URL/history was polled. Reading those means
    /// an Objective-C round trip and a string copy, so it is throttled rather
    /// than done on all 60 frames a second.
    page_probe_at: Option<Instant>,
    /// Set once the root has been focused for the first time.
    focused_once: bool,
    reader_title: SharedString,
    reader_meta: SharedString,
    reader_link: SharedString,
    reader_note: Option<SharedString>,

    focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    side_scroll: UniformListScrollHandle,
    reader_scroll: UniformListScrollHandle,
    help: bool,
}

impl App {
    pub fn new(
        engine: Arc<Engine>,
        search_input: Entity<InputState>,
        side_input: Entity<InputState>,
        add_input: Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = engine.snapshot();
        let events = engine.events();
        let archive_on_open = engine.config().current().settings.archive_on_open;
        let mut app = Self {
            engine,
            snapshot: Arc::new(Snapshot::default()),
            list: Vec::new(),
            sidebar: Vec::new(),
            scope: Scope::All,
            dim: SideDim::Blog,
            hide_empty_blogs: false,
            collapsed: FxHashSet::default(),
            expanded_tags: FxHashSet::default(),
            tags_by_source: Vec::new(),
            selected: 0,
            status_text: SharedString::default(),
            problem_text: None,
            query: String::new(),
            search_focus: false,
            search_input: search_input.clone(),
            side_input: side_input.clone(),
            side_filter_raw: String::new(),
            side_filter: String::new(),
            curating: false,
            picked: FxHashSet::default(),
            confirming_delete: None,
            toast: None,
            progress_of: None,
            progress_saved_at: None,
            restore_pending: None,
            add_open: false,
            add_input: add_input.clone(),
            add_status: SharedString::default(),
            add_found: Vec::new(),
            add_selected: FxHashSet::default(),
            add_cursor: 0,
            add_scanning: false,
            add_heading: SharedString::from("这个博客的友情链接"),
            add_group: String::from("订阅"),
            login: false,
            login_note: SharedString::default(),
            zhihu_need_login: false,
            cookie_wait: false,
            progress_polled_at: None,
            page_started_at: None,
            page_loaded_at: None,
            pending_query: None,
            content: None,
            content_raw: None,
            simplify: false,
            last_random: None,
            reader_cols: 80,
            reading: false,
            read_mode: ReadMode::Original,
            #[cfg(target_os = "macos")]
            web: None,
            web_notice: None,
            pending_url: None,
            page_polling: false,
            archiving: false,
            archive_note: None,
            archive_on_open,
            page_probe_at: None,
            page_can_back: false,
            page_url: None,
            focused_once: false,
            reader_title: SharedString::default(),
            reader_meta: SharedString::default(),
            reader_link: SharedString::default(),
            reader_note: None,
            focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            side_scroll: UniformListScrollHandle::new(),
            reader_scroll: UniformListScrollHandle::new(),
            help: false,
        };

        app.adopt(snapshot);

        // The field owns its own text; the app only reacts to it.
        cx.subscribe(
            &app.search_input,
            |app: &mut App, input: Entity<InputState>, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    app.query = input.read(cx).value().to_string();
                    app.send_search(cx);
                }
                InputEvent::Focus => {
                    app.search_focus = true;
                    cx.notify();
                }
                InputEvent::Blur => {
                    app.search_focus = false;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    app.search_focus = false;
                    cx.notify();
                }
            },
        )
        .detach();

        cx.spawn(async move |weak, cx| {
            // One long-lived task per window; the engine never blocks us.
            while let Ok(evt) = events.recv_async().await {
                if weak.update(cx, |app, cx| app.on_event(evt, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Seed the reader with the first item.
        app.select(0, cx);
        app
    }

    // -- state sync --------------------------------------------------------

    fn adopt(&mut self, snapshot: Arc<Snapshot>) {
        if snapshot.generation == self.snapshot.generation && !self.sidebar.is_empty() {
            return;
        }
        self.tags_by_source = (0..snapshot.sources.len().max(1))
            .map(|i| {
                snapshot
                    .sources
                    .get(i)
                    .map(|s| s.tags.clone())
                    .unwrap_or_else(|| Arc::from(Vec::new()))
            })
            .collect();
        self.snapshot = snapshot;
        self.rebuild_sidebar();
        self.rebuild_list();
        self.rebuild_status();
        self.problem_text = if self.snapshot.problems.is_empty() {
            None
        } else {
            Some(SharedString::new(self.snapshot.problems[0].clone()))
        };
    }

    fn on_event(&mut self, evt: EngineEvent, cx: &mut Context<Self>) {
        match evt {
            EngineEvent::Discovered { url, feed, error } => {
                match (feed, error) {
                    (Some(feed), _) => {
                        self.add_status =
                            SharedString::from(format!("已识别：{} → {}", url, feed.url));
                        // A URL typed by hand is unambiguous, so it is added
                        // straight away rather than asking for a tick.
                        self.engine.send(UiCommand::AddSource {
                            source: artifeeds_core::config::NewSource {
                                title: feed.title.to_string(),
                                url: feed.url.to_string(),
                                group: String::from("订阅"),
                                tags: vec![String::from("手动添加")],
                            },
                        });
                    }
                    (None, Some(error)) => {
                        self.add_status = SharedString::from(format!("识别失败：{error}"));
                    }
                    _ => {}
                }
                cx.notify();
            }
            EngineEvent::FriendFeed { candidate, feed } => {
                if self.add_group == "知乎收藏" {
                    // A blogroll scan that finished after the reader switched to
                    // Zhihu collections; its results belong to the other flow.
                    return;
                }
                // Ticked by default: the reader opened the blogroll flow on
                // purpose, and unticking is cheaper than ticking twenty times.
                self.add_selected.insert(Arc::clone(&feed.url));
                self.add_found.push((candidate, feed));
                self.add_found.sort_by(|a, b| a.0.label.cmp(&b.0.label));
                self.add_status =
                    SharedString::from(format!("找到 {} 个可订阅的博客（扫描中…）", self.add_found.len()));
                cx.notify();
            }
            EngineEvent::FriendScanDone { tried, found } => {
                if self.add_group == "知乎收藏" {
                    return;
                }
                self.add_scanning = false;
                self.add_status = SharedString::from(if tried == 0 {
                    "这个页面没有可扫描的友情链接".to_string()
                } else {
                    format!("扫描完成：试了 {tried} 个链接，找到 {found} 个 feed")
                });
                cx.notify();
            }
            EngineEvent::Featured { title, featured } => {
                self.toast = Some((
                    SharedString::from(if featured {
                        format!("已加精《{title}》—— 侧栏「精选」里可以找到")
                    } else {
                        format!("已取消加精《{title}》")
                    }),
                    Instant::now(),
                ));
                cx.notify();
            }
            EngineEvent::SourcesAdded { added } => {
                match added.as_slice() {
                    // One new feed: show it. The reader asked for this blog a
                    // moment ago, and "where did it go?" is the whole reason the
                    // sidebar's filter felt like the only answer.
                    [(id, title, group)] => {
                        self.set_scope(Scope::Source(*id), cx);
                        self.toast = Some((
                            SharedString::from(format!(
                                "已添加《{title}》· {group} —— 它就在这里，按 r 可立即抓取"
                            )),
                            Instant::now(),
                        ));
                    }
                    many => {
                        self.toast = Some((
                            SharedString::from(format!(
                                "已添加 {} 个订阅 —— 按 f 可按名字筛选，或在侧栏分组里找到它们",
                                many.len()
                            )),
                            Instant::now(),
                        ));
                    }
                }
                cx.notify();
            }
            EngineEvent::SourceAdded { title, url, file, error } => {
                match error {
                    None => {
                        self.toast = Some((
                            SharedString::from(format!("已添加《{title}》")),
                            Instant::now(),
                        ));
                        tracing::info!(%url, %file, "source added");
                    }
                    Some(e) if e.starts_with("已经在订阅里") => {
                        // Not a failure: the reader asked twice for the same
                        // feed, or it was already there.
                        self.toast = Some((
                            SharedString::from(format!("已在订阅里：《{title}》")),
                            Instant::now(),
                        ));
                    }
                    Some(e) => {
                        self.add_status = SharedString::from(format!("添加失败：{e}"));
                        tracing::warn!(%url, %e, "adding a source failed");
                    }
                }
                cx.notify();
            }
            EngineEvent::ZhihuCollections { name, collections, error } => {
                self.add_scanning = false;
                if let Some(error) = error {
                    if error.contains("未登录") {
                        self.zhihu_need_login = true;
                    }
                    self.add_status = SharedString::from(error);
                } else {
                    self.zhihu_need_login = false;
                    self.add_heading = SharedString::from(format!("{name} 的收藏夹"));
                    self.add_found = collections
                        .iter()
                        .map(|collection| {
                            let url = format!("https://www.zhihu.com/collection/{}", collection.id);
                            (
                                artifeeds_core::discover::Candidate {
                                    url: Arc::from(url.as_str()),
                                    label: Arc::from(collection.title.as_str()),
                                },
                                artifeeds_core::discover::Feed {
                                    url: Arc::from(url.as_str()),
                                    title: Arc::from(collection.title.as_str()),
                                },
                            )
                        })
                        .collect();
                    self.add_selected =
                        self.add_found.iter().map(|(_, f)| Arc::clone(&f.url)).collect();
                    self.add_cursor = 0;
                    self.add_status = SharedString::from(format!(
                        "{} 个收藏夹，共 {} 条收藏 —— ⏎ 全部加入",
                        collections.len(),
                        collections.iter().map(|c| c.count as usize).sum::<usize>()
                    ));
                }
                cx.notify();
            }
            EngineEvent::Content { id, text } => {
                if self.selected_article().map(|a| a.id) == Some(id) {
                    self.set_reader(id, &text);
                }
            }
            EngineEvent::Archived { id, path, bytes, error } => {
                self.archiving = false;
                match (path, error) {
                    (Some(path), _) => {
                        let size = artifeeds_core::text::human_bytes(bytes as u64);
                        self.archive_note = Some(SharedString::from(format!("本地存档 · {size}")));
                        for a in self.list.iter_mut() {
                            if a.id != id {
                                continue;
                            }
                            let mut copy = (**a).clone();
                            copy.archive_path = Some(Arc::clone(&path));
                            copy.archive_bytes = bytes;
                            *a = Arc::new(copy);
                        }
                        // Show the saved copy, so what is on screen is what is
                        // on disk (scripts stripped, resources inlined).
                        if self.reading
                            && self.read_mode == ReadMode::Original
                            && self.selected_article().map(|a| a.id) == Some(id)
                        {
                            self.pending_url = Some((path, true));
                        }
                    }
                    (None, Some(error)) => {
                        self.archive_note = Some(SharedString::from(format!("存档失败：{error}")));
                    }
                    (None, None) => {}
                }
            }
            EngineEvent::Published { .. } | EngineEvent::Ready => {
                let snap = self.engine.snapshot();
                self.adopt(snap);
            }
            EngineEvent::SearchResults { query, ids } => {
                let _ = (query, ids);
            }
            EngineEvent::ConfigReloaded { sources, problems } => {
                tracing::info!(sources, problems, "configuration reloaded");
                let snap = self.engine.snapshot();
                self.adopt(snap);
            }
            EngineEvent::Problem(msg) => {
                self.problem_text = Some(SharedString::new(msg));
            }
        }
        cx.notify();
    }

    fn rebuild_sidebar(&mut self) {
        let mut rows: Vec<SideRow> = Vec::with_capacity(self.snapshot.sources.len() + 64);
        // Library-wide counts from the window, so an active search narrows the
        // article list without also emptying the sidebar.
        let total = self.snapshot.stats.mem_articles as u32;
        let unread = self.snapshot.stats.window_unread as u32;
        let starred = self.snapshot.stats.window_starred as u32;
        let empty_hidden = self.hide_empty_blogs;

        #[allow(clippy::too_many_arguments)]
        let simplify = self.simplify;
        let mut push = |scope: Option<Scope>,
                        label: SharedString,
                        subtitle: SharedString,
                        count_label: SharedString,
                        count_accent: bool,
                        depth: u8,
                        health: Option<Health>,
                        is_group: bool,
                        is_section: bool,
                        collapse_key: Option<Arc<str>>,
                        collapsed: bool| {
            let (label, subtitle) = if simplify {
                (
                    SharedString::from(artifeeds_core::text::to_simplified(&label)),
                    SharedString::from(artifeeds_core::text::to_simplified(&subtitle)),
                )
            } else {
                (label, subtitle)
            };
            rows.push(SideRow {
                scope,
                label,
                subtitle,
                count_label,
                count_accent,
                depth,
                health,
                is_group,
                is_section,
                collapse_key,
                collapsed,
            });
        };

        // `unread/total` when there is something to read, the size alone
        // otherwise: the two numbers a reader actually compares.
        let progress = |unread: u32, total: u32| -> (SharedString, bool) {
            if unread > 0 {
                (SharedString::from(format!("{unread}/{total}")), true)
            } else {
                (SharedString::from(total.to_string()), false)
            }
        };

        let db_total = self.snapshot.stats.db_articles as u32;
        let db_unread = self.snapshot.stats.db_unread as u32;
        let stored = |window: u32, db: u32| -> SharedString {
            if db > window {
                SharedString::from(format!("{window} / {db}"))
            } else {
                SharedString::from(window.to_string())
            }
        };
        push(Some(Scope::All), SharedString::from("All items"), SharedString::default(), stored(total, db_total), false, 0, None, false, false, None, false);
        let label = if db_unread > unread {
            SharedString::from(format!("{unread} / {db_unread}"))
        } else {
            progress(unread, total).0
        };
        let accent = unread > 0;
        push(Some(Scope::Unread), SharedString::from("Unread"), SharedString::default(), label, accent, 0, None, false, false, None, false);
        // Starred has no meaningful denominator.
        push(
            Some(Scope::Starred),
            SharedString::from("Starred"),
            SharedString::default(),
            if starred > 0 { SharedString::from(starred.to_string()) } else { SharedString::default() },
            starred > 0,
            0,
            None,
            false,
            false,
            None,
            false,
        );
        let history = self.snapshot.articles.iter().filter(|a| a.read_at > 0).count() as u32;
        push(
            Some(Scope::History),
            SharedString::from("History"),
            SharedString::default(),
            if history > 0 { SharedString::from(history.to_string()) } else { SharedString::default() },
            false,
            0,
            None,
            false,
            false,
            None,
            false,
        );

        // 精选: the subscriptions the reader marked as their own, kept together
        // at the top. Only rendered when there are any — an empty section is a
        // row that teaches nothing.
        let mut featured: Vec<&artifeeds_core::model::SourceRow> =
            self.snapshot.sources.iter().filter(|s| s.featured).collect();
        if !featured.is_empty() {
            featured.sort_by(|a, b| ci_cmp(&a.title, &b.title));
            push(
                None,
                SharedString::from(format!("精选 · {}", featured.len())),
                SharedString::default(),
                SharedString::default(),
                false,
                0,
                None,
                false,
                true,
                None,
                false,
            );
            for row in featured {
                let (label, accent) = progress(row.unread, row.total);
                push(
                    Some(Scope::Source(row.id)),
                    SharedString::new(row.title.clone()),
                    source_subtitle(row, false),
                    label,
                    accent,
                    1,
                    Some(row.health),
                    false,
                    false,
                    None,
                    false,
                );
            }
        }

        // A filter turns the sidebar into a flat list of matches: with a query
        // in hand the grouping is noise, and the answer is always "these feeds".
        if !self.side_filter.is_empty() {
            let needle = squash(&self.side_filter);
            let mut hits: Vec<&artifeeds_core::model::SourceRow> = self
                .snapshot
                .sources
                .iter()
                .filter(|s| name_matches(&s.title, &s.host, &needle))
                .collect();
            hits.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| ci_cmp(&a.title, &b.title)));
            push(
                None,
                SharedString::from(format!("匹配 {} 个博客", hits.len())),
                SharedString::default(),
                SharedString::default(),
                false,
                0,
                None,
                false,
                true,
                None,
                false,
            );
            for row in hits {
                let (label, accent) = progress(row.unread, row.total);
                push(
                    Some(Scope::Source(row.id)),
                    SharedString::new(row.title.clone()),
                    source_subtitle(row, false),
                    label,
                    accent,
                    0,
                    Some(row.health),
                    false,
                    false,
                    None,
                    false,
                );
            }
            self.sidebar = rows;
            return;
        }

        match self.dim {
            SideDim::Group => {
                let mut by_group: BTreeMap<&str, Vec<&artifeeds_core::model::SourceRow>> = BTreeMap::new();
                for s in self.snapshot.sources.iter() {
                    by_group.entry(s.group.as_ref()).or_default().push(s);
                }
                for (group, sources) in by_group.iter_mut() {
                    sources.sort_by(|a, b| ci_cmp(&a.title, &b.title));
                    let mut unread_sum = 0u32;
                    let mut total_sum = 0u32;
                    for s in sources.iter() {
                        unread_sum += s.unread;
                        total_sum += s.total;
                    }
                    let (label, accent) = progress(unread_sum, total_sum);
                    let key: Arc<str> = Arc::from(format!("g:{group}").as_str());
                    let folded = self.collapsed.contains(&key);
                    push(
                        Some(Scope::Group(Arc::from(*group))),
                        SharedString::new(Arc::<str>::from(*group)),
                        SharedString::from(format!("{} feeds", sources.len())),
                        label,
                        accent,
                        0,
                        None,
                        true,
                        false,
                        Some(key),
                        folded,
                    );
                    if folded {
                        continue;
                    }
                    for s in sources.iter() {
                        let (label, accent) = progress(s.unread, s.total);
                        push(
                            Some(Scope::Source(s.id)),
                            SharedString::new(s.title.clone()),
                            source_subtitle(s, true),
                            label,
                            accent,
                            1,
                            Some(s.health),
                            false,
                            false,
                            None,
                            false,
                        );
                    }
                }
            }
            SideDim::Blog => {
                // Blogs with something to read come first, A–Z with section
                // headers; blogs with nothing in the window sink to the bottom
                // (or are left out entirely) so a 1 400-feed list stays usable.
                let (mut active, mut empty): (Vec<_>, Vec<_>) = self
                    .snapshot
                    .sources
                    .iter()
                    .partition(|s| s.total > 0);

                active.sort_by(|a, b| {
                    bucket_of(&a.title).cmp(&bucket_of(&b.title)).then_with(|| ci_cmp(&a.title, &b.title))
                });
                let mut current: Option<(u8, char)> = None;
                for s in active {
                    let bucket = bucket_of(&s.title);
                    if current != Some(bucket) {
                        current = Some(bucket);
                        push(None, SharedString::from(section_label(bucket)), SharedString::default(), SharedString::default(), false, 0, None, false, true, None, false);
                    }
                    let (label, accent) = progress(s.unread, s.total);
                    push(
                        Some(Scope::Source(s.id)),
                        SharedString::new(s.title.clone()),
                        source_subtitle(s, false),
                        label,
                        accent,
                        0,
                        Some(s.health),
                        false,
                        false,
                        None,
                        false,
                    );
                }

                if !empty_hidden && !empty.is_empty() {
                    empty.sort_by(|a, b| ci_cmp(&a.title, &b.title));
                    let key: Arc<str> = Arc::from("z:empty");
                    let folded = self.collapsed.contains(&key);
                    push(
                        None,
                        SharedString::from(format!("暂无文章 · {}", empty.len())),
                        SharedString::default(),
                        SharedString::default(),
                        false,
                        0,
                        None,
                        false,
                        true,
                        Some(key),
                        folded,
                    );
                    for s in empty.iter().filter(|_| !folded) {
                        push(
                            Some(Scope::Source(s.id)),
                            SharedString::new(s.title.clone()),
                            source_subtitle(s, false),
                            SharedString::default(),
                            false,
                            0,
                            Some(s.health),
                            false,
                            false,
                            None,
                            false,
                        );
                    }
                }
            }
            SideDim::Tag => {
                // Grouped by each feed's *primary* tag so a blog appears exactly
                // once and the counts stay honest; a feed's full tag list is
                // shown as chips when it is selected.
                let mut by_tag: BTreeMap<Arc<str>, Vec<&artifeeds_core::model::SourceRow>> =
                    BTreeMap::new();
                for row in self.snapshot.sources.iter() {
                    let primary = row
                        .tags
                        .first()
                        .cloned()
                        .unwrap_or_else(|| Arc::from(row.group.as_ref()));
                    by_tag.entry(primary).or_default().push(row);
                }
                let mut tags: Vec<(Arc<str>, Vec<&artifeeds_core::model::SourceRow>)> =
                    by_tag.into_iter().collect();
                // Tags with unread items first, then alphabetical: what is worth
                // reading rises to the top on its own.
                tags.sort_by(|a, b| {
                    let (ua, ub) = (
                        a.1.iter().map(|s| s.unread).sum::<u32>(),
                        b.1.iter().map(|s| s.unread).sum::<u32>(),
                    );
                    ub.cmp(&ua).then_with(|| a.0.cmp(&b.0))
                });
                for (tag, mut sources) in tags {
                    let mut unread_sum = 0u32;
                    let mut total_sum = 0u32;
                    for row in sources.iter() {
                        unread_sum += row.unread;
                        total_sum += row.total;
                    }
                    let (label, accent) = progress(unread_sum, total_sum);
                    let key: Arc<str> = Arc::from(format!("t:{tag}").as_str());
                    let folded = !self.expanded_tags.contains(&key);
                    push(
                        Some(Scope::Tag(tag.clone())),
                        SharedString::from(format!("#{tag}")),
                        SharedString::from(format!("{} feeds", sources.len())),
                        label,
                        accent,
                        0,
                        None,
                        true,
                        false,
                        Some(key),
                        folded,
                    );
                    if folded {
                        continue;
                    }
                    sources.sort_by(|a, b| ci_cmp(&a.title, &b.title));
                    for row in sources {
                        let (label, accent) = progress(row.unread, row.total);
                        push(
                            Some(Scope::Source(row.id)),
                            SharedString::new(row.title.clone()),
                            source_subtitle(row, false),
                            label,
                            accent,
                            1,
                            Some(row.health),
                            false,
                            false,
                            None,
                            false,
                        );
                    }
                }
            }
        }
        self.sidebar = rows;
    }

    /// Does an article belong to the current scope? Tag membership is resolved
    /// through the feed the article came from.
    fn in_scope(&self, a: &Article) -> bool {
        match &self.scope {
            Scope::All => true,
            Scope::Unread => !a.read,
            Scope::Starred => a.starred,
            Scope::History => a.read_at > 0,
            Scope::Group(g) => a.group.as_ref() == g.as_ref(),
            Scope::Tag(tag) => self
                .tags_by_source
                .get(a.source as usize)
                .is_some_and(|tags| tags.iter().any(|t| t.as_ref() == tag.as_ref())),
            Scope::Source(id) => a.source == *id,
        }
    }

    fn rebuild_list(&mut self) {
        let mut list = Vec::with_capacity(self.snapshot.articles.len().min(8192));
        for a in self.snapshot.articles.iter() {
            if self.in_scope(a) {
                list.push(a.clone());
            }
        }
        // History is an ordering as much as a filter: most recently read first.
        if self.scope == Scope::History {
            list.sort_by_key(|a| std::cmp::Reverse(a.read_at));
        }
        self.list = list;
        if self.selected >= self.list.len() {
            self.selected = self.list.len().saturating_sub(1);
        }
    }

    fn rebuild_status(&mut self) {
        let s = &self.snapshot.stats;
        let cache = if s.responses_304 + s.responses_200 > 0 {
            s.responses_304 as f64 / (s.responses_304 + s.responses_200) as f64 * 100.0
        } else {
            0.0
        };
        let text = format!(
            "{} feeds · {} enabled · {} due · {} in flight   |   req {} · {:.0}% 304 · errors {}   |   net {} (saved {})   |   db {} · win {}   |   p95 {}ms · batch {}ms · rss {} MiB",
            s.sources_total,
            s.sources_enabled,
            s.sources_due,
            s.in_flight,
            s.requests,
            cache,
            s.errors,
            human_bytes(s.bytes_in),
            human_bytes(s.bytes_saved_304),
            human_bytes(s.db_bytes),
            s.mem_articles,
            s.p95_latency_ms,
            s.last_batch_ms,
            s.rss_kib / 1024,
        );
        self.status_text = SharedString::from(text);
    }

    fn selected_article(&self) -> Option<&Arc<Article>> {
        self.list.get(self.selected)
    }

    /// The status line for the current sidebar scope: host, learned interval and
    /// health for a source, feed count for a group. `None` for the built-in
    /// scopes, where the keyboard hint is more useful.
    /// Tags to show as chips for the current scope: a feed's own tags, or the
    /// tag being browsed.
    fn scope_tags(&self) -> Vec<Arc<str>> {
        match &self.scope {
            Scope::Source(id) => self
                .tags_by_source
                .get(*id as usize)
                .map(|t| t.to_vec())
                .unwrap_or_default(),
            Scope::Tag(tag) => vec![Arc::clone(tag)],
            _ => Vec::new(),
        }
    }

    fn scope_subtitle(&self) -> Option<SharedString> {
        if matches!(self.scope, Scope::All | Scope::Unread | Scope::Starred) {
            return None;
        }
        self.sidebar
            .iter()
            .find(|row| row.scope.as_ref() == Some(&self.scope))
            .map(|row| row.subtitle.clone())
    }

    // -- actions -----------------------------------------------------------

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.list.is_empty() {
            self.content = None;
            return;
        }
        let index = index.min(self.list.len() - 1);
        self.selected = index;
        self.list_scroll.scroll_to_item(index, ScrollStrategy::Center);
        self.reader_scroll.scroll_to_item(0, ScrollStrategy::Top);

        let article = self.list[index].clone();
        self.reader_title = SharedString::new(article.title.clone());
        self.reader_meta = SharedString::from(format!(
            "{} · {} · {}",
            article.source_title,
            article.age,
            article.author.as_deref().unwrap_or("—"),
        ));
        self.reader_link = SharedString::new(article.link.clone());
        self.content = None;
        self.reader_note = None;

        if article.has_content {
            self.engine.send(UiCommand::LoadContent { id: article.id });
        } else {
            let id = article.id;
            let preview = article.preview.clone();
            self.set_reader(id, &preview);
        }
        // Reading mode follows the selection: j/k moves on to the next page.
        if self.reading {
            self.request_page(cx);
        }

        cx.notify();
    }

    fn mark_read(&mut self, ids: &[i64], read: bool) {
        self.engine.send(UiCommand::MarkRead { ids: ids.to_vec(), read });
        for a in self.list.iter_mut() {
            if ids.contains(&a.id) && a.read != read {
                let mut copy = (**a).clone();
                copy.read = read;
                *a = Arc::new(copy);
            }
        }
    }

    fn toggle_star(&mut self, cx: &mut Context<Self>) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        let starred = !article.starred;
        self.engine.send(UiCommand::ToggleStar { id: article.id, starred });
        for a in self.list.iter_mut() {
            if a.id == article.id {
                let mut copy = (**a).clone();
                copy.starred = starred;
                *a = Arc::new(copy);
            }
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: i64, cx: &mut Context<Self>) {
        if self.list.is_empty() {
            return;
        }
        let next = (self.selected as i64 + delta).clamp(0, self.list.len() as i64 - 1) as usize;
        if next != self.selected {
            self.select(next, cx);
        }
    }

    /// Fold or unfold a sidebar section. Which set is touched depends on the
    /// section's kind, because groups and tags have opposite defaults.
    fn toggle_section(&mut self, key: Arc<str>, cx: &mut Context<Self>) {
        let set = if key.starts_with("t:") {
            &mut self.expanded_tags
        } else {
            &mut self.collapsed
        };
        if !set.remove(&key) {
            set.insert(key);
        }
        self.rebuild_sidebar();
        cx.notify();
    }

    /// Blogs with no articles are listed last by default; this hides them.
    fn toggle_empty_blogs(&mut self, cx: &mut Context<Self>) {
        self.hide_empty_blogs = !self.hide_empty_blogs;
        self.rebuild_sidebar();
        self.side_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn set_dim(&mut self, dim: SideDim, cx: &mut Context<Self>) {
        if self.dim == dim {
            return;
        }
        self.dim = dim;
        self.rebuild_sidebar();
        self.side_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Opening a blog asks for that blog's history: the window holds a recency
    /// slice of the fleet, which for a dormant feed is two articles — a truthful
    /// answer to "what is new" and a useless one to "show me this blog".
    fn request_source_history(&self, scope: &Scope) {
        if let Scope::Source(id) = scope {
            self.engine.send(UiCommand::LoadSource { id: *id, limit: SOURCE_HISTORY });
        }
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.request_source_history(&scope);
        self.scope = scope;
        self.selected = 0;
        self.rebuild_list();
        self.select(0, cx);
        cx.notify();
    }

    /// Text as it should be displayed: simplified when the reader asked for it.
    fn zh(&self, text: &str) -> SharedString {
        if self.simplify {
            SharedString::from(artifeeds_core::text::to_simplified(text))
        } else {
            SharedString::from(text.to_string())
        }
    }

    fn toggle_simplify(&mut self, cx: &mut Context<Self>) {
        self.simplify = !self.simplify;
        self.rewrap();
        self.toast = Some((
            SharedString::from(if self.simplify {
                "繁体已转为简体"
            } else {
                "显示原文（繁体保持原样）"
            }),
            Instant::now(),
        ));
        cx.notify();
    }

    /// A number in `0..n`, without a dependency: xorshift64* seeded from the
    /// clock once per thread.
    fn random_below(n: usize) -> usize {
        use std::cell::Cell;
        thread_local! {
            static STATE: Cell<u64> = const { Cell::new(0) };
        }
        STATE.with(|state| {
            let mut x = state.get();
            if x == 0 {
                x = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x2545_F491_4F6C_DD1D)
                    | 1;
            }
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            state.set(x);
            ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize) % n.max(1)
        })
    }

    /// Draw one unread article from the current scope and open it.
    fn read_something_random(&mut self, cx: &mut Context<Self>) {
        let unread: Vec<i64> = self
            .list
            .iter()
            .filter(|article| !article.read)
            .map(|article| article.id)
            .collect();
        let Some(id) = draw_unread(&unread, self.last_random, Self::random_below) else {
            self.toast = Some((SharedString::from("当前范围没有未读"), Instant::now()));
            cx.notify();
            return;
        };
        let Some(pick) = self.list.iter().position(|article| article.id == id) else {
            return;
        };
        self.last_random = Some(id);
        self.select(pick, cx);
        self.enter_reading(cx);
        cx.notify();
    }

    fn set_reader(&mut self, id: i64, text: &Arc<str>) {
        self.content_raw = Some((id, Arc::clone(text)));
        self.rewrap();
    }

    /// Re-flow the reader body. Cheap enough to run on every resize: it only
    /// ever touches the single selected article.
    fn rewrap(&mut self) {
        let Some((id, text)) = self.content_raw.clone() else {
            return;
        };
        // Converted here rather than at render time: this is the one place the
        // body turns into lines, so the result is cached in `lines` and a toggle
        // is a re-wrap rather than a per-frame cost.
        let text = if self.simplify {
            std::borrow::Cow::Owned(artifeeds_core::text::to_simplified(&text))
        } else {
            std::borrow::Cow::Borrowed(text.as_ref())
        };
        let mut lines: Vec<SharedString> =
            artifeeds_core::text::wrap_into_lines(&text, self.reader_cols)
                .into_iter()
                .map(SharedString::from)
                .collect();
        if lines.len() > MAX_READER_LINES {
            lines.truncate(MAX_READER_LINES);
            lines.push(SharedString::from("… truncated for display"));
        }
        self.reader_note = None;
        self.content = Some((id, lines));
    }

    /// Width of the centred text column, in px.
    fn text_column_px(window_px: f32) -> f32 {
        let margin = 120.0f32.min(window_px * 0.08);
        (window_px - 2.0 * margin).clamp(220.0, READ_COL_MAX)
    }

    /// Wrap budget for the current window width, in half-width columns.
    fn columns_for(window_px: f32) -> usize {
        let text = (Self::text_column_px(window_px) - READ_PAD).max(120.0);
        ((text / HALF_PX_COL) as usize).clamp(30, 240)
    }

    fn enter_reading(&mut self, cx: &mut Context<Self>) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        // Opening *is* reading: this is where an article becomes read.
        if !article.read {
            self.mark_read(&[article.id], true);
        }
        if self.reading {
            return;
        }
        self.reading = true;
        self.request_page(cx);
        cx.notify();
    }

    fn exit_reading(&mut self, cx: &mut Context<Self>) {
        // Leaving is the last chance to remember the position.
        self.track_progress(cx);
        self.flush_progress();
        if !self.reading {
            return;
        }
        self.reading = false;
        self.hide_page();
        cx.notify();
    }

    fn set_read_mode(&mut self, mode: ReadMode, cx: &mut Context<Self>) {
        if self.read_mode == mode {
            return;
        }
        self.read_mode = mode;
        self.web_notice = None;
        match mode {
            ReadMode::Original => self.request_page(cx),
            ReadMode::Text => self.hide_page(),
        }
        cx.notify();
    }

    /// Ask for the selected article's original page. The native side is driven
    /// from `render`, which is where the window handle is available.
    fn request_page(&mut self, cx: &mut Context<Self>) {
        if self.read_mode != ReadMode::Original {
            return;
        }
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        if article.link.is_empty() {
            self.read_mode = ReadMode::Text;
            self.web_notice = Some(SharedString::from("该条目没有原始链接，改为显示纯文本"));
            return;
        }
        self.web_notice = None;
        self.archiving = false;
        self.page_started_at = Some(Instant::now());
        self.page_loaded_at = None;
        match &article.archive_path {
            // Already archived: render the saved copy. No network at all.
            Some(path) => {
                self.archive_note = Some(SharedString::from(format!(
                    "本地存档 · {}",
                    artifeeds_core::text::human_bytes(article.archive_bytes as u64)
                )));
                self.pending_url = Some((Arc::clone(path), true));
            }
            None => {
                self.archive_note = None;
                self.pending_url = Some((Arc::clone(&article.link), false));
                self.maybe_archive(article.id, cx);
            }
        }
        self.spawn_page_poll(cx);
    }

    /// Follow the sidebar field's *current* text.
    ///
    /// The component emits `InputEvent::Change` when text is committed, which is
    /// the right signal for the article search (an FTS query per pinyin letter
    /// would be absurd). A filter is the opposite case: while an IME is composing
    /// `css` there is no committed text at all, so the value is read directly and
    /// compared. The comparison is a `str` compare, so the steady state costs
    /// nothing.
    fn sync_side_filter(&mut self, cx: &mut Context<Self>) {
        let value = self.side_input.read(cx).value();
        if value.as_ref() != self.side_filter_raw {
            self.side_filter_raw = value.to_string();
            self.side_filter = self.side_filter_raw.to_lowercase();
            self.rebuild_sidebar();
        }
    }

    /// Open the add-a-subscription overlay and start scanning the current blog
    /// for links to other blogs.
    fn open_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_open = true;
        self.add_group = String::from("订阅");
        self.add_heading = SharedString::from("这个博客的友情链接");
        self.add_found.clear();
        self.add_selected.clear();
        self.add_cursor = 0;
        self.add_status = SharedString::from("正在扫描这个博客的友情链接…");
        self.add_scanning = true;
        self.add_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        // The engine starts from the article (a page whose sidebar usually has
        // the links) and then tries the pages blogs keep blogrolls on.
        if let Some(article) = self.selected_article() {
            let url = Arc::clone(&article.link);
            self.engine.send(UiCommand::FriendFeeds { page_url: url });
        } else {
            self.add_status = SharedString::from("手动输入一个博客或 feed 地址");
            self.add_scanning = false;
        }
        let handle = self.add_input.read(cx).focus_handle(cx);
        cx.notify();
        let _ = handle;
    }

    /// Is an overlay covering the list right now?
    fn modal_open(&self) -> bool {
        self.add_open || self.login || self.help || self.confirming_delete.is_some()
    }

    /// List the collections, signing in first if there is no usable session.
    fn zhihu_collections(&mut self, cx: &mut Context<Self>) {
        let jar = std::fs::read_to_string(artifeeds_core::engine::zhihu_cookie_path())
            .unwrap_or_default();
        if jar.trim().is_empty() || self.zhihu_need_login {
            self.open_zhihu_login(cx);
            return;
        }
        self.add_open = true;
        self.add_heading = SharedString::from("知乎收藏");
        self.add_group = String::from("知乎收藏");
        self.add_found.clear();
        self.add_selected.clear();
        self.add_cursor = 0;
        self.add_scanning = true;
        self.add_status = SharedString::from("正在读取你的知乎收藏夹…");
        self.engine.send(UiCommand::ZhihuCollections);
        cx.notify();
    }

    /// Show zhihu.com in the embedded browser so the reader can sign in.
    fn open_zhihu_login(&mut self, cx: &mut Context<Self>) {
        self.add_open = false;
        self.login = true;
        self.cookie_wait = false;
        self.login_note = SharedString::from("登录完成后点右上角「我已完成登录」");
        self.pending_url = Some((Arc::from("https://www.zhihu.com/signin"), false));
        cx.notify();
    }

    fn close_login(&mut self, cx: &mut Context<Self>) {
        self.login = false;
        self.cookie_wait = false;
        self.pending_url = None;
        cx.notify();
    }

    /// Ask the web view for its zhihu.com cookies. The answer arrives in a later
    /// frame, because `getAllCookies:` is asynchronous.
    fn finish_zhihu_login(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            let Some(web) = &self.web else {
                self.login_note = SharedString::from("内嵌浏览器不可用");
                cx.notify();
                return;
            };
            web.request_cookies("zhihu.com");
            self.cookie_wait = true;
            self.login_note = SharedString::from("正在读取登录状态…");
        }
        cx.notify();
    }

    /// Once the jar has arrived: save it, and offer the account's collections.
    fn poll_zhihu_cookies(&mut self, cx: &mut Context<Self>) {
        if !self.cookie_wait {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let Some(jar) = webview::take_cookies() else {
                return;
            };
            self.cookie_wait = false;
            if jar.trim().is_empty() {
                self.login_note =
                    SharedString::from("没读到知乎登录状态：先在页面里登录，再点「我已完成登录」");
                cx.notify();
                return;
            }
            let path = artifeeds_core::engine::zhihu_cookie_path();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(&path, &jar) {
                Ok(()) => {
                    tracing::info!(bytes = jar.len(), "zhihu session saved");
                    self.zhihu_need_login = false;
                    self.login = false;
                    self.pending_url = None;
                    self.add_open = true;
                    self.add_found.clear();
                    self.add_selected.clear();
                    self.add_cursor = 0;
                    self.add_heading = SharedString::from("知乎收藏");
                    self.add_group = String::from("知乎收藏");
                    self.add_status = SharedString::from("正在读取你的知乎收藏夹…");
                    self.add_scanning = true;
                    self.engine.send(UiCommand::ZhihuCollections);
                }
                Err(e) => {
                    self.login_note = SharedString::from(format!("保存登录状态失败：{e}"));
                }
            }
        }
        cx.notify();
    }

    fn close_add(&mut self, cx: &mut Context<Self>) {
        self.add_open = false;
        self.add_scanning = false;
        cx.notify();
    }

    /// Add the ticked blogs, or the URL in the field.
    fn commit_add(&mut self, cx: &mut Context<Self>) {
        let typed = self.add_input.read(cx).value().to_string();
        let typed = typed.trim().to_string();
        if !typed.is_empty() {
            self.engine.send(UiCommand::DiscoverFeed { url: Arc::from(typed.as_str()) });
            self.add_status = SharedString::from(format!("正在识别 {typed} …"));
            cx.notify();
            return;
        }
        let picks: Vec<(discover::Candidate, discover::Feed)> = self
            .add_found
            .iter()
            .filter(|(_, feed)| self.add_selected.contains(&feed.url))
            .cloned()
            .collect();
        if picks.is_empty() {
            self.add_status = SharedString::from("没有选中任何博客");
            cx.notify();
            return;
        }
        for (candidate, feed) in &picks {
            self.engine.send(UiCommand::AddSource {
                source: artifeeds_core::config::NewSource {
                    title: String::from(candidate.label.as_ref()),
                    url: String::from(feed.url.as_ref()),
                    group: self.add_group.clone(),
                    tags: vec![String::from(if self.add_group == "知乎收藏" {
                        "知乎"
                    } else {
                        "手动添加"
                    })],
                },
            });
        }
        self.add_open = false;
        self.toast = Some((
            SharedString::from(format!("已添加 {} 个订阅", picks.len())),
            Instant::now(),
        ));
        cx.notify();
    }

    /// Which blog a destructive action should target: the scoped one, or the one
    /// behind the selected article.
    fn target_source(&self) -> Option<SourceId> {
        match &self.scope {
            Scope::Source(id) => Some(*id),
            _ => self.selected_article().map(|a| a.source),
        }
    }

    /// Curation mode: clicks tick subscriptions instead of changing the scope.
    fn toggle_curating(&mut self, cx: &mut Context<Self>) {
        self.curating = !self.curating;
        if !self.curating {
            self.picked.clear();
        }
        self.toast = Some((
            SharedString::from(if self.curating {
                "整理模式：点侧栏勾选订阅，d 删除选中 · * 加精选中 · esc 退出"
            } else {
                "已退出整理模式"
            }),
            Instant::now(),
        ));
        cx.notify();
    }

    /// `*`: feature what the reader is looking at — the picked set while
    /// curating, otherwise the subscription in scope.
    fn feature_target(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<SourceId> = if self.curating && !self.picked.is_empty() {
            self.picked.iter().copied().collect()
        } else if let Some(id) = self.target_source() {
            vec![id]
        } else {
            Vec::new()
        };
        if targets.is_empty() {
            return;
        }
        // Marking a batch sets them all to the same state: if every one is
        // already featured, the gesture means "unmark".
        let all_featured = targets.iter().all(|id| {
            self.snapshot
                .sources
                .get(*id as usize)
                .map(|s| s.featured)
                .unwrap_or(false)
        });
        for id in &targets {
            self.engine.send(UiCommand::SetFeatured {
                source: *id,
                featured: !all_featured,
            });
        }
        cx.notify();
    }

    /// Flip the reader's own mark on a subscription.
    fn toggle_featured(&mut self, id: SourceId, cx: &mut Context<Self>) {
        let Some(row) = self.snapshot.sources.get(id as usize) else {
            return;
        };
        let featured = !row.featured;
        self.engine.send(UiCommand::SetFeatured { source: id, featured });
        cx.notify();
    }

    /// Ask before deleting a specific subscription — the sidebar's ✕ rail, as
    /// opposed to `d`, which acts on whatever the reader is looking at.
    fn request_delete_of(&mut self, id: SourceId, cx: &mut Context<Self>) {
        let Some(row) = self.snapshot.sources.get(id as usize) else {
            return;
        };
        let file = self
            .engine
            .config()
            .source_origin(id)
            .map(|(_, file)| file.to_string())
            .unwrap_or_else(|| "<inline>".to_string());
        self.confirming_delete = Some(PendingDelete {
            items: vec![(id, SharedString::new(row.title.clone()), SharedString::from(file))],
        });
        cx.notify();
    }

    /// Ask before deleting everything the reader ticked while curating.
    fn request_batch_delete(&mut self, cx: &mut Context<Self>) {
        let mut items = Vec::new();
        for id in self.picked.iter().copied() {
            let Some(row) = self.snapshot.sources.get(id as usize) else {
                continue;
            };
            let file = self
                .engine
                .config()
                .source_origin(id)
                .map(|(_, file)| file.to_string())
                .unwrap_or_else(|| "<inline>".to_string());
            items.push((id, SharedString::new(row.title.clone()), SharedString::from(file)));
        }
        if items.is_empty() {
            return;
        }
        items.sort_by(|a, b| a.1.cmp(&b.1));
        self.confirming_delete = Some(PendingDelete { items });
        cx.notify();
    }

    /// Ask before deleting, and say exactly what will be edited.
    fn request_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.target_source() else {
            return;
        };
        let Some(row) = self.snapshot.sources.get(id as usize) else {
            return;
        };
        let file = self
            .engine
            .config()
            .source_origin(id)
            .map(|(_, file)| file.to_string())
            .unwrap_or_else(|| "<inline>".to_string());
        self.confirming_delete =
            Some(PendingDelete {
                items: vec![(id, SharedString::new(row.title.clone()), SharedString::from(file))],
            });
        cx.notify();
    }

    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.confirming_delete = None;
        cx.notify();
    }

    /// Remove the feed from the file that defines it. The engine's watcher picks
    /// the change up and reloads, so the feed and its articles leave the window on
    /// the next publish. The history stays in the database — the engine hides rows
    /// whose source is gone — so re-adding the URL later restores the articles.
    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.confirming_delete.take() else {
            return;
        };
        // One write per file, however many feeds are being removed from it:
        // several edits in a row would leave a backup per edit and rewrite the
        // same text repeatedly.
        let mut by_file: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (id, _, file) in &pending.items {
            let Some((url, _)) = self.engine.config().source_origin(*id) else {
                continue;
            };
            by_file.entry(file.to_string()).or_default().push(url.to_string());
        }
        let mut removed = 0usize;
        let mut failures: Vec<String> = Vec::new();
        let root = self.engine.config().path().to_path_buf();
        for (file, urls) in &by_file {
            match artifeeds_core::dedupe::remove_sources_from_file(file, urls) {
                Ok(n) => {
                    removed += n;
                    // Record it in the file the reader owns as well: the file the
                    // subscription was in may be regenerated by the next build or
                    // re-import, and a deletion that only lives there comes back.
                    for url in urls {
                        if let Err(e) = artifeeds_core::config::append_removed_to_config(&root, url) {
                            failures.push(format!("记录删除失败：{e}"));
                        }
                    }
                }
                Err(e) => failures.push(e),
            }
        }
        let message = if failures.is_empty() {
            format!(
                "已删除 {removed} 个订阅（{} 个文件）· 历史保留，重新添加同一 URL 即可恢复",
                by_file.len()
            )
        } else {
            format!("删除了 {removed} 个，另有失败：{}", failures.join("；"))
        };
        tracing::info!(removed, files = by_file.len(), "delete feeds");
        self.picked.clear();
        self.toast = Some((SharedString::from(message), Instant::now()));
        cx.notify();
    }

    /// Sample the reading position and, every few seconds, remember it.
    ///
    /// Called from the frame loop while reading; the write is throttled because
    /// the database is a single writer shared with fetching and parsing.
    fn track_progress(&mut self, cx: &mut Context<Self>) {
        if !self.reading {
            return;
        }
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        // Only the embedded page can be sampled: GPUI's uniform-list handle can
        // be *told* a position but not asked for one. Text mode therefore restores
        // a position without ever downgrading the record.
        if self.read_mode != ReadMode::Original {
            return;
        }
        // JavaScript answers asynchronously, so the request is throttled and the
        // answer read on a later frame.
        #[cfg(target_os = "macos")]
        let fraction = match &self.web {
            Some(web) => {
                let due = self
                    .progress_polled_at
                    .is_none_or(|at| at.elapsed() >= PROGRESS_POLL_INTERVAL);
                if due {
                    web.request_scroll_progress();
                    self.progress_polled_at = Some(Instant::now());
                }
                web.scroll_progress()
            }
            None => 0.0,
        };
        #[cfg(not(target_os = "macos"))]
        let fraction = 0.0;
        self.progress_of = Some((article.id, fraction));
        let due = self
            .progress_saved_at
            .is_none_or(|at| at.elapsed() >= PROGRESS_SAVE_INTERVAL);
        if due {
            self.flush_progress();
            cx.notify();
        }
    }

    /// Write the sampled position to the database.
    fn flush_progress(&mut self) {
        if let Some((id, progress)) = self.progress_of.take() {
            self.engine.send(UiCommand::SaveProgress { id, progress });
            self.progress_saved_at = Some(Instant::now());
        }
    }

    /// How far the article on screen has been read, for the progress bar.
    fn current_progress(&self) -> f32 {
        self.selected_article().map(|a| a.progress).unwrap_or(0.0)
    }

    /// Show the blog an article came from: scope the list to that feed and bring
    /// its sidebar row into view. The answer to "what else is on this blog?"
    fn jump_to_blog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        let id = article.source;
        self.reading = false;
        self.hide_page();
        self.scope = Scope::Source(id);
        self.request_source_history(&self.scope.clone());
        self.rebuild_list();
        self.selected = 0;
        self.rebuild_sidebar();
        self.select(0, cx);
        // Reveal the row. The sidebar is virtualised, so this is a scroll to an
        // index — and it has to happen *after* the frame that lays out the new
        // row list, which is why it is deferred rather than done here.
        if let Some(ix) = self.sidebar.iter().position(|r| r.scope == Some(Scope::Source(id))) {
            let handle = self.side_scroll.clone();
            cx.on_next_frame(window, move |_app, _window, _cx| {
                handle.scroll_to_item(ix, ScrollStrategy::Center);
            });
        }
        self.toast = Some((
            SharedString::from(format!("已跳转到《{}》", article.source_title)),
            Instant::now(),
        ));
        cx.notify();
    }

    /// Ask the engine to save the page locally, unless the configuration turns
    /// archiving off or it is already running for this article.
    fn maybe_archive(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.archiving || !self.archive_on_open {
            return;
        }
        self.archiving = true;
        self.archive_note = Some(SharedString::from("正在保存原文到本地…"));
        self.engine.send(UiCommand::ArchiveArticle { id });
        cx.notify();
    }

    fn hide_page(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            web.set_visible(false);
        }
    }

    /// WebKit reports the outcome on a later run-loop pass. Poll briefly so the
    /// page appears (or the failure notice shows) without user input; the loop
    /// is bounded and stops as soon as the state settles.
    #[cfg(target_os = "macos")]
    fn spawn_page_poll(&mut self, cx: &mut Context<Self>) {
        if self.page_polling {
            return;
        }
        self.page_polling = true;
        cx.spawn(async move |weak, cx| {
            for tick in 0..200 {
                // The first moments decide how the transition feels, so poll
                // them closely and relax once the page is up.
                let wait = if tick < 30 { 50 } else { 150 };
                cx.background_executor().timer(Duration::from_millis(wait)).await;
                let state = webview::state();
                let settled = matches!(state, webview::LoadState::Loaded | webview::LoadState::Failed(_));
                if let webview::LoadState::Failed(message) = &state {
                    let message = message.clone();
                    let _ = weak.update(cx, |app, cx| {
                        app.web_notice = Some(SharedString::from(format!(
                            "原文加载失败（{message}），已切换为纯文本"
                        )));
                        app.read_mode = ReadMode::Text;
                        app.hide_page();
                        cx.notify();
                    });
                    break;
                }
                if weak.update(cx, |_app, cx| cx.notify()).is_err() {
                    break;
                }
                if settled {
                    break;
                }
            }
            weak.update(cx, |app, _cx| app.page_polling = false).ok();
        })
        .detach();
    }

    #[cfg(not(target_os = "macos"))]
    fn spawn_page_poll(&mut self, _cx: &mut Context<Self>) {}

    /// Ask the engine to save the page locally, unless the configuration turns
    #[cfg(not(target_os = "macos"))]
    fn hide_page(&mut self) {}

    /// Push the embedded page's geometry and visibility. Runs on every frame of
    /// reading mode, so the view follows window resizes and the toolbar.
    #[cfg(target_os = "macos")]
    fn sync_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The sign-in browser shares the web view with reading mode: one native
        // subview, laid out under whichever toolbar is on screen.
        if self.login {
            if self.web.is_none() {
                self.web = WebView::attach(window);
                if self.web.is_none() {
                    self.login_note = SharedString::from("无法创建内嵌浏览器");
                    self.login = false;
                    return;
                }
            }
            let Some(web) = &self.web else { return };
            if let Some((target, _)) = self.pending_url.take() {
                web.layout(READ_TOOLBAR_H as f64);
                web.load(&target);
            }
            web.layout(READ_TOOLBAR_H as f64);
            let state = webview::state();
            let ready = matches!(state, webview::LoadState::Loaded);
            let started = matches!(state, webview::LoadState::Loading);
            web.set_visible(ready || started);
            self.poll_zhihu_cookies(cx);
            return;
        }
        if !self.reading || self.read_mode != ReadMode::Original {
            self.hide_page();
            return;
        }
        if self.web.is_none() {
            self.web = WebView::attach(window);
            if self.web.is_none() {
                self.read_mode = ReadMode::Text;
                self.web_notice = Some(SharedString::from("无法创建内嵌浏览器，改为显示纯文本"));
                return;
            }
        }
        let Some(web) = &self.web else {
            return;
        };
        if let Some((target, from_disk)) = self.pending_url.take() {
            web.layout(READ_TOOLBAR_H as f64);
            if from_disk {
                web.load_file(&target);
            } else {
                web.load(&target);
            }
            // Only worth restoring if there is somewhere to restore to.
            self.restore_pending = self.selected_article().map(|a| a.progress).filter(|p| *p > 0.02);
        }
        // Put the reader back where they stopped — but only after the layout has
        // a document height, or the offset would be clamped to a blank page.
        if let Some(fraction) = self.restore_pending
            && matches!(webview::state(), webview::LoadState::Loaded)
        {
            web.set_scroll_progress(fraction);
            self.restore_pending = None;
        }
        web.layout(READ_TOOLBAR_H as f64);

        // Reveal the page as soon as a navigation *starts*, not when it has
        // fully finished: WebKit paints progressively, so waiting for `Loaded`
        // means staring at an empty panel for as long as the slowest image on
        // the page takes. The view is still hidden while the help overlay is up,
        // because the page is a native subview and would otherwise cover it.
        let state = webview::state();
        let ready = matches!(state, webview::LoadState::Loaded);
        let started = matches!(state, webview::LoadState::Loading);
        web.set_visible((ready || started) && !self.help);
        // Two numbers, because they answer different questions: when the page
        // became visible (what the eye waits for) and when it finished.
        if started
            && self.page_loaded_at.is_none()
            && let Some(at) = self.page_started_at
        {
            tracing::info!(ms = at.elapsed().as_millis() as u64, "page visible");
            self.page_loaded_at = Some(at);
        }
        if ready && let Some(logged) = self.page_loaded_at.take() {
            tracing::info!(ms = logged.elapsed().as_millis() as u64, "page fully loaded");
        }

        // At most four probes a second, and only while the page is on screen.
        let probe_due = self
            .page_probe_at
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250));
        if ready && probe_due {
            self.page_probe_at = Some(Instant::now());
            self.page_can_back = web.can_go_back();
            self.page_url = web.current_url().map(SharedString::from);
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn sync_page(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// Forward a paging key into the embedded page, then sample where it landed.
    fn scroll_page(&mut self, pages: f64, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            web.scroll_page(pages);
            web.request_scroll_progress();
            self.progress_polled_at = Some(Instant::now());
        }
        let _ = pages;
        cx.notify();
    }

    /// Put the article's original URL on the clipboard, ready to paste into a
    /// browser, a chat or a note.
    fn copy_link(&mut self, cx: &mut Context<Self>) {
        let Some(article) = self.selected_article().cloned() else {
            return;
        };
        if article.link.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(article.link.to_string()));
        self.toast = Some((
            SharedString::from(format!("已复制链接 · {}", article.link)),
            Instant::now(),
        ));
        cx.notify();
    }

    fn open_selected(&self, cx: &mut Context<Self>) {
        if let Some(a) = self.selected_article()
            && !a.link.is_empty()
        {
            cx.open_url(&a.link);
        }
    }

    fn send_search(&mut self, cx: &mut Context<Self>) {
        let query: Arc<str> = Arc::from(self.query.trim());
        if query.is_empty() {
            self.engine.send(UiCommand::ClearSearch);
            return;
        }
        self.pending_query = Some(query.to_string());
        let pending = query.to_string();
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            weak.update(cx, |app, _cx| {
                // Only fire if the query has not changed since.
                if app.pending_query.as_deref() == Some(pending.as_str()) {
                    app.engine.send(UiCommand::Search { query: Arc::from(pending.as_str()), limit: 4_000 });
                    app.pending_query = None;
                }
            })
            .ok();
        })
        .detach();
    }

    // -- key handling ------------------------------------------------------

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;

        // The sign-in browser is modal while it is up.
        if self.login {
            if k.key == "escape" || k.key == "\u{1b}" {
                self.close_login(cx);
            }
            return;
        }

        // The add overlay is modal. Its list owns the keyboard; `i` hands it to
        // the URL field, which then behaves like any other text field.
        if self.add_open {
            if self.add_input.read(cx).focus_handle(cx).is_focused(window) {
                if k.key == "escape" || k.key == "\u{1b}" {
                    // Esc leaves the panel, not just the field. Returning focus
                    // to the list and staying open made the key look broken:
                    // the hint promises Esc closes, and a modal that needs the
                    // same key twice reads as one you cannot leave. The field
                    // has its own clear button, so nothing is lost by this.
                    self.close_add(cx);
                } else if k.key == "enter" {
                    self.commit_add(cx);
                    self.focus.focus(window, cx);
                }
                return;
            }
            match k.key.as_str() {
                "escape" | "\u{1b}" | "n" => self.close_add(cx),
                "enter" => self.commit_add(cx),
                "i" => {
                    let handle = self.add_input.read(cx).focus_handle(cx);
                    handle.focus(window, cx);
                }
                "j" | "down" => {
                    if !self.add_found.is_empty() {
                        self.add_cursor = (self.add_cursor + 1).min(self.add_found.len() - 1);
                    }
                    cx.notify();
                }
                "k" | "up" => {
                    self.add_cursor = self.add_cursor.saturating_sub(1);
                    cx.notify();
                }
                "space" => {
                    if let Some((_, feed)) = self.add_found.get(self.add_cursor) {
                        let url = Arc::clone(&feed.url);
                        if !self.add_selected.remove(&url) {
                            self.add_selected.insert(url);
                        }
                    }
                    cx.notify();
                }
                "a" => {
                    if self.add_selected.len() == self.add_found.len() {
                        self.add_selected.clear();
                    } else {
                        self.add_selected = self.add_found.iter().map(|(_, f)| Arc::clone(&f.url)).collect();
                    }
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        // A destructive confirmation is modal: nothing else may happen while it
        // is on screen.
        if self.confirming_delete.is_some() {
            match k.key.as_str() {
                "enter" => self.confirm_delete(cx),
                "escape" | "\u{1b}" => self.cancel_delete(cx),
                _ => {}
            }
            return;
        }

        if m.platform && k.key == "q" {
            cx.quit();
            return;
        }
        if m.platform && k.key == "r" {
            self.engine.send(UiCommand::RefreshAll);
            cx.notify();
            return;
        }

        // Reading mode first: it is a modal surface, so its keys win.
        if self.reading {
            match k.key.as_str() {
                // The help overlay sits on top; Esc closes it before the mode.
                "escape" | "\u{1b}" | "h" | "left" | "backspace" if self.help => self.help = false,
                "escape" | "\u{1b}" | "h" | "left" | "backspace" => self.exit_reading(cx),
                "j" | "down" | "n" | "space" => self.move_selection(1, cx),
                "k" | "up" | "p" => self.move_selection(-1, cx),
                "g" => self.select(0, cx),
                "G" => {
                    let last = self.list.len().saturating_sub(1);
                    self.select(last, cx);
                }
                "s" => self.toggle_star(cx),
                "u" => {
                    if let Some(a) = self.selected_article().cloned() {
                        self.mark_read(&[a.id], !a.read);
                    }
                }
                "o" | "enter" => self.open_selected(cx),
                // Both are most useful *while* reading: draw another unread
                // article instead of going back for one, and convert the script
                // on the page in front of you.
                "x" => self.read_something_random(cx),
                "T" => self.toggle_simplify(cx),
                "v" => {
                    let next = match self.read_mode {
                        ReadMode::Original => ReadMode::Text,
                        ReadMode::Text => ReadMode::Original,
                    };
                    self.set_read_mode(next, cx);
                }
                "b" => self.jump_to_blog(window, cx),
                "y" => self.copy_link(cx),
                // The page is a native subview that refuses first responder, so
                // paging keys are forwarded to it rather than dropped.
                "pagedown" => self.scroll_page(1.0, cx),
                "pageup" => self.scroll_page(-1.0, cx),
                "home" => self.scroll_page(f64::NEG_INFINITY, cx),
                "end" => self.scroll_page(f64::INFINITY, cx),
                "r" => {
                    #[cfg(target_os = "macos")]
                    if let Some(web) = &self.web {
                        web.reload();
                    }
                }
                "[" if m.platform => {
                    #[cfg(target_os = "macos")]
                    if let Some(web) = &self.web {
                        web.go_back();
                    }
                }
                "?" => self.help = true,
                _ => {}
            }
            cx.notify();
            return;
        }

        // While a field has focus the component owns every key — text, selection,
        // clipboard and IME composition are all its job. `j` must not scroll the
        // list while someone is typing `java` into a filter.
        //
        // Escape is the exception, and has to be handled here: the field consumes
        // it (to drop a composition), but a filter you cannot leave with Esc is a
        // trap — the next letter goes into the box instead of running a command.
        if self.side_input.read(cx).focus_handle(cx).is_focused(window) {
            if k.key == "escape" || k.key == "\u{1b}" || k.key == "enter" {
                self.side_input.update(cx, |state, cx| state.set_value("", window, cx));
                self.side_filter_raw.clear();
                self.side_filter.clear();
                self.rebuild_sidebar();
                self.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }
        if self.search_focus {
            if k.key == "enter" || k.key == "escape" || k.key == "\u{1b}" {
                self.search_focus = false;
                self.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }

        match k.key.as_str() {
            "j" | "down" => self.move_selection(1, cx),
            "k" | "up" => self.move_selection(-1, cx),
            "g" => self.select(0, cx),
            "G" => {
                let last = self.list.len().saturating_sub(1);
                self.select(last, cx);
            }
            "enter" => self.enter_reading(cx),
            "o" => self.open_selected(cx),
            "s" => self.toggle_star(cx),
            "b" => self.jump_to_blog(window, cx),
            "d" if self.curating && !self.picked.is_empty() => self.request_batch_delete(cx),
            "d" => self.request_delete(cx),
            "y" => self.copy_link(cx),
            "n" => self.open_add(window, cx),
            "x" => self.read_something_random(cx),
            "T" => self.toggle_simplify(cx),
            "u" => {
                if let Some(a) = self.selected_article().cloned() {
                    self.mark_read(&[a.id], !a.read);
                    self.move_selection(1, cx);
                }
            }
            "a" => {
                let scope = self.scope.clone();
                let before = artifeeds_core::config::now_unix();
                let ui_scope = match scope {
                    Scope::Group(g) => artifeeds_core::engine::Scope::Group(g),
                    Scope::Source(id) => artifeeds_core::engine::Scope::Source(id),
                    // Marking a tag read means marking its feeds read; the
                    // engine only knows groups and feeds.
                    Scope::Tag(t) => artifeeds_core::engine::Scope::Group(t),
                    _ => artifeeds_core::engine::Scope::All,
                };
                self.engine.send(UiCommand::MarkAllRead { scope: ui_scope, before });
            }
            "r" => {
                self.engine.send(UiCommand::RefreshAll);
            }
            "R" => {
                match self.scope.clone() {
                    Scope::Group(g) => self.engine.send(UiCommand::RefreshGroup(g)),
                    Scope::Source(id) => self.engine.send(UiCommand::RefreshSource(id)),
                    _ => self.engine.send(UiCommand::RefreshAll),
                }
            }
            "t" => {
                let next = self.dim.next();
                self.set_dim(next, cx);
            }
            "z" => self.toggle_empty_blogs(cx),
            "m" => self.toggle_curating(cx),
            "*" => self.feature_target(cx),
            "1" => self.set_scope(Scope::All, cx),
            "2" => self.set_scope(Scope::Unread, cx),
            "3" => self.set_scope(Scope::Starred, cx),
            "4" => self.set_scope(Scope::History, cx),
            "escape" => {
                if self.help {
                    self.help = false;
                } else if self.curating {
                    self.toggle_curating(cx);
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.send_search(cx);
                }
            }
            "?" => self.help = !self.help,
            "c" => self.engine.send(UiCommand::ReloadConfig),
            _ => {}
        }
        cx.notify();
    }

    // -- rendering ---------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {

        // `Input` from GPUI Kit: text editing, selection, clipboard and IME
        // composition are the component's job, not ours.
        let search_box = div()
            .w(px(320.))
            .flex_none()
            .child(
                Input::new(&self.search_input)
                    .small()
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).xsmall()),
            );

        let random = Button::new("random-unread")
            .label("随机 x")
            .outline()
            .xsmall()
            .tooltip("从未读里随机抽一篇打开")
            .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                app.read_something_random(cx);
            }));

        let script = Button::new("simplify-toggle")
            .label(if self.simplify { "简 T" } else { "繁 T" })
            .outline()
            .xsmall()
            .tooltip("繁体原文转为简体显示（不改变已存内容）")
            .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                app.toggle_simplify(cx);
            }));

        let refresh = Button::new("refresh")
            .label("refresh  r")
            .outline()
            .xsmall()
            .tooltip("重新抓取全部源")
            .on_click(cx.listener(|app, _: &ClickEvent, _window, _cx| {
                app.engine.send(UiCommand::RefreshAll);
            }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .h(px(40.))
            .flex_none()
            .pl(px(84.))
            .pr(px(12.))
            .bg(theme::toolbar())
            .border_b(px(1.))
            .border_color(theme::border())
            .child(
                div()
                    .typo(type_scale::LABEL.weight(FontWeight::SEMIBOLD).with(type_scale::Emphasis::Primary))
                    .child("Artifeeds"),
            )
            .child(div().flex_1())
            .child(search_box)
            .child(random)
            .child(script)
            .child(refresh)
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> Div {
        let count = self.sidebar.len();
        let selected_scope = self.scope.clone();
        let list = uniform_list("sidebar", count, cx.processor(move |app: &mut App, range: std::ops::Range<usize>, _window, cx| {
            let mut rows = Vec::with_capacity(range.len());
            for ix in range {
                let row = &app.sidebar[ix];
                let active = row.scope.as_ref() == Some(&selected_scope);
                let scope = row.scope.clone();
                let depth = row.depth;
                let is_group = row.is_group;
                let is_section = row.is_section;

                // Section headers are labels: full width, no hit target.
                if is_section {
                    rows.push(
                        div()
                            .id(ix)
                            .flex()
                            .flex_row()
                            .items_center()
                            .w_full()
                            .h(px(SIDE_ROW_H))
                            .px(px(10.))
                            .bg(theme::panel_alt())
                            .border_b(px(1.))
                            .border_color(theme::border_soft())
                            .typo(type_scale::CAPTION.weight(FontWeight::SEMIBOLD))
                            .child(row.label.clone()),
                    );
                    continue;
                }

                // One inset pill per row keeps the left and right edges of the
                // column visually straight.
                let mut el = div()
                    .id(ix)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .w_full()
                    .h(px(SIDE_ROW_H - 2.))
                    .pl(px(4. + depth as f32 * 12.))
                    .pr(px(10.))
                    .rounded(px(5.))
                    .typo(type_scale::LABEL);
                el = if active {
                    el.bg(theme::accent_soft()).text_color(theme::text()).cursor_pointer()
                } else if is_group {
                    el.text_color(theme::text_dim()).cursor_pointer().hover(|s| s.bg(theme::hover()))
                } else {
                    el.text_color(theme::text_dim()).cursor_pointer().hover(|s| s.bg(theme::hover()))
                };
                // The source behind this row, when it is a feed rather than a
                // group or a fixed scope: only a feed can be featured or deleted.
                let feed = match &scope {
                    Some(Scope::Source(id)) => Some(*id),
                    _ => None,
                };
                if let Some(scope) = scope.clone() {
                    el = el.on_click(cx.listener(move |app, ev: &ClickEvent, _window, cx| {
                        // Curating: a feed row is a checkbox, and the rails are
                        // disabled so a mis-aimed click cannot delete anything
                        // while the reader is ticking through a list.
                        if app.curating && let Some(id) = feed {
                            if !app.picked.remove(&id) {
                                app.picked.insert(id);
                            }
                            cx.notify();
                            return;
                        }
                        // A nested hit target inside a list row never receives the
                        // click — the row captures the mouse down — so the row
                        // interprets its own right-hand margin instead.
                        let x: f32 = ev.position().x.into();
                        if let Some(id) = feed {
                            // The count column is deliberately inert: clicking a
                            // number must never offer to delete the feed.
                            if (SIDE_DELETE_X - SIDE_ACTION_SLACK..SIDE_COUNT_X).contains(&x) {
                                app.request_delete_of(id, cx);
                                return;
                            }
                            if (SIDE_FEATURE_X - SIDE_ACTION_SLACK
                                ..SIDE_DELETE_X - SIDE_ACTION_SLACK)
                                .contains(&x)
                            {
                                app.toggle_featured(id, cx);
                                return;
                            }
                        }
                        app.set_scope(scope.clone(), cx);
                    }));
                }

                // One fixed-width marker column, shared by the health dot and
                // the fold arrow: a header has no health, a feed has no children,
                // and both must start their text on the same x as the fixed
                // scopes above them. The arrow is its own hit target — the arrow
                // folds, the rest of the row scopes.
                if let Some(key) = row.collapse_key.clone() {
                    el = el.child(
                        div()
                            .id(ElementId::Name(SharedString::from(format!("fold-{ix}"))))
                            .w(px(SIDE_MARKER_W))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .cursor_pointer()
                            .text_color(theme::text_faint())
                            .hover(|s| s.text_color(theme::text()))
                            .on_click(cx.listener(move |app, _: &ClickEvent, _window, cx| {
                                cx.stop_propagation();
                                app.toggle_section(Arc::clone(&key), cx);
                            }))
                            .child(if row.collapsed { "▸" } else { "▾" }),
                    );
                } else if app.curating && let Some(id) = feed {
                    let ticked = app.picked.contains(&id);
                    el = el.child(
                        div()
                            .w(px(SIDE_MARKER_W))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .typo(type_scale::CAPTION)
                            .text_color(if ticked { theme::accent() } else { theme::text_faint() })
                            .child(if ticked { "☑" } else { "☐" }),
                    );
                } else if let Some(health) = row.health {
                    el = el.child(
                        div()
                            .w(px(SIDE_MARKER_W))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .text_color(theme::health_color(health))
                            .child(health.dot()),
                    );
                } else {
                    el = el.child(div().w(px(SIDE_MARKER_W)).flex_none());
                }
                el = el.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .font_weight(if is_group { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                        .child(row.label.clone()),
                );
                if let Some(id) = feed {
                    let featured = app
                        .snapshot
                        .sources
                        .get(id as usize)
                        .map(|s| s.featured)
                        .unwrap_or(false);
                    el = el
                        .child(
                            div()
                                .w(px(SIDE_ACTION_W))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .typo(type_scale::CAPTION)
                                .text_color(if featured { theme::accent() } else { theme::text_faint() })
                                .child(if featured { "★" } else { "☆" }),
                        )
                        .child(
                            div()
                                .w(px(SIDE_ACTION_W))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .typo(type_scale::CAPTION)
                                .text_color(theme::text_faint())
                                .child("✕"),
                        );
                }
                el = el
                    .child(
                        div()
                            .w(px(SIDE_COUNT_W))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .typo(type_scale::CAPTION)
                            .tabular()
                            .text_color(if row.count_accent { theme::accent() } else { theme::text_faint() })
                            .child(row.count_label.clone()),
                    );
                rows.push(el);
            }
            rows
        }))
        .track_scroll(&self.side_scroll)
        .w_full()
        .h_full();

        // Curation mode says what it is and what it costs: a bar naming the
        // count, because "how many did I tick?" is the question a batch delete
        // makes the reader ask.
        let curation = if self.curating {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(6.))
                .h(px(26.))
                .px(px(10.))
                .flex_none()
                .bg(theme::accent_soft())
                .typo(type_scale::CAPTION)
                .text_color(theme::text())
                .child(SharedString::from(format!("整理模式 · 已选 {}", self.picked.len())))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(10.))
                        .flex_none()
                        .child(
                            div()
                                .text_color(if self.picked.is_empty() {
                                    theme::text_faint()
                                } else {
                                    theme::red()
                                })
                                .child("d 删除"),
                        )
                        .child(div().text_color(theme::text_dim()).child("* 加精"))
                        .child(div().text_color(theme::text_faint()).child("esc 退出")),
                )
        } else {
            div().h(px(0.)).flex_none()
        };

        // Two rows: the controls, then a filter box. 1 400 feeds is a list you
        // search, not one you scroll.
        let controls = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(6.))
            .h(px(28.))
            .px(px(10.))
            .typo(type_scale::CAPTION)
            .child(
                div().truncate().child(SharedString::from(format!(
                    "{} 源",
                    self.snapshot.stats.sources_total
                ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .flex_none()
                    .child(pill(
                        "add-feed",
                        "+ 添加",
                        false,
                        cx.listener(|app, _: &ClickEvent, window, cx| app.open_add(window, cx)),
                    ))
                    .child(pill(
                        "empty-toggle",
                        if self.hide_empty_blogs { "隐藏空源" } else { "空源置底" },
                        self.hide_empty_blogs,
                        cx.listener(|app, _: &ClickEvent, _window, cx| app.toggle_empty_blogs(cx)),
                    ))
                    .child(pill(
                        "dim-toggle",
                        self.dim.label(),
                        false,
                        cx.listener(|app, _: &ClickEvent, _window, cx| {
                            let next = app.dim.next();
                            app.set_dim(next, cx);
                        }),
                    )),
            );

        let filter = div().px(px(8.)).pb(px(6.)).child(
            Input::new(&self.side_input)
                .small()
                .cleanable(true)
                .prefix(Icon::new(IconName::Search).xsmall()),
        );

        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_none()
            .bg(theme::panel())
            .border_r(px(1.))
            .border_color(theme::border())
            .child(controls)
            .child(curation)
            .child(filter)
            .child(div().flex_1().min_h(px(0.)).child(list))
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> Div {
        let count = self.list.len();
        let selected = self.selected;

        let list = uniform_list("articles", count, cx.processor(move |app: &mut App, range: std::ops::Range<usize>, _window, cx| {
            let items: Vec<Arc<Article>> = app.list[range.clone()].to_vec();
            let mut rows = Vec::with_capacity(items.len());
            for (offset, article) in items.into_iter().enumerate() {
                let ix = range.start + offset;
                let active = ix == selected;
                let read = article.read;
                let starred = article.starred;

                // A single-column row whose height is fixed: `uniform_list`
                // measures the first item and needs that number to be exact.
                // The active marker is a left border rather than a child, so
                // nothing can change the measured height.
                let mut row = div()
                    .id(ix)
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(3.))
                    .w_full()
                    .h(px(ARTICLE_ROW_H))
                    .pl(px(11.))
                    .pr(px(14.))
                    .border_l(px(3.))
                    .border_b(px(1.))
                    .cursor_pointer();
                // One border colour for both edges: accent marks the selection,
                // a faint tick otherwise (same 3 px gutter either way, so rows
                // never shift).
                row = if active {
                    row.bg(theme::selected()).border_color(theme::accent())
                } else {
                    row.border_color(theme::border_soft()).hover(|s| s.bg(theme::hover()))
                };
                row = row.on_click(cx.listener(move |app, ev: &ClickEvent, window, cx| {
                    // An overlay is modal: a click that leaked past it must not
                    // also open an article behind it.
                    if app.modal_open() {
                        return;
                    }
                    app.select(ix, cx);
                    // The glyph at the row's right edge means "this blog only".
                    // It is a zone of the row rather than a nested hitbox,
                    // because GPUI delivers a click to the element that captured
                    // the mouse down — the row — and a nested listener inside a
                    // list item never sees it.
                    let width: f32 = window.bounds().size.width.into();
                    let x: f32 = ev.position().x.into();
                    if x >= width - (ARTICLE_JUMP_W + 22.0) {
                        app.jump_to_blog(window, cx);
                    } else {
                        // Clicking an article is a request to read it: take over
                        // the whole window, `Esc` or the back button returns.
                        app.enter_reading(cx);
                    }
                }));

                // The title leads: larger, brighter and heavier than anything
                // else in the row. In the list the title *is* the content.
                let mut title_row = div().flex().flex_row().items_center().gap(px(8.));
                title_row = title_row
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .flex_none()
                            .rounded(px(3.))
                            .bg(if read { theme::border() } else { theme::accent() }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .typo(if read {
                                type_scale::ROW_TITLE
                                    .weight(FontWeight::NORMAL)
                                    .with(type_scale::Emphasis::Secondary)
                            } else {
                                type_scale::ROW_TITLE
                            })
                            .child(app.zh(&article.title)),
                    );
                if starred {
                    title_row = title_row.child(
                        div()
                            .flex_none()
                            .typo(type_scale::CAPTION)
                            .text_color(theme::amber())
                            .child("★"),
                    );
                }
                title_row = title_row.child(
                    div()
                        .w(px(ARTICLE_AGE_W))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .typo(type_scale::CAPTION)
                        .tabular()
                        .child(SharedString::new(article.age.clone())),
                );
                // Every row carries the way into its blog, because the question
                // ("what else has this blog written?") arrives while reading the
                // list, not at the sidebar.
                title_row = title_row.child(
                    div()
                        .id(("to-blog", ix))
                        .flex_none()
                        .w(px(ARTICLE_JUMP_W))
                        .flex()
                        .justify_end()
                        .typo(type_scale::CAPTION)
                        .text_color(theme::text_faint())
                        .child("⤴"),
                );
                row = row.child(title_row);

                // A preview only earns its line when it says something the title
                // does not: feeds routinely repeat the title as the summary, and
                // the repetition is what made the list feel dense.
                if preview_is_useful(&article.title, &article.preview) {
                    row = row.child(
                        div()
                            .pl(px(14.))
                            .typo(type_scale::BODY_SM)
                            .truncate()
                            .child(app.zh(&article.preview)),
                    );
                }

                // Caption: where it came from, nothing more.
                let mut meta = div().flex().flex_row().items_center().gap(px(6.)).pl(px(14.));
                // The feed name is a link: clicking it answers "what else is on
                // this blog?" without going back to the sidebar.
                meta = meta.child(
                    div()
                        .id(("jump", ix))
                        .flex_none()
                        .typo(type_scale::CAPTION)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme::accent()))
                        .on_click(cx.listener(
                            move |app: &mut App, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                app.select(ix, cx);
                                app.jump_to_blog(window, cx);
                            },
                        ))
                        .child(app.zh(&article.source_title)),
                );
                if let Some(tag) = app
                    .tags_by_source
                    .get(article.source as usize)
                    .and_then(|tags| tags.first())
                    .cloned()
                {
                    meta = meta.child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(theme::panel_alt())
                            .typo(type_scale::CAPTION)
                            .child(app.zh(&format!("#{tag}"))),
                    );
                }
                row = row.child(meta);

                // How far into this one the reader got. Shown for articles that
                // are *partly* read: a finished one needs no marker, and neither
                // does an untouched one.
                if article.progress > 0.02 && article.progress < 0.95 {
                    let width = (ARTICLE_PROGRESS_W * article.progress).max(2.0);
                    row = row.child(
                        div()
                            .mt(px(2.))
                            .w(px(width))
                            .h(px(2.))
                            .flex_none()
                            .rounded(px(1.))
                            .bg(theme::accent().opacity(0.55)),
                    );
                }

                rows.push(row);
            }
            rows
        }))
        .track_scroll(&self.list_scroll)
        .w_full()
        .h_full();

        // Header: scope, its tags as chips, then the keyboard hint.
        let scope_label = self.scope.label();
        let scope_tags = self.scope_tags();
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .h(px(34.))
            .flex_none()
            .px(px(14.))
            .bg(theme::panel_alt())
            .border_b(px(1.))
            .border_color(theme::border())
            .typo(type_scale::LABEL)
            .child(
                div()
                    .flex_none()
                    .typo(type_scale::LABEL.with(type_scale::Emphasis::Primary))
                    .tabular()
                    .child(SharedString::from(format!("{scope_label}  ({count})"))),
            );
        if !scope_tags.is_empty() {
            let mut chips = div().flex().flex_row().items_center().gap(px(4.)).min_w(px(0.));
            for tag in scope_tags.iter().take(8) {
                chips = chips.child(tag_chip(self.zh(&format!("#{tag}"))));
            }
            if scope_tags.len() > 8 {
                chips = chips.child(
                    div().flex_none().text_color(theme::text_faint()).child(SharedString::from(format!(
                        "+{}",
                        scope_tags.len() - 8
                    ))),
                );
            }
            header = header.child(chips);
        }
        let hint = match self.toast.as_ref().filter(|(_, at)| at.elapsed() < TOAST_TTL) {
            Some((message, _)) => (message.clone(), theme::amber()),
            None => (
                if self.query.is_empty() {
                    self.scope_subtitle().unwrap_or_else(|| {
                        SharedString::from("⏎ 阅读 · x 随机 · ⤴/b 只看这个博客 · m 整理 · ? 帮助")
                    })
                } else {
                    SharedString::from(format!("搜索：{}", self.query))
                },
                theme::text_faint(),
            ),
        };
        let selected_source = self.selected_article().map(|a| a.source);
        let already_scoped = matches!(self.scope, Scope::Source(_));
        header = header.child(div().flex_1().min_w(px(0.)));
        if let Some(id) = selected_source.filter(|_| !already_scoped) {
            header = header.child(
                Button::new("only-this-blog")
                    .label("只看这个博客")
                    .ghost()
                    .xsmall()
                    .tooltip("把这个文章所属博客单独列出来（快捷键 b）")
                    .on_click(cx.listener(move |app: &mut App, _: &ClickEvent, window, cx| {
                        app.set_scope(Scope::Source(id), cx);
                        app.jump_to_blog(window, cx);
                    })),
            );
        }
        header = header.child(
            div()
                .flex_none()
                .truncate()
                .typo(type_scale::CAPTION)
                .text_color(hint.1)
                .child(hint.0),
        );

        let body = if count == 0 {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .typo(type_scale::BODY_SM)
                .child(if self.snapshot.stats.sources_total == 0 {
                    "还没有配置任何源。编辑 feeds.toml，保存即热重载。"
                } else {
                    "这里还没有文章。按 r 抓取，或点某个博客看它自己的历史。"
                })
        } else {
            div().flex().flex_col().flex_1().min_h(px(0.)).child(list)
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .bg(theme::bg())
            .child(header)
            .child(body)
    }


    fn render_reader_header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(READ_PAD / 2.0))
            .pt(px(26.))
            .pb(px(12.))
            .border_b(px(1.))
            .border_color(theme::border_soft())
            .child(
                div()
                    .typo(type_scale::TITLE)
                    .child(if self.simplify {
                        SharedString::from(artifeeds_core::text::to_simplified(&self.reader_title))
                    } else {
                        self.reader_title.clone()
                    }),
            )
            .child(
                div().typo(type_scale::LABEL).child(self.reader_meta.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .id("open-browser")
                            .typo(type_scale::LABEL)
                            .text_color(theme::accent())
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme::text()))
                            .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                                app.open_selected(cx);
                            }))
                            .child("open in browser  o"),
                    )
                    .child(
                        div()
                            .typo(type_scale::CAPTION)
                            .truncate()
                            .child(self.reader_link.clone()),
                    ),
            )
    }

    /// The article body: a virtualised list of pre-wrapped fixed-height lines.
    fn render_reader_lines(&mut self, cx: &mut Context<Self>) -> Div {
        let (count, lines) = match &self.content {
            Some((_, lines)) => (lines.len(), lines.clone()),
            None => (0, Vec::new()),
        };

        if count == 0 {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .typo(type_scale::BODY_SM)
                .child(if self.list.is_empty() { "没有文章。" } else { "正在读取原文…" });
        }

        let list = uniform_list(
            "reader",
            count,
            cx.processor(move |_app: &mut App, range: std::ops::Range<usize>, _window, _cx| {
                let mut rows = Vec::with_capacity(range.len());
                for ix in range {
                    let line = lines.get(ix).cloned().unwrap_or_default();
                    rows.push(
                        div()
                            .h(px(READ_LINE_H))
                            .px(px(READ_PAD / 2.0))
                            .typo(type_scale::PROSE)
                            .child(line),
                    );
                }
                rows
            }),
        )
        .track_scroll(&self.reader_scroll)
        .h_full();

        div().flex().flex_col().flex_1().min_h(px(0.)).child(list)
    }

    /// Full-window reading surface: a slim toolbar, then the article centred in
    /// a fixed-width column so line length stays comfortable.
    fn render_reading(&mut self, window_px: f32, cx: &mut Context<Self>) -> Div {
        let column = Self::text_column_px(window_px);
        let position = if self.list.is_empty() {
            SharedString::default()
        } else {
            SharedString::from(format!("{} / {}", self.selected + 1, self.list.len()))
        };
        let starred = self.selected_article().map(|a| a.starred).unwrap_or(false);
        let read = self.selected_article().map(|a| a.read).unwrap_or(true);
        let featured_source = self
            .selected_article()
            .and_then(|a| self.snapshot.sources.get(a.source as usize))
            .map(|row| row.featured)
            .unwrap_or(false);

        // Only shown once the reader has followed a link inside the page.
        let mut page_back = div().flex_none();
        if self.page_can_back {
            page_back = page_back.child(
                div()
                    .id("page-back")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .bg(theme::panel())
                    .border(px(1.))
                    .border_color(theme::border())
                    .typo(type_scale::LABEL)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                    .on_click(cx.listener(|app, _: &ClickEvent, _window, _cx| {
                        #[cfg(target_os = "macos")]
                        if let Some(web) = &app.web {
                            web.go_back();
                        }
                    }))
                    .child("‹ 网页后退"),
            );
        }

        let back = div()
            .id("reading-back")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .h(px(26.))
            .px(px(10.))
            .rounded(px(6.))
            .bg(theme::panel())
            .border(px(1.))
            .border_color(theme::border())
            .typo(type_scale::LABEL)
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
            .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                app.exit_reading(cx);
            }))
            .child("← 返回")
            .child(div().text_color(theme::text_faint()).child("esc"));

        let action = |id: &'static str, label: SharedString, active: bool| {
            let mut el = div()
                .id(id)
                .flex_none()
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .typo(type_scale::LABEL)
                .cursor_pointer();
            el = if active {
                el.bg(theme::accent_soft()).text_color(theme::accent())
            } else {
                el.bg(theme::panel())
                    .border(px(1.))
                    .border_color(theme::border())
                    .text_color(theme::text_dim())
                    .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
            };
            el.child(label)
        };

        // 原文 / 纯文本 segment. `原文` is the original page in an embedded
        // WebKit view; `纯文本` is the distilled text this app stored.
        let archive_note = self.archive_note.clone();
        let mut dim_pill = div()
            .id("read-mode")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(6.))
            .bg(theme::panel())
            .border(px(1.))
            .border_color(theme::border())
            .cursor_pointer()
            .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                let next = match app.read_mode {
                    ReadMode::Original => ReadMode::Text,
                    ReadMode::Text => ReadMode::Original,
                };
                app.set_read_mode(next, cx);
            }));
        for mode in [ReadMode::Original, ReadMode::Text] {
            let active = mode == self.read_mode;
            let mut seg = div()
                .flex_none()
                .px(px(8.))
                .h(px(20.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .typo(type_scale::LABEL);
            seg = if active {
                seg.bg(theme::accent_soft()).text_color(theme::accent())
            } else {
                seg.text_color(theme::text_faint())
            };
            dim_pill = dim_pill.child(seg.child(mode.label()));
        }

        // A hairline of progress. It has to live *inside* the toolbar, on its
        // bottom edge: the page is a native subview that starts exactly where the
        // toolbar ends and covers everything below, so a sibling line underneath
        // would never be seen.
        let progress = div()
            .absolute()
            .left_0()
            .bottom_0()
            .h(px(2.))
            .w(relative(self.current_progress()))
            .bg(theme::accent());

        let toolbar = div()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .h(px(READ_TOOLBAR_H))
            .flex_none()
            .pl(px(84.))
            .pr(px(14.))
            .bg(theme::toolbar())
            .border_b(px(1.))
            .border_color(theme::border())
            .child(back)
            .child(
                div()
                    .truncate()
                    .typo(type_scale::CAPTION)
                    .child(match (&self.page_url, self.read_mode) {
                        (Some(url), ReadMode::Original) => url.clone(),
                        _ => SharedString::new(
                            self.selected_article().map(|a| a.source_title.clone()).unwrap_or_default(),
                        ),
                    }),
            )
            .child(page_back)
            .child(
                div().truncate().typo(type_scale::CAPTION).children(archive_note),
            )
            .child(div().flex_1())
            .child(dim_pill)
            .child(
                div()
                    .flex_none()
                    .typo(type_scale::CAPTION)
                    .tabular()
                    .child(SharedString::from(format!("j / k 切换 · {position}"))),
            )
            .child(
                action("r-star", SharedString::from(if starred { "★ 已收藏" } else { "☆ 收藏" }), starred)
                    .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| app.toggle_star(cx))),
            )
            // Featuring the *feed* while reading its article: the moment you
            // decide a blog is worth keeping is the moment you are reading it.
            .child(
                action(
                    "r-feature",
                    SharedString::from(if featured_source { "★ 已加精" } else { "☆ 加精" }),
                    featured_source,
                )
                .on_click(cx.listener(|app, _: &ClickEvent, _window, cx| {
                    if let Some(id) = app.target_source() {
                        app.toggle_featured(id, cx);
                    }
                })),
            )
            .child(
                action("r-read", SharedString::from(if read { "已读" } else { "未读" }), read).on_click(
                    cx.listener(|app, _: &ClickEvent, _window, cx| {
                        if let Some(a) = app.selected_article().cloned() {
                            let now = a.read;
                            app.mark_read(&[a.id], !now);
                        }
                        cx.notify();
                    }),
                ),
            );

        let header = self.render_reader_header(cx);
        let body = self.render_reader_lines(cx);

        let mut column_el = div()
            .flex()
            .flex_col()
            .w(px(column))
            .h_full()
            .bg(theme::panel())
            .border_l(px(1.))
            .border_r(px(1.))
            .border_color(theme::border_soft());
        if let Some(notice) = &self.web_notice {
            column_el = column_el.child(
                div()
                    .px(px(READ_PAD / 2.0))
                    .py(px(8.))
                    .typo(type_scale::BODY_SM)
                    .text_color(theme::amber())
                    .child(notice.clone()),
            );
        }
        column_el = column_el.child(header).child(body);

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(toolbar.child(progress))
            // The distilled text is rendered unconditionally *behind* the
            // embedded page: while the page loads (or if it fails) the reader
            // still has something to read, and the native view simply covers it
            // when WebKit reports success.
            .child(div().flex().flex_row().flex_1().min_h(px(0.)).justify_center().child(column_el))
    }

    fn render_status(&self) -> Div {
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .h(px(24.))
            .flex_none()
            .px(px(12.))
            .bg(theme::toolbar())
            .border_t(px(1.))
            .border_color(theme::border())
            .typo(type_scale::CAPTION)
            .tabular()
            .child(self.status_text.clone());
        if self.simplify {
            bar = bar.child(
                div()
                    .flex_none()
                    .text_color(theme::accent())
                    .child("简"),
            );
        }
        if let Some(problem) = &self.problem_text {
            bar = bar.child(
                div()
                    .truncate()
                    .text_color(theme::amber())
                    .child(SharedString::from(format!("⚠ {problem}"))),
            );
        }
        bar
    }

    /// The Zhihu sign-in: a real browser on zhihu.com, inside the app.
    ///
    /// Everything below the toolbar is the web view, which is a native subview
    /// rather than a GPUI element — the toolbar is what tells the reader that
    /// this is a step in adding subscriptions and not a random web page.
    fn render_login(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .h(px(READ_TOOLBAR_H))
                    // The window has a full-size content view, so the traffic
                    // lights sit on top of this row's left end.
                    .pl(px(84.))
                    .pr(px(14.))
                    .flex_none()
                    .bg(theme::toolbar())
                    .border_b(px(1.))
                    .border_color(theme::border())
                    .child(
                        div()
                            .typo(type_scale::LABEL.with(type_scale::Emphasis::Primary))
                            .child("登录知乎"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .typo(type_scale::CAPTION)
                            .text_color(theme::text_faint())
                            .child(self.login_note.clone()),
                    )
                    .child(
                        Button::new("zhihu-login-done")
                            .label("我已完成登录")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|app: &mut App, _: &ClickEvent, _window, cx| {
                                app.finish_zhihu_login(cx);
                            })),
                    )
                    .child(
                        div()
                            .typo(type_scale::CAPTION)
                            .text_color(theme::text_faint())
                            .child("esc 取消"),
                    ),
            )
            // The web view paints here; the rest is backdrop.
            .child(div().flex_1().min_h(px(0.)))
    }

    /// The add-a-subscription overlay: a URL field for typing, and the blogs
    /// found on the current blog's links page, ticked and ready to add.
    fn render_add(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col().gap(px(2.)).max_h(px(320.));
        if self.add_found.is_empty() {
            list = list.child(
                div()
                    .py(px(10.))
                    .typo(type_scale::BODY_SM)
                    .child(if self.add_scanning {
                        SharedString::from("扫描中…")
                    } else {
                        SharedString::from("没有找到可订阅的博客")
                    }),
            );
        }
        for (ix, (candidate, feed)) in self.add_found.iter().enumerate() {
            let ticked = self.add_selected.contains(&feed.url);
            let cursor = ix == self.add_cursor;
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .h(px(26.))
                .rounded(px(4.));
            row = if cursor { row.bg(theme::selected()) } else { row };
            list = list.child(
                row.child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .typo(type_scale::LABEL)
                        .text_color(if ticked { theme::accent() } else { theme::text_faint() })
                        .child(if ticked { "✓" } else { "·" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .typo(type_scale::LABEL.with(type_scale::Emphasis::Primary))
                        .child(SharedString::new(candidate.label.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(230.))
                        .truncate()
                        .typo(type_scale::CAPTION)
                        .child(SharedString::new(feed.url.clone())),
                ),
            );
        }

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .w(px(640.))
            .p(px(20.))
            .rounded(px(10.))
            .bg(theme::panel())
            .border(px(1.))
            .border_color(theme::border())
            .child(
                div()
                    .typo(type_scale::ROW_TITLE.with(type_scale::Emphasis::Primary))
                    .child("添加博客"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .typo(type_scale::LABEL)
                            .text_color(theme::text_faint())
                            .child("手动输入（i 聚焦，回车识别并添加）"),
                    )
                    .child(
                        Input::new(&self.add_input)
                            .small()
                            .cleanable(true)
                            .prefix(Icon::new(IconName::Globe).xsmall()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .typo(type_scale::LABEL)
                            .text_color(theme::text_faint())
                            .child(self.add_heading.clone()),
                    )
                    .child(
                        Button::new("zhihu-collections")
                            .label("知乎收藏")
                            .ghost()
                            .xsmall()
                            .tooltip("登录知乎，把收藏夹作为独立的来源加入")
                            .on_click(cx.listener(|app: &mut App, _: &ClickEvent, _window, cx| {
                                app.zhihu_collections(cx);
                            })),
                    ),
            )
            .child(list)
            .child(
                div()
                    .typo(type_scale::CAPTION)
                    .text_color(theme::text_faint())
                    .child(self.add_status.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(16.))
                    .pt(px(2.))
                    .child(
                        div()
                            .typo(type_scale::LABEL)
                            .text_color(theme::accent())
                            .child("⏎ 添加选中的"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .typo(type_scale::LABEL)
                            .text_color(theme::text_faint())
                            .child("空格 勾选 · a 全选 · i 输入地址"),
                    )
                    .child(
                        Button::new("add-close")
                            .label("完成 esc")
                            .outline()
                            .xsmall()
                            .on_click(cx.listener(|app: &mut App, _: &ClickEvent, _window, cx| {
                                app.close_add(cx);
                            })),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::overlay())
            .child(panel)
    }

    /// The delete confirmation. It names the feed, the file that will be edited
    /// and what happens to the history, because this is the one action in the app
    /// that rewrites the user's configuration.
    fn render_delete_confirm(&self, cx: &mut Context<Self>) -> Div {
        let Some(pending) = &self.confirming_delete else {
            return div();
        };
        let many = pending.items.len() > 1;
        let headline = if many {
            SharedString::from(format!("删除这 {} 个订阅？", pending.items.len()))
        } else {
            SharedString::from(format!("删除《{}》？", pending.items[0].1))
        };
        let articles: u32 = pending
            .items
            .iter()
            .map(|(id, _, _)| {
                self.snapshot
                    .sources
                    .get(*id as usize)
                    .map(|row| row.total)
                    .unwrap_or(0)
            })
            .sum();
        // One file per line at most: a batch usually spans several, and the name
        // of the file is the thing that makes this dialog honest.
        let mut files: Vec<SharedString> =
            pending.items.iter().map(|(_, _, file)| file.clone()).collect();
        files.sort();
        files.dedup();
        let file_label = if files.len() > 1 {
            SharedString::from(format!("{} 个文件", files.len()))
        } else {
            files[0].clone()
        };

        let line = |label: &str, value: SharedString, colour: Hsla| {
            div()
                .flex()
                .flex_row()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(64.))
                        .flex_none()
                        .typo(type_scale::LABEL)
                        .text_color(theme::text_faint())
                        .child(SharedString::from(label.to_string())),
                )
                .child(div().flex_1().min_w(px(0.)).typo(type_scale::BODY_SM).text_color(colour).child(value))
        };

        let panel = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .w(px(560.))
            .p(px(20.))
            .rounded(px(10.))
            .bg(theme::panel())
            .border(px(1.))
            .border_color(theme::red())
            .child(
                div()
                    .typo(type_scale::ROW_TITLE.with(type_scale::Emphasis::Primary))
                    .child(headline),
            )
            .child(line("配置文件", file_label.clone(), theme::text_dim()))
            .child(line(
                "影响",
                SharedString::from(format!(
                    "{articles} 篇当前文章会从列表消失；数据库里的历史保留，重新添加该 URL 即可恢复"
                )),
                theme::text_dim(),
            ))
            .child(line(
                "备份",
                SharedString::from(format!("{}.bak", file_label)),
                theme::text_faint(),
            ))
            // Real buttons, not key hints: a confirmation whose only way to say
            // yes is a keystroke is a dialog half the readers cannot answer. The
            // shortcuts still work, and are shown next to the buttons.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .pt(px(4.))
                    .child(
                        Button::new("confirm-delete")
                            .label("确认删除 ⏎")
                            .danger()
                            .xsmall()
                            .on_click(cx.listener(|app: &mut App, _: &ClickEvent, _window, cx| {
                                app.confirm_delete(cx);
                            })),
                    )
                    .child(
                        Button::new("cancel-delete")
                            .label("取消 esc")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|app: &mut App, _: &ClickEvent, _window, cx| {
                                app.cancel_delete(cx);
                            })),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::overlay())
            .child(panel)
    }

    fn render_help(&self) -> Div {
        let rows = [
            ("enter / 点击", "进入阅读模式（正文占满整个窗口）"),
            ("esc / h / ←", "退出阅读模式，返回三栏"),
            ("v", "阅读模式内容：原文（内嵌网页）/ 纯文本"),
            ("", "原文首次打开时抓成单个自包含 HTML 存到本地，之后离线渲染"),
            ("j / k, ↓ / ↑", "移动选择（阅读模式内为上一篇/下一篇）"),
            ("g / G", "first / last"),
            ("o", "在浏览器中打开原文"),
            ("y", "复制原始链接"),
            ("4", "History：最近读过的"),
            ("s", "star / unstar"),
            ("u", "toggle read"),
            ("a", "mark everything in scope read"),
            ("r / R", "refresh all / current scope"),
            ("x", "随机：从未读里抽一篇并打开"),
            ("*", "加精 / 取消加精（订阅级；侧栏顶部出现「精选」分区）"),
            ("m", "整理模式：点侧栏勾选多个订阅，再批量删除或加精"),
            ("侧栏行右端", "☆ 加精 · ✕ 删除（删除会二次确认并写配置文件备份）"),
            ("T", "繁 → 简：把繁体原文转为简体显示（不改已存内容）"),
            ("t", "sidebar dimension: 按博客 / 按标签 / 按分类"),
            ("b", "看这个博客：跳到文章所属博客"),
            ("d", "删除该博客（从配置文件移除，二次确认）"),
            ("1 / 2 / 3", "all · unread · starred"),
            ("/", "搜索文章（SQLite FTS5，全部历史）"),
            ("f", "筛选博客（左侧按名字过滤）"),
            ("c", "reload feeds.toml now"),
            ("?", "toggle this help"),
            ("⌘q", "quit"),
        ];
        let mut panel = div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .w(px(460.))
            .p(px(18.))
            .rounded(px(10.))
            .bg(theme::panel())
            .border(px(1.))
            .border_color(theme::border());
        panel = panel.child(
            div()
                .typo(type_scale::ROW_TITLE.with(type_scale::Emphasis::Primary))
                .child("Keyboard"),
        );
        for (key, desc) in rows {
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(12.))
                    .typo(type_scale::LABEL)
                    .child(
                        div()
                            .w(px(110.))
                            .flex_none()
                            .typo(type_scale::LABEL)
                            .text_color(theme::accent())
                            .child(key),
                    )
                    .child(
                        div()
                            .flex_1()
                            .typo(type_scale::BODY.with(type_scale::Emphasis::Secondary))
                            .child(desc),
                    ),
            );
        }

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::overlay())
            .child(panel)
    }
}

/// Case-insensitive comparison without allocating a lowercased copy: the
/// sidebar is re-sorted whenever the snapshot changes, so per-comparison
/// allocation would be wasteful.
fn ci_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => match x.cmp(&y) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            },
        }
    }
}

/// Lower-case and drop every space, for comparisons against text an input method
/// may have produced.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Does a blog match a name filter?
///
/// Spaces are ignored on both sides, because an IME reports composing text with
/// syllable separators in it: typing `css` through Pinyin arrives as `c s s`, and
/// comparing that literally finds nothing.
fn name_matches(title: &str, host: &str, needle_squashed: &str) -> bool {
    if needle_squashed.is_empty() {
        return true;
    }
    squash(title).contains(needle_squashed) || squash(host).contains(needle_squashed)
}

/// Does a feed's summary add anything to its title?
///
/// Most feeds repeat the title as the summary (or as `description`), and some
/// put a byline there instead of content. Both waste a row of the list, so the
/// preview is dropped unless it carries text the title does not.
fn preview_is_useful(title: &str, preview: &str) -> bool {
    let preview = preview.trim();
    // Too short to be a summary; it is a tagline or a stray word.
    if preview.chars().count() < 24 {
        return false;
    }
    if looks_like_byline(preview) {
        return false;
    }
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !is_filler(*c))
            .flat_map(|c| c.to_lowercase())
            .collect()
    };
    let (t, p) = (norm(title), norm(preview));
    if t.is_empty() || p.is_empty() {
        return false;
    }
    // The summary repeating the title — in full, as its opening, or as a
    // fragment of it — is duplication, which is what makes a list feel dense.
    !(p == t || p.starts_with(&t) || t.contains(&p))
}

/// `由 <名字> 发表于 <日期> 更新于 <日期>`: a byline sitting where the summary
/// should be. Several of the imported feeds publish exactly this.
fn looks_like_byline(preview: &str) -> bool {
    let prefixed = BLURB_PREFIXES.iter().any(|p| preview.starts_with(p));
    let dates = preview.chars().filter(char::is_ascii_digit).count();
    let visible = preview.chars().filter(|c| !c.is_whitespace()).count().max(1);
    prefixed && dates >= 6 && dates * 100 / visible >= 30
}

/// Openers that mark a feed summary as a byline rather than a summary.
const BLURB_PREFIXES: [&str; 4] = ["由", "作者", "Posted by", "by "];

/// Punctuation and connectives ignored when comparing a title with a summary.
fn is_filler(c: char) -> bool {
    matches!(
        c,
        '·' | '-' | '_' | '|' | ':' | '：' | '，' | ',' | '。' | '.' | '、' | '!' | '！' | '?' | '？' | '"' | '\'' | '“' | '”' | '(' | ')' | '（' | '）' | '[' | ']' | '【' | '】' | '的' | '了'
    )
}

/// Remove a feed from the file that defines it, as a text edit.
///
/// A backup is taken once per file before the first edit, and the new text is
/// written to a temporary file and renamed into place, so an interrupted write
/// cannot leave a configuration that no longer parses. The engine's file watcher
/// notices the change on its own and reloads.
/// A small toolbar toggle, built from the component library's `Button`.
fn pill(
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    Button::new(id)
        .label(label)
        .ghost()
        .xsmall()
        .toggled(active)
        .on_click(on_click)
}

/// A tag chip, from the component library.
fn tag_chip(label: SharedString) -> impl IntoElement {
    Tag::secondary().xsmall().child(label)
}

/// Sort/index bucket for a blog title: letters first, then digits, then CJK.
fn bucket_of(title: &str) -> (u8, char) {
    let first = title.chars().find(|c| c.is_alphanumeric()).unwrap_or('?');
    if first.is_ascii_alphabetic() {
        (0, first.to_ascii_uppercase())
    } else if first.is_ascii_digit() {
        (1, '#')
    } else if first.is_alphanumeric() {
        (2, '中')
    } else {
        (3, '?')
    }
}

fn section_label(bucket: (u8, char)) -> String {
    match bucket {
        (0, c) => c.to_string(),
        (1, _) => "#".to_string(),
        (2, _) => "中文 · 其他".to_string(),
        _ => "?".to_string(),
    }
}

/// Sidebar tooltip line for a source: its host in the grouped dimension, its
/// category in the blog dimension, plus the learned interval and health.
fn source_subtitle(s: &artifeeds_core::model::SourceRow, grouped: bool) -> SharedString {
    if let Some(err) = &s.last_error {
        return SharedString::from(format!("{} · {}", s.health.label(), err));
    }
    let context = if grouped { s.host.as_ref() } else { s.group.as_ref() };
    SharedString::from(format!(
        "{} · {}s · {}",
        context,
        s.effective_interval_secs,
        s.health.label()
    ))
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_side_filter(cx);
        self.track_progress(cx);

        // The reader re-flows to the actual pane width; a CJK body wrapped by
        // character count would overflow by 2x.
        // `track_focus` only registers the handle; something has to *give* it
        // focus. Without this the whole keyboard layer is dead until the user
        // clicks an element that focuses itself.
        if !self.focused_once {
            self.focused_once = true;
            let handle = self.focus.clone();
            cx.on_next_frame(window, move |_app, window, cx| {
                window.focus(&handle, cx);
            });
        }

        self.sync_page(window, cx);

        let window_px: f32 = window.bounds().size.width.into();
        let cols = Self::columns_for(window_px);
        if cols != self.reader_cols {
            self.reader_cols = cols;
            self.rewrap();
        }

        let mut root = div()
            .id("root")
            .key_context("Artifeeds")
            .on_action(cx.listener(|app: &mut App, _: &FocusArticleSearch, window, cx| {
                let handle = app.search_input.read(cx).focus_handle(cx);
                handle.focus(window, cx);
                app.engine.send(UiCommand::ClearSearch);
                cx.notify();
            }))
            .on_action(cx.listener(|app: &mut App, _: &FocusSidebarFilter, window, cx| {
                let handle = app.side_input.read(cx).focus_handle(cx);
                handle.focus(window, cx);
                cx.notify();
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .typo(type_scale::LABEL.with(type_scale::Emphasis::Primary))
            .track_focus(&self.focus)
            // Clicking anywhere in the app restores keyboard focus: the window
            // can lose it when another application is activated.
            .on_mouse_down(MouseButton::Left, cx.listener(|app: &mut App, _ev: &MouseDownEvent, window, cx| {
                // ...unless the click landed in a text field. This has to name
                // *every* input: a click on the sidebar filter bubbling up here
                // used to hand the keyboard straight back to the list, so the
                // field could be seen but never typed into.
                let typing = app.search_input.read(cx).focus_handle(cx).is_focused(window)
                    || app.side_input.read(cx).focus_handle(cx).is_focused(window);
                if !typing && !app.focus.is_focused(window) {
                    window.focus(&app.focus, cx);
                }
            }))
            .on_key_down(cx.listener(|app: &mut App, ev: &KeyDownEvent, window, cx| {
                app.on_key(ev, window, cx);
            }));

        if self.login {
            let login = self.render_login(cx);
            root = root.child(login);
        } else if self.reading {
            let reading = self.render_reading(window_px, cx);
            root = root.child(reading);
        } else {
            let toolbar = self.render_toolbar(cx);
            let sidebar = self.render_sidebar(cx);
            let list = self.render_list(cx);
            let status = self.render_status();
            root = root
                .child(toolbar)
                // Home is the list, and nothing else: reading happens in the
                // full-window reading mode, so there is no preview column
                // competing for width.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h(px(0.))
                        .child(sidebar)
                        .child(list),
                )
                .child(status);
        }

        if self.help {
            let help = self.render_help();
            root = root.child(help);
        }
        if self.confirming_delete.is_some() {
            let confirm = self.render_delete_confirm(cx);
            root = root.child(confirm);
        }
        if self.add_open {
            root = root.child(self.render_add(cx));
        }
        root
    }
}

#[cfg(test)]
mod list_tests {
    use super::preview_is_useful;

    #[test]
    fn repeated_titles_are_not_previews() {
        // The common case: `description` is just the title again.
        assert!(!preview_is_useful("打造个人的工作流", "打造个人的工作流"));
        assert!(!preview_is_useful("记 GIF 动画转 CSS 逐帧动画工具", "记 GIF 动画转 CSS 逐帧动画工具"));
        // Punctuation and spacing differences still count as a repeat.
        assert!(!preview_is_useful("关于状态可见原则", "关于状态可见原则。"));
        assert!(!preview_is_useful("Memos: 客户端藏不住接口参数和密钥", "Memos 客户端藏不住接口参数和密钥"));
        // A title embedded in a longer summary is still a repeat.
        assert!(!preview_is_useful("地区限制", "地区限制 中国大陆、香港、澳门这几个地区被大量主流 AI 厂商限制提供服务。"));
    }

    #[test]
    fn bylines_and_fragments_are_not_previews() {
        assert!(!preview_is_useful("在腾讯的第九个两年", "由 Ghostzhang 发表于 2024-06-03 更新于 2024-06-14"));
        assert!(!preview_is_useful("短文", "太短了"));
        assert!(!preview_is_useful("标题", ""));
    }

    #[test]
    fn real_summaries_survive() {
        assert!(preview_is_useful(
            "AI Coding 从小工到专家",
            "过去一年是 AI Coding 在整个业界爆发的一年，Claude Code、Codex、Cursor 等工具覆盖了海量程序员。"
        ));
        assert!(preview_is_useful("奇怪的梦", "梦的内容是一群人要求抢劫我车里的一份数据，号召的抢我数据的人是我一个小学同学。"));
    }
}

#[cfg(test)]
mod filter_tests {
    use super::{name_matches, squash};

    #[test]
    fn ime_composition_spaces_do_not_block_a_match() {
        // macOS reports Pinyin composing text as "c s s".
        let needle = squash("c s s");
        assert_eq!(needle, "css");
        assert!(name_matches("CSS 森林", "cssforest.org", &needle));
        assert!(name_matches("CSS-Tricks", "css-tricks.com", &needle));
        assert!(!name_matches("少数派的诚实", "sspai.com", &needle));
    }

    #[test]
    fn chinese_names_match_with_and_without_spaces() {
        assert!(name_matches("少数派的诚实", "sspai.com", &squash("少数派")));
        assert!(name_matches("阮一峰的网络日志", "ruanyifeng.com", &squash("阮一峰")));
        assert!(!name_matches("少数派的诚实", "sspai.com", &squash("阮一峰")));
    }

    #[test]
    fn an_empty_needle_matches_everything() {
        assert!(name_matches("anything", "host", ""));
    }

    #[test]
    fn hosts_are_searchable_too() {
        // Half of these feeds are titled `/home/someone`; the domain is the
        // part a reader actually recognises.
        assert!(name_matches("/home/rook1e", "rook1e.com", &squash("rook1e")));
    }
}

#[cfg(test)]
mod draw_tests {
    use super::draw_unread;

    #[test]
    fn no_unread_draws_nothing() {
        assert_eq!(draw_unread(&[], None, |_| 0), None);
    }

    #[test]
    fn a_single_unread_is_drawn_even_if_it_was_last() {
        assert_eq!(draw_unread(&[7], Some(7), |_| 0), Some(7));
    }

    #[test]
    fn draws_by_roll() {
        assert_eq!(draw_unread(&[10, 20, 30], None, |_| 0), Some(10));
        assert_eq!(draw_unread(&[10, 20, 30], None, |_| 2), Some(30));
    }

    #[test]
    fn never_draws_the_same_article_twice_in_a_row() {
        // Every roll value, against every article as the previous one.
        for previous in [10, 20, 30] {
            for wanted in 0..4 {
                let drawn = draw_unread(&[10, 20, 30], Some(previous), move |_| wanted);
                assert_ne!(drawn, Some(previous), "roll {wanted} repeated {previous}");
            }
        }
    }

    #[test]
    fn the_excluded_article_does_not_shrink_the_roll_space() {
        // Three candidates, one excluded: two remain, so both are reachable.
        let mut seen = Vec::new();
        for wanted in 0..2 {
            seen.push(draw_unread(&[10, 20, 30], Some(20), move |_| wanted));
        }
        seen.sort();
        assert_eq!(seen, vec![Some(10), Some(30)]);
    }

    #[test]
    fn a_bad_roll_cannot_panic() {
        // A caller returning `n` (off by one) is clamped, not indexed out.
        assert_eq!(draw_unread(&[10, 20], None, |n| n), Some(20));
    }
}


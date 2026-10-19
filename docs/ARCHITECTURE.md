# Architecture

```
artifeeds/
├── crates/artifeeds-core/     the engine — no UI dependency, unit-tested
│   ├── config.rs      TOML/OPML composition, validation, compile, hot reload
│   ├── text.rs        HTML→text, entities, wrapping, hashing (no dependencies)
│   ├── model.rs       Article / SourceRow / Snapshot / Stats
│   ├── http.rs        pooled client, conditional GET, concurrency budget
│   ├── parse.rs       feed-rs → RawEntry with budgets and dedup hashes
│   ├── archive.rs     SingleFile-style page capture (lol_html + resvg-free)
│   ├── dedup.rs       bounded per-source and content-hash rings
│   ├── store.rs       SQLite (WAL) with a single writer thread
│   ├── sched.rs       adaptive interval policy + due-time heap
│   ├── metrics.rs     relaxed-atomic counters + latency histogram
│   ├── engine.rs      orchestration and the snapshot publisher
│   └── bin/artifeeds.rs   headless CLI: check / fetch / bench / scaffold
└── crates/artifeeds-app/      the GPUI shell
    ├── theme.rs       palette
    ├── icon.rs        icon embedded in the binary + installed at runtime
    ├── webview.rs     embedded WKWebView (original-page reading mode)
    ├── app.rs         root view, key handling, three virtualised panes
    └── main.rs        process entry, engine startup, window creation
```

## Data flow

```
              ┌─────────────────────────────┐
  disk ──────▶│ ConfigStore  (ArcSwap)      │◀──── notify watcher (debounced)
              │  CompiledConfig             │
              └──────────────┬──────────────┘
                             │ Arc<CompiledConfig>
                             ▼
   ┌────────────────────────────────────────────────────────────┐
   │ Runner — one task, owns all mutable state                   │
   │                                                             │
   │  Scheduler (BinaryHeap<due>)                                │
   │      │ pop_due                                              │
   │      ▼                                                      │
   │  tokio::spawn fetch task ──▶ HttpEngine ──▶ parse (blocking)│
   │      ▲                                          │           │
   │      │            FetchDone (bounded flume)     │           │
   │      └──────────────────────────────────────────┘           │
   │      │                                                      │
   │  dedup rings → NewArticle batch → Store (bounded channel)   │
   │      │                                                      │
   │  in-memory window (Vec<Arc<Article>>, capped)               │
   │      │                                                      │
   │  Snapshot build (≤ 4 Hz, only the dirty half)               │
   └──────┬──────────────────────────────────────────────────────┘
          │ ArcSwap::store(Arc<Snapshot>)
          ▼
   ┌──────────────────────┐        flume::Sender<UiCommand>
   │ GPUI render (60 fps) │──────────────────────────────────────▶ Runner
   │  one atomic load     │
   └──────────────────────┘
```

## Invariants

These are the rules that keep the thing predictable. Breaking one of them is how
this kind of application usually regresses.

1. **One writer.** All mutable engine state lives in the `Runner` task. Fetch
   tasks are pure functions from `(FetchSpec, ParseBudget)` to `FetchResult`
   and communicate only over a bounded channel.
2. **The UI never blocks.** It reads `ArcSwap<Snapshot>` and sends commands over
   a `flume` channel with `try_send`. There is no lock in the render path and no
   `await` in a GPUI callback.
3. **Everything is bounded.** Every queue, cache, ring, body and window has a
   configured ceiling. There is no `unbounded()` anywhere in the data path.
4. **Backpressure is implicit.** A saturated stage blocks the stage before it
   rather than buffering: the store channel filling stops the ingest loop, which
   stops dispatching new fetches.
5. **Nothing expensive per frame.** Filtering happens on generation change,
   status text on snapshot build, article bodies on selection, HTML→text at
   parse time.
6. **Fault isolation.** A bad config file, a dead host, a malformed feed or a
   panicking parse becomes a problem line, never a crash. Feed parsing runs in
   `spawn_blocking` specifically so a panic there is contained.
7. **Cost is measured.** `Metrics` is fed on the hot path and surfaced in the
   status bar, and `artifeeds fetch` prints the same numbers headlessly. Claims
   about performance should be reproducible with `artifeeds bench`.

## Why these choices

**Why `feed-rs` rather than a hand-written parser?** Format coverage (RSS 0.9x
through 2.0, Atom 0.3/1.0, JSON Feed, MediaRSS, encoding detection) is a large
surface with a lot of real-world variation. The parser *is* the easy part to get
subtly wrong, and it is not where the time goes — the byte transfer is.

**Why SQLite rather than a document store?** The access pattern is
"append many, read newest N, occasionally search, prune by age". That is exactly
what a single-table database with covering indexes does well, and it gives
`UNIQUE(source_id, uid_hash)` dedup, FTS5 search and transactional batches for
free, in one file, with no server.

**Why one writer thread rather than a pool?** SQLite serialises writers anyway;
a pool would only add lock contention. One connection also means one prepared
statement cache and one page cache — the smallest possible footprint.

**Why `uniform_list` rather than a normal scrollable column?** With a 4 000-item
window, a plain list lays out and shapes every row every frame. `uniform_list`
lays out the first row, then positions the rest arithmetically and only
instantiates the visible range.

**Why wrap the reader body into fixed-width lines?** So the reader can be
virtualised too. It moves wrapping from "every frame, for the whole document" to
"once, when the article is opened", at the cost of one `Vec<String>` for the
selected article.

**Why is reading mode a native WebKit view rather than GPUI elements?** The
requirement is the page *as it is* — its CSS, fonts and layout. Reproducing that
in GPUI would mean writing a browser. So a `WKWebView` is attached as a subview
of GPUI's `NSView`: it moves, clips and resizes with the window for free, and
GPUI keeps rendering the chrome around it. Two details make that safe: the
subclass refuses first-responder status, so clicking in the page does not steal
the keyboard from the app (the wheel and links still work — both are dispatched
by hit-testing), and the view is only made visible once WebKit reports the
navigation finished, so GPUI's own text is what you see until the page is ready.

**Why archive instead of always loading live?** A feed reader re-reads the same
article many times. Archiving converts that from N network round trips (plus N
third-party requests, scripts and trackers) into one, and makes the article
available offline and immune to link rot. It is bounded and separated from feed
fetching so it can never degrade a refresh.

**How does the search box accept Chinese input?** It does not — `Input` does.
The field is `gpui_kit::component::input::Input` over an `InputState` entity, so
text editing, selection, clipboard, undo and IME composition (including the
composition range the platform reports) are the component's responsibility. The
app subscribes to `InputEvent` and only keeps the query string: `Change` updates
it, `Focus`/`Blur` track whether the field owns the keyboard, and `PressEnter`
leaves the field. An earlier version hand-rolled this against
`EntityInputHandler`; the component is strictly better and several hundred lines
shorter.

**Why does the keyboard not fall through from the field to the list?** Because
the app checks `search_focus` before running any list shortcut, and the root's
click-to-refocus handler skips when the input's focus handle is focused —
otherwise clicking the list would take the keyboard away mid-word.

**Why is typography a module instead of literals?** Because a size is never
just a size. `text_size(px(13.))` scattered through a view means 13 px sometimes
arrives with 16 px leading and sometimes with 20, at weight 400 here and 500
there, and the result reads as noise even when every individual choice is
defensible. `type_scale.rs` makes the four decisions at once, so "which role is
this?" is the only question a call site answers — and the roles are few enough
to hold in your head. The same reasoning applies to the three text colours: they
are documented with their measured contrast ratios rather than picked by eye,
which is how `#5d6878` was caught sitting at 3.35:1 while carrying 11 px text.

**Why does a Zhihu collection need its own client and its own rules?** Three
measured reasons. The API answers an anonymous request with
`AuthenticationError`, so the app lets the reader sign in inside its own web
view and reads the session out of that view's cookie store. Asked for more than
**20 items in one page**, the `items` endpoint answers `200` with an *empty*
list — indistinguishable from "this collection is empty", which is exactly how
the first version reported every collection as empty — so fetching pages 20 at a
time. And the engine's HTTP client, which keeps hundreds of connections busy
during a sweep, times out against zhihu.com while a fresh client talks to it
happily, so the Zhihu API gets a client of its own.

The body rule follows from a storage decision rather than a network one: an
existing article row is **never updated** (`INSERT ... ON CONFLICT DO NOTHING`),
so a body that is missing when the row is first written is missing forever.
Bodies therefore travel with the row that introduces the item, the per-refresh
budget is the frontier's step size, and an item whose body cannot be fetched is
left for the next pass instead of being stored incomplete. `artifeeds refetch`
exists for the rows that an earlier version stored that way.

**Why is a reload applied rather than re-checked?** Because the watcher fires
*after* it has installed the new configuration, so asking the config store to
reload again compared equal and answered "nothing changed" — new sources were
never scheduled and changed settings never applied. The event is the
notification; the current configuration is what gets applied.

**How does "add a blog" decide what is a blog?** By asking for a feed. A links
page is full of things that are not blogs — licence deeds, the static-site
framework's own site, hosting dashboards — and every one of them has a feed, so
"does it have a feed" is necessary but not sufficient: a small deny-list removes
the frameworks and licences that appear in every footer. What the heuristic
deliberately does *not* do is guess from markup that a section called
「友情链接」 is a blogroll; it collects candidate links and lets verification
decide, which is a rule that can be tested (and is, for both halves).

**Why does adding a feed throttle the sweep?** For the same reason archiving
does: the reader is waiting on the result. Measured, this was the difference
between a blogroll scan finding 0 feeds and finding 6 — the sweep held every
connection the scan needed.

**Why is "only this blog" a zone of the row rather than a button in it?** Because
GPUI delivers a click to the element that *captured the mouse down*, and inside a
list row that is the row: a nested `.on_click` never fires, however large the
child is (a 200 pt test button received nothing). The glyph at the row's right
edge is therefore part of the row's own hit area, and the row interprets the
click's x position — right margin means "this blog", anywhere else means "read
it". The list header carries a real `Button` for the same action, which is the
discoverable one, and `b` does it from the keyboard.

**How is an article's reading position read out of a web view?** Through
JavaScript, and the reason is worth recording: `WKWebView.scrollView` is declared
inside `#if TARGET_OS_IPHONE` in the SDK's own header, so on macOS the selector
does not exist. Sending it raises an `NSException`, and an uncaught Objective-C
exception terminates the process — that is a crash, not an error. The bridge now
asks `respondsToSelector:` before every selector it is not certain about, warns
once instead of aborting, and reads the scroll state with
`evaluateJavaScript:`, whose answer arrives in a block and is picked up on a
later frame. Restoring a position is the same round trip in reverse.

**Why does the reading record live in the database?** Because it is the only
thing in the app that is neither content nor configuration: it is what the reader
did. `read_at` and `progress` are two columns on `articles` (schema v4), which
means History is a `WHERE read_at > 0`, the progress hairline is a `f32` already
in the in-memory article, and reopening is a lookup rather than a second store.

**Why do the sidebar counts not come from the window?** Because the window's
counts answer a different question than the one being asked. The window is a
recency slice plus two items per feed; a blog that last posted in 2023 therefore
holds exactly two articles in it, and the sidebar would report `2/2` while the
database held sixty. The sidebar's job is "how much is there", so the counts come
from a grouped scan (`source_counts`), refreshed on a 30-second timer rather than
per publish because the answer only changes when articles arrive. The `All items`
row shows both numbers when they differ, so a reader can see that the window is a
slice rather than a truncation.

**Why does opening a blog load from the database?** For the same reason, from the
other side: `All` wants recency across the fleet, while a selected feed wants
*that feed*. `LoadSource` reads up to 400 of its rows and merges them into the
window — merging, not replacing, so `All` keeps its shape — and each feed is
loaded once, bounded by `max_in_memory_articles`, so visiting blogs cannot grow
the window without limit.

**Why does the sidebar filter read the field every frame?** Because
`InputEvent::Change` fires when text is *committed*, and an input method composes
for as long as the user is typing Pinyin. A filter that waited for a commit would
sit still through the whole word; the article search is the opposite case, where
one FTS query per composing keystroke would be absurd. So the filter compares the
field's current value (a `str` compare, no allocation in the steady state) and
the search listens for `Change`. Both squash whitespace, because macOS reports
composing text as `c s s`.

**Why is deleting a feed a text edit rather than a config rewrite?** The files it
edits are generated with comments, blank-line grouping and aligned `=` signs, and
`toml::to_string` would discard all of it. `remove_source_from_toml` finds the
block by URL and removes exactly those lines, with tests for the first, middle and
last block, for single-quoted URLs, and for a file where the next thing after the
block is a table rather than another feed. It returns `None` when no block matches,
so a caller cannot silently rewrite a file it did not understand.

**Why is the summary sometimes missing from a row?** Because the feed's summary
is often not a summary. `preview_is_useful` normalises both strings (case,
whitespace and punctuation) and drops the preview when it equals the title,
opens with it, is a fragment of it, is under 24 characters, or matches the
`由 <name> 发表于 <date>` byline shape that several imported feeds publish. It is
a pure function with unit tests, so the rule is inspectable rather than a matter
of taste.

**Why is the embedded page shown before it has loaded?** A `WKWebView` is a
native subview and it paints progressively; holding it hidden until
`didFinishNavigation` throws away everything WebKit already rendered. The panel
therefore becomes visible on the first `Loading` state, and the help overlay
still hides it (a native subview would otherwise float above GPUI's own panels).

**Why does the home screen have no preview pane?** Reading is a mode, not a
column. A third pane costs width from the list on every frame and duplicates
what reading mode shows properly; `j`/`k` in the list only move the highlight.
An article becomes read when it is *opened*, which is the only moment the reader
has actually seen it.

**Why is the in-memory window capped rather than complete?** Bounded memory. The
database holds everything and search queries it; the window is the reader's
recent view. The sidebar's unread counts are counts within that window, which is
what the list shows — the counts and the list can never disagree.

## Extending it

* **A new source field**: add it to `SourceConfig`/`PartialSource`, include it in
  the fingerprint computation in `config::compile` so reloads notice it, and
  thread it into `SourceSpec`.
* **A new engine command**: add a variant to `UiCommand`, handle it in
  `Runner::on_command`, and (if it changes displayed state) set
  `articles_dirty` / `sources_dirty` so the next publish picks it up. If the UI
  needs a response, add an `EngineEvent` variant.
* **A new pane**: keep it a `uniform_list` over a pre-built `Vec` in `App`, and
  rebuild that `Vec` in the same places as `rebuild_list`.

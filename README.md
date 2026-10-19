# Artifeeds

A desktop feed reader — RSS / Atom / JSON Feed / OPML — built with **Rust + GPUI**
and **SQLite**. Designed around one hard requirement: **many external feeds,
fast and cheap**.

* **Fast**: thousands of sources are scheduled from a single timer, fetched
  concurrently under a two-level budget, and rendered through virtualised lists.
* **Cheap**: conditional requests mean an unchanged feed costs one `304` and
  zero bytes of body; adaptive intervals mean a quiet feed is polled less and
  less; every queue, buffer and cache has a hard cap.
* **Configurable at scale**: hundreds of config files, composed with `include`,
  hot-reloaded with a *diff* so unchanged feeds keep their state.

```
crates/
  artifeeds-core/   ingestion engine — no UI dependency, fully testable
  artifeeds-app/    GPUI desktop shell
```

## Quick start

```sh
# Homebrew Rust toolchain, macOS or Linux
cargo run -p artifeeds-app --release
```

First run creates `~/Library/Application Support/Artifeeds/feeds.toml` (on
macOS) with a starter set of feeds and opens the window. Edit that file while
the app runs: it reloads within ~300 ms, and only the feeds you actually
changed are touched.

Headless commands (useful in CI, or to measure things):

```sh
cargo run -p artifeeds-core --release --bin artifeeds -- check      # validate config
cargo run -p artifeeds-core --release --bin artifeeds -- fetch      # run the engine, print a report
cargo run -p artifeeds-core --release --bin artifeeds -- bench --sources 5000 --items 20
cargo run -p artifeeds-core --release --bin artifeeds -- scaffold
cargo run -p artifeeds-core --release --bin artifeeds -- paths
```

Environment: `ARTIFEEDS_CONFIG`, `ARTIFEEDS_DATA_DIR`, `ARTIFEEDS_THREADS`,
`RUST_LOG`.

## Three ways to browse

The home screen is the article list; the sidebar is organised along three
dimensions, one keypress apart (`t`, or the pill in the sidebar header):

* **按博客** (default) — every feed is a top-level entry, sorted
  case-insensitively and sectioned `A`–`Z` / `中文` / `#`. Good for
  "what is new on this blog".
* **按标签** — the tags the feeds themselves carry (`tags = [...]` in the
  config, or the upstream OPML/CSV tags), most-active first, with the feeds
  filed under their primary tag. Selecting a tag filters the list to every
  article from every feed carrying it.
* **按分类** — the coarse group (`group = "技术"`), feeds nested underneath.

Counts sit in a fixed-width right-aligned column as `unread/total`, so the
numbers line up down the sidebar; a feed with nothing new shows its size alone.
Blogs with no articles in the window are listed last, under `暂无文章` — or
hidden entirely with `z` (`空源置底` / `隐藏空源` in the header). Every dimension
is virtualised, so 1 400 feeds scroll as cheaply as 8.

### Typography

One scale, in [`type_scale.rs`](crates/artifeeds-app/src/type_scale.rs), and every
piece of text names a role from it. Size, leading, weight and colour are decided
**together**, because a scale only looks coherent if a given size always arrives
with the same leading and weight — mixing them ad hoc is what makes an interface
feel assembled rather than designed.

| role | size · leading | weight | used for |
| --- | --- | --- | --- |
| `CAPTION` | 11 · 16 | medium | ages, counts, status-bar figures, key hints |
| `LABEL` | 12 · 18 | medium | sidebar rows, buttons, chips, help keys |
| `BODY_SM` | 13 · 20 | regular | list summaries, notices |
| `BODY` | 14 · 22 | regular | help descriptions |
| `PROSE` | 14 · 24 | regular | the reading surface's wrapped lines |
| `ROW_TITLE` | 17 · 26 | semibold | an article's title in the list |
| `TITLE` | 24 · 32 | semibold | the article title in reading mode |

The numbers come from where these things are actually specified: **Material
Design 3** for the shape of the ladder (label / body / title roles, and
line-height falling as size rises), **Apple's HIG** for the body sizes a desktop
app wants (11 / 12 / 13 / 14 / 17), and **Refactoring UI** for the constraints
that keep it disciplined — at most three weights, a handful of sizes, and small
text that is never grey-on-grey. **CJK** sets the floor on leading: Han glyphs
are denser than Latin and need more air, so no role dips below 1.38× and prose
sits at 1.71×.

Letter-spacing would normally be part of this (negative for large sizes,
positive for small labels); this GPUI snapshot exposes none, so the hierarchy
rests on size, weight and colour.

### Colour is measured, not eyeballed

Three levels of emphasis, each with the contrast ratio it earns:

| token | colour | on `bg()` | WCAG |
| --- | --- | --- | --- |
| `text` | `#c9d1d9` | 12.3:1 | AAA |
| `text_dim` | `#8b97a8` | 6.4:1 | AA |
| `text_faint` | `#78859a` | 5.1:1 | AA |

The tertiary tone was `#5d6878` — 3.35:1, under the 4.5:1 floor for body text,
while being used at 11 px. Everything in the table above is a measurement, and
`artifeeds-app` enables OpenType `tnum` on every column of numbers so counts and
ages line up instead of shivering as digits change.

### Why a feed can show two articles, and what the counts mean

The in-memory window is a **recency slice of the whole fleet**, not a copy of the
database: the newest articles inside the retention window, plus a guarantee of two
per feed so a blog that posts twice a year still appears at all. That is the right
shape for "what is new" — and for a dormant blog it means the window holds exactly
those two items, which is where a sidebar reading `2/2` came from.

So the counts do not come from the window:

* **Each feed shows its true totals from the database.** `CSS 森林 49/60` means
  forty-nine unread of sixty stored, refreshed from SQLite every thirty seconds.
* **`All items` shows both numbers when they differ** — `804 / 2857` is "804
  articles in memory, 2 857 in the database". Nothing is hidden: the window is a
  slice and the row says so.
* **Opening a blog loads its history.** Selecting a feed (`b`, or clicking it)
  asks the engine for that feed's articles — up to 400, merged into the window —
  so the list shows the blog rather than the two items the recency window kept.
  The load is bounded by `max_in_memory_articles` and happens once per feed.

### Adding a blog

`+ 添加` in the sidebar header (or `n`) opens one overlay with both routes:

* **From the current blog's links.** The app fetches the blog behind the selected
  article and the pages blogs keep blogrolls on (`/links`, `/friends`, `/about`,
  `/blogroll`), merges the links it finds, and probes each for a feed. Only links
  with a feed are offered, because that is the difference between a blog and a
  news article — markup can guess at "is this a blogroll", a feed behind the link
  is evidence. Everything found arrives ticked; `space` untick, `a` toggles all,
  `⏎` adds.
* **By URL.** `i` focuses the field; a blog address or a feed address both work,
  since discovery tries the URL, the document's `<link rel="alternate">`, and
  then the conventional paths.

New subscriptions are written to `custom.toml` beside the feeds that are already
configured — that directory is already `include`d, so nothing else has to change,
and the generated files stay generated. The write is the same shape as deletion:
parse-checked first, backed up once per file, written to a temporary file and
renamed into place.

The scan is bounded on purpose: up to 40 links, four at a time, cheap discovery,
and a 45-second deadline. It is a look at somebody else's site, not a crawl.

### Searching, and finding what you just added

The search box covers **both** article text and subscription names. A blog's name
is not in the text of its articles, so a search for one you just added has to
match the *feed* and show its posts — otherwise adding a subscription looks like
it did nothing, which is exactly how it felt.

It also answers from the **whole database**, not from the cache. The reader looks
at an in-memory window of articles while a search runs against everything, so the
hits are pulled into the window before the list is drawn: an empty result now
means "nothing matches", not "nothing matches that happens to be loaded".

Adding follows the same rule — when a feed is added, the app **opens it**, so what
you subscribed to is on screen instead of a toast naming a file.

Leaving the add panel is `esc` from anywhere in it — including from the URL field,
where escape used to only hand focus back and leave the panel up, which reads as
a key that does not work. There is also a `完成 esc` button, because a panel whose
only exit is a keyboard shortcut is a panel people get stuck in.

### Reading: random draw, and 繁 → 简

Two small things that decide how the app feels day to day.

**`x`** draws one unread article from the current scope and opens it — from the
list, from inside the reading view, or from the `随机 x` button. It never draws
the same article twice in a row while there is a choice: the pool excludes the
last one rather than re-rolling, because a re-roll only makes a repeat unlikely
and "unlikely" is what a stuck key feels like. In a scope with nothing unread it
says so instead of doing nothing.

**`T`** (or the `繁 T` button, which reads `简 T` while it is on) shows
traditional-script text as simplified. It is a *display* choice: what is stored
stays exactly what the feed published, so turning it off restores the original.
It applies to the list, the sidebar, tags and the article body — and it is
applied where each of those turns into display text, so the reading body is
converted once per open and re-wrapped on toggle rather than converted on every
frame. The page itself, in 原文 mode, is untouched: it is somebody else's HTML
rendering, and the status bar shows `简` whenever what you are reading is a
conversion rather than the source.

### Turning a link into subscriptions

```sh
# Read pages with the local pi agent and propose subscriptions.
artifeeds suggest https://jackyst0.github.io/awesome-rsshub-routes/ --apply
```

Give it any link — a news site, a forum, a directory, a blog — and it works out
what to subscribe to, then writes it into the configuration under one group.
Under the hood there are three parts, and the split is the point:

1. **Evidence** (no model): the page is fetched, and its declared feeds, its
   links with their labels, its title and its text are extracted.
2. **The agent** decides what is worth subscribing to. It runs with **web tools
   only** — no shell, no filesystem — because a directory page often ships an
   empty shell and fills it with JavaScript, and reading the page over the
   network is the difference between "nothing here" and the actual list.
3. **The feed parser decides what is real.** Every proposed URL is fetched and
   must parse as a feed before it is offered; anything that fails is printed with
   the reason rather than dropped quietly. A model that invents a plausible
   address produces a rejected row.

Some directories *are* a subscription list, and one format this project already
reads is OPML. When the page links an `.opml`, that file is imported instead of
paying an agent to re-derive it — the example above yields **102 feeds in 12
groups** from a single file, and because `<data>/feeds` is already `include`d, it
lands with no configuration edit. If the page's OPML has nested folders, those
become the groups.

```sh
artifeeds suggest <url>...            # dry run: print what it would add
  --apply                             # write it into <data>/feeds/*.opml or custom.toml
  --group 名称 --tags a,b             # where non-OPML results are filed
  --max N                             # cap on proposals from the agent
  --hint "只要前端相关的"               # tell the agent what you care about
  --no-ai                             # deterministic only: declared feeds + validated links
  --force-ai                          # analyse even when an OPML is present
  --proxy http://127.0.0.1:7897       # see below
  --model deepseek-v4-pro --pi /path/to/pi --timeout 240
```

Without `pi` on `PATH`, or with `--no-ai`, the same pipeline runs on the
deterministic half: the page's own feeds and the blogs its links resolve to.
`ARTIFEEDS_PI` and `ARTIFEEDS_PI_MODEL` set the binary and model for a GUI
launch, which has a PATH of its own.

### The local proxy

This machine runs Clash Verge, and CLI/GUI traffic does **not** go through the
system PAC by default — direct connections to github and friends time out, which
is what made so many feeds look dead. Measured on the same 102-feed list:

| | resolved | average latency |
| --- | --- | --- |
| direct | 6 of 10 sampled | ~5 000 ms |
| through `http://127.0.0.1:7897` | **10 of 10** | **973 ms** |

```toml
[settings]
proxy = "http://127.0.0.1:7897"    # Clash Verge's mixed port
```

With it set, the app's own success rate went from ~15 % to ~66 % and average
latency from 5 s to 1 s. Remove the line to go back to direct connections.

### Zhihu collections

A Zhihu collection is a list of saved answers rather than a feed, so it is
fetched differently: `+ 添加` → **知乎收藏**, sign in once in the browser that
opens *inside the app*, and every collection becomes a source in its own
`知乎收藏` group.

Signing in there rather than importing cookies from Safari or Edge is the whole
point: the session stays in this app's own web data store, nothing is read out
of another browser's keychain, and the cookie jar at
`~/Library/Application Support/Artifeeds/zhihu.cookies` never has to be pasted
anywhere. If it expires, the next attempt opens the sign-in page again.

Collections get their **full text**, not just titles. That is bounded on
purpose — see `ZHIHU_BODY_BUDGET` in the engine — because an article is inserted
once and never updated, so the text has to arrive with the row. Each refresh
completes a batch from the newest end; a 150-item collection fills in over a few
refreshes instead of firing 150 requests at once. Anything whose body cannot be
fetched is left for the next pass rather than stored incomplete.

By hand, the same thing is one config block — a collection URL is recognised by
its shape, no extra fields:

```toml
[[sources]]
title = "我的收藏"
url   = "https://www.zhihu.com/collection/706669334"
group = "知乎收藏"
```

Two commands help when it misbehaves:

```sh
artifeeds zhihu --list              # which account, which collections
artifeeds zhihu <id> --content      # one collection's items, and one body
artifeeds refetch "zhihu.com/collection"   # forget stored rows, so they come back
```

### One blog, several addresses

A feed reaches you under many addresses — its own, a FeedBurner or feedx proxy in
front of it, `http` and `https`, with and without a trailing slash — and every
imported list spells it differently. The loader deduplicates by URL, which is the
right rule for *identity* and the wrong one for *meaning*: two addresses of one
blog are two subscriptions, two unread counts, two rows, two copies of every
article.

`artifeeds dedupe` finds those and keeps one. The signal is the feed's own
**title**, which is the same whichever address serves it, and the survivor is a
rule rather than a guess: the blog's own address beats a proxy's, `https` beats
`http`, the shorter URL wins, and when nothing separates them the first by sort
order — so running it twice proposes the same thing. A title only counts as an
identity when it says something: `Home`, `博客` and `RSS` are not identities, and
neither is anything under four characters.

    artifeeds dedupe              # show the duplicate groups
    artifeeds dedupe --apply      # keep one of each

### Deleting, and keeping it deleted

Generated import files — `tech.toml`, an OPML from `suggest` — are
**machine-owned**: the importer rewrites them, and `scripts/build.sh` copies them
over the installed ones. A deletion kept only inside one of those comes back with
the next build. So a removal is recorded twice: the subscription is taken out of
the file it was in, and its URL goes into `removed = [...]` in the main
configuration, which the loader honours before anything else. Removing a feed in
the app does both.

    removed = ["http://feeds.feedburner.com/ruanyifeng"]

### Curating subscriptions

A feed reader with 1 600 subscriptions is a library you prune, so a subscription
has two things you can do to it, and both live where the subscription does.

**★ 加精** marks a feed as one of your own. It is a *reader* state, kept in the
database rather than in the configuration — an imported OPML is somebody else's
file, and re-importing it must not un-feature anything. Marked feeds gather in a
**精选** section at the top of the sidebar and show a filled star wherever they
appear. `*` marks whatever you are looking at, and the reading toolbar has the
same button, because the moment you decide a blog is worth keeping is the moment
you are reading it.

**✕ 删除** removes the subscription from the configuration (its history stays in
the database; re-adding the same URL — after taking it out of `removed` — brings
it back). Every row carries both actions in its right-hand margin — `☆` and `✕`
— and, as with the article list, they are zones of the row rather than nested
buttons, because a nested hit target inside a list row never receives the click.
The zones are derived from the row's own layout, gaps included; getting that
wrong is how "the buttons do nothing" happens, since a zone that misses the glyph
falls through to selecting the blog, which looks like nothing at all.

For prunes of more than one feed, **`m`** enters 整理模式: sidebar clicks tick
subscriptions instead of changing the scope, the row shows a `☑`, a bar counts
what is selected, and then `d` deletes the batch or `*` features it. A batch
delete rewrites each configuration file **once**, however many feeds it is
removing from it, and still leaves the file's `.bak` as it was.

### Getting to one blog

Four ways, because the question arrives in different places: `b` from the
keyboard; the `⤴` at the right edge of any article row (that margin is the row's
"only this blog" zone); the feed name under the summary; or the `只看这个博客`
button in the list header, which acts on the selected article. All of them scope
the list to that feed and pull its history in from the database.

**按标签 starts folded.** A hundred tags each holding dozens of feeds is a list
you read as an overview first, so tags open folded and remember being opened —
unlike 按分类, which opens expanded.

### The reading record

Opening an article records **when** it was opened and **how far** it was read.
Both live in SQLite (`read_at`, `progress`), so they survive restarts:

* **History** (sidebar, `4`) lists what has been opened, most recently read
  first.
* **A progress hairline** runs along the bottom of the reading toolbar, and a
  partly-read article carries one at the foot of its row in the list.
* **Reopening puts you back** where you stopped, in both the original page and
  the distilled text.

The position is sampled from the page every 1.5 s (via JavaScript, because
`WKWebView.scrollView` exists on iOS only) and written to the database every 5 s
and on the way out, so a crash costs seconds of progress rather than the record.

### Copying and opening

`o` opens the article's original page in the default browser; `y` copies the URL
to the clipboard. Both act on the selected article, in the list or in reading
mode. `PageUp`/`PageDown`/`Home`/`End` scroll the embedded page — the page is a
native subview that refuses first responder so that `j`, `b`, `d` keep working,
which also means it never sees the keyboard; those four keys are translated into
page scrolls.

### Finding and pruning feeds

1 400 feeds is a list you search, not one you scroll. `f` focuses a filter box
above the sidebar; the list collapses to the matches, ordered by size. The filter
reads the field's **current** text rather than waiting for a commit, because an
IME reports Pinyin composition as `c s s` — spaces and all — and the matcher
squashes whitespace on both sides, so typing `css` through Pinyin finds
「CSS 森林」.

From an article, `b` (or clicking the feed name under it) answers "what else is
on this blog?": the list scopes to that feed and the sidebar scrolls to its row.

`d` deletes a feed. It names the file it will edit, the number of articles that
will leave the list, and the backup it takes, and it asks first. The edit is a
**text** edit of that one `[[sources]]` block — not a re-serialisation, which
would throw away the generated file's comments and alignment — written through a
temporary file and a rename. The feed's history stays in the database (the engine
already hides rows whose source is gone), so re-adding the URL restores the
articles. Re-running `scripts/import_cib.py` also restores it: deletion is an
edit of generated files, and the importer is the generator.

### The list shows titles, not feeds

An article row is a **title** first: 16 px, semibold, the brightest thing in the
row. Below it sits a summary *only when the summary says something the title does
not* — most feeds repeat the title as their `description`, and several of the
imported ones publish `由 … 发表于 … 更新于 …` in that slot instead of content.
Both are dropped, because the repetition is what made a long list feel dense.
The caption (`feed · #tag`) and the age stay small and faint: they orient you,
they are not what you are reading.

### Reading is fast to appear

Reading mode opens the **original page by default**. The embedded view is revealed
as soon as a navigation *starts* rather than when it finishes, because WebKit
paints progressively and waiting for `Loaded` means staring at an empty panel for
as long as the slowest image takes. Measured on a live article: the panel appears
at **65 ms** and the page finishes loading at **7 488 ms** — the whole of that
seven seconds used to be blank. The app logs both numbers (`page visible`,
`page fully loaded`), so this stays a measurement rather than an impression.

If the article has been archived, the local copy is loaded instead and no network
is touched at all.

## Included feed set: 中文独立博客

[`scripts/import_cib.py`](scripts/import_cib.py) imports
[timqian/chinese-independent-blogs](https://github.com/timqian/chinese-independent-blogs)
— **1 390 feeds** — and splits them across `config/blogs/*.toml` by category:

```sh
python3 scripts/import_cib.py --out config
# config/blogs/tech.toml      1118 feeds
# config/blogs/life.toml       198 feeds
# config/blogs/misc.toml        38 feeds
# config/blogs/reading.toml     24 feeds
# config/blogs/career.toml      12 feeds
# config/feeds.toml                 settings + include = ["blogs"]
```

Each entry keeps the blog's own name as `title` (the blog dimension) and the
category as `group` (the category dimension), pins `key` to the feed URL, and
carries the upstream tags. Re-running the importer regenerates the files from
upstream; hand edits are overwritten, so put your own additions in a separate
file and `include` that too.

The starter configuration written to the platform config directory points at
these files, so `cargo run -p artifeeds-app --release` picks them up with no
arguments. 1 390 independent blogs is a deliberately hostile workload — a large
fraction are dead, slow, or reject non-browser user agents — which is exactly
what the bounded-concurrency and backoff machinery is for.

## Reading modes

Clicking an article (or `⏎`) takes over the whole window. Two presentations, one
keypress apart with `v`:

* **原文** — the *original page*, rendered by an embedded `WKWebView` inside the
  GPUI window: the site's own CSS, fonts, images and layout. The first time an
  article is opened, the page is **saved locally as one self-contained HTML
  file** (SingleFile-style: stylesheets, images and fonts inlined as data URIs;
  scripts, iframes, CSP and prefetch hints removed). Every later open renders
  that local file — **no network at all**, and no trackers or scripts either.
* **纯文本** — the distilled text this app extracted at ingest time. Instant,
  offline, and the automatic fallback when a page cannot be loaded.

`Esc`, `h`, `←` or the **← 返回** button leaves reading mode; `j`/`k` move to the
next/previous article and stay in it.

```
┌──────────────────────────────────────────────────────────────┐
│ ← 返回 esc   file:///…/archives/eeb20e1f….html   本地存档 · 654.4 KiB │
│                                    原文 | 纯文本   j/k·1/558  ☆ 已读 │
├──────────────────────────────────────────────────────────────┤
│          the page, exactly as the site styled it              │
└──────────────────────────────────────────────────────────────┘
```

### How the archive is produced

Doing this from a browser extension means fighting CORS. Doing it in the engine
means we are just an HTTP client, so the page's stylesheet, images and fonts are
fetched directly — with the same politeness, connection pool and byte budgets as
the feed fetcher:

```
GET page ─▶ decode (GBK/Big5/…) ─▶ pass 1: scan for resources
                                        │
                  stylesheet / img / srcset / inline style / <style>
                                        ▼
                          fetch, bounded, in parallel (own budget)
                                        │
                     @import and url() found inside CSS, one level deeper
                                        ▼
   pass 2: inline everything, drop script/iframe/CSP/refresh/prefetch,
           absolutise links, force the document to UTF-8
```

Two passes because inlining needs the bytes and a streaming rewriter cannot
block on I/O mid-parse; `lol_html` walks the same string twice and never builds a
DOM, so a 4 MB page stays at a few MB of memory. Try it without the GUI:

```sh
artifeeds archive https://example.com/post.html --out post.html
```

Archives live next to the database in `archives/`, are reused forever, and are
garbage-collected when their article is pruned.

## Keyboard

| key | action |
|---|---|
| `⏎` / 点击文章 | 进入阅读模式（正文占满整个窗口） |
| `esc` / `h` / `←` | 退出阅读模式 |
| `v` | 原文（内嵌网页）/ 纯文本 |
| `t` | switch sidebar dimension: 按博客 / 按标签 / 按分类 |
| `f` | 筛选博客：按名字过滤左侧列表（输入法拼写中也实时生效） |
| `b` | 看这个博客：跳到当前文章所属博客，列表范围切到该源 |
| `d` | 删除该博客（二次确认，从配置文件移除并留备份） |
| `z` | blogs with no articles: list last / hide |
| 点 `▾` | fold a tag / group section (click the label to filter by it) |
| `j` / `k`, `↓` / `↑` | move selection |
| `g` / `G` | first / last |
| `⏎` / `o` | open in browser |
| `s` | star / unstar |
| `u` | toggle read, then move on |
| `a` | mark everything in scope read |
| `r` / `R` | refresh all / refresh current scope |
| `1` `2` `3` | All · Unread · Starred |
| `/` | search (SQLite FTS5 over the full history) |
| `c` | reload `feeds.toml` immediately |
| `?` | help overlay |
| `⌘q` | quit |

## Configuration

Everything lives in one root file plus any number of included files:

```toml
include = ["conf.d", "subscriptions.opml"]   # files or whole directories

[settings]
global_concurrency   = 16      # in-flight requests, whole process
per_host_concurrency = 2       # in-flight requests, per hostname
default_interval_secs = 1800
max_in_memory_articles = 20000 # UI window; SQLite keeps everything
retention_days = 120

[group_defaults.News]
interval_secs = 900

[[sources]]
title = "Rust Blog"
url   = "https://blog.rust-lang.org/feed.xml"
group = "Rust"
interval_secs = 1800
```

See [`feeds.example.toml`](feeds.example.toml) for the annotated version and
[`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) for the full reference.

Highlights that matter when there are thousands of feeds:

* **`include` accepts directories** — every `*.toml`, `*.opml`, `*.xml` inside
  is picked up, so an OPML export from another reader drops straight in.
* **A broken file never takes the app down.** It produces a problem line in the
  status bar and the rest of the configuration still loads.
* **Duplicate detection** is done on the normalised URL (scheme/host case,
  trailing slash, `utm_*`/`fbclid`/`gclid` parameters), because the same feed
  reached by two URLs is the single most common config mistake.
* **`key`** pins a source's identity across URL changes, so history and read
  state survive a blog move.
* **Reload is a diff**: each source carries a 64-bit fingerprint; a reload only
  touches genuinely added / removed / changed sources. ETags, backoff and
  learned intervals survive.

## How it stays fast and small

The short version; [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) has the details
and the measured numbers.

**Network**

| mechanism | effect |
|---|---|
| a two-tier window: newest *N* overall **plus** the newest 2 items of every feed | a blog whose last post is six months old still appears, with a truthful unread count, instead of being filtered out of existence by the time window |
| one pooled `reqwest` client, HTTP/2, keep-alive | a refresh of 2 000 feeds opens a handful of sockets per host, not 2 000 |
| `If-None-Match` / `If-Modified-Since` from persisted validators | an unchanged feed transfers **0 bytes of body** |
| global × per-host semaphores plus a per-host minimum gap | bounded concurrency; one popular domain is not hammered |
| streamed bodies with a hard cap | a broken or hostile endpoint cannot blow up memory |
| bounded retries with per-source jitter, `Retry-After` honoured | no retry storms |

**Scheduling**

| observation | next interval |
|---|---|
| new items | reset to the configured base |
| 200 OK, nothing new | ×1.25, up to `quiet_factor × base` |
| 304 Not Modified | ×1.5, up to `quiet_factor × base` |
| error | ×2 per consecutive error, capped by `max_interval_secs` |
| `Retry-After` | honoured verbatim (bounded) |

Every wake-up is jittered by `±jitter_percent` derived from
`(source, sequence)` and the first poll of every source is spread across its
own interval, so 3 000 feeds never line up on the same second. With nothing due
the scheduler parks on a single timer — idle CPU is zero, not "low".

**Storage**

* one writer thread owns one SQLite connection (WAL, `synchronous=NORMAL`);
  one transaction per fetch, `INSERT … ON CONFLICT DO NOTHING RETURNING id` for
  dedup and id assignment in a single statement.
* the FTS5 index is maintained by triggers and only rewritten when title or
  preview change — marking something read does not touch it.
* read + unstarred rows are pruned on a retention window, plus a hard row cap,
  so the database cannot grow without bound.
* article bodies are read **lazily, on selection**, not held in memory.

**UI**

* the render path takes **no locks**: it reads one immutable `ArcSwap` snapshot.
* snapshots are published at most every 250 ms and only the dirty half is
  rebuilt, so a 5 000-source fleet does not re-materialise 5 000 rows per frame.
* the sidebar, the article list and the reader pane are all `uniform_list`s:
  only visible rows are instantiated. Even a 6 000-line article is virtualised
  (the body is wrapped into fixed-width lines once, on load).
* status text is formatted once per snapshot, not per frame.
* search input is debounced by 200 ms before it reaches SQLite.

**Process**

| thread | count | job |
|---|---|---|
| main | 1 | GPUI window, render, input |
| `artifeeds-io` | 2 (configurable) | tokio: HTTP, parse dispatch |
| blocking pool | ≤4 | feed parsing (CPU-bound) |
| `artifeeds-store` | 1 | SQLite writer |
| `artifeeds-config-watch` | 1 | debounced fs watcher |

## Storage schema

```sql
sources(id, key UNIQUE, title, url, group_name, etag, last_modified,
        last_fetch, last_status, error_count, last_error, new_items, interval_secs, added_at)

articles(id, source_id, uid_hash, content_hash, title, link, author,
         published, fetched, preview, content, read, starred)
  UNIQUE(source_id, uid_hash)          -- per-feed dedup, enforced by SQLite
  INDEX(published DESC, source_id, read, content_hash)

articles_fts USING fts5(title, preview, content='articles')  -- optional
```

## Status

Working: config composition + hot reload, HTTP with conditional requests,
RSS/Atom/JSON Feed/OPML parsing, adaptive scheduling, SQLite storage with FTS5,
GPUI window with virtualised sidebar/list/reader, search, star/read state,
headless CLI and a benchmark harness.

Deliberate scope limits, so nothing is a surprise:

* reading mode renders the **whole original page** in an embedded WebKit view,
  not an extracted "reader" reformatting; if you want the text-only version,
  press `v`;
* archived pages are snapshots: a post edited after you first opened it still
  shows the archived revision (that is the point, but it is worth knowing);
* the in-memory window is the newest `max_in_memory_articles` items *plus* the
  two newest of every configured feed, so it is bounded by
  `max_in_memory_articles + 2 × sources`;
* the search box takes keystrokes directly rather than going through an IME
  input handler, so composing CJK in the search field is not supported yet;
* `unread` counts in the sidebar are counts *within the in-memory window*
  (`max_in_memory_articles`), which is what the list displays. The database
  holds everything and search covers all of it.

## Building and installing

```sh
./scripts/build.sh                 # build → install to /Applications → launch
./scripts/build.sh --no-launch     # just build and install
./scripts/build.sh --skip-feeds    # do not re-copy the generated feed files
```

That is the only command needed after a change. It runs `cargo build --release`,
refreshes the feed files, packs the bundle, installs it into `/Applications`, and
restarts the installed app — replacing a **running** copy is deliberately not
attempted, because a process keeps the code it started with.

### Where everything lives

| What | Where | Notes |
| --- | --- | --- |
| The app | `/Applications/Artifeeds.app` | installed by `scripts/build.sh`; `dist/Artifeeds.app` is only the staging copy |
| Database | `~/Library/Application Support/Artifeeds/artifeeds.db` | SQLite, WAL |
| Configuration | `~/Library/Application Support/Artifeeds/feeds.toml` | edited by hand; watched while the app runs |
| Feed files | `~/Library/Application Support/Artifeeds/feeds/*.toml` | the generated blogs plus anything added in the app |
| Page archives | `~/Library/Application Support/Artifeeds/archives/*.html` | one self-contained file per saved page |

Nothing is read from or written to Documents, Desktop or Downloads — those are
the folders macOS guards with a per-launch permission prompt, and an app that
lives in one inherits that prompt. The working tree only *generates* the feed
files (`python3 scripts/import_cib.py --out config`); `scripts/install_feeds.sh`
copies them into the data directory and points the configuration there, so the
installed app keeps working even if the project moves.

```sh
# Why does one feed have no articles? Ask the engine itself, not the DB:
cargo run --release --bin artifeeds -- probe https://example.com/feed --feed
#   → 0.61s OK 59775 https://example.com/feed
#   → 5.02s FAIL https://dead.example/feed <- connect timed out

# What does a blog's links page actually contain?
cargo run --release --bin artifeeds -- friends https://example.com/links/

# Why is the app using a core? Measure, do not guess: this runs the release
# binary against a copy of the live database and prints cores used.
python3 scripts/bench_cpu.py baseline --seconds 12
python3 scripts/bench_cpu.py ui_only  --disable-sources    # UI and store alone
./scripts/measure_cpu.sh <pid>                             # the running app

# A feed that seems to hang the app can be run through the parser directly:
cargo run --release --bin artifeeds -- parse /tmp/feed.xml
cargo run --release --bin artifeeds -- htmltext /tmp/page.html

python3 scripts/make_icon.py        # assets/AppIcon.iconset + .icns
./scripts/make_app_bundle.sh        # bundle + install to /Applications
```

The icon is drawn analytically — a signed-distance rasteriser in plain Python,
no image libraries — so it is antialiased correctly at every size macOS asks for
(16 px to 1024 px). The artwork sits on **Apple's icon grid**, which is what
decides whether an icon looks the size of its neighbours: measured against Mail,
Terminal and Finder, the plate is **80.5 %** of the canvas (824 of 1024, centred,
corner radius 0.2237 of the plate) and the baked drop shadow brings the *visible*
extent to **84.4 %**, asymmetric — 8.6 % clear above, 7.0 % below. Filling the
canvas makes an icon read as a size larger than everything beside it; drawing the
plate alone, with no shadow, makes it read smaller, because every other icon in
the Dock has that halo. Both mistakes have been made here once. The 512 px frame is compiled into the binary and installed
with `NSApplication.setApplicationIconImage:`; the bundle adds the persistent
Finder/Launchpad icon and a display name.

## Development

```sh
cargo test                       # 74 unit tests across config, parsing, text,
                                 # dedup, scheduling, storage, discovery, wrapping
cargo clippy --all-targets
```

The UI is built on [GPUI Kit](https://gpui-kit.com) (`gpui-kit = "0.7.1"`), which
pins its own GPUI snapshot (`gpui-pre`) and re-exports it. The app therefore
depends on **only** `gpui-kit`: declaring `gpui` as well would put two
incompatible GPUI versions in one dependency graph, whose types cannot be mixed.
`use gpui_kit::*` is the GPUI API; `use gpui_kit::component::*` is the styled
component library.

Widgets come from the library rather than from hand-rolled divs: the search field
is `Input` (text editing, selection, clipboard and IME composition included), the
toolbar and sidebar toggles are `Button`, tag chips are `Tag`, and the whole view
lives inside `Root`, which is what supplies components with their theme and
overlay context. The frames that carry the app's own layout — the virtualised
feed list, article rows and reading surface — stay custom, because they are
virtualised for 1 400 feeds and a generic list would not carry the counts,
health dots and fold state that belong to them.

The build needs only the Xcode **Command Line Tools**; GPUI Kit's snapshot
compiles its shaders at runtime.

## Licence

MIT.

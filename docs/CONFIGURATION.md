# Configuration reference

Artifeeds reads one root TOML file plus everything it `include`s. The default
location is `~/Library/Application Support/Artifeeds/feeds.toml` on macOS and
`$XDG_CONFIG_HOME/artifeeds/feeds.toml` elsewhere; override it with
`ARTIFEEDS_CONFIG` or by passing a path to the binary.

Unknown keys are rejected (`deny_unknown_fields`), so a typo is reported as a
problem instead of being silently ignored.

## Composition

```toml
include = ["conf.d", "topics/rust.toml", "subscriptions.opml"]
```

* A **file** include is loaded as TOML or OPML depending on its extension
  (`.toml`; `.opml`/`.xml`).
* A **directory** include loads every `*.toml`, `*.opml` and `*.xml` directly
  inside it, in sorted order. Directories are not walked recursively — point at
  the subdirectory if you want that.
* Relative paths resolve against the file that contains the `include`.
* Cycles are detected by canonical path and ignored.
* Includes are processed breadth-first; `defaults` and `group_defaults`
  accumulate, with later files winning.

`[settings]` is taken from the root file. If the root has none, the first
included file that declares one supplies it.

### Fault isolation

A file that fails to parse, cannot be read, or is missing produces a
`ConfigProblem` (shown in the UI status bar and printed by `artifeeds check`)
and is skipped. Every other file still loads. A source with a malformed URL is
skipped the same way.

## `[settings]`

| key | default | meaning |
|---|---|---|
| `db_path` | platform data dir | SQLite file; `~` is expanded |
| `user_agent` | `Artifeeds/<version>` | sent when a source has no override |
| `accept_language` | `en,zh;q=0.9,*;q=0.5` | |
| `global_concurrency` | 16 | in-flight requests across the whole process |
| `per_host_concurrency` | 2 | in-flight requests per hostname |
| `host_min_gap_ms` | 250 | minimum spacing between two requests to one host |
| `request_timeout_secs` | 20 | |
| `connect_timeout_secs` | 5 | |
| `min_interval_secs` | 300 | floor; nothing is polled faster |
| `max_interval_secs` | 86400 | ceiling, even after long error streaks |
| `default_interval_secs` | 1800 | used when a source does not set one |
| `jitter_percent` | 10 | ± randomisation on every wake-up |
| `quiet_factor` | 4 | how far a quiet feed may stretch past its base interval |
| `max_entries_per_fetch` | 200 | feed archives are truncated to the newest N |
| `max_body_bytes` | 8388608 | hard cap per response body |
| `max_concurrent_fetches` | 24 | spawned fetch tasks (dispatch backpressure) |
| `preview_chars` | 420 | plain-text preview length |
| `max_content_bytes` | 98304 | stored body length per article |
| `preview_chars` | 420 | |
| `max_in_memory_articles` | 20000 | UI window size (~15 MiB of articles) |
| `retention_days` | 120 | prune read articles older than this (0 = keep) |
| `max_articles` | 200000 | hard row cap, oldest unstarred first (0 = unlimited) |
| `full_text_search` | true | create the FTS5 index |
| `archive_on_open` | true | save the original page locally the first time an article is opened |
| `archive_max_bytes` | 12582912 | cap on one archived document, everything inlined included |
| `archive_max_resource_bytes` | 2097152 | cap on a single inlined sub-resource |
| `archive_max_resources` | 80 | how many sub-resources one page may pull in |
| `proxy` | – | e.g. `http://127.0.0.1:7890` |
| `verbose` | false | extra engine logging |
| `headers` | – | extra headers sent with every request |

```toml
[settings.headers]
"x-api-key" = "…"
```

## `[defaults]` and `[group_defaults.<Name>]`

Both accept the same optional fields as a source (`title`, `url`, `enabled`,
`group`, `tags`, `interval_secs`, `headers`, `user_agent`, `max_entries`), and
are applied in this order, most specific first:

```
source field  →  group_defaults[source.group]  →  [defaults]  →  [settings]
```

```toml
[defaults]
user_agent = "Artifeeds (+https://example.com/bot)"

[group_defaults.News]
interval_secs = 600
tags = ["news"]
```

## `[[sources]]`

| key | required | meaning |
|---|---|---|
| `url` | ✔ | `http`/`https` feed URL |
| `title` | | display name; defaults to the hostname |
| `group` | | sidebar section; defaults to the hostname |
| `tags` | | free-form labels |
| `interval_secs` | | base polling interval (clamped to the configured bounds) |
| `enabled` | | default `true`; `false` keeps it listed but never fetched |
| `headers` | | per-source request headers |
| `user_agent` | | per-source override |
| `max_entries` | | per-source entry budget |
| `key` | | stable identity; defaults to the normalised URL |

### URL normalisation and duplicates

Before use, a URL is lowercased in scheme and host, given a single trailing
slash rule, and stripped of `utm_*`, `fbclid` and `gclid` parameters. Two
sources that normalise to the same URL are a duplicate: the **first** one wins
and the second is reported as a problem. This is the most common configuration
mistake when merging OPML files, and catching it prevents double-fetching.

### `key` and stability

`key` is the source's identity across restarts: it is what maps to the row in
the `sources` table, so read state and history follow it. Set it explicitly when
a feed's URL changes:

```toml
[[sources]]
key   = "blog.example.com/feed"
title = "Example Blog"
url   = "https://blog.example.com/feed.xml"
```

On reload, sources are matched by `key` and their runtime state (ETag,
`Last-Modified`, backoff counters, learned interval) is carried over — an
unrelated edit elsewhere in the file does not reset anything.

## Importing a feed list

```sh
python3 scripts/import_cib.py --out config
```

imports [timqian/chinese-independent-blogs](https://github.com/timqian/chinese-independent-blogs)
(1 390 feeds) into `config/blogs/<category>.toml`, one file per category, plus a
`config/feeds.toml` holding the settings and the `include`. Each source gets:

```toml
[[sources]]
title = "阮一峰的网络日志"        # the blog's own name -> the 按博客 dimension
url   = "http://feeds.feedburner.com/ruanyifeng"
group = "技术"                    # coarse category -> the 按分类 dimension
tags  = ["创业", "编程", "前端"]
key   = "http://feeds.feedburner.com/ruanyifeng"
```

Re-running the importer regenerates those files from upstream, so put local
edits in a separate file and include that as well. The importer normalises feed
URLs and drops duplicates the same way the engine does, so `artifeeds check`
stays clean.

## Page archiving

Reading mode's **原文** view renders the original page from a local,
self-contained copy. The first open fetches the page and every resource it needs
(stylesheets, images, fonts), inlines them as data URIs, strips scripts, iframes,
CSP/refresh metas and prefetch hints, resolves every remaining link against the
page URL, and rewrites the document to UTF-8 — the SingleFile technique,
implemented natively so no CORS rules apply.

The result is one `.html` file under `archives/` next to the database
(`~/.local/share/artifeeds/archives` or
`~/Library/Application Support/Artifeeds/archives`). It is reused on every later
open, works offline, and is deleted when its article is pruned. Set
`archive_on_open = false` to keep reading mode live-only, or run

```sh
artifeeds archive <url> [--out FILE]
```

to produce an archive by hand.

## Tuning the fetch fleet

| key | shipped | why |
| --- | --- | --- |
| `global_concurrency` | 24 | Concurrent requests across all hosts. 1 300 distinct hostnames at 48+ saturate DNS/TLS: measured per-feed latency goes from ~1 s to ~12 s. |
| `max_concurrent_fetches` | 24 | Same ceiling, applied to admitted fetches. |
| `per_host_concurrency` | 2 | No single host is ever hammered, even if 300 feeds share it. |
| `host_min_gap_ms` | 400 | At most 2.5 requests/second to any one host. |
| `request_timeout_secs` | 30 | Generous, because a slow feed is still a feed. |
| `max_body_bytes` | 16 MiB | Archive-style feeds inline every post's `content:encoded` and routinely run to several MiB; a 4 MiB ceiling marks them as failures. |
| `startup_spread_secs` | 45 | Staggers the first pass so the fleet does not all wake at once. |

After changing these, save the file — the app watches it and reloads in place —
and press `r` to re-sweep everything immediately.

## Deleting a feed

Press `d` with a blog in scope (or any of its articles selected). The app shows
what it is about to do — the file, the article count, the backup path — and only
then edits it.

Removal is a text edit of the single `[[sources]]` block that carries the URL, so
the rest of the file keeps its comments, ordering and aligned keys. Before the
first edit of a file a backup is written next to it (`<name>.toml.bak`), and the
new contents are written to a temporary file and renamed into place, so an
interrupted write cannot leave a configuration that no longer parses. The engine's
watcher notices the change on its own; `c` forces a reload.

The feed's articles stay in the database — the engine hides rows whose source is
no longer configured — so re-adding the URL brings the history back with it. Note
that these files are generated: re-running `scripts/import_cib.py` restores every
feed it knows about.

## Tags

```toml
[[sources]]
title = "CSS 森林"
url   = "https://www.cssforest.org/blog/index.xml"
group = "技术"
tags  = ["编程", "生活随笔"]
```

`tags` is free-form. It is used in three places:

* the **按标签** sidebar dimension (`t`), which files each feed under its first
  tag and lets you scope the article list to a tag — the filter resolves through
  the feed an article came from, so a tag with 200 feeds lists all of them;
* chips in the list header when a feed or tag is selected;
* the tag chip on every article row, next to the feed name.

The importer fills `tags` from the upstream CSV, so the 1 390 Chinese blogs
arrive already labelled.

## Sidebar dimensions

`t` cycles **按博客 → 按标签 → 按分类**. Each is virtualised, so fleet size does
not affect scroll cost.

* **按博客** — sorted case-insensitively, sectioned `A`–`Z`, `#`, `中文 · 其他`.
  Feeds with no articles in the window are listed last under `暂无文章 · N`, or
  hidden entirely; `z` (or the `空源置底` / `隐藏空源` pill) switches.
* **按标签** — tags ordered by how many unread articles they hold, then
  alphabetically, with feeds nested under their primary tag.
* **按分类** — the `group` field, feeds nested underneath.

Counts are always `unread/total` (or just the total when nothing is unread).

## OPML

```toml
include = ["subscriptions.opml"]
```

`<outline>` elements with an `xmlUrl` become sources; `title` (or `text`) is the
title; `category` becomes the group. Nesting is flattened. Outlines without an
`xmlUrl` (folders) are counted and reported, not treated as errors.

## Reload semantics

The root file and every contributing directory are watched with a single
debounced watcher (300 ms settle). On a change:

1. everything is re-read and re-compiled;
2. if the whole-set fingerprint is unchanged, nothing happens;
3. otherwise a key-level diff decides which sources were added, removed or
   changed;
4. added sources are scheduled (spread across their first interval), removed
   sources are dropped from the schedule (their rows stay in the database),
   changed sources keep their state but adopt the new interval/headers.

Problems from the new compile replace the old ones in the status bar.

## Zhihu collections

A collection is recognised by its URL shape, so it needs no extra field:

```toml
[[sources]]
title = "我的收藏"
url   = "https://www.zhihu.com/collection/706669334"
group = "知乎收藏"
tags  = ["知乎"]
```

The session comes from signing in under `+ 添加` → 知乎收藏, and lives in
`~/Library/Application Support/Artifeeds/zhihu.cookies`. Without it the source
reports 知乎未登录 and retries later; nothing else about the source is special.
Because an article row is never rewritten, the full text is fetched as part of
the fetch that first stores an item — a collection larger than one batch fills
in over successive refreshes, newest first.

## Proxy

Feed fetching honours a proxy when the machine needs one — on this machine the
CLI and the app bypass the system PAC, so direct external access times out:

```toml
[settings]
proxy = "http://127.0.0.1:7897"   # Clash Verge mixed port; empty/absent = direct
```

Measured on a 102-feed list: 6 of 10 sampled feeds resolved directly against 10
of 10 through the proxy, and the app's sweep moved from ~15 % to ~66 % success
with average latency dropping from 5 s to 1 s.

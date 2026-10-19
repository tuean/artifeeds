# Performance management

This is the design rationale behind "fast **and** cheap" at fleet scale — a few
thousand feeds on a laptop, with the app open all day. Numbers quoted here were
measured with `artifeeds bench` and `artifeeds fetch`; the commands to reproduce
them are at the end.

## The cost model

For a feed reader the resources rank like this:

| # | resource | why it dominates |
|---|---|---|
| 1 | **bytes on the wire** | proportional to how often you poll and how much you re-download |
| 2 | **TLS handshakes / TCP connections** | ~2-4 round trips each; hundreds of them dominate wall-clock time |
| 3 | **CPU during render** | a naive reader re-lays-out thousands of rows 60× a second |
| 4 | **memory** | unbounded queues, caches and article windows |
| 5 | **disk** | re-writing the same rows, unbounded growth |

Almost every decision below attacks (1) or (2), because those are the ones that
also bound wall-clock latency.

## 1. Bytes: conditional requests and adaptive intervals

**Conditional GET.** Every source persists its `ETag` and `Last-Modified`. The
next request sends `If-None-Match` / `If-Modified-Since`; an unchanged feed
answers `304 Not Modified` with no body. For a typical configured fleet this is
the dominant case: after the first refresh, the steady state is ~95 % 304s. A
304 costs a few hundred bytes of headers instead of tens of kilobytes of XML.

**Adaptive interval.** A fixed interval is either wasteful (polling a blog that
posts monthly, every 15 minutes) or slow (missing a news site's burst). Each
source learns:

```
new items        → interval := base
200, nothing new → interval := interval × 1.25, capped at quiet_factor × base
304              → interval := interval × 1.5,  capped at quiet_factor × base
error            → interval := base × 2^consecutive_errors, capped at max_interval
Retry-After      → honoured (bounded by max_interval)
```

with `quiet_factor = 4` by default. A feed that never changes converges to 4×
its configured interval and stays there, and the moment it does change it snaps
back to the base interval. So the steady-state request rate tracks the actual
publication rate rather than the configured guess.

**Entry budget before work.** Feeds with 10 000 archived entries are common. The
parser extracts only the cheap sort key (`published`) for every entry, keeps the
newest `max_entries_per_fetch` with `select_nth_unstable` (O(n), no allocation
of intermediate records), and only then does the expensive HTML→text work on the
survivors. Parsing a 4 MB archive feed costs the same as a 30-item one.

**Body cap.** Responses are streamed and abandoned the moment they exceed
`max_body_bytes` (8 MiB default). A misconfigured URL that points at a video
cannot make the process allocate gigabytes.

## 2. Connections: one client, two-level admission control

* **One `reqwest::Client` for the whole process.** Connection pooling, HTTP/2
  multiplexing, keep-alive and DNS caching all come for free; a refresh of
  2 000 feeds spread over a few hundred hosts opens a handful of sockets per
  host, not one socket per feed.
* **Global semaphore** (`global_concurrency`, default 16) bounds total
  in-flight requests, so a refresh cannot saturate the network stack or the
  tokio runtime.
* **Per-host semaphore** (`per_host_concurrency`, default 2) plus a
  **minimum inter-request gap** (`host_min_gap_ms`, default 250 ms) keeps a
  single heavily-subscribed origin (a company blog with 40 feeds, Blogspot,
  Medium) polite. The gap is implemented as a per-host "next allowed start
  time" token in a mutex-protected map — O(1) and self-cleaning.
* **Retries are bounded (2) and jittered** per `(source, attempt)`, and a
  `Retry-After` longer than 5 s aborts retrying immediately and is handed to the
  scheduler instead. That converts a rate-limit response into a real backoff
  rather than a retry storm.

## 3. CPU: nothing expensive happens per frame

**The UI never takes a lock.** The engine publishes immutable snapshots through
an `ArcSwap`; the render path is `snapshot.load_full()` — one atomic load. There
is no mutex on the hot path in either direction.

**Only dirty halves are rebuilt.** A snapshot has two independently-dirty parts:
per-source rows (rebuilt on fetch completion, config reload, or pause) and the
article window (rebuilt on ingest / read-state change / search). Both are
coalesced to at most one publish per 250 ms. For 5 000 sources that turns
"rebuild 5 000 structs every frame" into "rebuild them at most 4×/s".

**Virtualised lists everywhere.** The sidebar, the article list *and* the
reader pane are `uniform_list`s: only rows inside the viewport are instantiated,
laid out and painted. The reader wraps its body into fixed-width display lines
once when the article is opened, which is what allows it to be virtualised too —
a 6 000-line article costs the same to display as a 6-line one.

**Precomputed display strings.** `Article` carries an interned `Arc<str>` for
everything the list shows (`title`, `preview`, `source_title`, `group`, and a
pre-formatted `age`), so the row builder does no formatting beyond one small
`format!` for the meta line. The status bar string is built once per snapshot,
not per frame.

**Strings are shared, not copied.** Interning `Arc<str>` through the config and
the article model means a fleet-wide `group` name exists once in memory.

## 4. Memory: every queue has a ceiling

| bound | default | what it protects |
|---|---|---|
| `max_in_memory_articles` | 20 000 | recency tier of the UI window (~15 MiB) |
| + 2 newest per feed | × sources | presence tier, so dormant feeds still show up |
| `max_articles` | 200 000 | total rows in SQLite |
| `max_concurrent_fetches` | 24 | spawned fetch tasks |
| store command channel | 256 | SQLite writer queue |
| fetch-result channel | 512 | result handoff |
| dedup rings | 4 096 / source, 65 536 global | duplicate suppression |
| `max_body_bytes` | 8 MiB | a single response |
| `max_content_bytes` | 96 KiB | a single stored article |
| `preview_chars` | 420 | inline preview |

Dedup uses two bounded FIFO ring sets (`HashSet` + `VecDeque`): a per-source
`(source, uid)` set so a feed repeating its last 20 entries forever costs
nothing, and a global content-hash set so the same story syndicated through five
feeds is shown once. Memory is a constant `capacity × 8 bytes` per source no
matter how long the app runs.

Article bodies are read **lazily from SQLite on selection** (`UiCommand::LoadContent`),
so the in-memory window holds previews, not full text.

**Backpressure is implicit.** When the store is saturated its bounded channel
fills, the engine loop blocks awaiting the write reply, and dispatch of new
fetches stops. No unbounded growth is possible, and no explicit governor is
needed.

## 5. Disk: batched writes and a retention policy

* One commit per fetch, not one per article, in WAL mode with
  `synchronous=NORMAL`, `temp_store=MEMORY`, 16 MB page cache and 128 MB mmap.
* `INSERT … ON CONFLICT(source_id, uid_hash) DO NOTHING RETURNING id` deduplicates
  and assigns ids in a single statement — no read-modify-write.
* The FTS5 index uses `content='articles'` with triggers limited to
  `UPDATE OF title, preview`, so marking an article read never rewrites the
  index.
* Read, unstarred articles older than `retention_days` are deleted in 5 000-row
  chunks (so a first-time prune on a huge database does not hold one enormous
  transaction), then a hard row cap trims the oldest unstarred rows.
* `wal_checkpoint(PASSIVE)` runs on write batches and every ~2 minutes; a
  `TRUNCATE` checkpoint runs on shutdown.

## 6. Archiving is a first-class cost, so it gets its own budget

Rendering the original page in an embedded WebKit view means the page's own
sub-resources get fetched. That is the single most expensive thing the
application does, so it is capped and separated from feed fetching:

| bound | default |
|---|---|
| concurrent archive jobs | 2 |
| concurrent sub-resource requests | 6 (its own semaphore) |
| sub-resources per page | 80 |
| one inlined sub-resource | 2 MiB |
| one archived document | 12 MiB |
| host politeness | shared with the fetcher (`per_host_concurrency`, `host_min_gap_ms`) |

An archive is done once per article, then served from disk forever — so the
steady-state network cost of reading is *zero* re-fetches, and the page renders
even with no connection. `tokio::fs` was deliberately not used for the write:
its operations queue on the shared blocking pool that feed parsing also uses, and
on a 1 400-feed refresh that queue can be seconds deep. A direct `std::fs` write
of a few hundred KB costs a millisecond and cannot be starved.

Measured on one real blog post (654 KiB document, 14 sub-resources inlined):
**~4 s** standalone, **~10 s** while 1 400 feeds were being refreshed
concurrently. 670 KB on disk. Reopening it: 0 network requests.

## 7. Idle costs nothing

With nothing due, the engine loop parks on a single `sleep_until` (or a
`pending()` future when there is genuinely nothing scheduled) and wakes only for
the next deadline, a UI command, a fetch result or a config event. There is no
polling loop, no 60 Hz timer, and no background thread spinning. Idle is
0 % CPU, not "low CPU".

## Measured

On an Apple-silicon laptop, release build, `--release` (thin LTO,
`codegen-units = 1`).

### Scale: 5 001 sources across 101 files, 100 020 articles

```
config compile     21.2 ms for 5001 sources (0 problems)
  per source       0.004 ms
config store init  16.2 ms
forced reload      14.8 ms (changed: false)

write phase
  inserted         100020 rows in 2.98 s
  throughput       33546 rows/s
  db size          54.7 MiB (573 bytes/row, FTS5 enabled)

read phase
  window load      65.4 ms for 5000 rows
  fts query        43.9 ms, 500 hits
  unread counts    12.9 ms for 5001 sources

peak resident set  45 MiB
```

So the whole configuration of a 5 000-feed fleet compiles in ~21 ms, a full
reload diff takes ~15 ms, and 100 000 articles land in SQLite at ~33 000 rows/s
with the full-text index maintained.

### End to end against 1 390 real blogs

The imported `chinese-independent-blogs` fleet (1 390 feeds, a large share of
them dead, slow, or hostile to non-browser user agents), two minutes into a
sweep with `global_concurrency = 48`:

```
requests           1245 in 120 s          10.4 requests/s
200 / 304          16 / 58
cache hit rate     78.4 %                 of answered requests
bytes downloaded   3.0 MiB
bytes saved (304)  18.0 MiB               ~6x the live traffic
errors             378 (30 %)             DNS failures, 404s, TLS, timeouts
process RSS        34 MiB
database           11.7 MiB
```

The 30 % error rate is the real state of that list, not a defect: every failing
feed is backed off exponentially (2^errors, capped at `max_interval_secs`) and
surfaced with its reason in the sidebar, so the steady state converges to
polling only what actually works.

### End to end against real feeds (8 sources)

| | cold cache | warm cache |
|---|---|---|
| requests | 9 | 9 |
| 200 / 304 | 5 / 0 | 1 / 4 |
| cache hit rate | 0 % | **80 %** |
| bytes downloaded | 417.6 KiB | **16.3 KiB** |
| bytes saved by 304s | – | **401.2 KiB** |
| new items | 60 | 0 |
| duplicates dropped | 29 | 25 |
| process RSS | 21 MiB | 21 MiB |
| database | 189 KiB | 224 KiB |

A warm start therefore refreshes the fleet for **~4 % of the cold-start
bandwidth**, which is the entire point of the conditional-request + adaptive
interval design. (The two errors in that run are intentional test cases: an
unresolvable host and an HTML page configured as a feed.)

### Startup

With the database already populated, `engine ready` (config compiled, schema
migrated, 5 001 sources registered, 89 articles loaded into the window, first
snapshot published) is logged at **11 ms** — the window appears with content
already in it.

Idle: **0.7 % CPU** over a 25-second sample with the window open and nothing due.

## Reproducing the numbers

```sh
# Config compile + SQLite throughput with 5 000 sources × 20 articles.
cargo run --release -p artifeeds-core --bin artifeeds -- bench --sources 5000 --items 20

# A real end-to-end run: prints requests, 304 ratio, bytes, latency, DB size, RSS.
cargo run --release -p artifeeds-core --bin artifeeds -- fetch --seconds 60

# Validate a large composed configuration.
cargo run --release -p artifeeds-core --bin artifeeds -- check
```

`artifeeds fetch` is the honest end-to-end measurement: it runs the same engine
the GUI runs, and reports

```
cache hit rate      % of refreshes answered with 304
bytes downloaded    actual payload bytes
bytes saved (304)   from each source's persisted last known body size
latency avg / p95   from a 24-bucket histogram, no allocation per sample
database            bytes on disk including -wal
process rss         memory footprint
```

> Note on the RSS reading: macOS's `TASK_BASIC_INFO.resident_size` (flavour 4/5)
> reports nonsense for 64-bit processes — an earlier revision of this metric
> claimed 4 GiB for a process `ps` measured at 45 MiB. It now uses
> `TASK_VM_INFO` (flavour 22) `phys_footprint`, which is the number the system
> itself uses for memory pressure, with `resident_size` as a fallback.

## What is deliberately not optimised

* **In-app readability extraction / full-site scraping** — out of scope; the
  reader shows the stored body and links out.
* **HTTP caching of images** — the UI does not render remote images yet; when it
  does, that needs its own bounded cache and is a separate design.
* **Cross-source fuzzy dedup** — the content hash compares title plus the head
  of the preview. Near-duplicate detection with embeddings would cost far more
  CPU and memory than it saves.

## Idle and steady-state CPU

Measured with `scripts/bench_cpu.py`, which seeds a copy of the live database so
the scheduler starts from the same state the real app is in:

| configuration | CPU |
| --- | --- |
| 1 404 sources, sweep running (before) | **487 % of one core** |
| 1 404 sources, sweep running (after) | **17 %** |
| 1 404 sources, all sources disabled — UI + store only | **0.1 %** |
| 2 sources | 0.0 % |

Two bugs accounted for the difference, and neither was in the UI.

**The text reducer could not advance past a bare `&`.** Feed text reaches
`html_to_text_into` *after* XML entity decoding, so `&amp;&amp;` in a feed arrives
as `&&`. The run scanner stopped on `&` (it is where an entity would start), the
entity branch was skipped because there was no `;` within reach, and the run it
then copied was empty — so the cursor never moved. One item like that in one feed
pinned a core indefinitely, and the fetch it belonged to never completed, which
is why the app looked busy doing nothing. `2023 && 2024` in a single post was
enough. The loop now only stops on an ampersand that actually starts an entity,
and a fallback advances the cursor for any byte the scan refuses, so no future
condition can reintroduce the spin.

**The engine loop span on its own deadline.** The scheduler's next deadline is
the earliest *due* time, which for an overdue fleet is in the past;
`sleep_until(past)` returns immediately, so the loop ran flat out —
recomputing deadlines, dispatching, running maintenance — while the in-flight
budget was full and nothing could be started. It now wakes for the scheduler only
when a dispatch is actually possible, and otherwise parks on its channels, where
a completion wakes it the moment a slot frees.

A hang of this kind is invisible in logs, because the thing that would log it is
the thing that is stuck. `artifeeds parse <file>` reproduces it offline in
milliseconds, and that is the loop the fix was developed against.

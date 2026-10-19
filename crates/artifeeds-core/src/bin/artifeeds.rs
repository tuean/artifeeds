//! Headless entry point.
//!
//! The GUI is the product, but everything interesting is measurable without it,
//! which is the point of this binary:
//!
//! ```text
//! artifeeds check                  validate config, list problems
//! artifeeds fetch [--seconds N]    run the full engine headless, print a report
//! artifeeds bench [--sources N]    synthetic load: config compile + SQLite throughput
//! artifeeds scaffold               write a starter feeds.toml
//! artifeeds paths                  show where config and database live
//! ```

use artifeeds_core::config::{self, ConfigStore, load};
use artifeeds_core::engine::{Engine, UiCommand};
use artifeeds_core::store::{NewArticle, SourceUpsert, Store};
use artifeeds_core::text::human_bytes;
use artifeeds_core::{default_config_path, default_data_dir};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "check" => check(&args),
        "fetch" => fetch(&args),
        "bench" => bench(&args),
        "archive" => archive(&args),
        "probe" => probe(&args),
        "friends" => friends(&args),
        "zhihu" => zhihu(&args),
        "refetch" => refetch(&args),
        "parse" => parse_file(&args),
        "htmltext" => htmltext(&args),
        "suggest" => suggest(&args),
        "dedupe" => dedupe(&args),
        "scaffold" => scaffold(&args),
        "paths" => paths(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "artifeeds — feed ingestion engine (headless)

USAGE:
    artifeeds <command> [options]

COMMANDS:
    archive <url> [--out FILE]      Save a page as one self-contained HTML file
    probe <url>...                  Fetch URLs through the engine and report why they fail
    friends <url>                   Scan a page for linked blogs and their feeds
    zhihu [collection-url|--list]   Zhihu collections, using the app's saved login
    refetch <url-substring>         Drop stored articles so they are fetched again
    parse <file>...                 Parse local feed files (the offline parser loop)
    htmltext <file> [--max N]       Run the HTML→text reducer on a local file
    suggest <url>...                Read pages with the local pi agent and propose
                                    subscriptions (--apply writes them)
    dedupe [--apply]                Find the same blog subscribed under several
                                    addresses and keep one
    check [config]                  Validate configuration and report problems
    fetch [config] [--seconds N]    Run the engine headless for N seconds (default 30)
    bench [--sources N] [--items M] Synthetic load test of config + storage
    scaffold [config]               Write a commented starter config
    paths                           Print config/database locations

ENVIRONMENT:
    ARTIFEEDS_CONFIG   path to feeds.toml
    ARTIFEEDS_DATA_DIR directory for the SQLite database
    ARTIFEEDS_THREADS  tokio worker threads (default 2)
    RUST_LOG           tracing filter (default: warn)"
    );
}

fn config_path(args: &[String]) -> PathBuf {
    args.iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path)
}

fn flag_u64(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .try_init();
}

fn check(args: &[String]) -> anyhow::Result<()> {
    let path = config_path(args);
    let outcome = load(&path);
    let cfg = &outcome.config;
    println!("config      {}", path.display());
    println!("files       {}", cfg.files.len());
    println!("sources     {} ({} enabled)", cfg.sources.len(), cfg.enabled_count());
    println!("groups      {}", cfg.groups_count());
    println!("fingerprint {:#018x}", cfg.fingerprint);
    println!("interval    {}-{} s (default {} s)", cfg.settings.min_interval_secs, cfg.settings.max_interval_secs, cfg.settings.default_interval_secs);
    println!("db          {}", cfg.settings.db_path().display());
    if outcome.problems.is_empty() {
        println!("\nno problems found");
    } else {
        println!("\n{} problem(s):", outcome.problems.len());
        for p in &outcome.problems {
            println!("  - {p}");
        }
    }
    Ok(())
}

fn fetch(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let path = config_path(args);
    let seconds = flag_u64(args, "--seconds", 30).max(1);
    let engine = Engine::start(path)?;
    let events = engine.events();
    let total = engine.snapshot().stats.sources_enabled;

    println!("fetching {total} source(s), up to {seconds}s …");
    let started = Instant::now();
    let mut last_report = Instant::now();

    while started.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_millis(150));
        let snap = engine.snapshot();
        let s = &snap.stats;
        if last_report.elapsed() >= Duration::from_secs(2) {
            last_report = Instant::now();
            println!(
                "  {:>4}s  req={:<5} 304={:<5} items={:<6} in-flight={:<3} due={:<4} win={}",
                started.elapsed().as_secs(),
                s.requests,
                s.responses_304,
                s.items_new,
                s.in_flight,
                s.sources_due,
                s.mem_articles
            );
        }
        // Stop early once every source has answered at least once.
        if s.sources_due == 0 && s.in_flight == 0 && s.requests >= total && total > 0 {
            break;
        }
        while events.try_recv().is_ok() {}
    }

    // Let the store drain, then force a prune/checkpoint.
    engine.send(UiCommand::PruneNow);
    std::thread::sleep(Duration::from_millis(400));

    let snap = engine.snapshot();
    let s = &snap.stats;
    let processed = s.responses_304 + s.responses_200;
    println!("\n── report ─────────────────────────────────────────────");
    println!("elapsed            {:.1}s", started.elapsed().as_secs_f64());
    println!("requests           {} ({} × 200, {} × 304, {} errors)", s.requests, s.responses_200, s.responses_304, s.errors);
    println!("cache hit rate     {:.1}%", if processed == 0 { 0.0 } else { s.responses_304 as f64 / processed as f64 * 100.0 });
    println!("bytes downloaded   {} ({})", s.bytes_in, human_bytes(s.bytes_in));
    println!("bytes saved (304)  {} ({})", s.bytes_saved_304, human_bytes(s.bytes_saved_304));
    println!("items new / seen   {} / {}", s.items_new, s.items_seen);
    println!("duplicates dropped {}", s.dupes_dropped);
    println!("latency avg / p95  {} ms / {} ms", s.avg_latency_ms, s.p95_latency_ms);
    println!("sqlite batch       {} ms (last)", s.last_batch_ms);
    println!("database           {} ({})", human_bytes(s.db_bytes), s.db_bytes);
    println!("window             {} articles held", s.mem_articles);
    println!("process rss        {} KiB", s.rss_kib);
    println!("config problems    {}", s.config_problems);
    if !snap.problems.is_empty() {
        println!("\nproblems:");
        for p in snap.problems.iter().take(12) {
            println!("  - {p}");
        }
    }
    engine.shutdown();
    Ok(())
}

/// Synthetic load: how fast can we compile a huge config and persist a large
/// number of articles? This is the "scales to thousands of feeds" claim, with
/// numbers instead of adjectives.
fn bench(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let sources = flag_u64(args, "--sources", 2_000) as usize;
    let per_source = flag_u64(args, "--items", 20) as usize;

    let dir = std::env::temp_dir().join(format!("artifeeds-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let cfg_path = dir.join("feeds.toml");
    let db_path = dir.join("bench.db");

    // 100 files × N/100 sources, mimicking a real split-by-topic layout.
    let files = 100;
    let per_file = sources.div_ceil(files);
    // `include` first: a TOML key placed after a [table] header would belong to
    // that table, not the document root.
    let mut root = String::from(
        "include = [\"conf.d\"]\n\n[settings]\nfull_text_search = true\nmax_in_memory_articles = 5000\n\n",
    );
    root.push_str("[[sources]]\nurl = \"https://root.example/feed\"\n");
    std::fs::create_dir_all(dir.join("conf.d"))?;
    std::fs::write(&cfg_path, root)?;
    for f in 0..files {
        let mut body = String::with_capacity(per_file * 120);
        for i in 0..per_file {
            let n = f * per_file + i;
            body.push_str(&format!(
                "[[sources]]\ntitle = \"Source {n}\"\nurl = \"https://host{}.example/feed/{n}\"\ngroup = \"Group {}\"\ninterval_secs = {}\n\n",
                n % 97,
                n % 23,
                600 + (n % 12) * 300
            ));
        }
        std::fs::write(dir.join("conf.d").join(format!("part{f:03}.toml")), body)?;
    }

    println!("── bench ──────────────────────────────────────────────");
    println!("sources target     {sources} across {} files", files + 1);

    let t = Instant::now();
    let outcome = load(&cfg_path);
    let compile_ms = t.elapsed().as_secs_f64() * 1000.0;
    let cfg = outcome.config;
    println!("config compile     {:.1} ms for {} sources ({} problems)", compile_ms, cfg.sources.len(), cfg.problems.len());
    println!("  per source       {:.3} ms", compile_ms / cfg.sources.len().max(1) as f64);

    // Watch/reload timing uses the same code path.
    let t = Instant::now();
    let store_handle = ConfigStore::new(&cfg_path);
    let watch_setup_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("config store init  {:.1} ms", watch_setup_ms);
    let t = Instant::now();
    let reloaded = store_handle.reload();
    println!("forced reload      {:.1} ms (changed: {})", t.elapsed().as_secs_f64() * 1000.0, reloaded.is_some());

    let mut settings = cfg.settings.clone();
    settings.full_text_search = true;
    let metrics = artifeeds_core::metrics::Metrics::new();
    let store = Store::open(&db_path, &settings, Arc::clone(&metrics))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let total_items = cfg.sources.len() * per_source;
    println!("articles to write  {total_items}");

    let started = Instant::now();
    let mut inserted_total = 0usize;
    rt.block_on(async {
        let upserts: Vec<SourceUpsert> = cfg
            .sources
            .iter()
            .map(|s| SourceUpsert {
                key: s.key.clone(),
                title: s.title.clone(),
                url: s.url.clone(),
                group: s.group.clone(),
            })
            .collect();
        let db_ids = store.ensure_sources(upserts).await;
        let now = config::now_unix();

        let mut batch: Vec<NewArticle> = Vec::with_capacity(50);
        for (i, spec) in cfg.sources.iter().enumerate() {
            let db_id = db_ids.get(i).copied().unwrap_or(0);
            for j in 0..per_source {
                let uid = (i as u64) << 32 | j as u64;
                let title = format!("Item {j} from {}", spec.title);
                let preview = format!(
                    "This is a synthetic article body for benchmarking the ingestion path. \
                     Source {} item {j}. Rust, feeds, SQLite and GPUI.",
                    spec.title
                );
                batch.push(NewArticle {
                    source: spec.id,
                    db_source: db_id,
                    uid_hash: uid,
                    content_hash: uid.wrapping_mul(0x9E37_79B9),
                    title: Arc::from(title.as_str()),
                    link: Arc::from(format!("{}#{j}", spec.url).as_str()),
                    author: None,
                    published: now - (j as i64) * 60,
                    fetched: now,
                    preview: Arc::from(preview.as_str()),
                    content: None,
                });
                if batch.len() >= 50 {
                    let done = store.insert_batch(std::mem::take(&mut batch)).await;
                    inserted_total += done.len();
                }
            }
        }
        if !batch.is_empty() {
            inserted_total += store.insert_batch(batch).await.len();
        }
    });
    let write_secs = started.elapsed().as_secs_f64();
    let bytes = rt.block_on(store.refresh_db_bytes());

    println!("\nwrite phase");
    println!("  inserted         {inserted_total} rows in {write_secs:.2}s");
    println!("  throughput       {:.0} rows/s", inserted_total as f64 / write_secs.max(0.001));
    println!("  db size          {} ({:.0} bytes/row)", human_bytes(bytes), bytes as f64 / inserted_total.max(1) as f64);

    // Read path: window load + FTS query.
    let t = Instant::now();
    let window = rt.block_on(store.load_window(settings.max_in_memory_articles.max(200), 60, 0));
    println!("\nread phase");
    println!("  window load      {:.1} ms for {} rows", t.elapsed().as_secs_f64() * 1000.0, window.len());
    let t = Instant::now();
    let hits = rt.block_on(store.search("rust feeds".into(), 500));
    println!("  fts query        {:.1} ms, {} hits", t.elapsed().as_secs_f64() * 1000.0, hits.len());
    let t = Instant::now();
    let counts = rt.block_on(store.counts());
    println!("  unread counts    {:.1} ms for {} sources", t.elapsed().as_secs_f64() * 1000.0, counts.len());
    let t = Instant::now();
    let pruned = rt.block_on(store.prune(0, 0));
    println!("  prune no-op      {:.1} ms ({} deleted)", t.elapsed().as_secs_f64() * 1000.0, pruned);

    store.shutdown();
    let ks = metrics.snapshot_stats();
    println!("\nprocess rss        {} KiB", ks.rss_kib);

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// One blog, several addresses: keep one.
///
///   artifeeds dedupe [--apply] [--config PATH]
///
/// A feed reaches a reader under many addresses — its own, a FeedBurner or feedx
/// proxy in front of it — and every imported list spells it differently. Two
/// addresses of one blog are two subscriptions: two unread counts, two sidebar
/// rows, two copies of every article.
fn dedupe(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let apply = flag("--apply");
    let config_path = value("--config")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_config_path);

    let outcome = artifeeds_core::config::load(&config_path);
    let groups = artifeeds_core::dedupe::find_duplicates(
        outcome
            .config
            .sources
            .iter()
            .map(|spec| (Some(spec.title.as_ref()), spec.url.as_ref())),
    );
    if groups.is_empty() {
        println!("{} 个订阅里没有发现重复的博客。", outcome.config.sources.len());
        return Ok(());
    }

    let total_drop: usize = groups.iter().map(|g| g.drop.len()).sum();
    println!(
        "{} 个订阅中，{} 组重复（将去掉 {} 条）：",
        outcome.config.sources.len(),
        groups.len(),
        total_drop
    );
    for group in &groups {
        println!("  《{}》", group.title);
        println!("    保留  {}", group.keep);
        for (url, why) in &group.drop {
            println!("    去掉  {url}\n          {why}");
        }
    }

    if !apply {
        println!("\n（未写入。加 --apply 去掉上面这些，保留每组的第一条）");
        return Ok(());
    }

    // One write per file, and a URL is only removed from the file that holds it.
    let mut by_file: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let origin_of = |url: &str| -> Option<String> {
        outcome
            .config
            .sources
            .iter()
            .find(|spec| spec.url.as_ref() == url)
            .map(|spec| spec.origin.to_string())
    };
    for group in &groups {
        for (url, _) in &group.drop {
            if let Some(file) = origin_of(url) {
                by_file.entry(file).or_default().push(url.clone());
            }
        }
    }
    let mut removed = 0usize;
    for (file, urls) in &by_file {
        match artifeeds_core::dedupe::remove_sources_from_file(file, urls) {
            Ok(n) => {
                removed += n;
                println!("  从 {} 去掉 {n} 条", file);
                // Also recorded where the reader owns it, so regenerating an
                // import file does not bring the duplicate back.
                for url in urls {
                    if let Err(e) = artifeeds_core::config::append_removed_to_config(&config_path, url) {
                        println!("  记录删除失败 {url}：{e}");
                    }
                }
            }
            Err(e) => println!("  {file} 处理失败：{e}"),
        }
    }
    println!("\n合计去掉 {removed} 条重复订阅（历史保留在数据库里）。");
    println!("重启 app 或等文件监听自动加载即可生效。");
    Ok(())
}

/// Read pages with the local agent and turn them into subscriptions.
///
///   artifeeds suggest <url>... [--apply] [--group G] [--tags a,b] [--max N]
///                              [--hint "只要前端"] [--no-ai] [--model M]
///
/// The agent proposes, the feed parser decides: every proposed URL is fetched and
/// must parse as a feed before it is offered, and the ones that fail are printed
/// with the reason rather than dropped quietly.
fn suggest(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    // Flags take values, and some of those values look exactly like the pages
    // we are here to read (`--proxy http://127.0.0.1:7897`). Positional
    // arguments are the ones no flag claims.
    const VALUED: [&str; 9] = [
        "--group", "--tags", "--max", "--hint", "--model", "--pi", "--timeout", "--proxy",
        "--config",
    ];
    let mut urls: Vec<String> = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if VALUED.contains(&arg.as_str()) {
            skip_next = true;
            continue;
        }
        if arg.starts_with("http://") || arg.starts_with("https://") {
            urls.push(arg.clone());
        }
    }
    if urls.is_empty() {
        anyhow::bail!("usage: artifeeds suggest <url>... [--apply] [--group G] [--max N] [--no-ai]");
    }
    let apply = flag("--apply");
    let use_ai = !flag("--no-ai");
    let max: usize = value("--max").and_then(|v| v.parse().ok()).unwrap_or(8);
    let group = value("--group").unwrap_or_else(|| "智能订阅".to_string());
    let hint = value("--hint").unwrap_or_default();
    let tags: Vec<String> = value("--tags")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_else(|| vec!["智能订阅".to_string()]);
    // The same convention as the other subcommands (`artifeeds <cfg> check`),
    // plus an explicit flag because `suggest` takes positional URLs.
    let config_path = value("--config")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::args()
                .nth(1)
                .map(std::path::PathBuf::from)
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        })
        .unwrap_or_else(default_config_path);

    let mut settings = artifeeds_core::config::Settings::default();
    // On a machine whose CLI traffic does not go through the system proxy,
    // reaching the open internet needs it said out loud.
    if let Some(proxy) = value("--proxy").or_else(|| std::env::var("ARTIFEEDS_PROXY").ok()) {
        settings.proxy = Some(proxy);
    }
    let metrics = artifeeds_core::metrics::Metrics::new();
    let http = std::sync::Arc::new(artifeeds_core::http::HttpEngine::new(&settings, metrics)?);
    let max_bytes = settings.max_body_bytes;

    let sample: usize = value("--sample").and_then(|v| v.parse().ok()).unwrap_or(8);
    let force_ai = flag("--force-ai");
    let pi = {
        let mut pi = artifeeds_core::suggest::Pi::default();
        if let Some(bin) = value("--pi") {
            pi.bin = bin;
        }
        if let Some(model) = value("--model") {
            pi.model = Some(model);
        }
        if let Some(secs) = value("--timeout").and_then(|v| v.parse::<u64>().ok()) {
            pi.timeout = std::time::Duration::from_secs(secs);
        }
        if flag("--no-web") {
            pi.web = false;
        }
        pi
    };
    if use_ai && !pi.available() {
        println!("找不到 pi（{}），改用确定性解析：只取页面声明的 feed 与外链验证。", pi.bin);
        println!("   可用 --pi <path> 指定，或设环境变量 ARTIFEEDS_PI。\n");
    }

    // What is already subscribed: an OPML that is already covered would only
    // re-state those feeds, and "last definition wins" would replace the titles
    // and groups they were curated with.
    let known: std::collections::HashSet<String> = {
        let outcome = artifeeds_core::config::load(&config_path);
        outcome.config.sources.iter().map(|s| s.url.to_string()).collect()
    };
    let known_hosts: std::collections::HashSet<String> = known
        .iter()
        .map(|url| artifeeds_core::discover::host_of(url))
        .collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let mut total_accepted = 0usize;
    let mut total_rejected = 0usize;
    let mut to_write: Vec<artifeeds_core::config::NewSource> = Vec::new();

    for url in &urls {
        println!("══ {url}");
        let evidence = match rt.block_on(artifeeds_core::suggest::gather(&http, url, max_bytes)) {
            Ok(evidence) => evidence,
            Err(e) => {
                println!("   抓取失败：{e}\n");
                continue;
            }
        };
        println!(
            "   标题 {}   ·   声明 feed {} 个   ·   候选链接 {} 个   ·   正文 {} 字",
            if evidence.title.is_empty() { "(无)" } else { &evidence.title },
            evidence.advertised.len(),
            evidence.links.len(),
            evidence.text.chars().count()
        );

        // A page that hands us an OPML subscription list has already answered
        // the question, and the answer is the format this project reads: import
        // it instead of paying an agent to re-derive it, and instead of writing
        // a hundred `[[sources]]` blocks that the next update of the list would
        // leave stale.
        let opml = artifeeds_core::suggest::opml_links(&evidence);
        if !opml.is_empty() && !force_ai {
            for link in &opml {
                println!("   发现 OPML 订阅清单：{link}");
                match rt.block_on(import_opml(
                    &http, link, max_bytes, sample, apply, &known, &known_hosts,
                )) {
                    Ok(report) => println!("{report}"),
                    Err(e) => println!("   导入失败：{e}"),
                }
            }
            if !apply {
                println!("   （加 --apply 即导入；也可用 --force-ai 让 pi 另找订阅项）");
            }
            println!();
            continue;
        }

        let proposals = if use_ai && pi.available() {
            let prompt = artifeeds_core::suggest::build_prompt(&evidence, max, &hint);
            println!("   交给 {} 分析（{} 字符的提示）…", pi.bin, prompt.chars().count());
            match pi.ask_blocking(&prompt) {
                Ok(answer) => match artifeeds_core::suggest::parse_analysis(&answer, max) {
                    Ok(analysis) => {
                        if !analysis.page_kind.is_empty() || !analysis.summary.is_empty() {
                            println!(
                                "   pi 判断：{} {}",
                                analysis.page_kind,
                                artifeeds_core::text::truncate_chars(&analysis.summary, 120)
                            );
                        }
                        if analysis.proposals.is_empty() {
                            println!("   pi 认为这个页面没有可直接订阅的 feed。");
                        }
                        analysis.proposals
                    }
                    Err(e) => {
                        println!("   解析 pi 的回复失败：{e}");
                        println!("   原文片段：{}", artifeeds_core::text::truncate_chars(&answer, 200));
                        artifeeds_core::suggest::deterministic_proposals(&evidence, max)
                    }
                },
                Err(e) => {
                    println!("   pi 调用失败：{e}");
                    artifeeds_core::suggest::deterministic_proposals(&evidence, max)
                }
            }
        } else {
            artifeeds_core::suggest::deterministic_proposals(&evidence, max)
        };
        println!("   候选 {} 条，逐条验证…", proposals.len());

        let (accepted, rejected) = rt.block_on(artifeeds_core::suggest::validate(
            &http,
            proposals,
            max_bytes,
        ));
        for item in &accepted {
            println!(
                "   ✓ {:<28} {}  (置信 {:.2})",
                artifeeds_core::text::truncate_chars(&item.proposal.title, 28),
                item.feed.url,
                item.proposal.confidence
            );
            if !item.proposal.why.is_empty() {
                println!("       {}", artifeeds_core::text::truncate_chars(&item.proposal.why, 90));
            }
            to_write.push(artifeeds_core::config::NewSource {
                title: if item.proposal.title.trim().is_empty() {
                    item.feed.title.to_string()
                } else {
                    item.proposal.title.clone()
                },
                url: item.feed.url.to_string(),
                group: group.clone(),
                tags: tags.clone(),
            });
        }
        for item in &rejected {
            println!(
                "   ✗ {:<28} {}  ← {}",
                artifeeds_core::text::truncate_chars(&item.proposal.title, 28),
                item.proposal.url,
                item.reason
            );
        }
        total_accepted += accepted.len();
        total_rejected += rejected.len();
        println!();
    }

    println!("合计：{total_accepted} 条通过验证，{total_rejected} 条未通过。");
    if to_write.is_empty() {
        return Ok(());
    }
    if !apply {
        println!("（未写入配置。加 --apply 写入，分组「{group}」）");
        return Ok(());
    }
    let outcome = artifeeds_core::config::load(&config_path);
    let target = artifeeds_core::engine::added_sources_file(&outcome.config, &config_path);
    let mut written = 0usize;
    let mut skipped = 0usize;
    // A URL already configured is not an error, it is just already there.
    let known: Vec<String> = outcome
        .config
        .sources
        .iter()
        .map(|s| s.url.to_string())
        .collect();
    for source in &to_write {
        if known.iter().any(|u| u == &source.url) {
            println!("   已订阅，跳过：{}", source.url);
            skipped += 1;
            continue;
        }
        match artifeeds_core::engine::append_source_to_file(
            target.to_string_lossy().as_ref(),
            source,
        ) {
            Ok(true) => written += 1,
            Ok(false) => {
                println!("   已订阅，跳过：{}", source.url);
                skipped += 1;
            }
            Err(e) => println!("   写入失败 {}：{e}", source.url),
        }
    }
    println!(
        "写入 {} 条到 {}（跳过已存在 {} 条）",
        written,
        target.display(),
        skipped
    );
    println!("重启 app 或等文件监听自动加载即可看到新订阅。");
    Ok(())
}

/// Fetch an OPML subscription list, check it, and drop it where the
/// configuration already looks for feed files.
///
/// Sampling rather than probing all of them: a hundred feeds are about to be
/// fetched by the app itself, so what the reader needs here is "this list is
/// alive", not a second full sweep.
async fn import_opml(
    http: &std::sync::Arc<artifeeds_core::http::HttpEngine>,
    url: &str,
    max_bytes: u64,
    sample: usize,
    apply: bool,
    known: &std::collections::HashSet<String>,
    known_hosts: &std::collections::HashSet<String>,
) -> Result<String, String> {
    let response = http
        .fetch_resource_with(url, max_bytes, "text/x-opml,application/xml,text/xml,*/*", 2, &[])
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response.body).to_string();
    let placeholder = std::path::Path::new(url);
    let (sources, skipped) = artifeeds_core::config::parse_opml(&text, placeholder);
    if sources.is_empty() {
        return Err(format!("不是 OPML 订阅清单（{skipped} 条 outline 没有 xmlUrl）"));
    }
    let mut groups: Vec<&str> = sources.iter().filter_map(|s| s.group.as_deref()).collect();
    groups.sort_unstable();
    groups.dedup();
    let already = sources.iter().filter(|s| known.contains(&s.url)).count();
    // The same blogs are often reachable at several feed addresses — `/feed`,
    // `/index.xml`, `/atom.xml` — so counting URLs alone calls a list of
    // already-subscribed blogs "mostly new" and imports a second subscription to
    // every one of them. The host is what identifies the site.
    let known_sites = sources
        .iter()
        .filter(|s| known_hosts.contains(&artifeeds_core::discover::host_of(&s.url)))
        .count();
    if known_sites * 10 >= sources.len() * 9 {
        return Ok(format!(
            "   已有 {known_sites}/{} 个站点在订阅中（URL 完全相同的 {already} 个），\
跳过这份清单 —— 不会用它的标题覆盖现有整理，也不会给同一博客加第二个地址",
            sources.len()
        ));
    }

    // Sample a few, so a list that is entirely dead says so now.
    let step = (sources.len() / sample.max(1)).max(1);
    let mut checked = 0usize;
    let mut alive = 0usize;
    for source in sources.iter().step_by(step).take(sample.max(1)) {
        checked += 1;
        if artifeeds_core::discover::discover(http, &source.url, max_bytes).await.is_ok() {
            alive += 1;
        }
    }

    let mut report = format!(
        "   {} 个订阅源，{} 个分组（其中 {} 个站点是新的）；抽样 {checked} 个，{alive} 个可订阅",
        sources.len(),
        groups.len(),
        sources.len() - known_sites
    );
    if !groups.is_empty() {
        report.push_str(&format!(
            "\n   分组：{}",
            groups.iter().take(8).cloned().collect::<Vec<_>>().join(" · ")
        ));
    }
    if !apply {
        return Ok(report);
    }

    // The app's configuration already includes `<data>/feeds`, so a file written
    // there is picked up with no edit and no restart of anything but the reload.
    let dir = artifeeds_core::default_data_dir().join("feeds");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    let stem: String = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect();
    let stem = stem
        .trim_matches('-')
        .replace("--", "-")
        .trim_end_matches(".opml")
        .to_string();
    let target = dir.join(format!("{}.opml", artifeeds_core::text::truncate_chars(&stem, 60)));
    // Keep the bytes as fetched: it is somebody else's export, and the loader
    // reads it directly.
    std::fs::write(&target, text.as_bytes()).map_err(|e| format!("写入失败：{e}"))?;
    report.push_str(&format!(
        "\n   已导入 {} → {}（app 会自动加载）",
        sources.len(),
        target.display()
    ));
    Ok(report)
}

/// The HTML-to-text reducer alone, on a local file: the layer where a byte that
/// nothing consumes turns into a spinning loop.
fn htmltext(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let file = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("usage: artifeeds htmltext <file> [--max N]"))?;
    let max = args
        .iter()
        .position(|a| a == "--max")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(400);
    let html = std::fs::read_to_string(&file)?;
    println!("{} bytes, max_chars={max}", html.len());
    let started = std::time::Instant::now();
    let text = artifeeds_core::text::html_to_text(&html, max);
    println!("→ {} chars in {:?}", text.chars().count(), started.elapsed());
    println!("  {}", text.chars().take(120).collect::<String>());
    Ok(())
}

/// Parse local files through the real parser. A feed that hangs the parser is
/// otherwise only visible as a mysterious core burning inside the app.
fn parse_file(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let files: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        anyhow::bail!("usage: artifeeds parse <file>...");
    }
    let budget = artifeeds_core::parse::ParseBudget {
        max_entries: 200,
        preview_chars: 400,
        max_content_bytes: 65536,
    };
    for file in files {
        let bytes = std::fs::read(file)?;
        println!("parsing {file} ({} bytes)…", bytes.len());
        let started = std::time::Instant::now();
        match artifeeds_core::parse::parse(&bytes, &budget) {
            Ok(feed) => println!(
                "  ok: {} entries, truncated={} in {:?}",
                feed.entries.len(),
                feed.truncated,
                started.elapsed()
            ),
            Err(e) => println!("  failed in {:?}: {e}", started.elapsed()),
        }
    }
    Ok(())
}

/// Forget stored articles whose source URL matches, so the next fetch inserts
/// them again — the only way to pick up something an earlier version of the
/// fetcher left out, because an existing article row is never updated.
fn refetch(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let needle = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("usage: artifeeds refetch <url-substring>"))?;
    let db = artifeeds_core::default_data_dir().join("artifeeds.db");
    let conn = rusqlite::Connection::open(&db)?;
    let deleted = conn.execute(
        "DELETE FROM articles WHERE source_id IN (SELECT id FROM sources WHERE url LIKE ?1)",
        rusqlite::params![format!("%{needle}%")],
    )?;
    // The in-memory window and the source's dedup memory are rebuilt at startup,
    // so the next launch re-fetches rather than trusting the old state.
    conn.execute(
        "UPDATE sources SET last_status = 0, last_fetch = 0, added_at = 0 WHERE url LIKE ?1",
        rusqlite::params![format!("%{needle}%")],
    )?;
    println!("deleted {deleted} article(s) for sources matching {needle:?}");
    println!("restart the app to fetch them again");
    Ok(())
}

/// Zhihu collections through the same client the app uses, with the cookie jar
/// the in-app login wrote. `--list` prints the account's collections; a URL or id
/// prints that collection's items, and `--content` pulls one body to prove the
/// full-text path works.
fn zhihu(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let list = args.iter().any(|a| a == "--list");
    let with_content = args.iter().any(|a| a == "--content");
    let target = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned();

    let jar = artifeeds_core::default_data_dir().join("zhihu.cookies");
    let cookies = std::fs::read_to_string(&jar)
        .map_err(|_| anyhow::anyhow!("还没有登录：{} 不存在（先在 app 里登录知乎）", jar.display()))?
        .trim()
        .to_string();
    if cookies.is_empty() {
        anyhow::bail!("{} 是空的", jar.display());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    rt.block_on(async {
        if list || target.is_none() {
            // `/api/v4/me` answers with the signed-in account, which is where the
            // url_token for the collections listing comes from.
            let me = artifeeds_core::zhihu::whoami(&cookies)
                .await
                .map_err(anyhow::Error::msg)?;
            println!("account       {} ({})", me.name, me.url_token);
            let collections =
                artifeeds_core::zhihu::list_collections(&cookies, &me.url_token)
                    .await
                    .map_err(anyhow::Error::msg)?;
            println!("collections   {}", collections.len());
            for c in &collections {
                println!("  {:<28} id={} items={}", c.title, c.id, c.count);
            }
            return Ok::<(), anyhow::Error>(());
        }

        let raw = target.unwrap();
        let id = artifeeds_core::zhihu::collection_id(&raw)
            .ok_or_else(|| anyhow::anyhow!("不是收藏夹地址：{raw}"))?;
        let (items, more) =
            artifeeds_core::zhihu::collection_items(&cookies, &id, 0, 20)
                .await
                .map_err(anyhow::Error::msg)?;
        println!("collection    {id}  first page {} items (more: {more})", items.len());
        for item in items.iter().take(10) {
            println!("  [{:?}] {:<40} {}", item.kind, item.title, item.url);
        }
        if with_content
            && let Some(first) = items.first()
        {
            let body =
                artifeeds_core::zhihu::content_of(&cookies, first.kind, &first.id)
                    .await
                    .map_err(anyhow::Error::msg)?;
            println!("\nfirst item body: {} bytes of HTML", body.len());
            println!("  {}", artifeeds_core::text::html_to_text(&body, 160));
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Scan a page for blogroll links and report which of them are feeds. The same
/// pipeline the app's "add a blog" overlay uses, so a disappointing result in
/// the UI can be reproduced and explained here.
fn friends(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let url = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("usage: artifeeds friends <url>"))?;
    let settings = artifeeds_core::config::Settings::default();
    let metrics = artifeeds_core::metrics::Metrics::new();
    let http = std::sync::Arc::new(artifeeds_core::http::HttpEngine::new(&settings, metrics)?);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let max_bytes = settings.max_body_bytes;
        let resp = http
            .fetch_resource(&url, max_bytes, "text/html,application/xhtml+xml,*/*;q=0.8", 2)
            .await?;
        let html = String::from_utf8_lossy(&resp.body).to_string();
        println!("page          {} ({} bytes)", resp.final_url, resp.bytes);
        println!("mentions links {}", artifeeds_core::discover::mentions_links(&html));
        let candidates =
            artifeeds_core::discover::friend_candidates(&html, &resp.final_url, 40);
        println!("candidates    {}", candidates.len());
        let mut hits = 0;
        for c in &candidates {
            match artifeeds_core::discover::discover_common(&http, &c.url, max_bytes).await {
                Ok(feed) => {
                    hits += 1;
                    println!("  ✓ {:<24} {} → {}", c.label, c.url, feed.url);
                }
                Err(_) => println!("  · {:<24} {}", c.label, c.url),
            }
        }
        println!("\n{hits} of {} candidates are feeds", candidates.len());
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Fetch URLs through the real HTTP engine and report the outcome, so a
/// "timed out" row in the database can be explained rather than guessed at.
fn probe(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    // `--concurrency 48` must not contribute "48" as a URL.
    let mut urls: Vec<String> = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with("--") {
            skip_next = true;
            continue;
        }
        urls.push(arg.clone());
    }
    if urls.is_empty() {
        anyhow::bail!("usage: artifeeds probe <url>... [--concurrency N] [--timeout S]");
    }
    let concurrency = flag_u64(args, "--concurrency", 1).max(1) as usize;
    let timeout = flag_u64(args, "--timeout", 20);
    // `--feed` goes through `HttpEngine::fetch` — the path the app actually
    // uses, with per-host admission and retries — instead of `fetch_resource`.
    let as_feed = args.iter().any(|a| a == "--feed");
    let gap = flag_u64(args, "--gap", 0);
    let settings = artifeeds_core::config::Settings {
        request_timeout_secs: timeout,
        connect_timeout_secs: timeout.min(5),
        global_concurrency: concurrency,
        per_host_concurrency: concurrency,
        host_min_gap_ms: gap,
        max_body_bytes: 16 * 1024 * 1024,
        ..Default::default()
    };

    let metrics = artifeeds_core::metrics::Metrics::new();
    let http = std::sync::Arc::new(artifeeds_core::http::HttpEngine::new(&settings, metrics)?);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    let started = Instant::now();
    let results = rt.block_on(async {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        let mut tasks = Vec::new();
        for (i, url) in urls.into_iter().enumerate() {
            let http = std::sync::Arc::clone(&http);
            let permit = std::sync::Arc::clone(&semaphore);
            tasks.push(tokio::spawn(async move {
                let _p = permit.acquire_owned().await.ok();
                let host: Arc<str> = reqwest::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(Arc::<str>::from))
                    .unwrap_or_else(|| Arc::from(""));
                let t = Instant::now();
                let res = if as_feed {
                    let spec = artifeeds_core::http::FetchSpec {
                        source: i as u32,
                        url: Arc::from(url.as_str()),
                        host,
                        etag: None,
                        last_modified: None,
                        headers: Arc::from(Vec::new()),
                        user_agent: None,
                        max_body_bytes: 16 * 1024 * 1024,
                    };
                    http.fetch(&spec, concurrency)
                        .await
                        .map(|r| artifeeds_core::http::ResourceResponse {
                            body: r.body,
                            final_url: url.clone(),
                            content_type: None,
                            bytes: r.bytes,
                        })
                } else {
                    http.fetch_resource(&url, 4 * 1024 * 1024, "*/*", concurrency).await
                };
                (url, t.elapsed(), res)
            }));
        }
        futures::future::join_all(tasks).await
    });

    let mut ok = 0;
    for r in results {
        let Ok((url, dt, res)) = r else { continue };
        match res {
            Ok(resp) => {
                ok += 1;
                println!("{:>7.2}s  {:>4}  {:>8}  {}", dt.as_secs_f64(), resp.bytes, "OK", url);
            }
            Err(e) => println!("{:>7.2}s  {:>4}  {:>8}  {}  <- {}", dt.as_secs_f64(), "-", "FAIL", url, e),
        }
    }
    println!("\n{ok} of the URLs fetched; total {:.1}s", started.elapsed().as_secs_f64());
    Ok(())
}

/// Archive a single page, exactly the way reading mode does — useful for
/// verifying the SingleFile-style pipeline without the GUI.
fn archive(args: &[String]) -> anyhow::Result<()> {
    init_tracing();
    let url = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("usage: artifeeds archive <url> [--out FILE]"))?;
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| {
            let name = artifeeds_core::archive::archive_path(std::path::Path::new("."), &url);
            name.display().to_string()
        });

    let settings = artifeeds_core::config::Settings::default();
    let metrics = artifeeds_core::metrics::Metrics::new();
    let http = artifeeds_core::http::HttpEngine::new(&settings, metrics)?;
    let budget = artifeeds_core::archive::ArchiveBudget::default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let started = Instant::now();
    let result = rt.block_on(artifeeds_core::archive::archive_page(&http, &url, &budget))?;
    let elapsed = started.elapsed();

    println!("source        {}", result.source_url);
    if let Some(title) = &result.title {
        println!("title         {title}");
    }
    println!("resources     {} found, {} inlined, {} skipped", result.stats.resources_found, result.stats.inlined, result.stats.skipped);
    println!("inlined bytes {}", human_bytes(result.stats.bytes as u64));
    println!("document      {} ({})", result.html.len(), human_bytes(result.html.len() as u64));
    println!("elapsed       {:.0} ms", elapsed.as_secs_f64() * 1000.0);
    std::fs::write(&out, result.html.as_bytes())?;
    println!("wrote         {out}");
    Ok(())
}

fn scaffold(args: &[String]) -> anyhow::Result<()> {
    let path = config_path(args);
    if path.exists() {
        println!("already exists, refusing to overwrite: {}", path.display());
        return Ok(());
    }
    config::write_scaffold(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn paths() -> anyhow::Result<()> {
    println!("config   {}", default_config_path().display());
    println!("data dir {}", default_data_dir().display());
    #[cfg(target_os = "macos")]
    println!("database {} (default)", default_data_dir().join("artifeeds.db").display());
    Ok(())
}

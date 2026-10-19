//! Single-file page archiving — the "SingleFile" technique, implemented
//! natively.
//!
//! Browser extensions such as SingleFile capture a page by fetching every
//! sub-resource from inside the page and rewriting the DOM to point at data
//! URIs. Doing it in a browser context means fighting CORS. Doing it *here*
//! means we are a normal HTTP client: we can fetch the stylesheet, the images
//! and the fonts directly, with the same politeness, connection pool and
//! byte budgets as the feed fetcher, and no origin restrictions.
//!
//! The result is one self-contained `.html` file — no scripts, no external
//! requests, no trackers — that renders offline with the site's own layout.
//!
//! ## Pipeline
//!
//! ```text
//!   GET page ──▶ decode (charset) ──▶ PASS 1: scan for resources
//!                                          │
//!                        stylesheet / img / inline style / <style>
//!                                          ▼
//!                                    fetch, bounded, in parallel
//!                                          │
//!                          @import + url() discovered in CSS fetched too
//!                                          ▼
//!              PASS 2: rewrite — inline everything, drop scripts/iframes/
//!                      CSP/prefetch, absolutise links, force UTF-8
//! ```
//!
//! Two passes rather than one because inlining needs the fetched bytes, and a
//! streaming rewriter cannot block on I/O mid-parse. `lol_html` is a streaming
//! rewriter, so the same input is walked twice — cheap, and it never builds a
//! DOM (a 4 MB page stays at a few MB of memory).

use crate::http::HttpEngine;
use base64::Engine as _;
use lol_html::html_content::ContentType;
use lol_html::{RewriteStrSettings, element, rewrite_str, text};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Hard limits. Archiving is the most expensive thing this application does, so
/// every dimension of it is capped and the caps are configurable.
#[derive(Debug, Clone)]
pub struct ArchiveBudget {
    /// Cap on the HTML document itself.
    pub max_page_bytes: u64,
    /// Cap on any single sub-resource.
    pub max_resource_bytes: u64,
    /// Cap on the sum of inlined resources.
    pub max_total_bytes: u64,
    /// Cap on how many sub-resources are fetched.
    pub max_resources: usize,
    /// Per-host request limit, shared with the fetcher's policy.
    pub per_host: usize,
}

impl Default for ArchiveBudget {
    fn default() -> Self {
        Self {
            max_page_bytes: 8 * 1024 * 1024,
            max_resource_bytes: 2 * 1024 * 1024,
            max_total_bytes: 12 * 1024 * 1024,
            max_resources: 80,
            per_host: 2,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveStats {
    pub resources_found: usize,
    pub inlined: usize,
    pub skipped: usize,
    pub bytes: usize,
}

#[derive(Debug)]
pub struct Archived {
    /// The self-contained document, UTF-8.
    pub html: String,
    /// The URL the page finally came from (after redirects) — the base for
    /// resolving relative links.
    pub source_url: String,
    pub title: Option<String>,
    pub stats: ArchiveStats,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("page fetch: {0}")]
    Fetch(String),
    #[error("page is not HTML ({0})")]
    NotHtml(String),
    #[error("rewrite: {0}")]
    Rewrite(String),
}

const DOC_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const ASSET_ACCEPT: &str = "*/*";

/// Fetch `page_url` and return it as a single self-contained HTML document.
pub async fn archive_page(
    http: &HttpEngine,
    page_url: &str,
    budget: &ArchiveBudget,
) -> Result<Archived, ArchiveError> {
    let page = http
        .fetch_resource(page_url, budget.max_page_bytes, DOC_ACCEPT, budget.per_host)
        .await
        .map_err(|e| ArchiveError::Fetch(e.to_string()))?;

    tracing::debug!(bytes = page.bytes, "archive: page downloaded");
    let content_type = page.content_type.clone().unwrap_or_default();
    if !content_type.is_empty()
        && !content_type.contains("html")
        && !content_type.contains("xml")
        && !content_type.contains("text/plain")
    {
        return Err(ArchiveError::NotHtml(content_type));
    }

    let html = decode_document(&page.body, &content_type);
    let base = page.final_url.clone();

    // ---- pass 1: what does the page need? ---------------------------------
    //
    // `lol_html` handlers share their state through `Rc`/`RefCell`, which is
    // exactly right for a streaming single-threaded rewriter and exactly wrong
    // for a future that has to be `Send`. So the whole scan lives in its own
    // scope and hands back owned values: nothing non-`Send` is alive by the time
    // this function first awaits, which is what makes it spawnable.
    let (resources, stylesheet_hrefs, style_blocks): (Vec<ResourceRef>, Vec<String>, Vec<String>) = {
        let resources: Rc<RefCell<Vec<ResourceRef>>> = Rc::new(RefCell::new(Vec::new()));
        let seen: Rc<RefCell<FxHashSet<String>>> = Rc::new(RefCell::new(FxHashSet::default()));
        let base_rc: Rc<String> = Rc::new(base.clone());
        let stylesheet_hrefs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let style_blocks: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let style_index: Rc<Cell<usize>> = Rc::new(Cell::new(usize::MAX));

        rewrite_str(
            &html,
            RewriteStrSettings::new()
                .append_element_content_handler({
                    let (res, seen, base) =
                        (Rc::clone(&resources), Rc::clone(&seen), Rc::clone(&base_rc));
                    let h_hrefs = Rc::clone(&stylesheet_hrefs);
                    element!("link[href]", move |el| {
                        let rel = el.get_attribute("rel").unwrap_or_default().to_ascii_lowercase();
                        if rel.split_whitespace().any(|r| r == "stylesheet") {
                            let href = el.get_attribute("href").unwrap_or_default();
                            h_hrefs.borrow_mut().push(href.clone());
                            record_resource(&res, &seen, &base, &href, ResourceKind::Stylesheet);
                        } else if rel.split_whitespace().any(|r| r == "icon" || r == "apple-touch-icon") {
                            record_resource(
                                &res,
                                &seen,
                                &base,
                                &el.get_attribute("href").unwrap_or_default(),
                                ResourceKind::Image,
                            );
                        }
                        Ok(())
                    })
                })
                .append_element_content_handler({
                    let (res, seen, base) =
                        (Rc::clone(&resources), Rc::clone(&seen), Rc::clone(&base_rc));
                    element!("img[src]", move |el| {
                        record_resource(
                            &res,
                            &seen,
                            &base,
                            &el.get_attribute("src").unwrap_or_default(),
                            ResourceKind::Image,
                        );
                        Ok(())
                    })
                })
                .append_element_content_handler({
                    let (res, seen, base) =
                        (Rc::clone(&resources), Rc::clone(&seen), Rc::clone(&base_rc));
                    element!("[srcset]", move |el| {
                        for candidate in split_srcset(&el.get_attribute("srcset").unwrap_or_default()) {
                            record_resource(&res, &seen, &base, &candidate, ResourceKind::Image);
                        }
                        Ok(())
                    })
                })
                .append_element_content_handler({
                    let (res, seen, base) =
                        (Rc::clone(&resources), Rc::clone(&seen), Rc::clone(&base_rc));
                    element!("[style]", move |el| {
                        for url in css_urls(&el.get_attribute("style").unwrap_or_default()) {
                            record_resource(&res, &seen, &base, &url, ResourceKind::Other);
                        }
                        Ok(())
                    })
                })
                .append_element_content_handler({
                    let blocks = Rc::clone(&style_blocks);
                    let index = Rc::clone(&style_index);
                    element!("style", move |_el| {
                        let mut b = blocks.borrow_mut();
                        b.push(String::new());
                        index.set(b.len() - 1);
                        Ok(())
                    })
                })
                .append_element_content_handler({
                    let blocks = Rc::clone(&style_blocks);
                    let index = Rc::clone(&style_index);
                    text!("style", move |chunk| {
                        let i = index.get();
                        if i != usize::MAX
                            && let Some(slot) = blocks.borrow_mut().get_mut(i)
                        {
                            slot.push_str(chunk.as_str());
                        }
                        Ok(())
                    })
                }),
        )
        .map_err(|e| ArchiveError::Rewrite(e.to_string()))?;

        // CSS embedded in the document (not in external sheets) pulls its own
        // images and fonts, which the scan above could not see as attributes.
        {
            let mut res = resources.borrow_mut();
            let mut seen = seen.borrow_mut();
            for block in style_blocks.borrow().iter() {
                for url in css_urls(block) {
                    if let Some(abs) = absolutise(&base, &url)
                        && seen.insert(abs.clone())
                    {
                        res.push(ResourceRef { url: abs, kind: ResourceKind::Other });
                    }
                }
            }
            res.truncate(budget.max_resources);
        }

        let mut resources = take_rc(resources);
        resources.truncate(budget.max_resources);
        (resources, take_rc(stylesheet_hrefs), take_rc(style_blocks))
    };

    // ---- fetch everything ------------------------------------------------
    let mut fetched: FxHashMap<String, Fetched> = FxHashMap::default();
    let mut stats = ArchiveStats { resources_found: resources.len(), ..Default::default() };
    let mut total: u64 = 0;

    // Stylesheets first and in parallel; their contents can pull in more
    // (@import, fonts, background images), which is a second round.
    tracing::debug!(found = resources.len(), "archive: resources discovered");
    let (css_refs, other_refs): (Vec<_>, Vec<_>) =
        resources.iter().partition(|r| r.kind == ResourceKind::Stylesheet);

    let css = fetch_all(http, &css_refs, budget, &mut fetched, &mut stats, &mut total).await;

    // Round two: whatever the CSS referenced.
    let already: FxHashSet<String> = resources.iter().map(|r| r.url.clone()).collect();
    let mut nested: Vec<ResourceRef> = Vec::new();
    let mut nested_seen: FxHashSet<String> = FxHashSet::default();
    for (url, body) in &css {
        let text = decode_document(body, "text/css; charset=utf-8");
        for import in css_imports(&text) {
            if let Some(abs) = absolutise(url, &import)
                && nested_seen.insert(abs.clone())
            {
                nested.push(ResourceRef { url: abs, kind: ResourceKind::Stylesheet });
            }
        }
        for asset in css_urls(&text) {
            if let Some(abs) = absolutise(url, &asset)
                && nested_seen.insert(abs.clone())
                && !already.contains(&abs)
            {
                nested.push(ResourceRef { url: abs, kind: ResourceKind::Other });
            }
        }
    }
    nested.truncate(budget.max_resources.saturating_sub(other_refs.len()));
    let (nested_css, nested_assets): (Vec<_>, Vec<_>) =
        nested.iter().partition(|r| r.kind == ResourceKind::Stylesheet);
    tracing::debug!(css = css.len(), nested = nested.len(), "archive: stylesheets fetched");
    let nested_css_data =
        fetch_all(http, &nested_css, budget, &mut fetched, &mut stats, &mut total).await;
    let _ = fetch_all(http, &nested_assets, budget, &mut fetched, &mut stats, &mut total).await;
    let _ = fetch_all(http, &other_refs, budget, &mut fetched, &mut stats, &mut total).await;
    for (url, body) in nested_css_data {
        fetched.insert(url, Fetched { mime: "text/css".into(), body });
    }
    stats.bytes = total as usize;

    // Resolve every stylesheet to its (possibly rewritten-in-place) content.
    let mut css_map: FxHashMap<String, String> = FxHashMap::default();
    for r in resources.iter().chain(nested.iter()).filter(|r| r.kind == ResourceKind::Stylesheet) {
        if let Some(data) = fetched.get(&r.url) {
            let text = decode_document(&data.body, &data.mime);
            css_map.insert(r.url.clone(), rewrite_css(&text, &r.url, &fetched, 0));
        }
    }

    tracing::debug!(assets = other_refs.len(), "archive: assets fetched; rewriting");
    // ---- pass 2: rewrite ------------------------------------------------
    let href_queue: Rc<RefCell<std::collections::VecDeque<String>>> =
        Rc::new(RefCell::new(stylesheet_hrefs.into()));
    let style_queue: Rc<RefCell<std::collections::VecDeque<String>>> =
        Rc::new(RefCell::new(style_blocks.into()));
    let style_cursor: Rc<Cell<usize>> = Rc::new(Cell::new(usize::MAX));
    let style_emitted: Rc<RefCell<FxHashSet<usize>>> = Rc::new(RefCell::new(FxHashSet::default()));

    let base_for_rewrite: Rc<String> = Rc::new(base.clone());
    let fetched_for_rewrite = fetched.clone();

    let out = rewrite_str(
        &html,
        RewriteStrSettings::new()
            // --- drop things that would reach the network or execute ---
            .append_element_content_handler(element!(
                "script, noscript, iframe, object, embed, base, link[rel=preload], link[rel=prefetch], link[rel=preconnect], link[rel=dns-prefetch], link[rel=modulepreload]",
                |el| {
                    el.remove();
                    Ok(())
                }
            ))
            .append_element_content_handler(element!("meta[http-equiv]", |el| {
                let value = el.get_attribute("http-equiv").unwrap_or_default().to_ascii_lowercase();
                if value.contains("content-security-policy") || value.contains("refresh") {
                    el.remove();
                }
                Ok(())
            }))
            // The document is re-encoded to UTF-8 on the way out, so a page that
            // declared GBK must be told otherwise or it renders as mojibake.
            .append_element_content_handler(element!("meta[charset]", |el| {
                el.set_attribute("charset", "utf-8").ok();
                Ok(())
            }))
            .append_element_content_handler(element!("meta[http-equiv=content-type]", |el| {
                el.set_attribute("content", "text/html; charset=utf-8").ok();
                Ok(())
            }))
            // --- inline ---
            .append_element_content_handler({
                let queue = Rc::clone(&href_queue);
                let css = css_map.clone();
                let base_for_rewrite = Rc::clone(&base_for_rewrite);
                element!("link[rel~=stylesheet]", move |el| {
                    let href = queue.borrow_mut().pop_front().unwrap_or_default();
                    let absolute = absolutise(&base_for_rewrite, &href).unwrap_or(href);
                    match css.get(&absolute) {
                        Some(sheet) => el.replace(
                            &format!("<style>{sheet}</style>"),
                            ContentType::Html,
                        ),
                        None => {
                            el.remove();
                        }
                    }
                    Ok(())
                })
            })
            .append_element_content_handler({
                let queue = Rc::clone(&style_queue);
                let cursor = Rc::clone(&style_cursor);
                let emitted = Rc::clone(&style_emitted);
                let fetched = fetched_for_rewrite.clone();
                element!("style", move |_el| {
                    cursor.set(cursor.get().wrapping_add(1));
                    let _ = &emitted;
                    let _ = &fetched;
                    let _ = &queue;
                    Ok(())
                })
            })
            .append_element_content_handler({
                let cursor = Rc::clone(&style_cursor);
                let emitted = Rc::clone(&style_emitted);
                let fetched = fetched_for_rewrite.clone();
                let base_for_rewrite = Rc::clone(&base_for_rewrite);
                text!("style", move |chunk| {
                    let index = cursor.get();
                    if index == usize::MAX || !emitted.borrow_mut().insert(index) {
                        // Already wrote this block (or never saw its start tag):
                        // later chunks of the same node must not duplicate it.
                        chunk.remove();
                        return Ok(());
                    }
                    let rewritten = rewrite_css(chunk.as_str(), &base_for_rewrite, &fetched, 0);
                    // Inside <style> the content is raw CSS, not HTML text.
                    chunk.replace(&rewritten, ContentType::Html);
                    Ok(())
                })
            })
            .append_element_content_handler({
                let fetched = fetched_for_rewrite.clone();
                let base = Rc::clone(&base_for_rewrite);
                element!("[style]", move |el| {
                    let value = el.get_attribute("style").unwrap_or_default();
                    if value.contains("url(") {
                        let rewritten = rewrite_css(&value, &base, &fetched, 0);
                        el.set_attribute("style", &rewritten).ok();
                    }
                    Ok(())
                })
            })
            .append_element_content_handler({
                let fetched = fetched_for_rewrite.clone();
                let base = Rc::clone(&base_for_rewrite);
                element!("img[src]", move |el| {
                    let src = el.get_attribute("src").unwrap_or_default();
                    if let Some(data) = data_uri(&base, &src, &fetched) {
                        el.set_attribute("src", &data).ok();
                    } else if let Some(abs) = absolutise(&base, &src) {
                        el.set_attribute("src", &abs).ok();
                    }
                    // A leftover srcset would make the browser fetch the
                    // original (possibly oversized) image again.
                    el.remove_attribute("srcset");
                    el.remove_attribute("loading");
                    Ok(())
                })
            })
            .append_element_content_handler(element!("[srcset]", |el| {
                el.remove_attribute("srcset");
                Ok(())
            }))
            // --- make everything else absolute so the file works offline-ish ---
            .append_element_content_handler({
                let base = Rc::clone(&base_for_rewrite);
                element!("[href]", move |el| {
                    let href = el.get_attribute("href").unwrap_or_default();
                    if href.starts_with('#') || href.starts_with("data:") || href.starts_with("mailto:") {
                        return Ok(());
                    }
                    if let Some(abs) = absolutise(&base, &href) {
                        el.set_attribute("href", &abs).ok();
                    }
                    Ok(())
                })
            })
            .append_element_content_handler({
                let base = Rc::clone(&base_for_rewrite);
                element!("[poster]", move |el| {
                    let poster = el.get_attribute("poster").unwrap_or_default();
                    if let Some(abs) = absolutise(&base, &poster) {
                        el.set_attribute("poster", &abs).ok();
                    }
                    Ok(())
                })
            }),
    )
    .map_err(|e| ArchiveError::Rewrite(e.to_string()))?;

    tracing::debug!(bytes = out.len(), "archive: rewrite finished");
    let title = extract_title(&out);
    Ok(Archived { html: out, source_url: base, title, stats })
}

// ---------------------------------------------------------------------------
// resources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    Stylesheet,
    Image,
    Other,
}

#[derive(Debug)]
struct ResourceRef {
    url: String,
    kind: ResourceKind,
}

#[derive(Debug, Clone)]
struct Fetched {
    mime: String,
    body: Vec<u8>,
}

/// Take the value out of a shared cell, tolerating a stray clone.
fn take_rc<T: Default>(cell: Rc<RefCell<T>>) -> T {
    match Rc::try_unwrap(cell) {
        Ok(cell) => cell.into_inner(),
        Err(shared) => std::mem::take(&mut *shared.borrow_mut()),
    }
}

/// Record one resource reference, deduplicated and resolved against `base`.
fn record_resource(
    resources: &Rc<RefCell<Vec<ResourceRef>>>,
    seen: &Rc<RefCell<FxHashSet<String>>>,
    base: &str,
    raw: &str,
    kind: ResourceKind,
) {
    let Some(abs) = absolutise(base, raw) else {
        return;
    };
    if seen.borrow_mut().insert(abs.clone()) {
        resources.borrow_mut().push(ResourceRef { url: abs, kind });
    }
}

/// Fetch a batch concurrently and return the CSS bodies as `(url, bytes)`.
async fn fetch_all(
    http: &HttpEngine,
    refs: &[&ResourceRef],
    budget: &ArchiveBudget,
    fetched: &mut FxHashMap<String, Fetched>,
    stats: &mut ArchiveStats,
    total: &mut u64,
) -> Vec<(String, Vec<u8>)> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut css_out = Vec::new();
    // Bounded fan-out: `HttpEngine` also bounds globally and per host.
    const WIDTH: usize = 6;
    for window in refs.chunks(WIDTH) {
        let mut futures = Vec::with_capacity(window.len());
        for r in window {
            let url = r.url.clone();
            futures.push(async move {
                let res = http
                    .fetch_resource(&url, budget.max_resource_bytes, ASSET_ACCEPT, budget.per_host)
                    .await;
                (url, res)
            });
        }
        for (url, res) in futures::future::join_all(futures).await {
            match res {
                Ok(resp) => {
                    if *total + resp.bytes > budget.max_total_bytes {
                        stats.skipped += 1;
                        continue;
                    }
                    *total += resp.bytes;
                    stats.inlined += 1;
                    let mime = resp
                        .content_type
                        .clone()
                        .unwrap_or_else(|| guess_mime(&url).to_string());
                    if mime.contains("css") {
                        css_out.push((url.clone(), resp.body.clone()));
                    }
                    fetched.insert(url, Fetched { mime, body: resp.body });
                }
                Err(e) => {
                    stats.skipped += 1;
                    tracing::debug!(url, error = %e, "archive: resource skipped");
                }
            }
        }
    }
    css_out
}

/// Replace `url(...)` references and `@import` rules with inlined data URIs.
fn rewrite_css(
    css: &str,
    base: &str,
    fetched: &FxHashMap<String, Fetched>,
    depth: u32,
) -> String {
    if depth > 4 || !css.contains("url(") && !css.contains("@import") {
        return css.to_string();
    }
    let mut out = String::with_capacity(css.len() + 64);
    let bytes = css.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // url( ... )
        if bytes[i..].starts_with(b"url(") {
            let start = i + 4;
            let Some(close) = css[start..].find(')') else {
                out.push_str(&css[i..]);
                break;
            };
            let raw = css[start..start + close].trim().trim_matches(['"', '\'']).trim();
            match data_uri(base, raw, fetched) {
                Some(data) => {
                    out.push_str("url(");
                    out.push_str(&data);
                    out.push(')');
                }
                None => {
                    out.push_str("url(");
                    out.push_str(&css[start..start + close]);
                    out.push(')');
                }
            }
            i = start + close + 1;
            continue;
        }
        // @import "…"; — the referenced sheet is inlined in place, recursively.
        if bytes[i..].starts_with(b"@import")
            && let Some(semi) = css[i..].find(';')
        {
            let rule = &css[i..i + semi];
            let target = rule
                .split_once("@import")
                .map(|(_, rest)| rest)
                .unwrap_or("")
                .trim();
            let target = if target.starts_with("url(") {
                target
                    .trim_start_matches("url(")
                    .split(')')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches(['"', '\''])
            } else {
                target.split_whitespace().next().unwrap_or("").trim_matches(['"', '\''])
            };
            match absolutise(base, target) {
                Some(abs) => match fetched.get(&abs) {
                    Some(data) => {
                        let nested = decode_document(&data.body, &data.mime);
                        out.push_str(&rewrite_css(&nested, &abs, fetched, depth + 1));
                    }
                    None => out.push_str(rule),
                },
                None => out.push_str(rule),
            }
            out.push(';');
            i += semi + 1;
            continue;
        }
        let ch_len = css[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&css[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn data_uri(base: &str, raw: &str, fetched: &FxHashMap<String, Fetched>) -> Option<String> {
    if raw.is_empty() || raw.starts_with("data:") || raw.starts_with('#') {
        return None;
    }
    let abs = absolutise(base, raw)?;
    let data = fetched.get(&abs)?;
    let mime = if data.mime.is_empty() { guess_mime(&abs).to_string() } else { data.mime.clone() };
    let mut uri = String::with_capacity(data.body.len() * 4 / 3 + 32);
    uri.push_str("data:");
    uri.push_str(&mime);
    uri.push_str(";base64,");
    uri.push_str(&base64::engine::general_purpose::STANDARD.encode(&data.body));
    Some(uri)
}

/// Turn a possibly relative reference into an absolute URL.
pub fn absolutise(base: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("data:") || raw.starts_with('#') || raw.starts_with("javascript:") {
        return None;
    }
    let base = reqwest::Url::parse(base).ok()?;
    let joined = base.join(raw).ok()?;
    match joined.scheme() {
        "http" | "https" => Some(joined.to_string()),
        _ => None,
    }
}

/// Every `url(...)` target in a chunk of CSS or a `style` attribute.
fn css_urls(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = css;
    while let Some(at) = rest.find("url(") {
        let start = at + 4;
        let Some(close) = rest[start..].find(')') else {
            break;
        };
        let raw = rest[start..start + close].trim().trim_matches(['"', '\'']).trim();
        if !raw.is_empty() {
            out.push(raw.to_string());
        }
        rest = &rest[start + close + 1..];
    }
    out
}

fn css_imports(css: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = css;
    while let Some(at) = rest.find("@import") {
        let after = &rest[at + 7..];
        let end = after.find(';').unwrap_or(after.len().min(200));
        let rule = &after[..end];
        let target = if rule.trim_start().starts_with("url(") {
            rule.trim_start()
                .trim_start_matches("url(")
                .split(')')
                .next()
                .unwrap_or("")
        } else {
            rule.split_whitespace().next().unwrap_or("")
        };
        let target = target.trim().trim_matches(['"', '\'']);
        if !target.is_empty() {
            out.push(target.to_string());
        }
        rest = &after[end.min(after.len())..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

fn split_srcset(srcset: &str) -> Vec<String> {
    srcset
        .split(',')
        .filter_map(|candidate| candidate.split_whitespace().next())
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
        .collect()
}

fn guess_mime(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".avif") {
        "image/avif"
    } else {
        "application/octet-stream"
    }
}

/// Decode a response body using the declared charset, falling back to UTF-8.
///
/// Chinese blogs still ship GBK/GB18030 pages; decoding those as UTF-8 turns the
/// saved archive into mojibake.
fn decode_document(body: &[u8], content_type: &str) -> String {
    let charset = content_type
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("charset="))
        .map(|c| c.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
        .next();
    if let Some(label) = charset.as_deref()
        && let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes())
        && encoding != encoding_rs::UTF_8
    {
        let (text, _, _) = encoding.decode(body);
        return text.into_owned();
    }
    match std::str::from_utf8(body) {
        Ok(text) => text.to_string(),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let end = lower[open_end..].find("</title>")? + open_end;
    let title = crate::text::squeeze_ws(&html[open_end..end], 200);
    (!title.is_empty()).then_some(title)
}

/// Where an archived page should live on disk, next to the database.
pub fn archive_path(dir: &std::path::Path, page_url: &str) -> std::path::PathBuf {
    dir.join(format!("{:016x}.html", crate::text::hash64(page_url.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_urls_are_extracted() {
        let css = "a{background:url('a.png')} b{background-image: url( /b.jpg )} c{color:red}";
        assert_eq!(css_urls(css), vec!["a.png".to_string(), "/b.jpg".to_string()]);
    }

    #[test]
    fn css_imports_are_extracted() {
        assert_eq!(css_imports("@import url(\"base.css\"); a{}"), vec!["base.css".to_string()]);
        assert_eq!(css_imports("@import 'x.css' screen;"), vec!["x.css".to_string()]);
    }

    #[test]
    fn srcset_candidates() {
        assert_eq!(
            split_srcset("a.png 1x, b.png 2x, c.png 100w"),
            vec!["a.png".to_string(), "b.png".to_string(), "c.png".to_string()]
        );
    }

    #[test]
    fn absolutise_resolves_and_rejects() {
        assert_eq!(
            absolutise("https://e.com/a/b.html", "../img/x.png").as_deref(),
            Some("https://e.com/img/x.png")
        );
        assert_eq!(absolutise("https://e.com/a", "data:image/png;base64,AA"), None);
        assert_eq!(absolutise("https://e.com/a", "#anchor"), None);
        assert_eq!(absolutise("https://e.com/a", "javascript:void(0)"), None);
    }

    #[test]
    fn decodes_gbk_pages() {
        // "中文" in GBK.
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let text = decode_document(&gbk, "text/html; charset=gbk");
        assert_eq!(text, "中文");
        let utf8 = "中文".as_bytes();
        assert_eq!(decode_document(utf8, "text/html; charset=utf-8"), "中文");
    }

    #[test]
    fn mime_guessing() {
        assert_eq!(guess_mime("https://e.com/a.PNG?v=2"), "image/png");
        assert_eq!(guess_mime("https://e.com/f.woff2"), "font/woff2");
    }

    #[test]
    fn rewrites_css_urls_from_the_map() {
        let mut fetched = FxHashMap::default();
        // The key is the *resolved* URL: url(a.png) against https://e.com/x/.
        fetched.insert(
            "https://e.com/x/a.png".to_string(),
            Fetched { mime: "image/png".into(), body: vec![1, 2, 3] },
        );
        let out = rewrite_css("a{background:url(a.png)}", "https://e.com/x/", &fetched, 0);
        assert!(out.contains("data:image/png;base64,AQID"), "{out}");
    }

    #[test]
    fn title_extraction() {
        assert_eq!(extract_title("<html><head><title> Hi  there </title>").as_deref(), Some("Hi there"));
        assert_eq!(extract_title("<html>"), None);
    }

    #[test]
    fn archive_path_is_stable() {
        let dir = std::path::Path::new("/tmp");
        assert_eq!(archive_path(dir, "https://e.com/a"), archive_path(dir, "https://e.com/a"));
        assert_ne!(archive_path(dir, "https://e.com/a"), archive_path(dir, "https://e.com/b"));
    }
}

//! Finding feeds: from a URL the reader typed, or from a blog's links page.
//!
//! Two jobs, one theme — turning "here is a website" into "here is a feed I can
//! subscribe to":
//!
//! * [`discover`] takes a URL and answers with a feed: the URL itself if it is
//!   already a feed, otherwise the document's `<link rel="alternate">` (what a
//!   browser's feed button reads), otherwise the conventional paths
//!   (`/feed`, `/rss.xml`, …).
//! * [`friend_candidates`] takes a page and finds the *blogs* on it, which is
//!   what a blogroll is. Membership is decided by `discover` rather than by
//!   guessing at markup: a link that resolves to a feed is a blog, and one that
//!   does not is a news article or a social profile.
//!
//! The HTML halves are pure functions with tests, because markup heuristics are
//! exactly the kind of code that rots silently.

use std::sync::Arc;

use crate::http::HttpEngine;

/// A feed found somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub url: Arc<str>,
    pub title: Arc<str>,
}

/// A link found on a page that might be somebody's blog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: Arc<str>,
    /// The anchor text, which is usually the blog's name.
    pub label: Arc<str>,
}

/// Paths tried when a site advertises nothing. Ordered by how common they are.
const COMMON_FEED_PATHS: [&str; 8] = [
    "/feed",
    "/feed.xml",
    "/rss.xml",
    "/atom.xml",
    "/index.xml",
    "/rss",
    "/feed/",
    "/feeds/posts/default",
];

/// Find a feed for `url`, trying the conventional paths as a last resort.
///
/// This is the thorough answer, for a URL somebody typed and is waiting on.
pub async fn discover(http: &HttpEngine, url: &str, max_bytes: u64) -> Result<Feed, String> {
    discover_inner(http, url, max_bytes, COMMON_FEED_PATHS.len()).await
}

/// The cheap answer: the URL itself, or what the page advertises.
///
/// Used when scanning a blogroll, where the alternative is `N × 10` requests to
/// strangers' servers because most links do not lead to feeds.
pub async fn discover_quick(http: &HttpEngine, url: &str, max_bytes: u64) -> Result<Feed, String> {
    discover_inner(http, url, max_bytes, 0).await
}

/// Between the two: what a blogroll scan uses. Half of the blogs on a links page
/// do not advertise their feed, and the first few conventional paths catch most
/// of those without the ten requests the thorough answer may cost.
pub async fn discover_common(http: &HttpEngine, url: &str, max_bytes: u64) -> Result<Feed, String> {
    discover_inner(http, url, max_bytes, 3).await
}

async fn discover_inner(
    http: &HttpEngine,
    url: &str,
    max_bytes: u64,
    common_paths: usize,
) -> Result<Feed, String> {
    let base = url.trim();
    if base.is_empty() {
        return Err("空的地址".into());
    }
    let page = http
        .fetch_resource(base, max_bytes, "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8", 2)
        .await
        .map_err(|e| e.to_string())?;

    // The URL may already be a feed, which is the common case when someone pastes
    // the address out of a reader.
    let body = String::from_utf8_lossy(&page.body);
    if looks_like_feed(&body) {
        let title = feed_title(&body).unwrap_or_else(|| base.to_string());
        return Ok(Feed { url: Arc::from(base), title: Arc::from(title) });
    }

    // Otherwise the page should advertise one.
    if let Some(href) = advertised_feeds(&body)
        .into_iter()
        .next()
        .and_then(|href| resolve(&page.final_url, &href))
        && let Ok(feed) = http.fetch_resource(&href, max_bytes, "*/*", 2).await
        && looks_like_feed(&String::from_utf8_lossy(&feed.body))
    {
        let title = feed_title(&String::from_utf8_lossy(&feed.body))
            .unwrap_or_else(|| base.to_string());
        return Ok(Feed { url: Arc::from(href.as_str()), title: Arc::from(title) });
    }

    if common_paths == 0 {
        return Err("这个链接没有 feed".into());
    }
    // Last resort: the paths everyone uses.
    let origin = origin_of(&page.final_url).ok_or_else(|| "无法解析站点地址".to_string())?;
    for path in COMMON_FEED_PATHS.into_iter().take(common_paths) {
        let candidate = format!("{origin}{path}");
        let Ok(resp) = http.fetch_resource(&candidate, max_bytes, "*/*", 2).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&resp.body);
        if looks_like_feed(&text) {
            let title = feed_title(&text).unwrap_or_else(|| base.to_string());
            return Ok(Feed { url: Arc::from(candidate.as_str()), title: Arc::from(title) });
        }
    }
    Err("没有找到这个站点的 feed".into())
}

/// Does this document look like RSS or Atom?
pub fn looks_like_feed(text: &str) -> bool {
    let head: String = text.chars().take(2048).collect::<String>().to_lowercase();
    if head.contains("<html") && !head.contains("<rss") && !head.contains("<feed") {
        return false;
    }
    head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:rdf")
}

/// The feed's own title, if it declares one.
pub fn feed_title(text: &str) -> Option<String> {
    // `<title>` inside `<channel>` (RSS) or the feed element (Atom) — both are
    // simply the first title in the document.
    let lower = text.to_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let end = lower[open_end..].find("</title>")? + open_end;
    let raw = text[open_end..end].trim();
    let cleaned = crate::text::html_to_text(raw, 120);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// Feed URLs a document advertises: `<link rel="alternate" type="…+xml">`.
pub fn advertised_feeds(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tag in tags(html, "link") {
        let rel = attr(&tag, "rel").unwrap_or_default().to_lowercase();
        let kind = attr(&tag, "type").unwrap_or_default().to_lowercase();
        if !rel.split_whitespace().any(|r| r == "alternate") {
            continue;
        }
        if !(kind.contains("rss") || kind.contains("atom") || kind.contains("xml")) {
            continue;
        }
        if let Some(href) = attr(&tag, "href")
            && !href.trim().is_empty()
        {
            out.push(href.trim().to_string());
        }
    }
    out
}

/// Links on a page that might be other blogs.
///
/// Everything external and human-looking is offered; what makes a link a *blog*
/// is that [`discover`] finds a feed behind it, which is the caller's next step.
/// That keeps this function honest — it is a filter for obvious non-candidates
/// (scripts, images, social silos, the site's own pages), not a classifier.
pub fn friend_candidates(html: &str, page_url: &str, limit: usize) -> Vec<Candidate> {
    candidates(html, page_url, limit, false)
}

/// Every link worth looking at, including the page's own host.
///
/// A blogroll is other people's blogs, so [`friend_candidates`] drops the same
/// host. A *directory* — a curated list of feeds, a route index, a link roundup
/// — is the opposite: everything useful is usually on one host, and dropping it
/// leaves nothing to read.
pub fn page_candidates(html: &str, page_url: &str, limit: usize) -> Vec<Candidate> {
    candidates(html, page_url, limit, true)
}

fn candidates(html: &str, page_url: &str, limit: usize, keep_own_host: bool) -> Vec<Candidate> {
    let page_host = host_of(page_url);
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for anchor in anchors(html) {
        let Some(href) = attr(&anchor.inner, "href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') || href.starts_with("mailto:") || href.starts_with("javascript:") {
            continue;
        }
        let Some(url) = resolve(page_url, href) else {
            continue;
        };
        let host = host_of(&url);
        if host.is_empty() || is_noise_host(&host) {
            continue;
        }
        if !keep_own_host && host == page_host {
            continue;
        }
        // The anchor text is the blog's name far more often than the URL is.
        // Collapsed to one line: an anchor can wrap or contain a `<br>`, and the
        // list shows one line per blog.
        let label = crate::text::squeeze_ws(&crate::text::html_to_text(&anchor.text, 120), 80);
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        if !seen.insert(url.clone()) {
            continue;
        }
        out.push(Candidate { url: Arc::from(url.as_str()), label: Arc::from(label) });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Hosts that are never somebody's blog in a blogroll.
fn is_noise_host(host: &str) -> bool {
    const NOISE: [&str; 31] = [
        "twitter.com", "x.com", "facebook.com", "instagram.com", "youtube.com", "youtu.be",
        "github.com", "gitlab.com", "gitee.com", "weibo.com", "zhihu.com", "douban.com",
        "juejin.cn", "csdn.net", "medium.com", "reddit.com", "linkedin.com", "t.me",
        "google.com", "apple.com", "microsoft.com", "beian.miit.gov.cn",
        // Frameworks, licences and hosting: they appear in every blog's footer,
        // and they have feeds, so the "does it have a feed" test alone lets them
        // through.
        "creativecommons.org", "gohugo.io", "jekyllrb.com", "hexo.io",
        "wordpress.org", "w3.org", "cloudflare.com", "vercel.com", "netlify.com",
    ];
    let bare = host.strip_prefix("www.").unwrap_or(host);
    NOISE.iter().any(|n| bare == *n || bare.ends_with(&format!(".{n}")))
}

/// Does the page look like it has a blogroll at all? Used to decide whether to
/// bother probing every link.
pub fn mentions_links(html: &str) -> bool {
    const WORDS: [&str; 10] = [
        "友情链接", "友链", "友情鏈接", "友情連結", "链接", "鏈接", "blogroll", "links", "friends", "邻居",
    ];
    let lower = html.to_lowercase();
    WORDS.iter().any(|w| lower.contains(&w.to_lowercase()))
}

// -- small HTML helpers -------------------------------------------------
//
// A hand-rolled tag scanner rather than a parser: the archiver already carries
// `lol_html` for rewriting, but this only needs attributes and text out of a
// page whose markup may be anything, and a scanner that cannot throw is worth
// more here than one that is correct about namespaces.

struct Anchor {
    inner: String,
    text: String,
}

fn anchors(html: &str) -> Vec<Anchor> {
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("<a ") {
        let start = cursor + rel;
        let Some(open_end_rel) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_rel + 1;
        let Some(close_rel) = lower[open_end..].find("</a>") else {
            break;
        };
        let close = open_end + close_rel;
        out.push(Anchor {
            inner: html[start..open_end].to_string(),
            text: html[open_end..close].to_string(),
        });
        cursor = close + 4;
        if out.len() > 4000 || cursor >= bytes.len() {
            break;
        }
    }
    out
}

fn tags(html: &str, name: &str) -> Vec<String> {
    let lower = html.to_lowercase();
    let needle = format!("<{name}");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&needle) {
        let start = cursor + rel;
        // `<link` must not match `<linkage`.
        let after = lower.as_bytes().get(start + needle.len()).copied();
        if !matches!(after, Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'/') | Some(b'>')) {
            cursor = start + needle.len();
            continue;
        }
        let Some(end_rel) = lower[start..].find('>') else {
            break;
        };
        let end = start + end_rel;
        out.push(html[start..end].to_string());
        cursor = end + 1;
        if out.len() > 2000 {
            break;
        }
    }
    out
}

/// One attribute's value, single- or double-quoted, case-insensitively.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(name) {
        let at = cursor + rel;
        let before_ok = at == 0
            || matches!(lower.as_bytes()[at - 1], b' ' | b'\t' | b'\n' | b'\r' | b'<' | b'"' | b'\'');
        let after = lower.as_bytes().get(at + name.len()).copied();
        if before_ok && matches!(after, Some(b'=') | Some(b' ') | Some(b'\t')) {
            let rest = &tag[at + name.len()..];
            let Some(eq) = rest.find('=') else {
                cursor = at + name.len();
                continue;
            };
            let value = rest[eq + 1..].trim_start();
            let mut chars = value.chars();
            let quote = chars.next()?;
            if quote == '"' || quote == '\'' {
                let end = value[1..].find(quote)? + 1;
                return Some(value[1..end].to_string());
            }
            let end = value.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(value.len());
            return Some(value[..end].to_string());
        }
        cursor = at + name.len();
    }
    None
}

/// Resolve a possibly relative URL against a page URL, without pulling in a URL
/// crate beyond the one already in the tree.
pub fn resolve(page_url: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let parsed = reqwest::Url::parse(page_url).ok()?;
    parsed.join(href).ok().map(|u| u.to_string())
}

fn origin_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    Some(format!("{}://{}", parsed.scheme(), parsed.host_str()?))
}

/// The hostname of a URL, lowercased and without `www.` — the identity of a
/// site, as opposed to the identity of one feed address on it.
pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_feeds_and_their_titles() {
        let rss = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Dvel's Blog</title><item><title>x</title></item></channel></rss>"#;
        assert!(looks_like_feed(rss));
        assert_eq!(feed_title(rss).as_deref(), Some("Dvel's Blog"));

        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>少数派</title></feed>"#;
        assert!(looks_like_feed(atom));
        assert_eq!(feed_title(atom).as_deref(), Some("少数派"));

        assert!(!looks_like_feed("<!DOCTYPE html><html><body>hi</body></html>"));
    }

    #[test]
    fn reads_advertised_feeds() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" title="RSS" href="/feed.xml">
            <link rel="stylesheet" href="/style.css">
            <link rel='alternate' type='application/atom+xml' href='https://x.example/atom.xml'>
            </head></html>"#;
        let feeds = advertised_feeds(html);
        assert_eq!(feeds, vec!["/feed.xml", "https://x.example/atom.xml"]);
    }

    #[test]
    fn finds_friend_links_and_ignores_noise() {
        let html = r#"<div class="links"><h3>友情链接</h3>
            <a href="https://cavill.site/">少数派的诚实</a>
            <a href="/about">关于我</a>
            <a href="https://github.com/someone">GitHub</a>
            <a href="https://twitter.com/x">Twitter</a>
            <a href="mailto:a@b.c">mail</a>
            <a href="https://another.example/blog">另一个博客</a>
            </div>"#;
        let found = friend_candidates(html, "https://dvel.me/about/", 30);
        let urls: Vec<&str> = found.iter().map(|c| c.url.as_ref()).collect();
        assert_eq!(urls, vec!["https://cavill.site/", "https://another.example/blog"]);
        assert_eq!(found[0].label.as_ref(), "少数派的诚实");
        assert!(mentions_links(html));
        // A directory keeps its own host; a blogroll does not.
        let directory = page_candidates(html, "https://dvel.me/about/", 30);
        let dir_urls: Vec<&str> = directory.iter().map(|c| c.url.as_ref()).collect();
        assert!(dir_urls.contains(&"https://dvel.me/about"), "{dir_urls:?}");
    }

    #[test]
    fn resolves_relative_links_against_the_page() {
        assert_eq!(
            resolve("https://dvel.me/about/", "../blog/list").as_deref(),
            Some("https://dvel.me/blog/list")
        );
        assert_eq!(
            resolve("https://dvel.me/posts/1.html", "/friends").as_deref(),
            Some("https://dvel.me/friends")
        );
    }

    #[test]
    fn attribute_parsing_handles_both_quote_styles_and_missing_ones() {
        assert_eq!(attr(r#"<a href="/x">"#, "href").as_deref(), Some("/x"));
        assert_eq!(attr(r#"<a href='/y'>"#, "href").as_deref(), Some("/y"));
        assert_eq!(attr(r#"<a href=/z>"#, "href").as_deref(), Some("/z"));
        // `href` inside another attribute's name must not match.
        assert_eq!(attr(r#"<a data-href="no" href="yes">"#, "href").as_deref(), Some("yes"));
        assert_eq!(attr(r#"<a title="x">"#, "href"), None);
    }

    #[test]
    fn a_link_tag_is_not_a_linkage_tag() {
        assert!(tags("<linkage>x</linkage>", "link").is_empty());
        assert_eq!(tags("<link rel=\"alternate\">", "link").len(), 1);
    }
}

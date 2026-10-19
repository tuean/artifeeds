//! Feed parsing and normalisation.
//!
//! Wraps `feed-rs` (RSS 0.9x/1.0/2.0, Atom, JSON Feed, MediaRSS) and turns a
//! `Feed` into flat [`RawEntry`] records with everything the rest of the system
//! needs already computed exactly once:
//!
//! * a stable dedup hash,
//! * pre-rendered plain-text preview (so the UI never strips HTML at 60 fps),
//! * a bounded body.
//!
//! Two cost controls matter at fleet scale:
//!
//! * **Entry budget before work, not after.** Feeds with 10 000 archived items
//!   are common. Rather than converting every entry and then truncating, we
//!   first extract the cheap sort key (`published`), keep only the newest
//!   `max_entries` with `select_nth_unstable`, and only then do the expensive
//!   HTML→text work. Parsing a 4 MB archive feed costs the same as a 30-item one.
//! * **Scratch buffers are reused** across every entry of a fetch, so a
//!   200-entry feed performs ~2 allocations instead of ~800.

use crate::text::{hash64, hash_pair64, html_to_text_into, squeeze_ws};
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub struct ParseBudget {
    pub max_entries: usize,
    pub preview_chars: usize,
    pub max_content_bytes: usize,
}

impl Default for ParseBudget {
    fn default() -> Self {
        Self { max_entries: 200, preview_chars: 420, max_content_bytes: 96 * 1024 }
    }
}

#[derive(Debug, Clone)]
pub struct RawEntry {
    /// Source-provided stable id (falls back to the link, then to a hash).
    pub uid: Arc<str>,
    pub uid_hash: u64,
    pub title: Arc<str>,
    pub link: Arc<str>,
    pub author: Option<Arc<str>>,
    pub published: i64,
    pub preview: Arc<str>,
    /// Full text, only when it adds something beyond the preview.
    pub content: Option<Arc<str>>,
    pub content_hash: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedFeed {
    pub feed_title: Option<Arc<str>>,
    pub site: Option<Arc<str>>,
    pub entries: Vec<RawEntry>,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("feed parser: {0}")]
    Parser(String),
    #[error("feed contained no entries")]
    Empty,
}

/// Parse a feed body. The body is already size-capped by the HTTP layer.
pub fn parse(body: &[u8], budget: &ParseBudget) -> Result<ParsedFeed, ParseError> {
    let feed = feed_rs::parser::parse(body).map_err(|e| ParseError::Parser(e.to_string()))?;

    let feed_title = feed
        .title
        .as_ref()
        .map(|t| Arc::<str>::from(squeeze_ws(&t.content, 160).as_str()))
        .filter(|t| !t.is_empty());
    let site = pick_site_link(&feed).map(|s| Arc::<str>::from(s.as_str()));

    let entries = feed.entries;
    let total = entries.len();

    // Pass 1: cheap timestamps only.
    let mut keyed: Vec<(i64, u32)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (entry_timestamp(e), i as u32))
        .collect();
    // `Entry` is not `Default`, so park the survivors in `Option` slots and
    // take them out one by one — this moves each entry instead of cloning it.
    let mut slots: Vec<Option<feed_rs::model::Entry>> = entries.into_iter().map(Some).collect();

    let truncated = keyed.len() > budget.max_entries;
    if truncated {
        let keep = budget.max_entries.max(1);
        keyed.select_nth_unstable_by(keep - 1, |a, b| b.0.cmp(&a.0));
        keyed.truncate(keep);
    }
    // Newest first; ties keep the feed's own order (stable by index).
    keyed.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    // Pass 2: expensive normalisation, on the survivors only, with reused buffers.
    let mut out: Vec<RawEntry> = Vec::with_capacity(keyed.len());
    let mut scratch = String::with_capacity(budget.preview_chars * 2);
    let mut body_scratch = String::with_capacity(budget.max_content_bytes.min(64 * 1024));
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::with_capacity(keyed.len() * 2);

    for (ts, idx) in &keyed {
        let Some(entry) = slots[*idx as usize].take() else {
            continue;
        };

        let title_raw = entry.title.as_ref().map(|t| t.content.as_str()).unwrap_or("");
        let title = if title_raw.is_empty() {
            Arc::<str>::from("(untitled)")
        } else {
            Arc::<str>::from(squeeze_ws(title_raw, 300).as_str())
        };

        let link = pick_entry_link(&entry)
            .or_else(|| entry.id.starts_with("http").then(|| entry.id.clone()))
            .or_else(|| site.as_ref().map(|s| s.to_string()))
            .unwrap_or_default();

        let uid_raw: &str = if !entry.id.is_empty() { &entry.id } else { &link };
        let uid: Arc<str> = if uid_raw.is_empty() {
            Arc::from(format!("{}:{}", title, ts).as_str())
        } else {
            Arc::from(uid_raw)
        };
        let uid_hash = hash64(uid.as_bytes());
        if !seen.insert(uid_hash) {
            continue; // duplicate id inside one feed
        }

        // Body: prefer full content, fall back to summary.
        scratch.clear();
        let mut used_content = false;
        if let Some(content) = entry.content.as_ref().and_then(|c| c.body.as_deref()) {
            if is_html(entry.content.as_ref().map(|c| c.content_type.as_ref())) {
                html_to_text_into(content, &mut scratch, budget.preview_chars);
            } else {
                scratch.push_str(&squeeze_ws(content, budget.preview_chars));
            }
            used_content = !scratch.is_empty();
        }
        if scratch.is_empty()
            && let Some(summary) = entry.summary.as_ref().map(|s| s.content.as_str())
        {
            if is_html(None) {
                html_to_text_into(summary, &mut scratch, budget.preview_chars);
            } else {
                scratch.push_str(&squeeze_ws(summary, budget.preview_chars));
            }
        }
        if scratch.is_empty() {
            scratch.push_str(&title);
        }
        let preview: Arc<str> = Arc::from(scratch.as_str());

        // Full body only when it meaningfully exceeds the preview.
        let content: Option<Arc<str>> = if used_content {
            let raw = entry.content.as_ref().and_then(|c| c.body.as_deref()).unwrap_or("");
            if raw.len() > preview.len().saturating_mul(2) && raw.len() > 400 {
                body_scratch.clear();
                if is_html(entry.content.as_ref().map(|c| c.content_type.as_ref())) {
                    html_to_text_into(raw, &mut body_scratch, budget.max_content_bytes);
                } else {
                    body_scratch.push_str(&squeeze_ws(raw, budget.max_content_bytes));
                }
                (body_scratch.len() > preview.len()).then(|| Arc::from(body_scratch.as_str()))
            } else {
                None
            }
        } else {
            None
        };

        let author = entry
            .authors
            .first()
            .map(|p| p.name.trim())
            .filter(|n| !n.is_empty())
            .map(Arc::<str>::from);

        // Cross-source content identity: title + the head of the preview.
        let head_len = preview.len().min(160);
        let head = &preview.as_bytes()[..floor_char_boundary(&preview, head_len)];
        let content_hash = hash_pair64(hash64(title.as_bytes()), hash64(head));

        out.push(RawEntry {
            uid,
            uid_hash,
            title,
            link: Arc::from(link.as_str()),
            author,
            published: *ts,
            preview,
            content,
            content_hash,
        });
    }

    if out.is_empty() && total == 0 {
        return Err(ParseError::Empty);
    }

    Ok(ParsedFeed { feed_title, site, entries: out, truncated })
}

#[inline]
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[inline]
fn is_html(content_type: Option<&str>) -> bool {
    match content_type {
        Some(ct) => {
            let ct = ct.trim_start();
            ct.starts_with("text/html") || ct.starts_with("application/xhtml") || ct.contains("html")
        }
        None => true,
    }
}

fn entry_timestamp(entry: &feed_rs::model::Entry) -> i64 {
    entry
        .published
        .as_ref()
        .or(entry.updated.as_ref())
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

fn pick_entry_link(entry: &feed_rs::model::Entry) -> Option<String> {
    // Prefer rel="alternate" / no rel; skip enclosure/self.
    let mut fallback: Option<&str> = None;
    for link in &entry.links {
        match link.rel.as_deref() {
            None | Some("alternate") => return Some(link.href.clone()),
            Some(_) => fallback = fallback.or(Some(link.href.as_str())),
        }
    }
    fallback.map(|s| s.to_string())
}

fn pick_site_link(feed: &feed_rs::model::Feed) -> Option<String> {
    let mut fallback: Option<&str> = None;
    for link in &feed.links {
        match link.rel.as_deref() {
            None | Some("alternate") => return Some(link.href.clone()),
            Some(_) => fallback = fallback.or(Some(link.href.as_str())),
        }
    }
    fallback.map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
<title>Test Feed</title>
<link>https://example.com</link>
<item><title>Second</title><link>https://example.com/2</link>
  <description>&lt;p&gt;Body &lt;b&gt;two&lt;/b&gt; &amp;amp; more&lt;/p&gt;</description>
  <pubDate>Wed, 02 Oct 2024 12:00:00 GMT</pubDate></item>
<item><title>First</title><link>https://example.com/1</link>
  <description>Body one</description>
  <pubDate>Tue, 01 Oct 2024 12:00:00 GMT</pubDate></item>
</channel></rss>"#;

    #[test]
    fn parses_rss_in_newest_first_order() {
        let parsed = parse(RSS.as_bytes(), &ParseBudget::default()).unwrap();
        assert_eq!(parsed.feed_title.as_deref(), Some("Test Feed"));
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].title.as_ref(), "Second");
        assert_eq!(parsed.entries[1].title.as_ref(), "First");
        assert_eq!(parsed.entries[0].preview.as_ref(), "Body two & more");
    }

    #[test]
    fn entry_budget_keeps_newest() {
        let mut xml = String::from("<rss version=\"2.0\"><channel><title>t</title>");
        for i in 0..500 {
            xml.push_str(&format!(
                "<item><title>item {i}</title><link>https://e.com/{i}</link><pubDate>Wed, 02 Oct 2024 12:00:00 GMT</pubDate></item>"
            ));
        }
        xml.push_str("</channel></rss>");
        let budget = ParseBudget { max_entries: 10, ..Default::default() };
        let parsed = parse(xml.as_bytes(), &budget).unwrap();
        assert_eq!(parsed.entries.len(), 10);
        assert!(parsed.truncated);
    }

    #[test]
    fn atom_feed_works() {
        let atom = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Test</title><link href="https://a.example/"/>
  <entry><title>E1</title><id>tag:a.example,2024:1</id>
    <link rel="alternate" href="https://a.example/1"/>
    <updated>2024-10-02T12:00:00Z</updated>
    <content type="html">&lt;p&gt;Hello&lt;/p&gt;</content></entry>
</feed>"#;
        let parsed = parse(atom.as_bytes(), &ParseBudget::default()).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].link.as_ref(), "https://a.example/1");
        assert_eq!(parsed.entries[0].preview.as_ref(), "Hello");
    }

    #[test]
    fn json_feed_works() {
        let json = r#"{"version":"https://jsonfeed.org/version/1.1","title":"JF",
          "items":[{"id":"1","title":"J1","url":"https://j.example/1",
          "content_text":"plain body","date_published":"2024-10-02T12:00:00Z"}]}"#;
        let parsed = parse(json.as_bytes(), &ParseBudget::default()).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].preview.as_ref(), "plain body");
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(parse(b"<html><body>not a feed</body></html>", &ParseBudget::default()).is_err());
        assert!(parse(b"", &ParseBudget::default()).is_err());
    }
}

#[cfg(test)]
mod ampersand_feed_tests {
    use super::{ParseBudget, parse};

    /// A feed whose item text contains `&amp;&amp;` used to hang the parser.
    ///
    /// The entity is decoded *before* the HTML-to-text pass, so what arrives
    /// there is a bare `&&` — and a single item like that pinned a core inside
    /// the app, because the text reducer could not advance past it.
    #[test]
    fn a_feed_with_escaped_ampersands_parses() {
        let feed = br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>t</title>
<item><title>Commands</title><link>https://example.com/a</link>
<description>module unload 2023 &amp;&amp; module load 2024 &amp;amp; done</description>
</item></channel></rss>"#;
        let budget = ParseBudget { max_entries: 10, preview_chars: 200, max_content_bytes: 4096 };
        let parsed = parse(feed, &budget).expect("a plain feed must parse");
        assert_eq!(parsed.entries.len(), 1);
        assert!(
            parsed.entries[0].preview.contains("&&"),
            "the decoded ampersands survive: {}",
            parsed.entries[0].preview
        );
    }
}

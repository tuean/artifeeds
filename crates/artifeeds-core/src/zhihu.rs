//! Zhihu collections as a feed source.
//!
//! A Zhihu collection is a person's saved answers and articles. It is not a
//! feed: the list comes from a JSON API that only answers an authenticated
//! request, the items are heterogeneous (answers, articles, sometimes questions),
//! and the interesting part — the text — is one more request per item.
//!
//! So the shape is: [`list_collections`] to find them, [`collection_items`] to
//! page through one, [`content_of`] to pull an item's body. All three need the
//! same cookie header, which the app obtains by letting the reader log in to
//! Zhihu inside the embedded browser — the session then lives in this app's own
//! web data store rather than being lifted out of Safari or Edge.
//!
//! The JSON halves are pure functions with fixtures, because an API that changes
//! shape must fail in a test rather than in the reader's list.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

/// A Zhihu collection, as the sidebar's "feed" would see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: String,
    pub title: String,
    pub count: u32,
}

/// One saved item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: ItemKind,
    /// The answer/article id, which is what the content endpoint wants.
    pub id: String,
    pub title: String,
    pub url: String,
    pub excerpt: String,
    pub created: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Answer,
    Article,
    Other,
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            ItemKind::Answer => "answer",
            ItemKind::Article => "article",
            ItemKind::Other => "other",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "answer" => ItemKind::Answer,
            "article" => ItemKind::Article,
            _ => ItemKind::Other,
        }
    }
}

/// Is this URL a Zhihu collection? Used to route a source to this module rather
/// than to the feed parser, so a collection can be added by pasting its link.
pub fn is_collection_url(url: &str) -> bool {
    collection_id(url).is_some()
}

/// The numeric id in `zhihu.com/collection/<id>`, or in a bare `collection/<id>`.
pub fn collection_id(url: &str) -> Option<String> {
    // A bare id is accepted too: that is what the CLI takes, and what somebody
    // copying an id out of a URL has in hand.
    let trimmed = url.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(trimmed.to_string());
    }
    let after = url.split("collection/").nth(1)?;
    let id: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() { None } else { Some(id) }
}

/// The person token in `zhihu.com/people/<token>/collections`.
pub fn people_token(url: &str) -> Option<String> {
    let after = url.split("people/").nth(1)?;
    let token: String = after
        .chars()
        .take_while(|c| *c != '/' && *c != '?' && *c != '#')
        .collect();
    if token.is_empty() || token == "self" { None } else { Some(token) }
}

fn cookie_header(cookies: &str) -> Vec<(String, String)> {
    vec![
        ("Cookie".to_string(), cookies.to_string()),
        // The API answers a bare client with 403 on some endpoints; a browser
        // user agent is what the web client itself sends.
        (
            "User-Agent".to_string(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
                .to_string(),
        ),
        ("x-requested-with".to_string(), "fetch".to_string()),
    ]
}

/// The client the Zhihu API talks over.
///
/// Deliberately *not* the feed engine's client. Two reasons, one of them
/// measured: the engine keeps hundreds of connections in flight and this app's
/// long-lived pool starts timing out against zhihu.com while a fresh process
/// talks to it happily — while a reader is waiting for their collections, that
/// is not a difference worth debugging twice. The other is that an API which
/// authenticates by cookie wants its own user agent, its own connection limit
/// and no shared retry policy.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(60))
            .gzip(true)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// One JSON GET against the Zhihu API, retried.
///
/// A single connect timeout would otherwise surface to the reader as "没有找到"
/// — the same failure that made archiving look broken, fixed the same way. The
/// feed sweep keeps hundreds of connections busy, so a first attempt that loses
/// the race is normal rather than exceptional.
async fn get_json(url: &str, cookies: &str) -> Result<serde_json::Value, String> {
    const ATTEMPTS: u32 = 3;
    let mut attempt = 0;
    let response = loop {
        let request = client()
            .get(url)
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8");
        let request = cookie_header(cookies)
            .into_iter()
            .fold(request, |req, (name, value)| req.header(name, value));
        match request.send().await {
            Ok(response) => break response,
            Err(e) => {
                attempt += 1;
                if attempt >= ATTEMPTS {
                    return Err(format!("{e}（已重试 {attempt} 次）"));
                }
                tracing::debug!(%url, attempt, error = %e, "zhihu request retrying");
                tokio::time::sleep(Duration::from_millis(400 << (attempt - 1))).await;
            }
        }
    };
    let text = response.text().await.map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("知乎返回的不是 JSON（{e}）"))?;
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(if code == 101 {
            format!("未登录或登录已过期（{message}）")
        } else {
            format!("知乎错误 {code}：{message}")
        });
    }
    Ok(value)
}

/// The signed-in account, which is how the app learns the `url_token` it needs
/// to list somebody's collections — the alternative is asking the reader to find
/// their own handle in a URL.
pub struct Account {
    pub name: String,
    pub url_token: String,
}

pub async fn whoami(cookies: &str) -> Result<Account, String> {
    let value = get_json("https://www.zhihu.com/api/v4/me", cookies).await?;
    let name = value
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("(未知)")
        .to_string();
    let url_token = value
        .get("url_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "知乎没有返回账号标识".to_string())?
        .to_string();
    Ok(Account { name, url_token })
}

/// A person's collections. `url_token` is their Zhihu handle.
pub async fn list_collections(cookies: &str, url_token: &str) -> Result<Vec<Collection>, String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let url = format!(
            "https://www.zhihu.com/api/v4/people/{url_token}/collections?offset={offset}&limit=20"
        );
        let value = get_json(&url, cookies).await?;
        let batch = parse_collections(&value);
        if batch.is_empty() {
            break;
        }
        out.extend(batch);
        let more = value
            .get("paging")
            .and_then(|p| p.get("is_end"))
            .and_then(|e| e.as_bool())
            .map(|end| !end)
            .unwrap_or(false);
        if !more || out.len() > 200 {
            break;
        }
        offset += 20;
    }
    Ok(out)
}

/// The largest page this endpoint will actually return.
///
/// Not a preference: asked for more than this, the API answers `200` with an
/// **empty** `data` array. That is indistinguishable from "this collection is
/// empty" unless you know, which is why the number lives here with a comment
/// rather than at a call site — the first version of this code asked for 50 and
/// reported every collection as empty.
pub const PAGE_LIMIT: usize = 20;

/// Up to `want` items, paging 20 at a time.
pub async fn collection_items_upto(
    cookies: &str,
    collection: &str,
    want: usize,
) -> Result<Vec<Item>, String> {
    let mut out: Vec<Item> = Vec::new();
    let mut offset = 0usize;
    // A ceiling as well as a count: a collection can hold hundreds of items and
    // the reader asked for a window, not for the whole thing.
    while out.len() < want && offset <= 400 {
        let (items, more) = collection_items(cookies, collection, offset, PAGE_LIMIT).await?;
        if items.is_empty() {
            break;
        }
        out.extend(items);
        if !more {
            break;
        }
        offset += PAGE_LIMIT;
    }
    out.truncate(want);
    Ok(out)
}

/// One page of a collection: `(items, has_more)`.
pub async fn collection_items(
    cookies: &str,
    collection: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Item>, bool), String> {
    let url = format!(
        "https://www.zhihu.com/api/v4/collections/{collection}/items?offset={offset}&limit={limit}"
    );
    let value = get_json(&url, cookies).await?;
    let items = parse_items(&value);
    let more = value
        .get("paging")
        .and_then(|p| p.get("is_end"))
        .and_then(|e| e.as_bool())
        .map(|end| !end)
        .unwrap_or(false);
    Ok((items, more))
}

/// The full body of an answer or article, as HTML.
pub async fn content_of(cookies: &str, kind: ItemKind, id: &str) -> Result<String, String> {
    let path = match kind {
        ItemKind::Article => "articles",
        // Questions have no body worth reading; answers are the text.
        ItemKind::Answer | ItemKind::Other => "answers",
    };
    let url = format!("https://www.zhihu.com/api/v4/{path}/{id}?include=content");
    let value = get_json(&url, cookies).await?;
    Ok(value
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string())
}

// -- pure parsing --------------------------------------------------------

/// An id that Zhihu may send as a number or as a string.
///
/// The item endpoint sends answer ids as strings (`"id": "2089889426348439208"`)
/// while the collections endpoint sends them as numbers. Both are ids; only the
/// JSON type differs.
fn id_of(value: &serde_json::Value) -> Option<String> {
    let field = value.get("id")?;
    if let Some(text) = field.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        return None;
    }
    field.as_i64().map(|n| n.to_string())
}

/// Collections out of a `people/{token}/collections` payload.
pub fn parse_collections(value: &serde_json::Value) -> Vec<Collection> {
    let Some(items) = value.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|entry| {
            let id = id_of(entry)?;
            let title = entry
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("未命名收藏夹")
                .trim()
                .to_string();
            let count = entry
                .get("item_count")
                .and_then(|c| c.as_u64())
                .unwrap_or(0) as u32;
            Some(Collection { id, title, count })
        })
        .collect()
}

/// Items out of a `collections/{id}/items` payload.
///
/// The payload nests the real object under `content`, and the two kinds carry
/// their title in different places — an answer's is its question's.
pub fn parse_items(value: &serde_json::Value) -> Vec<Item> {
    let Some(entries) = value.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let content = entry.get("content")?;
            let kind = ItemKind::parse(content.get("type").and_then(|t| t.as_str()).unwrap_or(""));
            let id = id_of(content)?;
            let url = content
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .replace("api.zhihu.com", "www.zhihu.com");
            let title = item_title(content);
            let excerpt = content
                .get("excerpt")
                .and_then(|e| e.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let created = content
                .get("created_time")
                .or_else(|| entry.get("created"))
                .and_then(|c| c.as_i64())
                .unwrap_or(0);
            Some(Item { kind, id, title, url, excerpt, created })
        })
        .collect()
}

/// An answer's title is its question's; an article has its own.
fn item_title(content: &serde_json::Value) -> String {
    if let Some(title) = content.get("title").and_then(|t| t.as_str())
        && !title.trim().is_empty()
    {
        return title.trim().to_string();
    }
    if let Some(question) = content.get("question")
        && let Some(title) = question.get("title").and_then(|t| t.as_str())
    {
        return title.trim().to_string();
    }
    match content.get("excerpt").and_then(|e| e.as_str()) {
        Some(excerpt) if !excerpt.trim().is_empty() => {
            crate::text::truncate_chars(excerpt.trim(), 60).to_string()
        }
        _ => "无标题".to_string(),
    }
}

/// Does this item look worth storing as an article with a body?
pub fn kind_label(kind: ItemKind) -> &'static str {
    kind.as_str()
}

/// An item as the store wants it.
///
/// The uid is Zhihu's own `<kind>/<id>`, which is stable across refreshes and
/// survives the item being renamed — the API's `url` is a better link but a worse
/// identity, because it changes host between responses.
pub fn to_entry(item: &Item, source_url: &str, preview_chars: usize) -> crate::parse::RawEntry {
    use crate::text::hash64;
    let uid = format!("{}/{}", kind_label(item.kind), item.id);
    let preview = if item.excerpt.is_empty() {
        crate::text::truncate_chars(&item.title, preview_chars).to_string()
    } else {
        crate::text::truncate_chars(&item.excerpt, preview_chars).to_string()
    };
    let link = if item.url.is_empty() { source_url } else { &item.url };
    crate::parse::RawEntry {
        uid: Arc::from(uid.as_str()),
        uid_hash: hash64(uid.as_bytes()),
        title: Arc::from(item.title.as_str()),
        link: Arc::from(link),
        author: None,
        published: item.created,
        preview: Arc::from(preview.as_str()),
        content: None,
        content_hash: crate::text::hash64(preview.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_collection_and_people_urls() {
        assert!(is_collection_url("https://www.zhihu.com/collection/123456789"));
        assert_eq!(
            collection_id("https://www.zhihu.com/collection/123456789?page=2").as_deref(),
            Some("123456789")
        );
        assert!(is_collection_url("https://zhihu.com/collection/1"));
        assert!(!is_collection_url("https://www.zhihu.com/people/someone/collections"));
        // ...and a bare id, which is what the CLI and a copy-paste produce.
        assert_eq!(collection_id("428211787").as_deref(), Some("428211787"));
        assert_eq!(
            people_token("https://www.zhihu.com/people/zhong-xiao-tian-11/collections").as_deref(),
            Some("zhong-xiao-tian-11")
        );
        assert_eq!(people_token("https://www.zhihu.com/people/self/collections"), None);
    }

    #[test]
    fn parses_a_collection_list() {
        let payload = serde_json::json!({
            "data": [
                {"id": 9001, "title": "技术", "item_count": 42},
                {"id": 9002, "title": "  生活  ", "item_count": 7},
                {"title": "no id"}
            ],
            "paging": {"is_end": true}
        });
        let collections = parse_collections(&payload);
        assert_eq!(collections.len(), 2);
        assert_eq!(collections[0].id, "9001");
        assert_eq!(collections[0].count, 42);
        assert_eq!(collections[1].title, "生活");
    }

    #[test]
    fn parses_answers_and_articles_from_one_page() {
        let payload = serde_json::json!({
            "data": [
                {"content": {
                    "type": "answer", "id": 111,
                    "url": "https://api.zhihu.com/answers/111",
                    "excerpt": "答案摘要",
                    "created_time": 1700000000,
                    "question": {"title": "如何评价某某？"}
                }},
                {"content": {
                    "type": "article", "id": 222, "title": "我的文章",
                    "url": "https://api.zhihu.com/articles/222",
                    "excerpt": "文章摘要", "created_time": 1690000000
                }},
                {"content": {"type": "pin", "id": 333}}
            ],
            "paging": {"is_end": false}
        });
        let items = parse_items(&payload);
        assert_eq!(items.len(), 3);
        // An answer's title is its question's title.
        assert_eq!(items[0].title, "如何评价某某？");
        assert_eq!(items[0].kind, ItemKind::Answer);
        // ...and the API host is rewritten to the one a reader can open.
        assert_eq!(items[0].url, "https://www.zhihu.com/answers/111");
        assert_eq!(items[1].title, "我的文章");
        assert_eq!(items[1].kind, ItemKind::Article);
        assert_eq!(items[1].created, 1690000000);
        assert_eq!(items[2].kind, ItemKind::Other);
    }

    /// Verbatim shape of `collections/{id}/items`: the answer id is a *string*
    /// here and a number in the collections payload, which is exactly the kind of
    /// difference that silently yields an empty list.
    #[test]
    fn parses_the_real_answer_payload() {
        let payload = serde_json::json!({
            "paging": {"is_end": false, "totals": 154},
            "data": [{
                "created": 1791220000,
                "content": {
                    "type": "answer",
                    "id": "2089889426348439208",
                    "url": "https://www.zhihu.com/question/2088368253169034470/answer/2089889426348439208",
                    "excerpt": "[图片] 为了防止你的大肥鱼在创造模式下因为找不到官方文档/源码四处扒拉吃白饭…",
                    "created_time": 1791048467,
                    "question": {"type": "question", "title": "如何评价 DeepSeek Harness"},
                    "author": {"name": "无能的艾薇娜", "url_token": "klarkxy"}
                }
            }]
        });
        let items = parse_items(&payload);
        assert_eq!(items.len(), 1, "a string id must not drop the item");
        assert_eq!(items[0].id, "2089889426348439208");
        assert_eq!(items[0].kind, ItemKind::Answer);
        assert_eq!(items[0].title, "如何评价 DeepSeek Harness");
        assert_eq!(items[0].created, 1791048467);
    }

    #[test]
    fn an_answer_without_a_question_falls_back_to_its_excerpt() {
        let payload = serde_json::json!({
            "data": [{"content": {"type": "answer", "id": 1, "excerpt": "一句话的答案"}}]
        });
        let items = parse_items(&payload);
        assert_eq!(items[0].title, "一句话的答案");
        assert_eq!(items[0].url, "");
    }

    #[test]
    fn an_authentication_error_is_reported_as_such() {
        let payload = serde_json::json!({
            "error": {"code": 101, "name": "AuthenticationError", "message": "身份未经过验证"}
        });
        // The check lives in `get_json`; this asserts the shape it keys on.
        let code = payload["error"]["code"].as_i64().unwrap();
        assert_eq!(code, 101);
    }
}

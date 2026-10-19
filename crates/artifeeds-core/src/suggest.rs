//! Turning an arbitrary page into subscriptions, with the local `pi` agent as the
//! analyst and the feed parser as the judge.
//!
//! The shape of the problem: somebody has a link — a news site, a forum, a
//! directory of routes like the RSSHub list — and wants to know what to
//! subscribe to. Deciding that needs reading the page, which is what a model is
//! good at. Getting it *right* needs knowing that the answer is a real feed,
//! which is what the parser is good at.
//!
//! So this module does both and trusts neither alone:
//!
//! 1. [`gather`] fetches the page and extracts evidence — its declared feeds, its
//!    links with their labels, and its text. No model involved.
//! 2. [`build_prompt`] asks the agent for **JSON proposals only**, with the URLs
//!    restricted to what the evidence contains or directly implies.
//! 3. [`validate`] fetches every proposed URL and keeps the ones that are
//!    actually feeds. A model that invents a plausible-looking address produces a
//!    rejected row, not a broken subscription.
//!
//! Without an agent the same pipeline runs on the deterministic half: the page's
//! advertised feeds and the blogs its links resolve to.

use crate::discover::{self, Candidate, Feed};
use crate::http::HttpEngine;

/// What the page gave us to reason about.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub url: String,
    pub title: String,
    pub description: String,
    /// Feed URLs the page declares, absolute.
    pub advertised: Vec<String>,
    /// Outbound links that could be a feed or a blog, with their anchor text.
    pub links: Vec<Candidate>,
    /// The readable text of the page, truncated.
    pub text: String,
    /// The page *is* an OPML subscription list rather than a page linking one.
    pub is_opml: bool,
}

/// One thing worth subscribing to, as proposed by the agent.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub title: String,
    pub url: String,
    pub why: String,
    pub confidence: f32,
}

/// A proposal that survived validation.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub proposal: Proposal,
    /// Where the URL really points, which may differ from what was proposed.
    pub feed: Feed,
}

/// A proposal that did not, with the reason.
#[derive(Debug, Clone)]
pub struct Rejected {
    pub proposal: Proposal,
    pub reason: String,
}

const MAX_LINKS: usize = 60;
const MAX_TEXT_CHARS: usize = 4000;

/// Fetch a page and extract everything worth reasoning about.
pub async fn gather(http: &HttpEngine, url: &str, max_bytes: u64) -> Result<Evidence, String> {
    let response = http
        .fetch_resource_with(
            url,
            max_bytes,
            "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
            2,
            &[(
                "User-Agent".to_string(),
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
                    .to_string(),
            )],
        )
        .await
        .map_err(|e| e.to_string())?;
    let html = String::from_utf8_lossy(&response.body).to_string();
    let final_url = response.final_url.clone();

    let mut evidence = Evidence {
        url: final_url.clone(),
        title: page_title(&html).unwrap_or_default(),
        description: meta_description(&html).unwrap_or_default(),
        advertised: discover::advertised_feeds(&html)
            .into_iter()
            .filter_map(|href| discover::resolve(&final_url, &href))
            .collect(),
        links: discover::page_candidates(&html, &final_url, MAX_LINKS),
        text: crate::text::html_to_text(&html, MAX_TEXT_CHARS),
        is_opml: false,
    };
    // A page that is itself a feed needs no analysis.
    if discover::looks_like_feed(&html) {
        evidence.advertised.insert(0, final_url.clone());
    }
    // ...and a page that is itself a subscription list has already handed over
    // everything it has. The URL is the file; nothing needs analysing.
    let head = evidence.text.trim_start().to_lowercase();
    evidence.is_opml =
        head.contains("<opml") || html[..html.len().min(4096)].to_lowercase().contains("<opml");
    Ok(evidence)
}

/// The page's `<title>`, title-tag or og:title.
pub fn page_title(html: &str) -> Option<String> {
    for key in ["og:title", "twitter:title"] {
        if let Some(value) = meta_content(html, key) {
            return Some(value);
        }
    }
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let open = lower[start..].find('>')? + start + 1;
    let end = lower[open..].find("</title>")? + open;
    let title = crate::text::squeeze_ws(&crate::text::html_to_text(&html[open..end], 200), 200);
    (!title.trim().is_empty()).then(|| title.trim().to_string())
}

/// The page's `<meta name="description">`.
pub fn meta_description(html: &str) -> Option<String> {
    for key in ["og:description", "description", "twitter:description"] {
        if let Some(value) = meta_content(html, key) {
            return Some(value);
        }
    }
    None
}

fn meta_content(html: &str, key: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("<meta") {
        let start = cursor + rel;
        let end = lower[start..].find('>')? + start;
        let tag = &html[start..end];
        let name = attr(tag, "name").or_else(|| attr(tag, "property"));
        if name.is_some_and(|n| n.eq_ignore_ascii_case(key))
            && let Some(content) = attr(tag, "content")
        {
            let text = crate::text::squeeze_ws(&content, 300);
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
        cursor = end + 1;
    }
    None
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let at = lower.find(&format!("{name}="))?;
    let rest = &tag[at + name.len() + 1..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// The prompt handed to the agent.
///
/// Deliberately narrow: the model chooses *what is worth subscribing to*, and the
/// URL space it may choose from is bounded by evidence, because a plausible feed
/// URL is easy to invent and impossible to distinguish from a real one by reading
/// alone.
pub fn build_prompt(evidence: &Evidence, max: usize, hint: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "你是 RSS 订阅助手。用户在用桌面阅读器订阅内容，需要你判断这个网页里有哪些值得订阅的 feed。\n\n",
    );
    out.push_str(&format!("页面地址: {}\n", evidence.url));
    if !evidence.title.is_empty() {
        out.push_str(&format!("页面标题: {}\n", evidence.title));
    }
    if !evidence.description.is_empty() {
        out.push_str(&format!("页面描述: {}\n", evidence.description));
    }
    if !evidence.advertised.is_empty() {
        out.push_str(&format!(
            "页面声明的 feed（最可信，优先使用）: {}\n",
            evidence.advertised.join("  ")
        ));
    }
    if !hint.trim().is_empty() {
        out.push_str(&format!("用户额外要求: {}\n", hint.trim()));
    }
    if !evidence.links.is_empty() {
        out.push_str("\n页面上的候选链接（文字 → 地址）:\n");
        for link in evidence.links.iter().take(MAX_LINKS) {
            out.push_str(&format!("- {} → {}\n", link.label, link.url));
        }
    }
    out.push_str("\n页面正文（可能被截断）:\n");
    out.push_str(evidence.text.trim());
    out.push_str("\n\n规则:\n");
    out.push_str("1. url 必须是可直接订阅的 feed 地址（RSS / Atom / JSON Feed）。页面声明了 feed 就用它。\n");
    out.push_str("2. 若这是目录或清单类页面（例如 RSSHub 路由列表、友链集合、导航站），从中挑出最通用、最可能长期有效的若干条，给出具体订阅地址；RSSHub 的路由写成 https://rsshub.app/<route>。\n");
    out.push_str("3. 不要编造地址：只能用上面出现的链接、页面声明的 feed，或由它们明确推导出的地址。拿不准就不要写。\n");
    out.push_str("4. 中文页面用中文标题；标题是订阅显示名，简短清楚。\n");
    out.push_str("5. why 用一句中文说明为什么值得订阅。confidence 是 0 到 1 的小数。\n");
    out.push_str(&format!("6. 最多 {max} 条，按 confidence 从高到低。\n"));
    out.push_str("\n只输出 JSON，不要 Markdown 代码块，不要任何解释文字:\n");
    out.push_str(
        "{\"page_kind\":\"directory|news|forum|blog|other\",\"summary\":\"一句话\",\
\"subscriptions\":[{\"title\":\"\",\"url\":\"\",\"why\":\"\",\"confidence\":0.0}]}\n",
    );
    out
}

/// The agent's reading of the page, including the case where the honest answer
/// is "nothing here is subscribable".
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub page_kind: String,
    pub summary: String,
    pub proposals: Vec<Proposal>,
}

/// An OPML subscription list linked from a page, if there is one.
///
/// Some directories *are* a subscription list: the page is a shell and the
/// payload is an `.opml` next to it, which is the format this project already
/// reads. Importing that file beats inventing a feed per entry.
pub fn opml_links(evidence: &Evidence) -> Vec<String> {
    // The URL itself first: asking to subscribe to an OPML file should import
    // that file, not send an agent looking for feeds on it.
    let mut out: Vec<String> = if evidence.is_opml {
        vec![evidence.url.clone()]
    } else {
        Vec::new()
    };
    out.extend(evidence
        .advertised
        .iter()
        .chain(evidence.links.iter().map(|l| l.url.to_string()).collect::<Vec<_>>().iter())
        .filter(|url| {
            let lowered = url.to_lowercase();
            lowered.ends_with(".opml") || lowered.contains(".opml?") || lowered.ends_with("/opml")
        })
        .map(|url| url.to_string()));
    out.dedup();
    out
}

/// Read proposals out of the agent's answer.
///
/// Tolerant on purpose: a model that wraps its JSON in prose or a code fence is
/// still answering the question, and refusing the answer over formatting would
/// make the feature feel broken for no reason.
pub fn parse_analysis(answer: &str, max: usize) -> Result<Analysis, String> {
    let start = answer.find('{').ok_or_else(|| "回复里没有 JSON".to_string())?;
    let end = answer.rfind('}').ok_or_else(|| "回复里没有完整的 JSON".to_string())?;
    if end <= start {
        return Err("回复里的 JSON 不完整".to_string());
    }
    let slice = &answer[start..=end];
    let value: serde_json::Value =
        serde_json::from_str(slice).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let analysis = Analysis {
        page_kind: value
            .get("page_kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string(),
        summary: value
            .get("summary")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string(),
        proposals: Vec::new(),
    };
    let Some(items) = value.get("subscriptions").and_then(|s| s.as_array()) else {
        // No array at all is a malformed answer; an empty one is a finding.
        return Err("JSON 里没有 subscriptions 数组".to_string());
    };
    let mut analysis = analysis;
    let mut out = Vec::new();
    for item in items {
        let Some(url) = item.get("url").and_then(|u| u.as_str()).map(str::trim) else {
            continue;
        };
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        out.push(Proposal {
            title: item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            url: url.to_string(),
            why: item
                .get("why")
                .and_then(|w| w.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            confidence: item
                .get("confidence")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0) as f32,
        });
        if out.len() >= max {
            break;
        }
    }
    analysis.proposals = out;
    Ok(analysis)
}

/// Convenience for callers that only want the list.
pub fn parse_proposals(answer: &str, max: usize) -> Result<Vec<Proposal>, String> {
    parse_analysis(answer, max).map(|a| a.proposals)
}

/// Check every proposal against the feed parser.
///
/// This is the half that makes the feature trustworthy: what comes back is what a
/// reader can actually subscribe to, and everything else is reported with the
/// reason it failed rather than silently dropped.
pub async fn validate(
    http: &HttpEngine,
    proposals: Vec<Proposal>,
    max_bytes: u64,
) -> (Vec<Accepted>, Vec<Rejected>) {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for proposal in proposals {
        // A proposal may name a page rather than the feed itself; discovery is
        // what turns "this blog" into the address a reader subscribes to.
        match discover::discover(http, &proposal.url, max_bytes).await {
            Ok(feed) => {
                let mut proposal = proposal;
                if proposal.title.trim().is_empty() {
                    proposal.title = feed.title.to_string();
                }
                accepted.push(Accepted { proposal, feed });
            }
            Err(reason) => rejected.push(Rejected { proposal, reason }),
        }
    }
    (accepted, rejected)
}

/// Proposals that need no model: the page's own feeds, and the blogs its links
/// resolve to. Doubles as the fallback when the agent is unavailable.
pub fn deterministic_proposals(evidence: &Evidence, max: usize) -> Vec<Proposal> {
    let mut out = Vec::new();
    for feed in &evidence.advertised {
        out.push(Proposal {
            title: evidence.title.clone(),
            url: feed.clone(),
            why: "页面自己声明的 feed".to_string(),
            confidence: 0.95,
        });
        if out.len() >= max {
            return out;
        }
    }
    for link in &evidence.links {
        out.push(Proposal {
            title: link.label.to_string(),
            url: link.url.to_string(),
            why: "页面上的外链，验证后确认为 feed".to_string(),
            confidence: 0.5,
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

// -- the agent -----------------------------------------------------------

/// How to reach the local agent.
#[derive(Debug, Clone)]
pub struct Pi {
    pub bin: String,
    pub model: Option<String>,
    pub timeout: std::time::Duration,
    /// Let the agent fetch and search the web itself.
    ///
    /// Some pages — the ones a directory like the RSSHub list is built from —
    /// ship an empty shell and fill it in with JavaScript, so the static
    /// evidence below is a title and a hundred characters. The agent reading the
    /// page over the network is the difference between "nothing to subscribe to"
    /// and the actual list. Only the web tools are granted; the agent gets no
    /// filesystem and no shell, because all it is being asked for is a list.
    pub web: bool,
}

impl Default for Pi {
    fn default() -> Self {
        Self {
            // `ARTIFEEDS_PI` first: the binary is a global npm install, and a
            // launchd-started GUI app has a PATH of its own.
            bin: std::env::var("ARTIFEEDS_PI").unwrap_or_else(|_| "pi".to_string()),
            model: std::env::var("ARTIFEEDS_PI_MODEL").ok(),
            timeout: std::time::Duration::from_secs(240),
            web: true,
        }
    }
}

impl Pi {
    /// Is the agent there at all? Cheap enough to ask before promising anything.
    pub fn available(&self) -> bool {
        std::process::Command::new(&self.bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// One non-interactive question, with a hard deadline.
    ///
    /// Blocking, so callers run it off the reactor. The deadline is enforced by
    /// polling rather than by `wait_with_output`, because an agent that hangs
    /// would otherwise hang the command that asked it: stdout and stderr are
    /// drained on their own threads so a large answer cannot deadlock the pipe.
    pub fn ask_blocking(&self, prompt: &str) -> Result<String, String> {
        use std::io::{Read as _, Write as _};
        let mut command = std::process::Command::new(&self.bin);
        command.args(["-p", "--no-session"]);
        if self.web {
            command.args(["--tools", "fetch_content,web_search"]);
        } else {
            command.arg("--no-tools");
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(model) = &self.model {
            command.args(["--model", model]);
        }
        let mut child = command.spawn().map_err(|e| format!("无法启动 {}：{e}", self.bin))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "无法写入 pi 的标准输入".to_string())?;
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|e| format!("写入 pi 失败：{e}"))?;
            // Closing stdin is what tells the agent the prompt is complete.
        }
        let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = pipe.read_to_string(&mut text);
                text
            })
        };
        let stdout = drain(Box::new(child.stdout.take().expect("piped")));
        let stderr = drain(Box::new(child.stderr.take().expect("piped")));

        let deadline = std::time::Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("pi {} 秒未响应，已终止", self.timeout.as_secs()));
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        };
        let out = stdout.join().unwrap_or_default();
        let err = stderr.join().unwrap_or_default();
        if !status.success() {
            return Err(format!(
                "pi 退出码 {:?}：{}",
                status.code(),
                crate::text::truncate_chars(err.trim(), 200)
            ));
        }
        let text = out.trim().to_string();
        if text.is_empty() {
            return Err("pi 没有输出".to_string());
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sample_html() -> String {
        r#"<html><head>
            <title>Awesome RSSHub Routes</title>
            <meta name="description" content="A curated list of RSSHub routes">
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            </head><body>
            <h1>RSSHub 路由大全</h1>
            <p>这里收集了常用的路由。</p>
            <a href="https://rsshub.app/github/trending/daily/any">GitHub Trending</a>
            <a href="https://example.com/blog">某个博客</a>
            <a href="https://github.com/x/y">GitHub</a>
            </body></html>"#
            .to_string()
    }

    #[test]
    fn extracts_page_evidence() {
        let html = sample_html();
        assert_eq!(page_title(&html).as_deref(), Some("Awesome RSSHub Routes"));
        assert_eq!(
            meta_description(&html).as_deref(),
            Some("A curated list of RSSHub routes")
        );
        let advertised = discover::advertised_feeds(&html);
        assert_eq!(advertised, vec!["/feed.xml"]);
        // The resolver turns that into something a reader can fetch.
        assert_eq!(
            discover::resolve("https://jackyst0.github.io/awesome-rsshub-routes/", &advertised[0])
                .as_deref(),
            Some("https://jackyst0.github.io/feed.xml")
        );
        // Noise hosts stay out of the candidate list.
        let links = discover::friend_candidates(&html, "https://jackyst0.github.io/", 20);
        let urls: Vec<&str> = links.iter().map(|c| c.url.as_ref()).collect();
        assert_eq!(urls, vec!["https://rsshub.app/github/trending/daily/any", "https://example.com/blog"]);
    }

    #[test]
    fn the_prompt_carries_the_evidence_and_the_rules() {
        let evidence = Evidence {
            url: "https://jackyst0.github.io/awesome-rsshub-routes/".into(),
            title: "Awesome RSSHub Routes".into(),
            description: "A curated list".into(),
            advertised: vec!["https://jackyst0.github.io/feed.xml".into()],
            links: vec![Candidate {
                url: Arc::from("https://rsshub.app/github/trending/daily/any"),
                label: Arc::from("GitHub Trending"),
            }],
            text: "RSSHub 路由大全".into(),
            is_opml: false,
        };
        let prompt = build_prompt(&evidence, 5, "只要前端相关的");
        for needle in [
            "https://jackyst0.github.io/awesome-rsshub-routes/",
            "Awesome RSSHub Routes",
            "https://jackyst0.github.io/feed.xml",
            "GitHub Trending",
            "https://rsshub.app/github/trending/daily/any",
            "只要前端相关的",
            "最多 5 条",
            "不要编造地址",
        ] {
            assert!(prompt.contains(needle), "prompt is missing {needle}");
        }
    }

    #[test]
    fn reads_proposals_out_of_a_plain_answer() {
        let answer = r#"{"page_kind":"directory","summary":"路由清单",
            "subscriptions":[
              {"title":"GitHub 趋势","url":"https://rsshub.app/github/trending/daily/any","why":"每天更新","confidence":0.8},
              {"title":"","url":"https://example.com/feed","why":"","confidence":2}]}"#;
        let proposals = parse_proposals(answer, 10).unwrap();
        assert_eq!(proposals.len(), 2);
        assert_eq!(proposals[0].title, "GitHub 趋势");
        assert!((proposals[0].confidence - 0.8).abs() < 0.001);
        // Out-of-range confidence is clamped rather than trusted.
        assert!((proposals[1].confidence - 1.0).abs() < 0.001);
    }

    #[test]
    fn tolerates_code_fences_and_surrounding_prose() {
        let answer = "好的，分析如下：\n```json\n{\"subscriptions\":[{\"title\":\"A\",\"url\":\"https://a.example/feed\",\"confidence\":0.5}]}\n```\n希望有帮助。";
        let proposals = parse_proposals(answer, 10).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].url, "https://a.example/feed");
    }

    #[test]
    fn an_empty_list_is_an_answer_not_a_failure() {
        let answer = r#"{"page_kind":"directory","summary":"目录页，没有可直接订阅的 feed",
            "subscriptions":[]}"#;
        let analysis = parse_analysis(answer, 5).unwrap();
        assert!(analysis.proposals.is_empty());
        assert_eq!(analysis.page_kind, "directory");
        assert!(analysis.summary.contains("没有可直接订阅"));
        // ...but a missing array is still malformed.
        assert!(parse_analysis(r#"{"page_kind":"x"}"#, 5).is_err());
    }

    #[test]
    fn an_opml_url_is_its_own_import() {
        let evidence = Evidence {
            url: "https://raw.githubusercontent.com/x/y/master/feedlist.opml".into(),
            is_opml: true,
            ..Default::default()
        };
        assert_eq!(
            opml_links(&evidence),
            vec!["https://raw.githubusercontent.com/x/y/master/feedlist.opml"]
        );
        // A page that merely *links* an opml still works the other way round.
        let page = Evidence {
            url: "https://example.com/".into(),
            is_opml: false,
            links: vec![Candidate {
                url: Arc::from("https://example.com/feeds.opml"),
                label: Arc::from("OPML"),
            }],
            ..Default::default()
        };
        assert_eq!(opml_links(&page), vec!["https://example.com/feeds.opml"]);
    }

    #[test]
    fn finds_an_opml_link_on_the_page() {
        let evidence = Evidence {
            url: "https://jackyst0.github.io/awesome-rsshub-routes/".into(),
            links: vec![
                Candidate {
                    url: Arc::from("https://jackyst0.github.io/awesome-rsshub-routes/feeds.opml"),
                    label: Arc::from("下载 OPML"),
                },
                Candidate {
                    url: Arc::from("https://example.com/blog"),
                    label: Arc::from("博客"),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            opml_links(&evidence),
            vec!["https://jackyst0.github.io/awesome-rsshub-routes/feeds.opml"]
        );
    }

    #[test]
    fn refuses_answers_that_are_not_usable() {
        assert!(parse_proposals("我不知道", 5).is_err());
        assert!(parse_proposals("{}", 5).is_err());
        // Entries without a usable URL are dropped, and an empty result is an error.
        // Entries without a usable URL are dropped; an empty result is fine now.
        assert!(parse_proposals(r#"{"subscriptions":[{"title":"x","url":""}]}"#, 5)
            .unwrap()
            .is_empty());
        assert!(parse_proposals(r#"{"subscriptions":[{"title":"x","url":"ftp://x"}]}"#, 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn honours_the_cap() {
        let answer = r#"{"subscriptions":[
            {"url":"https://a.example/1"},{"url":"https://a.example/2"},{"url":"https://a.example/3"}]}"#;
        assert_eq!(parse_proposals(answer, 2).unwrap().len(), 2);
    }

    #[test]
    fn deterministic_proposals_lead_with_the_declared_feed() {
        let evidence = Evidence {
            url: "https://x.example/".into(),
            title: "X".into(),
            advertised: vec!["https://x.example/feed".into()],
            links: vec![Candidate {
                url: Arc::from("https://y.example/"),
                label: Arc::from("Y"),
            }],
            ..Default::default()
        };
        let proposals = deterministic_proposals(&evidence, 5);
        assert_eq!(proposals[0].url, "https://x.example/feed");
        assert_eq!(proposals[1].url, "https://y.example/");
    }
}

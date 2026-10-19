//! HTTP layer: one pooled client for the whole process, conditional requests,
//! and a two-level concurrency budget (global × per-host).
//!
//! Cost model this is built around: for a feed reader the expensive resources
//! are, in order, (1) bytes transferred, (2) TLS handshakes, (3) TCP
//! connections, (4) CPU. Everything here attacks (1)–(3):
//!
//! * **One client, pooled.** Connection reuse and HTTP/2 multiplexing mean a
//!   refresh of 2 000 feeds opens only a handful of sockets per host.
//! * **Conditional GET.** Every source persists its `ETag` /
//!   `Last-Modified`; an unchanged feed costs a 304 with no body. This is the
//!   single biggest bandwidth win and it is on by default.
//! * **Two-level admission control.** A global semaphore bounds total
//!   in-flight requests; a per-host semaphore plus a minimum inter-request gap
//!   stops one heavily-subscribed origin from being hammered.
//! * **Bounded bodies.** The body is streamed and abandoned the moment it
//!   exceeds `max_body_bytes`, so a hostile or broken endpoint cannot make the
//!   process allocate unbounded memory.
//! * **Retries with jitter.** Transient failures are retried a bounded number
//!   of times; `Retry-After` is surfaced to the scheduler, which turns it into
//!   a real backoff instead of a retry storm.

use crate::config::Settings;
use crate::metrics::Metrics;
use crate::model::SourceId;
use futures::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED,
    RETRY_AFTER, USER_AGENT,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const ACCEPT_FEED: &str = "application/atom+xml, application/rss+xml, application/feed+json, application/json, application/xml;q=0.9, text/xml;q=0.9, text/html;q=0.8, */*;q=0.5";
const MAX_RETRIES: u32 = 2;

/// Everything needed to fetch one feed. Borrowed-free: all fields are cheap
/// `Arc` clones so the struct can be moved into a spawned task.
#[derive(Debug, Clone)]
pub struct FetchSpec {
    pub source: SourceId,
    pub url: Arc<str>,
    pub host: Arc<str>,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    pub headers: Arc<[(Arc<str>, Arc<str>)]>,
    pub user_agent: Option<Arc<str>>,
    pub max_body_bytes: u64,
}

/// A raw sub-resource (stylesheet, image, font) or an HTML document fetched
/// for the archiver. No validators, no feed semantics — just bytes, the final
/// URL after redirects, and the declared type.
#[derive(Debug)]
pub struct ResourceResponse {
    pub final_url: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct FetchResponse {
    pub not_modified: bool,
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    pub bytes: u64,
    pub attempts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("http {status} {}", message.as_deref().unwrap_or(""))]
    Status {
        status: u16,
        message: Option<String>,
        retry_after: Option<u64>,
    },
    #[error("transport: {0}")]
    Transport(String),
    #[error("timed out")]
    Timeout,
    /// Never established a connection inside `connect_timeout_secs`. Split out
    /// from `Timeout` because the two mean completely different things: this is
    /// the network or the host refusing us, that one is a slow response.
    #[error("connect timed out")]
    ConnectTimeout,
    #[error("body larger than {} bytes", limit)]
    TooLarge { limit: u64 },
    #[error("invalid url: {0}")]
    Url(String),
}

impl FetchError {
    /// `Some(secs)` when the server explicitly asked us to wait, or when the
    /// error class justifies a scheduler-level backoff.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            FetchError::Status { status, retry_after, .. } => match retry_after {
                Some(secs) => Some(*secs),
                None if *status == 429 || *status >= 500 => Some(60),
                None => None,
            },
            _ => None,
        }
    }

    pub fn is_transient(&self) -> bool {
        match self {
            // A refused connection is not usually fixed by asking again a
            // quarter of a second later, and retrying it three times multiplies
            // the load of an unreachable host by three.
            FetchError::ConnectTimeout => false,
            FetchError::Transport(_) | FetchError::Timeout => true,
            FetchError::Status { status, .. } => *status == 429 || *status >= 500 || *status == 408,
            _ => false,
        }
    }
}

pub struct HttpEngine {
    client: reqwest::Client,
    global: Arc<Semaphore>,
    /// Separate, smaller budget for page sub-resources so archiving a page
    /// cannot starve feed refreshes.
    resources: Arc<Semaphore>,
    per_host: Mutex<FxHashMap<Arc<str>, Arc<Semaphore>>>,
    /// Earliest instant at which the next request to a host may start.
    host_gate: Mutex<FxHashMap<Arc<str>, Instant>>,
    host_gap: Duration,
    default_ua: Arc<str>,
    accept_language: Arc<str>,
    global_headers: Arc<[(Arc<str>, Arc<str>)]>,
    max_body_bytes: u64,
    metrics: Arc<Metrics>,
}

impl HttpEngine {
    pub fn new(settings: &Settings, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(settings.connect_timeout_secs.max(1)))
            .timeout(Duration::from_secs(settings.request_timeout_secs.max(2)))
            .user_agent(settings.user_agent.clone())
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .http2_adaptive_window(true)
            .pool_idle_timeout(Duration::from_secs(90))
            // Keep a small number of warm sockets per origin: enough to overlap
            // DNS/TLS with body reads, not enough to look like an attack.
            .pool_max_idle_per_host(settings.per_host_concurrency.max(1))
            .redirect(reqwest::redirect::Policy::limited(5))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true);
        if let Some(proxy) = settings.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy.trim())?);
        }
        let client = builder.build()?;

        let global_headers: Arc<[(Arc<str>, Arc<str>)]> = settings
            .headers
            .iter()
            .map(|(k, v)| (Arc::<str>::from(k.as_str()), Arc::<str>::from(v.as_str())))
            .collect::<Vec<_>>()
            .into();

        Ok(Self {
            client,
            global: Arc::new(Semaphore::new(settings.global_concurrency.max(1))),
            resources: Arc::new(Semaphore::new(6)),
            per_host: Mutex::new(FxHashMap::default()),
            host_gate: Mutex::new(FxHashMap::default()),
            host_gap: Duration::from_millis(settings.host_min_gap_ms),
            default_ua: Arc::from(settings.user_agent.as_str()),
            accept_language: Arc::from(settings.accept_language.as_str()),
            global_headers,
            max_body_bytes: settings.max_body_bytes,
            metrics,
        })
    }

    /// Fetch one sub-resource (stylesheet, image, font) or a whole HTML page.
    ///
    /// Deliberately independent of [`HttpEngine::fetch`]: no conditional
    /// requests, no feed accept header, its own concurrency budget, and a body
    /// cap enforced while streaming. Errors are values — the archiver skips a
    /// resource that fails rather than failing the page.
    pub async fn fetch_resource(
        &self,
        url: &str,
        max_bytes: u64,
        accept: &str,
        per_host: usize,
    ) -> Result<ResourceResponse, FetchError> {
        self.fetch_resource_with(url, max_bytes, accept, per_host, &[]).await
    }

    /// `fetch_resource` with extra request headers.
    ///
    /// Needed for APIs that authenticate by cookie rather than by URL: the
    /// Zhihu collection endpoints reject an anonymous request outright, and
    /// there is no query-parameter equivalent.
    pub async fn fetch_resource_with(
        &self,
        url: &str,
        max_bytes: u64,
        accept: &str,
        per_host: usize,
        headers: &[(String, String)],
    ) -> Result<ResourceResponse, FetchError> {
        let parsed = reqwest::Url::parse(url).map_err(|e| FetchError::Url(e.to_string()))?;
        let host: Arc<str> = Arc::from(parsed.host_str().unwrap_or("").to_ascii_lowercase().as_str());

        let Ok(_budget) = self.resources.clone().acquire_owned().await else {
            return Err(FetchError::Transport("resource limiter closed".into()));
        };
        let host_sem = self.host_semaphore(&host, per_host);
        let Ok(_permit) = host_sem.acquire_owned().await else {
            return Err(FetchError::Transport("host limiter closed".into()));
        };
        let now = Instant::now();
        let start_at = self.reserve_host_slot(&host, now);
        if start_at > now {
            tokio::time::sleep(start_at - now).await;
        }

        self.metrics.record_request();
        let started = Instant::now();
        let mut request = self
            .client
            .get(parsed)
            .header(ACCEPT, accept)
            .header(USER_AGENT, self.default_ua.as_ref())
            .header(ACCEPT_LANGUAGE, self.accept_language.as_ref());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(classify_transport)?;
        self.metrics.record_latency(started.elapsed().as_millis().min(u32::MAX as u128) as u32);

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            return Err(FetchError::Status { status: status.as_u16(), message: None, retry_after });
        }
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        if let Some(len) = response.content_length()
            && len > max_bytes
        {
            return Err(FetchError::TooLarge { limit: max_bytes });
        }

        let mut body: Vec<u8> = Vec::with_capacity(response.content_length().unwrap_or(8192).min(max_bytes) as usize);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(classify_transport)?;
            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(FetchError::TooLarge { limit: max_bytes });
            }
            body.extend_from_slice(&chunk);
        }
        let bytes = body.len() as u64;
        self.metrics.record_200(bytes);
        Ok(ResourceResponse { final_url, content_type, body, bytes })
    }

    fn host_semaphore(&self, host: &Arc<str>, limit: usize) -> Arc<Semaphore> {
        let mut map = self.per_host.lock();
        map.entry(host.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(limit.max(1))))
            .clone()
    }

    /// Reserve a start slot for `host`, honouring the minimum gap. Returns the
    /// instant the caller may begin.
    fn reserve_host_slot(&self, host: &Arc<str>, now: Instant) -> Instant {
        let mut gate = self.host_gate.lock();
        if gate.len() > 4_096 {
            // Extremely large fleets: drop stale entries rather than grow.
            gate.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
        }
        let entry = gate.entry(host.clone()).or_insert(now);
        let start = (*entry).max(now);
        *entry = start + self.host_gap;
        start
    }

    /// Fetch a feed. Retries transient failures internally; everything the
    /// scheduler needs to back off is carried in [`FetchError`].
    pub async fn fetch(
        &self,
        spec: &FetchSpec,
        per_host_limit: usize,
    ) -> Result<FetchResponse, FetchError> {
        let url = reqwest::Url::parse(&spec.url).map_err(|e| FetchError::Url(e.to_string()))?;
        let mut last_err: Option<FetchError> = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let backoff = Duration::from_millis(250 * (1 << (attempt - 1)))
                    + Duration::from_millis(jitter_ms(spec.source, attempt));
                tokio::time::sleep(backoff).await;
            }
            // Admission control, in the order "host first, then global", so a
            // slow host cannot hoard the global budget while waiting.
            let host_sem = self.host_semaphore(&spec.host, per_host_limit);
            let Ok(host_permit) = host_sem.acquire_owned().await else {
                return Err(FetchError::Transport("host limiter closed".into()));
            };
            let Ok(global_permit) = self.global.clone().acquire_owned().await else {
                return Err(FetchError::Transport("global limiter closed".into()));
            };

            let now = Instant::now();
            let start_at = self.reserve_host_slot(&spec.host, now);
            if start_at > now {
                tokio::time::sleep(start_at - now).await;
            }

            self.metrics.record_request();
            let started = Instant::now();
            let result = self.attempt(&url, spec).await;
            let elapsed_ms = started.elapsed().as_millis() as u32;
            self.metrics.record_latency(elapsed_ms);

            drop(global_permit);
            drop(host_permit);

            match result {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    let transient = err.is_transient();
                    let retry_after = err.retry_after();
                    last_err = Some(err);
                    if !transient || retry_after.is_some_and(|s| s > 5) {
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| FetchError::Transport("no attempt made".into())))
    }

    async fn attempt(
        &self,
        url: &reqwest::Url,
        spec: &FetchSpec,
    ) -> Result<FetchResponse, FetchError> {
        let mut req = self
            .client
            .get(url.clone())
            .header(ACCEPT, ACCEPT_FEED)
            .header(ACCEPT_LANGUAGE, self.accept_language.as_ref())
            .header(USER_AGENT, spec.user_agent.as_deref().unwrap_or(self.default_ua.as_ref()))
            .header("Cache-Control", "max-age=0");

        // Conditional request: this is what makes a quiet feed cost ~200 bytes.
        if let Some(etag) = &spec.etag {
            req = req.header(IF_NONE_MATCH, etag.as_ref());
        }
        if let Some(lm) = &spec.last_modified {
            req = req.header(IF_MODIFIED_SINCE, lm.as_ref());
        }
        for (k, v) in self.global_headers.iter() {
            if !matches!(k.as_ref(), "user-agent" | "accept" | "accept-language" | "accept-encoding") {
                req = req.header(k.as_ref(), v.as_ref());
            }
        }
        for (k, v) in spec.headers.iter() {
            req = req.header(k.as_ref(), v.as_ref());
        }

        let resp = req.send().await.map_err(classify_transport)?;
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(FetchResponse {
                not_modified: true,
                status: 304,
                body: Vec::new(),
                etag: spec.etag.clone(),
                last_modified: spec.last_modified.clone(),
                bytes: 0,
                attempts: 1,
            });
        }

        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            let message = if status.is_redirection() {
                Some("redirect not followed".to_string())
            } else {
                // Error bodies are usually an HTML error page; a 140-character
                // plain-text digest is far more useful in a status bar than
                // 160 characters of markup.
                resp.text().await.ok().map(|t| clean_error_body(&t))
            };
            return Err(FetchError::Status { status: status.as_u16(), message, retry_after });
        }

        let cap = spec.max_body_bytes.min(self.max_body_bytes);
        if let Some(len) = resp.content_length()
            && len > cap
        {
            return Err(FetchError::TooLarge { limit: cap });
        }

        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| Arc::<str>::from(s.trim()));
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| Arc::<str>::from(s.trim()));

        let mut body: Vec<u8> = Vec::with_capacity(resp.content_length().unwrap_or(16 * 1024).min(cap) as usize);
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(classify_transport)?;
            if body.len() as u64 + chunk.len() as u64 > cap {
                return Err(FetchError::TooLarge { limit: cap });
            }
            body.extend_from_slice(&chunk);
        }
        let bytes = body.len() as u64;
        Ok(FetchResponse {
            not_modified: false,
            status: status.as_u16(),
            body,
            etag,
            last_modified,
            bytes,
            attempts: 1,
        })
    }

    pub fn available_global_permits(&self) -> usize {
        self.global.available_permits()
    }
}

fn classify_transport(err: reqwest::Error) -> FetchError {
    // A one-line diagnosis of *where* a transport failure happened. The
    // user-facing message stays short; this is what makes "timed out" debuggable.
    if tracing::enabled!(tracing::Level::DEBUG) {
        let mut chain = String::new();
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&err);
        while let Some(e) = src {
            if !chain.is_empty() {
                chain.push_str(" <- ");
            }
            chain.push_str(&e.to_string());
            src = e.source();
        }
        tracing::debug!(
            url = %err.url().map(|u| u.to_string()).unwrap_or_default(),
            host = %err.url().and_then(|u| u.host_str().map(|h| h.to_string())).unwrap_or_default(),
            connect = err.is_connect(),
            timeout = err.is_timeout(),
            request = err.is_request(),
            body = err.is_body(),
            decode = err.is_decode(),
            error = %err,
            caused_by = %chain,
            "transport failure"
        );
    }
    if err.is_timeout() {
        if err.is_connect() {
            FetchError::ConnectTimeout
        } else {
            FetchError::Timeout
        }
    } else {
        FetchError::Transport(err.to_string())
    }
}

/// Turn an error response body into a short, human-readable line.
fn clean_error_body(body: &str) -> String {
    let head = crate::text::truncate_chars(body, 4096);
    let text = if head.trim_start().starts_with('<') {
        crate::text::html_to_text(head, 140)
    } else {
        crate::text::squeeze_ws(head, 140)
    };
    if text.is_empty() {
        "(empty body)".to_string()
    } else {
        text
    }
}

#[allow(dead_code)]
fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let cut = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// `Retry-After` is either delta-seconds or an HTTP date; support both.
fn parse_retry_after(value: &str) -> Option<u64> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs.min(86_400));
    }
    // Very small HTTP-date parser: only the IMF-fixdate form is required.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    let target = parse_http_date(v)?;
    Some((target - now).clamp(0, 86_400) as u64)
}

fn parse_http_date(v: &str) -> Option<i64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = v.split([' ', ',']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let mut hms = parts[4].split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let s: i64 = hms.next().unwrap_or("0").parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + s)
}

/// Howard Hinnant's days-from-civil, no dependency required.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Deterministic per-source jitter, so retries do not synchronise fleet-wide.
fn jitter_ms(source: SourceId, attempt: u32) -> u64 {
    let h = crate::text::hash_pair64(source as u64, attempt as u64);
    h % 250
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after("999999"), Some(86_400));
    }

    #[test]
    fn parses_http_date() {
        // 1994-11-06T08:49:37Z
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
    }

    #[test]
    fn too_large_is_not_transient() {
        let e = FetchError::TooLarge { limit: 10 };
        assert!(!e.is_transient());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn server_errors_are_transient() {
        let e = FetchError::Status { status: 503, message: None, retry_after: None };
        assert!(e.is_transient());
        assert!(e.retry_after().is_some());
    }
}

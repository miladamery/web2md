//! HTTP fetching: per-host politeness, retries, and content-type gating.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use url::Url;

use crate::config::Settings;
use crate::error::{Error, Result};

/// A successfully fetched document.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The URL after redirects. Links are resolved against this, not the
    /// requested URL, or a redirecting page yields wrong relative links.
    pub final_url: Url,
    pub status: u16,
    pub body: String,
}

pub struct Fetcher {
    client: reqwest::Client,
    delay: Duration,
    max_attempts: u32,
    /// One gate per host. The outer map is held only long enough to hand out
    /// the inner lock, so different hosts never block each other.
    gates: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
}

impl Fetcher {
    pub fn new(settings: &Settings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(&settings.user_agent)
            .timeout(Duration::from_secs(settings.timeout_secs))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(Error::Http)?;

        Ok(Self {
            client,
            delay: Duration::from_millis(settings.delay_ms),
            max_attempts: 3,
            gates: Mutex::new(HashMap::new()),
        })
    }

    /// Wait until this host is due another request. Serializing here is what
    /// makes `--delay` a real floor rather than an average: the timestamp is
    /// only advanced once a request is actually about to go out.
    async fn wait_for_turn(&self, host: &str) {
        if self.delay.is_zero() {
            return;
        }
        let cell = {
            let mut map = self.gates.lock().await;
            map.entry(host.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let mut last = cell.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.delay {
                tokio::time::sleep(self.delay - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// Fetch raw bytes, with retries. Used for robots.txt and sitemaps.
    pub async fn get_bytes(&self, url: &Url) -> Result<(Url, u16, Vec<u8>, Option<String>)> {
        let r = self.request(url, &[]).await?;
        Ok((r.final_url, r.status, r.body, r.content_type))
    }

    /// Fetch a page as text. Returns `None` for content that isn't HTML, so
    /// the caller can skip a PDF or an image without treating it as an error.
    pub async fn get_html(&self, url: &Url) -> Result<Option<Fetched>> {
        Ok(match self.get_html_conditional(url, None, None).await? {
            Conditional::Modified { fetched, .. } => Some(fetched),
            _ => None,
        })
    }

    /// Fetch a page, asking the server to skip the body when nothing changed.
    ///
    /// This is what makes re-running against a large site cheap: a `304` costs
    /// a round trip and nothing else, where an unconditional GET costs the
    /// whole page.
    pub async fn get_html_conditional(
        &self,
        url: &Url,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<Conditional> {
        let mut headers: Vec<(&str, String)> = Vec::new();
        if let Some(e) = etag {
            headers.push(("If-None-Match", e.to_string()));
        }
        if let Some(lm) = last_modified {
            headers.push(("If-Modified-Since", lm.to_string()));
        }

        let r = self.request(url, &headers).await?;

        if r.status == 304 {
            return Ok(Conditional::NotModified);
        }
        if !looks_like_html(r.content_type.as_deref(), &r.body) {
            return Ok(Conditional::NotHtml);
        }

        Ok(Conditional::Modified {
            fetched: Fetched {
                final_url: r.final_url,
                status: r.status,
                body: decode(r.body),
            },
            etag: r.etag,
            last_modified: r.last_modified,
        })
    }

    /// One request, retried on transient failures. `304 Not Modified` is a
    /// success here — it is the whole point of a conditional GET — so it is
    /// returned rather than treated as an error.
    async fn request(&self, url: &Url, extra_headers: &[(&str, String)]) -> Result<RawResponse> {
        let host = url.host_str().unwrap_or_default().to_string();
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            self.wait_for_turn(&host).await;

            let mut req = self.client.get(url.as_str());
            for (name, value) in extra_headers {
                req = req.header(*name, value);
            }

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < self.max_attempts && is_retryable_error(&e) {
                        tokio::time::sleep(backoff(attempt, None)).await;
                        continue;
                    }
                    return Err(Error::Http(e));
                }
            };

            let status = response.status();
            if status.is_success() || status == reqwest::StatusCode::NOT_MODIFIED {
                let final_url = response.url().clone();
                let header = |name: reqwest::header::HeaderName| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                let content_type = header(reqwest::header::CONTENT_TYPE);
                let etag = header(reqwest::header::ETAG);
                let last_modified = header(reqwest::header::LAST_MODIFIED);
                let body = response.bytes().await.map_err(Error::Http)?.to_vec();

                return Ok(RawResponse {
                    final_url,
                    status: status.as_u16(),
                    body,
                    content_type,
                    etag,
                    last_modified,
                });
            }

            if attempt < self.max_attempts && is_retryable_status(status) {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }

            return Err(Error::Other(format!("HTTP {} for {url}", status.as_u16())));
        }
    }
}

/// A raw response, before any HTML interpretation.
#[derive(Debug, Clone)]
struct RawResponse {
    final_url: Url,
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Outcome of a conditional fetch.
#[derive(Debug, Clone)]
pub enum Conditional {
    /// The server confirmed our cached copy is current; no body was sent.
    NotModified,
    /// Reached, but not an HTML document.
    NotHtml,
    Modified {
        fetched: Fetched,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// Retry only on the failures that are plausibly transient. A 404 will still
/// be a 404 in two seconds; retrying it just wastes the host's patience.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn is_retryable_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
}

/// Exponential backoff, capped, with an honoured `Retry-After` when it is
/// short enough to be worth waiting for.
fn backoff(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    if let Some(secs) = retry_after_secs {
        if secs <= 30 {
            return Duration::from_secs(secs);
        }
    }
    let ms = 400u64.saturating_mul(1 << attempt.min(4));
    Duration::from_millis(ms.min(8_000))
}

fn looks_like_html(content_type: Option<&str>, body: &[u8]) -> bool {
    if let Some(ct) = content_type {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("html") || ct.contains("xml") {
            return true;
        }
        // Some doc servers serve pages as text/plain. Only trust a sniffer
        // when the type is one of the generic text types.
        if ct.starts_with("text/") || ct.contains("octet-stream") {
            return sniff_html(body);
        }
        return false;
    }
    // No Content-Type at all: fall back to sniffing.
    sniff_html(body)
}

fn sniff_html(body: &[u8]) -> bool {
    let head = &body[..body.len().min(1024)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.contains("<!doctype html") || text.contains("<html") || text.contains("<body")
}

/// reqwest's `charset` feature already decodes most non-UTF-8 pages. This is
/// the last resort for bodies that arrived without a usable charset.
fn decode(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1, None), Duration::from_millis(800));
        assert_eq!(backoff(2, None), Duration::from_millis(1600));
        assert!(backoff(9, None) <= Duration::from_millis(8_000));
    }

    #[test]
    fn retry_after_wins_when_short() {
        assert_eq!(backoff(1, Some(3)), Duration::from_secs(3));
        // An absurd Retry-After is ignored in favour of our own cap.
        assert_eq!(backoff(1, Some(600)), Duration::from_millis(800));
    }

    #[test]
    fn content_type_gating() {
        assert!(looks_like_html(Some("text/html; charset=utf-8"), b""));
        assert!(looks_like_html(Some("application/xhtml+xml"), b""));
        assert!(!looks_like_html(Some("image/png"), b""));
        assert!(!looks_like_html(Some("application/pdf"), b""));
        // Generic text types fall through to sniffing.
        assert!(looks_like_html(Some("text/plain"), b"<!DOCTYPE html><html>"));
        assert!(!looks_like_html(Some("text/plain"), b"just a note"));
        // Missing type entirely: sniff.
        assert!(looks_like_html(None, b"<html><body>hi"));
    }

    #[test]
    fn retryable_statuses_are_the_transient_ones() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    }
}

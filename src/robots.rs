//! robots.txt, fetched once per host and cached.
//!
//! Failure to fetch is treated as "allowed". A documentation site that has no
//! robots.txt, or that 500s on it, has not actually forbidden anything — and
//! refusing to crawl in that case would make the tool useless against exactly
//! the sites it targets.

use std::collections::HashMap;
use std::sync::Arc;

use texting_robots::Robot;
use tokio::sync::Mutex;
use url::Url;

use crate::fetch::Fetcher;

pub struct Robots {
    fetcher: Arc<Fetcher>,
    agent: String,
    /// `None` means "no usable robots.txt for this host".
    cache: Mutex<HashMap<String, Option<Arc<Robot>>>>,
}

/// robots.txt matches on the product token, not the full User-Agent string.
/// `"web2md/0.1 (+https://example.com/bot)"` -> `"web2md"`.
fn agent_token(user_agent: &str) -> String {
    user_agent
        .split(['/', ' ', '(', ';'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("web2md")
        .to_string()
}

impl Robots {
    pub fn new(fetcher: Arc<Fetcher>, user_agent: &str) -> Self {
        Self {
            fetcher,
            agent: agent_token(user_agent),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// True if `url` may be fetched. Never errors: an unreachable or malformed
    /// robots.txt means allowed.
    pub async fn allowed(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return true;
        };

        // Clone the entry out and release the lock before any await, so a slow
        // robots.txt fetch never blocks lookups for other hosts.
        let cached = self.cache.lock().await.get(host).cloned();

        let entry = match cached {
            Some(v) => v,
            None => {
                let fetched = self.fetch(host, url.scheme()).await;
                self.cache
                    .lock()
                    .await
                    .insert(host.to_string(), fetched.clone());
                fetched
            }
        };

        let Some(robot) = entry else {
            return true;
        };

        let path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        robot.allowed(&path)
    }

    async fn fetch(&self, host: &str, scheme: &str) -> Option<Arc<Robot>> {
        let robots_url = Url::parse(&format!("{scheme}://{host}/robots.txt")).ok()?;
        let (_, status, bytes, _) = self.fetcher.get_bytes(&robots_url).await.ok()?;
        if status >= 400 {
            return None;
        }
        Robot::new(&self.agent, &bytes).ok().map(Arc::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_token_is_extracted_from_a_full_ua() {
        assert_eq!(
            agent_token("web2md/0.1 (+https://example.com/bot)"),
            "web2md"
        );
        assert_eq!(agent_token("Mozilla/5.0 (compatible)"), "Mozilla");
        assert_eq!(agent_token("plain"), "plain");
        // Degenerate input must not yield an empty agent, which would make
        // every rule match us.
        assert_eq!(agent_token("/"), "web2md");
        assert_eq!(agent_token(""), "web2md");
    }

    #[test]
    fn parses_a_real_robots_file() {
        let txt = b"User-agent: *\nDisallow: /private/\nAllow: /private/ok.html\n";
        let robot = Robot::new("web2md", txt).unwrap();
        assert!(robot.allowed("/docs/index.html"));
        assert!(!robot.allowed("/private/secret.html"));
        assert!(robot.allowed("/private/ok.html"));
    }

    #[test]
    fn garbage_robots_is_not_fatal() {
        // Whatever the crate decides, parsing must not panic — the caller
        // treats an error as "allowed".
        let _ = Robot::new("web2md", b"\x00\xff not a robots file");
    }
}

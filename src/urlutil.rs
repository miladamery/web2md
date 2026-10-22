use regex::RegexSet;
use url::Url;

use crate::error::Error;

/// Query parameters that only ever identify a campaign, never a document.
/// Stripping them keeps the visited-set from treating one page as many.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "gclid",
    "fbclid",
    "mc_cid",
    "mc_eid",
    "ref",
    "source",
];

/// Extensions we never treat as documentation pages. Note `.xml` is here: it
/// stops link-following from wandering into sitemaps and feed files, but
/// sitemap *fetching* goes through its own path and is unaffected.
pub const SKIP_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "bmp", "avif", "tiff", "css", "js", "mjs",
    "cjs", "map", "json", "xml", "txt", "pdf", "zip", "gz", "tgz", "bz2", "xz", "jar", "war",
    "ear", "woff", "woff2", "ttf", "otf", "eot", "mp4", "webm", "mp3", "ogg", "wav", "avi", "mov",
    "rss", "atom", "exe", "dmg", "msi", "deb", "rpm", "sha1", "sha256", "sha512", "asc", "sig",
    "pom", "gradle", "lock", "csv", "tsv", "xls", "xlsx", "doc", "docx", "ppt", "pptx",
];

/// Patterns excluded by `--exclude-common`. These are the shapes that make a
/// documentation sitemap explode into tens of thousands of near-duplicate URLs.
pub const COMMON_EXCLUDES: &[&str] = &[
    r"/api/",
    r"/apidocs?/",
    r"/javadoc/",
    r"/javadocs/",
    r"/_static/",
    r"/_sources/",
    r"/_images/",
    r"/genindex",
    r"/py-modindex",
    r"/searchindex",
    r"/changelog\.html$",
    r"SNAPSHOT",
    r"[?&](print|view|raw|download)=",
];

/// Drop the fragment, tracking params, default port, and `index.html` tails so
/// that the same document reached two ways canonicalizes to one key.
pub fn canonicalize(url: &Url) -> Url {
    let mut u = url.clone();
    u.set_fragment(None);

    if u.query().is_some() {
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            u.set_query(None);
        } else {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in &kept {
                ser.append_pair(k, v);
            }
            u.set_query(Some(&ser.finish()));
        }
    }

    let path = u.path().to_string();
    if let Some(stripped) = path.strip_suffix("/index.html") {
        u.set_path(&format!("{stripped}/"));
    } else if let Some(stripped) = path.strip_suffix("/index.htm") {
        u.set_path(&format!("{stripped}/"));
    } else if path == "/index.html" || path == "/index.htm" {
        u.set_path("/");
    }

    u
}

/// Canonical string form, used as the dedup key and the regex match target.
pub fn canonical_key(url: &Url) -> String {
    canonicalize(url).to_string()
}

pub fn has_skippable_extension(url: &Url) -> bool {
    let path = url.path();
    match path.rsplit_once('.') {
        Some((_, ext)) => {
            let ext = ext.to_ascii_lowercase();
            SKIP_EXTENSIONS.contains(&ext.as_str())
        }
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Same host as the seed (default).
    #[default]
    Host,
    /// Same host *and* path under the seed's directory. Needed for Pekko,
    /// whose docs live under a versioned prefix on a shared host.
    Prefix,
    /// No host restriction — only sensible alongside an explicit `--include`.
    Any,
}

/// Decides which discovered URLs are in scope for the crawl.
#[derive(Debug, Clone)]
pub struct UrlFilter {
    include: Option<RegexSet>,
    exclude: Option<RegexSet>,
    scope: Scope,
    origin_host: String,
    origin_prefix: String,
}

impl UrlFilter {
    pub fn new(
        origin: &Url,
        include: &[String],
        exclude: &[String],
        scope: Scope,
        exclude_common: bool,
    ) -> Result<Self, Error> {
        let include = if include.is_empty() {
            None
        } else {
            Some(RegexSet::new(include).map_err(|e| Error::Config(format!("--include: {e}")))?)
        };

        let mut all_excludes: Vec<String> = exclude.to_vec();
        if exclude_common {
            all_excludes.extend(COMMON_EXCLUDES.iter().map(|s| s.to_string()));
        }
        let exclude = if all_excludes.is_empty() {
            None
        } else {
            Some(
                RegexSet::new(&all_excludes)
                    .map_err(|e| Error::Config(format!("--exclude: {e}")))?,
            )
        };

        // The seed's directory, e.g. `/docs/pekko/1.7/` for
        // `https://pekko.apache.org/docs/pekko/1.7/actors.html`.
        let mut origin_prefix = origin.path().to_string();
        if !origin_prefix.ends_with('/') {
            match origin_prefix.rfind('/') {
                Some(i) => origin_prefix.truncate(i + 1),
                None => origin_prefix = "/".to_string(),
            }
        }

        Ok(Self {
            include,
            exclude,
            scope,
            origin_host: origin.host_str().unwrap_or_default().to_string(),
            origin_prefix,
        })
    }

    pub fn admits(&self, url: &Url) -> bool {
        if url.scheme() != "http" && url.scheme() != "https" {
            return false;
        }
        if has_skippable_extension(url) {
            return false;
        }

        let host = url.host_str().unwrap_or_default();
        match self.scope {
            Scope::Any => {}
            Scope::Host => {
                if host != self.origin_host {
                    return false;
                }
            }
            Scope::Prefix => {
                if host != self.origin_host || !url.path().starts_with(&self.origin_prefix) {
                    return false;
                }
            }
        }

        let key = canonical_key(url);
        if let Some(ex) = &self.exclude {
            if ex.is_match(&key) {
                return false;
            }
        }
        if let Some(inc) = &self.include {
            if !inc.is_match(&key) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn strips_fragment_and_tracking() {
        let c = canonicalize(&u("https://x.dev/a.html#sec?utm_source=nl"));
        assert_eq!(c.as_str(), "https://x.dev/a.html");

        let c = canonicalize(&u("https://x.dev/a.html?utm_source=nl&page=2"));
        assert_eq!(c.as_str(), "https://x.dev/a.html?page=2");

        let c = canonicalize(&u("https://x.dev/a.html?utm_source=nl&gclid=1"));
        assert_eq!(c.as_str(), "https://x.dev/a.html");
    }

    #[test]
    fn collapses_index_html() {
        assert_eq!(
            canonicalize(&u("https://x.dev/docs/index.html")).as_str(),
            "https://x.dev/docs/"
        );
        assert_eq!(
            canonicalize(&u("https://x.dev/index.html")).as_str(),
            "https://x.dev/"
        );
        // A page merely *named* index.html deeper down is still collapsed,
        // which is correct: it is the directory index.
        assert_eq!(
            canonicalize(&u("https://x.dev/a/b/index.html")).as_str(),
            "https://x.dev/a/b/"
        );
    }

    #[test]
    fn skips_binary_extensions() {
        assert!(has_skippable_extension(&u("https://x.dev/logo.png")));
        assert!(has_skippable_extension(&u("https://x.dev/a/b.css?v=1")));
        assert!(!has_skippable_extension(&u("https://x.dev/actors.html")));
        assert!(!has_skippable_extension(&u("https://x.dev/docs/")));
    }

    #[test]
    fn prefix_scope_confines_to_seed_directory() {
        let seed = u("https://pekko.apache.org/docs/pekko/1.7/actors.html");
        let f = UrlFilter::new(&seed, &[], &[], Scope::Prefix, false).unwrap();

        assert!(f.admits(&u("https://pekko.apache.org/docs/pekko/1.7/typed.html")));
        // Different project on the same host — the reason Prefix scope exists.
        assert!(!f.admits(&u("https://pekko.apache.org/docs/pekko/1.6/actors.html")));
        assert!(!f.admits(&u("https://pekko.apache.org/download.html")));
        assert!(!f.admits(&u("https://other.apache.org/docs/pekko/1.7/x.html")));
    }

    #[test]
    fn common_excludes_tame_a_versioned_sitemap() {
        let seed = u("https://docs.spring.io/spring-boot/");
        let f = UrlFilter::new(&seed, &[], &[], Scope::Host, true).unwrap();

        assert!(f.admits(&u("https://docs.spring.io/spring-boot/reference/using/devtools.html")));
        assert!(!f.admits(&u("https://docs.spring.io/spring-boot/api/java/org/x/Foo.html")));
        assert!(!f.admits(&u("https://docs.spring.io/spring-boot/4.2-SNAPSHOT/x.html")));
    }

    #[test]
    fn include_is_a_whitelist() {
        let seed = u("https://docs.spring.io/spring-boot/");
        let inc = vec![r"/spring-boot/4\.2/reference/".to_string()];
        let f = UrlFilter::new(&seed, &inc, &[], Scope::Host, true).unwrap();

        assert!(f.admits(&u("https://docs.spring.io/spring-boot/4.2/reference/a.html")));
        assert!(!f.admits(&u("https://docs.spring.io/spring-boot/4.0/reference/a.html")));
    }
}

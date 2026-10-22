//! Finding the pages to crawl: sitemaps, explicit URL lists, and link-following.

use std::collections::{HashSet, VecDeque};
use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use scraper::{Html, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::fetch::Fetcher;
use crate::urlutil::{canonical_key, has_skippable_extension, UrlFilter};

/// Cap on sitemap-index recursion. Real sites nest two levels at most; this
/// only exists so a self-referential sitemap cannot loop forever.
const MAX_SITEMAPS: usize = 64;

/// Breadth-first crawl queue with a canonical-key visited set.
#[derive(Debug, Default)]
pub struct Frontier {
    queue: VecDeque<(Url, usize)>,
    seen: HashSet<String>,
}

impl Frontier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue `url` at `depth`. Returns false if it was already seen — the
    /// canonical key means `page`, `page#x`, and `page?utm_source=nl` count
    /// as one page.
    pub fn push(&mut self, url: Url, depth: usize) -> bool {
        if self.seen.insert(canonical_key(&url)) {
            self.queue.push_back((url, depth));
            true
        } else {
            false
        }
    }

    pub fn pop(&mut self) -> Option<(Url, usize)> {
        self.queue.pop_front()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn visited(&self) -> usize {
        self.seen.len()
    }
}

/// Every `<a href>` in the document, resolved against `base` and stripped of
/// fragments. Non-HTTP schemes and asset links are dropped here rather than
/// left for the filter, because resolving them is wasted work.
pub fn extract_links(html: &str, base: &Url) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for el in doc.select(&sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("mailto:")
            || lower.starts_with("javascript:")
            || lower.starts_with("tel:")
            || lower.starts_with("data:")
        {
            continue;
        }
        let Ok(mut u) = base.join(href) else { continue };
        u.set_fragment(None);
        if u.scheme() != "http" && u.scheme() != "https" {
            continue;
        }
        if has_skippable_extension(&u) {
            continue;
        }
        out.push(u);
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum SitemapItem {
    Page(Url),
    Nested(Url),
}

/// Parse a `<urlset>` or `<sitemapindex>`. Unknown elements are ignored, which
/// is what keeps this working against the various namespace-prefixed sitemaps
/// in the wild.
fn parse_sitemap(bytes: &[u8]) -> Result<Vec<SitemapItem>> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);

    let mut items = Vec::new();
    let mut buf = Vec::new();
    // Which container we are inside decides what a <loc> means.
    let mut in_sitemap = false;
    let mut in_url = false;

    loop {
        // `read_event_into` borrows `buf`, so the event must be fully handled
        // (including any nested `read_text`) before the next iteration.
        let event = match reader.read_event_into(&mut buf) {
            Ok(e) => e,
            Err(e) => return Err(Error::Other(format!("sitemap parse error: {e}"))),
        };

        match event {
            Event::Start(e) => {
                let name = e.name();
                match local_name(name.as_ref()) {
                    "urlset" => {
                        in_sitemap = false;
                        in_url = false;
                    }
                    "sitemap" => in_sitemap = true,
                    "url" => in_url = true,
                    // `read_text` consumes through the matching end tag and
                    // unescapes entities, so `&amp;` in a URL survives.
                    "loc" if in_url || in_sitemap => {
                        let nested = in_sitemap;
                        if let Ok(text) = reader.read_text(name) {
                            push_item(&mut items, text.trim(), nested);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match local_name(e.name().as_ref()) {
                "sitemap" => in_sitemap = false,
                "url" => in_url = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(items)
}

/// Strip any `prefix:` from a qualified name so `sm:url` matches `url`.
fn local_name(raw: &str) -> &str {
    match raw.rfind(':') {
        Some(i) => &raw[i + 1..],
        None => raw,
    }
}

fn push_item(items: &mut Vec<SitemapItem>, loc: &str, nested: bool) {
    if loc.is_empty() {
        return;
    }
    let Ok(u) = Url::parse(loc) else { return };
    items.push(if nested {
        SitemapItem::Nested(u)
    } else {
        SitemapItem::Page(u)
    });
}

/// Gunzip if the payload is gzip. Plenty of sites publish only
/// `sitemap.xml.gz`, and reqwest's decompression does not cover a `.gz` file
/// served as an opaque attachment.
fn maybe_gunzip(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() < 2 || bytes[0] != 0x1f || bytes[1] != 0x8b {
        return Ok(bytes);
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut out)
        .map_err(|e| Error::Other(format!("sitemap gunzip failed: {e}")))?;
    Ok(out)
}

/// Walk a sitemap (recursing through sitemap indexes) and return the pages
/// that pass `filter`. Nested sitemap URLs are *not* filtered — they often
/// live outside the documentation prefix that the pages themselves use.
pub async fn collect_from_sitemap(
    fetcher: &Fetcher,
    root: &Url,
    filter: &UrlFilter,
) -> Result<Vec<Url>> {
    let mut pending = VecDeque::from([root.clone()]);
    let mut fetched: HashSet<String> = HashSet::new();
    let mut pages: Vec<Url> = Vec::new();
    let mut seen_pages: HashSet<String> = HashSet::new();

    while let Some(sitemap_url) = pending.pop_front() {
        if fetched.len() >= MAX_SITEMAPS {
            break;
        }
        if !fetched.insert(canonical_key(&sitemap_url)) {
            continue;
        }

        let (_, status, bytes, _) = fetcher.get_bytes(&sitemap_url).await.map_err(|e| {
            Error::Other(format!("sitemap {sitemap_url}: {e}"))
        })?;
        if status >= 400 {
            return Err(Error::Other(format!(
                "sitemap {sitemap_url} returned HTTP {status}"
            )));
        }

        for item in parse_sitemap(&maybe_gunzip(bytes)?)? {
            match item {
                SitemapItem::Nested(u) => pending.push_back(u),
                SitemapItem::Page(u) => {
                    if filter.admits(&u) && seen_pages.insert(canonical_key(&u)) {
                        pages.push(u);
                    }
                }
            }
        }
    }

    Ok(pages)
}

/// Read a newline-delimited URL file. Blank lines and `#` comments are
/// ignored so the file can be hand-maintained.
pub fn read_url_file(path: &std::path::Path) -> Result<Vec<Url>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;

    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let u = Url::parse(line)
            .map_err(|e| Error::BadUrl(format!("{}:{}: {line}: {e}", path.display(), i + 1)))?;
        out.push(u);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_a_urlset() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
        <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
          <url><loc>https://x.dev/a.html</loc><lastmod>2024-01-01</lastmod></url>
          <url><loc>https://x.dev/b.html</loc></url>
        </urlset>"#;
        let items = parse_sitemap(xml).unwrap();
        assert_eq!(
            items,
            vec![
                SitemapItem::Page(u("https://x.dev/a.html")),
                SitemapItem::Page(u("https://x.dev/b.html")),
            ]
        );
    }

    #[test]
    fn parses_a_sitemap_index_with_namespaces() {
        let xml = br#"<?xml version="1.0"?>
        <sm:sitemapindex xmlns:sm="http://www.sitemaps.org/schemas/sitemap/0.9">
          <sm:sitemap><sm:loc>https://x.dev/sm-1.xml</sm:loc></sm:sitemap>
          <sm:sitemap><sm:loc>https://x.dev/sm-2.xml</sm:loc></sm:sitemap>
        </sm:sitemapindex>"#;
        let items = parse_sitemap(xml).unwrap();
        assert_eq!(
            items,
            vec![
                SitemapItem::Nested(u("https://x.dev/sm-1.xml")),
                SitemapItem::Nested(u("https://x.dev/sm-2.xml")),
            ]
        );
    }

    #[test]
    fn a_page_url_is_not_mistaken_for_a_nested_sitemap() {
        // The <loc> under <url> must be a page even though <url> is nested in
        // <urlset>; only <sitemap> means "another sitemap".
        let xml = br#"<urlset><url><loc>https://x.dev/page.html</loc></url></urlset>"#;
        assert_eq!(
            parse_sitemap(xml).unwrap(),
            vec![SitemapItem::Page(u("https://x.dev/page.html"))]
        );
    }

    #[test]
    fn resolves_links_against_a_base_and_drops_junk() {
        let base = u("https://x.dev/docs/pekko/1.7/actors.html");
        // NOTE: r##"..."## — the HTML contains `"#`, which closes r#"..."#.
        let html = r##"<a href="typed.html">t</a>
            <a href="/docs/pekko/1.7/io.html">io</a>
            <a href="#section">self</a>
            <a href="mailto:a@b.c">mail</a>
            <a href="javascript:void(0)">js</a>
            <a href="/logo.png">img</a>
            <a href="https://other.dev/x.html">off</a>"##;
        let got: Vec<String> = extract_links(html, &base).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            got,
            vec![
                "https://x.dev/docs/pekko/1.7/typed.html",
                "https://x.dev/docs/pekko/1.7/io.html",
                "https://other.dev/x.html",
            ]
        );
    }

    #[test]
    fn frontier_dedups_on_canonical_key() {
        let mut f = Frontier::new();
        assert!(f.push(u("https://x.dev/a.html"), 0));
        assert!(!f.push(u("https://x.dev/a.html#top"), 1));
        assert!(!f.push(u("https://x.dev/a.html?utm_source=nl"), 1));
        assert!(f.push(u("https://x.dev/b.html"), 1));
        assert_eq!(f.len(), 2);
        assert_eq!(f.visited(), 2);
    }

    #[test]
    fn frontier_is_breadth_first() {
        let mut f = Frontier::new();
        f.push(u("https://x.dev/a.html"), 0);
        f.push(u("https://x.dev/b.html"), 1);
        assert_eq!(f.pop().unwrap().1, 0);
        assert_eq!(f.pop().unwrap().1, 1);
        assert!(f.pop().is_none());
    }

    #[test]
    fn gunzip_passes_plain_xml_through() {
        let xml = b"<urlset/>".to_vec();
        assert_eq!(maybe_gunzip(xml.clone()).unwrap(), xml);
    }

    #[test]
    fn gunzip_decodes_a_gzipped_sitemap() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let xml = br#"<urlset><url><loc>https://x.dev/a.html</loc></url></urlset>"#;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(xml).unwrap();
        let gz = enc.finish().unwrap();

        let out = maybe_gunzip(gz).unwrap();
        assert_eq!(
            parse_sitemap(&out).unwrap(),
            vec![SitemapItem::Page(u("https://x.dev/a.html"))]
        );
    }

    #[test]
    fn url_file_ignores_blanks_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("urls.txt");
        std::fs::write(&p, "# seed list\n\nhttps://x.dev/a.html\n  https://x.dev/b.html  \n").unwrap();

        let urls = read_url_file(&p).unwrap();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[1].as_str(), "https://x.dev/b.html");
    }

    #[test]
    fn url_file_reports_the_offending_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("urls.txt");
        std::fs::write(&p, "https://x.dev/a.html\nnot a url\n").unwrap();
        let err = read_url_file(&p).unwrap_err().to_string();
        assert!(err.contains(":2:"), "should name line 2: {err}");
    }
}

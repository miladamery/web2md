//! Driving the crawl: discover, fetch, extract, write, index.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use scraper::Html;
use url::Url;

use crate::cli::LinkStyle;
use crate::config::Settings;
use crate::discover::{self, Frontier};
use crate::error::{Error, Result};
use crate::extract::{self, ExtractOptions};
use crate::fetch::{Conditional, Fetcher};
use crate::output::{self, OutputMap, PageRecord};
use crate::postprocess::{self, FrontMatter, LinkContext};
use crate::profile::{self, Profile};
use crate::robots::Robots;
use crate::state::{Entry, State};
use crate::urlutil::UrlFilter;

/// What a run produced.
#[derive(Debug, Default)]
pub struct Summary {
    pub discovered: usize,
    pub written: usize,
    /// Reached, but the server said our copy was current.
    pub unchanged: usize,
    /// Already on disk and `--skip-existing` was set.
    pub skipped: usize,
    /// `(url, reason)` for pages that could not be turned into Markdown.
    pub failed: Vec<(String, String)>,
    /// Non-fatal oddities worth reporting, e.g. a code-count mismatch.
    pub warnings: Vec<String>,
    pub output_dir: PathBuf,
    pub index: Option<PathBuf>,
    pub merged: Option<PathBuf>,
    pub dry_run: bool,
    pub urls: Vec<String>,
}

/// What happened to one page.
enum Outcome {
    Written { title: String, warnings: Vec<String> },
    Unchanged,
    Skipped,
    Failed(String),
}

/// One page to process, and whether the crawl needs its body back.
struct Job<'a> {
    url: &'a Url,
    path: &'a Path,
    /// Link-following needs the body, so a BFS job cannot take the cheap exits
    /// that a sitemap job can: a `304` or a skip would end discovery there.
    need_body: bool,
}

struct Crawler {
    settings: Settings,
    filter: UrlFilter,
    fetcher: Arc<Fetcher>,
    robots: Option<Arc<Robots>>,
    profiles: Vec<Profile>,
    extract_opts: ExtractOptions,
    state: State,
}

/// Run a crawl from already-resolved settings.
pub async fn run(settings: Settings) -> Result<Summary> {
    let origin = settings
        .origin()
        .cloned()
        .ok_or_else(|| Error::Config("no origin URL to crawl from".into()))?;

    let filter = UrlFilter::new(
        &origin,
        &settings.include,
        &settings.exclude,
        settings.scope,
        settings.exclude_common,
    )?;

    let fetcher = Arc::new(Fetcher::new(&settings)?);
    let robots = settings
        .respect_robots
        .then(|| Arc::new(Robots::new(fetcher.clone(), &settings.user_agent)));

    let extract_opts = ExtractOptions {
        keep_images: settings.keep_images,
        transform_dl_tabs: true,
    };
    let state = State::load(&settings.output_dir);
    let profiles = settings.profiles.clone();

    Crawler {
        settings,
        filter,
        fetcher,
        robots,
        profiles,
        extract_opts,
        state,
    }
    .run()
    .await
}

impl Crawler {
    async fn run(&self) -> Result<Summary> {
        // An explicit URL list or a sitemap gives the whole set up front,
        // which is both faster and lets the output prefix be computed from
        // every URL rather than guessed from the seed.
        if let Some(path) = &self.settings.urls_file {
            let urls: Vec<Url> = discover::read_url_file(path)?
                .into_iter()
                .filter(|u| self.filter.admits(u))
                .collect();
            return self.process_known(urls).await;
        }

        if let Some(sitemap) = &self.settings.sitemap {
            let root = Url::parse(sitemap).map_err(|e| Error::BadUrl(format!("{sitemap}: {e}")))?;
            let urls = discover::collect_from_sitemap(&self.fetcher, &root, &self.filter).await?;
            return self.process_known(urls).await;
        }

        self.crawl_bfs().await
    }

    /// Every URL is known before the first page is fetched.
    async fn process_known(&self, urls: Vec<Url>) -> Result<Summary> {
        let urls = match self.settings.limit {
            Some(n) => urls.into_iter().take(n).collect(),
            None => urls,
        };

        if self.settings.dry_run {
            return Ok(self.dry_run_summary(urls));
        }

        let prefix = output::common_dir_prefix(&urls);
        let mut map = OutputMap::new(self.settings.output_dir.clone(), prefix);
        // Assign every path before any fetching, so the link rewriter has the
        // complete page set when it runs.
        let jobs: Vec<(Url, PathBuf)> = urls
            .into_iter()
            .map(|u| {
                let p = map.assign(&u);
                (u, p)
            })
            .collect();

        let pb = self.progress(jobs.len() as u64);
        let results = stream::iter(jobs)
            .map(|(url, path)| {
                let pb = pb.clone();
                async move {
                    let job = Job {
                        url: &url,
                        path: &path,
                        need_body: false,
                    };
                    let (outcome, _links) = self.process_one(&job).await;
                    pb.inc(1);
                    (url, outcome)
                }
            })
            .buffer_unordered(self.settings.concurrency)
            .collect()
            .await;
        pb.finish_and_clear();

        self.finish(results, &map).await
    }

    /// No sitemap and no URL list: follow links outward from the seeds.
    ///
    /// Paths are assigned per batch rather than up front, because the URL set
    /// is not known until the crawl ends.
    async fn crawl_bfs(&self) -> Result<Summary> {
        // The seed's own directory is the only prefix knowable in advance, and
        // it is the useful one: it puts a single-project crawl at the root.
        let prefix = match self.settings.origin() {
            Some(u) => output::common_dir_prefix(std::slice::from_ref(u)),
            None => "/".to_string(),
        };
        let mut map = OutputMap::new(self.settings.output_dir.clone(), prefix);

        let mut frontier = Frontier::new();
        for seed in &self.settings.seeds {
            frontier.push(seed.clone(), 0);
        }

        let pb = self.progress_spinner();
        let mut results = Vec::new();
        let max_depth = self.settings.max_depth;

        loop {
            let want = match self.settings.limit {
                Some(limit) => match limit.checked_sub(results.len()) {
                    Some(0) | None => break,
                    Some(remaining) => self.settings.concurrency.min(remaining),
                },
                None => self.settings.concurrency,
            };

            // Assign paths serially, then share the map immutably for the
            // fetch. Collision resolution needs `&mut`, so it cannot happen
            // inside the concurrent section.
            let mut batch: Vec<(Url, PathBuf, usize)> = Vec::new();
            while batch.len() < want {
                let Some((url, depth)) = frontier.pop() else {
                    break;
                };
                let path = map.assign(&url);
                batch.push((url, path, depth));
            }
            if batch.is_empty() {
                break;
            }

            let found = stream::iter(batch)
                .map(|(url, path, depth)| {
                    let pb = pb.clone();
                    async move {
                        let job = Job {
                            url: &url,
                            path: &path,
                            need_body: true,
                        };
                        let (outcome, links) = self.process_one(&job).await;
                        pb.inc(1);
                        (url, outcome, links, depth)
                    }
                })
                .buffer_unordered(self.settings.concurrency)
                .collect::<Vec<_>>()
                .await;

            // Links are followed from the same fetch that produced the page,
            // so no page is ever downloaded twice.
            for (url, outcome, links, depth) in found {
                if depth < max_depth {
                    for link in links {
                        if self.filter.admits(&link) {
                            frontier.push(link, depth + 1);
                        }
                    }
                }
                results.push((url, outcome));
            }
        }

        pb.finish_and_clear();
        self.finish(results, &map).await
    }

    /// One page, end to end. Returns the links found on it, which only the BFS
    /// path uses.
    async fn process_one(&self, job: &Job<'_>) -> (Outcome, Vec<Url>) {
        let url = job.url;

        if !job.need_body
            && self.settings.skip_existing
            && self.settings.output_dir.join(job.path).exists()
        {
            return (Outcome::Skipped, Vec::new());
        }

        if let Some(robots) = &self.robots {
            if !robots.allowed(url).await {
                return (Outcome::Failed("disallowed by robots.txt".into()), Vec::new());
            }
        }

        let conditional = if job.need_body {
            // A `304` would leave us with no body and therefore no links, so
            // the BFS path always asks for the page itself.
            self.fetcher.get_html_conditional(url, None, None).await
        } else {
            let previous = self.state.get(url);
            self.fetcher
                .get_html_conditional(
                    url,
                    previous.and_then(|e| e.etag.as_deref()),
                    previous.and_then(|e| e.last_modified.as_deref()),
                )
                .await
        };

        let fetched = match conditional {
            Ok(Conditional::NotModified) => return (Outcome::Unchanged, Vec::new()),
            Ok(Conditional::NotHtml) => {
                return (Outcome::Failed("not an HTML document".into()), Vec::new())
            }
            Ok(Conditional::Modified { fetched, .. }) => fetched,
            Err(e) => return (Outcome::Failed(e.to_string()), Vec::new()),
        };

        let links = if job.need_body {
            discover::extract_links(&fetched.body, &fetched.final_url)
        } else {
            Vec::new()
        };

        let profile = self.profile_for(&fetched.final_url, &fetched.body);
        let extracted = match extract::extract_page(
            &fetched.body,
            &fetched.final_url,
            profile,
            &self.extract_opts,
        ) {
            Ok(e) => e,
            Err(e) => return (Outcome::Failed(e.to_string()), links),
        };

        let mut body = postprocess::normalize(&extracted.markdown);

        if self.settings.front_matter {
            let fm = FrontMatter {
                title: extracted.title.clone().unwrap_or_default(),
                source: url.to_string(),
                site: url.host_str().unwrap_or_default().to_string(),
                section: section_of(url, self.settings.origin()),
            };
            body = format!("{}{}", fm.render(), body);
        }

        match output::write_file(&self.settings.output_dir, job.path, &body) {
            Ok(_) => (
                Outcome::Written {
                    title: extracted.title.unwrap_or_default(),
                    warnings: extracted.warnings,
                },
                links,
            ),
            Err(e) => (Outcome::Failed(e.to_string()), links),
        }
    }

    /// Explicit profile, then the host, then `<meta name="generator">`.
    ///
    /// The host claim is checked against the page before it is trusted. One
    /// host can serve more than one documentation system — `doc.akka.io` is
    /// Paradox under `/libraries/akka-core/` and Antora under `/concepts/` —
    /// and an unchecked host match hands every Antora page to the Paradox
    /// profile, which then misses and quietly degrades to the generic chain on
    /// the whole SDK half of the site.
    ///
    /// The cost is one DOM parse on the host path that the common case does not
    /// strictly need. That is a couple of milliseconds against a network fetch
    /// of a couple of hundred, and it buys a correct profile on mixed hosts.
    fn profile_for<'a>(&'a self, url: &Url, html: &str) -> &'a Profile {
        if let Some(name) = self.settings.explicit_profile.as_deref() {
            if let Some(p) = self.profiles.iter().find(|p| p.name == name) {
                return p;
            }
        }

        let host = profile::resolve_by_host(&self.profiles, url);
        let doc = Html::parse_document(html);

        if let Some(p) = host {
            if extract::root_matches(&doc, &p.content_selectors) {
                return p;
            }
            let generator = extract::detect_generator(&doc);
            if let Some(g) = profile::resolve_by_generator(&self.profiles, generator.as_deref()) {
                return g;
            }
            // Nothing fits better. Keep the host profile so `extract_page`
            // still reports that it matched no content root — that warning is
            // how a site redesign makes itself known.
            return p;
        }

        let generator = extract::detect_generator(&doc);
        profile::resolve(&self.profiles, url, generator.as_deref(), None)
    }

    /// Rewrite links, build the index, optionally merge, and save state.
    async fn finish(
        &self,
        results: Vec<(Url, Outcome)>,
        map: &OutputMap,
    ) -> Result<Summary> {
        let mut summary = Summary {
            discovered: results.len(),
            output_dir: self.settings.output_dir.clone(),
            ..Default::default()
        };
        let mut records: Vec<PageRecord> = Vec::new();

        for (url, outcome) in results {
            let path = map.get(&url).cloned();
            match outcome {
                Outcome::Written { title, warnings } => {
                    summary.written += 1;
                    summary.warnings.extend(warnings);
                    if let Some(path) = path {
                        records.push(PageRecord {
                            url: url.clone(),
                            path: path.to_string_lossy().replace('\\', "/"),
                            title,
                        });
                    }
                }
                // Skipped and unchanged pages are still part of the corpus, so
                // they belong in the index. Without this, `--skip-existing`
                // would quietly shrink the table of contents on every re-run.
                Outcome::Skipped | Outcome::Unchanged => {
                    if matches!(outcome, Outcome::Skipped) {
                        summary.skipped += 1;
                    } else {
                        summary.unchanged += 1;
                    }
                    if let Some(path) = path {
                        let rel = path.to_string_lossy().replace('\\', "/");
                        let full = self.settings.output_dir.join(&path);
                        let title = existing_title(&full).unwrap_or_default();
                        records.push(PageRecord {
                            url: url.clone(),
                            path: rel,
                            title,
                        });
                    }
                }
                Outcome::Failed(reason) => summary.failed.push((url.to_string(), reason)),
            }
        }

        // Links can only be resolved once every page's path is known.
        self.rewrite_links_on_disk(&records, map);

        records.sort_by(|a, b| output::natural_cmp(&a.path, &b.path));

        if !records.is_empty() {
            let site = self
                .settings
                .origin()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            let index = output::build_index(&records, &site);
            summary.index = Some(output::write_file(
                &self.settings.output_dir,
                Path::new(output::INDEX_FILE),
                &index,
            )?);
        }

        if self.settings.merge && !records.is_empty() {
            summary.merged = Some(self.write_merged(&records)?);
        }

        if let Err(e) = self.save_state(&records) {
            // A state file we could not write costs a future full re-crawl,
            // not a failed run.
            tracing::warn!("could not save state: {e}");
        }

        Ok(summary)
    }

    fn rewrite_links_on_disk(&self, records: &[PageRecord], map: &OutputMap) {
        if self.settings.link_style == LinkStyle::Keep {
            return;
        }
        let pages = map.pages().clone();
        for record in records {
            let full = self.settings.output_dir.join(&record.path);
            let Ok(text) = std::fs::read_to_string(&full) else {
                continue;
            };
            let ctx = LinkContext {
                current: &record.path,
                style: self.settings.link_style,
                pages: &pages,
            };
            let rewritten = postprocess::rewrite_links(&text, &ctx);
            if rewritten != text {
                let _ = std::fs::write(&full, rewritten);
            }
        }
    }

    fn write_merged(&self, records: &[PageRecord]) -> Result<PathBuf> {
        let site = self
            .settings
            .origin()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "documentation".to_string());

        let mut out = format!("# {site}\n\n{} pages, concatenated.\n", records.len());

        for record in records {
            let full = self.settings.output_dir.join(&record.path);
            let Ok(text) = std::fs::read_to_string(&full) else {
                continue;
            };
            out.push_str("\n\n---\n\n");
            // Front matter would be a stray `---` block mid-document; demote
            // it so the merged file has exactly one, or none.
            out.push_str(&demote_front_matter(&text));
        }

        let name = format!("{}.md", sanitize_for_filename(&site));
        output::write_file(&self.settings.output_dir, Path::new(&name), &out)
    }

    fn save_state(&self, records: &[PageRecord]) -> Result<()> {
        let mut state = State::default();
        for record in records {
            // Carry the validators forward: this run's response headers are
            // not captured per page, but the previous ones still describe the
            // copy on disk.
            let previous = self.state.get(&record.url);
            state.insert(
                &record.url,
                Entry {
                    path: record.path.clone(),
                    etag: previous.and_then(|e| e.etag.clone()),
                    last_modified: previous.and_then(|e| e.last_modified.clone()),
                },
            );
        }
        state.save(&self.settings.output_dir)
    }

    fn dry_run_summary(&self, urls: Vec<Url>) -> Summary {
        Summary {
            discovered: urls.len(),
            output_dir: self.settings.output_dir.clone(),
            dry_run: true,
            urls: urls.iter().map(|u| u.to_string()).collect(),
            ..Default::default()
        }
    }

    fn progress(&self, len: u64) -> ProgressBar {
        let pb = ProgressBar::new(len);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40}] {pos}/{len}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
        );
        pb
    }

    fn progress_spinner(&self) -> ProgressBar {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {pos} pages")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        pb
    }
}

/// Read the title of a page written by an earlier run, so the index stays
/// complete when `--skip-existing` or a `304` keeps us from regenerating it.
fn existing_title(full: &Path) -> Option<String> {
    let text = std::fs::read_to_string(full).ok()?;

    if let Some(rest) = text.strip_prefix("---\n") {
        for line in rest.lines() {
            if line == "---" {
                break;
            }
            if let Some(v) = line.strip_prefix("title: ") {
                let v = v.trim().trim_matches('"').replace("\\\"", "\"");
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }

    text.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
}

/// The directory a page sits in, relative to the crawl origin. Useful as a
/// retrieval hint ("which chapter is this from").
fn section_of(url: &Url, origin: Option<&Url>) -> String {
    let path = url.path();
    let dir = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    let dir = match origin {
        Some(o) => {
            let prefix = o.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            dir.strip_prefix(prefix).unwrap_or(dir)
        }
        None => dir,
    };
    dir.trim_matches('/').to_string()
}

/// Turn the leading `---` front-matter block into an HTML comment so the
/// merged document has exactly one front matter block (or none).
fn demote_front_matter(text: &str) -> String {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text.to_string();
    };
    let Some(end) = rest.find("\n---\n") else {
        return text.to_string();
    };
    format!("<!--\n{}\n-->\n{}", &rest[..end], &rest[end + 5..])
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_is_relative_to_the_origin() {
        let origin = Url::parse("https://pekko.apache.org/docs/pekko/1.7/actors.html").unwrap();
        let u = Url::parse("https://pekko.apache.org/docs/pekko/1.7/typed/actors.html").unwrap();
        assert_eq!(section_of(&u, Some(&origin)), "typed");
    }

    #[test]
    fn section_of_a_root_page_is_empty() {
        let origin = Url::parse("https://x.dev/docs/a.html").unwrap();
        assert_eq!(section_of(&origin, Some(&origin)), "");
    }

    #[test]
    fn demotes_front_matter_to_a_comment() {
        let text = "---\ntitle: \"X\"\n---\n\n# Body\n";
        let out = demote_front_matter(text);
        assert!(out.starts_with("<!--\ntitle: \"X\"\n-->"));
        assert!(out.contains("# Body"));
        assert!(!out.contains("\n---\n"));
    }

    #[test]
    fn demote_is_a_no_op_without_front_matter() {
        assert_eq!(demote_front_matter("# Body\n"), "# Body\n");
        // An unterminated block must not be mangled.
        assert_eq!(demote_front_matter("---\ntitle: x\n"), "---\ntitle: x\n");
    }

    #[test]
    fn filename_sanitization() {
        assert_eq!(sanitize_for_filename("doc.akka.io"), "doc.akka.io");
        assert_eq!(sanitize_for_filename("x:8080"), "x-8080");
    }

    #[test]
    fn existing_title_reads_front_matter_then_h1() {
        let dir = tempfile::tempdir().unwrap();

        let with_fm = dir.path().join("a.md");
        std::fs::write(&with_fm, "---\ntitle: \"Actors\"\n---\n\n# Ignored\n").unwrap();
        assert_eq!(existing_title(&with_fm).as_deref(), Some("Actors"));

        let no_fm = dir.path().join("b.md");
        std::fs::write(&no_fm, "# From Heading\n").unwrap();
        assert_eq!(existing_title(&no_fm).as_deref(), Some("From Heading"));

        assert!(existing_title(&dir.path().join("missing.md")).is_none());
    }
}

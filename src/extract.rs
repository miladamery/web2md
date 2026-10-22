//! Turning one HTML page into Markdown.
//!
//! Pipeline: pick the content root -> prune noise and normalize tab groups ->
//! capture `<pre>` bodies -> stub them out -> convert -> splice the code back.
//!
//! Pruning happens *before* capture so that things like Asciidoctor's
//! line-number gutter — which is a second `<pre>` holding bare digits — are
//! gone before we start counting code blocks.

use std::borrow::Cow;
use std::cell::Cell;

use lol_html::html_content::ContentType;
use lol_html::{rewrite_str, ElementContentHandlers, RewriteStrSettings, Selector as LolSelector};
use scraper::{ElementRef, Html, Selector};
use url::Url;

use crate::codeblock::{self, CodeBlock};
use crate::error::{Error, Result};
use crate::profile::Profile;

/// A content root needs at least this much text before we trust it. Guards
/// against latching onto an empty `<article>` shell.
const MIN_ROOT_TEXT: usize = 200;

/// The element `lol_html` hands to a local handler.
///
/// These two aliases exist to pin `lol_html`'s handler-type parameter. Left to
/// inference, `ElementContentHandlers::default()` is ambiguous in `H`, which
/// makes the `IntoHandler` target an unresolved `_` and produces a confusing
/// "trait not implemented" error rather than a type error.
type El<'r, 'h> = lol_html::html_content::Element<'r, 'h, lol_html::LocalHandlerTypes>;
type Handlers<'h> = lol_html::ElementContentHandlers<'h, lol_html::LocalHandlerTypes>;

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub keep_images: bool,
    pub transform_dl_tabs: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            keep_images: false,
            transform_dl_tabs: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub markdown: String,
    pub title: Option<String>,
    pub code_blocks: usize,
    pub warnings: Vec<String>,
}

/// `<meta name="generator">`, used to pick a profile for unknown hosts.
pub fn detect_generator(doc: &Html) -> Option<String> {
    let sel = Selector::parse(r#"meta[name="generator"]"#).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(|s| s.to_string())
}

/// Try the profile's selectors in priority order. The first one carrying
/// enough text wins; otherwise the richest match is used so we still emit
/// something rather than nothing.
pub fn select_root<'a>(doc: &'a Html, selectors: &[String]) -> Option<ElementRef<'a>> {
    let mut best: Option<(ElementRef<'a>, usize)> = None;

    for s in selectors {
        let Ok(sel) = Selector::parse(s) else { continue };
        let Some(el) = doc.select(&sel).next() else {
            continue;
        };
        let len: usize = el.text().map(str::len).sum();
        if len >= MIN_ROOT_TEXT {
            return Some(el);
        }
        if best.as_ref().is_none_or(|(_, b)| len > *b) {
            best = Some((el, len));
        }
    }

    best.map(|(el, _)| el)
}

/// Whether any of `selectors` matches this document at all.
///
/// Used to test a host-matched profile against the page in hand: one host can
/// serve more than one doc system, and the host claim must not shadow the
/// generator for the other one.
pub fn root_matches(doc: &Html, selectors: &[String]) -> bool {
    selectors.iter().any(|s| {
        Selector::parse(s)
            .ok()
            .is_some_and(|sel| doc.select(&sel).next().is_some())
    })
}

/// The page's own title.
///
/// The content root is searched first. Several doc themes put an `<h1>` in the
/// site header — Akka's reads "Akka core" on every page — and a document-wide
/// `h1` query would title the entire site with it.
pub fn extract_title(root: &ElementRef, doc: &Html, selector: Option<&str>) -> Option<String> {
    let candidates = match selector {
        Some(s) => vec![s, "h1"],
        None => vec!["h1"],
    };

    for scope in [Some(root), None] {
        for s in &candidates {
            let Ok(sel) = Selector::parse(s) else { continue };
            let found = match scope {
                Some(r) => r.select(&sel).next(),
                None => doc.select(&sel).next(),
            };
            if let Some(el) = found {
                let t = clean_title(&el.text().collect::<String>());
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }

    let sel = Selector::parse("title").ok()?;
    let el = doc.select(&sel).next()?;
    let t = clean_title(&el.text().collect::<String>());
    (!t.is_empty()).then_some(t)
}

/// Strip the site-name suffix doc sites append to their titles, e.g.
/// `"Classic Actors - Apache Pekko Documentation"` -> `"Classic Actors"`.
fn clean_title(raw: &str) -> String {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    for sep in [" :: ", " | ", " — ", " – ", " • ", " · "] {
        if let Some((head, _)) = t.split_once(sep) {
            return head.trim().to_string();
        }
    }
    // " - " is ambiguous (many titles legitimately contain it), so only split
    // when the tail looks like a site name.
    if let Some((head, tail)) = t.rsplit_once(" - ") {
        let tail_l = tail.to_ascii_lowercase();
        if tail_l.contains("documentation")
            || tail_l.contains("docs")
            || tail_l.contains("reference")
        {
            return head.trim().to_string();
        }
    }
    t
}

/// Prune noise, label tab groups, and bold Asciidoctor listing captions.
fn prune_fragment(
    fragment: &str,
    prune_selectors: &[String],
    transform_dl_tabs: bool,
) -> Result<String> {
    let mut settings = RewriteStrSettings::new();

    if !prune_selectors.is_empty() {
        let joined = prune_selectors.join(", ");
        let sel = parse_selector(&joined)?;
        settings = settings.append_element_content_handler((
            Cow::Owned(sel),
            Handlers::default().element(|el: &mut El<'_, '_>| {
                el.remove();
                Ok(())
            }),
        ));
    }

    // Paradox (Akka/Pekko) renders tabbed code variants as
    // `<dl><dt>Java</dt><dd><pre>…</pre></dd>…</dl>`. Turning each term into a
    // bold label is what keeps "Java" attached to its snippet; without it the
    // page reads as a run of unexplained near-duplicate code.
    if transform_dl_tabs {
        if let Ok(sel) = parse_selector("dl > dt") {
            settings = settings.append_element_content_handler((
                Cow::Owned(sel),
                Handlers::default().element(bolden_as_paragraph),
            ));
        }
    }

    // Asciidoctor names each listing with a sibling `div.title` ("Maven",
    // "Gradle"). Bold it so the label stays visually attached to its block.
    if let Ok(sel) = parse_selector("div.listingblock > div.title") {
        settings = settings.append_element_content_handler((
            Cow::Owned(sel),
            ElementContentHandlers::default().element(bolden_as_paragraph),
        ));
    }

    // Admonition labels ("Note", "Warning"). Left alone they come out as a
    // bare paragraph that reads like the first sentence of the body.
    if let Ok(sel) = parse_selector(".callout-title, .admonition-title, .admonitionblock td.icon") {
        settings = settings.append_element_content_handler((
            Cow::Owned(sel),
            ElementContentHandlers::default().element(bolden_as_paragraph),
        ));
    }

    // MkDocs Material tabbed code. Labels are emitted as bold paragraphs ahead
    // of the content; the radio inputs are already removed by BASE_PRUNE.
    if let Ok(sel) = parse_selector(".tabbed-labels > label") {
        settings = settings.append_element_content_handler((
            Cow::Owned(sel),
            ElementContentHandlers::default().element(bolden_as_paragraph),
        ));
    }

    rewrite_str(fragment, settings).map_err(|e| Error::Rewrite(e.to_string()))
}

fn parse_selector(s: &str) -> Result<LolSelector> {
    s.parse::<LolSelector>()
        .map_err(|e| Error::Rewrite(format!("invalid selector {s:?}: {e}")))
}

/// Turn `<dt>Java</dt>` into `<p><strong>Java</strong></p>` so the label
/// survives as a bold line rather than being flattened into the code.
fn bolden_as_paragraph(el: &mut El<'_, '_>) -> lol_html::HandlerResult {
    let _ = el.set_tag_name("p");
    el.prepend("<strong>", ContentType::Html);
    el.append("</strong>", ContentType::Html);
    Ok(())
}

/// Resolve every `<a href>` against the page URL.
///
/// Doc sites link to each other relatively (`typed.html`, `../io.html`), and
/// the Markdown converter passes those through verbatim. Left alone they would
/// point at files that were never written, because the output tree is `.md`.
/// Making them absolute here gives the post-processor a URL it can look up.
fn absolutize_links(fragment: &str, base: &Url) -> Result<String> {
    let base = base.clone();
    let sel = parse_selector("a[href]")?;

    let settings = RewriteStrSettings::new().append_element_content_handler((
        Cow::Owned(sel),
        Handlers::default().element(move |el: &mut El<'_, '_>| {
            if let Some(href) = el.get_attribute("href") {
                if let Some(abs) = resolve_href(href.trim(), &base) {
                    let _ = el.set_attribute("href", &abs);
                }
            }
            Ok(())
        }),
    ));

    rewrite_str(fragment, settings).map_err(|e| Error::Rewrite(e.to_string()))
}

/// `None` for anything that is not a document link: bare anchors, `mailto:`,
/// `javascript:`, and URLs that simply will not resolve.
fn resolve_href(href: &str, base: &Url) -> Option<String> {
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("mailto:")
        || lower.starts_with("javascript:")
        || lower.starts_with("tel:")
        || lower.starts_with("data:")
    {
        return None;
    }
    // The fragment is kept: `--link-style absolute` should preserve a deep
    // link, and the relative rewriter maps it through separately.
    base.join(href).ok().map(|u| u.to_string())
}

/// Replace each `<pre>` with a sentinel. Document order matches
/// [`codeblock::capture`]'s, which is what makes the indices line up.
fn stub_code_blocks(fragment: &str) -> Result<(String, usize)> {
    let counter = Cell::new(0usize);
    let sel = parse_selector("pre")?;

    let settings = RewriteStrSettings::new().append_element_content_handler((
        Cow::Owned(sel),
        Handlers::default().element(|el: &mut El<'_, '_>| {
            let i = counter.get();
            counter.set(i + 1);
            el.replace(&codeblock::sentinel(i), ContentType::Text);
            Ok(())
        }),
    ));

    let out = rewrite_str(fragment, settings).map_err(|e| Error::Rewrite(e.to_string()))?;
    Ok((out, counter.get()))
}

/// Convert HTML to Markdown. The input still contains sentinels, never code.
fn html_to_markdown(html: &str, keep_images: bool) -> Result<String> {
    use htmd::element_handler::Handlers;
    use htmd::options::{
        BulletListMarker, CodeBlockStyle, HeadingStyle, Options,
    };
    use htmd::{Element, HtmlToMarkdownBuilder};

    let mut builder = HtmlToMarkdownBuilder::new().options(Options {
        heading_style: HeadingStyle::Atx,
        bullet_list_marker: BulletListMarker::Dash,
        code_block_style: CodeBlockStyle::Fenced,
        ..Default::default()
    });

    if !keep_images {
        // Images are already pruned; this covers the `--keep-images=false`
        // path if a prune selector is ever mis-scoped.
        builder = builder.add_handler(vec!["img"], |_: &dyn Handlers, _: Element| None);
    }

    builder
        .build()
        .convert(html)
        .map_err(|e| Error::Convert(e.to_string()))
}

/// Full extraction for one page.
pub fn extract_page(
    html: &str,
    url: &Url,
    profile: &Profile,
    opts: &ExtractOptions,
) -> Result<Extracted> {
    let doc = Html::parse_document(html);
    let mut warnings = Vec::new();

    // A profile that matches nothing means either a site redesign or a page
    // that isn't really documentation. Fall back to the generic chain before
    // giving up, and say so — a warning beats both a silent wrong extraction
    // and a hard failure on one odd page out of a thousand.
    let (root, fell_back) = match select_root(&doc, &profile.content_selectors) {
        Some(r) => (r, false),
        None => {
            let generic: Vec<String> = crate::profile::GENERIC_CONTENT_SELECTORS
                .iter()
                .map(|s| s.to_string())
                .collect();
            match select_root(&doc, &generic) {
                Some(r) => (r, true),
                None => {
                    return Err(Error::Other(
                        "no content root matched; the page may be empty or JS-rendered".into(),
                    ))
                }
            }
        }
    };
    if fell_back {
        warnings.push(format!(
            "profile {:?} matched no content root; used the generic chain",
            profile.name
        ));
    }

    let title = extract_title(&root, &doc, profile.title_selector.as_deref());
    let fragment = root.inner_html();

    // 1. Prune noise and label tab groups.
    let transform_dl = opts.transform_dl_tabs && profile.transform_dl_tabs;
    let pruned = prune_fragment(&fragment, &profile.prune_selectors(opts.keep_images), transform_dl)?;

    // 2. Resolve relative hrefs while the page URL is still known.
    let pruned = absolutize_links(&pruned, url)?;

    // 3. Capture code from the *pruned* tree, so pruning cannot shift indices.
    let blocks: Vec<CodeBlock> = codeblock::capture(&pruned);

    // 4. Replace every <pre> with a sentinel.
    let (stubbed, stubbed_count) = stub_code_blocks(&pruned)?;

    if stubbed_count != blocks.len() {
        warnings.push(format!(
            "code block count mismatch: captured {} but stubbed {}",
            blocks.len(),
            stubbed_count
        ));
    }

    // 5. Convert, then put the code back.
    let raw_md = html_to_markdown(&stubbed, opts.keep_images)?;
    let (markdown, unused) = codeblock::splice(&raw_md, &blocks);

    // 6. Never lose code silently: anything whose sentinel never surfaced is
    //    appended rather than dropped.
    let mut markdown = markdown;
    if !unused.is_empty() {
        warnings.push(format!(
            "{} code block(s) were dropped by the converter and re-appended",
            unused.len()
        ));
        markdown.push_str("\n## Code\n\n");
        for i in &unused {
            markdown.push_str(&codeblock::render_fence(&blocks[*i], ""));
            markdown.push('\n');
        }
    }

    if codeblock::contains_sentinel(&markdown) {
        warnings.push("unreplaced code sentinel left in output".into());
    }

    Ok(Extracted {
        markdown,
        title,
        code_blocks: blocks.len(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::builtins;

    fn profile(name: &str) -> Profile {
        builtins().into_iter().find(|p| p.name == name).unwrap()
    }

    fn page(article: &str) -> String {
        format!(
            r#"<html><head><title>T - Site Docs</title></head><body>
            <nav class="md-nav"><a href="/other">Nav link text</a></nav>
            <article class="md-content__inner md-typeset">{article}</article>
            <footer>SITE FOOTER SENTINEL</footer></body></html>"#
        )
    }

    #[test]
    fn extracts_content_and_drops_chrome() {
        let html = page("<h1>Classic Actors</h1><p>Actors are the unit of computation in a system that needs to do more than one thing at a time.</p>");
        let out = extract_page(
            &html,
            &Url::parse("https://pekko.apache.org/docs/pekko/1.7/actors.html").unwrap(),
            &profile("mkdocs"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(out.title.as_deref(), Some("Classic Actors"));
        assert!(out.markdown.contains("# Classic Actors"));
        assert!(out.markdown.contains("unit of computation"));
        assert!(!out.markdown.contains("Nav link text"), "nav leaked in");
        assert!(!out.markdown.contains("SITE FOOTER SENTINEL"), "footer leaked in");
    }

    #[test]
    fn code_survives_end_to_end_byte_for_byte() {
        let code = "val PekkoVersion = \"1.7.1\"\nlibraryDependencies ++= Seq(\n  \"org.apache.pekko\" %% \"pekko-actor\" % PekkoVersion\n)";
        let html = page(&format!(
            r##"<h1>Setup</h1><p>Add the dependency below to your build.</p>
            <pre class="prettyprint"><button class="snippet-button copy-snippet">copy</button><code class="language-scala">{}</code></pre>"##,
            code.replace('"', "&quot;")
        ));

        let out = extract_page(
            &html,
            &Url::parse("https://pekko.apache.org/docs/pekko/1.7/actors.html").unwrap(),
            &profile("mkdocs"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(out.code_blocks, 1);
        assert!(
            out.markdown.contains(&format!("```scala\n{code}\n```")),
            "code was not reproduced verbatim:\n{}",
            out.markdown
        );
        assert!(!out.markdown.contains("copy"), "snippet button text leaked in");
        assert!(out.warnings.is_empty(), "unexpected warnings: {:?}", out.warnings);
    }

    #[test]
    fn paradox_tab_variants_keep_their_labels() {
        let html = page(
            r#"<h1>Actors</h1><p>Here is how you create an actor in each language.</p>
            <dl><dt>Scala</dt><dd><pre><code class="language-scala">val a = system.actorOf(p)</code></pre></dd>
            <dt>Java</dt><dd><pre><code class="language-java">ActorRef a = system.actorOf(p);</code></pre></dd></dl>"#,
        );

        let out = extract_page(
            &html,
            &Url::parse("https://doc.akka.io/libraries/akka-core/current/actors.html").unwrap(),
            &profile("paradox"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(out.code_blocks, 2);
        assert!(out.markdown.contains("**Scala**"), "Scala label lost:\n{}", out.markdown);
        assert!(out.markdown.contains("**Java**"), "Java label lost:\n{}", out.markdown);
        assert!(out.markdown.contains("```scala\nval a = system.actorOf(p)\n```"));
        assert!(out.markdown.contains("```java\nActorRef a = system.actorOf(p);\n```"));
    }

    #[test]
    fn asciidoctor_listing_caption_and_language() {
        let html = page(
            r#"<h1>Developer Tools</h1><p>The devtools module can be added to any project for faster restarts.</p>
            <div class="listingblock"><div class="title">Gradle</div><div class="content">
            <pre class="highlightjs highlight"><code class="language-gradle hljs" data-lang="gradle">dependencies { developmentOnly("x") }</code></pre>
            </div></div>"#,
        );

        let out = extract_page(
            &html,
            &Url::parse("https://docs.spring.io/spring-boot/reference/using/devtools.html").unwrap(),
            &profile("asciidoctor"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert!(out.markdown.contains("**Gradle**"), "caption lost:\n{}", out.markdown);
        assert!(
            out.markdown.contains("```gradle\ndependencies { developmentOnly(\"x\") }\n```"),
            "got:\n{}",
            out.markdown
        );
    }

    #[test]
    fn line_number_gutter_is_not_a_second_code_block() {
        let html = page(
            r#"<h1>Sample</h1><p>A short paragraph that is long enough to be considered real page content by the selector.</p>
            <table class="highlighttable"><tr>
            <td class="linenos"><div class="linenodiv"><pre>1
2</pre></div></td>
            <td class="code"><div class="highlight"><pre><code class="language-java">int a;
int b;</code></pre></div></td>
            </tr></table>"#,
        );

        let out = extract_page(
            &html,
            &Url::parse("https://docs.spring.io/spring-boot/reference/x.html").unwrap(),
            &profile("asciidoctor"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert_eq!(out.code_blocks, 1, "gutter counted as code:\n{}", out.markdown);
        assert!(out.markdown.contains("int a;\nint b;"));
        assert!(!out.markdown.contains("```\n1\n2\n```"), "gutter emitted as code");
    }

    #[test]
    fn images_are_dropped_by_default() {
        let html = page(r#"<h1>X</h1><p>Some prose that is long enough for the content selector to accept this article as the real root.</p><img src="/a.png" alt="ARCH_DIAGRAM_ALT">"#);
        let out = extract_page(
            &html,
            &Url::parse("https://pekko.apache.org/docs/pekko/1.7/x.html").unwrap(),
            &profile("mkdocs"),
            &ExtractOptions::default(),
        )
        .unwrap();
        assert!(!out.markdown.contains("ARCH_DIAGRAM_ALT"));
        assert!(!out.markdown.contains(".png"));
    }

    #[test]
    fn tables_survive() {
        let html = page(
            r#"<h1>Matrix</h1><p>A paragraph of sufficient length so the content root selector accepts this article element.</p>
            <table><thead><tr><th>Lang</th><th>Year</th></tr></thead>
            <tbody><tr><td>Rust</td><td>2010</td></tr></tbody></table>"#,
        );
        let out = extract_page(
            &html,
            &Url::parse("https://pekko.apache.org/docs/pekko/1.7/x.html").unwrap(),
            &profile("mkdocs"),
            &ExtractOptions::default(),
        )
        .unwrap();
        assert!(out.markdown.contains("| Lang | Year |"), "got:\n{}", out.markdown);
        assert!(out.markdown.contains("| Rust | 2010 |"));
    }

    #[test]
    fn relative_links_become_absolute() {
        // Without this the link would be emitted verbatim and point at a file
        // that the output tree never contains.
        // NOTE: r##"..."## — the HTML contains `"#`, which closes r#"..."#.
        let html = page(
            r##"<h1>Actors</h1><p>A paragraph long enough for the content selector to accept this article element.</p>
            <a href="typed.html">Typed</a> <a href="../io.html">IO</a>
            <a href="#local">Local</a> <a href="mailto:x@y.z">Mail</a>"##,
        );
        let out = extract_page(
            &html,
            &Url::parse("https://pekko.apache.org/docs/pekko/1.7/actors.html").unwrap(),
            &profile("mkdocs"),
            &ExtractOptions::default(),
        )
        .unwrap();

        assert!(
            out.markdown.contains("https://pekko.apache.org/docs/pekko/1.7/typed.html"),
            "sibling link not resolved:\n{}",
            out.markdown
        );
        assert!(
            out.markdown.contains("https://pekko.apache.org/docs/pekko/io.html"),
            "parent link not resolved:\n{}",
            out.markdown
        );
        assert!(out.markdown.contains("(#local)"), "anchor should stay local");
        assert!(out.markdown.contains("(mailto:x@y.z)"));
    }

    #[test]
    fn resolve_href_leaves_non_documents_alone() {
        let base = Url::parse("https://x.dev/docs/a.html").unwrap();
        assert_eq!(resolve_href("#top", &base), None);
        assert_eq!(resolve_href("", &base), None);
        assert_eq!(resolve_href("mailto:a@b.c", &base), None);
        assert_eq!(resolve_href("javascript:void(0)", &base), None);
        assert_eq!(resolve_href("tel:+1", &base), None);
        assert_eq!(resolve_href("data:text/plain,x", &base), None);
        // A deep link keeps its fragment.
        assert_eq!(
            resolve_href("b.html#sec", &base).as_deref(),
            Some("https://x.dev/docs/b.html#sec")
        );
    }

    #[test]
    fn clean_title_strips_site_suffix() {
        assert_eq!(clean_title("Classic Actors - Apache Pekko Documentation"), "Classic Actors");
        assert_eq!(clean_title("Developer Tools :: Spring Boot"), "Developer Tools");
        assert_eq!(clean_title("Actors | Akka"), "Actors");
        // A legitimate hyphenated title must not be truncated.
        assert_eq!(clean_title("Client-side Caching"), "Client-side Caching");
    }
}

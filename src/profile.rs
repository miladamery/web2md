use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// `vec!` for `&str` literals landing in a `Vec<String>` field.
macro_rules! strs {
    ($($s:expr),* $(,)?) => { vec![$($s.to_string()),*] };
}

/// A site profile: which selector wins for the content root, and what extra
/// noise to strip. Built-ins cover the three named targets; users can add
/// their own in `web2md.toml` under `[[site]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Host suffixes this profile claims, e.g. `pekko.apache.org`.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Substrings matched against `<meta name="generator">`, lowercased.
    #[serde(default)]
    pub generators: Vec<String>,
    /// Tried in order; first selector with enough text wins.
    #[serde(default)]
    pub content_selectors: Vec<String>,
    #[serde(default)]
    pub title_selector: Option<String>,
    /// Extra noise selectors, on top of [`BASE_PRUNE`].
    #[serde(default)]
    pub prune_selectors: Vec<String>,
    /// Convert `dl > dt` terms into bold labels. This is what keeps the
    /// Java/Scala variants labelled on Paradox pages.
    #[serde(default = "default_true")]
    pub transform_dl_tabs: bool,
}

/// Noise stripped from every site. Deliberately conservative — the
/// `MUST_KEEP` list below is the regression guard against this list growing
/// into real content.
pub const BASE_PRUNE: &[&str] = &[
    "script",
    "style",
    "noscript",
    "template",
    "iframe",
    "svg",
    "canvas",
    "form",
    "input",
    "select",
    "textarea",
    "button",
    "video",
    "audio",
    "source",
    "object",
    "embed",
    "map",
    "area",
    "link",
    "meta",
    "base",
    "picture",
    "nav",
    "[role=navigation]",
    "[role=banner]",
    "[role=contentinfo]",
    "[hidden]",
    "[aria-hidden=true]",
    ".sr-only",
    ".visually-hidden",
    ".screen-reader-text",
    ".skip-link",
    ".skip-to-content",
    // Asciidoctor / Pygments line-number gutters. These matter: the gutter is
    // a *separate* <pre> holding the line numbers, so leaving it in would
    // inject a second code block of bare digits next to every listing.
    ".lineno",
    ".linenos",
    ".linenodiv",
    ".gutter",
    ".line-numbers-rows",
    ".hljs-ln-numbers",
    ".linenums",
    // Copy-to-clipboard affordances. On Pekko/Akka these live *inside* <pre>,
    // so without this a naive text read prepends the word "copy" to the code.
    ".snippet-button",
    ".copy-snippet",
    ".go-to-source",
    ".copy-to-clipboard",
    ".clipboard-btn",
    ".hljs-copy",
    // Heading permalinks and page-action buttons. `a.anchor` is the
    // sbt-paradox-material theme used by Pekko and Akka: every heading carries
    // `<a class="anchor"><span class="anchor-link"></span></a>`, which without
    // this comes out as a stray empty `[](#slug)` before each heading.
    "a.anchor",
    ".anchor-link",
    "a.headerlink",
    ".headerlink",
    "a.toclink",
    ".icon-link",
    ".md-content__button",
    ".md-content__icon",
    // Comment / feedback furniture.
    "#comments",
    ".comments",
    ".disqus",
    ".feedback",
    ".pagination",
    ".pager",
    ".prev-next",
];

/// Content that must survive pruning. Asserted in tests.
pub const MUST_KEEP: &[&str] = &[
    ".admonition",
    ".admonitionblock",
    ".callout",
    ".sidebarblock",
    "details",
    "dl",
    "table",
    "figure",
    "figcaption",
    "kbd",
    "sup",
    "sub",
    "blockquote",
    "ol",
    "ul",
];

impl Profile {
    pub fn prune_selectors(&self, keep_images: bool) -> Vec<String> {
        let mut v: Vec<String> = BASE_PRUNE.iter().map(|s| s.to_string()).collect();
        if !keep_images {
            v.push("img".to_string());
        }
        v.extend(self.prune_selectors.iter().cloned());
        v
    }
}

/// MkDocs Material — Apache Pekko.
fn mkdocs() -> Profile {
    Profile {
        name: "mkdocs".into(),
        hosts: vec![],
        generators: vec!["mkdocs".into()],
        content_selectors: strs![
            "article.md-content__inner",
            "article.md-typeset",
            ".md-content",
        ],
        title_selector: Some("h1".into()),
        prune_selectors: strs![
            ".md-header",
            ".md-sidebar",
            ".md-nav",
            ".md-search",
            ".md-source",
            ".md-footer",
            ".md-dialog",
            ".md-top",
            ".md-skip",
            ".md-banner",
            ".md-announce",
            ".md-tags",
            ".md-source-file",
            ".md-feedback",
            ".md-copyright",
            ".md-nav__source",
            ".md-overlay",
        ],
        transform_dl_tabs: true,
    }
}

/// Lightbend Paradox — Apache Pekko (older) and Akka.
fn paradox() -> Profile {
    Profile {
        name: "paradox".into(),
        hosts: vec![],
        generators: vec!["paradox".into()],
        content_selectors: strs![
            "article#page-content",
            "article.page-content",
            ".page-content",
        ],
        title_selector: Some("h1".into()),
        prune_selectors: strs![
            "#sidebar",
            ".sidebar",
            "#site-header",
            ".site-header",
            "#site-footer",
            ".site-footer",
            ".page-nav",
            ".page-toc",
            "#toc",
            ".toc",
            ".edit-page",
            ".edit-this-page",
            ".source-link",
            ".social",
            ".algolia",
            ".search",
            "#search",
            ".version-selector",
            ".banner",
            ".callout-banner",
            ".navigation",
        ],
        transform_dl_tabs: true,
    }
}

/// Asciidoctor — Spring.
fn asciidoctor() -> Profile {
    Profile {
        name: "asciidoctor".into(),
        hosts: vec![],
        generators: vec!["asciidoctor".into()],
        content_selectors: strs![
            "article.doc",
            "#content",
            ".sect1",
        ],
        title_selector: Some("h1".into()),
        // NOTE: `#header` is deliberately absent. On older Asciidoctor output
        // `#header` wraps the document title, so pruning it would delete the
        // page's own H1. The breadcrumb container is the real noise here.
        prune_selectors: strs![
            ".breadcrumbs-container",
            ".breadcrumbs",
            "#footer",
            "#toc",
            ".toc2",
            ".toc",
            "#search",
            ".edit-this-page",
            ".contributors",
            ".copyright",
            ".nav",
            ".version-selector",
            "table.highlighttable td.linenos",
        ],
        transform_dl_tabs: false,
    }
}

/// Antora — the Akka SDK docs (`doc.akka.io/concepts/**`), and Antora-based
/// docs generally.
fn antora() -> Profile {
    Profile {
        name: "antora".into(),
        // No host: `doc.akka.io` serves Paradox under `/libraries/` and Antora
        // everywhere else, so this profile is reached through the generator
        // meta rather than a host claim. See `pipeline::profile_for`.
        hosts: vec![],
        generators: vec!["antora".into()],
        content_selectors: strs!["article.doc", "main.article", ".doc"],
        title_selector: Some("h1".into()),
        prune_selectors: strs![
            ".nav",
            ".navigation",
            ".toc",
            ".toc-menu",
            ".sidebar",
            "#header",
            "#footer",
            ".toolbar",
            ".page-toolbar",
            ".breadcrumbs",
            ".edit-this-page",
            ".source-toolbox",
            ".pagination",
            ".version-selector",
            ".announcement",
        ],
        transform_dl_tabs: false,
    }
}

/// The fallback content chain, ordered most-specific to least.
///
/// This is the generic profile's selector list *and* the safety net used when
/// a site profile's own selectors all miss: a site redesign should degrade to
/// a less clean extraction, not to nothing extracted at all.
pub const GENERIC_CONTENT_SELECTORS: &[&str] = &[
    "article",
    "main",
    "[role=main]",
    "#content",
    "#main-content",
    ".content",
    ".document",
    ".doc",
    ".markdown-body",
    ".rst-content",
    ".theme-doc-markdown",
    "body",
];

/// Fallback for anything unrecognised.
fn generic() -> Profile {
    Profile {
        name: "generic".into(),
        hosts: vec![],
        generators: vec![],
        content_selectors: GENERIC_CONTENT_SELECTORS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        title_selector: Some("h1".into()),
        prune_selectors: strs![],
        transform_dl_tabs: true,
    }
}

/// Built-in profiles, in resolution order.
pub fn builtins() -> Vec<Profile> {
    let mut mk = mkdocs();
    mk.hosts = vec!["pekko.apache.org".into()];

    let mut px = paradox();
    px.hosts = vec!["doc.akka.io".into(), "akka.io".into()];

    let mut ad = asciidoctor();
    ad.hosts = vec!["docs.spring.io".into()];

    vec![mk, px, ad, antora(), generic()]
}

/// Resolve by host alone.
///
/// Split out so the caller can skip parsing the page for its `<meta
/// name="generator">` when the host has already decided — which, for all three
/// built-in targets, it has.
pub fn resolve_by_host<'a>(profiles: &'a [Profile], url: &url::Url) -> Option<&'a Profile> {
    let host = url.host_str().unwrap_or_default();
    profiles
        .iter()
        .find(|p| p.hosts.iter().any(|h| host == h || host.ends_with(&format!(".{h}"))))
}

/// Resolve by `<meta name="generator">`, matched as a case-insensitive
/// substring so `"Antora 3.1.15"` finds the `antora` profile.
pub fn resolve_by_generator<'a>(
    profiles: &'a [Profile],
    generator: Option<&str>,
) -> Option<&'a Profile> {
    let gen = generator?.to_ascii_lowercase();
    profiles
        .iter()
        .find(|p| p.generators.iter().any(|g| gen.contains(g.as_str())))
}

/// Resolve a profile: explicit name > host suffix > generator meta > generic.
pub fn resolve<'a>(
    profiles: &'a [Profile],
    url: &url::Url,
    generator: Option<&str>,
    explicit: Option<&str>,
) -> &'a Profile {
    if let Some(name) = explicit {
        if let Some(p) = profiles.iter().find(|p| p.name == name) {
            return p;
        }
    }

    if let Some(p) = resolve_by_host(profiles, url) {
        return p;
    }

    if let Some(p) = resolve_by_generator(profiles, generator) {
        return p;
    }

    profiles
        .iter()
        .find(|p| p.name == "generic")
        .unwrap_or(&profiles[profiles.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn resolves_by_host_then_generator_then_generic() {
        let ps = builtins();

        let pekko = Url::parse("https://pekko.apache.org/docs/pekko/1.7/actors.html").unwrap();
        assert_eq!(resolve(&ps, &pekko, None, None).name, "mkdocs");

        let akka = Url::parse("https://doc.akka.io/libraries/akka-core/current/actors.html").unwrap();
        assert_eq!(resolve(&ps, &akka, None, None).name, "paradox");

        let spring = Url::parse("https://docs.spring.io/spring-boot/reference/index.html").unwrap();
        assert_eq!(resolve(&ps, &spring, None, None).name, "asciidoctor");

        // Unknown host falls through to the generator meta.
        let other = Url::parse("https://example.dev/docs/x.html").unwrap();
        assert_eq!(
            resolve(&ps, &other, Some("mkdocs-1.6.1"), None).name,
            "mkdocs"
        );
        assert_eq!(resolve(&ps, &other, None, None).name, "generic");

        // Explicit name always wins.
        assert_eq!(resolve(&ps, &pekko, None, Some("asciidoctor")).name, "asciidoctor");
    }

    #[test]
    fn resolves_antora_by_generator_not_host() {
        // `doc.akka.io` claims Paradox by host, but its SDK half is Antora.
        // Host resolution cannot tell them apart, so the generator has to win
        // there — see `pipeline::profile_for`.
        let ps = builtins();
        let sdk = Url::parse("https://doc.akka.io/concepts/ai-agents.html").unwrap();
        assert_eq!(resolve_by_host(&ps, &sdk).unwrap().name, "paradox");
        assert_eq!(
            resolve_by_generator(&ps, Some("Antora 3.1.15")).unwrap().name,
            "antora"
        );
        // An unknown generator must not be mistaken for a match.
        assert!(resolve_by_generator(&ps, Some("Hugo 0.120")).is_none());
        assert!(resolve_by_generator(&ps, None).is_none());
    }

    #[test]
    fn images_pruned_unless_kept() {
        let p = builtins().pop().unwrap(); // generic
        assert!(p.prune_selectors(false).iter().any(|s| s == "img"));
        assert!(!p.prune_selectors(true).iter().any(|s| s == "img"));
    }

    #[test]
    fn asciidoctor_does_not_prune_header() {
        // Regression guard: #header holds the document title on older
        // Asciidoctor output.
        let p = builtins().into_iter().find(|p| p.name == "asciidoctor").unwrap();
        assert!(!p.prune_selectors(false).iter().any(|s| s == "#header"));
    }
}

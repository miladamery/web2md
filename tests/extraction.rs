//! Fixture tests against real saved pages from the three target sites.
//!
//! These are the tests that matter most: the whole point of the tool is that
//! code arrives intact. Everything here runs offline against `tests/fixtures`.

use scraper::{Html, Selector};
use url::Url;

use web2md::extract::{extract_page, ExtractOptions};
use web2md::profile::{builtins, Profile};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn profile(name: &str) -> Profile {
    builtins()
        .into_iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no profile {name}"))
}

struct Case {
    fixture: &'static str,
    profile: &'static str,
    url: &'static str,
    /// A string from the site chrome that must never reach the Markdown.
    chrome: &'static str,
}

const CASES: &[Case] = &[
    Case {
        fixture: "pekko-actors.html",
        profile: "mkdocs",
        url: "https://pekko.apache.org/docs/pekko/current/actors.html",
        chrome: "Permanent link",
    },
    Case {
        fixture: "akka-actors.html",
        profile: "paradox",
        url: "https://doc.akka.io/libraries/akka-core/current/actors.html",
        chrome: "Akka core",
    },
    Case {
        fixture: "spring-devtools.html",
        profile: "asciidoctor",
        url: "https://docs.spring.io/spring-boot/reference/using/devtools.html",
        chrome: "Edit this Page",
    },
];

fn run(case: &Case) -> web2md::extract::Extracted {
    let html = fixture(case.fixture);
    extract_page(
        &html,
        &Url::parse(case.url).unwrap(),
        &profile(case.profile),
        &ExtractOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", case.fixture))
}

/// Every `<pre>` in the document, read with an independent DOM walk.
///
/// `codeblock::capture` uses `scraper` too, but through the `lol_html`-rewritten
/// fragment. Reading the fixture directly is a second opinion on the same
/// question: how many code blocks are actually on this page.
fn count_pre(html: &str) -> usize {
    let doc = Html::parse_document(html);
    doc.select(&Selector::parse("pre").unwrap()).count()
}

/// The code text of one `<pre>`, using `code` when present.
///
/// The copy-to-clipboard button is a *sibling* of `<code>` inside `<pre>`, so
/// this must not fall back to `pre.text()` when a `<code>` child exists.
fn code_text(pre: &scraper::ElementRef<'_>) -> Option<String> {
    let code = pre.select(&Selector::parse("code").unwrap()).next()?;
    let mut text = String::new();
    for node in code.descendants() {
        let Some(t) = node.value().as_text() else { continue };
        // Skip text inside a copy-to-clipboard affordance, in case a generator
        // ever moves it inside <code>. Note this inspects each *ancestor*
        // itself — `ancestor.select(..)` would match its descendants and skip
        // every text node in the block.
        let in_affordance = node.ancestors().filter_map(scraper::ElementRef::wrap).any(|a| {
            let v = a.value();
            v.name() == "button"
                || v.classes().any(|c| c == "snippet-button" || c == "copy-snippet")
        });
        if !in_affordance {
            text.push_str(t);
        }
    }
    Some(text)
}

/// The body of every fenced code block, dedented by the fence's own
/// indentation.
///
/// That dedent is not a convenience: CommonMark removes up to N leading spaces
/// from each line of a fence indented N spaces, so this is what any Markdown
/// renderer — and therefore any consumer of these files — actually sees. Code
/// nested in a list item is indented by the item's content column, which is
/// correct Markdown but not part of the code.
fn fenced_bodies(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    // (indent, fence length, body lines)
    let mut open: Option<(usize, usize, Vec<String>)> = None;

    for line in md.lines() {
        match &mut open {
            Some((indent, len, lines)) => {
                let trimmed = line.trim_start();
                let run = trimmed.chars().take_while(|&c| c == '`').count();
                if run >= *len && trimmed[run..].trim().is_empty() {
                    let pad = " ".repeat(*indent);
                    out.push(
                        lines
                            .iter()
                            .map(|l| l.strip_prefix(pad.as_str()).unwrap_or(l).to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                    open = None;
                } else {
                    lines.push(line.to_string());
                }
            }
            None => {
                let indent = line.len() - line.trim_start_matches(' ').len();
                // Any indentation counts here. At top level CommonMark would
                // call 4+ spaces an indented code block, but a fence nested in
                // a list item is indented by the item's content column and is
                // still a fence — and telling those apart needs a full
                // container-aware parser, which is not what is under test.
                let trimmed = &line[indent..];
                let run = trimmed.chars().take_while(|&c| c == '`').count();
                if run >= 3 {
                    open = Some((indent, run, Vec::new()));
                }
            }
        }
    }

    out
}

#[test]
fn every_code_block_on_the_page_survives() {
    for case in CASES {
        let html = fixture(case.fixture);
        let doc = Html::parse_document(&html);
        let out = run(case);

        // Blocks removed with the chrome (a `<pre>` inside the nav, say) are
        // legitimately absent, so the assertion is one-sided: we never emit
        // more than the page has.
        assert!(
            out.code_blocks <= count_pre(&html),
            "{}: emitted {} blocks, page has only {}",
            case.fixture,
            out.code_blocks,
            count_pre(&html)
        );

        let bodies = fenced_bodies(&out.markdown);
        assert_eq!(
            bodies.len(),
            out.code_blocks,
            "{}: {} fenced blocks in the output for {} captured",
            case.fixture,
            bodies.len(),
            out.code_blocks
        );

        // And the code itself, block by block, byte for byte.
        let mut checked = 0;
        for pre in doc.select(&Selector::parse("pre").unwrap()) {
            let Some(expected) = code_text(&pre) else {
                continue;
            };
            let expected = expected.trim_end_matches('\n');
            if expected.trim().is_empty() {
                continue;
            }
            assert!(
                bodies.iter().any(|b| b == expected),
                "{}: this code block is missing or altered:\n{}",
                case.fixture,
                &expected[..expected.len().min(400)]
            );
            checked += 1;
        }
        assert!(
            checked >= out.code_blocks,
            "{}: checked {checked} blocks but emitted {}",
            case.fixture,
            out.code_blocks
        );
    }
}

#[test]
fn no_copy_button_text_leaks_into_the_code() {
    for case in CASES {
        let out = run(case);
        // The affordance renders the literal word "copy" inside <pre>. If it
        // leaked, it would appear at the start of a fence's first line.
        for (i, line) in out.markdown.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                let next = out.markdown.lines().nth(i + 1).unwrap_or("");
                assert!(
                    !next.trim_start().starts_with("copy"),
                    "{}: code block at line {i} starts with the copy button: {next:?}",
                    case.fixture
                );
            }
        }
    }
}

#[test]
fn chrome_never_reaches_the_markdown() {
    for case in CASES {
        let out = run(case);
        assert!(
            !out.markdown.contains(case.chrome),
            "{}: site chrome {:?} leaked into the output",
            case.fixture,
            case.chrome
        );
    }
}

#[test]
fn titles_come_from_the_page_not_the_site_header() {
    // Akka's site header carries `<h1><a href="index.html">Akka core</a></h1>`
    // on every page; a document-wide `h1` query titles the whole site with it.
    for case in CASES {
        let out = run(case);
        let title = out.title.as_deref().unwrap_or("");
        assert!(!title.is_empty(), "{}: no title", case.fixture);
        assert_ne!(title, "Akka core", "{}: picked up the site header", case.fixture);
        assert_ne!(title, "Apache Pekko", "{}: picked up the site header", case.fixture);
    }
}

#[test]
fn headings_keep_their_text_and_lose_their_permalinks() {
    for case in CASES {
        let out = run(case);
        // An anchor link in body prose is legitimate; one attached to a
        // heading is the permalink affordance leaking through.
        for line in out.markdown.lines().filter(|l| l.starts_with('#')) {
            assert!(
                !line.contains("](#"),
                "{}: heading permalink survived: {line:?}",
                case.fixture
            );
        }
        // ...but the heading text is still there.
        let h1 = out
            .markdown
            .lines()
            .find(|l| l.starts_with("# "))
            .unwrap_or("");
        assert!(h1.len() > 3, "{}: empty h1: {h1:?}", case.fixture);
    }
}

#[test]
fn tab_variants_keep_their_labels() {
    // Pekko and Akka are the pages where the Java/Scala/Maven/Gradle variants
    // *are* the content; an unlabelled run of near-identical blocks is useless.
    for case in [&CASES[0], &CASES[1]] {
        let out = run(case);
        for label in ["**sbt**", "**Maven**", "**Gradle**"] {
            assert!(
                out.markdown.contains(label),
                "{}: tab label {label} is missing",
                case.fixture
            );
        }
    }
}

#[test]
fn spring_listing_captions_survive() {
    let out = run(&CASES[2]);
    // Asciidoctor puts the listing's name in a sibling `div.title`.
    assert!(
        out.markdown.contains("**Maven**") || out.markdown.contains("**Gradle**"),
        "no listing caption found:\n{}",
        &out.markdown[..out.markdown.len().min(800)]
    );
}

#[test]
fn prose_and_structure_survive() {
    for case in CASES {
        let out = run(case);
        assert!(out.markdown.contains("## "), "{}: no h2", case.fixture);
        assert!(
            out.markdown.lines().any(|l| l.starts_with("- ") || l.starts_with("1. ")),
            "{}: no list",
            case.fixture
        );
        assert!(
            out.markdown.len() > 5_000,
            "{}: suspiciously short output ({} bytes)",
            case.fixture,
            out.markdown.len()
        );
    }
}

#[test]
fn images_are_dropped() {
    for case in CASES {
        let out = run(case);
        assert!(!out.markdown.contains("!["), "{}: image syntax present", case.fixture);
    }
}

#[test]
fn output_is_deterministic() {
    // Two runs over the same fixture must agree exactly; anything else means a
    // HashMap iteration order is leaking into the output.
    for case in CASES {
        assert_eq!(run(case).markdown, run(case).markdown, "{}", case.fixture);
    }
}

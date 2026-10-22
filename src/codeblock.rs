//! Code-block fidelity.
//!
//! The contract: a fenced block's body must be byte-identical to the text the
//! browser renders inside `<pre>`. We get there by removing every `<pre>` from
//! the HTML *before* it reaches the Markdown converter and re-inserting the
//! captured text afterwards, so the converter can never reflow, escape, or
//! indent code. Everything else in this module exists to make that swap exact.

use std::sync::OnceLock;

use regex::Regex;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};

/// Sentinel delimiters. Private-use codepoints cannot occur in real page text,
/// and neither they nor the ASCII between them are characters the Markdown
/// converter escapes, so the token survives conversion intact.
const SENTINEL_OPEN: char = '\u{E000}';
const SENTINEL_CLOSE: char = '\u{E001}';

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeBlock {
    /// Normalized language tag; empty when undetectable.
    pub lang: String,
    /// Verbatim code text, newline-normalized only.
    pub text: String,
}

/// Class tokens injected by syntax highlighters. They name the *highlighter*,
/// never the language, so they must not be mistaken for one.
const HIGHLIGHTER_CLASSES: &[&str] = &[
    "hljs",
    "highlight",
    "prettyprint",
    "sourcecode",
    "chroma",
    "highlighter-rouge",
    "codehilite",
];

/// Languages we accept from a bare class token (e.g. `class="scala"`) when no
/// `language-*` marker is present.
const KNOWN_LANGS: &[&str] = &[
    "scala", "java", "kotlin", "groovy", "gradle", "bash", "shell", "sh", "console", "xml",
    "html", "json", "yaml", "yml", "properties", "sql", "python", "javascript", "typescript",
    "c", "cpp", "csharp", "go", "rust", "diff", "ini", "http", "css", "ruby", "php", "swift",
    "perl", "haskell", "erlang", "elixir", "clojure", "lisp", "r", "matlab", "julia", "dart",
    "lua", "protobuf", "dockerfile", "makefile", "toml", "text", "plaintext",
];

/// Classes whose text must never reach the code body: line-number gutters and
/// copy/source affordances. These are normally removed by the prune pass
/// before capture; repeating the check here is defence in depth, so that a
/// missing or mis-scoped prune rule cannot inject a column of digits or the
/// literal word "copy" into someone's code sample.
const SKIP_TEXT_CLASSES: &[&str] = &[
    // gutters
    "lineno",
    "linenos",
    "linenodiv",
    "gutter",
    "line-numbers-rows",
    "hljs-ln-numbers",
    "linenums",
    "line-number",
    // copy / source affordances
    "snippet-button",
    "copy-snippet",
    "go-to-source",
    "copy-to-clipboard",
    "clipboard-btn",
    "hljs-copy",
];

pub fn sentinel(index: usize) -> String {
    format!("{SENTINEL_OPEN}W2MDCODE{index}{SENTINEL_CLOSE}")
}

pub fn sentinel_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x{E000}W2MDCODE(\d+)\x{E001}").expect("valid sentinel regex"))
}

/// True if the string still contains an unreplaced sentinel. Used by tests and
/// by the pipeline's self-check.
pub fn contains_sentinel(s: &str) -> bool {
    sentinel_regex().is_match(s)
}

/// Capture every `<pre>` in document order from already-pruned content HTML.
///
/// Document order matters: it is what lets the rewrite pass stub the Nth
/// `<pre>` with the Nth captured block.
pub fn capture(html: &str) -> Vec<CodeBlock> {
    let doc = Html::parse_fragment(html);
    let pre_sel = Selector::parse("pre").expect("valid selector");
    let code_sel = Selector::parse("code").expect("valid selector");

    doc.select(&pre_sel)
        .map(|pre| {
            let code = pre.select(&code_sel).next();
            let lang = detect_lang(code, &pre).unwrap_or_default();
            // Prefer <code>'s text. On Pekko/Akka the <pre> also contains a
            // "copy" <button> and a "go to source" <a> as *siblings* of
            // <code>, so reading the <pre> directly would prepend the literal
            // word "copy" to the code.
            let text = match code {
                Some(c) => extract_text(c),
                None => extract_text(pre),
            };
            CodeBlock {
                lang,
                text: normalize_text(&text),
            }
        })
        .collect()
}

/// Concatenate descendant text in order, skipping gutters and mapping `<br>`
/// to a newline. Reading *through* highlight `<span>`s is how highlighted
/// markup gets flattened without a separate unwrapping step.
fn extract_text(el: ElementRef<'_>) -> String {
    let mut out = String::new();
    walk(el, &mut out);
    out
}

fn walk(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&t.text),
            Node::Element(e) => {
                if e.classes().any(|c| SKIP_TEXT_CLASSES.contains(&c)) {
                    continue;
                }
                if e.name() == "br" {
                    out.push('\n');
                    continue;
                }
                if let Some(c) = ElementRef::wrap(child) {
                    walk(c, out);
                }
            }
            _ => {}
        }
    }
}

fn detect_lang(code: Option<ElementRef<'_>>, pre: &ElementRef<'_>) -> Option<String> {
    if let Some(c) = code {
        if let Some(l) = class_language(c.value()) {
            return Some(l);
        }
        if let Some(l) = c.value().attr("data-lang") {
            return Some(normalize_lang(l));
        }
    }
    if let Some(l) = pre.value().attr("data-lang") {
        return Some(normalize_lang(l));
    }
    if let Some(l) = class_language(pre.value()) {
        return Some(l);
    }
    if let Some(c) = code {
        if let Some(l) = known_lang_token(c.value()) {
            return Some(l);
        }
    }
    known_lang_token(pre.value())
}

fn class_language(el: &scraper::node::Element) -> Option<String> {
    el.classes()
        .find_map(|c| c.strip_prefix("language-"))
        .map(normalize_lang)
        .filter(|s| !s.is_empty())
}

fn known_lang_token(el: &scraper::node::Element) -> Option<String> {
    el.classes()
        .filter(|c| !HIGHLIGHTER_CLASSES.contains(&c.to_ascii_lowercase().as_str()))
        .find(|c| KNOWN_LANGS.contains(&c.to_ascii_lowercase().as_str()))
        .map(normalize_lang)
        .filter(|s| !s.is_empty())
}

fn normalize_lang(raw: &str) -> String {
    let l = raw.trim().trim_matches(|c: char| c == '"' || c == '\'').to_ascii_lowercase();
    match l.as_str() {
        "sh" | "shell" | "zsh" | "console-shell" => "bash".to_string(),
        "yml" => "yaml".to_string(),
        "c#" | "cs" => "csharp".to_string(),
        "objective-c" | "objc" => "objectivec".to_string(),
        // A fence tag of `text` adds nothing; emit a bare fence instead.
        "text" | "plaintext" | "plain" | "none" | "txt" => String::new(),
        _ => l,
    }
}

/// Newline normalization only. Whitespace, tabs, and interior blank lines are
/// left exactly as authored — ASCII-art alignment and Makefile tabs must
/// survive, and trailing spaces can be significant in Markdown-adjacent code.
fn normalize_text(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    // A newline immediately after the opening tag is an HTML formatting
    // artifact, not content.
    let s = s.strip_prefix('\n').unwrap_or(&s);
    let trimmed = s.trim_end_matches(['\n', ' ', '\t']);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Longest run of consecutive backticks anywhere in the body. The fence must
/// be longer than this so code that itself contains ``` cannot close it early.
fn longest_backtick_run(s: &str) -> usize {
    let (mut best, mut cur) = (0usize, 0usize);
    for c in s.chars() {
        if c == '`' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// Render a fenced block, applying `carry` (list indent or blockquote prefix)
/// to every non-empty line.
pub fn render_fence(block: &CodeBlock, carry: &str) -> String {
    let n = std::cmp::max(3, longest_backtick_run(&block.text) + 1);
    let fence: String = "`".repeat(n);

    let mut out = String::new();
    out.push_str(carry);
    out.push_str(&fence);
    out.push_str(&block.lang);
    out.push('\n');
    // `text` carries a single trailing newline by construction; splitting on it
    // verbatim would emit a spurious blank line before the closing fence.
    let body = block.text.strip_suffix('\n').unwrap_or(&block.text);
    if !body.is_empty() {
        for line in body.split('\n') {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(carry);
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str(carry);
    out.push_str(&fence);
    out.push('\n');
    out
}

/// Length of a list-marker prefix at the start of `s`, spaces included.
///
/// `"- "` -> 2, `"-   "` -> 4, `"12. "` -> 4. This is the column the item's
/// content starts at, which is where a continuation block has to line up.
fn list_marker_len(s: &str) -> Option<usize> {
    match s.chars().next()? {
        // NOTE: the trailing space is what distinguishes the marker "- " from
        // a stray hyphen, so a bare "-" must not match.
        '-' | '*' | '+' => {
            let rest = &s[1..];
            rest.starts_with(' ')
                .then(|| 1 + rest.chars().take_while(|&c| c == ' ').count())
        }
        c if c.is_ascii_digit() => {
            let digits = s.chars().take_while(char::is_ascii_digit).count();
            let rest = &s[digits..];
            (rest.starts_with(". ") || rest.starts_with(") ")).then(|| {
                digits + 1 + rest[1..].chars().take_while(|&c| c == ' ').count()
            })
        }
        _ => None,
    }
}

/// What precedes a sentinel on its own line, and must be re-applied to the
/// fence so code inside a list item or blockquote stays in its container.
///
/// `prefix` must be taken from the markdown *as the converter emitted it*. A
/// prefix read from the already-spliced output is wrong whenever the converter
/// put two blocks on one line: the first block's fence then becomes the second
/// block's "indentation".
fn carry_for_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_start();

    if trimmed.is_empty() {
        // Indentation and nothing else. This is the converter's own list
        // continuation indent, and it must be reproduced on both the fence and
        // the body — Markdown strips up to that many leading spaces from each
        // line of a fenced block, so the code itself is unaffected.
        return prefix.to_string();
    }

    // The sentinel shares its line with real content.
    if list_marker_len(trimmed).is_some() {
        return " ".repeat(prefix.chars().count());
    }
    // "> ", "> > " -> keep the quote nesting.
    if trimmed.starts_with('>') {
        return prefix.to_string();
    }
    // Token was inlined into a sentence; start a fresh block.
    String::new()
}

/// Replace sentinels with their fenced blocks.
///
/// Returns the spliced markdown plus the indices of any blocks that never
/// appeared — the caller reports those rather than dropping code silently.
pub fn splice(markdown: &str, blocks: &[CodeBlock]) -> (String, Vec<usize>) {
    let re = sentinel_regex();
    let mut out = String::with_capacity(markdown.len() + blocks.len() * 256);
    let mut used = vec![false; blocks.len()];
    let mut cursor = 0usize;

    for caps in re.captures_iter(markdown) {
        let whole = caps.get(0).expect("group 0 always present");
        let idx: usize = match caps.get(1).and_then(|m| m.as_str().parse().ok()) {
            Some(i) => i,
            None => continue,
        };
        if idx >= blocks.len() {
            continue;
        }

        out.push_str(&markdown[cursor..whole.start()]);

        // The sentinel's line prefix, read from the *original* markdown: `out`
        // may already hold a fence where a block the converter had inlined
        // ahead of this one used to be, and that would read as indentation.
        // The two offsets are not interchangeable — `out` is longer than
        // `markdown[..whole.start()]` by everything spliced so far.
        let md_line_start = markdown[..whole.start()].rfind('\n').map_or(0, |i| i + 1);
        let carry = carry_for_prefix(&markdown[md_line_start..whole.start()]);

        // Drop the partial line the sentinel was on, then guarantee exactly
        // one blank line before the fence.
        let line_start = out.rfind('\n').map_or(0, |i| i + 1);
        out.truncate(line_start);
        while out.ends_with('\n') {
            out.pop();
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }

        out.push_str(&render_fence(&blocks[idx], &carry));
        used[idx] = true;
        cursor = whole.end();
        // The sentinel occupied its own line; swallow that line's newline so
        // the fence is not followed by a stray blank line.
        if markdown[cursor..].starts_with('\n') {
            cursor += 1;
        }
    }

    out.push_str(&markdown[cursor..]);

    let unused = used
        .iter()
        .enumerate()
        .filter(|(_, u)| !**u)
        .map(|(i, _)| i)
        .collect();

    (out, unused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_from_every_marker() {
        let cases = [
            (r#"<pre><code class="language-scala">x</code></pre>"#, "scala"),
            (r#"<pre><code class="language-xml hljs">x</code></pre>"#, "xml"),
            (r#"<pre><code data-lang="gradle">x</code></pre>"#, "gradle"),
            (r#"<pre data-lang="java"><code>x</code></pre>"#, "java"),
            (r#"<pre class="language-kotlin"><code>x</code></pre>"#, "kotlin"),
            (r#"<pre class="highlight"><code class="python">x</code></pre>"#, "python"),
            // A highlighter class must not be mistaken for a language.
            (r#"<pre class="highlightjs highlight"><code class="hljs">x</code></pre>"#, ""),
        ];
        for (html, want) in cases {
            assert_eq!(capture(html)[0].lang, want, "for {html}");
        }
    }

    #[test]
    fn snippet_button_inside_pre_is_not_captured() {
        // The real Pekko/Akka shape: the copy button is a sibling of <code>.
        // NOTE: r##"..."## — the HTML contains `"#`, which would close r#"..."#.
        let html = r##"<pre class="prettyprint"><button class="snippet-button copy-snippet">copy</button><a class="snippet-button go-to-source" href="#">src</a><code class="language-scala">val x = 1</code></pre>"##;
        assert_eq!(capture(html)[0].text, "val x = 1\n");
    }

    #[test]
    fn snippet_button_is_stripped_even_without_a_code_child() {
        // Fallback path: no <code>, so we read the <pre> itself.
        let html = r#"<pre><button class="snippet-button">copy</button>plain code</pre>"#;
        assert_eq!(capture(html)[0].text, "plain code\n");
    }

    #[test]
    fn preserves_indentation_tabs_and_interior_blank_lines() {
        let src = "def f:\n\tif x:\n\t\tpass\n\n\treturn 1";
        let html = format!(r#"<pre><code class="language-python">{src}</code></pre>"#);
        let got = &capture(&html)[0].text;
        assert!(got.contains('\t'), "tabs must survive: {got:?}");
        assert!(got.contains("\n\n"), "interior blank line must survive");
        assert!(got.starts_with("def f:"), "no leading newline artifact");
    }

    #[test]
    fn decodes_entities_through_the_parser() {
        let html = r#"<pre><code class="language-xml">&lt;dependency&gt;</code></pre>"#;
        assert_eq!(capture(html)[0].text, "<dependency>\n");
    }

    #[test]
    fn unwraps_highlight_spans_into_plain_text() {
        let html = r#"<pre><code class="language-scala"><span class="k">val</span> <span class="n">x</span> = <span class="mi">1</span></code></pre>"#;
        assert_eq!(capture(html)[0].text, "val x = 1\n");
    }

    #[test]
    fn skips_gutter_columns() {
        let html = r#"<pre><code class="language-java"><span class="lineno">1</span>int x;</code></pre>"#;
        assert_eq!(capture(html)[0].text, "int x;\n");
    }

    #[test]
    fn fence_grows_past_embedded_backticks() {
        let b = CodeBlock {
            lang: "md".into(),
            text: "```\ninner\n```\n".into(),
        };
        let f = render_fence(&b, "");
        assert!(f.starts_with("````md\n"), "got {f:?}");
        assert!(f.contains("```\ninner\n```"));
        assert!(f.trim_end().ends_with("````"));
    }

    #[test]
    fn fence_is_three_backticks_normally() {
        let b = CodeBlock {
            lang: "scala".into(),
            text: "val x = 1\n".into(),
        };
        assert_eq!(render_fence(&b, ""), "```scala\nval x = 1\n```\n");
    }

    #[test]
    fn empty_language_emits_bare_fence() {
        let b = CodeBlock {
            lang: String::new(),
            text: "x\n".into(),
        };
        assert!(render_fence(&b, "").starts_with("```\n"));
    }

    #[test]
    fn splice_replaces_sentinels_in_order() {
        let blocks = vec![
            CodeBlock { lang: "scala".into(), text: "a\n".into() },
            CodeBlock { lang: "java".into(), text: "b\n".into() },
        ];
        let md = format!("Intro\n\n{}\n\nMiddle\n\n{}\n\nEnd\n", sentinel(0), sentinel(1));
        let (out, unused) = splice(&md, &blocks);
        assert!(unused.is_empty());
        assert!(out.contains("```scala\na\n```"));
        assert!(out.contains("```java\nb\n```"));
        assert!(out.contains("Intro"));
        assert!(out.contains("Middle"));
        assert!(out.contains("End"));
        assert!(!contains_sentinel(&out));
    }

    #[test]
    fn an_inlined_block_does_not_inherit_a_carry() {
        // Regression, and the reason `splice` reads the prefix from the
        // original markdown: `htmd` renders two adjacent `<pre>`s on *one*
        // line separated by a space. Splicing the first left that space as the
        // second's line prefix, and carrying it prepended a space to every line
        // of the second block's code.
        let blocks = vec![
            CodeBlock { lang: "scala".into(), text: "val a = 1\n".into() },
            CodeBlock { lang: "java".into(), text: "int b = 2;\n".into() },
        ];
        let md = format!("{} {}\n", sentinel(0), sentinel(1));
        let (out, _) = splice(&md, &blocks);
        assert!(out.contains("```java\nint b = 2;\n```"), "got {out:?}");
    }

    #[test]
    fn a_genuinely_indented_block_keeps_its_indent() {
        // `htmd` indents a block nested in a list item by the item's content
        // column; the fence and the body both need it.
        let blocks = vec![CodeBlock { lang: "scala".into(), text: "a\n".into() }];
        let md = format!("-   an item\n\n    {}\n", sentinel(0));
        let (out, _) = splice(&md, &blocks);
        assert!(out.contains("    ```scala\n    a\n    ```"), "got {out:?}");
    }

    #[test]
    fn list_marker_widths() {
        assert_eq!(list_marker_len("- "), Some(2));
        assert_eq!(list_marker_len("-   x"), Some(4));
        assert_eq!(list_marker_len("* x"), Some(2));
        assert_eq!(list_marker_len("1. x"), Some(3));
        assert_eq!(list_marker_len("12) x"), Some(4));
        // Not markers: a hyphen without a space, a heading, a fence.
        assert_eq!(list_marker_len("-x"), None);
        assert_eq!(list_marker_len("## Heading"), None);
        assert_eq!(list_marker_len("```"), None);
    }

    #[test]
    fn splice_indents_code_inside_a_list_item() {
        let blocks = vec![CodeBlock { lang: "scala".into(), text: "a\nb\n".into() }];
        let md = format!("- {}\n", sentinel(0));
        let (out, _) = splice(&md, &blocks);
        assert!(out.contains("  ```scala\n  a\n  b\n  ```"), "got {out:?}");
    }

    #[test]
    fn splice_keeps_blockquote_prefix() {
        let blocks = vec![CodeBlock { lang: "scala".into(), text: "a\n".into() }];
        let md = format!("> {}\n", sentinel(0));
        let (out, _) = splice(&md, &blocks);
        assert!(out.contains("> ```scala\n> a\n> ```"), "got {out:?}");
    }

    #[test]
    fn splice_reports_blocks_that_never_appeared() {
        let blocks = vec![
            CodeBlock { lang: "scala".into(), text: "a\n".into() },
            CodeBlock { lang: "java".into(), text: "b\n".into() },
        ];
        let md = format!("only {}\n", sentinel(0));
        let (_, unused) = splice(&md, &blocks);
        assert_eq!(unused, vec![1]);
    }

    #[test]
    fn sentinel_survives_the_markdown_converter() {
        // Guards the assumption the whole design rests on.
        let md = htmd::convert(&format!("<p>before {}</p>", sentinel(7))).unwrap();
        assert!(contains_sentinel(&md), "sentinel was mangled: {md:?}");
        let caps = sentinel_regex().captures(&md).unwrap();
        assert_eq!(&caps[1], "7");
    }
}

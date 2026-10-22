//! Post-conversion cleanup: front-matter, whitespace, and link rewriting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use url::Url;

use crate::cli::LinkStyle;
use crate::urlutil::canonical_key;

/// YAML front-matter for a page. The fields are chosen for retrieval: `title`
/// and `section` are what a chunker uses to give a fragment its context, and
/// `source` is what lets a reader get back to the live page.
#[derive(Debug, Clone)]
pub struct FrontMatter {
    pub title: String,
    pub source: String,
    pub site: String,
    pub section: String,
}

impl FrontMatter {
    pub fn render(&self) -> String {
        let mut s = String::from("---\n");
        s.push_str(&format!("title: {}\n", yaml_string(&self.title)));
        s.push_str(&format!("source: {}\n", yaml_string(&self.source)));
        s.push_str(&format!("site: {}\n", yaml_string(&self.site)));
        if !self.section.is_empty() {
            s.push_str(&format!("section: {}\n", yaml_string(&self.section)));
        }
        s.push_str(&format!("crawled: {}\n", yaml_string(&today_utc())));
        s.push_str("---\n\n");
        s
    }
}

/// Always double-quote. Titles contain colons, quotes, and leading `-`, all of
/// which are ambiguous or invalid as bare YAML scalars.
fn yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push(' '),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `YYYY-MM-DD` in UTC, without pulling in a date crate.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days-from-civil, inverted.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Collapse runs of blank lines, strip trailing whitespace, and guarantee
/// exactly one trailing newline.
///
/// Trailing whitespace is stripped *only outside* fenced code, where two
/// trailing spaces are Markdown's hard line break.
pub fn normalize(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank_run = 0usize;
    let mut fence: Option<Fence> = None;

    for line in md.lines() {
        if let Some(f) = &fence {
            out.push_str(line);
            out.push('\n');
            if f.closes(line) {
                fence = None;
            }
            continue;
        }

        if let Some(f) = Fence::opens(line) {
            // A fence starting right after prose needs a blank line before it.
            if blank_run == 0 && !out.is_empty() {
                out.push('\n');
            }
            blank_run = 0;
            out.push_str(line.trim_end());
            out.push('\n');
            fence = Some(f);
            continue;
        }

        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            // Two blank lines is the most that carries meaning.
            if blank_run <= 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(trimmed);
        out.push('\n');
    }

    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Fence {
    ch: char,
    len: usize,
}

impl Fence {
    /// A line opens a fence if it starts (after up to 3 spaces) with 3+ of
    /// the same backtick or tilde character.
    fn opens(line: &str) -> Option<Self> {
        let s = line.trim_start_matches(' ');
        if line.len() - s.len() > 3 {
            return None;
        }
        let ch = s.chars().next()?;
        if ch != '`' && ch != '~' {
            return None;
        }
        let len = s.chars().take_while(|&c| c == ch).count();
        (len >= 3).then_some(Fence { ch, len })
    }

    fn closes(&self, line: &str) -> bool {
        let s = line.trim();
        let run = s.chars().take_while(|&c| c == self.ch).count();
        run >= self.len && s[run..].trim().is_empty()
    }
}

/// Everything the link rewriter needs to turn a URL into a relative path.
pub struct LinkContext<'a> {
    /// The output path of the page being rewritten, relative to the output
    /// root, with forward slashes.
    pub current: &'a str,
    pub style: LinkStyle,
    /// Canonical URL -> output path, for every page in this run.
    pub pages: &'a HashMap<String, PathBuf>,
}

/// Rewrite Markdown links to point at the generated `.md` files.
///
/// Fenced code is skipped: a URL inside a code sample is content, not a link.
pub fn rewrite_links(md: &str, ctx: &LinkContext<'_>) -> String {
    if ctx.style == LinkStyle::Keep {
        return md.to_string();
    }

    let re = link_regex();
    let mut out = String::with_capacity(md.len());
    let mut fence: Option<Fence> = None;

    for line in md.lines() {
        if let Some(f) = &fence {
            out.push_str(line);
            out.push('\n');
            if f.closes(line) {
                fence = None;
            }
            continue;
        }
        if let Some(f) = Fence::opens(line) {
            fence = Some(f);
            out.push_str(line);
            out.push('\n');
            continue;
        }

        out.push_str(&re.replace_all(line, |caps: &regex::Captures| {
            let prefix = &caps[1];
            let target = &caps[2];
            let suffix = caps.get(3).map_or("", |m| m.as_str());
            match rewrite_target(target, ctx) {
                Some(new) => format!("{prefix}{new}{suffix}"),
                None => caps[0].to_string(),
            }
        }));
        out.push('\n');
    }

    // `lines()` drops the trailing newline that `normalize` established.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `](url)`, `](url "title")`, and the `![...]` image form.
///
/// Group 3 is everything from just after the URL through the closing paren, so
/// a title and its trailing `)` are carried across untouched. A `)` inside a
/// URL must be percent-encoded, which is what lets the class stop at the first
/// one.
fn link_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(!?\[[^\]]*\]\(\s*)([^)\s]+)([^)]*\))"#).expect("valid link regex")
    })
}

fn rewrite_target(target: &str, ctx: &LinkContext<'_>) -> Option<String> {
    // A bare anchor points within this page, and `Url::parse` would reject a
    // relative path outright — both are left exactly as written.
    if target.starts_with('#') {
        return None;
    }
    let url = Url::parse(target).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }

    let key = canonical_key(&url);

    match ctx.pages.get(&key) {
        // A page we generated: point at its Markdown file.
        Some(path) => {
            let target_path = path.to_string_lossy().replace('\\', "/");
            let mut rel = relative_from(ctx.current, &target_path);
            // Carry the deep link across. The heading slugs will not match,
            // but the section name is still a useful signal for a reader.
            if let Some(fragment) = url.fragment() {
                rel.push('#');
                rel.push_str(fragment);
            }
            Some(rel)
        }
        // Not part of this run. Absolute keeps the reference usable; leaving
        // it relative would point at a file that was never written.
        None => match ctx.style {
            LinkStyle::Absolute => Some(url.to_string()),
            _ => None,
        },
    }
}

/// Relative path from one output file to another, both with forward slashes.
fn relative_from(from_file: &str, to_file: &str) -> String {
    let from: Vec<&str> = from_file.split('/').collect();
    let to: Vec<&str> = to_file.split('/').collect();
    // The last component of `from` is the file itself, not a directory.
    let from_dirs = &from[..from.len().saturating_sub(1)];

    let common = from_dirs
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_dirs.len() {
        parts.push("..".to_string());
    }
    parts.extend(to[common..].iter().map(|s| s.to_string()));

    let joined = parts.join("/");
    if joined.is_empty() {
        // Same file: link to itself rather than emit an empty target.
        Path::new(to_file)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| to_file.to_string())
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_quotes_are_escaped() {
        assert_eq!(yaml_string(r#"a "b" c"#), r#""a \"b\" c""#);
        assert_eq!(yaml_string("line\nbreak"), "\"line break\"");
    }

    #[test]
    fn front_matter_is_valid_yaml() {
        let fm = FrontMatter {
            title: "Actors: Classic".into(),
            source: "https://x.dev/a.html".into(),
            site: "x.dev".into(),
            section: "docs/a".into(),
        };
        let out = fm.render();
        assert!(out.starts_with("---\n"));
        assert!(out.ends_with("---\n\n"));
        assert!(out.contains("title: \"Actors: Classic\"\n"));
        assert!(out.contains("section: \"docs/a\"\n"));
        // Round-trips through a real YAML parser's expectations: no bare colon
        // after a quote, and every value quoted.
        for line in out.lines().filter(|l| l.contains(": ")) {
            let value = line.split_once(": ").unwrap().1;
            assert!(value.starts_with('"') && value.ends_with('"'), "unquoted: {line}");
        }
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // Leap day.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn normalize_collapses_blank_runs() {
        assert_eq!(normalize("a\n\n\n\n\nb\n"), "a\n\nb\n");
        assert_eq!(normalize("a\n\n\nb\n"), "a\n\nb\n");
    }

    #[test]
    fn normalize_preserves_code_and_trailing_newline() {
        // Two trailing spaces inside a fence are a hard break; outside, noise.
        let md = "prose   \n\n```\ncode  \n```\n";
        let out = normalize(md);
        assert!(out.contains("prose\n"), "trailing spaces stripped outside code");
        assert!(out.contains("code  \n"), "trailing spaces kept inside code: {out:?}");
        assert!(out.ends_with("```\n"));
    }

    #[test]
    fn normalize_adds_a_blank_line_before_a_fence() {
        assert_eq!(normalize("text\n```\nx\n```\n"), "text\n\n```\nx\n```\n");
    }

    #[test]
    fn fence_detection_handles_length_and_char() {
        let f = Fence::opens("````").unwrap();
        assert_eq!((f.ch, f.len), ('`', 4));
        // A 3-backtick line must not close a 4-backtick fence.
        assert!(!f.closes("```"));
        assert!(f.closes("````"));
        assert!(Fence::opens("~~~").is_some());
        // Indented 4+ spaces is an indented code block, not a fence.
        assert!(Fence::opens("    ```").is_none());
        assert!(Fence::opens("``").is_none());
    }

    fn ctx<'a>(
        current: &'a str,
        pages: &'a HashMap<String, PathBuf>,
        style: LinkStyle,
    ) -> LinkContext<'a> {
        LinkContext { current, pages, style }
    }

    #[test]
    fn rewrites_a_known_page_to_a_relative_path() {
        let mut pages = HashMap::new();
        pages.insert(
            canonical_key(&Url::parse("https://x.dev/docs/a/typed.html").unwrap()),
            PathBuf::from("docs/a/typed.md"),
        );
        let md = "See [Typed](https://x.dev/docs/a/typed.html) for more.";
        let out = rewrite_links(md, &ctx("docs/a/actors.md", &pages, LinkStyle::Relative));
        assert!(out.contains("[Typed](typed.md)"), "got: {out}");
    }

    #[test]
    fn relative_paths_walk_up_the_tree() {
        let mut pages = HashMap::new();
        pages.insert(
            canonical_key(&Url::parse("https://x.dev/docs/b/other.html").unwrap()),
            PathBuf::from("docs/b/other.md"),
        );
        let md = "[o](https://x.dev/docs/b/other.html)";
        let out = rewrite_links(md, &ctx("docs/a/actors.md", &pages, LinkStyle::Relative));
        assert!(out.contains("(../b/other.md)"), "got: {out}");
    }

    #[test]
    fn unknown_pages_are_left_absolute_or_untouched() {
        let pages = HashMap::new();
        let md = "[x](https://other.dev/page.html)";

        let abs = rewrite_links(md, &ctx("a.md", &pages, LinkStyle::Absolute));
        assert!(abs.contains("(https://other.dev/page.html)"));

        let rel = rewrite_links(md, &ctx("a.md", &pages, LinkStyle::Relative));
        assert!(rel.contains("(https://other.dev/page.html)"), "left as-is: {rel}");

        let keep = rewrite_links(md, &ctx("a.md", &pages, LinkStyle::Keep));
        assert_eq!(keep, md);
    }

    #[test]
    fn anchors_and_mailto_are_untouched() {
        let pages = HashMap::new();
        let md = "[a](#section) and [b](mailto:x@y.z)";
        let out = rewrite_links(md, &ctx("a.md", &pages, LinkStyle::Absolute));
        assert!(out.contains("(#section)"));
        assert!(out.contains("(mailto:x@y.z)"));
    }

    #[test]
    fn links_inside_fences_are_not_rewritten() {
        let mut pages = HashMap::new();
        pages.insert(
            canonical_key(&Url::parse("https://x.dev/docs/a/typed.html").unwrap()),
            PathBuf::from("docs/a/typed.md"),
        );
        let md = "```md\n[Typed](https://x.dev/docs/a/typed.html)\n```\n";
        let out = rewrite_links(md, &ctx("docs/a/actors.md", &pages, LinkStyle::Relative));
        assert!(
            out.contains("(https://x.dev/docs/a/typed.html)"),
            "code link was rewritten: {out}"
        );
    }

    #[test]
    fn link_titles_survive() {
        let mut pages = HashMap::new();
        pages.insert(
            canonical_key(&Url::parse("https://x.dev/docs/a/typed.html").unwrap()),
            PathBuf::from("docs/a/typed.md"),
        );
        let md = r#"[T](https://x.dev/docs/a/typed.html "The title")"#;
        let out = rewrite_links(md, &ctx("docs/a/actors.md", &pages, LinkStyle::Relative));
        assert!(out.contains(r#"(typed.md "The title")"#), "got: {out}");
    }

    #[test]
    fn images_are_rewritten_too() {
        let mut pages = HashMap::new();
        pages.insert(
            canonical_key(&Url::parse("https://x.dev/docs/a/d.html").unwrap()),
            PathBuf::from("docs/a/d.md"),
        );
        let md = "![diagram](https://x.dev/docs/a/d.html)";
        let out = rewrite_links(md, &ctx("docs/a/actors.md", &pages, LinkStyle::Relative));
        assert!(out.contains("![diagram](d.md)"), "got: {out}");
    }
}

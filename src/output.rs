//! Mapping URLs onto files, and generating the index.
//!
//! The mirror is meant to be a directory tree a person can navigate, so the
//! URL path is preserved almost verbatim. The interesting part is everything
//! that can go wrong doing that on Windows.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use url::Url;

use crate::error::{Error, Result};
use crate::urlutil::canonical_key;

/// Characters Windows forbids in a filename, plus the POSIX separator so a
/// decoded `%2F` cannot silently create a directory level.
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names that Windows resolves to hardware regardless of extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Longest single path segment we emit. Well under every filesystem's limit,
/// and long enough that real doc URLs are untouched.
const MAX_SEGMENT_CHARS: usize = 120;

/// Longest relative path we emit. Windows' MAX_PATH is 260 including the
/// output directory, so this leaves generous headroom.
const MAX_RELATIVE_CHARS: usize = 180;

/// The generated table of contents. Reserved so a crawled page cannot claim
/// it — the index is the one file a reader is guaranteed to look for.
pub const INDEX_FILE: &str = "index.md";

/// Longest directory prefix shared by every URL, ending in `/`.
///
/// Stripping it is what turns `docs/pekko/1.7/actors.md` into `actors.md` for
/// a single-project crawl. For a single URL this is its own directory, so a
/// one-page run lands at the output root.
pub fn common_dir_prefix(urls: &[Url]) -> String {
    let dirs: Vec<Vec<String>> = urls.iter().map(dir_segments).collect();
    let Some(first) = dirs.first() else {
        return "/".to_string();
    };

    let mut common = first.clone();
    for d in &dirs[1..] {
        let n = common
            .iter()
            .zip(d.iter())
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(n);
    }

    if common.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", common.join("/"))
    }
}

fn dir_segments(url: &Url) -> Vec<String> {
    let path = url.path();
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    dir.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Map one URL onto a path relative to the output root.
///
/// Segments are split on the *encoded* path and decoded individually, so a
/// `%2F` inside a segment decodes to a slash that sanitization then defuses,
/// rather than quietly becoming a new directory level.
pub fn url_to_relative_path(url: &Url, strip_prefix: &str) -> PathBuf {
    let raw_path = url.path();
    let rest = raw_path.strip_prefix(strip_prefix).unwrap_or(raw_path);
    let trailing_slash = rest.ends_with('/') || rest.is_empty();

    let mut segments: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| sanitize_segment(&percent_decode_str(s).decode_utf8_lossy()))
        .collect();

    if trailing_slash || segments.is_empty() {
        segments.push(INDEX_FILE.to_string());
    } else if let Some(last) = segments.last_mut() {
        *last = map_extension(last);
    }

    PathBuf::from(segments.join("/"))
}

/// `.html`/`.htm` become `.md`; anything extensionless gains `.md`.
fn map_extension(segment: &str) -> String {
    let lower = segment.to_ascii_lowercase();
    for ext in [".html", ".htm", ".xhtml"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return format!("{}.md", &segment[..stem.len()]);
        }
    }
    if segment.contains('.') {
        // Some other extension. The crawler should not have admitted it, but
        // if it did, keep the name readable rather than truncating it.
        return format!("{segment}.md");
    }
    format!("{segment}.md")
}

fn sanitize_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for c in segment.chars() {
        if c.is_control() || FORBIDDEN.contains(&c) {
            out.push('_');
        } else {
            out.push(c);
        }
    }

    // Windows silently drops trailing dots and spaces, which would make two
    // distinct URLs collide only after the filesystem had its way with them.
    let trimmed = out.trim_end_matches(['.', ' ']);
    let mut s = if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    };

    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s = format!("_{s}");
    }

    truncate_chars(&mut s, MAX_SEGMENT_CHARS);
    s
}

fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() <= max {
        return;
    }
    let cut = s
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s.truncate(cut);
}

/// FNV-1a, truncated to 32 bits. Only needs to be stable and short.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

/// Assigns each crawled URL a unique output path.
#[derive(Debug)]
pub struct OutputMap {
    root: PathBuf,
    strip_prefix: String,
    /// Canonical URL key -> path relative to `root`.
    pages: HashMap<String, PathBuf>,
    /// Lowercased relative path -> the canonical URL key that owns it.
    taken: HashMap<String, String>,
}

impl OutputMap {
    pub fn new(root: PathBuf, strip_prefix: String) -> Self {
        let mut map = Self {
            root,
            strip_prefix,
            pages: HashMap::new(),
            taken: HashMap::new(),
        };
        // Reserve the index before any page can claim it.
        map.taken
            .insert(INDEX_FILE.to_lowercase(), String::new());
        map
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn pages(&self) -> &HashMap<String, PathBuf> {
        &self.pages
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn get(&self, url: &Url) -> Option<&PathBuf> {
        self.pages.get(&canonical_key(url))
    }

    /// Assign (or look up) the output path for `url`.
    pub fn assign(&mut self, url: &Url) -> PathBuf {
        let key = canonical_key(url);
        if let Some(p) = self.pages.get(&key) {
            return p.clone();
        }

        let mut path = url_to_relative_path(url, &self.strip_prefix);
        let mut path_str = path.to_string_lossy().replace('\\', "/");

        // Windows filesystems are case-insensitive, so `Actors.md` and
        // `actors.md` are the same file. Disambiguate rather than lose a page.
        if let Some(owner) = self.taken.get(&path_str.to_lowercase()) {
            if owner != &key {
                path = disambiguate(&path, &key);
                path_str = path.to_string_lossy().replace('\\', "/");
            }
        }

        // Last resort for a path that is still absurdly long.
        if path_str.chars().count() > MAX_RELATIVE_CHARS {
            path = disambiguate(&shorten(&path), &key);
            path_str = path.to_string_lossy().replace('\\', "/");
        }

        self.taken.insert(path_str.to_lowercase(), key.clone());
        self.pages.insert(key, path.clone());
        path
    }

    /// Write `content` to the page's file, creating directories as needed.
    pub fn write(&self, relative: &Path, content: &str) -> Result<PathBuf> {
        write_file(&self.root, relative, content)
    }
}

/// Write a file under `root`, creating parent directories as needed.
pub fn write_file(root: &Path, relative: &Path, content: &str) -> Result<PathBuf> {
    let full = root.join(relative);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Other(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(&full, content).map_err(|e| Error::Other(format!("{}: {e}", full.display())))?;
    Ok(full)
}

/// `docs/a/actors.md` -> `docs/a/actors-1a2b3c4d.md`.
fn disambiguate(path: &Path, key: &str) -> PathBuf {
    let hash = short_hash(key);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_string());
    let name = format!("{stem}-{hash}.md");
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

/// Keep the leading directories and the filename, drop the middle.
fn shorten(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page.md".to_string());
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    let keep = MAX_RELATIVE_CHARS.saturating_sub(name.chars().count() + 10);
    let mut dir_cut = dir;
    truncate_chars(&mut dir_cut, keep);
    // Do not leave a half-written directory name.
    if let Some(i) = dir_cut.rfind('/') {
        dir_cut.truncate(i);
    }

    if dir_cut.is_empty() {
        PathBuf::from(name)
    } else {
        PathBuf::from(dir_cut).join(name)
    }
}

/// One crawled page, as the index needs to see it.
#[derive(Debug, Clone)]
pub struct PageRecord {
    pub url: Url,
    /// Relative path with forward slashes.
    pub path: String,
    pub title: String,
}

/// The root table of contents: pages grouped by directory, sorted the way a
/// person reads (`part2` before `part10`).
pub fn build_index(records: &[PageRecord], site: &str) -> String {
    let mut by_dir: BTreeMap<String, Vec<&PageRecord>> = BTreeMap::new();
    for r in records {
        let dir = match r.path.rfind('/') {
            Some(i) => r.path[..i].to_string(),
            None => String::new(),
        };
        by_dir.entry(dir).or_default().push(r);
    }

    let mut dirs: Vec<String> = by_dir.keys().cloned().collect();
    dirs.sort_by(|a, b| natural_cmp(a, b));

    let mut out = String::new();
    out.push_str("# Documentation index\n\n");
    out.push_str(&format!(
        "{} — {} page{}.\n",
        site,
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    ));

    for dir in dirs {
        let mut pages = by_dir.remove(&dir).unwrap_or_default();
        pages.sort_by(|a, b| natural_cmp(&a.path, &b.path));

        out.push('\n');
        if dir.is_empty() {
            out.push_str("## Top level\n");
        } else {
            out.push_str(&format!("## {dir}\n"));
        }
        out.push('\n');
        for p in pages {
            let title = if p.title.trim().is_empty() {
                p.path.rsplit('/').next().unwrap_or(&p.path).to_string()
            } else {
                // A `]` in a title would close the link text early.
                p.title.replace('[', "(").replace(']', ")")
            };
            // Paths are relative to the output root, and the index lives
            // there too, so they are already correct as written.
            out.push_str(&format!("- [{}]({})\n", title.trim(), p.path));
        }
    }

    out
}

/// Numeric-aware ordering, so `1.10` sorts after `1.9`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let ta = tokenize(a);
    let tb = tokenize(b);

    for (x, y) in ta.iter().zip(tb.iter()) {
        let ord = match (x.0, y.0) {
            (true, true) => {
                let nx: u128 = x.1.parse().unwrap_or(0);
                let ny: u128 = y.1.parse().unwrap_or(0);
                // Equal magnitude: fewer leading zeros sorts first.
                nx.cmp(&ny).then_with(|| x.1.len().cmp(&y.1.len()))
            }
            _ => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ta.len().cmp(&tb.len())
}

/// Split into `(is_digits, text)` runs.
fn tokenize(s: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_digit: Option<bool> = None;

    for c in s.chars() {
        let d = c.is_ascii_digit();
        if cur_digit.is_some_and(|prev| prev != d) {
            out.push((cur_digit.unwrap(), std::mem::take(&mut cur)));
        }
        cur_digit = Some(d);
        cur.push(c);
    }
    if let Some(d) = cur_digit {
        if !cur.is_empty() {
            out.push((d, cur));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn maps_html_to_md_and_directories_to_index() {
        assert_eq!(
            url_to_relative_path(&u("https://x.dev/docs/a/actors.html"), "/docs/a/"),
            PathBuf::from("actors.md")
        );
        assert_eq!(
            url_to_relative_path(&u("https://x.dev/docs/a/"), "/docs/a/"),
            PathBuf::from("index.md")
        );
        assert_eq!(
            url_to_relative_path(&u("https://x.dev/docs/a"), "/docs/"),
            PathBuf::from("a.md")
        );
    }

    #[test]
    fn extensionless_and_other_extensions_gain_md() {
        assert_eq!(
            url_to_relative_path(&u("https://x.dev/docs/guide"), "/docs/"),
            PathBuf::from("guide.md")
        );
        assert_eq!(
            url_to_relative_path(&u("https://x.dev/docs/a.htm"), "/docs/"),
            PathBuf::from("a.md")
        );
    }

    #[test]
    fn strips_the_common_directory_prefix() {
        let urls = vec![
            u("https://pekko.apache.org/docs/pekko/1.7/actors.html"),
            u("https://pekko.apache.org/docs/pekko/1.7/typed.html"),
        ];
        assert_eq!(common_dir_prefix(&urls), "/docs/pekko/1.7/");
    }

    #[test]
    fn single_page_strips_its_own_directory() {
        let urls = vec![u("https://pekko.apache.org/docs/pekko/1.7/actors.html")];
        assert_eq!(common_dir_prefix(&urls), "/docs/pekko/1.7/");
    }

    #[test]
    fn unrelated_urls_share_only_the_root() {
        let urls = vec![u("https://x.dev/a/one.html"), u("https://x.dev/b/two.html")];
        assert_eq!(common_dir_prefix(&urls), "/");
    }

    #[test]
    fn sanitizes_windows_forbidden_characters() {
        assert_eq!(sanitize_segment("a:b?c*d"), "a_b_c_d");
        assert_eq!(sanitize_segment("a<b>c"), "a_b_c");
        // Control characters cannot reach a filename.
        assert_eq!(sanitize_segment("a\u{7}b"), "a_b");
    }

    #[test]
    fn decodes_percent_escapes_but_not_into_new_directories() {
        // %20 becomes a space; %2F must NOT become a path separator.
        let p = url_to_relative_path(&u("https://x.dev/docs/a%20b/c%2Fd.html"), "/docs/");
        assert_eq!(p, PathBuf::from("a b/c_d.md"));
    }

    #[test]
    fn rescues_reserved_device_names() {
        assert_eq!(sanitize_segment("CON"), "_CON");
        assert_eq!(sanitize_segment("nul"), "_nul");
        assert_eq!(sanitize_segment("COM1"), "_COM1");
        assert_eq!(sanitize_segment("CON.html"), "_CON.html");
        // Not reserved: only the exact stem matters.
        assert_eq!(sanitize_segment("console"), "console");
        assert_eq!(sanitize_segment("COM10"), "COM10");
    }

    #[test]
    fn strips_trailing_dots_and_spaces() {
        assert_eq!(sanitize_segment("name. "), "name");
        assert_eq!(sanitize_segment("name..."), "name");
        // A segment that is nothing but dots cannot become an empty name.
        assert_eq!(sanitize_segment("..."), "_");
    }

    #[test]
    fn caps_segment_length() {
        let long = "a".repeat(500);
        assert_eq!(sanitize_segment(&long).chars().count(), MAX_SEGMENT_CHARS);
    }

    #[test]
    fn case_collisions_get_disambiguated() {
        let mut m = OutputMap::new(PathBuf::from("out"), "/docs/".into());
        let a = m.assign(&u("https://x.dev/docs/Actors.html"));
        let b = m.assign(&u("https://x.dev/docs/actors.html"));
        assert_ne!(a, b, "case-insensitive filesystems would clobber these");
        assert_eq!(a, PathBuf::from("Actors.md"));
        assert!(b.to_string_lossy().starts_with("actors-"), "got {b:?}");
    }

    #[test]
    fn the_index_file_is_reserved() {
        let mut m = OutputMap::new(PathBuf::from("out"), "/".into());
        let p = m.assign(&u("https://x.dev/"));
        assert_ne!(p, PathBuf::from(INDEX_FILE));
        assert!(p.to_string_lossy().starts_with("index-"), "got {p:?}");
    }

    #[test]
    fn assign_is_idempotent() {
        let mut m = OutputMap::new(PathBuf::from("out"), "/docs/".into());
        let url = u("https://x.dev/docs/a.html");
        assert_eq!(m.assign(&url), m.assign(&url));
        // And canonicalization means a tracking param is the same page.
        let same = u("https://x.dev/docs/a.html?utm_source=nl");
        assert_eq!(m.assign(&url), m.assign(&same));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn natural_order_puts_part2_before_part10() {
        let mut v = vec!["part10.md", "part2.md", "part1.md"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["part1.md", "part2.md", "part10.md"]);
    }

    #[test]
    fn natural_order_handles_versions() {
        let mut v = vec!["1.10/x.md", "1.9/x.md", "1.7/x.md"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["1.7/x.md", "1.9/x.md", "1.10/x.md"]);
    }

    #[test]
    fn index_groups_by_directory_and_links_relatively() {
        let records = vec![
            PageRecord {
                url: u("https://x.dev/docs/a/actors.html"),
                path: "docs/a/actors.md".into(),
                title: "Classic Actors".into(),
            },
            PageRecord {
                url: u("https://x.dev/docs/a/typed.html"),
                path: "docs/a/typed.md".into(),
                title: "Typed Actors".into(),
            },
            PageRecord {
                url: u("https://x.dev/docs/b/io.html"),
                path: "docs/b/io.md".into(),
                title: "IO".into(),
            },
        ];
        let idx = build_index(&records, "x.dev");
        assert!(idx.starts_with("# Documentation index\n"));
        assert!(idx.contains("x.dev — 3 pages."));
        assert!(idx.contains("## docs/a\n"));
        assert!(idx.contains("## docs/b\n"));
        assert!(idx.contains("- [Classic Actors](docs/a/actors.md)"));
        assert!(idx.contains("- [IO](docs/b/io.md)"));
    }

    #[test]
    fn index_escapes_brackets_in_titles() {
        let records = vec![PageRecord {
            url: u("https://x.dev/a.html"),
            path: "a.md".into(),
            title: "Array[T]".into(),
        }];
        let idx = build_index(&records, "x.dev");
        assert!(idx.contains("- [Array(T)](a.md)"), "got: {idx}");
    }

    #[test]
    fn index_falls_back_to_the_filename_for_an_empty_title() {
        let records = vec![PageRecord {
            url: u("https://x.dev/docs/a.html"),
            path: "docs/a.md".into(),
            title: "   ".into(),
        }];
        assert!(build_index(&records, "x.dev").contains("- [a.md](docs/a.md)"));
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let m = OutputMap::new(dir.path().to_path_buf(), "/".into());
        let full = m
            .write(Path::new("docs/a/x.md"), "# hi\n")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&full).unwrap(), "# hi\n");
        assert!(full.exists());
    }
}

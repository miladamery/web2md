# web2md

Crawl a documentation site and write one Markdown file per page.

Built for turning programming-language and framework docs — Pekko, Akka, Spring
and similar — into a corpus you can feed to an LLM or a RAG pipeline. Prose,
headings, lists, tables, admonitions and links all survive; images are dropped
by default; **code blocks are reproduced byte-for-byte**.

```
web2md https://pekko.apache.org/docs/pekko/current/actors.html -o out --limit 1
```

```markdown
---
title: "Classic Actors"
source: "https://pekko.apache.org/docs/pekko/current/actors.html"
site: "pekko.apache.org"
crawled: "2026-10-22"
---

# Classic Actors

**Note**

Pekko Classic pertains to the original Actor APIs, …

## Module info

To use Classic Actors, add the following dependency in your project:

**sbt**

```scala
val PekkoVersion = "1.7.1"
libraryDependencies ++= Seq(
  "org.apache.pekko" %% "pekko-actor" % PekkoVersion
)
```
```

## Install

```sh
cargo build --release
# target/release/web2md
```

Rust 1.82 or newer.

## Usage

The seed URL defines the crawl origin and the output subtree. Everything else
is optional.

```sh
# One page
web2md https://docs.example.com/guide/intro.html -o out

# A whole site via its sitemap
web2md https://doc.akka.io --sitemap https://doc.akka.io/sitemap.xml -o out-akka

# Follow links from a seed, staying under its directory
web2md https://pekko.apache.org/docs/pekko/current/ -o out --scope prefix

# A list of URLs you already have
web2md --urls urls.txt -o out
```

### Choosing what to crawl

Doc sitemaps are far larger than the prose you actually want. Spring Boot's is
**68,711 URLs**, most of it Javadoc for six versions including `-SNAPSHOT`
builds. Filtering is a first-class feature, not an afterthought:

```sh
# 89 URLs, from 68,711, in about three seconds
web2md https://docs.spring.io/spring-boot/ \
  --sitemap https://docs.spring.io/spring-boot/sitemap.xml \
  --include '/4\.2/reference/' --exclude-common --dry-run
```

`--include` and `--exclude` are repeatable regexes; any include match admits,
any exclude match rejects. `--exclude-common` adds built-in exclusions for
`/api/`, `/javadoc/`, `-SNAPSHOT` and non-HTML extensions.

Always `--dry-run` a new filter before committing to a crawl — it prints the
URL set without fetching anything.

`--scope` decides how far link-following may roam: `host` (default), `prefix`
(same host *and* directory — the right choice on hosts that serve several
projects, like Pekko's `/docs/pekko/<version>/`), or `any`.

### Re-running

`--skip-existing` skips pages already on disk and keeps the index intact, so a
re-run over a thousand-page site is near-instant:

```sh
web2md https://docs.spring.io/spring-boot/ --sitemap ... --skip-existing -o out
# wrote 0 page(s) to out
#   20 skipped (already on disk)
```

Without it, each page is revalidated with a conditional request and written
again only if the server reports a change.

### Output layout

The URL tree is mirrored, so `guide/intro.html` becomes `guide/intro.md`, and a
generated `index.md` holds a hierarchical table of contents labelled with each
page's own title:

```markdown
# Documentation index

doc.akka.io — 50 pages.

## concepts

- [Memory and context](concepts/agent-memory.md)
- [Agents](concepts/ai-agents.md)
- [Multi-agent coordination](concepts/ai-orchestration-patterns.md)
```

With `link_style = "relative"` (the default) links between crawled pages point
at the sibling `.md`; links to pages outside the crawl stay as absolute URLs
rather than becoming broken relative paths. `--merge` additionally writes one
concatenated file for whole-corpus context.

`index.md` is reserved for the generated table of contents. When the crawl
itself starts at a directory URL, its landing page wants the same name — so the
page is written as `index-<hash>.md` instead, with a hash of its URL that is
stable across runs. The index is the one file a reader is guaranteed to look
for, so it keeps the plain name.

Paths are sanitized for Windows as well as POSIX: reserved device names
(`CON`, `NUL`, `COM1`…), trailing dots and spaces, and case-insensitive
collisions are all handled, since `Actors.md` and `actors.md` would otherwise
silently clobber each other on a Windows filesystem.

## Configuration

`web2md.toml` in the working directory is picked up automatically; `--config`
names one explicitly. Anything on the command line wins. See
[`web2md.toml.example`](web2md.toml.example).

## How code stays intact

This is the part that matters, and it drives the design.

The naive approach — parse the HTML, convert to Markdown, hope the code
survives — fails in ways that are hard to notice. Markdown converters reflow
whitespace, escape `|` and `*`, and mangle indentation. A code block that comes
out *nearly* right is worse than one that fails loudly, because nobody checks.

So the converter never sees code at all:

1. **Select the content root** via the site profile's selectors, falling back
   to a generic chain (`article` → `main` → `[role=main]` → `#content` → …).
2. **Prune** chrome — nav, sidebars, footers, copy buttons, heading permalinks,
   line-number gutters.
3. **Absolutize** every `href` while the page URL is still known.
4. **Capture** every `<pre>` body verbatim, in document order.
5. **Stub** each `<pre>` out, replacing it with a private-use-area sentinel.
6. **Convert** the remaining HTML to Markdown — which now contains no code.
7. **Splice** the captured bodies back in as fenced blocks, choosing a fence
   longer than any backtick run inside the body.

Steps 4 and 5 both walk the document in order, so the indices line up. The
ordering is load-bearing: pruning must happen *before* capture, or
Asciidoctor's line-number gutter — a second `<pre>` holding bare digits —
becomes a phantom code block next to every listing.

Two details that only show up on real pages:

- **Copy buttons live inside `<pre>`.** On Pekko and Akka, 83 of 86 `<pre>`
  blocks contain `<button class="snippet-button">copy</button>` as a sibling of
  `<code>`. A naive `pre.text()` prepends the literal word `copy` to almost
  every block. The code is read from `code.text()`, and the buttons are pruned
  as well so the fallback path is safe too.
- **A stray space is not an indent.** When two `<pre>` elements sit adjacent,
  the converter emits both sentinels on one line separated by a space. Reading
  the fence's indentation from the partially-spliced output made that leftover
  space look like a deliberate indent, and every line of the next block came out
  shifted by one. The prefix is read from the *original* Markdown instead.

Anything the converter drops is re-appended under a `## Code` heading rather
than lost, and a sentinel that never got replaced raises a warning.

## Site profiles

A profile says where a site's content root is and what chrome to strip.
Built-ins cover MkDocs Material (Pekko), Lightbend Paradox (Akka), Asciidoctor
(Spring) and Antora, matched by host and by `<meta name="generator">`.

A host match is verified against the page before it is trusted, because one
host can serve more than one doc system: `doc.akka.io` is Paradox under
`/libraries/akka-core/` and Antora under `/concepts/`. When the host profile
doesn't match, the generator meta decides instead.

If a profile matches nothing at all, extraction degrades to the generic chain
and reports a warning — a site redesign should mean a less clean extraction,
not an empty one.

To support a new site without rebuilding, add a `[[site]]` block to
`web2md.toml`. `--profile <NAME>` forces one explicitly.

## Tests

```sh
cargo test
```

105 unit tests plus 10 fixture tests that run offline against real saved pages
from all three target sites (`tests/fixtures/`). The fixture tests assert that
every code block on the page is present and byte-identical, that copy-button
text never leaks in, that site chrome never reaches the output, and that titles
come from the page rather than the site header.

## Notes

- Fully server-rendered sites only — no headless browser. All three named
  targets qualify. A JS-rendered site will extract as an empty shell.
- `robots.txt` is honored by default; `--no-robots` overrides it. `--delay`
  (150 ms default) spaces requests to the same host.
- Only the `http`/`https` schemes are followed.

## License

MIT OR Apache-2.0

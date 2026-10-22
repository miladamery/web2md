# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

web2md is a Rust CLI that crawls documentation sites (Pekko, Akka, Spring — any fully server-rendered docs) and writes one Markdown file per HTML page, producing corpora for LLM/RAG pipelines. The core invariant: **code blocks must arrive byte-identical** to what the browser renders inside `<pre>`.

`requirements.txt` is not a Python requirements file — it is the original project brief. There is no Python in this repo.

## Commands

```sh
cargo build --release         # binary at target/release/web2md
cargo test                    # unit tests + fixture tests, all offline
cargo test --test extraction  # fixture tests only (tests/extraction.rs)
cargo test <substring>        # a single test by name
```

Rust 1.82+. Smoke-test filters against a real site with `--dry-run` before a crawl — it prints the URL set and fetches nothing:

```sh
cargo run -- https://docs.spring.io/spring-boot/ \
  --sitemap https://docs.spring.io/spring-boot/sitemap.xml \
  --include '/4\.2/reference/' --exclude-common --dry-run
```

## Architecture

Single crate, bin + lib. All logic lives in the lib (`src/lib.rs`); `main.rs` only parses args, resolves settings, runs the pipeline, and reports. Per-page failures are recorded in `Summary` and reported at the end, not propagated — only config/URL errors abort a run.

Flow: `cli` → `config::Settings::resolve` (config file merged under CLI) → `pipeline::run`, which discovers URLs, fetches, extracts, writes, then a `finish` phase rewrites links on disk, builds `index.md`, optionally merges, and saves state.

Design points that span multiple files:

- **Code-block fidelity (`extract.rs` + `codeblock.rs`).** The Markdown converter (htmd) never sees code: prune chrome → absolutize hrefs → capture every `<pre>` verbatim → stub with private-use-area sentinels → convert → splice back with a fence longer than any inner backtick run. Pruning *must* happen before capture (Asciidoctor's line-number gutter is a second `<pre>` of bare digits); capture and stub walk in the same order so indices line up. Code text is read from `code`, never `pre.text()` — copy buttons live inside `<pre>` on Pekko/Akka and would prepend the literal word "copy" to nearly every block. The fixture tests assert byte-identity against saved pages; run them after touching this path.

- **Two crawl modes in `pipeline.rs`.** With `--sitemap`/`--urls` the URL set is known up front: every output path is assigned before any fetch, so the link rewriter sees the complete page set. Without them, BFS follows links: paths are assigned per batch, and jobs carry `need_body = true` because a `304` or a `--skip-existing` exit would end discovery — the BFS path always asks for the full page. Per-host politeness (delay gates in `fetch.rs`) and robots.txt (`robots.rs`; failure to fetch is treated as allowed) apply to both modes.

- **Profile resolution (`profile.rs`, chosen per page in `pipeline::profile_for`).** Explicit `--profile`, then host match — verified against the page's DOM, because one host can serve two doc systems (`doc.akka.io` is Paradox under `/libraries/akka-core/` and Antora under `/concepts/`) — then `<meta name="generator">`, then the generic selector chain. Built-ins: `mkdocs`, `paradox`, `asciidoctor`, `antora`. Users add `[[site]]` blocks in `web2md.toml`; a user profile reusing a built-in's name overrides it.

- **CLI/config precedence (`cli.rs` + `config.rs`).** Every CLI field a config file can also set is an `Option`, so "unset on the command line" stays distinguishable from "explicitly set to the default" — that is what makes CLI-wins-over-file work. When adding a flag, add the field to both `Cli` and `FileConfig` and resolve it in `Settings::resolve`.

- **URL→file mapping (`output.rs`).** Mirrors the URL tree with Windows hardening: reserved device names (`CON`, `NUL`, `COM1`…), forbidden chars, length caps, and case-insensitive collision handling (`Actors.md` vs `actors.md`). `index.md` is reserved for the generated TOC; a crawled page that would claim the name becomes `index-<hash>.md`. Link rewriting happens after all writes, once every page's path is known.

- **Resumability (`state.rs`).** `.web2md/state.json` stores each page's path, ETag, and Last-Modified; re-runs revalidate with conditional requests so an unchanged site costs one `304` per page. `--skip-existing` is the cheaper exit but is only taken when discovery does not need the body.

## Dependency note

reqwest is at 0.13, which renamed the TLS features: `rustls-tls` no longer exists, and the default features already give rustls. See the comment in `Cargo.toml` before "fixing" the feature list.

## Configuration

`web2md.toml` in the working directory is picked up automatically; `--config` names one explicitly. `web2md.toml.example` documents every field, including user-defined site profiles.

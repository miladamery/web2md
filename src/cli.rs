//! Command-line surface.
//!
//! Every field that a config file can also set is an `Option` here, so that
//! "unset on the command line" stays distinguishable from "explicitly set to
//! the default". That distinction is what lets [`crate::config::Settings`]
//! apply CLI-wins-over-file precedence without guessing.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LinkStyle {
    /// Rewrite links to point at the sibling `.md` file (default). Keeps the
    /// generated tree self-consistent, which is what a RAG ingest wants.
    Relative,
    /// Keep the original absolute URL. Useful when the Markdown will be read
    /// alongside live pages.
    Absolute,
    /// Leave every link exactly as the HTML had it.
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScopeArg {
    /// Same host as the seed.
    Host,
    /// Same host and under the seed's directory. Use this on hosts that serve
    /// several projects (Pekko's `/docs/pekko/<version>/`).
    Prefix,
    /// No host restriction. Only sensible together with `--include`.
    Any,
}

#[derive(Debug, Parser)]
#[command(
    name = "web2md",
    version,
    about = "Crawl a documentation site and write one Markdown file per page.",
    after_help = "Examples:\n  \
        web2md https://pekko.apache.org/docs/pekko/current/actors.html -o out\n  \
        web2md https://doc.akka.io --sitemap https://doc.akka.io/sitemap.xml --limit 50\n  \
        web2md https://docs.spring.io/spring-boot/ --sitemap https://docs.spring.io/spring-boot/sitemap.xml \\\n      --include '/4\\.2/reference/' --exclude-common --dry-run"
)]
pub struct Cli {
    /// Seed URL(s). The first one defines the crawl origin and the default
    /// output subtree.
    #[arg(value_name = "URL")]
    pub seeds: Vec<String>,

    /// Directory to write Markdown into.
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// TOML config file. Defaults to `web2md.toml` in the working directory
    /// when that file exists.
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Read the URL list from a sitemap (recursing through sitemap indexes).
    #[arg(long, value_name = "URL")]
    pub sitemap: Option<String>,

    /// Read the URL list from a newline-delimited file instead of crawling.
    #[arg(long, value_name = "FILE")]
    pub urls: Option<PathBuf>,

    /// Only keep URLs matching this regex. Repeatable; any match admits.
    #[arg(long, value_name = "REGEX")]
    pub include: Vec<String>,

    /// Drop URLs matching this regex. Repeatable; any match rejects.
    #[arg(long, value_name = "REGEX")]
    pub exclude: Vec<String>,

    /// Also apply the built-in exclusions for Javadoc, `/api/`, `-SNAPSHOT`
    /// and similar near-duplicate trees.
    #[arg(long)]
    pub exclude_common: bool,

    /// How far from the seed to follow links. Sitemap and explicit URL lists
    /// ignore this.
    #[arg(long, value_name = "N")]
    pub max_depth: Option<usize>,

    /// Stop after this many pages.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Resolve and print the URL set without fetching any page bodies.
    #[arg(long)]
    pub dry_run: bool,

    /// How many pages to fetch at once.
    #[arg(long, value_name = "N")]
    pub concurrency: Option<usize>,

    /// Minimum delay between requests to the same host, in milliseconds.
    #[arg(long, value_name = "MS")]
    pub delay: Option<u64>,

    /// Per-request timeout, in seconds.
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Override the User-Agent header.
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,

    /// Ignore robots.txt.
    #[arg(long)]
    pub no_robots: bool,

    /// Keep `<img>` as Markdown image syntax. Off by default: the target
    /// corpus is text, and images are usually diagrams that add noise.
    #[arg(long)]
    pub keep_images: bool,

    /// How to emit links.
    #[arg(long, value_enum, value_name = "STYLE")]
    pub link_style: Option<LinkStyle>,

    /// Emit YAML front-matter. On by default; `--no-front-matter` disables it.
    #[arg(long, overrides_with = "no_front_matter")]
    pub front_matter: bool,

    #[arg(long = "no-front-matter", overrides_with = "front_matter")]
    pub no_front_matter: bool,

    /// Also write one concatenated `<site>.md` containing every page.
    #[arg(long)]
    pub merge: bool,

    /// Skip pages whose output file already exists. This is what makes a
    /// re-run over a thousand-page site cheap.
    #[arg(long)]
    pub skip_existing: bool,

    /// Force a site profile by name, bypassing host and generator detection.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// How much of the site to consider in scope.
    #[arg(long, value_enum, value_name = "SCOPE")]
    pub scope: Option<ScopeArg>,

    /// Increase log verbosity. Repeat for more.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Cli {
    /// `Some(true)` / `Some(false)` / `None` for the front-matter toggle, so
    /// the config file can still supply a value when neither flag was given.
    pub fn front_matter_flag(&self) -> Option<bool> {
        if self.no_front_matter {
            Some(false)
        } else if self.front_matter {
            Some(true)
        } else {
            None
        }
    }
}

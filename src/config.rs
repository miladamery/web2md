//! Resolved run settings: config file, overridden by the command line.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::{Cli, LinkStyle, ScopeArg};
use crate::error::{Error, Result};
use crate::profile::{self, Profile};
use crate::urlutil::Scope;

pub const DEFAULT_CONCURRENCY: usize = 8;
pub const DEFAULT_DELAY_MS: u64 = 150;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_DEPTH: usize = 6;
pub const DEFAULT_USER_AGENT: &str =
    "web2md/0.1 (+https://github.com/; documentation-to-markdown crawler)";

/// The `web2md.toml` shape. Every field is optional so a config can set only
/// what it cares about.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub output: Option<PathBuf>,
    pub concurrency: Option<usize>,
    pub delay_ms: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub user_agent: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_common: Option<bool>,
    pub max_depth: Option<usize>,
    pub limit: Option<usize>,
    pub keep_images: Option<bool>,
    pub link_style: Option<LinkStyle>,
    pub front_matter: Option<bool>,
    pub merge: Option<bool>,
    pub skip_existing: Option<bool>,
    pub scope: Option<ScopeArg>,
    pub respect_robots: Option<bool>,
    pub profile: Option<String>,
    /// Additional site profiles, or overrides of built-ins by name.
    #[serde(rename = "site")]
    pub sites: Vec<Profile>,
}

/// `LinkStyle` and `ScopeArg` come from clap, which does not derive serde.
/// Rather than pull serde into the CLI module, deserialize through a string.
impl<'de> Deserialize<'de> for LinkStyle {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "relative" => Ok(LinkStyle::Relative),
            "absolute" => Ok(LinkStyle::Absolute),
            "keep" => Ok(LinkStyle::Keep),
            other => Err(serde::de::Error::custom(format!(
                "link_style must be one of relative|absolute|keep, got {other:?}"
            ))),
        }
    }
}

impl Serialize for LinkStyle {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            LinkStyle::Relative => "relative",
            LinkStyle::Absolute => "absolute",
            LinkStyle::Keep => "keep",
        })
    }
}

impl<'de> Deserialize<'de> for ScopeArg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "host" => Ok(ScopeArg::Host),
            "prefix" => Ok(ScopeArg::Prefix),
            "any" => Ok(ScopeArg::Any),
            other => Err(serde::de::Error::custom(format!(
                "scope must be one of host|prefix|any, got {other:?}"
            ))),
        }
    }
}

impl Serialize for ScopeArg {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            ScopeArg::Host => "host",
            ScopeArg::Prefix => "prefix",
            ScopeArg::Any => "any",
        })
    }
}

/// Everything the pipeline needs, with every default already applied.
#[derive(Debug, Clone)]
pub struct Settings {
    pub seeds: Vec<Url>,
    pub output_dir: PathBuf,
    pub sitemap: Option<String>,
    pub urls_file: Option<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_common: bool,
    pub scope: Scope,
    pub max_depth: usize,
    pub limit: Option<usize>,
    pub dry_run: bool,
    pub concurrency: usize,
    pub delay_ms: u64,
    pub timeout_secs: u64,
    pub user_agent: String,
    pub respect_robots: bool,
    pub keep_images: bool,
    pub link_style: LinkStyle,
    pub front_matter: bool,
    pub merge: bool,
    pub skip_existing: bool,
    pub explicit_profile: Option<String>,
    pub profiles: Vec<Profile>,
    pub verbose: u8,
}

impl Settings {
    /// Merge CLI over config file. Anything given on the command line wins.
    pub fn resolve(cli: &Cli, file: FileConfig) -> Result<Self> {
        if cli.seeds.is_empty() && cli.sitemap.is_none() && cli.urls.is_none() {
            return Err(Error::Config(
                "give at least one seed URL, or --sitemap/--urls".into(),
            ));
        }

        let mut seeds = Vec::new();
        for raw in &cli.seeds {
            seeds.push(Url::parse(raw).map_err(|e| Error::BadUrl(format!("{raw}: {e}")))?);
        }

        // A sitemap-only run still needs an origin for scope filtering, so
        // derive one from the sitemap URL when no seed was given.
        if seeds.is_empty() {
            if let Some(s) = cli.sitemap.as_ref() {
                let u = Url::parse(s).map_err(|e| Error::BadUrl(format!("{s}: {e}")))?;
                seeds.push(u);
            }
        }

        let output_dir = cli
            .output
            .clone()
            .or(file.output)
            .unwrap_or_else(|| PathBuf::from("web2md-out"));

        // CLI filters are additive on top of the file's, not replacing: a
        // config that pins the site's shape plus a one-off `--exclude` for
        // today's run is the common case.
        let mut include = file.include.clone();
        include.extend(cli.include.iter().cloned());
        let mut exclude = file.exclude.clone();
        exclude.extend(cli.exclude.iter().cloned());

        let scope = cli
            .scope
            .or(file.scope)
            .unwrap_or(ScopeArg::Host);

        Ok(Self {
            seeds,
            output_dir,
            sitemap: cli.sitemap.clone(),
            urls_file: cli.urls.clone(),
            include,
            exclude,
            exclude_common: cli.exclude_common || file.exclude_common.unwrap_or(true),
            scope: match scope {
                ScopeArg::Host => Scope::Host,
                ScopeArg::Prefix => Scope::Prefix,
                ScopeArg::Any => Scope::Any,
            },
            max_depth: cli.max_depth.or(file.max_depth).unwrap_or(DEFAULT_MAX_DEPTH),
            limit: cli.limit.or(file.limit),
            dry_run: cli.dry_run,
            concurrency: cli
                .concurrency
                .or(file.concurrency)
                .unwrap_or(DEFAULT_CONCURRENCY)
                .max(1),
            delay_ms: cli.delay.or(file.delay_ms).unwrap_or(DEFAULT_DELAY_MS),
            timeout_secs: cli
                .timeout
                .or(file.timeout_secs)
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
            user_agent: cli
                .user_agent
                .clone()
                .or(file.user_agent)
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            respect_robots: !cli.no_robots && file.respect_robots.unwrap_or(true),
            keep_images: cli.keep_images || file.keep_images.unwrap_or(false),
            link_style: cli.link_style.or(file.link_style).unwrap_or(LinkStyle::Relative),
            front_matter: cli
                .front_matter_flag()
                .or(file.front_matter)
                .unwrap_or(true),
            merge: cli.merge || file.merge.unwrap_or(false),
            skip_existing: cli.skip_existing || file.skip_existing.unwrap_or(false),
            explicit_profile: cli.profile.clone().or(file.profile),
            profiles: merge_profiles(file.sites),
            verbose: cli.verbose,
        })
    }

    /// The URL that defines the crawl origin.
    pub fn origin(&self) -> Option<&Url> {
        self.seeds.first()
    }

    /// Load `web2md.toml`, or the path the user named. A named path that does
    /// not exist is an error; the implicit default simply means "no config".
    pub fn load_file(explicit: Option<&Path>) -> Result<FileConfig> {
        match explicit {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| Error::Config(format!("{}: {e}", p.display())))?;
                toml::from_str(&text)
                    .map_err(|e| Error::Config(format!("{}: {e}", p.display())))
            }
            None => {
                let default = Path::new("web2md.toml");
                if default.exists() {
                    let text = std::fs::read_to_string(default)
                        .map_err(|e| Error::Config(format!("web2md.toml: {e}")))?;
                    toml::from_str(&text)
                        .map_err(|e| Error::Config(format!("web2md.toml: {e}")))
                } else {
                    Ok(FileConfig::default())
                }
            }
        }
    }
}

/// User profiles first, so one can override a built-in by reusing its name or
/// by claiming a host the built-in also claims. Built-ins follow, then the
/// generic fallback is last by construction.
fn merge_profiles(user: Vec<Profile>) -> Vec<Profile> {
    let builtins = profile::builtins();
    // Owned names: holding &str into `user` would keep it borrowed and block
    // the move below.
    let overridden: Vec<String> = user.iter().map(|p| p.name.clone()).collect();
    let mut out = user;
    out.extend(
        builtins
            .into_iter()
            .filter(|b| !overridden.iter().any(|n| n == &b.name)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        use clap::Parser;
        let mut full = vec!["web2md"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn cli_beats_config_file() {
        let file = FileConfig {
            concurrency: Some(2),
            keep_images: Some(true),
            ..Default::default()
        };
        let s = Settings::resolve(&cli(&["https://x.dev/", "--concurrency", "16"]), file).unwrap();
        assert_eq!(s.concurrency, 16, "CLI should win");
        // Unset on the CLI, so the file's value survives.
        assert!(s.keep_images);
    }

    #[test]
    fn filters_from_file_and_cli_are_both_kept() {
        let file = FileConfig {
            exclude: vec![r"/old/".into()],
            ..Default::default()
        };
        let s = Settings::resolve(
            &cli(&["https://x.dev/", "--exclude", "/draft/"]),
            file,
        )
        .unwrap();
        assert_eq!(s.exclude, vec!["/old/", "/draft/"]);
    }

    #[test]
    fn front_matter_flag_tristate() {
        let s = Settings::resolve(&cli(&["https://x.dev/"]), FileConfig::default()).unwrap();
        assert!(s.front_matter, "on by default");

        let s = Settings::resolve(&cli(&["https://x.dev/", "--no-front-matter"]), FileConfig::default())
            .unwrap();
        assert!(!s.front_matter);

        // Explicit false in the file is honoured when no flag is given.
        let file = FileConfig { front_matter: Some(false), ..Default::default() };
        let s = Settings::resolve(&cli(&["https://x.dev/"]), file).unwrap();
        assert!(!s.front_matter);
    }

    #[test]
    fn user_profile_overrides_a_builtin_of_the_same_name() {
        let user = Profile {
            name: "mkdocs".into(),
            hosts: vec!["docs.example.com".into()],
            generators: vec![],
            content_selectors: vec![".custom".into()],
            title_selector: None,
            prune_selectors: vec![],
            transform_dl_tabs: true,
        };
        let merged = merge_profiles(vec![user]);
        assert_eq!(merged.iter().filter(|p| p.name == "mkdocs").count(), 1);
        assert_eq!(merged[0].hosts, vec!["docs.example.com"]);
        // Built-ins that were not overridden are still present.
        assert!(merged.iter().any(|p| p.name == "paradox"));
    }

    #[test]
    fn rejects_a_run_with_no_input_at_all() {
        let err = Settings::resolve(&cli(&[]), FileConfig::default()).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }
}

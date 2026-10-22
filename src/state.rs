//! Resumability state: what we wrote last time, and what the server said
//! about it.
//!
//! Recording the ETag and Last-Modified lets a re-run ask for the page
//! conditionally. On a thousand-page site that turns a full re-download into a
//! thousand cheap `304`s, which is the difference between a re-index being
//! routine and being something you avoid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};
use crate::urlutil::canonical_key;

const STATE_VERSION: u32 = 1;

/// Where the state file lives, relative to the output directory.
pub const STATE_DIR: &str = ".web2md";
pub const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    /// Output path, relative to the output root, with forward slashes.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    version: u32,
    #[serde(default)]
    pub entries: HashMap<String, Entry>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            entries: HashMap::new(),
        }
    }
}

impl State {
    /// Load state, or start fresh.
    ///
    /// A missing, unreadable, or stale-version file is never fatal: the worst
    /// outcome is a full re-crawl, which is exactly what would have happened
    /// without the file. It is reported so a persistent problem is visible.
    pub fn load(output_dir: &Path) -> Self {
        let path = state_path(output_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<State>(&text) {
            Ok(s) if s.version == STATE_VERSION => s,
            Ok(s) => {
                tracing::warn!(
                    "ignoring state file with version {} (expected {STATE_VERSION})",
                    s.version
                );
                Self::default()
            }
            Err(e) => {
                tracing::warn!("ignoring unreadable state file {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Write atomically: a run interrupted mid-save must not leave a truncated
    /// file that the next run then discards.
    pub fn save(&self, output_dir: &Path) -> Result<()> {
        let path = state_path(output_dir);
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Other(format!("state serialization: {e}")))?;

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| Error::Other(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
        Ok(())
    }

    pub fn get(&self, url: &Url) -> Option<&Entry> {
        self.entries.get(&canonical_key(url))
    }

    pub fn insert(&mut self, url: &Url, entry: Entry) {
        self.entries.insert(canonical_key(url), entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn state_path(output_dir: &Path) -> PathBuf {
    output_dir.join(STATE_DIR).join(STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = State::default();
        s.insert(
            &u("https://x.dev/a.html"),
            Entry {
                path: "a.md".into(),
                etag: Some("\"abc\"".into()),
                last_modified: None,
            },
        );
        s.save(dir.path()).unwrap();

        let loaded = State::load(dir.path());
        let e = loaded.get(&u("https://x.dev/a.html")).unwrap();
        assert_eq!(e.path, "a.md");
        assert_eq!(e.etag.as_deref(), Some("\"abc\""));
        assert!(e.last_modified.is_none());
    }

    #[test]
    fn keys_are_canonicalized() {
        let mut s = State::default();
        s.insert(
            &u("https://x.dev/a.html"),
            Entry {
                path: "a.md".into(),
                ..Default::default()
            },
        );
        // A tracking param and a fragment are the same page.
        assert!(s.get(&u("https://x.dev/a.html?utm_source=nl")).is_some());
        assert!(s.get(&u("https://x.dev/a.html#top")).is_some());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(State::load(dir.path()).is_empty());
    }

    #[test]
    fn corrupt_state_falls_back_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        std::fs::write(state_path(dir.path()), "{ not json").unwrap();
        assert!(State::load(dir.path()).is_empty());
    }

    #[test]
    fn a_future_version_is_discarded_not_misread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        std::fs::write(
            state_path(dir.path()),
            r#"{"version": 99, "entries": {"k": {"path": "x.md"}}}"#,
        )
        .unwrap();
        assert!(State::load(dir.path()).is_empty());
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        State::default().save(dir.path()).unwrap();
        assert!(state_path(dir.path()).exists());
        assert!(!state_path(dir.path()).with_extension("json.tmp").exists());
    }
}

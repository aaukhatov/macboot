//! Machine-local state: the record of every symlink macboot owns and every file
//! it backed up. Lives outside the config repo (defaults to
//! `~/.local/state/macboot/state.json`) so that `unlink` only ever touches links
//! we created, and clobbered files can be restored.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One managed symlink.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRecord {
    /// The stow package this link belongs to (e.g. "zsh").
    pub package: String,
    /// Absolute link target in $HOME (the symlink itself).
    pub target: PathBuf,
    /// Absolute path the symlink points at (inside the config repo).
    pub source: PathBuf,
    /// If we backed up a pre-existing file at `target`, where it went.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    /// ISO-ish timestamp the link was created.
    pub created: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Keyed by the absolute target path (as a string) for quick lookup.
    #[serde(default)]
    pub links: BTreeMap<String, LinkRecord>,
    #[serde(skip)]
    path: PathBuf,
}

impl State {
    /// Default state file location, overridable via `$MACBOOT_STATE`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("MACBOOT_STATE") {
            return PathBuf::from(p);
        }
        let base = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("macboot").join("state.json")
    }

    /// Load state from `path`, returning an empty state if the file is absent.
    pub fn load(path: &Path) -> Result<State> {
        if !path.exists() {
            return Ok(State {
                path: path.to_path_buf(),
                ..Default::default()
            });
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading state file {}", path.display()))?;
        let mut state: State = serde_json::from_str(&text)
            .with_context(|| format!("parsing state file {}", path.display()))?;
        state.path = path.to_path_buf();
        Ok(state)
    }

    /// Persist state atomically (write to temp, then rename).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("committing {}", self.path.display()))?;
        Ok(())
    }

    pub fn insert(&mut self, record: LinkRecord) {
        self.links
            .insert(record.target.to_string_lossy().into_owned(), record);
    }

    pub fn get(&self, target: &Path) -> Option<&LinkRecord> {
        self.links.get(&target.to_string_lossy().into_owned())
    }

    pub fn remove(&mut self, target: &Path) -> Option<LinkRecord> {
        self.links.remove(&target.to_string_lossy().into_owned())
    }

    /// All records belonging to a given package.
    pub fn for_package<'a>(&'a self, package: &'a str) -> impl Iterator<Item = &'a LinkRecord> {
        self.links.values().filter(move |r| r.package == package)
    }
}

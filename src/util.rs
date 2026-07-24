//! Small shared helpers with no better home.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Write a generated file, creating parent directories as needed. On a dry run
/// the body goes to stdout (data only) and the destination is announced on
/// stderr, so `… --dry-run > file` still produces a clean file.
///
/// Every `dump` command lands here, so they can't drift apart.
pub fn write_generated(target: &Path, body: &str, dry: bool) -> Result<()> {
    if dry {
        print!("{body}");
        crate::ui::info(format!("(dry-run) would write {}", tildify(target)));
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(target, body).with_context(|| format!("writing {}", target.display()))
}

/// Expand a leading `~` or `~/` to the user's home directory. Paths without a
/// leading tilde are returned unchanged.
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

/// Render a path with the home directory collapsed back to `~` for display.
pub fn tildify(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// A coarse timestamp (Unix seconds) suitable for state-file bookkeeping.
/// Avoids pulling in a date/time crate for what is only a human breadcrumb.
pub fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

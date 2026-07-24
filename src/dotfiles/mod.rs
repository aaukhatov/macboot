//! Native symlink engine — the stow replacement.
//!
//! Each top-level directory under `dotfiles/` is a package whose tree mirrors
//! `$HOME`. For every file leaf we plan a symlink `~/<rel> -> <repo>/<pkg>/<rel>`.
//! Unlike stow, every link we create is recorded in the state file, so `unlink`
//! only removes links we own, and any pre-existing file we displace is backed up
//! and restored on unlink.

use crate::config::Config;
use crate::state::{LinkRecord, State};
use crate::ui::{self, Status, Summary};
use crate::util;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// What linking a single target would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No target present — create the symlink (and any parent dirs).
    Create,
    /// The correct symlink already exists — nothing to do.
    AlreadyLinked,
    /// A macboot-owned symlink points at the wrong place — repoint it.
    Repoint,
    /// A foreign file/dir/symlink occupies the target — back it up, then link.
    Backup,
}

impl Action {
    fn status(self) -> Status {
        match self {
            Action::AlreadyLinked => Status::Unchanged,
            _ => Status::Changed,
        }
    }
}

/// A single planned link operation.
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub package: String,
    pub target: PathBuf,
    pub source: PathBuf,
    pub action: Action,
}

/// Compute the link plan for the given packages (all of `cfg.dotfiles` if empty).
pub fn plan(cfg: &Config, state: &State, packages: &[String]) -> Result<Vec<PlanItem>> {
    let selected = select_packages(cfg, packages)?;
    let home = home_dir()?;
    let mut items = Vec::new();
    for pkg in selected {
        let pkg_dir = cfg.dotfiles_dir().join(&pkg);
        for entry in WalkDir::new(&pkg_dir).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", pkg_dir.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let source = entry.path().to_path_buf();
            let rel = source
                .strip_prefix(&pkg_dir)
                .expect("walked path is under pkg_dir");
            let target = home.join(rel);
            let action = classify(&target, &source, state);
            items.push(PlanItem {
                package: pkg.clone(),
                target,
                source,
                action,
            });
        }
    }
    items.sort_by(|a, b| a.target.cmp(&b.target));
    Ok(items)
}

/// Decide the action for one target given the current filesystem + state.
fn classify(target: &Path, source: &Path, state: &State) -> Action {
    match std::fs::symlink_metadata(target) {
        Err(_) => Action::Create, // nothing there
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                match std::fs::read_link(target) {
                    Ok(dest) if dest == source => Action::AlreadyLinked,
                    _ if state.get(target).is_some() => Action::Repoint,
                    _ => Action::Backup, // foreign symlink
                }
            } else {
                Action::Backup // regular file or directory
            }
        }
    }
}

/// `link`: execute the plan, creating symlinks and recording them.
pub fn link(cfg: &Config, state: &mut State, packages: &[String], dry: bool) -> Result<Summary> {
    let items = plan(cfg, state, packages)?;
    let mut summary = Summary::new();
    if items.is_empty() {
        ui::warn("No dotfile packages to link.");
        return Ok(summary);
    }
    ui::heading(format!("Linking {} file(s)", items.len()));
    for item in items {
        let label = format!(
            "{} ({})",
            util::tildify(&item.target),
            action_label(item.action)
        );
        if item.action == Action::AlreadyLinked {
            summary.record(Status::Unchanged, &label);
            continue;
        }
        if dry {
            summary.record(Status::Changed, &format!("{label} [dry-run]"));
            continue;
        }
        match apply_one(&item, state) {
            Ok(()) => summary.record(Status::Changed, &label),
            Err(e) => {
                ui::err(format!("{}: {e:#}", util::tildify(&item.target)));
                summary.record(Status::Failed, &label);
            }
        }
    }
    if !dry {
        state.save()?;
    }
    Ok(summary)
}

/// Perform one link operation, backing up any displaced file and updating state.
fn apply_one(item: &PlanItem, state: &mut State) -> Result<()> {
    if let Some(parent) = item.target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut backup = None;
    match item.action {
        Action::Create => {}
        Action::Repoint => {
            // Our own link — safe to replace outright.
            std::fs::remove_file(&item.target)
                .with_context(|| format!("removing old link {}", item.target.display()))?;
        }
        Action::Backup => {
            backup = Some(back_up(&item.target, state)?);
        }
        Action::AlreadyLinked => return Ok(()),
    }

    std::os::unix::fs::symlink(&item.source, &item.target).with_context(|| {
        format!(
            "linking {} -> {}",
            item.target.display(),
            item.source.display()
        )
    })?;

    state.insert(LinkRecord {
        package: item.package.clone(),
        target: item.target.clone(),
        source: item.source.clone(),
        backup,
        created: util::now_stamp(),
    });
    Ok(())
}

/// Move a foreign file to the backup area and return its new path.
fn back_up(target: &Path, state: &State) -> Result<PathBuf> {
    let dir = backups_dir(state).join(util::now_stamp());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "file".into());
    let dest = dir.join(name);
    std::fs::rename(target, &dest)
        .with_context(|| format!("backing up {} -> {}", target.display(), dest.display()))?;
    ui::detail(format!(
        "backed up {} -> {}",
        util::tildify(target),
        util::tildify(&dest)
    ));
    Ok(dest)
}

/// `unlink`: remove only macboot-owned links for the given packages and restore
/// any backups. Never touches files it does not own.
pub fn unlink(cfg: &Config, state: &mut State, packages: &[String], dry: bool) -> Result<Summary> {
    let selected = select_packages(cfg, packages)?;
    let mut summary = Summary::new();
    let targets: Vec<LinkRecord> = selected
        .iter()
        .flat_map(|pkg| state.for_package(pkg).cloned().collect::<Vec<_>>())
        .collect();

    if targets.is_empty() {
        ui::warn("No owned links found for the selected packages.");
        return Ok(summary);
    }
    ui::heading(format!("Unlinking {} link(s)", targets.len()));
    for rec in targets {
        let label = util::tildify(&rec.target);
        if dry {
            summary.record(Status::Changed, &format!("{label} [dry-run]"));
            continue;
        }
        match remove_one(&rec) {
            Ok(()) => {
                state.remove(&rec.target);
                summary.record(Status::Changed, &label);
            }
            Err(e) => {
                ui::err(format!("{label}: {e:#}"));
                summary.record(Status::Failed, &label);
            }
        }
    }
    if !dry {
        state.save()?;
    }
    Ok(summary)
}

fn remove_one(rec: &LinkRecord) -> Result<()> {
    // Only remove the link if it is still our symlink.
    if let Ok(meta) = std::fs::symlink_metadata(&rec.target) {
        if meta.file_type().is_symlink() {
            if let Ok(dest) = std::fs::read_link(&rec.target) {
                if dest == rec.source {
                    std::fs::remove_file(&rec.target)
                        .with_context(|| format!("removing {}", rec.target.display()))?;
                }
            }
        }
    }
    // Restore a backup if we have one and the slot is now free.
    if let Some(backup) = &rec.backup {
        if backup.exists() && !rec.target.exists() {
            std::fs::rename(backup, &rec.target).with_context(|| {
                format!(
                    "restoring backup {} -> {}",
                    backup.display(),
                    rec.target.display()
                )
            })?;
            ui::detail(format!("restored backup to {}", util::tildify(&rec.target)));
        }
    }
    Ok(())
}

/// `relink`: unlink then link (after files have moved within a package).
pub fn relink(cfg: &Config, state: &mut State, packages: &[String], dry: bool) -> Result<Summary> {
    let mut summary = unlink(cfg, state, packages, dry)?;
    summary.merge(link(cfg, state, packages, dry)?);
    Ok(summary)
}

/// `status`: read-only report of what `link` would do.
pub fn status(cfg: &Config, state: &State, packages: &[String]) -> Result<Summary> {
    let items = plan(cfg, state, packages)?;
    let mut summary = Summary::new();
    ui::heading("Dotfiles");
    if items.is_empty() {
        ui::detail("(no packages)");
        return Ok(summary);
    }
    for item in items {
        let label = format!(
            "{} ({})",
            util::tildify(&item.target),
            action_label(item.action)
        );
        summary.record(item.action.status(), &label);
    }
    Ok(summary)
}

/// `adopt`: move an existing `~/…` file into package `pkg` (preserving its path
/// relative to `$HOME`), then link it back.
pub fn adopt(
    cfg: &Config,
    state: &mut State,
    pkg: &str,
    file: &Path,
    dry: bool,
) -> Result<Summary> {
    let home = home_dir()?;
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        home.join(file)
    };
    let rel = abs
        .strip_prefix(&home)
        .with_context(|| format!("{} is not under $HOME", abs.display()))?;
    let meta = std::fs::symlink_metadata(&abs)
        .with_context(|| format!("{} does not exist", abs.display()))?;
    if meta.file_type().is_symlink() {
        bail!("{} is already a symlink; nothing to adopt", abs.display());
    }
    let dest = cfg.dotfiles_dir().join(pkg).join(rel);
    let mut summary = Summary::new();
    let label = format!("{} -> {}/{}", util::tildify(&abs), pkg, rel.display());
    if dry {
        summary.record(Status::Changed, &format!("{label} [dry-run]"));
        return Ok(summary);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&abs, &dest).with_context(|| format!("moving {} into repo", abs.display()))?;
    std::os::unix::fs::symlink(&dest, &abs)
        .with_context(|| format!("linking {} back", abs.display()))?;
    state.insert(LinkRecord {
        package: pkg.to_string(),
        target: abs,
        source: dest,
        backup: None,
        created: util::now_stamp(),
    });
    state.save()?;
    summary.record(Status::Changed, &label);
    Ok(summary)
}

// ---- helpers ---------------------------------------------------------------

fn select_packages(cfg: &Config, packages: &[String]) -> Result<Vec<String>> {
    if packages.is_empty() {
        return Ok(cfg.dotfiles.clone());
    }
    for p in packages {
        if !cfg.dotfiles_dir().join(p).is_dir() {
            bail!(
                "unknown dotfiles package '{p}' (no directory under {})",
                util::tildify(&cfg.dotfiles_dir())
            );
        }
    }
    Ok(packages.to_vec())
}

fn action_label(action: Action) -> &'static str {
    match action {
        Action::Create => "new",
        Action::AlreadyLinked => "ok",
        Action::Repoint => "repoint",
        Action::Backup => "backup+link",
    }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine home directory")
}

/// Backups live next to the state file so they travel with machine-local state.
fn backups_dir(_state: &State) -> PathBuf {
    State::default_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // These tests mutate the shared HOME env var, so they must not overlap.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build a minimal config rooted at `root` with one dotfiles package.
    fn scaffold(root: &Path, pkg: &str, rel: &str, contents: &str) -> Config {
        let file = root.join("dotfiles").join(pkg).join(rel);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, contents).unwrap();
        Config::load(root, Some("personal")).unwrap()
    }

    #[test]
    fn plan_creates_link_for_new_target() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        let cfg = scaffold(dir.path(), "git", ".gitconfig", "[user]\n");
        let state = State::default();
        let items = plan(&cfg, &state, &[]).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].action, Action::Create);
    }

    #[test]
    fn foreign_file_is_classified_as_backup() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        // A pre-existing ~/.gitconfig blocks the link.
        fs::write(home.join(".gitconfig"), "pre-existing").unwrap();
        let cfg = scaffold(dir.path(), "git", ".gitconfig", "managed");
        let state = State::default();
        let items = plan(&cfg, &state, &[]).unwrap();
        assert_eq!(items[0].action, Action::Backup);
    }
}

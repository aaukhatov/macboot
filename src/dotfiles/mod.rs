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
    /// A symlinked directory sits between `$HOME` and this target, so every
    /// filesystem call here writes *through* that link. Reported, never applied.
    Blocked,
}

impl Action {
    fn status(self) -> Status {
        match self {
            Action::AlreadyLinked => Status::Unchanged,
            Action::Blocked => Status::Skipped,
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
            let action = classify(&target, &source, state, &home);
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

/// The first symlinked directory between `home` and `target`, if any.
///
/// `symlink_metadata` only tells us about the final component: it happily
/// follows a symlinked *parent*. Without this check, a target under a linked
/// directory looks like an ordinary foreign file, and `link` would "back up"
/// the very file inside the config repo that the link points at.
fn symlinked_ancestor(target: &Path, home: &Path) -> Option<PathBuf> {
    let rel = target.strip_prefix(home).ok()?;
    let mut acc = home.to_path_buf();
    let mut components: Vec<_> = rel.components().collect();
    components.pop(); // the leaf itself is classified normally
    for component in components {
        acc.push(component);
        let is_link = std::fs::symlink_metadata(&acc)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            return Some(acc);
        }
    }
    None
}

/// Decide the action for one target given the current filesystem + state.
fn classify(target: &Path, source: &Path, state: &State, home: &Path) -> Action {
    if symlinked_ancestor(target, home).is_some() {
        // If the path already resolves to our source through that link, the
        // file is effectively linked; otherwise refuse to touch it.
        let resolved = (std::fs::canonicalize(target), std::fs::canonicalize(source));
        return match resolved {
            (Ok(t), Ok(s)) if t == s => Action::AlreadyLinked,
            _ => Action::Blocked,
        };
    }
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
        if item.action == Action::Blocked {
            summary.record(Status::Skipped, &label);
            ui::detail(format!(
                "a parent directory of {} is a symlink; adopt or unlink it first",
                util::tildify(&item.target)
            ));
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
        Action::Blocked => bail!("refusing to write through a symlinked parent directory"),
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

/// `adopt`: move an existing `~/…` path into package `pkg` (preserving its path
/// relative to `$HOME`), then link it back.
///
/// A directory is adopted as its individual file leaves, never as a single
/// directory symlink: the link engine links files, and a directory symlink
/// would leave every file under it unreachable to `plan` except *through* that
/// link (see [`symlinked_ancestor`]).
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
    let meta = std::fs::symlink_metadata(&abs)
        .with_context(|| format!("{} does not exist", abs.display()))?;
    if meta.file_type().is_symlink() {
        bail!("{} is already a symlink; nothing to adopt", abs.display());
    }
    if let Some(link) = symlinked_ancestor(&abs, &home) {
        bail!(
            "{} lives under the symlink {}; unlink it before adopting",
            util::tildify(&abs),
            util::tildify(&link)
        );
    }

    let mut summary = Summary::new();
    let files: Vec<PathBuf> = if meta.is_dir() {
        let mut found = Vec::new();
        for entry in WalkDir::new(&abs).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", abs.display()))?;
            if entry.file_type().is_file() {
                found.push(entry.path().to_path_buf());
            }
        }
        found.sort();
        if found.is_empty() {
            ui::warn(format!("{} contains no files", util::tildify(&abs)));
            return Ok(summary);
        }
        ui::detail(format!(
            "{} is a directory; adopting its {} file(s)",
            util::tildify(&abs),
            found.len()
        ));
        found
    } else {
        vec![abs]
    };

    for path in &files {
        summary.merge(adopt_file(cfg, state, pkg, path, &home, dry)?);
    }
    if !dry {
        state.save()?;
    }
    Ok(summary)
}

/// Adopt exactly one file. Does not persist state — the caller saves once.
fn adopt_file(
    cfg: &Config,
    state: &mut State,
    pkg: &str,
    abs: &Path,
    home: &Path,
    dry: bool,
) -> Result<Summary> {
    let rel = abs
        .strip_prefix(home)
        .with_context(|| format!("{} is not under $HOME", abs.display()))?;
    let dest = cfg.dotfiles_dir().join(pkg).join(rel);
    let mut summary = Summary::new();
    let label = format!("{} -> {}/{}", util::tildify(abs), pkg, rel.display());
    if dry {
        summary.record(Status::Changed, &format!("{label} [dry-run]"));
        return Ok(summary);
    }
    if dest.exists() {
        ui::err(format!("{} already exists in the repo", dest.display()));
        summary.record(Status::Failed, &label);
        return Ok(summary);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(abs, &dest).with_context(|| format!("moving {} into repo", abs.display()))?;
    std::os::unix::fs::symlink(&dest, abs)
        .with_context(|| format!("linking {} back", abs.display()))?;
    state.insert(LinkRecord {
        package: pkg.to_string(),
        target: abs.to_path_buf(),
        source: dest,
        backup: None,
        created: util::now_stamp(),
    });
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
        Action::Blocked => "blocked: symlinked parent",
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

    #[test]
    fn file_under_symlinked_parent_is_blocked_not_backed_up() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        let cfg = scaffold(dir.path(), "nvim", ".config/nvim/init.lua", "managed");

        // ~/.config/nvim is a symlink to somewhere else entirely: every path
        // under it writes through the link, so we must not touch it.
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".config/nvim")).unwrap();

        let items = plan(&cfg, &State::default(), &[]).unwrap();
        assert_eq!(items[0].action, Action::Blocked);
    }

    #[test]
    fn adopting_a_directory_links_each_file_and_survives_relink() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let nvim = home.join(".config/nvim");
        fs::create_dir_all(&nvim).unwrap();
        fs::write(nvim.join("init.lua"), "REAL CONTENT").unwrap();
        std::env::set_var("HOME", &home);

        fs::create_dir_all(dir.path().join("dotfiles/nvim")).unwrap();
        let cfg = Config::load(dir.path(), Some("personal")).unwrap();
        let mut state = State::load(&dir.path().join("state.json")).unwrap();

        adopt(&cfg, &mut state, "nvim", Path::new(".config/nvim"), false).unwrap();
        // The directory itself must NOT have become a symlink.
        assert!(fs::symlink_metadata(&nvim).unwrap().is_dir());

        // A follow-up link is a no-op, and the content is still reachable.
        let items = plan(&cfg, &state, &[]).unwrap();
        assert_eq!(items[0].action, Action::AlreadyLinked);
        link(&cfg, &mut state, &[], false).unwrap();
        assert_eq!(
            fs::read_to_string(nvim.join("init.lua")).unwrap(),
            "REAL CONTENT"
        );
    }
}

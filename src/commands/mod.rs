//! Command handlers. Thin glue between the parsed CLI and the feature modules.

use crate::config::{Config, OnError};
use crate::ui::{self, Status, Summary};
use crate::{dotfiles, macos, pkg, profile, state::State};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Options common to every command that needs config.
pub struct Ctx {
    pub config_dir: PathBuf,
    pub profile: Option<String>,
    pub assume_yes: bool,
}

impl Ctx {
    fn load(&self) -> Result<Config> {
        Config::load(&self.config_dir, self.profile.as_deref())
    }
    fn state(&self) -> Result<State> {
        State::load(&State::default_path())
    }
}

fn split_only(only: Option<&str>) -> Option<Vec<String>> {
    only.map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
}

// ---- apply / status / diff -------------------------------------------------

pub fn apply(ctx: &Ctx, only: Option<&str>, dry: bool, skip_missing: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    ui::info(format!(
        "macboot apply — profile '{}'{}",
        cfg.active_profile,
        if dry { " (dry-run)" } else { "" }
    ));

    let problems = cfg.validate();
    if !problems.is_empty() {
        for p in &problems {
            ui::warn(p);
        }
    }

    let stages: Vec<String> = match split_only(only) {
        Some(list) => list,
        None => cfg.apply.stages.clone(),
    };

    let mut summary = Summary::new();
    for stage in &stages {
        let stage_summary = match stage.as_str() {
            "packages" | "pkgs" => {
                let providers = pkg::registry(&cfg.packages);
                pkg::preflight(&providers, skip_missing)?;
                let selected = pkg::select(&providers, None)?;
                pkg::apply(&selected, dry)?
            }
            "dotfiles" => dotfiles::link(&cfg, &mut state, &[], dry)?,
            "macos" => macos::apply(&cfg, &mut state, None, dry)?,
            other => bail!("unknown stage '{other}' (valid: packages, dotfiles, macos)"),
        };
        let failed = stage_summary.any_failed();
        summary.merge(stage_summary);
        if failed && cfg.apply.on_error == OnError::Abort {
            ui::err(format!("aborting: stage '{stage}' had failures"));
            break;
        }
    }

    summary.render();
    if summary.any_failed() {
        bail!("apply completed with failures");
    }
    Ok(())
}

pub fn status(ctx: &Ctx) -> Result<()> {
    let cfg = ctx.load()?;
    let state = ctx.state()?;
    let machine = if cfg.meta.name.is_empty() {
        String::new()
    } else {
        format!("{} · ", cfg.meta.name)
    };
    ui::info(format!(
        "{machine}Status — profile '{}'",
        cfg.active_profile
    ));

    let mut summary = Summary::new();
    summary.merge(dotfiles::status(&cfg, &state, &[])?);

    ui::heading("Packages");
    let providers = pkg::registry(&cfg.packages);
    for p in &providers {
        let avail = if p.is_available() {
            "available"
        } else {
            "MISSING"
        };
        ui::detail(format!("{} — {}", p.name(), avail));
    }

    summary.merge(macos::diff(&cfg, None)?);
    summary.render();
    Ok(())
}

pub fn diff(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let cfg = ctx.load()?;
    let state = ctx.state()?;
    let filter = split_only(only);
    let mut summary = Summary::new();

    if filter.is_none() || filter.as_ref().unwrap().iter().any(|s| s == "dotfiles") {
        summary.merge(dotfiles::status(&cfg, &state, &[])?);
    }
    if filter.is_none() || filter.as_ref().unwrap().iter().any(|s| s == "packages") {
        let providers = pkg::registry(&cfg.packages);
        let selected = pkg::select(&providers, None)?;
        pkg::report_diff(&selected)?;
    }
    if filter.is_none() || filter.as_ref().unwrap().iter().any(|s| s == "macos") {
        summary.merge(macos::diff(&cfg, None)?);
    }
    summary.render();
    Ok(())
}

// ---- dotfiles --------------------------------------------------------------

pub fn link(ctx: &Ctx, packages: &[String], dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let summary = dotfiles::link(&cfg, &mut state, packages, dry)?;
    summary.render();
    Ok(())
}

pub fn unlink(ctx: &Ctx, packages: &[String], dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let summary = dotfiles::unlink(&cfg, &mut state, packages, dry)?;
    summary.render();
    Ok(())
}

pub fn relink(ctx: &Ctx, packages: &[String], dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let summary = dotfiles::relink(&cfg, &mut state, packages, dry)?;
    summary.render();
    Ok(())
}

pub fn adopt(ctx: &Ctx, package: &str, files: &[PathBuf], dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let mut summary = Summary::new();
    for f in files {
        // Allow `~/...` arguments.
        let expanded = crate::util::expand_tilde(&f.to_string_lossy());
        summary.merge(dotfiles::adopt(&cfg, &mut state, package, &expanded, dry)?);
    }
    summary.render();
    Ok(())
}

// ---- packages --------------------------------------------------------------

pub fn pkg_apply(ctx: &Ctx, only: Option<&str>, dry: bool, skip_missing: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let providers = pkg::registry(&cfg.packages);
    pkg::preflight(&providers, skip_missing)?;
    let selected = pkg::select(&providers, only)?;
    let summary = pkg::apply(&selected, dry)?;
    summary.render();
    Ok(())
}

pub fn pkg_diff(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let cfg = ctx.load()?;
    let providers = pkg::registry(&cfg.packages);
    let selected = pkg::select(&providers, only)?;
    let any = pkg::report_diff(&selected)?;
    if any {
        // Non-zero exit lets CI/pre-commit gate drift.
        std::process::exit(1);
    }
    Ok(())
}

pub fn pkg_clean(ctx: &Ctx, only: Option<&str>, dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let providers = pkg::registry(&cfg.packages);
    let selected = pkg::select(&providers, only)?;
    if !dry && !ui::ask("Remove extraneous packages?", ctx.assume_yes) {
        ui::warn("Aborted.");
        return Ok(());
    }
    let summary = pkg::clean(&selected, dry)?;
    summary.render();
    Ok(())
}

pub fn pkg_dump(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let cfg = ctx.load()?;
    let providers = pkg::registry(&cfg.packages);
    let selected = pkg::select(&providers, only)?;
    pkg::dump(&selected)
}

pub fn pkg_list(ctx: &Ctx) -> Result<()> {
    let cfg = ctx.load()?;
    let providers = pkg::registry(&cfg.packages);
    ui::heading("Declared providers");
    for p in &providers {
        let status = if p.is_available() {
            Status::Unchanged
        } else {
            Status::Failed
        };
        let avail = if p.is_available() {
            "available"
        } else {
            "missing"
        };
        let req = if p.required() { "required" } else { "optional" };
        ui::info(format!(
            "{} {} — {}, {}",
            status.glyph(),
            p.name(),
            avail,
            req
        ));
    }
    Ok(())
}

// ---- macos / keyboard ------------------------------------------------------

pub fn macos_apply(ctx: &Ctx, only: Option<&str>, dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let filter = split_only(only);
    let summary = macos::apply(&cfg, &mut state, filter.as_deref(), dry)?;
    summary.render();
    Ok(())
}

pub fn macos_revert(ctx: &Ctx, domain: Option<&str>, dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let mut state = ctx.state()?;
    let scope = domain.unwrap_or("all recorded domains");
    if !dry && !ui::ask(&format!("Revert macOS defaults ({scope})?"), ctx.assume_yes) {
        ui::warn("Aborted.");
        return Ok(());
    }
    let summary = macos::revert(&cfg, &mut state, domain, dry)?;
    summary.render();
    Ok(())
}

pub fn macos_diff(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let cfg = ctx.load()?;
    let filter = split_only(only);
    let summary = macos::diff(&cfg, filter.as_deref())?;
    summary.render();
    Ok(())
}

pub fn macos_record(ctx: &Ctx, domains: &[String], output: Option<&str>, all: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let changes = macos::record::record(domains, all)?;
    if changes.is_empty() {
        ui::warn("No settings changed between the two snapshots.");
        ui::detail("If you did change something, re-run with --all to include filtered keys.");
        return Ok(());
    }
    ui::ok(format!("Recorded {} changed key(s)", changes.len()));

    let toml = macos::record::to_toml(&changes);
    let Some(name) = output else {
        // Data on stdout, commentary on stderr, so `> file` stays clean.
        print!("{toml}");
        return Ok(());
    };

    let target = cfg.root.join("macos").join(format!("{name}.toml"));
    if target.exists() {
        bail!(
            "{} already exists; pick another --output name or merge by hand",
            crate::util::tildify(&target)
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, toml).with_context(|| format!("writing {}", target.display()))?;
    ui::ok(format!("Wrote {}", crate::util::tildify(&target)));
    ui::detail(format!("Review it, then: macboot macos diff --only {name}"));
    Ok(())
}

pub fn keyboard_dump(ctx: &Ctx, stdout: bool, dry: bool) -> Result<()> {
    let cfg = ctx.load()?;
    let dumped = macos::keyboard::dump()?;
    let toml = macos::keyboard::to_toml(&dumped);
    let target = cfg.root.join("macos").join("keyboard.toml");
    if stdout || dry {
        print!("{toml}");
        if dry {
            ui::info(format!("(dry-run) would write {}", target.display()));
        }
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, toml)?;
    ui::ok(format!(
        "Wrote {} hotkey entries to {}",
        dumped.len(),
        crate::util::tildify(&target)
    ));
    Ok(())
}

pub fn keyboard_apply(ctx: &Ctx, dry: bool) -> Result<()> {
    macos_apply(ctx, Some("keyboard"), dry)
}

// ---- profile / doctor / capture -------------------------------------------

pub fn profile_show(ctx: &Ctx) -> Result<()> {
    let cfg = ctx.load()?;
    let (name, reason) = profile::resolve(&cfg.profile, ctx.profile.as_deref());
    ui::info(format!("Active profile: {name}"));
    ui::detail(&reason);
    if !cfg.vars.is_empty() {
        ui::heading("Variables");
        for (k, v) in &cfg.vars {
            ui::detail(format!("{k} = {v}"));
        }
    }
    Ok(())
}

pub fn doctor(ctx: &Ctx, _fix: bool) -> Result<()> {
    let mut summary = Summary::new();
    ui::heading("Self");

    // Config loads + validates.
    let cfg = match ctx.load() {
        Ok(c) => {
            summary.record(Status::Unchanged, "config parses");
            c
        }
        Err(e) => {
            ui::err(format!("config: {e:#}"));
            summary.record(Status::Failed, "config parses");
            summary.render();
            bail!("config could not be loaded");
        }
    };
    for problem in cfg.validate() {
        ui::warn(&problem);
        summary.record(Status::Failed, &problem);
    }
    summary.record(
        Status::Unchanged,
        &format!("profile resolves to '{}'", cfg.active_profile),
    );

    // Prerequisites.
    ui::heading("Prerequisites");
    let providers = pkg::registry(&cfg.packages);
    for p in &providers {
        if p.is_available() {
            summary.record(Status::Unchanged, &format!("{} CLI present", p.name()));
        } else {
            let st = if p.required() {
                Status::Failed
            } else {
                Status::Skipped
            };
            summary.record(st, &format!("{} CLI missing", p.name()));
            if p.required() {
                ui::detail(format!("install: {}", p.remediation().install));
            }
        }
    }
    let sudo_ok = crate::proc::has_command("sudo");
    summary.record(
        if sudo_ok {
            Status::Unchanged
        } else {
            Status::Failed
        },
        "sudo available",
    );
    let clt_ok = crate::proc::capture("xcode-select", &["-p"])
        .map(|o| o.success())
        .unwrap_or(false);
    summary.record(
        if clt_ok {
            Status::Unchanged
        } else {
            Status::Skipped
        },
        "Xcode Command Line Tools",
    );

    // Dotfiles link health (status prints its own "Dotfiles" heading).
    let state = ctx.state()?;
    summary.merge(dotfiles::status(&cfg, &state, &[])?);

    summary.render();
    if summary.any_failed() {
        std::process::exit(1);
    }
    Ok(())
}

pub fn capture(ctx: &Ctx) -> Result<()> {
    let cfg = ctx.load()?;
    ui::info("Snapshotting current machine state (review, then save):");
    ui::heading("packages.toml");
    let providers = pkg::registry(&cfg.packages);
    let selected: Vec<&dyn pkg::Provider> = providers.iter().map(|b| b.as_ref()).collect();
    let _ = pkg::dump(&selected);
    ui::heading("macos/keyboard.toml");
    match macos::keyboard::dump() {
        Ok(d) => print!("{}", macos::keyboard::to_toml(&d)),
        Err(e) => ui::warn(format!("keyboard dump skipped: {e:#}")),
    }
    Ok(())
}

/// Resolve the config directory: `--config`, else `$MACBOOT_HOME`, else a
/// `macboot.toml` in the CWD, else `~/.config/macboot`.
pub fn resolve_config_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("MACBOOT_HOME") {
        return Ok(PathBuf::from(p));
    }
    if Path::new("macboot.toml").exists() {
        return Ok(PathBuf::from("."));
    }
    Ok(default_config_dir())
}

/// Default config location: `$XDG_CONFIG_HOME/macboot`, else `~/.config/macboot`.
/// We deliberately use `~/.config` (the dotfiles convention) rather than
/// `dirs::config_dir()`, which resolves to `~/Library/Application Support` on macOS.
fn default_config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("macboot");
    }
    dirs::home_dir()
        .map(|h| h.join(".config").join("macboot"))
        .unwrap_or_else(|| PathBuf::from("macboot"))
}

/// `init`: scaffold a ready-to-edit config directory so a new user can start in
/// seconds. Never clobbers an existing config unless `force` is set.
pub fn init(target: Option<PathBuf>, force: bool, assume_yes: bool) -> Result<()> {
    let dir = target
        .or_else(|| std::env::var_os("MACBOOT_HOME").map(PathBuf::from))
        .unwrap_or_else(default_config_dir);

    let manifest = dir.join("macboot.toml");
    if manifest.exists() && !force {
        bail!(
            "config already exists at {} (use --force to overwrite the top-level files)",
            crate::util::tildify(&manifest)
        );
    }
    if !dir.exists()
        && !ui::ask(
            &format!("Create macboot config at {}?", crate::util::tildify(&dir)),
            assume_yes,
        )
    {
        ui::warn("Aborted.");
        return Ok(());
    }

    let username = std::env::var("USER").unwrap_or_else(|_| "your-username".to_string());

    // (relative path, contents, overwrite-if-exists)
    let files: [(&str, String, bool); 5] = [
        ("macboot.toml", template_macboot(&username), true),
        ("packages.toml", template_packages(), false),
        ("macos/dock.toml", template_dock(), false),
        ("profiles/personal.toml", template_profile(), false),
        ("dotfiles/.keep", String::new(), false),
    ];

    let mut created = 0;
    for (rel, contents, overwrite) in files {
        let path = dir.join(rel);
        if path.exists() && !(overwrite && force) {
            ui::detail(format!("exists, kept: {rel}"));
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {rel}"))?;
        ui::ok(format!("wrote {rel}"));
        created += 1;
    }

    ui::heading("Next steps");
    let shown = crate::util::tildify(&dir);
    ui::detail(format!("1. Edit {shown}/packages.toml and macos/*.toml"));
    ui::detail(format!(
        "2. Drop dotfiles under {shown}/dotfiles/<pkg>/ (mirrors $HOME)"
    ));
    ui::detail("3. Preview:  macboot status");
    ui::detail("4. Apply:    macboot apply --dry-run   then   macboot apply");
    if created == 0 {
        ui::warn("Nothing new was created.");
    }
    Ok(())
}

fn template_macboot(username: &str) -> String {
    format!(
        r#"[meta]
name = "{username}-mac"

[profile]
# First matching rule wins; else `default`. Replaces a $USER shell case.
default = "personal"
match = [
  # {{ profile = "work", username = "your.work.username" }},
]

[apply]
stages = ["packages", "dotfiles", "macos"]
on_error = "continue"   # or "abort"

# Omit to link every directory under dotfiles/.
# [dotfiles]
# packages = ["git", "zsh"]
"#
    )
}

fn template_packages() -> String {
    r#"# Only declared providers are ever touched. Brew is the common default.
[brew]
enabled = true
required = true
taps = []
brews = ["bat", "eza", "fd", "ripgrep", "fzf", "zoxide", "starship"]
casks = []
mas = []           # [{ name = "Amphetamine", id = 937984704 }]
vscode = []

# Other managers coexist with brew. Mark best-effort ones required = false.
# [cargo]
# required = false
# crates = ["bottom", "tokei"]
"#
    .to_string()
}

fn template_dock() -> String {
    r#"killall = ["Dock"]

[[defaults]]
domain = "com.apple.dock"
key = "tilesize"
type = "int"
value = 36

[[defaults]]
domain = "com.apple.dock"
key = "show-recents"
type = "bool"
value = false
"#
    .to_string()
}

fn template_profile() -> String {
    r#"# Overlay merged onto the base config when this profile is active.
[vars]
# git_email = "you@example.com"

# [packages.brew]
# brews = ["some-personal-tool"]
"#
    .to_string()
}

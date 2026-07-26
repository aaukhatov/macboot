//! Pluggable package managers. Every backend implements [`Provider`]; the
//! generic operations (`diff`, `apply`, `clean`, `dump`) are provider-agnostic,
//! so adding a manager is data + one small struct, never a change to the command
//! layer. Homebrew is the only bespoke backend (bundle-driven); everything else
//! is a thin command wrapper via [`generic::CmdProvider`].

pub mod brew;
pub mod generic;

use crate::config::Packages;
use crate::ui::{self, Status, Summary};
use anyhow::{bail, Result};
use std::path::Path;

/// How to install a missing provider CLI — shown by the preflight and `doctor`.
#[derive(Debug, Clone)]
pub struct Remediation {
    pub install: String,
    pub url: String,
}

/// A package manager backend.
pub trait Provider {
    /// Table name in `packages.toml` (e.g. "brew").
    fn name(&self) -> &str;
    /// The CLI binary that must be on PATH.
    fn cli(&self) -> &str;
    /// Whether a missing CLI is fatal (default) or best-effort.
    fn required(&self) -> bool;
    /// Actionable install hint when the CLI is missing.
    fn remediation(&self) -> Remediation;
    /// Package identifiers declared in the manifest.
    fn desired(&self) -> Vec<String>;
    /// Package identifiers currently installed (empty ⇒ detection unsupported).
    fn installed(&self) -> Result<Vec<String>>;
    /// Whether drift detection (`diff`/`clean`) is meaningful for this provider.
    fn supports_diff(&self) -> bool {
        true
    }
    /// Install what is declared (idempotently).
    fn install(&self, missing: &[String], dry: bool) -> Result<()>;
    /// Remove the given packages.
    fn remove(&self, extraneous: &[String], dry: bool) -> Result<()>;

    /// File name `pkg dump` writes for this provider, in the manager's own
    /// format. Lands at `packages/<provider>/<file>` under the config root.
    fn dump_file(&self) -> &str {
        "packages.txt"
    }

    /// What is installed, rendered in this manager's native format.
    ///
    /// The default is a bare newline-delimited list — the natural artifact for
    /// managers that have no manifest format of their own, and directly usable
    /// as `xargs <cli> install < packages.txt`. Keep such files comment-free:
    /// `xargs` has no notion of comments.
    fn dump_native(&self) -> Result<String> {
        let mut installed = self.installed()?;
        installed.sort();
        Ok(generic::lines_file(&installed))
    }

    /// Render what is installed as this provider's `packages.toml` block.
    ///
    /// The output must parse back into the same provider — `capture` feeds
    /// straight into a manifest, so a key this provider's spec does not accept
    /// would silently declare nothing.
    fn dump_block(&self) -> Result<String> {
        let mut installed = self.installed()?;
        installed.sort();
        let rendered: Vec<String> = installed.iter().map(|i| format!("{i:?}")).collect();
        Ok(format!(
            "[{}]\n{} = [{}]\n",
            self.name(),
            generic::items_key(self.name()),
            rendered.join(", ")
        ))
    }

    fn is_available(&self) -> bool {
        crate::proc::has_command(self.cli())
    }
}

/// Drift between manifest and machine for one provider.
pub struct Drift {
    pub missing: Vec<String>,    // declared, not installed
    pub extraneous: Vec<String>, // installed, not declared
}

/// Compute drift for a provider (requires a working CLI).
pub fn diff(provider: &dyn Provider) -> Result<Drift> {
    let desired = provider.desired();
    let installed = provider.installed()?;
    let missing: Vec<String> = desired
        .iter()
        .filter(|d| !installed.contains(d))
        .cloned()
        .collect();
    let extraneous: Vec<String> = installed
        .iter()
        .filter(|i| !desired.contains(i))
        .cloned()
        .collect();
    Ok(Drift {
        missing,
        extraneous,
    })
}

/// Build the ordered list of declared, enabled providers from the manifest.
pub fn registry(packages: &Packages) -> Vec<Box<dyn Provider>> {
    let mut out: Vec<Box<dyn Provider>> = Vec::new();

    if let Some(spec) = &packages.brew {
        if spec.enabled {
            out.push(Box::new(brew::BrewProvider::from_spec(spec)));
        }
    }
    for (name, spec) in packages.list_providers() {
        if spec.enabled {
            out.push(generic::builtin(name, &spec.items, spec.required));
        }
    }
    for (name, cp) in &packages.providers.custom {
        if cp.enabled {
            out.push(generic::from_custom(name, cp));
        }
    }
    out
}

/// Filter a registry by a comma-separated `--provider` selector (None ⇒ all).
pub fn select<'a>(
    providers: &'a [Box<dyn Provider>],
    provider: Option<&str>,
) -> Result<Vec<&'a dyn Provider>> {
    let all: Vec<&dyn Provider> = providers.iter().map(|b| b.as_ref()).collect();
    let Some(provider) = provider else {
        return Ok(all);
    };
    let wanted: Vec<&str> = provider.split(',').map(|s| s.trim()).collect();
    let mut out = Vec::new();
    for name in &wanted {
        match all.iter().find(|p| p.name() == *name) {
            Some(p) => out.push(*p),
            None => bail!("provider '{name}' is not declared in packages.toml"),
        }
    }
    Ok(out)
}

/// Preflight: verify each declared provider's CLI is present, printing an
/// actionable remediation for any that are missing. Returns Err if a *required*
/// provider is missing and `skip_missing` is false.
pub fn preflight(providers: &[Box<dyn Provider>], skip_missing: bool) -> Result<Vec<String>> {
    let mut usable = Vec::new();
    let mut blocking = Vec::new();
    for p in providers {
        if p.is_available() {
            usable.push(p.name().to_string());
            continue;
        }
        let rem = p.remediation();
        if p.required() && !skip_missing {
            ui::err(format!(
                "{} is required by [{}] but `{}` was not found on PATH.",
                p.cli(),
                p.name(),
                p.cli()
            ));
            ui::detail(format!("Install it:   {}", rem.install));
            ui::detail(format!("Docs:         {}", rem.url));
            ui::detail("Or bootstrap: macboot bootstrap");
            ui::detail("Or skip once: add --skip-missing");
            blocking.push(p.name().to_string());
        } else {
            ui::warn(format!(
                "{} not found; skipping [{}] (best-effort).",
                p.cli(),
                p.name()
            ));
        }
    }
    if !blocking.is_empty() {
        bail!(
            "missing required tool(s) for: {}. See remediation above.",
            blocking.join(", ")
        );
    }
    Ok(usable)
}

/// `pkg apply`: install declared packages for each usable provider.
pub fn apply(providers: &[&dyn Provider], dry: bool) -> Result<Summary> {
    let mut summary = Summary::new();
    for p in providers {
        if !p.is_available() {
            summary.record(Status::Skipped, &format!("{} (CLI missing)", p.name()));
            continue;
        }
        ui::heading(format!("Packages · {}", p.name()));
        let missing = if p.supports_diff() {
            diff(*p).map(|d| d.missing).unwrap_or_else(|_| p.desired())
        } else {
            p.desired()
        };
        if missing.is_empty() {
            summary.record(Status::Unchanged, &format!("{}: up to date", p.name()));
            continue;
        }
        match p.install(&missing, dry) {
            Ok(()) => summary.record(
                Status::Changed,
                &format!("{}: {} package(s)", p.name(), missing.len()),
            ),
            Err(e) => {
                ui::err(format!("{}: {e:#}", p.name()));
                summary.record(Status::Failed, p.name());
            }
        }
    }
    Ok(summary)
}

/// `pkg diff`: print per-provider drift; returns true if any drift was found.
pub fn report_diff(providers: &[&dyn Provider]) -> Result<bool> {
    let mut any = false;
    for p in providers {
        ui::heading(format!("Packages · {}", p.name()));
        if !p.is_available() {
            ui::warn(format!("{} CLI missing; cannot diff", p.cli()));
            continue;
        }
        if !p.supports_diff() {
            ui::detail("drift detection not supported (apply-only provider)");
            continue;
        }
        let d = diff(*p)?;
        if d.missing.is_empty() && d.extraneous.is_empty() {
            ui::ok(format!("{}: in sync", p.name()));
            continue;
        }
        any = true;
        for m in &d.missing {
            eprintln!("  {} {m}", Status::Changed.glyph());
        }
        for e in &d.extraneous {
            eprintln!("  {} {e} (extraneous)", Status::Changed.glyph());
        }
    }
    Ok(any)
}

/// `pkg clean`: remove extraneous packages (gated by the caller).
pub fn clean(providers: &[&dyn Provider], dry: bool) -> Result<Summary> {
    let mut summary = Summary::new();
    for p in providers {
        if !p.is_available() || !p.supports_diff() {
            summary.record(Status::Skipped, p.name());
            continue;
        }
        let extraneous = diff(*p)?.extraneous;
        if extraneous.is_empty() {
            summary.record(
                Status::Unchanged,
                &format!("{}: nothing to remove", p.name()),
            );
            continue;
        }
        ui::heading(format!("Cleaning · {}", p.name()));
        match p.remove(&extraneous, dry) {
            Ok(()) => summary.record(
                Status::Changed,
                &format!("{}: removed {}", p.name(), extraneous.len()),
            ),
            Err(e) => {
                ui::err(format!("{}: {e:#}", p.name()));
                summary.record(Status::Failed, p.name());
            }
        }
    }
    Ok(summary)
}

/// `pkg dump`: snapshot each provider's installed set into
/// `packages/<provider>/<native file>`, or print it on a dry run.
///
/// These files are outputs, not inputs: `packages.toml` stays the manifest
/// `apply` reads. Each one is a complete re-render of what is installed, so a
/// dump replaces the previous snapshot.
pub fn dump(providers: &[&dyn Provider], root: &Path, dry: bool) -> Result<Summary> {
    let mut summary = Summary::new();
    for p in providers {
        if !p.is_available() {
            summary.record(Status::Skipped, &format!("{} (CLI missing)", p.name()));
            continue;
        }
        if !p.supports_diff() {
            summary.record(
                Status::Skipped,
                &format!("{} (cannot list installed packages)", p.name()),
            );
            continue;
        }
        let rel = format!("packages/{}/{}", p.name(), p.dump_file());
        let target = root.join(&rel);
        let outcome = p
            .dump_native()
            .and_then(|body| crate::util::write_generated(&target, &body, dry));
        match outcome {
            Ok(()) => summary.record(
                Status::Changed,
                &format!("{rel}{}", if dry { " [dry-run]" } else { "" }),
            ),
            Err(e) => {
                ui::err(format!("{}: {e:#}", p.name()));
                summary.record(Status::Failed, p.name());
            }
        }
    }
    Ok(summary)
}

/// `capture`: print a `packages.toml`-shaped snapshot of what is installed, per
/// provider, to stdout (paste into the manifest after review).
pub fn dump_toml(providers: &[&dyn Provider]) -> Result<()> {
    for p in providers {
        if !p.is_available() || !p.supports_diff() {
            continue;
        }
        print!("{}", p.dump_block()?);
        println!();
    }
    Ok(())
}

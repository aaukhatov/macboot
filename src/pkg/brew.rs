//! Homebrew backend. Unlike the flat-list providers, brew installs via a
//! generated Brewfile and `brew bundle`, and covers taps/formulae/casks/mas/
//! VS Code in one shot. Drift is computed over top-level formulae + casks.

use super::{Provider, Remediation};
use crate::config::packages::BrewSpec;
use crate::proc;
use crate::ui;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct BrewProvider {
    required: bool,
    taps: Vec<String>,
    brews: Vec<String>,
    casks: Vec<String>,
    mas: Vec<(String, u64)>,
    vscode: Vec<String>,
}

impl BrewProvider {
    pub fn from_spec(spec: &BrewSpec) -> Self {
        BrewProvider {
            required: spec.required,
            taps: spec.taps.clone(),
            brews: spec.brews.clone(),
            casks: spec.casks.clone(),
            mas: spec.mas.iter().map(|a| (a.name.clone(), a.id)).collect(),
            vscode: spec.vscode.clone(),
        }
    }

    /// Render a Brewfile capturing the full declared state.
    fn brewfile(&self) -> String {
        let mut lines = Vec::new();
        for t in &self.taps {
            lines.push(format!("tap {t:?}"));
        }
        for b in &self.brews {
            lines.push(format!("brew {b:?}"));
        }
        for c in &self.casks {
            lines.push(format!("cask {c:?}"));
        }
        for (name, id) in &self.mas {
            lines.push(format!("mas {name:?}, id: {id}"));
        }
        for v in &self.vscode {
            lines.push(format!("vscode {v:?}"));
        }
        lines.join("\n") + "\n"
    }

    fn write_temp_brewfile(&self) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "macboot-Brewfile-{}-{}",
            std::process::id(),
            crate::util::now_stamp()
        ));
        std::fs::write(&path, self.brewfile())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

impl Provider for BrewProvider {
    fn name(&self) -> &str {
        "brew"
    }
    fn cli(&self) -> &str {
        "brew"
    }
    fn required(&self) -> bool {
        self.required
    }
    fn remediation(&self) -> Remediation {
        Remediation {
            install: "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"".to_string(),
            url: "https://brew.sh".to_string(),
        }
    }

    /// Diff scope: top-level formulae + casks (mas/vscode are apply-only).
    fn desired(&self) -> Vec<String> {
        let mut out = self.brews.clone();
        out.extend(self.casks.iter().cloned());
        out
    }

    fn installed(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        // `brew leaves` = installed formulae not depended on by others.
        if let Ok(text) = proc::output("brew", &["leaves"]) {
            out.extend(text.lines().map(|l| l.trim().to_string()));
        }
        if let Ok(text) = proc::output("brew", &["list", "--cask", "-1"]) {
            out.extend(text.lines().map(|l| l.trim().to_string()));
        }
        out.retain(|s| !s.is_empty());
        Ok(out)
    }

    fn install(&self, _missing: &[String], dry: bool) -> Result<()> {
        // `brew bundle` is idempotent; run the whole file rather than cherry-pick.
        let file = self.write_temp_brewfile()?;
        let file_arg = format!("--file={}", file.display());
        if dry {
            ui::detail(format!("brew bundle {file_arg} (dry-run)"));
            let _ = std::fs::remove_file(&file);
            return Ok(());
        }
        let res = proc::run("brew", &["bundle", &file_arg, "--verbose"]);
        let _ = std::fs::remove_file(&file);
        res
    }

    fn remove(&self, extraneous: &[String], dry: bool) -> Result<()> {
        for pkg in extraneous {
            ui::detail(format!("brew uninstall {pkg}"));
            if dry {
                continue;
            }
            // Ignore failures so one pinned/linked package doesn't abort cleanup.
            let _ = proc::run("brew", &["uninstall", pkg]);
        }
        Ok(())
    }
}

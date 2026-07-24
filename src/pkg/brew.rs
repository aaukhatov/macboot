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

/// Lines of a `brew` listing command, trimmed and free of blanks.
fn brew_lines(args: &[&str]) -> Vec<String> {
    match proc::output("brew", args) {
        Ok(text) => text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `brew leaves` = installed formulae not depended on by others.
fn installed_formulae() -> Vec<String> {
    brew_lines(&["leaves"])
}

fn installed_casks() -> Vec<String> {
    brew_lines(&["list", "--cask", "-1"])
}

fn installed_taps() -> Vec<String> {
    brew_lines(&["tap"])
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
        let mut out = installed_formulae();
        out.extend(installed_casks());
        Ok(out)
    }

    /// Homebrew spans four manifest keys, so the generic one-list default would
    /// emit a `packages = [...]` key that [`BrewSpec`] does not accept.
    fn dump_block(&self) -> Result<String> {
        let mut out = String::from("[brew]\n");
        for (key, mut values) in [
            ("taps", installed_taps()),
            ("brews", installed_formulae()),
            ("casks", installed_casks()),
        ] {
            values.sort();
            let rendered: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
            out.push_str(&format!("{key} = [{}]\n", rendered.join(", ")));
        }
        // mas and vscode are apply-only (not part of the diff scope), so they
        // are left for the user rather than guessed at.
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

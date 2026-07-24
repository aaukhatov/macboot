//! Declarative macOS settings: `defaults` writes, the command escape hatch,
//! keybindings, and per-app menu shortcuts. Every setting is read before it is
//! written, so `apply` is idempotent and `diff` shows real drift.

pub mod keyboard;

use crate::config::{Config, DefaultSetting, DefaultType, MacosFile};
use crate::proc;
use crate::ui::{self, Status, Summary};
use anyhow::Result;
use std::collections::BTreeSet;

/// Read the current value of a default, or None if unset.
fn read_default(setting: &DefaultSetting) -> Option<String> {
    proc::capture("defaults", &["read", &setting.domain, &setting.key])
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
}

/// Does the live value already match the desired value?
fn is_in_sync(setting: &DefaultSetting) -> bool {
    let Some(current) = read_default(setting) else {
        return false;
    };
    match setting.kind {
        DefaultType::Bool => {
            let want = setting.value.as_bool().unwrap_or(false);
            let got = matches!(current.as_str(), "1" | "true" | "YES");
            want == got
        }
        DefaultType::Int => setting.value.as_integer() == current.trim().parse::<i64>().ok(),
        DefaultType::Float => match (setting.value.as_float(), current.trim().parse::<f64>()) {
            (Some(w), Ok(g)) => (w - g).abs() < f64::EPSILON,
            _ => false,
        },
        DefaultType::String => setting.value.as_str() == Some(current.as_str()),
    }
}

/// Build the `defaults write` argv value flag + string for a setting.
fn write_value(setting: &DefaultSetting) -> (&'static str, String) {
    match setting.kind {
        DefaultType::Bool => (
            "-bool",
            if setting.value.as_bool().unwrap_or(false) {
                "true".into()
            } else {
                "false".into()
            },
        ),
        DefaultType::Int => ("-int", setting.value.as_integer().unwrap_or(0).to_string()),
        DefaultType::Float => (
            "-float",
            setting.value.as_float().unwrap_or(0.0).to_string(),
        ),
        DefaultType::String => (
            "-string",
            setting.value.as_str().unwrap_or_default().to_string(),
        ),
    }
}

fn write_default(setting: &DefaultSetting) -> Result<()> {
    let (flag, value) = write_value(setting);
    let args = ["write", &setting.domain, &setting.key, flag, &value];
    if setting.sudo {
        let mut sudo_args = vec!["defaults"];
        sudo_args.extend_from_slice(&args);
        proc::run("sudo", &sudo_args)
    } else {
        proc::run("defaults", &args)
    }
}

/// Select the macos files to act on (all, or filtered by `only` domain names).
fn selected_files<'a>(cfg: &'a Config, only: Option<&[String]>) -> Vec<&'a MacosFile> {
    cfg.macos
        .iter()
        .filter(|f| match only {
            Some(names) => names.iter().any(|n| n == &f.name()),
            None => true,
        })
        .collect()
}

/// `macos diff`: read-only drift report.
pub fn diff(cfg: &Config, only: Option<&[String]>) -> Result<Summary> {
    let mut summary = Summary::new();
    for file in selected_files(cfg, only) {
        ui::heading(format!("macOS · {}", file.name()));
        for setting in &file.defaults {
            let label = format!("{} {}", setting.domain, setting.key);
            let status = if is_in_sync(setting) {
                Status::Unchanged
            } else {
                Status::Changed
            };
            summary.record(status, &label);
        }
        // Commands and keybindings can't be cheaply diffed; report their presence.
        for cmd in &file.commands {
            summary.record(
                Status::Skipped,
                &format!(
                    "command: {}",
                    cmd.desc.clone().unwrap_or_else(|| cmd.run.join(" "))
                ),
            );
        }
        if !file.hotkey.is_empty() || !file.raw.is_empty() {
            summary.record(
                Status::Skipped,
                &format!("{} keybinding(s)", file.hotkey.len() + file.raw.len()),
            );
        }
    }
    Ok(summary)
}

/// `macos apply`: write drifted defaults, run commands, apply keybindings and app
/// shortcuts, then killall affected apps once.
pub fn apply(cfg: &Config, only: Option<&[String]>, dry: bool) -> Result<Summary> {
    let mut summary = Summary::new();
    let mut to_kill: BTreeSet<String> = BTreeSet::new();

    for file in selected_files(cfg, only) {
        ui::heading(format!("macOS · {}", file.name()));

        for setting in &file.defaults {
            let label = format!("{} {}", setting.domain, setting.key);
            if is_in_sync(setting) {
                summary.record(Status::Unchanged, &label);
                continue;
            }
            if dry {
                summary.record(Status::Changed, &format!("{label} [dry-run]"));
                continue;
            }
            match write_default(setting) {
                Ok(()) => summary.record(Status::Changed, &label),
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    summary.record(Status::Failed, &label);
                }
            }
        }

        for cmd in &file.commands {
            let label = cmd.desc.clone().unwrap_or_else(|| cmd.run.join(" "));
            if cmd.run.is_empty() {
                continue;
            }
            if dry {
                summary.record(Status::Changed, &format!("{label} [dry-run]"));
                continue;
            }
            let (bin, rest) = command_argv(cmd.sudo, &cmd.run);
            let argv: Vec<&str> = rest.iter().map(String::as_str).collect();
            match proc::run(&bin, &argv) {
                Ok(()) => summary.record(Status::Changed, &label),
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    summary.record(Status::Failed, &label);
                }
            }
        }

        // Per-app menu shortcuts.
        for sc in &file.app_shortcut {
            let label = format!("{}: {} = {}", sc.bundle, sc.menu_title, sc.chord);
            if dry {
                summary.record(Status::Changed, &format!("{label} [dry-run]"));
                continue;
            }
            match apply_app_shortcut(sc) {
                Ok(()) => summary.record(Status::Changed, &label),
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    summary.record(Status::Failed, &label);
                }
            }
        }

        // Keybindings. The `apply = "activateSettings"` hint documents that this
        // file reloads settings after writing; keyboard::apply performs it.
        if let Some(hook) = &file.apply {
            ui::detail(format!("post-apply hook: {hook}"));
        }
        if !file.hotkey.is_empty() || !file.raw.is_empty() {
            match keyboard::apply(&file.hotkey, &file.raw, dry) {
                Ok(n) if n > 0 => summary.record(Status::Changed, &format!("{n} keybinding(s)")),
                Ok(_) => {}
                Err(e) => {
                    ui::err(format!("keybindings: {e:#}"));
                    summary.record(Status::Failed, "keybindings");
                }
            }
        }

        for app in &file.killall {
            to_kill.insert(app.clone());
        }
    }

    if !dry {
        for app in &to_kill {
            // Best-effort; a not-running app is not an error.
            let _ = proc::capture("killall", &[app]);
        }
    }
    Ok(summary)
}

fn apply_app_shortcut(sc: &crate::config::AppShortcut) -> Result<()> {
    let glyphs = keyboard::chord_to_glyphs(&sc.chord)?;
    proc::run(
        "defaults",
        &[
            "write",
            &sc.bundle,
            "NSUserKeyEquivalents",
            "-dict-add",
            &sc.menu_title,
            &glyphs,
        ],
    )
}

/// Split a command step into (binary, argv), honoring the sudo flag.
fn command_argv(sudo: bool, run: &[String]) -> (String, Vec<String>) {
    if sudo && run.first().map(|s| s != "sudo").unwrap_or(false) {
        ("sudo".to_string(), run.to_vec())
    } else {
        (run[0].clone(), run[1..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(kind: DefaultType, value: toml::Value) -> DefaultSetting {
        DefaultSetting {
            domain: "com.example".into(),
            key: "k".into(),
            kind,
            value,
            sudo: false,
        }
    }

    #[test]
    fn write_value_formats_bool() {
        let (flag, val) = write_value(&setting(DefaultType::Bool, toml::Value::Boolean(true)));
        assert_eq!(flag, "-bool");
        assert_eq!(val, "true");
    }

    #[test]
    fn write_value_formats_int() {
        let (flag, val) = write_value(&setting(DefaultType::Int, toml::Value::Integer(36)));
        assert_eq!(flag, "-int");
        assert_eq!(val, "36");
    }

    #[test]
    fn command_argv_prepends_sudo() {
        let (bin, rest) = command_argv(true, &["pmset".into(), "-a".into()]);
        assert_eq!(bin, "sudo");
        assert_eq!(rest, vec!["pmset".to_string(), "-a".to_string()]);
    }
}

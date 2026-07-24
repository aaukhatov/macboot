//! Declarative macOS settings: `defaults` writes, the command escape hatch,
//! keybindings, and per-app menu shortcuts. Every setting is read before it is
//! written, so `apply` is idempotent and `diff` shows real drift.

pub mod keyboard;
pub mod record;

use crate::config::{Config, DefaultSetting, DefaultType, MacosFile};
use crate::proc;
use crate::state::{DefaultRecord, State};
use crate::ui::{self, Status, Summary};
use anyhow::{bail, Result};
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

/// The plist type of a live key, per `defaults read-type` ("boolean", "integer",
/// "string", …). None if the key does not exist.
fn read_type(domain: &str, key: &str) -> Option<String> {
    let out = proc::capture("defaults", &["read-type", domain, key]).ok()?;
    if !out.success() {
        return None;
    }
    // Output is literally "Type is boolean".
    out.stdout
        .trim()
        .rsplit(' ')
        .next()
        .map(|s| s.to_lowercase())
}

/// Remember what a key held before macboot first writes it, so `macos revert`
/// can put it back. Subsequent writes do not overwrite the original.
fn remember_previous(state: &mut State, setting: &DefaultSetting) {
    if state.knows_default(&setting.domain, &setting.key) {
        return;
    }
    state.insert_default(DefaultRecord {
        domain: setting.domain.clone(),
        key: setting.key.clone(),
        previous: read_default(setting),
        previous_type: read_type(&setting.domain, &setting.key),
        sudo: setting.sudo,
        recorded: crate::util::now_stamp(),
    });
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

/// Select the macos files to act on (all, or filtered by `only` file names).
///
/// An unknown name is an error rather than an empty selection: a mistyped
/// `--only` on `apply` must never look like a successful no-op.
fn selected_files<'a>(cfg: &'a Config, only: Option<&[String]>) -> Result<Vec<&'a MacosFile>> {
    let Some(names) = only else {
        return Ok(cfg.macos.iter().collect());
    };
    let available: Vec<String> = cfg.macos.iter().map(MacosFile::name).collect();
    let mut out = Vec::new();
    for name in names {
        match cfg.macos.iter().find(|f| &f.name() == name) {
            Some(file) => out.push(file),
            None => bail!(
                "no macos/{name}.toml in the config (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            ),
        }
    }
    Ok(out)
}

/// `macos diff`: read-only drift report.
pub fn diff(cfg: &Config, only: Option<&[String]>) -> Result<Summary> {
    let mut summary = Summary::new();
    for file in selected_files(cfg, only)? {
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
pub fn apply(
    cfg: &Config,
    state: &mut State,
    only: Option<&[String]>,
    dry: bool,
) -> Result<Summary> {
    let mut summary = Summary::new();
    let mut to_kill: BTreeSet<String> = BTreeSet::new();

    for file in selected_files(cfg, only)? {
        ui::heading(format!("macOS · {}", file.name()));
        // Restarting Dock/Finder is user-visible (lost windows, flicker), so a
        // file that changed nothing must not trigger its killall.
        let changed_before = summary.changed;

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
            remember_previous(state, setting);
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

        if summary.changed > changed_before {
            for app in &file.killall {
                to_kill.insert(app.clone());
            }
        }
    }

    if !dry {
        state.save()?;
        restart(&to_kill);
    }
    Ok(summary)
}

/// Restart the apps that must reload to pick up a change. Best-effort: an app
/// that is not running is not an error.
fn restart(apps: &BTreeSet<String>) {
    for app in apps {
        ui::detail(format!("restarting {app}"));
        let _ = proc::capture("killall", &[app]);
    }
}

/// The `defaults` flag that writes a value back as the type it originally had.
fn flag_for_type(plist_type: &str) -> Option<&'static str> {
    match plist_type {
        "boolean" => Some("-bool"),
        "integer" => Some("-int"),
        "float" | "real" => Some("-float"),
        "string" => Some("-string"),
        "date" => Some("-date"),
        // Arrays, dicts and data cannot be round-tripped through the one-line
        // `defaults read` output we captured.
        _ => None,
    }
}

/// `macos revert`: put every default macboot wrote back the way it was.
///
/// A key that did not exist before is deleted; a key that did is written back
/// with its original value and type. Reverted keys are dropped from state, so a
/// second revert is a no-op rather than a re-application.
pub fn revert(cfg: &Config, state: &mut State, domain: Option<&str>, dry: bool) -> Result<Summary> {
    let mut summary = Summary::new();
    let targets: Vec<DefaultRecord> = state
        .defaults
        .values()
        .filter(|r| domain.is_none_or(|d| r.domain == d))
        .cloned()
        .collect();

    if targets.is_empty() {
        ui::warn(match domain {
            Some(d) => format!("macboot has not written any keys in '{d}'."),
            None => "macboot has not written any macOS defaults yet.".to_string(),
        });
        return Ok(summary);
    }

    ui::heading(format!("Reverting {} default(s)", targets.len()));
    let mut reverted_domains: BTreeSet<String> = BTreeSet::new();
    for record in targets {
        let label = format!("{} {}", record.domain, record.key);
        if dry {
            let what = match &record.previous {
                Some(v) => format!("restore to {v}"),
                None => "delete".to_string(),
            };
            summary.record(Status::Changed, &format!("{label} ({what}) [dry-run]"));
            continue;
        }
        match revert_one(&record) {
            Ok(true) => {
                state.remove_default(&record.domain, &record.key);
                reverted_domains.insert(record.domain.clone());
                summary.record(Status::Changed, &label);
            }
            // Left alone on purpose (a shape we cannot restore); keep the
            // record so the user can still see what macboot changed.
            Ok(false) => summary.record(Status::Skipped, &label),
            Err(e) => {
                ui::err(format!("{label}: {e:#}"));
                summary.record(Status::Failed, &label);
            }
        }
    }

    if !dry {
        state.save()?;
        restart(&killall_for_domains(cfg, &reverted_domains));
    }
    Ok(summary)
}

/// Restore one key. Returns false when the original value's type is one we
/// cannot write back.
fn revert_one(record: &DefaultRecord) -> Result<bool> {
    let run = |args: &[&str]| -> Result<()> {
        if record.sudo {
            let mut with_sudo = vec!["defaults"];
            with_sudo.extend_from_slice(args);
            proc::run("sudo", &with_sudo)
        } else {
            proc::run("defaults", args)
        }
    };

    let Some(previous) = &record.previous else {
        // The key did not exist before macboot; deleting restores that.
        run(&["delete", &record.domain, &record.key])?;
        return Ok(true);
    };
    let plist_type = record.previous_type.as_deref().unwrap_or("string");
    let Some(flag) = flag_for_type(plist_type) else {
        ui::warn(format!(
            "{} {} was a {plist_type}; restore it by hand",
            record.domain, record.key
        ));
        return Ok(false);
    };
    run(&["write", &record.domain, &record.key, flag, previous])?;
    Ok(true)
}

/// The apps to restart after reverting keys in `domains`, taken from the
/// `killall` lists of the config files that declare those domains.
fn killall_for_domains(cfg: &Config, domains: &BTreeSet<String>) -> BTreeSet<String> {
    let mut apps = BTreeSet::new();
    for file in &cfg.macos {
        if file
            .defaults
            .iter()
            .any(|setting| domains.contains(&setting.domain))
        {
            apps.extend(file.killall.iter().cloned());
        }
    }
    apps
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
    use std::path::PathBuf;

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

    fn cfg_with_files(names: &[&str]) -> Config {
        let macos = names
            .iter()
            .map(|n| MacosFile {
                path: PathBuf::from(format!("/cfg/macos/{n}.toml")),
                ..Default::default()
            })
            .collect();
        Config {
            root: PathBuf::from("/cfg"),
            meta: Default::default(),
            profile: Default::default(),
            apply: Default::default(),
            packages: Default::default(),
            macos,
            dotfiles: Vec::new(),
            active_profile: "personal".into(),
            vars: Default::default(),
        }
    }

    #[test]
    fn only_selects_the_named_file() {
        let cfg = cfg_with_files(&["dock", "finder"]);
        let picked = selected_files(&cfg, Some(&["finder".to_string()])).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name(), "finder");
    }

    #[test]
    fn unknown_only_name_errors_instead_of_selecting_nothing() {
        let cfg = cfg_with_files(&["dock", "finder"]);
        let err = selected_files(&cfg, Some(&["dokk".to_string()])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("dokk"), "{msg}");
        assert!(msg.contains("dock, finder"), "{msg}");
    }

    #[test]
    fn restorable_types_map_to_write_flags() {
        assert_eq!(flag_for_type("boolean"), Some("-bool"));
        assert_eq!(flag_for_type("integer"), Some("-int"));
        assert_eq!(flag_for_type("string"), Some("-string"));
        // Shapes we captured as a one-line string cannot be written back.
        assert_eq!(flag_for_type("dictionary"), None);
        assert_eq!(flag_for_type("array"), None);
    }

    #[test]
    fn first_write_wins_so_revert_reaches_the_original() {
        let mut state = State::default();
        let setting = setting(DefaultType::Int, toml::Value::Integer(36));
        state.insert_default(DefaultRecord {
            domain: setting.domain.clone(),
            key: setting.key.clone(),
            previous: Some("48".into()),
            previous_type: Some("integer".into()),
            sudo: false,
            recorded: "1".into(),
        });
        // A later apply must not overwrite the pre-macboot value with macboot's.
        state.insert_default(DefaultRecord {
            domain: setting.domain.clone(),
            key: setting.key.clone(),
            previous: Some("36".into()),
            previous_type: Some("integer".into()),
            sudo: false,
            recorded: "2".into(),
        });
        assert_eq!(
            state.defaults["com.example k"].previous.as_deref(),
            Some("48")
        );
    }

    #[test]
    fn killall_targets_come_from_files_touching_the_domain() {
        let mut cfg = cfg_with_files(&["dock", "finder"]);
        cfg.macos[0].defaults = vec![setting(DefaultType::Bool, toml::Value::Boolean(true))];
        cfg.macos[0].killall = vec!["Dock".to_string()];
        cfg.macos[1].killall = vec!["Finder".to_string()];

        let domains = BTreeSet::from(["com.example".to_string()]);
        let apps = killall_for_domains(&cfg, &domains);
        assert_eq!(apps, BTreeSet::from(["Dock".to_string()]));
    }

    #[test]
    fn command_argv_prepends_sudo() {
        let (bin, rest) = command_argv(true, &["pmset".into(), "-a".into()]);
        assert_eq!(bin, "sudo");
        assert_eq!(rest, vec!["pmset".to_string(), "-a".to_string()]);
    }
}

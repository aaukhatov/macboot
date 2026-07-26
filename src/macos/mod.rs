//! Declarative macOS settings: `defaults` writes, the command escape hatch,
//! keybindings, and per-app menu shortcuts. Every setting is read before it is
//! written, so `apply` is idempotent and `diff` shows real drift.

pub mod dump;
pub mod keyboard;
pub mod value;

use crate::config::{Config, DefaultSetting, DefaultType, HostScope, MacosFile};
use crate::proc;
use crate::state::{DefaultRecord, State};
use crate::ui::{self, Status, Summary};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Live preference values, read through `defaults export` and cached per run.
///
/// Reading a whole domain at once rather than a key at a time matters twice
/// over: a file with twenty keys in two domains costs two processes instead of
/// twenty, and the XML gives us the real plist value — including arrays and
/// dicts, which `defaults read` only renders as un-reparseable old-style text.
#[derive(Default)]
struct Store {
    cache: BTreeMap<(String, HostScope), Option<BTreeMap<String, plist::Value>>>,
}

impl Store {
    fn new() -> Store {
        Store::default()
    }

    /// The live value of one key, or None if the domain or key is absent.
    fn read(&mut self, domain: &str, key: &str, host: HostScope) -> Option<plist::Value> {
        self.cache
            .entry((domain.to_string(), host))
            .or_insert_with(|| export_domain(domain, host))
            .as_ref()
            .and_then(|keys| keys.get(key).cloned())
    }

    /// Drop a cached domain after writing to it, so a later read in the same
    /// run cannot see a stale value.
    fn invalidate(&mut self, domain: &str, host: HostScope) {
        self.cache.remove(&(domain.to_string(), host));
    }
}

/// Read one domain in one scope into a key → value map. A domain that cannot be
/// exported (absent, sandboxed, malformed) yields None.
pub fn export_domain(domain: &str, host: HostScope) -> Option<BTreeMap<String, plist::Value>> {
    let mut args = host.args().to_vec();
    args.extend_from_slice(&["export", domain, "-"]);
    let out = proc::capture("defaults", &args).ok()?;
    if !out.success() {
        return None;
    }
    let parsed = plist::Value::from_reader_xml(std::io::Cursor::new(out.stdout.as_bytes())).ok()?;
    let dict = parsed.into_dictionary()?;
    Some(dict.into_iter().collect())
}

/// The plist value a setting wants the key to hold.
fn desired(setting: &DefaultSetting) -> Result<plist::Value> {
    value::to_plist(setting.kind, &setting.value).with_context(|| {
        format!(
            "in [[defaults]] for {} {}",
            setting.domain,
            label_key(setting)
        )
    })
}

/// Does the live value already match the desired value?
fn is_in_sync(setting: &DefaultSetting, store: &mut Store) -> Result<bool> {
    let want = desired(setting)?;
    let Some(current) = store.read(&setting.domain, &setting.key, setting.host) else {
        return Ok(false);
    };
    Ok(value::equal(&want, &current))
}

/// Run a `defaults` subcommand in the right scope, with sudo if required.
fn run_defaults(sudo: bool, host: HostScope, args: &[&str]) -> Result<()> {
    let mut argv = host.args().to_vec();
    argv.extend_from_slice(args);
    if sudo {
        let mut with_sudo = vec!["defaults"];
        with_sudo.extend_from_slice(&argv);
        proc::run("sudo", &with_sudo)
    } else {
        proc::run("defaults", &argv)
    }
}

/// The `defaults write` value flag + argument for a scalar setting.
///
/// Scalars keep their typed flags rather than going through XML so that the
/// command macboot runs stays the one a user would type by hand.
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
        // Complex types have no flag; write_default sends XML instead.
        DefaultType::Array | DefaultType::Dict | DefaultType::Data | DefaultType::Date => {
            ("", String::new())
        }
    }
}

/// Remember what a key held before macboot first writes it, so `macos revert`
/// can put it back. Subsequent writes do not overwrite the original.
fn remember_previous(state: &mut State, setting: &DefaultSetting, store: &mut Store) {
    if state.knows_default(&setting.domain, &setting.key, setting.host) {
        return;
    }
    // Archive the whole plist value, not `defaults read`'s flattened text, so
    // an array or dict can actually be put back.
    let previous = store
        .read(&setting.domain, &setting.key, setting.host)
        .and_then(|v| value::to_xml(&v).ok());
    state.insert_default(DefaultRecord {
        domain: setting.domain.clone(),
        key: setting.key.clone(),
        previous_plist: previous,
        previous: None,
        previous_type: None,
        sudo: setting.sudo,
        host: setting.host,
        recorded: crate::util::now_stamp(),
    });
}

fn write_default(setting: &DefaultSetting) -> Result<()> {
    match setting.kind {
        DefaultType::Bool | DefaultType::Int | DefaultType::Float | DefaultType::String => {
            let (flag, value) = write_value(setting);
            run_defaults(
                setting.sudo,
                setting.host,
                &["write", &setting.domain, &setting.key, flag, &value],
            )
        }
        // `defaults write` parses an XML plist document as the value, which is
        // the only way to set an array, dict, data blob or date.
        _ => {
            let xml = value::to_xml(&desired(setting)?)?;
            run_defaults(
                setting.sudo,
                setting.host,
                &["write", &setting.domain, &setting.key, &xml],
            )
        }
    }
}

/// The key half of a setting's label, tagged with its scope when it is not the
/// default one.
fn label_key(setting: &DefaultSetting) -> String {
    format!("{}{}", setting.key, setting.host.suffix())
}

fn label(setting: &DefaultSetting) -> String {
    format!("{} {}", setting.domain, label_key(setting))
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

/// Problems in the macOS files that can be found without touching the machine:
/// a `value =` that does not match its declared `type =`.
///
/// Surfaced by `doctor` so a typo is caught before `apply` runs against the
/// real system and leaves half a file applied.
pub fn validate(cfg: &Config) -> Vec<String> {
    let mut problems = Vec::new();
    for file in &cfg.macos {
        for setting in &file.defaults {
            if let Err(e) = desired(setting) {
                problems.push(format!("macos/{}.toml: {e:#}", file.name()));
            }
        }
    }
    problems
}

/// `macos diff`: read-only drift report.
pub fn diff(cfg: &Config, only: Option<&[String]>) -> Result<Summary> {
    let mut summary = Summary::new();
    let mut store = Store::new();
    for file in selected_files(cfg, only)? {
        ui::heading(format!("macOS · {}", file.name()));
        for setting in &file.defaults {
            let label = label(setting);
            // A value that does not match its declared type is a config bug,
            // not drift; report it as a failure rather than a pending change.
            let status = match is_in_sync(setting, &mut store) {
                Ok(true) => Status::Unchanged,
                Ok(false) => Status::Changed,
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    Status::Failed
                }
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
    let mut store = Store::new();

    for file in selected_files(cfg, only)? {
        ui::heading(format!("macOS · {}", file.name()));
        // Restarting Dock/Finder is user-visible (lost windows, flicker), so a
        // file that changed nothing must not trigger its killall.
        let changed_before = summary.changed;

        for setting in &file.defaults {
            let label = label(setting);
            match is_in_sync(setting, &mut store) {
                Ok(true) => {
                    summary.record(Status::Unchanged, &label);
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    summary.record(Status::Failed, &label);
                    continue;
                }
            }
            if dry {
                summary.record(Status::Changed, &format!("{label} [dry-run]"));
                continue;
            }
            remember_previous(state, setting, &mut store);
            match write_default(setting) {
                Ok(()) => {
                    store.invalidate(&setting.domain, setting.host);
                    summary.record(Status::Changed, &label)
                }
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
///
/// Only used for state written by older macboot versions, which archived the
/// flattened `defaults read` text plus a `read-type` name. Records written now
/// carry the full plist and restore through XML instead, so no type is lost.
fn flag_for_type(plist_type: &str) -> Option<&'static str> {
    match plist_type {
        "boolean" => Some("-bool"),
        "integer" => Some("-int"),
        "float" | "real" => Some("-float"),
        "string" => Some("-string"),
        "date" => Some("-date"),
        // A one-line capture of an array or dict cannot be written back.
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
        let label = format!("{} {}{}", record.domain, record.key, record.host.suffix());
        if dry {
            summary.record(
                Status::Changed,
                &format!("{label} ({}) [dry-run]", describe_restore(&record)),
            );
            continue;
        }
        match revert_one(&record) {
            Ok(true) => {
                state.remove_default(&record.domain, &record.key, record.host);
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

/// What `revert` would do to a key, for the dry-run line.
fn describe_restore(record: &DefaultRecord) -> String {
    if let Some(xml) = &record.previous_plist {
        return match value::from_xml(xml) {
            Ok(v) => format!("restore to {}", value::describe(&v)),
            Err(_) => "restore".to_string(),
        };
    }
    match &record.previous {
        Some(v) => format!("restore to {v}"),
        None => "delete".to_string(),
    }
}

/// Restore one key to its pre-macboot value. Returns false when the record is
/// an old-format one whose type cannot be written back.
fn revert_one(record: &DefaultRecord) -> Result<bool> {
    let run = |args: &[&str]| run_defaults(record.sudo, record.host, args);

    // Current records archive the full plist, so any type restores exactly.
    if let Some(xml) = &record.previous_plist {
        // Re-parsing proves the archive is intact before we overwrite the
        // live value with it.
        let restored = value::from_xml(xml)
            .with_context(|| format!("restoring {} {}", record.domain, record.key))?;
        let payload = value::to_xml(&restored)?;
        run(&["write", &record.domain, &record.key, &payload])?;
        return Ok(true);
    }

    let Some(previous) = &record.previous else {
        // The key did not exist before macboot; deleting restores that.
        run(&["delete", &record.domain, &record.key])?;
        return Ok(true);
    };
    let plist_type = record.previous_type.as_deref().unwrap_or("string");
    let Some(flag) = flag_for_type(plist_type) else {
        ui::warn(format!(
            "{} {} was a {plist_type} recorded before macboot archived full \
             values; restore it by hand",
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
            host: HostScope::Any,
        }
    }

    fn record(previous: Option<&str>, host: HostScope, recorded: &str) -> DefaultRecord {
        DefaultRecord {
            domain: "com.example".into(),
            key: "k".into(),
            previous_plist: previous.map(String::from),
            previous: None,
            previous_type: None,
            sudo: false,
            host,
            recorded: recorded.into(),
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
    fn legacy_records_map_their_type_to_a_write_flag() {
        assert_eq!(flag_for_type("boolean"), Some("-bool"));
        assert_eq!(flag_for_type("integer"), Some("-int"));
        assert_eq!(flag_for_type("string"), Some("-string"));
        // Shapes an old macboot captured as a one-line string can't be written
        // back; records written now carry XML and avoid this path entirely.
        assert_eq!(flag_for_type("dictionary"), None);
        assert_eq!(flag_for_type("array"), None);
    }

    #[test]
    fn first_write_wins_so_revert_reaches_the_original() {
        let mut state = State::default();
        state.insert_default(record(Some("<48/>"), HostScope::Any, "1"));
        // A later apply must not overwrite the pre-macboot value with macboot's.
        state.insert_default(record(Some("<36/>"), HostScope::Any, "2"));
        assert_eq!(
            state.defaults["com.example k"].previous_plist.as_deref(),
            Some("<48/>")
        );
    }

    /// A domain+key pair holds independent values in the two preference stores,
    /// so recording one must not make macboot think it knows the other.
    #[test]
    fn the_two_host_scopes_are_tracked_separately() {
        let mut state = State::default();
        state.insert_default(record(Some("<1/>"), HostScope::Any, "1"));
        assert!(state.knows_default("com.example", "k", HostScope::Any));
        assert!(!state.knows_default("com.example", "k", HostScope::Current));

        state.insert_default(record(Some("<2/>"), HostScope::Current, "2"));
        assert_eq!(state.defaults.len(), 2);
        assert!(state
            .remove_default("com.example", "k", HostScope::Current)
            .is_some());
        assert!(state.knows_default("com.example", "k", HostScope::Any));
    }

    /// Complex values are exactly what the old text-capture revert lost, so the
    /// archive has to survive a save/load cycle intact.
    #[test]
    fn an_archived_dict_survives_the_state_round_trip() {
        let mut dict = plist::Dictionary::new();
        dict.insert("Battery".into(), plist::Value::Integer(18.into()));
        let original = plist::Value::Dictionary(dict);

        let xml = value::to_xml(&original).unwrap();
        let mut state = State::default();
        state.insert_default(record(Some(&xml), HostScope::Any, "1"));

        let json = serde_json::to_string(&state).unwrap();
        let reloaded: State = serde_json::from_str(&json).unwrap();
        let stored = reloaded.defaults["com.example k"]
            .previous_plist
            .as_deref()
            .unwrap();
        assert!(value::equal(&original, &value::from_xml(stored).unwrap()));
    }

    /// State files written before ByHost and XML archiving must still revert.
    #[test]
    fn legacy_state_json_still_loads_and_reverts() {
        let json = r#"{
            "links": {},
            "defaults": {
                "com.apple.dock tilesize": {
                    "domain": "com.apple.dock",
                    "key": "tilesize",
                    "previous": "48",
                    "previous_type": "integer",
                    "sudo": false,
                    "recorded": "old"
                }
            }
        }"#;
        let state: State = serde_json::from_str(json).unwrap();
        let record = &state.defaults["com.apple.dock tilesize"];
        assert_eq!(record.host, HostScope::Any);
        assert!(record.previous_plist.is_none());
        assert_eq!(describe_restore(record), "restore to 48");
    }

    #[test]
    fn a_key_that_did_not_exist_reverts_by_deletion() {
        assert_eq!(
            describe_restore(&record(None, HostScope::Any, "1")),
            "delete"
        );
    }

    #[test]
    fn host_scope_selects_the_currenthost_flag() {
        assert!(HostScope::Any.args().is_empty());
        assert_eq!(HostScope::Current.args(), &["-currentHost"]);
    }

    #[test]
    fn complex_types_have_no_write_flag_and_go_through_xml() {
        let (flag, _) = write_value(&setting(
            DefaultType::Array,
            toml::Value::Array(vec![toml::Value::Integer(1)]),
        ));
        assert!(flag.is_empty());
    }

    #[test]
    fn validate_reports_a_value_that_contradicts_its_type() {
        let mut cfg = cfg_with_files(&["dock"]);
        cfg.macos[0].defaults = vec![DefaultSetting {
            domain: "com.apple.dock".into(),
            key: "tilesize".into(),
            kind: DefaultType::Int,
            value: toml::Value::String("big".into()),
            sudo: false,
            host: HostScope::Any,
        }];
        let problems = validate(&cfg);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("macos/dock.toml"), "{problems:?}");
        assert!(problems[0].contains("tilesize"), "{problems:?}");
    }

    #[test]
    fn validate_passes_a_well_formed_complex_value() {
        let mut cfg = cfg_with_files(&["dock"]);
        cfg.macos[0].defaults = vec![DefaultSetting {
            domain: "com.apple.dock".into(),
            key: "persistent-apps".into(),
            kind: DefaultType::Array,
            value: toml::Value::Array(vec![toml::Value::String("Safari".into())]),
            sudo: false,
            host: HostScope::Any,
        }];
        assert!(validate(&cfg).is_empty());
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

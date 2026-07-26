//! Declarative macOS settings: `defaults` writes, the command escape hatch,
//! keybindings, and per-app menu shortcuts. Every setting is read before it is
//! written, so `apply` is idempotent and `diff` shows real drift.

pub mod dump;
pub mod keyboard;
pub mod managed;
pub mod value;

use crate::config::{Config, DefaultSetting, DefaultType, HostScope, MacosFile};
use crate::proc;
use crate::state::{DefaultRecord, State};
use crate::ui::{self, Status, Summary};
use anyhow::{bail, Context, Result};
use managed::Managed;
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

/// Best-effort check for Full Disk Access. Opening `TCC.db` itself is gated
/// behind it, so failing to open it is a reliable (if indirect) signal —
/// without it, `macos dump`/`apply` can silently skip protected domains
/// (Mail, Safari, Messages, TCC) the same way `export_domain` does. Returns
/// `true` (don't false-alarm) if `$HOME` can't be resolved at all.
pub fn has_full_disk_access() -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    let tcc_db = home.join("Library/Application Support/com.apple.TCC/TCC.db");
    std::fs::File::open(&tcc_db).is_ok()
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

/// What `diff`/`apply` should do about one setting.
#[derive(Debug, PartialEq)]
enum Resolved {
    /// The machine already holds the desired value.
    Matches,
    /// The machine holds something else, and a write would fix it.
    Drifted,
    /// A configuration profile forces a *different* value. Writing the key
    /// would succeed and change nothing an app can see, so we don't.
    Forced(plist::Value),
}

/// Compare the declared value against what the machine actually resolves to.
///
/// A managed (profile-forced) key is checked first: the profile wins over
/// whatever the user domain holds, so the forced value — not the exported one —
/// is what the machine effectively has. A forced key that already matches is
/// simply in sync; one that doesn't is out of our hands rather than drift.
fn evaluate(
    setting: &DefaultSetting,
    store: &mut Store,
    managed: &mut Managed,
) -> Result<Resolved> {
    let want = desired(setting)?;
    if let Some(forced) = managed.forced(&setting.domain, &setting.key) {
        return Ok(if value::equal(&want, &forced) {
            Resolved::Matches
        } else {
            Resolved::Forced(forced)
        });
    }
    let Some(current) = store.read(&setting.domain, &setting.key, setting.host) else {
        return Ok(Resolved::Drifted);
    };
    Ok(if value::equal(&want, &current) {
        Resolved::Matches
    } else {
        Resolved::Drifted
    })
}

/// The summary line for a key we refuse to write because a profile owns it.
fn forced_label(setting: &DefaultSetting, forced: &plist::Value) -> String {
    format!(
        "{} (forced to {} by configuration profile)",
        label(setting),
        value::describe(forced)
    )
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

/// Declared keys an MDM profile forces to a different value, as summary labels.
///
/// Surfaced by `doctor` because the symptom is otherwise baffling: `apply`
/// reports success, `diff` still shows the key, and nothing on screen explains
/// that the machine is never going to accept the value.
pub fn forced_conflicts(cfg: &Config) -> Vec<String> {
    let mut managed = Managed::new();
    if !managed.is_active() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for file in &cfg.macos {
        for setting in &file.defaults {
            // A value that fails its own type check is `validate`'s problem.
            let Ok(want) = desired(setting) else { continue };
            if let Some(forced) = managed.forced(&setting.domain, &setting.key) {
                if !value::equal(&want, &forced) {
                    out.push(format!(
                        "macos/{}.toml: {}",
                        file.name(),
                        forced_label(setting, &forced)
                    ));
                }
            }
        }
    }
    out
}

/// `macos get`: the live value of a domain (or one key), rendered as TOML.
///
/// The read-only counterpart to `dump`: no noise filter, no diff, no file
/// written — just what the machine holds right now, in both host scopes, with
/// profile-forced keys called out. Returns the text for stdout.
pub fn get(domain: &str, key: Option<&str>, keys_only: bool, managed_only: bool) -> Result<String> {
    let mut managed = Managed::new();
    let scopes = [HostScope::Any, HostScope::Current];

    if let Some(key) = key {
        // Single-key reads stay bare so they can be captured in a shell
        // variable; the "this is forced" note goes to stderr as commentary.
        if let Some(forced) = managed.forced(domain, key) {
            ui::warn(format!(
                "{domain} {key} is forced by a configuration profile"
            ));
            return Ok(format!("{}\n", literal(&forced)));
        }
        if managed_only {
            bail!("{domain} {key} is not managed by a configuration profile");
        }
        for scope in scopes {
            if let Some(value) =
                export_domain(domain, scope).and_then(|keys| keys.get(key).cloned())
            {
                return Ok(format!("{}\n", literal(&value)));
            }
        }
        bail!("no key '{key}' in domain '{domain}'");
    }

    // Export each scope once: a `defaults` process per scope, never per key.
    let exported: Vec<(HostScope, BTreeMap<String, plist::Value>)> = if managed_only {
        Vec::new()
    } else {
        scopes
            .iter()
            .filter_map(|s| export_domain(domain, *s).map(|keys| (*s, keys)))
            .collect()
    };
    let forced = managed.keys(domain);

    // `--keys` output is meant for `xargs`, so it carries no scope headings and
    // no comments — just the union of the names, deduplicated across scopes.
    if keys_only {
        let names: BTreeSet<&String> = exported
            .iter()
            .flat_map(|(_, keys)| keys.keys())
            .chain(forced.keys())
            .collect();
        if names.is_empty() {
            bail!("{}", nothing_found(domain, managed_only));
        }
        return Ok(names.iter().fold(String::new(), |mut acc, k| {
            acc.push_str(k);
            acc.push('\n');
            acc
        }));
    }

    let mut out = String::new();
    for (scope, keys) in &exported {
        if keys.is_empty() {
            continue;
        }
        out.push_str(&format!("# {domain}{}\n", scope.suffix()));
        for (k, v) in keys {
            out.push_str(&format!("{} = {}", quote_key(k), literal(v)));
            // The profile wins, so flag any key whose exported value is not the
            // one an app would actually be handed.
            if let Some(profile_value) = forced.get(k) {
                if value::equal(profile_value, v) {
                    out.push_str("  # forced by profile");
                } else {
                    out.push_str(&format!(
                        "  # forced to {} by profile",
                        value::describe(profile_value)
                    ));
                }
            }
            out.push('\n');
        }
    }

    // A profile can force a key the user domain has never held, so list managed
    // keys no scope reported; otherwise they would be invisible here.
    let seen: BTreeSet<&String> = exported.iter().flat_map(|(_, keys)| keys.keys()).collect();
    let unseen: Vec<(&String, &plist::Value)> =
        forced.iter().filter(|(k, _)| !seen.contains(k)).collect();
    if !unseen.is_empty() {
        out.push_str(&format!("# {domain} (forced by configuration profile)\n"));
        for (k, v) in unseen {
            out.push_str(&format!("{} = {}\n", quote_key(k), literal(v)));
        }
    }

    if out.is_empty() {
        bail!("{}", nothing_found(domain, managed_only));
    }
    Ok(out)
}

/// Why `get` found nothing. `--managed` failing on an unmanaged Mac is the
/// common case and deserves to say so, rather than implying the domain is
/// unreadable.
fn nothing_found(domain: &str, managed_only: bool) -> String {
    if managed_only {
        format!("no keys in '{domain}' are forced by a configuration profile")
    } else {
        format!("no preferences readable in domain '{domain}'")
    }
}

/// A plist value as a TOML literal, falling back to a description for shapes
/// TOML cannot express (rather than printing nothing).
fn literal(value: &plist::Value) -> String {
    value::render(value, 0).unwrap_or_else(|| value::describe(value))
}

/// Preference keys contain spaces and dots freely, so quote anything that is
/// not a bare TOML key.
fn quote_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_string()
    } else {
        format!("{key:?}")
    }
}

/// `macos diff`: read-only drift report.
pub fn diff(cfg: &Config, only: Option<&[String]>) -> Result<Summary> {
    let mut summary = Summary::new();
    let mut store = Store::new();
    let mut managed = Managed::new();
    for file in selected_files(cfg, only)? {
        ui::heading(format!("macOS · {}", file.name()));
        for setting in &file.defaults {
            let label = label(setting);
            // A value that does not match its declared type is a config bug,
            // not drift; report it as a failure rather than a pending change.
            match evaluate(setting, &mut store, &mut managed) {
                Ok(Resolved::Matches) => summary.record(Status::Unchanged, &label),
                Ok(Resolved::Drifted) => summary.record(Status::Changed, &label),
                Ok(Resolved::Forced(forced)) => {
                    summary.record(Status::Skipped, &forced_label(setting, &forced))
                }
                Err(e) => {
                    ui::err(format!("{label}: {e:#}"));
                    summary.record(Status::Failed, &label);
                }
            }
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
    let mut managed = Managed::new();

    for file in selected_files(cfg, only)? {
        ui::heading(format!("macOS · {}", file.name()));
        // Restarting Dock/Finder is user-visible (lost windows, flicker), so a
        // file that changed nothing must not trigger its killall.
        let changed_before = summary.changed;

        for setting in &file.defaults {
            let label = label(setting);
            match evaluate(setting, &mut store, &mut managed) {
                Ok(Resolved::Matches) => {
                    summary.record(Status::Unchanged, &label);
                    continue;
                }
                Ok(Resolved::Drifted) => {}
                // Never write a forced key: the write would report success, the
                // effective value would not move, and state would gain a
                // revert record for a change that never happened.
                Ok(Resolved::Forced(forced)) => {
                    summary.record(Status::Skipped, &forced_label(setting, &forced));
                    ui::detail(
                        "an MDM configuration profile owns this key; remove it from the \
                         config or change it in the profile",
                    );
                    continue;
                }
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

    /// A `Managed` backed by a temp root holding one forced key on `com.example`,
    /// the domain `setting()` uses.
    fn managed_forcing(value: plist::Value) -> (tempfile::TempDir, Managed) {
        let dir = tempfile::tempdir().unwrap();
        let dict = plist::Dictionary::from_iter([("k".to_string(), value)]);
        plist::to_file_xml(
            dir.path().join("com.example.plist"),
            &plist::Value::Dictionary(dict),
        )
        .unwrap();
        let managed = Managed::with_root(dir.path().to_path_buf(), None);
        (dir, managed)
    }

    /// The forced value already being right means the machine is in sync — and
    /// crucially, the user domain is never even consulted.
    #[test]
    fn a_forced_key_that_matches_is_in_sync() {
        let (_dir, mut managed) = managed_forcing(plist::Value::Integer(36.into()));
        let s = setting(DefaultType::Int, toml::Value::Integer(36));
        let sync = evaluate(&s, &mut Store::new(), &mut managed).unwrap();
        assert_eq!(sync, Resolved::Matches);
    }

    /// The case that motivates all of this: without the managed check this key
    /// would be reported as drift forever and rewritten on every apply.
    #[test]
    fn a_forced_key_that_differs_is_not_drift() {
        let (_dir, mut managed) = managed_forcing(plist::Value::Integer(64.into()));
        let s = setting(DefaultType::Int, toml::Value::Integer(36));
        let sync = evaluate(&s, &mut Store::new(), &mut managed).unwrap();
        assert_eq!(sync, Resolved::Forced(plist::Value::Integer(64.into())));
    }

    /// Forced values go through `value::equal`, not `==`, for the same reason
    /// live ones do: a profile may store a boolean as 1.
    #[test]
    fn a_forced_bool_stored_as_an_int_still_matches() {
        let (_dir, mut managed) = managed_forcing(plist::Value::Integer(1.into()));
        let s = setting(DefaultType::Bool, toml::Value::Boolean(true));
        assert_eq!(
            evaluate(&s, &mut Store::new(), &mut managed).unwrap(),
            Resolved::Matches
        );
    }

    #[test]
    fn forced_label_names_the_value_the_profile_imposes() {
        let s = setting(DefaultType::Int, toml::Value::Integer(36));
        let label = forced_label(&s, &plist::Value::Integer(64.into()));
        assert!(label.contains("com.example k"), "{label}");
        assert!(label.contains("64"), "{label}");
        assert!(label.contains("configuration profile"), "{label}");
    }

    /// Preference keys are full of spaces and dots, and `macos get` output is
    /// meant to be readable as TOML.
    #[test]
    fn get_output_quotes_keys_that_are_not_bare() {
        assert_eq!(quote_key("tilesize"), "tilesize");
        assert_eq!(quote_key("AppleShowAllFiles-2"), "AppleShowAllFiles-2");
        assert_eq!(
            quote_key("com.apple.trackpad.scaling"),
            "\"com.apple.trackpad.scaling\""
        );
        assert_eq!(
            quote_key("NSToolbar Configuration"),
            "\"NSToolbar Configuration\""
        );
    }
}

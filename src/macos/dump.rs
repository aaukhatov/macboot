//! `macboot macos dump` — the reverse of `macos apply`.
//!
//! Snapshot every `defaults` domain, let the user change whatever they like in
//! System Settings, snapshot again, and emit the difference as ready-to-paste
//! `[[defaults]]` blocks. This is the discovery half of the tool: it removes the
//! need to know a domain/key pair before you can manage a setting.
//!
//! The same idea as [`super::keyboard::dump`], generalized past symbolichotkeys.

use super::value;
use crate::config::HostScope;
use crate::proc;
use crate::ui;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

/// Every key of every domain, in both preference stores, at one point in time.
/// Keyed by domain *and* scope, since the same domain holds different keys in
/// each.
type Snapshot = BTreeMap<(String, HostScope), BTreeMap<String, plist::Value>>;

/// One key that appeared or changed between the two snapshots.
pub struct Change {
    pub domain: String,
    pub key: String,
    pub host: HostScope,
    pub value: plist::Value,
    /// The value before the user's edit, if the key already existed.
    pub previous: Option<plist::Value>,
}

/// Keys that churn on their own (window geometry, recent-item lists, session
/// bookkeeping). Recording these produces noise that is never worth managing,
/// so they are filtered unless `--all` is passed. Matched case-insensitively as
/// substrings of the key name.
const NOISY_KEY_FRAGMENTS: &[&str] = &[
    "frame",
    "recent",
    "lastused",
    "lastsession",
    "lastlaunch",
    "nsnavpanel",
    "nssplitview",
    "nstableview",
    "nstoolbar",
    "nswindow",
    "tabviewselected",
    "uuid",
    "timestamp",
    "lastupdate",
    "cachedate",
    "sessionid",
];

/// Domains whose entire contents are runtime state rather than preferences.
const NOISY_DOMAINS: &[&str] = &[
    "com.apple.spaces",
    "com.apple.dock.extra",
    "com.apple.windowmanager.plist",
    "com.apple.assistant.backedup",
    "com.apple.identityservicesd",
    "com.apple.sharedfilelist",
];

/// Domain → the app that must be restarted for a change to take effect. Used to
/// pre-fill `killall` in the dumped file.
const KILLALL_FOR_DOMAIN: &[(&str, &str)] = &[
    ("com.apple.dock", "Dock"),
    ("com.apple.finder", "Finder"),
    ("com.apple.systemuiserver", "SystemUIServer"),
    ("com.apple.controlcenter", "ControlCenter"),
    ("com.apple.notificationcenterui", "NotificationCenter"),
    ("com.apple.universalaccess", "Dock"),
];

/// Every preference domain on this machine, plus `NSGlobalDomain`.
pub fn list_domains() -> Result<Vec<String>> {
    let out = proc::output("defaults", &["domains"]).context("listing defaults domains")?;
    let mut domains: Vec<String> = out
        .split(',')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    domains.push("NSGlobalDomain".to_string());
    domains.sort();
    domains.dedup();
    Ok(domains)
}

/// Snapshot all `domains` in both preference stores. Exporting 700+ domains
/// serially takes ~14s, so the work is split across a few scoped threads; each
/// one only touches its own chunk and the results are merged on the main
/// thread.
///
/// Both scopes are captured because a setting the user changes in System
/// Settings may land in either one, and the point of `dump` is that they should
/// not have to know which.
///
/// Also returns the domains whose default (`Any`-scope) export failed. Those
/// domains came from `defaults domains`, so macOS itself confirms they exist —
/// a failed export there is almost always the terminal lacking Full Disk
/// Access rather than the domain being genuinely empty. `Current` (ByHost)
/// failures are not tracked: most domains simply have no ByHost data, so that
/// would flag normal domains as unreadable.
fn snapshot(domains: &[String]) -> (Snapshot, Vec<String>) {
    const THREADS: usize = 8;
    const SCOPES: [HostScope; 2] = [HostScope::Any, HostScope::Current];
    let chunk = domains.len().div_ceil(THREADS).max(1);
    let mut merged = Snapshot::new();
    let mut failed = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = domains
            .chunks(chunk)
            .map(|group| {
                scope.spawn(move || {
                    let mut local = Snapshot::new();
                    let mut local_failed = Vec::new();
                    for domain in group {
                        for host in SCOPES {
                            match super::export_domain(domain, host) {
                                Some(keys) => {
                                    local.insert((domain.clone(), host), keys);
                                }
                                None if host == HostScope::Any => {
                                    local_failed.push(domain.clone());
                                }
                                None => {}
                            }
                        }
                    }
                    (local, local_failed)
                })
            })
            .collect();
        for handle in handles {
            if let Ok((local, local_failed)) = handle.join() {
                merged.extend(local);
                failed.extend(local_failed);
            }
        }
    });
    failed.sort();
    failed.dedup();
    (merged, failed)
}

/// Warn about domains that came back unreadable from one or both snapshots.
fn warn_unreadable(domains: &[String]) {
    if domains.is_empty() {
        return;
    }
    ui::warn(format!(
        "{} domain(s) could not be read and were skipped: {}",
        domains.len(),
        domains.join(", ")
    ));
    ui::detail(
        "This usually means the terminal running macboot lacks Full Disk Access \
         (System Settings → Privacy & Security → Full Disk Access) — affected apps \
         commonly include Mail, Safari, Messages, and TCC itself.",
    );
}

fn is_noisy(domain: &str, key: &str) -> bool {
    if NOISY_DOMAINS.iter().any(|d| domain.starts_with(d)) {
        return true;
    }
    let lower = key.to_lowercase();
    NOISY_KEY_FRAGMENTS.iter().any(|f| lower.contains(f))
}

/// Keys present-and-different or newly added in `after`. Deletions are ignored:
/// `[[defaults]]` can only express a value, not its absence.
fn changes(before: &Snapshot, after: &Snapshot, all: bool) -> Vec<Change> {
    let mut out = Vec::new();
    for ((domain, host), keys) in after {
        let old = before.get(&(domain.clone(), *host));
        for (key, value) in keys {
            if !all && is_noisy(domain, key) {
                continue;
            }
            let previous = old.and_then(|o| o.get(key));
            if previous == Some(value) {
                continue;
            }
            out.push(Change {
                domain: domain.clone(),
                key: key.clone(),
                host: *host,
                value: value.clone(),
                previous: previous.cloned(),
            });
        }
    }
    out
}

/// The `type =` / `value =` pair for a plist value, or None for a shape with no
/// `[[defaults]]` representation.
fn toml_value(v: &plist::Value) -> Option<(&'static str, String)> {
    // `value = ` is 8 characters, which is where a multi-line array continues.
    let rendered = value::render(v, 8)?;
    Some((value::type_name(value::kind_of(v)?), rendered))
}

/// Render captured changes as the body of a `macos/*.toml` file.
pub fn to_toml(changes: &[Change]) -> String {
    let mut out = String::from("# Generated by `macboot macos dump`.\n");
    out.push_str("# One System Settings toggle often writes several keys — keep what you meant\n");
    out.push_str("# to change and delete the rest before committing this file.\n\n");

    let mut apps: Vec<&str> = changes
        .iter()
        .filter_map(|c| {
            KILLALL_FOR_DOMAIN
                .iter()
                .find(|(d, _)| *d == c.domain)
                .map(|(_, app)| *app)
        })
        .collect();
    apps.sort_unstable();
    apps.dedup();
    if !apps.is_empty() {
        let rendered: Vec<String> = apps.iter().map(|a| format!("{a:?}")).collect();
        out.push_str(&format!("killall = [{}]\n\n", rendered.join(", ")));
    }

    for change in changes {
        match toml_value(&change.value) {
            Some((kind, rendered)) => {
                out.push_str("[[defaults]]\n");
                out.push_str(&format!("domain = {:?}\n", change.domain));
                out.push_str(&format!("key = {:?}\n", change.key));
                out.push_str(&format!("type = {kind:?}\n"));
                if change.host == HostScope::Current {
                    out.push_str("host = \"current\"\n");
                }
                out.push_str(&format!("value = {rendered}\n"));
                if let Some(previous) = &change.previous {
                    out.push_str(&format!("# was: {}\n", value::describe(previous)));
                }
            }
            // Keep unsupported shapes visible rather than dropping them: the
            // user can still apply them via the [[command]] escape hatch.
            None => {
                out.push_str(&format!(
                    "# {} {} changed to {} — not expressible as [[defaults]];\n\
                     # use a [[command]] with `defaults write` if you need it.\n",
                    change.domain,
                    change.key,
                    value::describe(&change.value)
                ));
            }
        }
        out.push('\n');
    }
    out
}

/// Run a full dump session: snapshot, wait for the user, snapshot again.
pub fn dump(domains: &[String], all: bool) -> Result<Vec<Change>> {
    let targets = if domains.is_empty() {
        list_domains()?
    } else {
        domains.to_vec()
    };

    ui::info(format!("Snapshotting {} domain(s)…", targets.len()));
    let (before, before_failed) = snapshot(&targets);
    ui::detail(format!("captured {} domain(s)", before.len()));

    if !ui::pause("Change what you want in System Settings, then press Enter") {
        bail!("stdin closed before the second snapshot; nothing was dumped");
    }

    ui::info("Re-snapshotting…");
    let (after, after_failed) = snapshot(&targets);

    let mut unreadable = before_failed;
    unreadable.extend(after_failed);
    unreadable.sort();
    unreadable.dedup();
    warn_unreadable(&unreadable);

    Ok(changes(&before, &after, all))
}

/// Capture every currently-set key as a `Change` with no before/after diff and
/// no pause for editing. Unlike `dump`, this needs nothing to change during the
/// run, so it can capture a machine that is already configured the way you
/// want it — `dump`'s diff only ever sees settings you touch *during* the
/// session.
pub fn snapshot_now(domains: &[String], all: bool) -> Result<Vec<Change>> {
    let targets = if domains.is_empty() {
        list_domains()?
    } else {
        domains.to_vec()
    };

    ui::info(format!("Snapshotting {} domain(s)…", targets.len()));
    let (current, failed) = snapshot(&targets);
    ui::detail(format!("captured {} domain(s)", current.len()));
    warn_unreadable(&failed);

    Ok(as_changes(&current, all))
}

/// Every key in `snap` as a `Change` with no `previous` value, filtered
/// through the same noise heuristics as a real diff.
fn as_changes(snap: &Snapshot, all: bool) -> Vec<Change> {
    let mut out = Vec::new();
    for ((domain, host), keys) in snap {
        for (key, value) in keys {
            if !all && is_noisy(domain, key) {
                continue;
            }
            out.push(Change {
                domain: domain.clone(),
                key: key.clone(),
                host: *host,
                value: value.clone(),
                previous: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, &str, plist::Value)]) -> Snapshot {
        scoped_snap(HostScope::Any, pairs)
    }

    fn scoped_snap(host: HostScope, pairs: &[(&str, &str, plist::Value)]) -> Snapshot {
        let mut out = Snapshot::new();
        for (domain, key, value) in pairs {
            out.entry((domain.to_string(), host))
                .or_default()
                .insert(key.to_string(), value.clone());
        }
        out
    }

    #[test]
    fn only_changed_keys_are_reported() {
        let before = snap(&[
            ("com.apple.dock", "autohide", plist::Value::Boolean(false)),
            (
                "com.apple.dock",
                "tilesize",
                plist::Value::Integer(48.into()),
            ),
        ]);
        let after = snap(&[
            ("com.apple.dock", "autohide", plist::Value::Boolean(true)),
            (
                "com.apple.dock",
                "tilesize",
                plist::Value::Integer(48.into()),
            ),
        ]);
        let got = changes(&before, &after, false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "autohide");
        assert_eq!(got[0].previous, Some(plist::Value::Boolean(false)));
    }

    #[test]
    fn new_key_is_reported_without_previous() {
        let after = snap(&[(
            "com.apple.finder",
            "ShowPathbar",
            plist::Value::Boolean(true),
        )]);
        let got = changes(&Snapshot::new(), &after, false);
        assert_eq!(got.len(), 1);
        assert!(got[0].previous.is_none());
    }

    #[test]
    fn churn_keys_are_filtered_unless_all() {
        let after = snap(&[(
            "com.apple.finder",
            "NSWindow Frame NSNavPanel",
            plist::Value::String("0 0 100 100".into()),
        )]);
        assert!(changes(&Snapshot::new(), &after, false).is_empty());
        assert_eq!(changes(&Snapshot::new(), &after, true).len(), 1);
    }

    #[test]
    fn as_changes_reports_every_key_with_no_previous() {
        let current = snap(&[
            ("com.apple.dock", "autohide", plist::Value::Boolean(true)),
            (
                "com.apple.dock",
                "tilesize",
                plist::Value::Integer(48.into()),
            ),
        ]);
        let got = as_changes(&current, false);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|c| c.previous.is_none()));
    }

    #[test]
    fn as_changes_filters_churn_unless_all() {
        let current = snap(&[(
            "com.apple.finder",
            "NSWindow Frame NSNavPanel",
            plist::Value::String("0 0 100 100".into()),
        )]);
        assert!(as_changes(&current, false).is_empty());
        assert_eq!(as_changes(&current, true).len(), 1);
    }

    #[test]
    fn toml_output_is_parseable_and_infers_killall() {
        let got = changes(
            &Snapshot::new(),
            &snap(&[("com.apple.dock", "autohide", plist::Value::Boolean(true))]),
            false,
        );
        let rendered = to_toml(&got);
        assert!(rendered.contains(r#"killall = ["Dock"]"#), "{rendered}");
        let parsed: crate::config::MacosFile = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed.defaults.len(), 1);
        assert_eq!(parsed.defaults[0].key, "autohide");
        assert_eq!(parsed.killall, vec!["Dock".to_string()]);
    }

    /// Dumping the machine and applying the result must describe the same
    /// settings, so whatever `to_toml` emits has to parse back into the very
    /// values it was rendered from.
    fn assert_dump_round_trips(host: HostScope, domain: &str, key: &str, live: plist::Value) {
        let got = changes(
            &Snapshot::new(),
            &scoped_snap(host, &[(domain, key, live.clone())]),
            false,
        );
        let rendered = to_toml(&got);
        let parsed: crate::config::MacosFile = toml::from_str(&rendered)
            .unwrap_or_else(|e| panic!("dump did not parse back: {e}\n{rendered}"));
        assert_eq!(parsed.defaults.len(), 1, "{rendered}");
        let setting = &parsed.defaults[0];
        assert_eq!(setting.key, key);
        assert_eq!(setting.host, host, "{rendered}");
        let reparsed = value::to_plist(setting.kind, &setting.value)
            .unwrap_or_else(|e| panic!("re-parsing dumped value: {e:#}\n{rendered}"));
        assert!(
            value::equal(&live, &reparsed),
            "dump lost the value: {live:?} -> {reparsed:?}\n{rendered}"
        );
    }

    #[test]
    fn an_array_dumps_and_parses_back() {
        assert_dump_round_trips(
            HostScope::Any,
            "com.apple.dock",
            "persistent-others",
            plist::Value::Array(vec![
                plist::Value::String("a".into()),
                plist::Value::Integer(2.into()),
            ]),
        );
    }

    /// Control Center's menu bar layout: a ByHost dict, which is both of the
    /// shapes the old dump could not express.
    #[test]
    fn a_byhost_dict_dumps_with_its_scope_and_parses_back() {
        let mut dict = plist::Dictionary::new();
        dict.insert("Battery".into(), plist::Value::Integer(18.into()));
        dict.insert("Bluetooth".into(), plist::Value::Integer(2.into()));
        assert_dump_round_trips(
            HostScope::Current,
            "com.apple.controlcenter",
            "MenuBar",
            plist::Value::Dictionary(dict),
        );
    }

    #[test]
    fn a_nested_array_of_dicts_dumps_and_parses_back() {
        let mut tile = plist::Dictionary::new();
        tile.insert("file-label".into(), plist::Value::String("Safari".into()));
        tile.insert("tile-type".into(), plist::Value::String("file-tile".into()));
        assert_dump_round_trips(
            HostScope::Any,
            "com.apple.dock",
            "persistent-apps",
            plist::Value::Array(vec![plist::Value::Dictionary(tile)]),
        );
    }

    #[test]
    fn the_default_scope_is_not_annotated() {
        let got = changes(
            &Snapshot::new(),
            &snap(&[("com.apple.dock", "autohide", plist::Value::Boolean(true))]),
            false,
        );
        assert!(!to_toml(&got).contains("host ="));
    }

    /// The same key in both stores is two settings, not one.
    #[test]
    fn both_scopes_are_reported_independently() {
        let mut after = scoped_snap(
            HostScope::Any,
            &[("com.apple.x", "k", plist::Value::Integer(1.into()))],
        );
        after.extend(scoped_snap(
            HostScope::Current,
            &[("com.apple.x", "k", plist::Value::Integer(2.into()))],
        ));
        let got = changes(&Snapshot::new(), &after, false);
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|c| c.host == HostScope::Current));
        assert!(got.iter().any(|c| c.host == HostScope::Any));
    }
}

//! `macboot macos record` — the reverse of `macos apply`.
//!
//! Snapshot every `defaults` domain, let the user change whatever they like in
//! System Settings, snapshot again, and emit the difference as ready-to-paste
//! `[[defaults]]` blocks. This is the discovery half of the tool: it removes the
//! need to know a domain/key pair before you can manage a setting.
//!
//! The same idea as [`super::keyboard::dump`], generalized past symbolichotkeys.

use crate::proc;
use crate::ui;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

/// Every key of every domain at one point in time.
type Snapshot = BTreeMap<String, BTreeMap<String, plist::Value>>;

/// One key that appeared or changed between the two snapshots.
pub struct Change {
    pub domain: String,
    pub key: String,
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
/// pre-fill `killall` in the recorded file.
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

/// Read one domain into a key → value map. A domain that cannot be exported
/// (sandboxed, malformed, removed mid-run) is simply absent from the snapshot.
fn export_domain(domain: &str) -> Option<BTreeMap<String, plist::Value>> {
    let out = proc::capture("defaults", &["export", domain, "-"]).ok()?;
    if !out.success() {
        return None;
    }
    let value = plist::Value::from_reader_xml(std::io::Cursor::new(out.stdout.as_bytes())).ok()?;
    let dict = value.into_dictionary()?;
    Some(dict.into_iter().collect())
}

/// Snapshot all `domains`. Exporting 700+ domains serially takes ~14s, so the
/// work is split across a few scoped threads; each one only touches its own
/// chunk and the results are merged on the main thread.
fn snapshot(domains: &[String]) -> Snapshot {
    const THREADS: usize = 8;
    let chunk = domains.len().div_ceil(THREADS).max(1);
    let mut merged = Snapshot::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = domains
            .chunks(chunk)
            .map(|group| {
                scope.spawn(move || {
                    let mut local = Snapshot::new();
                    for domain in group {
                        if let Some(keys) = export_domain(domain) {
                            local.insert(domain.clone(), keys);
                        }
                    }
                    local
                })
            })
            .collect();
        for handle in handles {
            if let Ok(local) = handle.join() {
                merged.extend(local);
            }
        }
    });
    merged
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
    for (domain, keys) in after {
        let old = before.get(domain);
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
                value: value.clone(),
                previous: previous.cloned(),
            });
        }
    }
    out
}

/// The `type =` / `value =` pair for a plist value, or None when the shape is
/// one `[[defaults]]` cannot express yet (arrays, dicts, data, dates).
fn toml_value(value: &plist::Value) -> Option<(&'static str, String)> {
    match value {
        plist::Value::Boolean(b) => Some(("bool", b.to_string())),
        plist::Value::Integer(i) => Some(("int", i.to_string())),
        plist::Value::Real(f) => Some(("float", f.to_string())),
        plist::Value::String(s) => Some(("string", format!("{s:?}"))),
        _ => None,
    }
}

/// A short, readable rendering of the previous value for the `# was:` comment.
fn describe(value: &plist::Value) -> String {
    match toml_value(value) {
        Some((_, rendered)) => rendered,
        None => match value {
            plist::Value::Array(a) => format!("<array of {}>", a.len()),
            plist::Value::Dictionary(d) => format!("<dict of {}>", d.len()),
            plist::Value::Data(_) => "<data>".to_string(),
            plist::Value::Date(_) => "<date>".to_string(),
            _ => "<unsupported>".to_string(),
        },
    }
}

/// Render recorded changes as the body of a `macos/*.toml` file.
pub fn to_toml(changes: &[Change]) -> String {
    let mut out = String::from("# Recorded by `macboot macos record`.\n");
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
            Some((kind, value)) => {
                out.push_str("[[defaults]]\n");
                out.push_str(&format!("domain = {:?}\n", change.domain));
                out.push_str(&format!("key = {:?}\n", change.key));
                out.push_str(&format!("type = {kind:?}\n"));
                out.push_str(&format!("value = {value}\n"));
                if let Some(previous) = &change.previous {
                    out.push_str(&format!("# was: {}\n", describe(previous)));
                }
            }
            // Keep unsupported shapes visible rather than dropping them: the
            // user can still apply them via the [[command]] escape hatch.
            None => {
                out.push_str(&format!(
                    "# {} {} changed to {} — not expressible as [[defaults]] yet;\n\
                     # use a [[command]] with `defaults write` if you need it.\n",
                    change.domain,
                    change.key,
                    describe(&change.value)
                ));
            }
        }
        out.push('\n');
    }
    out
}

/// Run a full record session: snapshot, wait for the user, snapshot again.
pub fn record(domains: &[String], all: bool) -> Result<Vec<Change>> {
    let targets = if domains.is_empty() {
        list_domains()?
    } else {
        domains.to_vec()
    };

    ui::info(format!("Snapshotting {} domain(s)…", targets.len()));
    let before = snapshot(&targets);
    ui::detail(format!("captured {} domain(s)", before.len()));

    if !ui::pause("Change what you want in System Settings, then press Enter") {
        bail!("stdin closed before the second snapshot; nothing was recorded");
    }

    ui::info("Re-snapshotting…");
    let after = snapshot(&targets);
    Ok(changes(&before, &after, all))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, &str, plist::Value)]) -> Snapshot {
        let mut out = Snapshot::new();
        for (domain, key, value) in pairs {
            out.entry(domain.to_string())
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

    #[test]
    fn unsupported_shapes_become_comments_not_bad_toml() {
        let got = changes(
            &Snapshot::new(),
            &snap(&[(
                "com.apple.dock",
                "persistent-apps",
                plist::Value::Array(vec![plist::Value::Boolean(true)]),
            )]),
            false,
        );
        let rendered = to_toml(&got);
        assert!(rendered.contains("not expressible"), "{rendered}");
        let parsed: crate::config::MacosFile = toml::from_str(&rendered).unwrap();
        assert!(parsed.defaults.is_empty());
    }
}

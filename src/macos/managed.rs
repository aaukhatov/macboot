//! Configuration-profile ("managed") preferences.
//!
//! An MDM-delivered profile lands in `/Library/Managed Preferences`, and macOS
//! layers those values *over* the user's own domain when an app reads a
//! preference. `defaults export` (see [`super::export_domain`]) reads a single
//! plist and so cannot see them at all — which is exactly why a forced key
//! looks like permanent drift: the user domain holds one value, the profile
//! forces another, and no amount of `defaults write` changes what the app
//! actually gets.
//!
//! So a forced key is not drift, and it is not ours to fix. Reading these
//! plists is the cheap equivalent of `UserDefaults.objectIsForced(forKey:)`:
//! the files are world-readable, need no Full Disk Access, and require no
//! Objective-C bridge.
//!
//! Scope note: managed values are keyed by domain only. A profile has no ByHost
//! variant, so a forced key overrides *both* [`HostScope`]s, and lookups here
//! deliberately ignore the scope a setting declares.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where macOS materializes profile-delivered preferences.
const MANAGED_ROOT: &str = "/Library/Managed Preferences";

/// Managed preference values, read once per domain and cached for the run.
pub struct Managed {
    root: PathBuf,
    /// Short username, used for the user-scoped managed directory.
    user: Option<String>,
    cache: BTreeMap<String, Option<BTreeMap<String, plist::Value>>>,
}

impl Managed {
    /// `$MACBOOT_MANAGED_PREFS` overrides the root, the same way
    /// `$MACBOOT_STATE` overrides the state file: without it there is no way to
    /// exercise the forced-key paths short of enrolling the machine in an MDM.
    pub fn new() -> Managed {
        let root = std::env::var_os("MACBOOT_MANAGED_PREFS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(MANAGED_ROOT));
        Managed::with_root(root, std::env::var("USER").ok().filter(|s| !s.is_empty()))
    }

    /// Construct against an arbitrary root, so tests need no real profile.
    pub fn with_root(root: PathBuf, user: Option<String>) -> Managed {
        Managed {
            root,
            user,
            cache: BTreeMap::new(),
        }
    }

    /// Is any configuration profile delivering preferences to this machine?
    ///
    /// The directory only exists once a profile has been installed, so this is
    /// the cheap way to skip the whole check on an unmanaged Mac.
    pub fn is_active(&self) -> bool {
        self.root.is_dir()
    }

    /// The forced value of `key`, or None when the key is not managed.
    pub fn forced(&mut self, domain: &str, key: &str) -> Option<plist::Value> {
        if !self.cache.contains_key(domain) && !self.is_active() {
            return None;
        }
        let entry = self
            .cache
            .entry(domain.to_string())
            .or_insert_with(|| load_domain(&self.root, self.user.as_deref(), domain));
        entry.as_ref().and_then(|keys| keys.get(key).cloned())
    }

    /// Every managed key in a domain, for `macos get --managed`.
    pub fn keys(&mut self, domain: &str) -> BTreeMap<String, plist::Value> {
        if !self.cache.contains_key(domain) && !self.is_active() {
            return BTreeMap::new();
        }
        self.cache
            .entry(domain.to_string())
            .or_insert_with(|| load_domain(&self.root, self.user.as_deref(), domain))
            .clone()
            .unwrap_or_default()
    }
}

impl Default for Managed {
    fn default() -> Self {
        Managed::new()
    }
}

/// Read a domain's managed values, device-level first with the user-scoped
/// profile layered on top — the precedence CFPreferences itself applies.
fn load_domain(
    root: &Path,
    user: Option<&str>,
    domain: &str,
) -> Option<BTreeMap<String, plist::Value>> {
    let file = format!("{domain}.plist");
    let mut candidates = vec![root.join(&file)];
    if let Some(user) = user {
        candidates.push(root.join(user).join(&file));
    }

    let mut merged: Option<BTreeMap<String, plist::Value>> = None;
    for path in candidates {
        let Some(keys) = read_plist(&path) else {
            continue;
        };
        merged.get_or_insert_with(BTreeMap::new).extend(keys);
    }
    merged
}

/// A managed plist, or None if it is absent or unreadable. An unreadable file
/// is treated as "not managed": guessing would be worse than the status quo.
fn read_plist(path: &Path) -> Option<BTreeMap<String, plist::Value>> {
    let dict = plist::Value::from_file(path).ok()?.into_dictionary()?;
    Some(dict.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plist(path: &Path, pairs: &[(&str, plist::Value)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let dict: plist::Dictionary = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        plist::to_file_xml(path, &plist::Value::Dictionary(dict)).unwrap();
    }

    #[test]
    fn absent_root_is_inactive_and_forces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut managed = Managed::with_root(dir.path().join("nope"), Some("alice".into()));
        assert!(!managed.is_active());
        assert_eq!(managed.forced("com.apple.dock", "tilesize"), None);
    }

    #[test]
    fn finds_a_device_level_forced_key() {
        let dir = tempfile::tempdir().unwrap();
        write_plist(
            &dir.path().join("com.apple.dock.plist"),
            &[("tilesize", plist::Value::Integer(64.into()))],
        );
        let mut managed = Managed::with_root(dir.path().to_path_buf(), Some("alice".into()));
        assert!(managed.is_active());
        assert_eq!(
            managed.forced("com.apple.dock", "tilesize"),
            Some(plist::Value::Integer(64.into()))
        );
        // A key the profile does not mention stays unmanaged.
        assert_eq!(managed.forced("com.apple.dock", "orientation"), None);
        // As does an entirely unmanaged domain.
        assert_eq!(managed.forced("com.apple.finder", "ShowPathbar"), None);
    }

    /// A user-scoped profile is layered over the device-scoped one, so the
    /// value we report is the one the machine would actually hand an app.
    #[test]
    fn user_scope_wins_over_device_scope() {
        let dir = tempfile::tempdir().unwrap();
        write_plist(
            &dir.path().join("com.apple.dock.plist"),
            &[
                ("tilesize", plist::Value::Integer(64.into())),
                ("orientation", plist::Value::String("bottom".into())),
            ],
        );
        write_plist(
            &dir.path().join("alice/com.apple.dock.plist"),
            &[("tilesize", plist::Value::Integer(36.into()))],
        );
        let mut managed = Managed::with_root(dir.path().to_path_buf(), Some("alice".into()));
        assert_eq!(
            managed.forced("com.apple.dock", "tilesize"),
            Some(plist::Value::Integer(36.into()))
        );
        // Device-level keys the user profile omits still apply.
        assert_eq!(
            managed.forced("com.apple.dock", "orientation"),
            Some(plist::Value::String("bottom".into()))
        );
    }

    #[test]
    fn keys_lists_the_whole_managed_domain() {
        let dir = tempfile::tempdir().unwrap();
        write_plist(
            &dir.path().join("com.apple.dock.plist"),
            &[
                ("tilesize", plist::Value::Integer(64.into())),
                ("orientation", plist::Value::String("left".into())),
            ],
        );
        let mut managed = Managed::with_root(dir.path().to_path_buf(), None);
        let keys = managed.keys("com.apple.dock");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains_key("tilesize"));
        assert!(managed.keys("com.apple.finder").is_empty());
    }

    #[test]
    fn a_malformed_plist_reads_as_unmanaged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("com.apple.dock.plist"), b"not a plist").unwrap();
        let mut managed = Managed::with_root(dir.path().to_path_buf(), None);
        assert_eq!(managed.forced("com.apple.dock", "tilesize"), None);
    }
}

//! Declarative configuration: the layered manifest that describes the desired
//! machine state. Loaded from a config directory:
//!
//! ```text
//! macboot.toml     top-level: meta, profile rules, apply stages, dotfiles list
//! packages.toml    package providers (brew, cargo, npm, ...)
//! macos/*.toml     per-domain defaults, commands, keybindings
//! profiles/*.toml  overlays merged onto the base for the active profile
//! ```

pub mod packages;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use packages::Packages;

fn default_true() -> bool {
    true
}

/// The fully-resolved configuration for one machine/profile.
#[derive(Debug)]
pub struct Config {
    /// Absolute path to the config directory.
    pub root: PathBuf,
    pub meta: Meta,
    pub profile: ProfileConfig,
    pub apply: ApplyConfig,
    pub packages: Packages,
    /// Parsed `macos/*.toml` files, in filename order.
    pub macos: Vec<MacosFile>,
    /// Names of dotfile packages to link (defaults to every dir under dotfiles/).
    pub dotfiles: Vec<String>,
    /// Active profile name after resolution.
    pub active_profile: String,
    /// Template variables from the active profile.
    pub vars: BTreeMap<String, String>,
}

impl Config {
    pub fn dotfiles_dir(&self) -> PathBuf {
        self.root.join("dotfiles")
    }
}

// ---- macboot.toml ----------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct RootFile {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    profile: ProfileConfig,
    #[serde(default)]
    apply: ApplyConfig,
    #[serde(default)]
    dotfiles: DotfilesConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub default: String,
    #[serde(default, rename = "match")]
    pub rules: Vec<MatchRule>,
}

/// A single profile-selection rule. All present fields must match.
#[derive(Debug, Clone, Deserialize)]
pub struct MatchRule {
    pub profile: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplyConfig {
    #[serde(default = "default_stages")]
    pub stages: Vec<String>,
    #[serde(default)]
    pub on_error: OnError,
}

impl Default for ApplyConfig {
    fn default() -> Self {
        ApplyConfig {
            stages: default_stages(),
            on_error: OnError::default(),
        }
    }
}

fn default_stages() -> Vec<String> {
    vec![
        "packages".to_string(),
        "dotfiles".to_string(),
        "macos".to_string(),
    ]
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    #[default]
    Continue,
    Abort,
}

#[derive(Debug, Default, Deserialize)]
struct DotfilesConfig {
    #[serde(default)]
    packages: Option<Vec<String>>,
}

// ---- macos/*.toml ----------------------------------------------------------

/// One macOS settings file. Every section is optional so a single struct covers
/// defaults domains, the command escape hatch, keybindings, and app shortcuts.
#[derive(Debug, Default, Deserialize)]
pub struct MacosFile {
    #[serde(skip)]
    pub path: PathBuf,
    /// Apps to `killall` once after this file's writes.
    #[serde(default)]
    pub killall: Vec<String>,
    /// Post-apply hook, e.g. "activateSettings" for keybindings.
    #[serde(default)]
    pub apply: Option<String>,
    #[serde(default, rename = "defaults")]
    pub defaults: Vec<DefaultSetting>,
    #[serde(default, rename = "command")]
    pub commands: Vec<CommandStep>,
    #[serde(default)]
    pub hotkey: Vec<Hotkey>,
    #[serde(default)]
    pub raw: Vec<RawHotkey>,
    #[serde(default)]
    pub app_shortcut: Vec<AppShortcut>,
}

impl MacosFile {
    pub fn name(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultSetting {
    pub domain: String,
    pub key: String,
    #[serde(rename = "type")]
    pub kind: DefaultType,
    pub value: toml::Value,
    /// Write with sudo (system domains under /Library).
    #[serde(default)]
    pub sudo: bool,
    /// Which preference store the key lives in. Defaults to the normal
    /// per-user store; `"current"` selects the per-machine ByHost store.
    #[serde(default)]
    pub host: HostScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultType {
    Bool,
    Int,
    Float,
    String,
    /// A plist array. The TOML value is an array; element types are inferred.
    Array,
    /// A plist dictionary. The TOML value is a table; value types are inferred.
    Dict,
    /// A plist data blob, written as a base64 TOML string.
    Data,
    /// A plist date, written as a TOML datetime or an RFC 3339 string.
    Date,
}

/// Which of the two `defaults` preference stores a key lives in.
///
/// macOS keeps some preferences per-user (`~/Library/Preferences`) and some
/// per-user-*and*-machine (`~/Library/Preferences/ByHost`, reached with
/// `defaults -currentHost`). Control Center's menu bar layout and screensaver
/// settings are ByHost, and are simply invisible to an unscoped read — so the
/// scope has to travel with every setting.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum HostScope {
    /// `~/Library/Preferences` — the default.
    #[default]
    Any,
    /// `defaults -currentHost`, i.e. the ByHost store for this machine.
    Current,
}

impl HostScope {
    /// The flag `defaults` needs, as a leading argv element.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            HostScope::Any => &[],
            HostScope::Current => &["-currentHost"],
        }
    }

    /// Short suffix used in labels and state keys. Empty for the default scope
    /// so existing state files keep matching.
    pub fn suffix(self) -> &'static str {
        match self {
            HostScope::Any => "",
            HostScope::Current => " @currentHost",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandStep {
    #[serde(default)]
    pub desc: Option<String>,
    pub run: Vec<String>,
    #[serde(default)]
    pub sudo: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hotkey {
    pub action: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub chord: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawHotkey {
    pub id: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub keycode: Option<u32>,
    #[serde(default)]
    pub mods: Vec<String>,
    /// Fully literal `[ascii, keycode, modifierMask]`, overrides key/keycode/mods.
    #[serde(default)]
    pub parameters: Option<[i64; 3]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppShortcut {
    pub bundle: String,
    pub menu_title: String,
    pub chord: String,
}

// ---- profiles/*.toml -------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ProfileOverlay {
    #[serde(default)]
    packages: Option<Packages>,
    #[serde(default)]
    dotfiles: Vec<String>,
    #[serde(default)]
    vars: BTreeMap<String, String>,
}

// ---- loading ---------------------------------------------------------------

impl Config {
    /// Load and resolve configuration from `root`, selecting `profile_override`
    /// or applying the manifest's match rules.
    pub fn load(root: &Path, profile_override: Option<&str>) -> Result<Config> {
        if !root.is_dir() {
            bail!("config directory not found: {}", root.display());
        }
        // Canonicalize so managed symlinks point at absolute, stable paths.
        let root = &std::fs::canonicalize(root)
            .with_context(|| format!("resolving config dir {}", root.display()))?;

        let root_file = read_toml::<RootFile>(&root.join("macboot.toml"))?.unwrap_or_default();

        let mut packages = read_toml::<Packages>(&root.join("packages.toml"))?.unwrap_or_default();

        let macos = load_macos_dir(&root.join("macos"))?;

        // Dotfiles packages: explicit list, else every directory under dotfiles/.
        let mut dotfiles = match &root_file.dotfiles.packages {
            Some(list) => list.clone(),
            None => discover_dotfile_packages(&root.join("dotfiles"))?,
        };

        // Resolve the active profile and merge its overlay.
        let (active_profile, reason) =
            crate::profile::resolve(&root_file.profile, profile_override);
        let mut vars = BTreeMap::new();
        let overlay_path = root.join("profiles").join(format!("{active_profile}.toml"));
        if let Some(overlay) = read_toml::<ProfileOverlay>(&overlay_path)? {
            if let Some(p) = overlay.packages {
                packages.merge(p);
            }
            for d in overlay.dotfiles {
                if !dotfiles.contains(&d) {
                    dotfiles.push(d);
                }
            }
            vars = overlay.vars;
        }
        let _ = reason; // available to callers via `profile show`

        Ok(Config {
            root: root.to_path_buf(),
            meta: root_file.meta,
            profile: root_file.profile,
            apply: root_file.apply,
            packages,
            macos,
            dotfiles,
            active_profile,
            vars,
        })
    }

    /// Validate the resolved config, returning a list of human-readable problems.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        // A dotfile package must exist as a directory.
        for pkg in &self.dotfiles {
            if !self.dotfiles_dir().join(pkg).is_dir() {
                problems.push(format!(
                    "dotfiles package '{pkg}' has no directory under {}",
                    crate::util::tildify(&self.dotfiles_dir())
                ));
            }
        }

        // The same binary name declared under two providers is a conflict.
        problems.extend(self.packages.conflicts());

        problems
    }
}

/// Read and parse a TOML file, returning None if it does not exist.
fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value =
        toml::from_str(&text).with_context(|| format!("parsing TOML in {}", path.display()))?;
    Ok(Some(value))
}

fn load_macos_dir(dir: &Path) -> Result<Vec<MacosFile>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "toml").unwrap_or(false))
        .collect();
    entries.sort();
    for path in entries {
        let mut file: MacosFile = read_toml(&path)?.unwrap_or_default();
        file.path = path;
        files.push(file);
    }
    Ok(files)
}

fn discover_dotfile_packages(dir: &Path) -> Result<Vec<String>> {
    let mut pkgs = Vec::new();
    if !dir.is_dir() {
        return Ok(pkgs);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            pkgs.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    pkgs.sort();
    Ok(pkgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_error_defaults_to_continue() {
        assert_eq!(OnError::default(), OnError::Continue);
    }

    #[test]
    fn missing_config_dir_errors() {
        let res = Config::load(Path::new("/no/such/dir/xyz"), None);
        assert!(res.is_err());
    }
}

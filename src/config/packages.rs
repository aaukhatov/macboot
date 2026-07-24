//! Package-provider configuration (`packages.toml`). Each provider gets its own
//! table; only declared providers are ever touched. A single `ListSpec` covers
//! every "flat list of package names" provider via serde field aliases, while
//! Homebrew and custom providers use dedicated shapes.

use serde::Deserialize;
use std::collections::BTreeMap;

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
pub struct Packages {
    pub brew: Option<BrewSpec>,
    pub cargo: Option<ListSpec>,
    pub go: Option<ListSpec>,
    pub npm: Option<ListSpec>,
    pub pipx: Option<ListSpec>,
    pub mise: Option<ListSpec>,
    pub macports: Option<ListSpec>,
    pub nix: Option<ListSpec>,
    /// `[providers.custom.<name>]` definitions (command templates + packages).
    #[serde(default)]
    pub providers: Providers,
}

#[derive(Debug, Default, Deserialize)]
pub struct Providers {
    #[serde(default)]
    pub custom: BTreeMap<String, CustomProvider>,
}

/// Homebrew: taps + formulae + casks + Mac App Store + VS Code, applied via
/// `brew bundle`.
#[derive(Debug, Default, Deserialize)]
pub struct BrewSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub taps: Vec<String>,
    #[serde(default)]
    pub brews: Vec<String>,
    #[serde(default)]
    pub casks: Vec<String>,
    #[serde(default)]
    pub mas: Vec<MasApp>,
    #[serde(default)]
    pub vscode: Vec<String>,
    /// Optional categorization; informational, does not affect apply.
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MasApp {
    pub name: String,
    pub id: u64,
}

/// A flat-list provider. `items` accepts whatever key reads naturally per tool
/// (crates/packages/globals/apps/tools/ports) via serde aliases.
#[derive(Debug, Default, Deserialize)]
pub struct ListSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(
        default,
        alias = "crates",
        alias = "packages",
        alias = "globals",
        alias = "apps",
        alias = "tools",
        alias = "ports"
    )]
    pub items: Vec<String>,
}

/// A user-defined provider driven entirely by command templates. `{pkg}` in a
/// template is substituted per package.
#[derive(Debug, Default, Deserialize)]
pub struct CustomProvider {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub required: bool,
    pub list: Vec<String>,
    pub install: Vec<String>,
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    #[serde(default)]
    pub packages: Vec<String>,
}

impl Packages {
    /// Append another `Packages` (a profile overlay) onto this one, unioning the
    /// package lists so overlays are additive.
    pub fn merge(&mut self, other: Packages) {
        merge_list(&mut self.cargo, other.cargo);
        merge_list(&mut self.go, other.go);
        merge_list(&mut self.npm, other.npm);
        merge_list(&mut self.pipx, other.pipx);
        merge_list(&mut self.mise, other.mise);
        merge_list(&mut self.macports, other.macports);
        merge_list(&mut self.nix, other.nix);

        match (&mut self.brew, other.brew) {
            (Some(base), Some(add)) => base.merge(add),
            (slot @ None, Some(add)) => *slot = Some(add),
            _ => {}
        }

        for (name, cp) in other.providers.custom {
            self.providers
                .custom
                .entry(name)
                .and_modify(|existing| union(&mut existing.packages, &cp.packages))
                .or_insert(cp);
        }
    }

    /// Detect a package name declared under more than one provider.
    pub fn conflicts(&self) -> Vec<String> {
        let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut record = |name: &str, provider: &str| {
            seen.entry(name.to_string())
                .or_default()
                .push(provider.to_string());
        };
        if let Some(b) = &self.brew {
            for x in b.brews.iter().chain(&b.casks) {
                record(x, "brew");
            }
        }
        for (prov, spec) in self.list_providers() {
            for x in &spec.items {
                record(x, prov);
            }
        }
        seen.into_iter()
            .filter(|(_, provs)| provs.len() > 1)
            .map(|(name, provs)| format!("package '{name}' declared under: {}", provs.join(", ")))
            .collect()
    }

    /// The flat-list providers that are present, paired with their name.
    pub fn list_providers(&self) -> Vec<(&'static str, &ListSpec)> {
        let pairs: [(&'static str, &Option<ListSpec>); 7] = [
            ("cargo", &self.cargo),
            ("go", &self.go),
            ("npm", &self.npm),
            ("pipx", &self.pipx),
            ("mise", &self.mise),
            ("macports", &self.macports),
            ("nix", &self.nix),
        ];
        pairs
            .into_iter()
            .filter_map(|(name, slot)| slot.as_ref().map(|spec| (name, spec)))
            .collect()
    }
}

impl BrewSpec {
    fn merge(&mut self, other: BrewSpec) {
        union(&mut self.taps, &other.taps);
        union(&mut self.brews, &other.brews);
        union(&mut self.casks, &other.casks);
        union(&mut self.vscode, &other.vscode);
        for app in other.mas {
            if !self.mas.iter().any(|a| a.id == app.id) {
                self.mas.push(app);
            }
        }
        for (k, v) in other.groups {
            self.groups.entry(k).or_default().extend(v);
        }
    }
}

fn merge_list(slot: &mut Option<ListSpec>, other: Option<ListSpec>) {
    if let Some(add) = other {
        match slot {
            Some(base) => union(&mut base.items, &add.items),
            None => *slot = Some(add),
        }
    }
}

/// Append items from `add` not already present in `base` (order-preserving).
fn union(base: &mut Vec<String>, add: &[String]) {
    for item in add {
        if !base.contains(item) {
            base.push(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> ListSpec {
        ListSpec {
            enabled: true,
            required: true,
            items: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn overlay_unions_lists_without_duplicates() {
        let mut base = Packages {
            cargo: Some(list(&["bottom", "tokei"])),
            ..Default::default()
        };
        let overlay = Packages {
            cargo: Some(list(&["tokei", "just"])),
            ..Default::default()
        };
        base.merge(overlay);
        assert_eq!(
            base.cargo.unwrap().items,
            vec!["bottom", "tokei", "just"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn same_package_in_two_providers_is_a_conflict() {
        let pkgs = Packages {
            brew: Some(BrewSpec {
                brews: vec!["bottom".into()],
                ..Default::default()
            }),
            cargo: Some(list(&["bottom"])),
            ..Default::default()
        };
        let conflicts = pkgs.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("bottom"));
    }

    #[test]
    fn list_alias_parses_crates_key() {
        let spec: ListSpec = toml::from_str(r#"crates = ["a", "b"]"#).unwrap();
        assert_eq!(spec.items, vec!["a".to_string(), "b".to_string()]);
        assert!(spec.enabled && spec.required);
    }
}

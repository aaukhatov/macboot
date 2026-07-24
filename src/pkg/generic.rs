//! A `Provider` implemented purely from command templates — covers cargo, npm,
//! pipx, go, mise, macports, nix, and any user-defined `[providers.custom.*]`.

use super::{Provider, Remediation};
use crate::config::packages::CustomProvider;
use crate::proc;
use crate::ui;
use anyhow::Result;

/// How to turn `installed`-command stdout into a list of package identifiers.
#[derive(Debug, Clone, Copy)]
pub enum ListParse {
    /// One package per line; take the first whitespace-separated column.
    FirstColumn,
    /// cargo's `install --list`: lines like `ripgrep v14.0.0:`; take word 1 of
    /// non-indented lines.
    CargoList,
    /// npm's `--parseable` output: absolute paths like
    /// `/opt/homebrew/lib/node_modules/pnpm`. The package name is everything
    /// after the final `node_modules/`, so scoped names survive intact.
    NpmParseable,
    /// Detection unsupported — provider is apply-only.
    None,
}

/// The file `pkg dump` writes for a provider, in that manager's own format.
#[derive(Debug, Clone, Copy)]
pub enum DumpFormat {
    /// One package per line. The natural artifact for managers with no manifest
    /// format of their own — `xargs cargo install < crates.txt`.
    Lines,
    /// npm's `package.json`, with the installed globals as `dependencies`.
    PackageJson,
    /// mise's `mise.toml`, with each installed tool pinned to its version.
    MiseToml,
}

pub struct CmdProvider {
    name: String,
    cli: String,
    required: bool,
    remediation: Remediation,
    desired: Vec<String>,
    list_args: Vec<String>,
    parse: ListParse,
    install_tpl: Vec<String>,
    remove_tpl: Option<Vec<String>>,
    dump_file: String,
    dump_format: DumpFormat,
}

impl CmdProvider {
    fn subst(tpl: &[String], pkg: &str) -> Vec<String> {
        tpl.iter().map(|a| a.replace("{pkg}", pkg)).collect()
    }

    /// Raw stdout of the provider's "what is installed" command.
    fn list_output(&self) -> Result<String> {
        let args: Vec<&str> = self.list_args.iter().map(String::as_str).collect();
        proc::output(&self.cli, &args)
    }
}

impl Provider for CmdProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn cli(&self) -> &str {
        &self.cli
    }
    fn required(&self) -> bool {
        self.required
    }
    fn remediation(&self) -> Remediation {
        self.remediation.clone()
    }
    fn desired(&self) -> Vec<String> {
        self.desired.clone()
    }
    fn supports_diff(&self) -> bool {
        !matches!(self.parse, ListParse::None)
    }

    fn installed(&self) -> Result<Vec<String>> {
        if matches!(self.parse, ListParse::None) {
            return Ok(Vec::new());
        }
        let out = self.list_output()?;
        Ok(parse_list(&out, self.parse))
    }

    fn dump_file(&self) -> &str {
        &self.dump_file
    }

    fn dump_native(&self) -> Result<String> {
        match self.dump_format {
            DumpFormat::Lines => {
                let mut installed = self.installed()?;
                installed.sort();
                Ok(lines_file(&installed))
            }
            DumpFormat::PackageJson => {
                let mut installed = self.installed()?;
                installed.sort();
                Ok(package_json(&installed))
            }
            // Versions live in the second column of the listing, which the
            // name-only `installed()` throws away.
            DumpFormat::MiseToml => Ok(mise_toml(&name_version_pairs(&self.list_output()?))),
        }
    }

    fn install(&self, missing: &[String], dry: bool) -> Result<()> {
        for pkg in missing {
            let args = Self::subst(&self.install_tpl, pkg);
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            ui::detail(format!("{} {}", self.cli, argv.join(" ")));
            if dry {
                continue;
            }
            proc::run(&self.cli, &argv)?;
        }
        Ok(())
    }

    fn remove(&self, extraneous: &[String], dry: bool) -> Result<()> {
        let Some(tpl) = &self.remove_tpl else {
            ui::warn(format!("{}: no remove command configured", self.name));
            return Ok(());
        };
        for pkg in extraneous {
            let args = Self::subst(tpl, pkg);
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            ui::detail(format!("{} {}", self.cli, argv.join(" ")));
            if dry {
                continue;
            }
            proc::run(&self.cli, &argv)?;
        }
        Ok(())
    }
}

fn parse_list(out: &str, parse: ListParse) -> Vec<String> {
    match parse {
        ListParse::None => Vec::new(),
        ListParse::FirstColumn => out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.trim_end_matches(':').to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        ListParse::CargoList => out
            .lines()
            // cargo indents the binaries under each crate; crate lines are flush-left.
            .filter(|l| !l.starts_with(char::is_whitespace) && !l.trim().is_empty())
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.trim_end_matches(':').to_string())
            .collect(),
        ListParse::NpmParseable => out
            .lines()
            .map(str::trim)
            .filter_map(|l| l.rsplit_once("node_modules/"))
            // npm itself is bundled with node, never a declared global.
            .filter(|(_, name)| !name.is_empty() && *name != "npm")
            .map(|(_, name)| name.to_string())
            .collect(),
    }
}

/// `name  version  …` rows (mise's listing), reduced to the first two columns.
fn name_version_pairs(out: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = out
        .lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let name = cols.next()?;
            Some((
                name.trim_end_matches(':').to_string(),
                cols.next().unwrap_or("latest").to_string(),
            ))
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    rows.sort();
    rows.dedup();
    rows
}

/// A bare one-per-line package list. No header comment: these files are meant
/// to be piped straight into `xargs`, which does not understand comments.
pub fn lines_file(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    items.join("\n") + "\n"
}

/// npm's native manifest. Globals carry no version in the listing, so each one
/// is pinned to the `latest` dist-tag.
fn package_json(items: &[String]) -> String {
    let deps: serde_json::Map<String, serde_json::Value> = items
        .iter()
        .map(|i| (i.clone(), serde_json::Value::from("latest")))
        .collect();
    let doc = serde_json::json!({
        "name": "macboot-npm-globals",
        "private": true,
        "description": "Generated by `macboot pkg dump`: global npm packages.",
        "dependencies": deps,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// mise's native config file.
fn mise_toml(rows: &[(String, String)]) -> String {
    let mut out = String::from("# Generated by `macboot pkg dump`.\n[tools]\n");
    for (name, version) in rows {
        out.push_str(&format!("{name:?} = {version:?}\n"));
    }
    out
}

fn s(v: &str) -> String {
    v.to_string()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|x| x.to_string()).collect()
}

/// The manifest key a provider's package list reads from (for `dump`).
pub fn items_key(name: &str) -> &'static str {
    match name {
        "cargo" => "crates",
        "npm" => "globals",
        "pipx" => "apps",
        "mise" => "tools",
        "macports" => "ports",
        _ => "packages",
    }
}

/// Construct a built-in flat-list provider by name.
pub fn builtin(name: &str, items: &[String], required: bool) -> Box<dyn Provider> {
    let desired = items.to_vec();
    let p = match name {
        "cargo" => CmdProvider {
            name: s("cargo"),
            cli: s("cargo"),
            required,
            remediation: Remediation {
                install: s("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"),
                url: s("https://rustup.rs"),
            },
            desired,
            list_args: args(&["install", "--list"]),
            parse: ListParse::CargoList,
            install_tpl: args(&["install", "{pkg}"]),
            remove_tpl: Some(args(&["uninstall", "{pkg}"])),
            dump_file: s("crates.txt"),
            dump_format: DumpFormat::Lines,
        },
        "npm" => CmdProvider {
            name: s("npm"),
            cli: s("npm"),
            required,
            remediation: Remediation {
                install: s("brew install node"),
                url: s("https://nodejs.org"),
            },
            desired,
            list_args: args(&["ls", "-g", "--depth=0", "--parseable"]),
            parse: ListParse::NpmParseable,
            install_tpl: args(&["install", "-g", "{pkg}"]),
            remove_tpl: Some(args(&["uninstall", "-g", "{pkg}"])),
            dump_file: s("package.json"),
            dump_format: DumpFormat::PackageJson,
        },
        "pipx" => CmdProvider {
            name: s("pipx"),
            cli: s("pipx"),
            required,
            remediation: Remediation {
                install: s("brew install pipx"),
                url: s("https://pipx.pypa.io"),
            },
            desired,
            list_args: args(&["list", "--short"]),
            parse: ListParse::FirstColumn,
            install_tpl: args(&["install", "{pkg}"]),
            remove_tpl: Some(args(&["uninstall", "{pkg}"])),
            dump_file: s("requirements.txt"),
            dump_format: DumpFormat::Lines,
        },
        "go" => CmdProvider {
            name: s("go"),
            cli: s("go"),
            required,
            remediation: Remediation {
                install: s("brew install go"),
                url: s("https://go.dev/dl"),
            },
            desired,
            // `go install` keeps no manifest; treat as apply-only.
            list_args: vec![],
            parse: ListParse::None,
            install_tpl: args(&["install", "{pkg}"]),
            remove_tpl: None,
            dump_file: s("packages.txt"),
            dump_format: DumpFormat::Lines,
        },
        "mise" => CmdProvider {
            name: s("mise"),
            cli: s("mise"),
            required,
            remediation: Remediation {
                install: s("brew install mise"),
                url: s("https://mise.jdx.dev"),
            },
            desired,
            list_args: args(&["ls", "--installed", "--no-header"]),
            parse: ListParse::FirstColumn,
            install_tpl: args(&["use", "-g", "{pkg}"]),
            remove_tpl: Some(args(&["uninstall", "{pkg}"])),
            dump_file: s("mise.toml"),
            dump_format: DumpFormat::MiseToml,
        },
        "macports" => CmdProvider {
            name: s("macports"),
            cli: s("port"),
            required,
            remediation: Remediation {
                install: s("see MacPorts installer pkg"),
                url: s("https://www.macports.org/install.php"),
            },
            desired,
            list_args: args(&["installed", "requested"]),
            parse: ListParse::FirstColumn,
            install_tpl: args(&["install", "{pkg}"]),
            remove_tpl: Some(args(&["uninstall", "{pkg}"])),
            dump_file: s("ports.txt"),
            dump_format: DumpFormat::Lines,
        },
        _ => CmdProvider {
            // "nix" and any unrecognized built-in fall through here (apply-only).
            name: s(name),
            cli: s("nix"),
            required,
            remediation: Remediation {
                install: s("sh <(curl -L https://nixos.org/nix/install)"),
                url: s("https://nixos.org/download"),
            },
            desired,
            list_args: vec![],
            parse: ListParse::None,
            install_tpl: args(&["profile", "install", "{pkg}"]),
            remove_tpl: None,
            dump_file: s("packages.txt"),
            dump_format: DumpFormat::Lines,
        },
    };
    Box::new(p)
}

/// Construct a provider from a user-defined `[providers.custom.<name>]` block.
pub fn from_custom(name: &str, cp: &CustomProvider) -> Box<dyn Provider> {
    let cli = cp
        .install
        .first()
        .cloned()
        .unwrap_or_else(|| name.to_string());
    Box::new(CmdProvider {
        name: name.to_string(),
        cli,
        required: cp.required,
        remediation: Remediation {
            install: format!("install `{}` yourself", name),
            url: String::new(),
        },
        desired: cp.packages.clone(),
        // Drop the leading binary from `list`; it becomes the argv.
        list_args: cp.list.iter().skip(1).cloned().collect(),
        parse: if cp.list.is_empty() {
            ListParse::None
        } else {
            ListParse::FirstColumn
        },
        install_tpl: cp.install.iter().skip(1).cloned().collect(),
        remove_tpl: cp
            .remove
            .as_ref()
            .map(|r| r.iter().skip(1).cloned().collect()),
        dump_file: s("packages.txt"),
        dump_format: DumpFormat::Lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_list_parses_crate_names() {
        let out = "ripgrep v14.1.0:\n    rg\ntokei v12.1.2:\n    tokei\n";
        let got = parse_list(out, ListParse::CargoList);
        assert_eq!(got, vec!["ripgrep".to_string(), "tokei".to_string()]);
    }

    #[test]
    fn npm_parseable_yields_names_not_paths() {
        let out = "/opt/homebrew/lib\n/opt/homebrew/lib/node_modules/npm\n\
                   /opt/homebrew/lib/node_modules/pnpm\n\
                   /opt/homebrew/lib/node_modules/@angular/cli\n";
        let got = parse_list(out, ListParse::NpmParseable);
        assert_eq!(got, vec!["pnpm".to_string(), "@angular/cli".to_string()]);
    }

    #[test]
    fn lines_file_is_comment_free_and_xargs_ready() {
        let got = lines_file(&[s("ripgrep"), s("tokei")]);
        assert_eq!(got, "ripgrep\ntokei\n");
        assert!(lines_file(&[]).is_empty());
    }

    #[test]
    fn package_json_is_valid_json_with_scoped_names() {
        let got = package_json(&[s("@angular/cli"), s("pnpm")]);
        let parsed: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(parsed["dependencies"]["@angular/cli"], "latest");
        assert_eq!(parsed["dependencies"]["pnpm"], "latest");
    }

    #[test]
    fn mise_toml_pins_installed_versions_and_parses_back() {
        let rows = name_version_pairs("node  20.11.0  ~/.config/mise/config.toml\npython 3.12.2\n");
        assert_eq!(rows[0], (s("node"), s("20.11.0")));
        let rendered = mise_toml(&rows);
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed["tools"]["python"].as_str(), Some("3.12.2"));
    }

    #[test]
    fn first_column_parser_trims_colons() {
        let out = "ruff 0.4.0\nhttpie 3.2\n";
        let got = parse_list(out, ListParse::FirstColumn);
        assert_eq!(got, vec!["ruff".to_string(), "httpie".to_string()]);
    }
}

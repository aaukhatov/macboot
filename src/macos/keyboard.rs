//! macOS system keybindings (`com.apple.symbolichotkeys`).
//!
//! Apple stores these as an integer-keyed plist where each entry is `enabled`
//! plus `parameters = [asciiChar(65535=none), virtualKeyCode, modifierMask]`.
//! This module translates between that opaque form and a friendly
//! `action`/`chord` representation, in both directions:
//!   - `dump` reads the live domain and reverse-maps it into `macos/keyboard.toml`
//!   - `apply` builds the plist and imports it, then runs `activateSettings -u`.

use crate::config::{Hotkey, RawHotkey};
use crate::proc;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;

const DOMAIN: &str = "com.apple.symbolichotkeys";

// Modifier bit masks used by symbolichotkeys.
const MOD_CMD: i64 = 0x100000; // 1048576
const MOD_SHIFT: i64 = 0x20000; // 131072
const MOD_CTRL: i64 = 0x40000; // 262144
const MOD_OPT: i64 = 0x80000; // 524288
const NO_ASCII: i64 = 65535;

/// Action-ID ↔ friendly name. Common system hotkeys; unknown IDs round-trip as
/// `[[raw]]` entries so nothing is lost.
const ACTIONS: &[(u32, &str)] = &[
    (7, "move-focus-menu-bar"),
    (8, "move-focus-dock"),
    (9, "move-focus-windows"),
    (10, "move-focus-window-toolbar"),
    (11, "move-focus-floating-window"),
    (12, "toggle-zoom"),
    (13, "zoom-in"),
    (15, "invert-colors"),
    (17, "increase-contrast"),
    (27, "screenshot-options"),
    (28, "screenshot-to-file"),
    (29, "screenshot-to-clipboard"),
    (30, "screenshot-area-to-file"),
    (31, "screenshot-area-to-clipboard"),
    (32, "mission-control"),
    (33, "mission-control-dashboard"),
    (34, "mission-control-app-windows"),
    (36, "application-windows"),
    (37, "show-desktop"),
    (52, "toggle-dock-hiding"),
    (57, "focus-status-menus"),
    (60, "select-previous-input-source"),
    (61, "select-next-input-source"),
    (64, "spotlight"),
    (65, "spotlight-finder-search"),
    (79, "move-left-a-space"),
    (81, "move-right-a-space"),
    (118, "switch-to-desktop-1"),
    (163, "notification-center"),
    (175, "quick-note"),
    (184, "custom-184"),
    (190, "launchpad"),
];

/// Key name ↔ virtual keycode. Printable keys also carry their ASCII code.
const KEYCODES: &[(&str, u32)] = &[
    ("a", 0),
    ("s", 1),
    ("d", 2),
    ("f", 3),
    ("h", 4),
    ("g", 5),
    ("z", 6),
    ("x", 7),
    ("c", 8),
    ("v", 9),
    ("b", 11),
    ("q", 12),
    ("w", 13),
    ("e", 14),
    ("r", 15),
    ("y", 16),
    ("t", 17),
    ("1", 18),
    ("2", 19),
    ("3", 20),
    ("4", 21),
    ("6", 22),
    ("5", 23),
    ("=", 24),
    ("9", 25),
    ("7", 26),
    ("-", 27),
    ("8", 28),
    ("0", 29),
    ("]", 30),
    ("o", 31),
    ("u", 32),
    ("[", 33),
    ("i", 34),
    ("p", 35),
    ("l", 37),
    ("j", 38),
    ("'", 39),
    ("k", 40),
    (";", 41),
    ("\\", 42),
    (",", 43),
    ("/", 44),
    ("n", 45),
    ("m", 46),
    (".", 47),
    ("`", 50),
    // Named non-printable keys (ASCII = 65535).
    ("return", 36),
    ("tab", 48),
    ("space", 49),
    ("delete", 51),
    ("escape", 53),
    ("left", 123),
    ("right", 124),
    ("down", 125),
    ("up", 126),
    ("f1", 122),
    ("f2", 120),
    ("f3", 99),
    ("f4", 118),
    ("f5", 96),
    ("f6", 97),
    ("f7", 98),
    ("f8", 100),
    ("f9", 101),
    ("f10", 109),
    ("f11", 103),
    ("f12", 111),
];

fn action_name(id: u32) -> Option<&'static str> {
    ACTIONS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

fn action_id(name: &str) -> Option<u32> {
    ACTIONS.iter().find(|(_, n)| *n == name).map(|(i, _)| *i)
}

fn keycode_of(name: &str) -> Option<u32> {
    let name = name.to_lowercase();
    KEYCODES.iter().find(|(k, _)| *k == name).map(|(_, c)| *c)
}

fn name_of_keycode(code: u32) -> Option<&'static str> {
    KEYCODES.iter().find(|(_, c)| *c == code).map(|(k, _)| *k)
}

/// Is this key one of the named non-printable keys (ASCII should be 65535)?
fn is_named_key(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "return" | "tab" | "space" | "delete" | "escape" | "left" | "right" | "down" | "up"
    ) || name.to_lowercase().starts_with('f')
        && name.len() >= 2
        && name[1..].chars().all(|c| c.is_ascii_digit())
}

/// Parse a chord like "cmd+shift+3" into `[ascii, keycode, modifierMask]`.
pub fn parse_chord(chord: &str) -> Result<[i64; 3]> {
    let mut mask = 0i64;
    let mut key: Option<&str> = None;
    for part in chord.split('+') {
        let p = part.trim();
        match p.to_lowercase().as_str() {
            "cmd" | "command" | "⌘" => mask |= MOD_CMD,
            "shift" | "⇧" => mask |= MOD_SHIFT,
            "ctrl" | "control" | "⌃" => mask |= MOD_CTRL,
            "opt" | "option" | "alt" | "⌥" => mask |= MOD_OPT,
            _ => {
                if key.is_some() {
                    bail!("chord '{chord}' has more than one non-modifier key");
                }
                key = Some(p);
            }
        }
    }
    let key = key.ok_or_else(|| anyhow!("chord '{chord}' has no key"))?;
    let keycode =
        keycode_of(key).ok_or_else(|| anyhow!("unknown key '{key}' in chord '{chord}'"))? as i64;
    let ascii = if is_named_key(key) {
        NO_ASCII
    } else if key.len() == 1 {
        key.chars().next().unwrap() as i64
    } else {
        NO_ASCII
    };
    Ok([ascii, keycode, mask])
}

/// Render `[ascii, keycode, modifierMask]` back into a chord string.
pub fn format_chord(params: [i64; 3]) -> String {
    let [ascii, keycode, mask] = params;
    let mut parts = Vec::new();
    if mask & MOD_CTRL != 0 {
        parts.push("ctrl".to_string());
    }
    if mask & MOD_OPT != 0 {
        parts.push("opt".to_string());
    }
    if mask & MOD_SHIFT != 0 {
        parts.push("shift".to_string());
    }
    if mask & MOD_CMD != 0 {
        parts.push("cmd".to_string());
    }
    let key = if ascii != NO_ASCII && ascii > 0 {
        char::from_u32(ascii as u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| format!("kc{keycode}"))
    } else {
        name_of_keycode(keycode as u32)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("kc{keycode}"))
    };
    parts.push(key);
    parts.join("+")
}

/// Convert a chord's modifier list to the `@$^~` glyph string used by
/// `NSUserKeyEquivalents` for per-app menu shortcuts.
pub fn chord_to_glyphs(chord: &str) -> Result<String> {
    let mut out = String::new();
    let mut key = None;
    for part in chord.split('+') {
        match part.trim().to_lowercase().as_str() {
            "cmd" | "command" => out.push('@'),
            "shift" => out.push('$'),
            "ctrl" | "control" => out.push('^'),
            "opt" | "option" | "alt" => out.push('~'),
            k => key = Some(k.to_string()),
        }
    }
    out.push_str(&key.ok_or_else(|| anyhow!("chord '{chord}' has no key"))?);
    Ok(out)
}

/// One resolved hotkey ready to serialize into the symbolichotkeys plist.
struct Entry {
    id: u32,
    enabled: bool,
    params: Option<[i64; 3]>,
}

fn resolve_entries(hotkeys: &[Hotkey], raws: &[RawHotkey]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for h in hotkeys {
        let id =
            action_id(&h.action).ok_or_else(|| anyhow!("unknown hotkey action '{}'", h.action))?;
        let params = match &h.chord {
            Some(c) => Some(parse_chord(c)?),
            None => None,
        };
        entries.push(Entry {
            id,
            enabled: h.enabled,
            params,
        });
    }
    for r in raws {
        let params = if let Some(p) = r.parameters {
            Some(p)
        } else if let (Some(kc), key) = (r.keycode, &r.key) {
            let mut mask = 0i64;
            for m in &r.mods {
                mask |= match m.to_lowercase().as_str() {
                    "cmd" | "command" => MOD_CMD,
                    "shift" => MOD_SHIFT,
                    "ctrl" | "control" => MOD_CTRL,
                    "opt" | "option" | "alt" => MOD_OPT,
                    other => bail!("unknown modifier '{other}' in raw hotkey {}", r.id),
                };
            }
            let ascii = key
                .as_ref()
                .filter(|k| k.len() == 1)
                .map(|k| k.chars().next().unwrap() as i64)
                .unwrap_or(NO_ASCII);
            Some([ascii, kc as i64, mask])
        } else {
            None
        };
        entries.push(Entry {
            id: r.id,
            enabled: r.enabled,
            params,
        });
    }
    Ok(entries)
}

/// Build the `AppleSymbolicHotKeys` plist value from resolved entries.
fn build_plist(entries: &[Entry]) -> plist::Value {
    use plist::{Dictionary, Value};
    let mut hotkeys = Dictionary::new();
    for e in entries {
        let mut entry = Dictionary::new();
        entry.insert("enabled".into(), Value::Boolean(e.enabled));
        if let Some(p) = e.params {
            let mut value = Dictionary::new();
            let params: Vec<Value> = p.iter().map(|n| Value::Integer((*n).into())).collect();
            value.insert("parameters".into(), Value::Array(params));
            value.insert("type".into(), Value::String("standard".into()));
            entry.insert("value".into(), Value::Dictionary(value));
        }
        hotkeys.insert(e.id.to_string(), Value::Dictionary(entry));
    }
    let mut root = Dictionary::new();
    root.insert("AppleSymbolicHotKeys".into(), Value::Dictionary(hotkeys));
    Value::Dictionary(root)
}

/// Apply keybindings: import the plist, then reload settings.
pub fn apply(hotkeys: &[Hotkey], raws: &[RawHotkey], dry: bool) -> Result<usize> {
    let entries = resolve_entries(hotkeys, raws)?;
    if entries.is_empty() {
        return Ok(0);
    }
    if dry {
        return Ok(entries.len());
    }
    let plist = build_plist(&entries);
    let path = std::env::temp_dir().join(format!(
        "macboot-symbolichotkeys-{}.plist",
        std::process::id()
    ));
    plist
        .to_file_xml(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    let path_str = path.to_string_lossy().into_owned();
    let res = proc::run("defaults", &["import", DOMAIN, &path_str]);
    let _ = std::fs::remove_file(&path);
    res?;
    // Reload so changes take effect without logout.
    let _ = proc::run(
        "/System/Library/PrivateFrameworks/SystemAdministration.framework/Resources/activateSettings",
        &["-u"],
    );
    Ok(entries.len())
}

/// A single hotkey read back from the live domain.
pub struct DumpedHotkey {
    pub id: u32,
    pub action: Option<&'static str>,
    pub enabled: bool,
    pub params: Option<[i64; 3]>,
}

/// Read the live `symbolichotkeys` domain and reverse-map it.
pub fn dump() -> Result<Vec<DumpedHotkey>> {
    let path = std::env::temp_dir().join(format!("macboot-export-{}.plist", std::process::id()));
    let path_str = path.to_string_lossy().into_owned();
    proc::run("defaults", &["export", DOMAIN, &path_str])
        .context("exporting symbolichotkeys domain")?;
    let value = plist::Value::from_file(&path).context("parsing exported plist")?;
    let _ = std::fs::remove_file(&path);

    let root = value
        .as_dictionary()
        .and_then(|d| d.get("AppleSymbolicHotKeys"))
        .and_then(|v| v.as_dictionary())
        .ok_or_else(|| anyhow!("no AppleSymbolicHotKeys in domain"))?;

    let mut out = Vec::new();
    // Sort numerically by id for stable output.
    let mut sorted: BTreeMap<u32, &plist::Value> = BTreeMap::new();
    for (k, v) in root {
        if let Ok(id) = k.parse::<u32>() {
            sorted.insert(id, v);
        }
    }
    for (id, v) in sorted {
        let dict = match v.as_dictionary() {
            Some(d) => d,
            None => continue,
        };
        let enabled = dict
            .get("enabled")
            .and_then(|b| b.as_boolean())
            .unwrap_or(false);
        let params = dict
            .get("value")
            .and_then(|v| v.as_dictionary())
            .and_then(|d| d.get("parameters"))
            .and_then(|p| p.as_array())
            .and_then(|arr| {
                let nums: Vec<i64> = arr.iter().filter_map(|x| x.as_signed_integer()).collect();
                if nums.len() == 3 {
                    Some([nums[0], nums[1], nums[2]])
                } else {
                    None
                }
            });
        out.push(DumpedHotkey {
            id,
            action: action_name(id),
            enabled,
            params,
        });
    }
    Ok(out)
}

/// Render dumped hotkeys as `macos/keyboard.toml` text.
pub fn to_toml(dumped: &[DumpedHotkey]) -> String {
    let mut s = String::new();
    s.push_str("# Generated by `macboot keyboard dump`.\n");
    s.push_str("apply = \"activateSettings\"\n\n");
    for h in dumped {
        match h.action {
            Some(action) => {
                s.push_str("[[hotkey]]\n");
                s.push_str(&format!("action = {action:?}\n"));
                s.push_str(&format!("enabled = {}\n", h.enabled));
                if let Some(p) = h.params {
                    s.push_str(&format!("chord = {:?}\n", format_chord(p)));
                }
            }
            None => {
                s.push_str(&format!("[[raw]] # id {} has no friendly name\n", h.id));
                s.push_str(&format!("id = {}\n", h.id));
                s.push_str(&format!("enabled = {}\n", h.enabled));
                if let Some(p) = h.params {
                    s.push_str(&format!("parameters = [{}, {}, {}]\n", p[0], p[1], p[2]));
                }
            }
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_roundtrips_through_params() {
        let params = parse_chord("cmd+shift+3").unwrap();
        // ⌘=1048576 + ⇧=131072 = 1179648, keycode 3 = 20, ascii '3' = 51.
        assert_eq!(params, [51, 20, 1179648]);
        assert_eq!(format_chord(params), "shift+cmd+3");
    }

    #[test]
    fn named_key_uses_no_ascii() {
        let params = parse_chord("cmd+space").unwrap();
        assert_eq!(params[0], NO_ASCII);
        assert_eq!(params[1], 49); // space
        assert_eq!(params[2], MOD_CMD);
    }

    #[test]
    fn ctrl_up_parses_for_mission_control() {
        let params = parse_chord("ctrl+up").unwrap();
        assert_eq!(params[1], 126); // up arrow
        assert_eq!(params[2], MOD_CTRL);
    }

    #[test]
    fn glyphs_for_app_shortcut() {
        assert_eq!(chord_to_glyphs("cmd+shift+m").unwrap(), "@$m");
    }
}

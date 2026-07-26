//! Translation between the TOML in `macos/*.toml` and the plist values that
//! `defaults` actually stores.
//!
//! Scalars are the easy half. The reason this module exists is the other half:
//! Dock tile lists, Control Center's menu bar layout and most browser settings
//! are arrays and dictionaries, so a settings manager that only speaks
//! bool/int/float/string cannot capture or restore them at all.
//!
//! Two directions, and both must agree:
//!
//! * [`to_plist`] takes a declared `type =` plus a TOML value and produces the
//!   plist value to write.
//! * [`render`] takes a live plist value and produces the `[[defaults]]` block
//!   that would write it back — which is what `macos dump` emits.
//!
//! Nesting is where the two can drift apart, because TOML has no `data` type
//! and only `type =` at the top carries an annotation. Inside an array or dict
//! the plist type is inferred structurally, and a data blob round-trips as a
//! [`DATA_PREFIX`]-tagged string.

use crate::config::DefaultType;
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// Marks a TOML string as a plist `<data>` blob. Needed because TOML has no
/// binary type, and nested values carry no `type =` annotation of their own.
pub const DATA_PREFIX: &str = "base64:";

/// Build the plist value a `[[defaults]]` entry describes.
pub fn to_plist(kind: DefaultType, value: &toml::Value) -> Result<plist::Value> {
    let mismatch = |want: &str| {
        anyhow::anyhow!(
            "type = {:?} needs a TOML {want}, got {}",
            type_name(kind),
            describe_toml(value)
        )
    };
    match kind {
        DefaultType::Bool => Ok(plist::Value::Boolean(
            value.as_bool().ok_or_else(|| mismatch("boolean"))?,
        )),
        DefaultType::Int => Ok(plist::Value::Integer(
            value
                .as_integer()
                .ok_or_else(|| mismatch("integer"))?
                .into(),
        )),
        // An integer literal is a perfectly good float; accept it rather than
        // making the user write `1.0`.
        DefaultType::Float => {
            let f = value
                .as_float()
                .or_else(|| value.as_integer().map(|i| i as f64))
                .ok_or_else(|| mismatch("float"))?;
            Ok(plist::Value::Real(f))
        }
        DefaultType::String => Ok(plist::Value::String(
            value
                .as_str()
                .ok_or_else(|| mismatch("string"))?
                .to_string(),
        )),
        DefaultType::Data => {
            let text = value.as_str().ok_or_else(|| mismatch("base64 string"))?;
            Ok(plist::Value::Data(decode_data(text)?))
        }
        DefaultType::Date => {
            let text = match value {
                toml::Value::Datetime(dt) => dt.to_string(),
                toml::Value::String(s) => s.clone(),
                _ => return Err(mismatch("datetime or RFC 3339 string")),
            };
            Ok(plist::Value::Date(parse_date(&text)?))
        }
        DefaultType::Array => {
            let items = value.as_array().ok_or_else(|| mismatch("array"))?;
            let out: Result<Vec<_>> = items.iter().map(infer).collect();
            Ok(plist::Value::Array(out?))
        }
        DefaultType::Dict => {
            let table = value.as_table().ok_or_else(|| mismatch("table"))?;
            let mut dict = plist::Dictionary::new();
            for (k, v) in table {
                dict.insert(k.clone(), infer(v)?);
            }
            Ok(plist::Value::Dictionary(dict))
        }
    }
}

/// Convert a TOML value nested inside an array or dict, where no `type =`
/// annotation is available and the plist type follows from the TOML shape.
fn infer(value: &toml::Value) -> Result<plist::Value> {
    Ok(match value {
        toml::Value::Boolean(b) => plist::Value::Boolean(*b),
        toml::Value::Integer(i) => plist::Value::Integer((*i).into()),
        toml::Value::Float(f) => plist::Value::Real(*f),
        toml::Value::Datetime(dt) => plist::Value::Date(parse_date(&dt.to_string())?),
        toml::Value::String(s) => match s.strip_prefix(DATA_PREFIX) {
            Some(encoded) => plist::Value::Data(decode_data(encoded)?),
            None => plist::Value::String(s.clone()),
        },
        toml::Value::Array(items) => {
            let out: Result<Vec<_>> = items.iter().map(infer).collect();
            plist::Value::Array(out?)
        }
        toml::Value::Table(table) => {
            let mut dict = plist::Dictionary::new();
            for (k, v) in table {
                dict.insert(k.clone(), infer(v)?);
            }
            plist::Value::Dictionary(dict)
        }
    })
}

fn decode_data(text: &str) -> Result<Vec<u8>> {
    let encoded = text.strip_prefix(DATA_PREFIX).unwrap_or(text);
    BASE64
        .decode(encoded.trim())
        .with_context(|| format!("decoding base64 data {encoded:?}"))
}

fn parse_date(text: &str) -> Result<plist::Date> {
    // TOML renders local datetimes without an offset, which RFC 3339 requires.
    let normalized = if text.ends_with('Z') || text.contains('+') || text.matches('-').count() > 2 {
        text.to_string()
    } else {
        format!("{text}Z")
    };
    plist::Date::from_xml_format(&normalized)
        .map_err(|e| anyhow::anyhow!("parsing date {text:?}: {e}"))
}

/// The `type =` name for a declared kind.
pub fn type_name(kind: DefaultType) -> &'static str {
    match kind {
        DefaultType::Bool => "bool",
        DefaultType::Int => "int",
        DefaultType::Float => "float",
        DefaultType::String => "string",
        DefaultType::Array => "array",
        DefaultType::Dict => "dict",
        DefaultType::Data => "data",
        DefaultType::Date => "date",
    }
}

fn describe_toml(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::String(_) => "string",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

/// The declared kind that describes a live plist value, for `macos dump`.
///
/// Returns None only for shapes with no plist equivalent we can name; every
/// value `defaults export` can produce maps to something.
pub fn kind_of(value: &plist::Value) -> Option<DefaultType> {
    Some(match value {
        plist::Value::Boolean(_) => DefaultType::Bool,
        plist::Value::Integer(_) => DefaultType::Int,
        plist::Value::Real(_) => DefaultType::Float,
        plist::Value::String(_) => DefaultType::String,
        plist::Value::Data(_) => DefaultType::Data,
        plist::Value::Date(_) => DefaultType::Date,
        plist::Value::Array(_) => DefaultType::Array,
        plist::Value::Dictionary(_) => DefaultType::Dict,
        _ => return None,
    })
}

/// Render a plist value as the TOML literal for a `value =` field.
///
/// `indent` is the column the value starts at, used to lay out multi-line
/// arrays. Dictionaries always render as single-line inline tables because
/// TOML forbids newlines inside one.
pub fn render(value: &plist::Value, indent: usize) -> Option<String> {
    Some(match value {
        plist::Value::Boolean(b) => b.to_string(),
        plist::Value::Integer(i) => i.to_string(),
        // `1` would parse back as an integer and fail the float type check.
        plist::Value::Real(f) => {
            let rendered = f.to_string();
            if rendered.contains(['.', 'e', 'E']) {
                rendered
            } else {
                format!("{rendered}.0")
            }
        }
        plist::Value::String(s) => quote(s),
        plist::Value::Data(bytes) => quote(&format!("{DATA_PREFIX}{}", BASE64.encode(bytes))),
        plist::Value::Date(d) => d.to_xml_format(),
        plist::Value::Array(items) => {
            if items.is_empty() {
                return Some("[]".to_string());
            }
            let pad = " ".repeat(indent + 2);
            let mut out = String::from("[\n");
            for item in items {
                out.push_str(&pad);
                out.push_str(&render(item, indent + 2)?);
                out.push_str(",\n");
            }
            out.push_str(&" ".repeat(indent));
            out.push(']');
            out
        }
        plist::Value::Dictionary(dict) => {
            if dict.is_empty() {
                return Some("{}".to_string());
            }
            let mut parts = Vec::with_capacity(dict.len());
            for (k, v) in dict {
                // An inline table must stay on one line, so nested arrays are
                // rendered flat regardless of how deep they sit.
                parts.push(format!("{} = {}", quote(k), flatten(&render(v, 0)?)));
            }
            format!("{{ {} }}", parts.join(", "))
        }
        _ => return None,
    })
}

/// Collapse a multi-line rendering onto one line, for use inside inline tables.
fn flatten(rendered: &str) -> String {
    let mut out = String::with_capacity(rendered.len());
    let mut pending_space = false;
    for line in rendered.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if pending_space && !trimmed.starts_with(']') && !out.ends_with('[') {
            out.push(' ');
        }
        out.push_str(trimmed);
        pending_space = true;
    }
    out
}

/// Quote a string as a TOML basic string.
fn quote(s: &str) -> String {
    format!("{s:?}")
}

/// Is the live value equivalent to the one we want to write?
///
/// Not plain `==`: `defaults` stores small numbers loosely, so a config that
/// says `1` must not read as drift against a stored `1.0`, or `apply` would
/// rewrite the same key on every run and `diff` would never come clean.
pub fn equal(want: &plist::Value, got: &plist::Value) -> bool {
    match (want, got) {
        (plist::Value::Real(a), plist::Value::Integer(b)) => {
            b.as_signed().map(|b| b as f64) == Some(*a)
        }
        (plist::Value::Integer(a), plist::Value::Real(b)) => {
            a.as_signed().map(|a| a as f64) == Some(*b)
        }
        (plist::Value::Real(a), plist::Value::Real(b)) => (a - b).abs() < f64::EPSILON,
        // Booleans are stored as 0/1 in some domains and as true/false in others.
        (plist::Value::Boolean(a), plist::Value::Integer(b)) => {
            b.as_signed().map(|b| b != 0) == Some(*a)
        }
        (plist::Value::Integer(a), plist::Value::Boolean(b)) => {
            a.as_signed().map(|a| a != 0) == Some(*b)
        }
        (plist::Value::Array(a), plist::Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| equal(x, y))
        }
        (plist::Value::Dictionary(a), plist::Value::Dictionary(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).map(|other| equal(v, other)).unwrap_or(false))
        }
        _ => want == got,
    }
}

/// Serialize a plist value as an XML document, which is the form `defaults
/// write` accepts for every type and the form we archive in state.
pub fn to_xml(value: &plist::Value) -> Result<String> {
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, value).context("serializing plist value to XML")?;
    String::from_utf8(buf).context("plist XML was not valid UTF-8")
}

/// Parse an XML plist document back into a value.
pub fn from_xml(text: &str) -> Result<plist::Value> {
    plist::Value::from_reader_xml(std::io::Cursor::new(text.as_bytes()))
        .context("parsing plist XML")
}

/// A short, human-readable rendering for summaries and `# was:` comments.
pub fn describe(value: &plist::Value) -> String {
    match value {
        plist::Value::Array(a) => format!("<array of {}>", a.len()),
        plist::Value::Dictionary(d) => format!("<dict of {}>", d.len()),
        plist::Value::Data(b) => format!("<{} bytes>", b.len()),
        other => render(other, 0).unwrap_or_else(|| "<unsupported>".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toml_value(text: &str) -> toml::Value {
        // Wrap so any bare value can be parsed, including multi-line arrays.
        let doc: toml::Value = toml::from_str(&format!("v = {text}")).unwrap();
        doc.get("v").unwrap().clone()
    }

    /// The property that matters: whatever `dump` writes for a live value must
    /// parse back into that same value. If this breaks, a dumped file silently
    /// stops describing the machine it came from.
    fn assert_round_trips(value: plist::Value) {
        let kind = kind_of(&value).expect("value should have a declared type");
        let rendered = render(&value, 0).expect("value should render");
        let parsed = to_plist(kind, &toml_value(&rendered))
            .unwrap_or_else(|e| panic!("re-parsing {rendered}: {e:#}"));
        assert!(
            equal(&value, &parsed),
            "round-trip changed the value: {value:?} -> {rendered} -> {parsed:?}"
        );
    }

    #[test]
    fn scalars_round_trip() {
        assert_round_trips(plist::Value::Boolean(true));
        assert_round_trips(plist::Value::Integer(36.into()));
        assert_round_trips(plist::Value::Real(0.25));
        assert_round_trips(plist::Value::String("Nlsv".into()));
    }

    #[test]
    fn a_whole_number_float_still_reads_back_as_a_float() {
        // `defaults` reports plenty of reals as 1; rendering that as bare `1`
        // would come back as an integer and fail the `type = "float"` check.
        let rendered = render(&plist::Value::Real(1.0), 0).unwrap();
        assert_eq!(rendered, "1.0");
        assert_round_trips(plist::Value::Real(1.0));
    }

    #[test]
    fn strings_needing_escapes_round_trip() {
        assert_round_trips(plist::Value::String(r#"a "quoted" \ path"#.into()));
        assert_round_trips(plist::Value::String("tab\there".into()));
    }

    #[test]
    fn arrays_round_trip() {
        assert_round_trips(plist::Value::Array(vec![
            plist::Value::String("a".into()),
            plist::Value::Integer(7.into()),
            plist::Value::Boolean(false),
        ]));
        assert_round_trips(plist::Value::Array(vec![]));
    }

    #[test]
    fn dicts_round_trip() {
        let mut dict = plist::Dictionary::new();
        dict.insert("Battery".into(), plist::Value::Integer(18.into()));
        dict.insert("Bluetooth".into(), plist::Value::Boolean(true));
        assert_round_trips(plist::Value::Dictionary(dict));
        assert_round_trips(plist::Value::Dictionary(plist::Dictionary::new()));
    }

    /// The real shape of `com.apple.dock persistent-apps`: an array of dicts
    /// with a nested dict inside each one.
    #[test]
    fn nested_dock_shaped_value_round_trips() {
        let mut tile_data = plist::Dictionary::new();
        tile_data.insert("file-label".into(), plist::Value::String("Safari".into()));
        tile_data.insert(
            "bundle-identifier".into(),
            plist::Value::String("com.apple.Safari".into()),
        );
        let mut tile = plist::Dictionary::new();
        tile.insert("tile-type".into(), plist::Value::String("file-tile".into()));
        tile.insert("tile-data".into(), plist::Value::Dictionary(tile_data));
        assert_round_trips(plist::Value::Array(vec![plist::Value::Dictionary(tile)]));
    }

    #[test]
    fn data_round_trips_through_a_tagged_string() {
        let value = plist::Value::Data(vec![0xde, 0xad, 0xbe, 0xef]);
        let rendered = render(&value, 0).unwrap();
        assert!(rendered.contains(DATA_PREFIX), "{rendered}");
        assert_round_trips(value);
    }

    /// Data nested in a dict has no `type =` of its own, so the prefix is the
    /// only thing keeping it from decaying into a plain string.
    #[test]
    fn nested_data_survives_because_of_the_prefix() {
        let mut dict = plist::Dictionary::new();
        dict.insert("blob".into(), plist::Value::Data(vec![1, 2, 3]));
        assert_round_trips(plist::Value::Dictionary(dict));
    }

    #[test]
    fn dates_round_trip() {
        let date = plist::Date::from_xml_format("2026-07-25T12:00:00Z").unwrap();
        assert_round_trips(plist::Value::Date(date));
    }

    #[test]
    fn inline_tables_stay_on_one_line() {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "items".into(),
            plist::Value::Array(vec![
                plist::Value::Integer(1.into()),
                plist::Value::Integer(2.into()),
            ]),
        );
        let rendered = render(&plist::Value::Dictionary(dict), 0).unwrap();
        assert!(
            !rendered.contains('\n'),
            "TOML forbids newlines here: {rendered}"
        );
        assert_round_trips(plist::Value::Dictionary({
            let mut d = plist::Dictionary::new();
            d.insert(
                "items".into(),
                plist::Value::Array(vec![plist::Value::Integer(1.into())]),
            );
            d
        }));
    }

    #[test]
    fn int_and_float_forms_of_the_same_number_are_not_drift() {
        assert!(equal(
            &plist::Value::Real(1.0),
            &plist::Value::Integer(1.into())
        ));
        assert!(equal(
            &plist::Value::Integer(1.into()),
            &plist::Value::Real(1.0)
        ));
        assert!(!equal(
            &plist::Value::Real(1.5),
            &plist::Value::Integer(1.into())
        ));
    }

    #[test]
    fn bools_stored_as_zero_or_one_are_not_drift() {
        assert!(equal(
            &plist::Value::Boolean(true),
            &plist::Value::Integer(1.into())
        ));
        assert!(equal(
            &plist::Value::Boolean(false),
            &plist::Value::Integer(0.into())
        ));
        assert!(!equal(
            &plist::Value::Boolean(true),
            &plist::Value::Integer(0.into())
        ));
    }

    #[test]
    fn dict_comparison_ignores_key_order_but_not_content() {
        let build = |pairs: &[(&str, i64)]| {
            let mut d = plist::Dictionary::new();
            for (k, v) in pairs {
                d.insert((*k).into(), plist::Value::Integer((*v).into()));
            }
            plist::Value::Dictionary(d)
        };
        assert!(equal(
            &build(&[("a", 1), ("b", 2)]),
            &build(&[("b", 2), ("a", 1)])
        ));
        assert!(!equal(&build(&[("a", 1)]), &build(&[("a", 1), ("b", 2)])));
    }

    #[test]
    fn xml_round_trips_for_complex_values() {
        let value = plist::Value::Array(vec![
            plist::Value::String("x".into()),
            plist::Value::Data(vec![9, 9]),
        ]);
        let xml = to_xml(&value).unwrap();
        assert!(equal(&value, &from_xml(&xml).unwrap()));
    }

    #[test]
    fn declared_type_must_match_the_toml_shape() {
        let err = to_plist(DefaultType::Int, &toml::Value::String("nope".into())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("int"), "{msg}");
        assert!(msg.contains("string"), "{msg}");
    }

    #[test]
    fn an_integer_literal_is_accepted_for_a_float() {
        let got = to_plist(DefaultType::Float, &toml::Value::Integer(2)).unwrap();
        assert!(equal(&plist::Value::Real(2.0), &got));
    }
}

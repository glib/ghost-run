//! A small YAML emitter that writes what PyYAML's `safe_dump(sort_keys=False)`
//! writes: block style, two-space indent, lists under a key not indented.
//!
//! Strings are left plain only when both PyYAML (YAML 1.1) and YAML 1.2
//! readers would read them back as strings, so `2026-05-31`, `yes`, `on`,
//! `1e5` or `0x1F` are quoted. The Python tool reads with YAML 1.1 rules, where
//! an unquoted `on` is `True` and an unquoted date is a date.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use serde_norway::{Mapping, Number, Value};

use crate::py;

/// PyYAML's implicit resolvers for plain scalars (bool, float, int, merge,
/// null, timestamp, value): anything matching is not read back as a string.
static YAML11_NON_STRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:",
        r"yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF",
        r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?",
        r"|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
        r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
        r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        r"|[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+",
        r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
        r"|<<|~|null|Null|NULL|=",
        r"|[0-9]{4}-[0-9]{2}-[0-9]{2}",
        r"|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?",
        r")$"
    ))
    .expect("valid regex")
});

pub fn emit(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Mapping(m) if !m.is_empty() => mapping(m, 0, false, &mut out),
        Value::Sequence(s) if !s.is_empty() => sequence(s, 0, false, &mut out),
        other => {
            out.push_str(&scalar(other));
            out.push('\n');
        }
    }
    out
}

fn is_block(value: &Value) -> bool {
    match untag(value) {
        Value::Mapping(m) => !m.is_empty(),
        Value::Sequence(s) => !s.is_empty(),
        _ => false,
    }
}

fn untag(value: &Value) -> &Value {
    match value {
        Value::Tagged(t) => untag(&t.value),
        v => v,
    }
}

/// Writes `m` at `indent`; when `inline`, the first key continues the current
/// line (after `- `).
fn mapping(m: &Mapping, indent: usize, inline: bool, out: &mut String) {
    for (i, (key, value)) in m.iter().enumerate() {
        if !(inline && i == 0) {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str(&scalar(key));
        out.push(':');
        match untag(value) {
            Value::Mapping(child) if !child.is_empty() => {
                out.push('\n');
                mapping(child, indent + 2, false, out);
            }
            Value::Sequence(items) if !items.is_empty() => {
                out.push('\n');
                sequence(items, indent, false, out);
            }
            v => {
                out.push(' ');
                out.push_str(&scalar(v));
                out.push('\n');
            }
        }
    }
}

fn sequence(items: &[Value], indent: usize, inline: bool, out: &mut String) {
    for (i, item) in items.iter().enumerate() {
        if !(inline && i == 0) {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str("- ");
        match untag(item) {
            Value::Mapping(child) if is_block(item) => mapping(child, indent + 2, true, out),
            Value::Sequence(child) if is_block(item) => sequence(child, indent + 2, true, out),
            v => {
                out.push_str(&scalar(v));
                out.push('\n');
            }
        }
    }
}

fn scalar(value: &Value) -> String {
    match untag(value) {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number(n),
        Value::String(s) => string(s),
        Value::Mapping(_) => "{}".into(),
        Value::Sequence(_) => "[]".into(),
        Value::Tagged(_) => unreachable!("untagged above"),
    }
}

/// Numbers as PyYAML writes them: Python `repr`, `.inf`/`.nan`, and a `.0`
/// added to bare exponents (`1.0e+16`).
fn number(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    let repr = py::float_repr(f);
    match repr.split_once('e') {
        Some((mantissa, exp)) => {
            let mantissa = if mantissa.contains('.') {
                mantissa.to_owned()
            } else {
                format!("{mantissa}.0")
            };
            let exp = if exp.starts_with('-') {
                exp.to_owned()
            } else {
                format!("+{exp}")
            };
            format!("{mantissa}e{exp}")
        }
        None => repr,
    }
}

/// PyYAML (`allow_unicode=False`, its default) double-quotes any string with
/// a character outside printable ASCII, escaping it; otherwise plain if safe,
/// else single-quoted.
fn string(s: &str) -> String {
    if !s.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        double_quoted(s)
    } else if plain_ok(s) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn plain_ok(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    if s.starts_with(' ') || s.ends_with(' ') || s.chars().any(|c| c.is_control()) {
        return false;
    }
    if "#,[]{}&*!|>'\"%@`".contains(first) {
        return false;
    }
    if "-?:".contains(first) && s.chars().nth(1).is_none_or(|c| c == ' ') {
        return false;
    }
    if s.contains(": ")
        || s.contains(" #")
        || s.ends_with(':')
        || s.starts_with("---")
        || s.starts_with("...")
    {
        return false;
    }
    if YAML11_NON_STRING.is_match(s) {
        return false;
    }
    // And a YAML 1.2 reader must also see a string.
    matches!(serde_norway::from_str::<Value>(s), Ok(Value::String(ref back)) if back == s)
}

fn double_quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\u{07}' => out.push_str("\\a"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0b}' => out.push_str("\\v"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{a0}' => out.push_str("\\_"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if c == ' ' || c.is_ascii_graphic() => out.push(c),
            c if (c as u32) <= 0xff => {
                let _ = write!(out, "\\x{:02X}", c as u32);
            }
            c if (c as u32) <= 0xffff => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => {
                let _ = write!(out, "\\U{:08X}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_strings_yaml_readers_would_retype() {
        for s in [
            "2026-05-31",
            "2026-05-31T23:59:59Z",
            "yes",
            "On",
            "off",
            "null",
            "~",
            "123",
            "1_000",
            "0x1F",
            "012",
            "1:30",
            "3.5",
            "1e5",
            ".inf",
            "<<",
            "=",
        ] {
            assert!(
                string(s).starts_with('\''),
                "{s} should be quoted, got {}",
                string(s)
            );
        }
        for s in [
            "GX010123.MP4",
            "Lower Whistler A-Line",
            "run_2026_05_31_001",
            "date_fallback",
            "S1",
            "GX010123.MP4:129979c14aa405e2",
        ] {
            assert_eq!(string(s), s, "{s} should stay plain");
        }
        assert_eq!(string(""), "''");
        assert_eq!(string("it's"), "it's");
        assert_eq!(string("- a"), "'- a'");
        assert_eq!(string("a: b"), "'a: b'");
        assert_eq!(string("#tag"), "'#tag'");
        assert_eq!(string("two\nlines"), "\"two\\nlines\"");
        // PyYAML's escapes: yaml.safe_dump("Café 🚵\t") == '"Caf\\xE9 \\U0001F6B5\\t"\n'
        assert_eq!(string("Café 🚵\t"), "\"Caf\\xE9 \\U0001F6B5\\t\"");
        assert_eq!(string("Ūla — 1"), "\"\\u016Ala \\u2014 1\"");
    }

    #[test]
    fn numbers_like_pyyaml() {
        let n = |f: f64| number(&Number::from(f));
        assert_eq!(n(59.94), "59.94");
        assert_eq!(n(100.0), "100.0");
        assert_eq!(n(1e16), "1.0e+16");
        assert_eq!(n(f64::INFINITY), ".inf");
    }

    #[test]
    fn layout_matches_pyyaml() {
        let doc: Value = serde_norway::from_str(
            "a: 1\nlist:\n- id: x\n  inner:\n    k: v\n  tags:\n  - t1\n- plain\nempty: []\nnone: null\n",
        )
        .unwrap();
        assert_eq!(
            emit(&doc),
            "a: 1\nlist:\n- id: x\n  inner:\n    k: v\n  tags:\n  - t1\n- plain\nempty: []\nnone: null\n"
        );
    }
}

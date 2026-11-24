//! Python-compatible conversions of YAML values.
//!
//! The Python tool reads the database with PyYAML and coerces fields with
//! `str()`, `int()`, `float()` and `bool()`. These helpers do the same, so both
//! tools accept the same files and read them the same way.

use serde_norway::Value;

/// Python `str(value)` for scalars. `None` for values Python would render as
/// something no sane database contains (sequences, mappings).
pub fn to_str(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("True".into()),
        Value::Bool(false) => Some("False".into()),
        Value::Null => Some("None".into()),
        Value::Number(n) => Some(if let Some(i) = n.as_i64() {
            i.to_string()
        } else if let Some(u) = n.as_u64() {
            u.to_string()
        } else {
            float_repr(n.as_f64().unwrap_or(f64::NAN))
        }),
        Value::Tagged(tagged) => to_str(&tagged.value),
        Value::Sequence(_) | Value::Mapping(_) => None,
    }
}

/// Python `int(value)`: integers as is, floats truncated toward zero, strings
/// parsed (surrounding whitespace allowed), booleans as 0/1.
pub fn to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            let f = n.as_f64()?;
            f.is_finite().then(|| f.trunc() as i64)
        }),
        Value::String(s) => s.trim().replace('_', "").parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Tagged(tagged) => to_int(&tagged.value),
        _ => None,
    }
}

/// Python `float(value)`.
pub fn to_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            match t.as_str() {
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                _ => t.replace('_', "").parse().ok(),
            }
        }
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Tagged(tagged) => to_float(&tagged.value),
        _ => None,
    }
}

/// Python truthiness (`bool(value)`).
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Sequence(s) => !s.is_empty(),
        Value::Mapping(m) => !m.is_empty(),
        Value::Tagged(tagged) => truthy(&tagged.value),
    }
}

/// Python `repr(value)`, for error messages.
pub fn repr(value: Option<&Value>) -> String {
    match value {
        None => "None".into(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(other) => to_str(other).unwrap_or_else(|| "...".into()),
    }
}

/// Python `repr(float)`: shortest round-trip digits, always with a decimal
/// point or exponent.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    let s = format!("{f:?}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// Python `json.dumps(string)` with the default `ensure_ascii=True`.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Python `round(x)` to an integer: halves go to the even neighbour.
pub fn round_half_even(x: f64) -> i64 {
    x.round_ties_even() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerces_like_python() {
        let v = |s: &str| serde_norway::from_str::<Value>(s).unwrap();
        assert_eq!(to_str(&v("42")).as_deref(), Some("42"));
        assert_eq!(to_str(&v("59.94")).as_deref(), Some("59.94"));
        assert_eq!(to_str(&v("60.0")).as_deref(), Some("60.0"));
        assert_eq!(to_int(&v("100.9")), Some(100));
        assert_eq!(to_int(&v("-3.7")), Some(-3));
        assert_eq!(to_int(&v("' 7 '")), Some(7));
        assert_eq!(to_float(&v("'29.97'")), Some(29.97));
        assert!(!truthy(&v("''")));
        assert!(truthy(&v("'false'")), "Python bool('false') is True");
        assert_eq!(repr(None), "None");
        assert_eq!(repr(Some(&v("'3'"))), "'3'");
    }

    #[test]
    fn json_escapes_like_python() {
        assert_eq!(json_string("Start"), "\"Start\"");
        assert_eq!(json_string("é\"\n"), "\"\\u00e9\\\"\\n\"");
        assert_eq!(json_string("🚵"), "\"\\ud83d\\udeb5\"");
    }

    #[test]
    fn rounds_half_to_even() {
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(3.5), 4);
        assert_eq!(round_half_even(-2.5), -2);
    }
}

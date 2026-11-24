//! Capture times: when a run's clip was recorded, as a UTC string
//! `YYYY-MM-DDTHH:MM:SSZ`. Comparisons ("best before this run") order runs by it.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde_norway::Value;

pub const FILENAME_TIMESTAMP_SOURCE: &str = "filename_timestamp";
pub const FILESYSTEM_CREATED_SOURCE: &str = "filesystem_created";
pub const DATE_FALLBACK_SOURCE: &str = "date_fallback";
pub const MANUAL_SOURCE: &str = "manual";

/// Valid `capture_time_source` values, sorted (as Python lists them in errors).
pub const CAPTURE_TIME_SOURCES: [&str; 4] = [
    DATE_FALLBACK_SOURCE,
    FILENAME_TIMESTAMP_SOURCE,
    FILESYSTEM_CREATED_SOURCE,
    MANUAL_SOURCE,
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid isoformat string: '{0}'")]
pub struct InvalidCaptureTime(pub String);

/// Normalizes a stored capture time to UTC `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Accepts what Python's `datetime.fromisoformat` does (after `Z` → `+00:00`):
/// a date, or a date and time with any one-character separator, optional
/// fraction and optional `±HH:MM[:SS]` offset. Times without an offset are UTC.
/// Fractions are dropped, not rounded. Empty and null values are `None`.
pub fn normalize(text: &str) -> Result<Option<String>, InvalidCaptureTime> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    parse_iso(&text.replace('Z', "+00:00"))
        .map(|dt| Some(format_utc(dt)))
        .ok_or_else(|| InvalidCaptureTime(text.to_owned()))
}

/// [`normalize`] for a YAML value (null and non-strings as Python's `str()`).
pub fn normalize_value(value: Option<&Value>) -> Result<Option<String>, InvalidCaptureTime> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => normalize(&crate::py::to_str(v).unwrap_or_default()),
    }
}

/// The fallback capture time for a run with only a date: 23:59:59 UTC that day,
/// so dated runs sort after anything with a real time on the same day.
pub fn from_date(run_date: &str) -> Result<Option<String>, InvalidCaptureTime> {
    if run_date.is_empty() {
        return Ok(None);
    }
    let date = parse_date(run_date).ok_or_else(|| InvalidCaptureTime(run_date.to_owned()))?;
    let dt = date.and_time(NaiveTime::from_hms_opt(23, 59, 59).expect("valid time"));
    Ok(Some(format_utc(Utc.from_utc_datetime(&dt).fixed_offset())))
}

/// The capture time used for ordering: the stored one, else the date fallback.
pub fn effective(capture_time: Option<&str>, run_date: &str) -> Option<String> {
    capture_time
        .and_then(|t| normalize(t).ok().flatten())
        .or_else(|| from_date(run_date).ok().flatten())
}

/// A DJI filename's embedded local capture time (`DJI_YYYYMMDDHHMMSS_seq_type.ext`),
/// converted to UTC using `local` as the camera's zone.
pub fn from_dji_filename<Tz: TimeZone>(filename: &str, local: &Tz) -> Option<String> {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let re = regex::Regex::new(r"(?i)^DJI_(\d{14})_\d+_[^.]+(?:\.[^.]+)?$").expect("valid regex");
    let stamp = re.captures(name)?.get(1)?.as_str();
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").ok()?;
    let local_dt = local.from_local_datetime(&naive).earliest()?;
    Some(format_utc(local_dt.fixed_offset()))
}

fn format_utc(dt: DateTime<FixedOffset>) -> String {
    dt.with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    NaiveDate::from_ymd_opt(digits(&s[0..4])?, digits(&s[5..7])?, digits(&s[8..10])?)
}

fn parse_iso(s: &str) -> Option<DateTime<FixedOffset>> {
    let date = parse_date(s.get(0..10)?)?;
    let rest = &s[10..];
    if rest.is_empty() {
        return Some(
            Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN))
                .fixed_offset(),
        );
    }
    // One separator character of any kind, then the time.
    let mut chars = rest.chars();
    chars.next()?;
    let rest = chars.as_str();
    let (time_part, offset) = match rest.find(['+', '-']) {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let time = parse_time(time_part)?;
    let naive = date.and_time(time);
    let offset = match offset {
        None => FixedOffset::east_opt(0)?,
        Some(o) => parse_offset(o)?,
    };
    offset.from_local_datetime(&naive).single()
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let (hms, frac) = match s.split_once(['.', ',']) {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (s, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let h = digits(parts[0])?;
    let m = parts.get(1).map_or(Some(0), |p| digits(p))?;
    let sec = parts.get(2).map_or(Some(0), |p| digits(p))?;
    if let Some(frac) = frac
        && (parts.len() != 3 || frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    NaiveTime::from_hms_opt(h, m, sec)
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let parts: Vec<&str> = s[1..].split(':').collect();
    if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let h: i32 = digits(parts[0])?;
    let m: i32 = digits(parts[1])?;
    let sec: i32 = parts.get(2).map_or(Some(0), |p| digits(p))?;
    if h > 23 || m > 59 || sec > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60 + sec))
}

fn digits<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_utc_z() {
        let n = |s: &str| normalize(s).unwrap();
        assert_eq!(
            n("2026-05-31T23:59:59Z").as_deref(),
            Some("2026-05-31T23:59:59Z")
        );
        assert_eq!(
            n("2026-05-31T16:00:00-07:00").as_deref(),
            Some("2026-05-31T23:00:00Z")
        );
        assert_eq!(
            n("2026-05-31 10:00:00").as_deref(),
            Some("2026-05-31T10:00:00Z")
        );
        assert_eq!(
            n("2026-05-31T10:00:00.999").as_deref(),
            Some("2026-05-31T10:00:00Z")
        );
        assert_eq!(n("2026-05-31").as_deref(), Some("2026-05-31T00:00:00Z"));
        assert_eq!(n("2026-05-31T10").as_deref(), Some("2026-05-31T10:00:00Z"));
        assert_eq!(n("  ").unwrap_or_default(), "");
        assert!(normalize("not a time").is_err());
        assert!(normalize("2026-13-01").is_err());
    }

    #[test]
    fn date_fallback_is_end_of_day_utc() {
        assert_eq!(
            from_date("2026-05-31").unwrap().as_deref(),
            Some("2026-05-31T23:59:59Z")
        );
        assert!(from_date("31/05/2026").is_err());
    }

    #[test]
    fn reads_dji_filename_in_camera_zone() {
        let pacific = FixedOffset::west_opt(7 * 3600).unwrap();
        assert_eq!(
            from_dji_filename("DJI_20260531101500_0001_D.MP4", &pacific).as_deref(),
            Some("2026-05-31T17:15:00Z")
        );
        assert_eq!(from_dji_filename("GX010123.MP4", &pacific), None);
        assert_eq!(
            from_dji_filename("DJI_20261301101500_0001_D.MP4", &pacific),
            None
        );
    }
}

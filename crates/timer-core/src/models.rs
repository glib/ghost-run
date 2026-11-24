//! Courses, runs and timing results. Field names and meaning follow the YAML
//! database (schema v2), which the Python tool reads and writes too.

use std::collections::BTreeMap;

use serde_norway::{Mapping, Value};

use crate::capture_time;
use crate::py;

pub const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Course {
    pub id: String,
    pub name: String,
    pub sector_count: i64,
}

/// Marker frames keyed by name (`Start`, `S1`…, `Finish`). Sorted by name, as
/// both tools store them.
pub type MarkerFrames = BTreeMap<String, i64>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMarker {
    pub name: String,
    pub frame: i64,
}

impl RawMarker {
    pub fn new(name: impl Into<String>, frame: i64) -> Self {
        Self {
            name: name.into(),
            frame,
        }
    }
}

/// Validated timing markers for one course: exactly `Start`, `S1`…`S(N-1)`,
/// `Finish`, in increasing frame order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerSnapshot {
    pub frames: MarkerFrames,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectorTiming {
    pub sector: i64,
    pub start_marker: String,
    pub end_marker: String,
    pub start_frame: i64,
    pub end_frame: i64,
    pub duration_frames: i64,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimingResult {
    pub sectors: Vec<SectorTiming>,
    pub lap_frames: i64,
    pub lap_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub id: String,
    pub course_id: String,
    pub date: String,
    pub filename: String,
    pub source_fps: f64,
    pub marker_frames: MarkerFrames,
    pub capture_time: Option<String>,
    pub capture_time_source: Option<String>,
    pub source_path: Option<String>,
    pub clip_id: Option<String>,
    pub fingerprint: Option<String>,
    pub committed: bool,
    pub ignored: bool,
    pub committed_at: Option<String>,
    pub metadata: Mapping,
}

impl RunRecord {
    /// A committed, not-ignored run with only the required fields set.
    pub fn new(
        id: impl Into<String>,
        course_id: impl Into<String>,
        date: impl Into<String>,
        filename: impl Into<String>,
        source_fps: f64,
        marker_frames: MarkerFrames,
    ) -> Self {
        Self {
            id: id.into(),
            course_id: course_id.into(),
            date: date.into(),
            filename: filename.into(),
            source_fps,
            marker_frames,
            capture_time: None,
            capture_time_source: None,
            source_path: None,
            clip_id: None,
            fingerprint: None,
            committed: true,
            ignored: false,
            committed_at: None,
            metadata: Mapping::new(),
        }
    }
}

/// Errors reading a course or run from the database.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("'{0}'")]
    MissingField(&'static str),
    #[error("{field} has an invalid value: {value}")]
    InvalidField { field: &'static str, value: String },
    #[error(transparent)]
    CaptureTime(#[from] capture_time::InvalidCaptureTime),
}

fn get<'a>(data: &'a Mapping, key: &str) -> Option<&'a Value> {
    data.get(key)
}

fn required<'a>(data: &'a Mapping, key: &'static str) -> Result<&'a Value, RecordError> {
    get(data, key).ok_or(RecordError::MissingField(key))
}

fn invalid(field: &'static str, value: &Value) -> RecordError {
    RecordError::InvalidField {
        field,
        value: py::repr(Some(value)),
    }
}

fn string_field(data: &Mapping, key: &'static str) -> Result<String, RecordError> {
    let v = required(data, key)?;
    py::to_str(v).ok_or_else(|| invalid(key, v))
}

/// Optional text field: missing, null and empty are `None`.
fn optional_text(data: &Mapping, key: &'static str) -> Result<Option<String>, RecordError> {
    match get(data, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(v) => py::to_str(v).map(Some).ok_or_else(|| invalid(key, v)),
    }
}

impl Course {
    pub fn from_value(value: &Value) -> Result<Self, RecordError> {
        let data = value.as_mapping().ok_or_else(|| invalid("course", value))?;
        let count = required(data, "sector_count")?;
        Ok(Self {
            id: string_field(data, "id")?,
            name: string_field(data, "name")?,
            sector_count: py::to_int(count).ok_or_else(|| invalid("sector_count", count))?,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut m = Mapping::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("name".into(), self.name.clone().into());
        m.insert("sector_count".into(), self.sector_count.into());
        Value::Mapping(m)
    }
}

impl RunRecord {
    /// Reads a run like the Python tool's `RunRecord.from_dict`: a missing
    /// date is today, and a missing capture time falls back to the date.
    pub fn from_value(value: &Value) -> Result<Self, RecordError> {
        let data = value.as_mapping().ok_or_else(|| invalid("run", value))?;

        let date = match get(data, "date") {
            Some(v) if py::truthy(v) => py::to_str(v).ok_or_else(|| invalid("date", v))?,
            _ => chrono::Local::now()
                .date_naive()
                .format("%Y-%m-%d")
                .to_string(),
        };

        let mut capture_time = capture_time::normalize_value(get(data, "capture_time"))?;
        let mut capture_time_source = optional_text(data, "capture_time_source")?;
        if capture_time.is_none() {
            capture_time = capture_time::from_date(&date)?;
            capture_time_source.get_or_insert_with(|| capture_time::DATE_FALLBACK_SOURCE.into());
        }

        let fps = required(data, "source_fps")?;
        let frames = required(data, "marker_frames")?;
        let frames_map = frames
            .as_mapping()
            .ok_or_else(|| invalid("marker_frames", frames))?;
        let mut marker_frames = MarkerFrames::new();
        for (k, v) in frames_map {
            let name = py::to_str(k).ok_or_else(|| invalid("marker_frames", k))?;
            let frame = py::to_int(v).ok_or_else(|| invalid("marker_frames", v))?;
            marker_frames.insert(name, frame);
        }

        let metadata = match get(data, "metadata") {
            Some(v) if py::truthy(v) => v
                .as_mapping()
                .cloned()
                .ok_or_else(|| invalid("metadata", v))?,
            _ => Mapping::new(),
        };

        Ok(Self {
            id: string_field(data, "id")?,
            course_id: string_field(data, "course_id")?,
            date,
            filename: match get(data, "filename") {
                Some(v) if py::truthy(v) => py::to_str(v).ok_or_else(|| invalid("filename", v))?,
                _ => String::new(),
            },
            source_fps: py::to_float(fps).ok_or_else(|| invalid("source_fps", fps))?,
            marker_frames,
            capture_time,
            capture_time_source,
            source_path: optional_text(data, "source_path")?,
            clip_id: optional_text(data, "clip_id")?,
            fingerprint: optional_text(data, "fingerprint")?,
            committed: get(data, "committed").is_none_or(py::truthy),
            ignored: get(data, "ignored").is_some_and(py::truthy),
            committed_at: optional_text(data, "committed_at")?,
            metadata,
        })
    }

    /// Writes a run in the Python tool's `to_dict` field order.
    pub fn to_value(&self) -> Value {
        let mut m = Mapping::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("course_id".into(), self.course_id.clone().into());
        m.insert("date".into(), self.date.clone().into());
        m.insert("filename".into(), self.filename.clone().into());
        m.insert("source_fps".into(), self.source_fps.into());
        m.insert("committed".into(), self.committed.into());
        m.insert("ignored".into(), self.ignored.into());
        let mut frames = Mapping::new();
        for (name, frame) in &self.marker_frames {
            frames.insert(name.clone().into(), (*frame).into());
        }
        m.insert("marker_frames".into(), Value::Mapping(frames));
        let optional = [
            ("capture_time", &self.capture_time),
            ("capture_time_source", &self.capture_time_source),
            ("source_path", &self.source_path),
            ("clip_id", &self.clip_id),
            ("fingerprint", &self.fingerprint),
            ("committed_at", &self.committed_at),
        ];
        for (key, value) in optional {
            if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                m.insert(key.into(), v.clone().into());
            }
        }
        if !self.metadata.is_empty() {
            m.insert("metadata".into(), Value::Mapping(self.metadata.clone()));
        }
        Value::Mapping(m)
    }
}

/// The current time as UTC `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

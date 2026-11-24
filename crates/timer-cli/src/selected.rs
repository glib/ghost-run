//! The clip a command times, from its arguments: `--course`, a marker CSV
//! (`name,frame` columns), the filename, fps and optional clip id, date and
//! capture time. Errors read as the Python CLI's.

use clap::Args;
use serde_norway::Value;
use timer_core::RawMarker;
use timer_core::capture_time::{
    FILENAME_TIMESTAMP_SOURCE, FILESYSTEM_CREATED_SOURCE, MANUAL_SOURCE, from_dji_filename,
};
use timer_core::overlay::SelectedRun;
use timer_core::py;

#[derive(Args)]
pub struct SelectedArgs {
    /// Course ID
    #[arg(long)]
    course: String,
    /// CSV with name,frame columns
    #[arg(long)]
    markers: String,
    #[arg(long)]
    filename: String,
    #[arg(long, value_parser = parse_float, allow_hyphen_values = true)]
    fps: f64,
    #[arg(long)]
    clip_id: Option<String>,
    #[arg(long)]
    date: Option<String>,
    #[arg(long)]
    capture_time: Option<String>,
    #[arg(long)]
    source_path: Option<String>,
}

/// Python `float()`, as argparse's `type=float`.
fn parse_float(s: &str) -> Result<f64, String> {
    py::to_float(&Value::String(s.into())).ok_or_else(|| format!("invalid float value: '{s}'"))
}

impl SelectedArgs {
    pub fn read(&self) -> Result<SelectedRun, String> {
        let mut capture_time = self.capture_time.clone().filter(|t| !t.is_empty());
        let mut source = capture_time.as_ref().map(|_| MANUAL_SOURCE);
        if capture_time.is_none() {
            capture_time = from_dji_filename(&self.filename, &chrono::Local);
            source = capture_time.as_ref().map(|_| FILENAME_TIMESTAMP_SOURCE);
        }
        if capture_time.is_none()
            && let Some(path) = self.source_path.as_deref().filter(|p| !p.is_empty())
        {
            capture_time = Some(changed_time(path).map_err(|e| {
                format!(
                    "could not read source file creation time: {}",
                    os_error(&e, path)
                )
            })?);
            source = Some(FILESYSTEM_CREATED_SOURCE);
        }
        Ok(SelectedRun {
            course_id: self.course.clone(),
            filename: self.filename.clone(),
            source_fps: self.fps,
            markers: read_marker_csv(&self.markers)?,
            clip_id: self.clip_id.clone(),
            run_date: self.date.clone(),
            capture_time,
            capture_time_source: source.map(str::to_owned),
            source_path: self.source_path.clone(),
        })
    }
}

/// The file's ctime, as the Python tool reads it (`st_ctime`), in UTC.
fn changed_time(path: &str) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    let seconds = std::os::unix::fs::MetadataExt::ctime(&meta);
    #[cfg(not(unix))]
    let seconds = meta
        .created()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let dt = chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default();
    Ok(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// An OS error as Python prints one: `[Errno 2] No such file or directory: 'x'`.
fn os_error(e: &std::io::Error, path: &str) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => {
            let message = text
                .strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text);
            format!("[Errno {code}] {message}: '{path}'")
        }
        None => text,
    }
}

/// Reads `name,frame` rows, as Python's `csv.DictReader` does.
pub fn read_marker_csv(path: &str) -> Result<Vec<RawMarker>, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("could not read marker CSV {path}: {}", os_error(&e, path)))?;
    let text = String::from_utf8(bytes).map_err(|e| {
        format!(
            "could not read marker CSV {path}: 'utf-8' codec can't decode byte at position {}",
            e.utf8_error().valid_up_to()
        )
    })?;
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut records = reader.records();
    let header = match records.next() {
        Some(r) => r.map_err(|e| format!("{path}: {e}"))?,
        None => csv::StringRecord::new(),
    };
    // A repeated column name: the last one wins, as in a dict.
    let column = |name: &str| {
        header
            .iter()
            .enumerate()
            .filter(|(_, h)| *h == name)
            .map(|(i, _)| i)
            .last()
    };
    let (name_col, frame_col) = (column("name"), column("frame"));
    let missing: Vec<&str> = [("frame", frame_col), ("name", name_col)]
        .iter()
        .filter(|(_, c)| c.is_none())
        .map(|(n, _)| *n)
        .collect();
    let (Some(name_col), Some(frame_col)) = (name_col, frame_col) else {
        return Err(format!(
            "marker CSV missing columns: {}",
            missing.join(", ")
        ));
    };

    let mut markers = Vec::new();
    // Rows are numbered from 2 (after the header); blank lines aren't rows.
    for (index, record) in records.enumerate() {
        let record = record.map_err(|e| format!("{path}: {e}"))?;
        let row = index + 2;
        let name = record.get(name_col).unwrap_or("").trim();
        if name.is_empty() {
            return Err(format!("{path}: row {row} missing marker name"));
        }
        let frame = record.get(frame_col);
        let parsed = frame.and_then(|f| py::to_int(&Value::String(f.into())));
        let Some(frame) = parsed else {
            let shown = frame.map(|f| Value::String(f.into()));
            return Err(format!(
                "{path}: row {row} invalid frame {}",
                py::repr(shown.as_ref())
            ));
        };
        markers.push(RawMarker::new(name, frame));
    }
    Ok(markers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv_file(body: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.csv");
        std::fs::write(&path, body).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[test]
    fn reads_markers() {
        let (_d, p) = csv_file("name,frame\nStart, 100\n\nS1,+220\nFinish,\"700\"\n");
        let m = read_marker_csv(&p).unwrap();
        let got: Vec<(&str, i64)> = m.iter().map(|m| (m.name.as_str(), m.frame)).collect();
        assert_eq!(got, [("Start", 100), ("S1", 220), ("Finish", 700)]);
    }

    #[test]
    fn errors_read_as_python() {
        let (_d, p) = csv_file("label,frame\n");
        assert_eq!(
            read_marker_csv(&p).unwrap_err(),
            "marker CSV missing columns: name"
        );
        let (_d, p) = csv_file("x\n");
        assert_eq!(
            read_marker_csv(&p).unwrap_err(),
            "marker CSV missing columns: frame, name"
        );
        let (_d, p) = csv_file("name,frame\nStart,0\n ,5\n");
        assert_eq!(
            read_marker_csv(&p).unwrap_err(),
            format!("{p}: row 3 missing marker name")
        );
        let (_d, p) = csv_file("name,frame\nStart,1.5\n");
        assert_eq!(
            read_marker_csv(&p).unwrap_err(),
            format!("{p}: row 2 invalid frame '1.5'")
        );
        let (_d, p) = csv_file("name,frame\nStart\n");
        assert_eq!(
            read_marker_csv(&p).unwrap_err(),
            format!("{p}: row 2 invalid frame None")
        );
        assert_eq!(
            read_marker_csv("/nonexistent/m.csv").unwrap_err(),
            "could not read marker CSV /nonexistent/m.csv: [Errno 2] No such file or directory: '/nonexistent/m.csv'"
        );
    }
}

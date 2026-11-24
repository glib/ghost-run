//! Finding the stored run a clip belongs to: by Resolve clip id, then by
//! fingerprint (filename + marker hash), then by filename and exact frames.

use sha2::{Digest, Sha256};

use crate::capture_time;
use crate::models::{MarkerFrames, RunRecord};
use crate::py;

/// First 16 hex digits of SHA-256 over the frames as compact, key-sorted JSON:
/// byte-for-byte what the Python tool hashes (`json.dumps(..., separators=(",", ":"))`).
pub fn marker_snapshot_hash(frames: &MarkerFrames) -> String {
    let body: Vec<String> = frames
        .iter()
        .map(|(name, frame)| format!("{}:{frame}", py::json_string(name)))
        .collect();
    let digest = Sha256::digest(format!("{{{}}}", body.join(",")).as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn clip_fingerprint(filename: &str, frames: &MarkerFrames) -> String {
    format!("{filename}:{}", marker_snapshot_hash(frames))
}

pub fn find_matching_run<'a>(
    runs: &'a [RunRecord],
    course_id: &str,
    filename: &str,
    frames: &MarkerFrames,
    clip_id: Option<&str>,
) -> Option<&'a RunRecord> {
    let course_runs: Vec<&RunRecord> = runs.iter().filter(|r| r.course_id == course_id).collect();
    if let Some(clip_id) = clip_id.filter(|c| !c.is_empty()) {
        let by_clip: Vec<&RunRecord> = course_runs
            .iter()
            .copied()
            .filter(|r| r.clip_id.as_deref() == Some(clip_id))
            .collect();
        if let Some(run) = earliest(&by_clip) {
            return Some(run);
        }
    }
    let fingerprint = clip_fingerprint(filename, frames);
    let by_fingerprint: Vec<&RunRecord> = course_runs
        .iter()
        .copied()
        .filter(|r| r.fingerprint.as_deref() == Some(fingerprint.as_str()))
        .collect();
    if let Some(run) = earliest(&by_fingerprint) {
        return Some(run);
    }
    let by_snapshot: Vec<&RunRecord> = course_runs
        .iter()
        .copied()
        .filter(|r| r.filename == filename && &r.marker_frames == frames)
        .collect();
    earliest(&by_snapshot)
}

/// The order runs are compared in: capture time, else commit time, else date.
pub fn run_order_time(run: &RunRecord) -> String {
    capture_time::effective(run.capture_time.as_deref(), &run.date)
        .or_else(|| run.committed_at.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| run.date.clone())
}

fn earliest<'a>(runs: &[&'a RunRecord]) -> Option<&'a RunRecord> {
    runs.iter()
        .copied()
        .min_by(|a, b| (run_order_time(a), &a.id).cmp(&(run_order_time(b), &b.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(items: &[(&str, i64)]) -> MarkerFrames {
        items.iter().map(|(n, f)| ((*n).to_owned(), *f)).collect()
    }

    fn run(id: &str, f: &MarkerFrames, clip_id: Option<&str>) -> RunRecord {
        let mut r = RunRecord::new(id, "course", "2026-05-31", "GX010123.MP4", 100.0, f.clone());
        r.clip_id = clip_id.map(str::to_owned);
        r.fingerprint = Some(clip_fingerprint("GX010123.MP4", f));
        r.committed_at = Some("2026-05-31T10:00:00Z".into());
        r
    }

    #[test]
    fn hash_matches_the_python_tool() {
        // From examples/timer_db.yaml, written by the Python tool.
        let f = frames(&[
            ("Start", 100),
            ("S1", 220),
            ("S2", 360),
            ("S3", 515),
            ("Finish", 700),
        ]);
        assert_eq!(
            clip_fingerprint("GX010123.MP4", &f),
            "GX010123.MP4:129979c14aa405e2"
        );
    }

    #[test]
    fn prefers_clip_id_then_fingerprint() {
        let a = frames(&[("Start", 0), ("S1", 100), ("Finish", 300)]);
        let b = frames(&[("Start", 0), ("S1", 80), ("Finish", 280)]);
        let runs = [
            run("run_a", &a, Some("clip-a")),
            run("run_b", &b, Some("clip-b")),
        ];
        let m = |clip| find_matching_run(&runs, "course", "GX010123.MP4", &b, clip).unwrap();
        assert_eq!(m(Some("clip-a")).id, "run_a");
        assert_eq!(m(None).id, "run_b");
        assert_eq!(marker_snapshot_hash(&a).len(), 16);
    }

    #[test]
    fn falls_back_to_filename_and_exact_frames() {
        let a = frames(&[("Start", 0), ("S1", 100), ("Finish", 300)]);
        let mut r = run("run_a", &a, None);
        r.fingerprint = None;
        let runs = [r];
        assert_eq!(
            find_matching_run(&runs, "course", "GX010123.MP4", &a, None)
                .unwrap()
                .id,
            "run_a"
        );
        assert!(find_matching_run(&runs, "other", "GX010123.MP4", &a, None).is_none());
    }
}

//! Rendering a run's overlay to a file: the frames from a second before Start
//! to a few seconds after Finish, split across ffmpeg processes as
//! [`encode::plan`] allows, joined, and given the clip's timecode so it lines
//! up with the clip in an editor. Used by `rtimer render` and the app.
//!
//! A `.mov` is written to a hidden `.<name>.partial.mov` beside it and renamed
//! when complete, so a stopped or failed render never leaves a file that looks
//! finished.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use timer_core::overlay::OverlayPayload;

use crate::FrameRenderer;
use crate::encode::{
    self, EncodeError, Encoder, FrameRate, Output, Plan, SourceInfo, Timecode, concat,
};

/// The smallest stretch of frames worth its own ffmpeg.
const MIN_CHUNK_FRAMES: i64 = 120;

/// What to render.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    pub payload: &'a OverlayPayload,
    /// `.mov` for ProRes 4444 with alpha; a `%05d` pattern for an image sequence.
    pub out: &'a Path,
    /// The clip the overlay goes over, probed (its size, rate and timecode are used).
    pub source: Option<&'a SourceInfo>,
    /// The frame size without a source.
    pub size: (u32, u32),
    /// Seconds of empty frames before Start.
    pub pre_roll: f64,
    /// Seconds the final times stay up after Finish.
    pub hold: f64,
    /// ffmpeg processes and threads each; `None` plans them from cores and memory.
    pub jobs: Option<usize>,
    pub threads: Option<usize>,
}

impl<'a> Spec<'a> {
    pub fn new(payload: &'a OverlayPayload, out: &'a Path, source: Option<&'a SourceInfo>) -> Self {
        Self {
            payload,
            out,
            source,
            size: (3840, 2160),
            pre_roll: 1.0,
            hold: 5.0,
            jobs: None,
            threads: None,
        }
    }
}

/// What was rendered.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Clip frames covered, inclusive.
    pub first: i64,
    pub last: i64,
    /// The first frame's timecode.
    pub timecode: String,
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate,
    pub plan: Plan,
    pub seconds: f64,
}

impl Report {
    pub fn frames(&self) -> i64 {
        self.last - self.first + 1
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(
        "the run is timed at {run} fps but the clip is {clip} fps: the timer would run at the wrong speed"
    )]
    FrameRate { run: String, clip: String },
    #[error("{0}")]
    Io(String),
    #[error("stopped")]
    Cancelled,
}

/// Checks the run's fps is the clip's (59.94 matches 60000/1001).
pub fn check_fps(fps: f64, rate: FrameRate) -> Result<(), RenderError> {
    let actual = rate.fps();
    if ((fps - actual) / actual).abs() <= 1e-4 {
        return Ok(());
    }
    Err(RenderError::FrameRate {
        run: trim(fps),
        clip: trim(actual),
    })
}

/// A rate as people write it: 29.97, 59.94, 23.976, 50.
pub fn trim(fps: f64) -> String {
    let s = format!("{fps:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Renders `spec`; `progress(done, total)` is called as frames are drawn (from
/// the encoding threads), and setting `cancel` stops it.
pub fn render(
    spec: &Spec<'_>,
    progress: &(dyn Fn(u64, u64) + Sync),
    cancel: &AtomicBool,
) -> Result<Report, RenderError> {
    let job = Job::new(spec)?;
    let (start, finish) = (spec.payload.start_frame, spec.payload.finish_frame);
    let fps = spec.payload.source_fps;
    let first = (start - (spec.pre_roll.max(0.0) * fps).round() as i64).max(0);
    let mut last = finish + (spec.hold.max(0.0) * fps).round() as i64;
    if let Some(frames) = spec.source.and_then(|i| i.frames) {
        last = last.min(frames - 1).max(first);
    }
    let source_tc = match spec.source.and_then(|i| i.timecode.as_deref()) {
        Some(tc) => Timecode::parse(tc, job.rate)?,
        None => Timecode {
            frames: 0,
            drop_frame: false,
        },
    };
    let timecode = source_tc.plus(first).format(job.rate);
    let count = last - first + 1;
    let mut plan = job.plan;
    plan.jobs = plan.jobs.min((count / MIN_CHUNK_FRAMES).max(1) as usize);

    let began = Instant::now();
    let done = AtomicU64::new(0);
    let tick = || {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(8) || n == count as u64 {
            progress(n, count as u64);
        }
    };
    let sequence = spec.out.to_string_lossy().contains('%');
    if sequence {
        let bounds = split(first, count, plan.jobs);
        job.encode_all(&bounds, |_| spec.out.to_owned(), &tick, cancel)?;
    } else {
        let partial = partial_path(spec.out);
        let parts_dir = partial.with_extension("parts");
        let result = if plan.jobs == 1 {
            job.encode(&partial, Some(&timecode), first, last, &tick, &|| {
                cancel.load(Ordering::Relaxed)
            })
        } else {
            std::fs::create_dir_all(&parts_dir).map_err(|e| {
                RenderError::Io(format!("could not create {}: {e}", parts_dir.display()))
            })?;
            let part = |k: usize| parts_dir.join(format!("part_{k:03}.mov"));
            let bounds = split(first, count, plan.jobs);
            job.encode_all(&bounds, part, &tick, cancel).and_then(|()| {
                let parts: Vec<PathBuf> = (0..bounds.len()).map(part).collect();
                Ok(concat(&parts, &partial, Some(&timecode))?)
            })
        };
        let _ = std::fs::remove_dir_all(&parts_dir);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        std::fs::rename(&partial, spec.out).map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            RenderError::Io(format!("could not write {}: {e}", spec.out.display()))
        })?;
    }
    Ok(Report {
        first,
        last,
        timecode,
        width: job.width,
        height: job.height,
        rate: job.rate,
        plan,
        seconds: began.elapsed().as_secs_f64(),
    })
}

/// Writes clip frame `n` alone, as an image (or a one-frame video).
pub fn still(spec: &Spec<'_>, n: i64) -> Result<(), RenderError> {
    let job = Job::new(spec)?;
    job.encode(spec.out, None, n, n, &|| {}, &|| false)
}

/// `<dir>/.<name>.partial.mov`.
pub fn partial_path(out: &Path) -> PathBuf {
    let name = out.file_stem().unwrap_or_default().to_string_lossy();
    out.with_file_name(format!(".{name}.partial.mov"))
}

/// `count` frames from `first` in `jobs` contiguous stretches.
fn split(first: i64, count: i64, jobs: usize) -> Vec<(i64, i64)> {
    let jobs = jobs.max(1) as i64;
    (0..jobs)
        .map(|k| (first + count * k / jobs, first + count * (k + 1) / jobs - 1))
        .collect()
}

/// What every stretch of the overlay is drawn and encoded with.
struct Job<'a> {
    payload: &'a OverlayPayload,
    width: u32,
    height: u32,
    rate: FrameRate,
    plan: Plan,
}

impl<'a> Job<'a> {
    fn new(spec: &Spec<'a>) -> Result<Self, RenderError> {
        let (width, height) = spec.source.map_or(spec.size, |i| (i.width, i.height));
        let rate = spec
            .source
            .map_or_else(|| FrameRate::from_fps(spec.payload.source_fps), |i| i.rate);
        if spec.source.is_some() {
            check_fps(spec.payload.source_fps, rate)?;
        }
        let planned = encode::plan(width, height);
        Ok(Self {
            payload: spec.payload,
            width,
            height,
            rate,
            plan: Plan {
                jobs: spec.jobs.unwrap_or(planned.jobs).max(1),
                threads: spec.threads.unwrap_or(planned.threads).max(1),
            },
        })
    }

    /// Clip frames `from..=to` into `path`, panel only (ffmpeg pads it out).
    /// Gives up as soon as `stopped()` says so.
    fn encode(
        &self,
        path: &Path,
        timecode: Option<&str>,
        from: i64,
        to: i64,
        tick: &(dyn Fn() + Sync),
        stopped: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RenderError> {
        let mut renderer = FrameRenderer::new(self.payload.clone(), self.width, self.height);
        let mut panel = renderer.blank_panel();
        let mut encoder = Encoder::start(&Output {
            path,
            width: self.width,
            height: self.height,
            region: renderer.layout().panel,
            rate: self.rate,
            timecode,
            first: from,
            threads: self.plan.threads,
        })?;
        for n in from..=to {
            if stopped() {
                // Closing ffmpeg's input ends it; the caller removes what it wrote.
                let _ = encoder.finish();
                return Err(RenderError::Cancelled);
            }
            renderer.render_panel(n, &mut panel);
            encoder.write_frame(&panel)?;
            tick();
        }
        Ok(encoder.finish()?)
    }

    /// Each `(from, to)` stretch into `path(index)`, all at once; one failing
    /// stops the rest.
    fn encode_all(
        &self,
        bounds: &[(i64, i64)],
        path: impl Fn(usize) -> PathBuf + Sync,
        tick: &(dyn Fn() + Sync),
        cancel: &AtomicBool,
    ) -> Result<(), RenderError> {
        let failed = AtomicBool::new(false);
        let stopped = || cancel.load(Ordering::Relaxed) || failed.load(Ordering::Relaxed);
        let results: Vec<Result<(), RenderError>> = std::thread::scope(|scope| {
            let workers: Vec<_> = bounds
                .iter()
                .enumerate()
                .map(|(k, &(from, to))| {
                    let (path, stopped, failed) = (&path, &stopped, &failed);
                    scope.spawn(move || {
                        let result = self.encode(&path(k), None, from, to, tick, stopped);
                        if matches!(result, Err(ref e) if !matches!(e, RenderError::Cancelled)) {
                            failed.store(true, Ordering::Relaxed);
                        }
                        result
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("encoder thread"))
                .collect()
        });
        if cancel.load(Ordering::Relaxed) {
            return Err(RenderError::Cancelled);
        }
        // The real failure, not the stretches it stopped.
        let mut stopped_early = false;
        for r in results {
            match r {
                Err(RenderError::Cancelled) => stopped_early = true,
                Err(e) => return Err(e),
                Ok(()) => {}
            }
        }
        if stopped_early {
            return Err(RenderError::Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_must_match_the_clip() {
        let ntsc = FrameRate::from_fps(29.97);
        assert!(check_fps(29.97, ntsc).is_ok());
        assert!(check_fps(30000.0 / 1001.0, ntsc).is_ok());
        assert!(check_fps(59.94, ntsc).is_err());
        // 60 against 59.94 is 0.1% off: 0.09 s over a 90 s run.
        assert!(check_fps(60.0, FrameRate::from_fps(59.94)).is_err());
        assert_eq!(trim(50.0), "50");
        assert_eq!(trim(23.976), "23.976");
    }

    #[test]
    fn stretches_cover_every_frame_once() {
        assert_eq!(split(10, 100, 3), [(10, 42), (43, 75), (76, 109)]);
        assert_eq!(split(0, 5, 1), [(0, 4)]);
    }

    #[test]
    fn partial_files_are_hidden_beside_the_output() {
        assert_eq!(
            partial_path(Path::new("/o/clip.overlay.mov")),
            Path::new("/o/.clip.overlay.partial.mov")
        );
    }
}

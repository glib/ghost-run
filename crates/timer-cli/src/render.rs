//! `rtimer render`: the overlay as one transparent video file to lay over the
//! clip, instead of a Fusion comp.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Args;
use timer_core::overlay::OverlayPayload;
use timer_core::overlay::OverlayPayload as Payload;
use timer_render::FrameRenderer;
use timer_render::encode::{
    EncodeError, Encoder, FrameRate, Output, Plan, Timecode, concat, plan, probe,
};

/// The smallest stretch of frames worth its own ffmpeg.
const MIN_CHUNK_FRAMES: i64 = 120;

#[derive(Args)]
pub struct RenderArgs {
    /// Output file: .mov for ProRes 4444 with alpha; .png with --still
    #[arg(long)]
    out: PathBuf,
    /// The clip the overlay goes over: its size, frame rate and timecode are used
    #[arg(long)]
    source: Option<PathBuf>,
    /// Frame size without --source
    #[arg(long, default_value = "3840x2160", value_parser = parse_size)]
    size: (u32, u32),
    /// Seconds of empty frames before the start marker
    #[arg(long, default_value_t = 1.0)]
    pre_roll: f64,
    /// Seconds to hold the final times after the finish marker
    #[arg(long, default_value_t = 5.0)]
    hold: f64,
    /// Write only this clip frame, as an image
    #[arg(long)]
    still: Option<i64>,
    /// ffmpeg processes to split the render across (default: planned from cores and free memory)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,
    /// Encoder threads in each (default: planned from cores and free memory)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    threads: Option<u32>,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or("size must be WIDTHxHEIGHT")?;
    let parse = |v: &str| {
        v.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0 && n % 2 == 0)
    };
    match (parse(w), parse(h)) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err("size must be WIDTHxHEIGHT, both even".into()),
    }
}

pub fn render(
    payload: OverlayPayload,
    args: &RenderArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = args.source.as_deref().map(probe).transpose()?;
    let (width, height) = info.as_ref().map_or(args.size, |i| (i.width, i.height));
    let rate = info
        .as_ref()
        .map_or_else(|| FrameRate::from_fps(payload.source_fps), |i| i.rate);
    let fps = payload.source_fps;
    if let (Some(info), Some(source)) = (&info, &args.source) {
        check_fps(fps, info.rate, source)?;
    }
    let (start, finish) = (payload.start_frame, payload.finish_frame);
    let planned = plan(width, height);
    let mut plan = Plan {
        jobs: args.jobs.map_or(planned.jobs, |j| j as usize),
        threads: args.threads.map_or(planned.threads, |t| t as usize),
    };
    let job = Job {
        payload: &payload,
        width,
        height,
        rate,
        threads: plan.threads,
    };

    if let Some(n) = args.still {
        job.encode(&args.out, None, n, n)?;
        println!("Wrote {} (clip frame {n})", args.out.display());
        return Ok(());
    }

    let first = (start - (args.pre_roll.max(0.0) * fps).round() as i64).max(0);
    let mut last = finish + (args.hold.max(0.0) * fps).round() as i64;
    if let Some(frames) = info.as_ref().and_then(|i| i.frames) {
        last = last.min(frames - 1).max(first);
    }
    let source_tc = match info.as_ref().and_then(|i| i.timecode.as_deref()) {
        Some(tc) => Timecode::parse(tc, rate)?,
        None => Timecode {
            frames: 0,
            drop_frame: false,
        },
    };
    let timecode = source_tc.plus(first).format(rate);

    let began = Instant::now();
    let count = last - first + 1;
    plan.jobs = plan.jobs.min((count / MIN_CHUNK_FRAMES).max(1) as usize);
    let sequence = args.out.to_string_lossy().contains('%');
    if plan.jobs == 1 {
        job.encode(&args.out, Some(&timecode), first, last)?;
    } else {
        // Contiguous stretches, encoded at once and joined.
        let jobs = plan.jobs as i64;
        let bounds: Vec<(i64, i64)> = (0..jobs)
            .map(|k| (first + count * k / jobs, first + count * (k + 1) / jobs - 1))
            .collect();
        if sequence {
            job.encode_all(&bounds, |_| args.out.clone())?;
        } else {
            let parts_dir = args.out.with_extension("parts");
            std::fs::create_dir_all(&parts_dir)
                .map_err(|e| format!("could not create {}: {e}", parts_dir.display()))?;
            let part = |k: usize| parts_dir.join(format!("part_{k:03}.mov"));
            let result = job.encode_all(&bounds, part).and_then(|()| {
                let parts: Vec<PathBuf> = (0..bounds.len()).map(part).collect();
                concat(&parts, &args.out, Some(&timecode))
            });
            let _ = std::fs::remove_dir_all(&parts_dir);
            result?;
        }
    }
    let secs = began.elapsed().as_secs_f64();
    println!(
        "Wrote {}: {count} frames ({width}x{height}, {:.3} fps) in {secs:.1} s ({:.0} frames/s; {} ffmpeg x {} threads)",
        args.out.display(),
        rate.fps(),
        count as f64 / secs.max(1e-9),
        plan.jobs,
        plan.threads,
    );
    if sequence {
        println!("Its frames are numbered by clip frame, from {first} to {last}.");
    } else {
        println!(
            "It starts at clip frame {first}, timecode {timecode}. Put it on the track above the \
             clip at that frame, or select both and use Auto Align Clips > Based on Timecode."
        );
    }
    Ok(())
}

/// What every stretch of the overlay is drawn and encoded with.
struct Job<'a> {
    payload: &'a Payload,
    width: u32,
    height: u32,
    rate: FrameRate,
    threads: usize,
}

impl Job<'_> {
    /// Clip frames `from..=to` into `path`, panel only (ffmpeg pads it out).
    fn encode(
        &self,
        path: &Path,
        timecode: Option<&str>,
        from: i64,
        to: i64,
    ) -> Result<(), EncodeError> {
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
            threads: self.threads,
        })?;
        for n in from..=to {
            renderer.render_panel(n, &mut panel);
            encoder.write_frame(&panel)?;
        }
        encoder.finish()
    }

    /// Each `(from, to)` stretch into `path(index)`, all at once.
    fn encode_all(
        &self,
        bounds: &[(i64, i64)],
        path: impl Fn(usize) -> PathBuf + Sync,
    ) -> Result<(), EncodeError> {
        std::thread::scope(|scope| {
            let workers: Vec<_> = bounds
                .iter()
                .enumerate()
                .map(|(k, &(from, to))| {
                    let path = &path;
                    scope.spawn(move || self.encode(&path(k), None, from, to))
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("encoder thread"))
                .collect::<Result<Vec<()>, _>>()
                .map(|_| ())
        })
    }
}

/// `--fps` turns marker frames into seconds; if it isn't the clip's rate, the
/// timer runs fast or slow against the video. 59.94 matches 60000/1001.
fn check_fps(fps: f64, rate: FrameRate, source: &Path) -> Result<(), String> {
    let actual = rate.fps();
    if ((fps - actual) / actual).abs() <= 1e-4 {
        return Ok(());
    }
    Err(format!(
        "--fps {} doesn't match {}, which is {} fps: the timer would run at the wrong speed. Use --fps {}",
        trim(fps),
        source.display(),
        trim(actual),
        trim(actual)
    ))
}

/// A rate as people write it: 29.97, 59.94, 23.976, 50.
fn trim(fps: f64) -> String {
    let s = format!("{fps:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_must_match_the_source() {
        let clip = Path::new("clip.mp4");
        let ntsc = FrameRate::from_fps(29.97);
        assert!(check_fps(29.97, ntsc, clip).is_ok());
        assert!(check_fps(30000.0 / 1001.0, ntsc, clip).is_ok());
        assert_eq!(
            check_fps(59.94, ntsc, clip).unwrap_err(),
            "--fps 59.94 doesn't match clip.mp4, which is 29.97 fps: the timer would run at \
             the wrong speed. Use --fps 29.97"
        );
        // 60 against 59.94 is 0.1% off: 0.09 s over a 90 s run.
        assert!(check_fps(60.0, FrameRate::from_fps(59.94), clip).is_err());
        assert_eq!(trim(50.0), "50");
        assert_eq!(trim(23.976), "23.976");
    }
}

//! `rtimer render`: the overlay as one transparent video file to lay over the
//! clip, instead of a Fusion comp.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use clap::Args;
use timer_core::overlay::OverlayPayload;
use timer_render::encode::{FrameRate, probe};
use timer_render::job::{self, Spec, trim};

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
    if let (Some(info), Some(source)) = (&info, &args.source) {
        check_fps(payload.source_fps, info.rate, source)?;
    }
    let spec = Spec {
        size: args.size,
        pre_roll: args.pre_roll,
        hold: args.hold,
        jobs: args.jobs.map(|j| j as usize),
        threads: args.threads.map(|t| t as usize),
        ..Spec::new(&payload, &args.out, info.as_ref())
    };
    if let Some(n) = args.still {
        job::still(&spec, n)?;
        println!("Wrote {} (clip frame {n})", args.out.display());
        return Ok(());
    }
    let r = job::render(&spec, &|_, _| {}, &AtomicBool::new(false))?;
    println!(
        "Wrote {}: {} frames ({}x{}, {:.3} fps) in {:.1} s ({:.0} frames/s; {} ffmpeg x {} threads)",
        args.out.display(),
        r.frames(),
        r.width,
        r.height,
        r.rate.fps(),
        r.seconds,
        r.frames() as f64 / r.seconds.max(1e-9),
        r.plan.jobs,
        r.plan.threads,
    );
    if args.out.to_string_lossy().contains('%') {
        println!(
            "Its frames are numbered by clip frame, from {} to {}.",
            r.first, r.last
        );
    } else {
        println!(
            "It starts at clip frame {}, timecode {}. Put it on the track above the \
             clip at that frame, or select both and use Auto Align Clips > Based on Timecode.",
            r.first, r.timecode
        );
    }
    Ok(())
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

//! Writing frames to video with ffmpeg (ProRes 4444 with alpha), reading a
//! source clip's size, rate and timecode with ffprobe, and timecode maths so
//! the overlay can carry the clip's timecode (Resolve can then line it up by
//! timecode, however the clip is trimmed).

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("could not run {program}: {source} (is ffmpeg installed and on PATH?)")]
    Spawn {
        program: &'static str,
        source: std::io::Error,
    },
    #[error("{program} failed: {message}")]
    Failed {
        program: &'static str,
        message: String,
    },
    #[error("could not read {path}: {message}")]
    Probe { path: String, message: String },
    #[error("invalid timecode {0:?}")]
    Timecode(String),
}

/// A frame rate as a fraction, so 59.94 is exactly 60000/1001.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    /// The exact rate a camera's nominal rate stands for: 29.97 → 30000/1001.
    pub fn from_fps(fps: f64) -> Self {
        let ntsc = fps * 1.001;
        if (ntsc - ntsc.round()).abs() < 0.005 && (fps - fps.round()).abs() > 0.001 {
            return Self {
                num: ntsc.round() as u32 * 1000,
                den: 1001,
            };
        }
        if (fps - fps.round()).abs() < 1e-6 {
            return Self {
                num: fps.round() as u32,
                den: 1,
            };
        }
        Self {
            num: (fps * 1000.0).round() as u32,
            den: 1000,
        }
    }

    /// `"60000/1001"`, as ffprobe prints rates.
    pub fn parse(s: &str) -> Option<Self> {
        let (num, den) = s.split_once('/').unwrap_or((s, "1"));
        let rate = Self {
            num: num.trim().parse().ok()?,
            den: den.trim().parse().ok()?,
        };
        (rate.num > 0 && rate.den > 0).then_some(rate)
    }

    pub fn fps(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// Frames per timecode second: 30 for 29.97.
    pub fn nominal(self) -> u32 {
        self.fps().round() as u32
    }

    fn arg(self) -> String {
        format!("{}/{}", self.num, self.den)
    }
}

/// `HH:MM:SS:FF`, or `HH:MM:SS;FF` for drop-frame (29.97, 59.94).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    /// Frames since 00:00:00:00.
    pub frames: i64,
    pub drop_frame: bool,
}

impl Timecode {
    pub fn parse(s: &str, rate: FrameRate) -> Result<Self, EncodeError> {
        let bad = || EncodeError::Timecode(s.to_owned());
        let drop_frame = s.contains([';', '.']);
        let parts: Vec<i64> = s
            .split([':', ';', '.'])
            .map(|p| p.parse::<i64>().map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        let [h, m, sec, f] = parts[..] else {
            return Err(bad());
        };
        let fps = i64::from(rate.nominal());
        let mut frames = ((h * 60 + m) * 60 + sec) * fps + f;
        if drop_frame {
            let minutes = h * 60 + m;
            frames -= drop_count(rate) * (minutes - minutes / 10);
        }
        Ok(Self { frames, drop_frame })
    }

    pub fn plus(self, frames: i64) -> Self {
        Self {
            frames: (self.frames + frames).max(0),
            ..self
        }
    }

    pub fn format(self, rate: FrameRate) -> String {
        let fps = i64::from(rate.nominal());
        let mut frames = self.frames;
        if self.drop_frame {
            let drop = drop_count(rate);
            let per_minute = fps * 60 - drop;
            let per_ten = per_minute * 10 + drop;
            let (tens, rest) = (frames / per_ten, frames % per_ten);
            frames += 9 * drop * tens;
            if rest > drop {
                frames += drop * ((rest - drop) / per_minute);
            }
        }
        let f = frames % fps;
        let s = frames / fps % 60;
        let m = frames / (fps * 60) % 60;
        let h = frames / (fps * 3600) % 24;
        let sep = if self.drop_frame { ';' } else { ':' };
        format!("{h:02}:{m:02}:{s:02}{sep}{f:02}")
    }
}

/// Frame numbers skipped each minute (but every tenth) in drop-frame timecode.
fn drop_count(rate: FrameRate) -> i64 {
    i64::from(rate.nominal()) / 15 // 2 at 29.97, 4 at 59.94
}

/// What the overlay needs to know about the clip it goes over.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate,
    pub frames: Option<i64>,
    pub timecode: Option<String>,
}

pub fn probe(path: &Path) -> Result<SourceInfo, EncodeError> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=width,height,r_frame_rate,nb_frames:stream_tags=timecode:format_tags=timecode")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .map_err(|source| EncodeError::Spawn {
            program: "ffprobe",
            source,
        })?;
    let err = |message: String| EncodeError::Probe {
        path: path.display().to_string(),
        message,
    };
    if !output.status.success() {
        return Err(err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_owned()));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| err(e.to_string()))?;
    let stream = &json["streams"][0];
    let dim = |key: &str| {
        stream[key]
            .as_u64()
            .map(|v| v as u32)
            .ok_or_else(|| err(format!("no video {key}")))
    };
    let tag = |v: &serde_json::Value| v["tags"]["timecode"].as_str().map(str::to_owned);
    Ok(SourceInfo {
        width: dim("width")?,
        height: dim("height")?,
        rate: stream["r_frame_rate"]
            .as_str()
            .and_then(FrameRate::parse)
            .ok_or_else(|| err("no frame rate".into()))?,
        frames: stream["nb_frames"].as_str().and_then(|n| n.parse().ok()),
        timecode: tag(stream).or_else(|| tag(&json["format"])),
    })
}

/// An ffmpeg process taking raw RGBA frames on stdin.
pub struct Encoder {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frame_bytes: usize,
}

impl Encoder {
    /// Starts writing `path`: ProRes 4444 with alpha for `.mov`, else
    /// whatever ffmpeg picks for the extension (e.g. `frame_%05d.png`). The
    /// first frame gets `timecode` in a `.mov`, number `first` in a sequence.
    pub fn start(
        path: &Path,
        width: u32,
        height: u32,
        rate: FrameRate,
        timecode: Option<&str>,
        first: i64,
    ) -> Result<Self, EncodeError> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{width}x{height}")])
            .args(["-framerate", &rate.arg(), "-i", "-"]);
        let is_mov = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mov"));
        if is_mov {
            // ProRes 4444 (profile 4, `ap4h`) keeps the alpha. prores_aw is
            // about 4x faster than prores_ks at 4K, for the same size.
            cmd.args(["-c:v", "prores_aw", "-profile:v", "4"]).args([
                "-pix_fmt",
                "yuva444p10le",
                "-vendor",
                "apl0",
            ]);
        }
        if is_mov {
            if let Some(tc) = timecode {
                cmd.args(["-timecode", tc]);
            }
        } else if path.to_string_lossy().contains('%') {
            cmd.args(["-start_number", &first.to_string()]);
        }
        let mut child = cmd
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| EncodeError::Spawn {
                program: "ffmpeg",
                source,
            })?;
        let stdin = child.stdin.take();
        Ok(Self {
            child: Some(child),
            stdin,
            frame_bytes: width as usize * height as usize * 4,
        })
    }

    pub fn write_frame(&mut self, rgba: &[u8]) -> Result<(), EncodeError> {
        assert_eq!(rgba.len(), self.frame_bytes, "frame size");
        let Some(stdin) = self.stdin.as_mut() else {
            return self.finish_inner();
        };
        if stdin.write_all(rgba).is_err() {
            // ffmpeg exited early; `finish` reports why.
            return self.finish_inner();
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), EncodeError> {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> Result<(), EncodeError> {
        drop(self.stdin.take());
        let Some(child) = self.child.take() else {
            return Ok(());
        };
        let output = child
            .wait_with_output()
            .map_err(|source| EncodeError::Spawn {
                program: "ffmpeg",
                source,
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(EncodeError::Failed {
                program: "ffmpeg",
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_rates_are_exact() {
        assert_eq!(
            FrameRate::from_fps(59.94),
            FrameRate {
                num: 60000,
                den: 1001
            }
        );
        assert_eq!(
            FrameRate::from_fps(29.97),
            FrameRate {
                num: 30000,
                den: 1001
            }
        );
        assert_eq!(FrameRate::from_fps(23.976).num, 24000);
        assert_eq!(FrameRate::from_fps(50.0), FrameRate { num: 50, den: 1 });
        assert_eq!(
            FrameRate::parse("60000/1001"),
            Some(FrameRate::from_fps(59.94))
        );
        assert_eq!(FrameRate::parse("0/0"), None);
    }

    #[test]
    fn non_drop_timecode() {
        let rate = FrameRate::from_fps(50.0);
        let tc = Timecode::parse("01:00:00:00", rate).unwrap();
        assert_eq!(tc.frames, 180_000);
        assert_eq!(tc.plus(51).format(rate), "01:00:01:01");
    }

    #[test]
    fn drop_frame_timecode_skips_numbers_each_minute() {
        let rate = FrameRate::from_fps(29.97);
        let tc = Timecode::parse("00:00:59;29", rate).unwrap();
        assert_eq!(tc.plus(1).format(rate), "00:01:00;02");
        let ten = Timecode::parse("00:09:59;29", rate).unwrap();
        assert_eq!(ten.plus(1).format(rate), "00:10:00;00");
        // An hour of 29.97 drop-frame is 107,892 frames.
        assert_eq!(
            Timecode::parse("01:00:00;00", rate).unwrap().frames,
            107_892
        );

        let rate = FrameRate::from_fps(59.94);
        let tc = Timecode::parse("00:00:59;59", rate).unwrap();
        assert_eq!(tc.plus(1).format(rate), "00:01:00;04");
        for s in ["12:34:56;20", "00:10:00;00", "23:59:59;59"] {
            assert_eq!(Timecode::parse(s, rate).unwrap().format(rate), s);
        }
    }
}

//! What ffprobe says about a clip: enough to list it, time it and check its proxy.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction (`30000/1001`).
    pub fps_num: u32,
    pub fps_den: u32,
    /// Video frames, from the container (or counted when it doesn't say).
    pub frames: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub timecode: Option<String>,
    /// Display rotation in degrees (DJI cameras mounted upside down say -180).
    pub rotation: i32,
    /// The container's `creation_time` tag, as written.
    pub creation_time: Option<String>,
    pub has_audio: bool,
}

impl MediaInfo {
    pub fn fps(&self) -> f64 {
        f64::from(self.fps_num) / f64::from(self.fps_den.max(1))
    }
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg(
            "stream=codec_type,codec_name,width,height,r_frame_rate,nb_frames,duration\
             :stream_disposition=attached_pic:stream_tags=timecode,creation_time\
             :stream_side_data=rotation:format=duration:format_tags=timecode,creation_time",
        )
        .args(["-of", "json"])
        .arg(path)
        .output()
        .context("could not run ffprobe (is FFmpeg installed?)")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse(&serde_json::from_slice(&output.stdout)?)
}

fn parse(json: &Value) -> Result<MediaInfo> {
    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
        .ok_or_else(|| anyhow!("no video stream"))?;
    let (fps_num, fps_den) = video["r_frame_rate"]
        .as_str()
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some((n.parse().ok()?, d.parse().ok()?)))
        .filter(|(n, d): &(u32, u32)| *n > 0 && *d > 0)
        .ok_or_else(|| anyhow!("no frame rate"))?;
    let number = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let tag = |key: &str| {
        video["tags"][key]
            .as_str()
            .or_else(|| json["format"]["tags"][key].as_str())
            .map(str::to_owned)
    };
    let rotation = video["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|d| d["rotation"].as_i64())
        .unwrap_or(0) as i32;
    Ok(MediaInfo {
        codec: video["codec_name"].as_str().unwrap_or("").to_owned(),
        width: video["width"].as_u64().ok_or_else(|| anyhow!("no width"))? as u32,
        height: video["height"]
            .as_u64()
            .ok_or_else(|| anyhow!("no height"))? as u32,
        fps_num,
        fps_den,
        frames: video["nb_frames"].as_str().and_then(|n| n.parse().ok()),
        duration_seconds: number(&video["duration"])
            .or_else(|| number(&json["format"]["duration"])),
        timecode: tag("timecode"),
        rotation,
        creation_time: tag("creation_time"),
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
    })
}

/// Counts the video packets (one per frame), for containers that don't say.
pub fn count_frames(path: &Path) -> Result<i64> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets"])
        .args(["-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("could not run ffprobe")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_end_matches(',')
        .parse()
        .context("ffprobe gave no frame count")
}

/// The clip's frame count, counting packets when the container doesn't say.
pub fn frames(path: &Path, info: &MediaInfo) -> Result<i64> {
    match info.frames {
        Some(n) => Ok(n),
        None => count_frames(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_dji_style_probe() {
        let json: Value = serde_json::from_str(
            r#"{"streams":[
              {"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,
               "r_frame_rate":"30000/1001","nb_frames":"6806","duration":"227.093533",
               "disposition":{"attached_pic":0},
               "tags":{"timecode":"07:52:36;12","creation_time":"2026-08-17T07:52:36.000000Z"},
               "side_data_list":[{"rotation":-180}]},
              {"codec_type":"audio","codec_name":"aac"},
              {"codec_type":"video","codec_name":"mjpeg","width":960,"height":540,
               "r_frame_rate":"90000/1","disposition":{"attached_pic":1}}],
             "format":{"duration":"227.1"}}"#,
        )
        .unwrap();
        let info = parse(&json).unwrap();
        assert_eq!(info.codec, "hevc");
        assert_eq!((info.width, info.height), (3840, 2160));
        assert_eq!((info.fps_num, info.fps_den), (30000, 1001));
        assert_eq!(info.frames, Some(6806));
        assert_eq!(info.timecode.as_deref(), Some("07:52:36;12"));
        assert_eq!(info.rotation, -180);
        assert!(info.has_audio);
        assert!((info.fps() - 29.97).abs() < 0.001);
    }

    #[test]
    fn needs_a_real_video_stream() {
        let only_cover: Value = serde_json::from_str(
            r#"{"streams":[{"codec_type":"video","width":1,"height":1,"r_frame_rate":"1/1",
                "disposition":{"attached_pic":1}}],"format":{}}"#,
        )
        .unwrap();
        assert!(parse(&only_cover).is_err());
    }
}

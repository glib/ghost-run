//! A video's cards (`library::videos::Card`): a title, an end, or a course's
//! personal bests on the Tracks chart, drawn with cairo at the video's size
//! and made into a short ProRes clip by ffmpeg, so Resolve takes it like any
//! other clip. Captions (`Item::caption`) are drawn the same way on a
//! transparent frame, as ProRes 4444 with alpha, and go over their clip.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use gtk::cairo;
use library::videos::{Card, CardKind};
use timer_core::tracks::Point;

use crate::tracks_view::{CHART_INK, CHART_SURFACE};

/// The chart card's data: the course's runs, and its target.
pub type ChartData = (Vec<Point>, Option<f64>);

/// Makes `card` as a clip at `out`: `width` × `height`, `fps` (e.g.
/// `30000/1001`), as many frames as its seconds. Returns the frame count.
pub fn make(
    card: &Card,
    (width, height): (i32, i32),
    fps: &str,
    chart: Option<ChartData>,
    out: &Path,
) -> Result<i64> {
    let rate = rate(fps).context("the frame rate couldn't be read")?;
    let frames = ((card.seconds * rate).round() as i64).max(1);
    let image = draw(card, width, height, chart)?;
    encode(&image, (width, height), fps, frames, false, out)?;
    Ok(frames)
}

/// How long a caption shows at the start of its clip.
pub const CAPTION_SECONDS: f64 = 4.0;

/// Makes a caption as an overlay clip at `out`: `text` low in the frame on a
/// transparent background, for [`CAPTION_SECONDS`] or `at_most` frames,
/// whichever is less. Returns the frame count.
pub fn make_caption(
    text: &str,
    (width, height): (i32, i32),
    fps: &str,
    at_most: i64,
    out: &Path,
) -> Result<i64> {
    let rate = rate(fps).context("the frame rate couldn't be read")?;
    let frames = ((CAPTION_SECONDS * rate).round() as i64)
        .min(at_most)
        .max(1);
    let image = draw_caption(text, width, height)?;
    encode(&image, (width, height), fps, frames, true, out)?;
    Ok(frames)
}

/// One still `image` (BGRA rows) repeated for `frames` frames as ProRes:
/// 422 Proxy, or 4444 keeping the alpha.
fn encode(
    image: &[u8],
    (width, height): (i32, i32),
    fps: &str,
    frames: i64,
    alpha: bool,
    out: &Path,
) -> Result<()> {
    let (profile, pix_fmt) = if alpha {
        ("4", "yuva444p10le")
    } else {
        ("0", "yuv422p10le")
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
        .args(["-s", &format!("{width}x{height}"), "-framerate", fps])
        .args(["-i", "pipe:0"])
        .args(["-vf", &format!("loop=loop={}:size=1:start=0", frames - 1)])
        .args(["-frames:v", &frames.to_string(), "-r", fps])
        .args(["-c:v", "prores_ks", "-profile:v", profile])
        .args(["-pix_fmt", pix_fmt])
        .arg(out)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run ffmpeg")?;
    child
        .stdin
        .take()
        .context("no pipe to ffmpeg")?
        .write_all(image)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "ffmpeg couldn't make the card: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// A caption's picture: the text on a rounded dark panel, centred low in the
/// frame, everything else transparent.
fn draw_caption(text: &str, width: i32, height: i32) -> Result<Vec<u8>> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let (w, h) = (f64::from(width), f64::from(height));
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        let mut size = h / 20.0;
        cr.set_font_size(size);
        let mut ext = cr.text_extents(text)?;
        if ext.width() > w * 0.8 {
            size *= w * 0.8 / ext.width();
            cr.set_font_size(size);
            ext = cr.text_extents(text)?;
        }
        let pad = size * 0.6;
        let (bw, bh) = (ext.width() + 2.0 * pad, size + 2.0 * pad);
        let (x, y) = ((w - bw) / 2.0, h * 0.86 - bh);
        rounded(&cr, x, y, bw, bh, bh * 0.2);
        let (r, g, b) = CHART_SURFACE;
        cr.set_source_rgba(r, g, b, 0.82);
        cr.fill()?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(x + pad - ext.x_bearing(), y + pad + size * 0.82);
        cr.show_text(text)?;
    }
    pixels(surface, width, height)
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// A surface's pixels as rows of BGRA with no padding.
fn pixels(mut surface: cairo::ImageSurface, width: i32, height: i32) -> Result<Vec<u8>> {
    surface.flush();
    let stride = surface.stride() as usize;
    let row = width as usize * 4;
    let data = surface.data().context("reading the card's pixels")?;
    Ok(data
        .chunks(stride)
        .take(height as usize)
        .flat_map(|r| &r[..row])
        .copied()
        .collect())
}

/// `30000/1001` or `29.97` as a number.
fn rate(fps: &str) -> Option<f64> {
    match fps.split_once('/') {
        Some((n, d)) => Some(n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?),
        None => fps.trim().parse().ok(),
    }
    .filter(|r: &f64| r.is_finite() && *r > 0.0)
}

/// The card's picture, as rows of BGRA pixels with no padding.
fn draw(card: &Card, width: i32, height: i32, chart: Option<ChartData>) -> Result<Vec<u8>> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let (w, h) = (f64::from(width), f64::from(height));
        let (r, g, b) = CHART_SURFACE;
        cr.set_source_rgb(r, g, b);
        cr.paint()?;
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        match card.kind {
            CardKind::Title | CardKind::End => {
                let size = h / 9.0;
                let has_subtitle = !card.subtitle.is_empty();
                let y = if has_subtitle { h * 0.48 } else { h * 0.55 };
                centred(&cr, &card.text, size, w, y, (1.0, 1.0, 1.0))?;
                if has_subtitle {
                    cr.select_font_face(
                        "Sans",
                        cairo::FontSlant::Normal,
                        cairo::FontWeight::Normal,
                    );
                    centred(&cr, &card.subtitle, h / 22.0, w, h * 0.6, CHART_INK)?;
                }
            }
            CardKind::Chart => {
                let heading = h / 16.0;
                centred(&cr, &card.text, heading, w, h * 0.12, (1.0, 1.0, 1.0))?;
                if let Some((points, target)) = chart {
                    // The page's chart is drawn for about 960 × 540; scaled up.
                    let scale = h / 540.0;
                    let top = h * 0.18;
                    cr.save()?;
                    cr.translate(w * 0.06, top);
                    cr.scale(scale, scale);
                    crate::tracks_view::paint(
                        points,
                        target,
                        &cr,
                        w * 0.88 / scale,
                        (h - top - h * 0.08) / scale,
                    )?;
                    cr.restore()?;
                }
            }
        }
    }
    pixels(surface, width, height)
}

/// Draws `text` centred at height `y`, shrunk to fit 90% of the width.
fn centred(
    cr: &cairo::Context,
    text: &str,
    size: f64,
    w: f64,
    y: f64,
    (r, g, b): (f64, f64, f64),
) -> Result<(), cairo::Error> {
    cr.set_font_size(size);
    let mut ext = cr.text_extents(text)?;
    if ext.width() > w * 0.9 {
        cr.set_font_size(size * w * 0.9 / ext.width());
        ext = cr.text_extents(text)?;
    }
    cr.set_source_rgb(r, g, b);
    cr.move_to((w - ext.width()) / 2.0 - ext.x_bearing(), y);
    cr.show_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_read() {
        assert_eq!(
            rate("30000/1001").map(|r| (r * 1000.0).round()),
            Some(29970.0)
        );
        assert_eq!(rate("25"), Some(25.0));
        assert_eq!(rate("0"), None);
        assert_eq!(rate("x"), None);
    }

    #[test]
    fn cards_become_clips_of_their_length() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("title.mov");
        let card = Card {
            kind: CardKind::Title,
            text: "Ridge".into(),
            subtitle: "2026-11-22".into(),
            seconds: 0.5,
            course: None,
        };
        let frames = make(&card, (320, 180), "30", None, &out).unwrap();
        assert_eq!(frames, 15);
        let info = library::probe::probe(&out).unwrap();
        assert_eq!((info.width, info.height), (320, 180));
        assert_eq!(library::probe::frames(&out, &info).unwrap(), 15);
    }

    #[test]
    fn captions_are_short_and_keep_their_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("caption.mov");
        // Four seconds at most, and no longer than its clip.
        let frames = make_caption("Attempt 3 · Landed", (320, 180), "10", 1000, &out).unwrap();
        assert_eq!(frames, 40);
        let short = make_caption("Attempt 4", (320, 180), "10", 12, &out).unwrap();
        assert_eq!(short, 12);
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=pix_fmt", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&probe.stdout).trim(),
            "yuva444p12le"
        );
        // Transparent away from the panel, the panel drawn low in the middle.
        let image = draw_caption("Attempt 3", 320, 180).unwrap();
        let alpha = |x: usize, y: usize| image[(y * 320 + x) * 4 + 3];
        assert_eq!(alpha(10, 10), 0);
        assert!(alpha(160, 145) > 150);
    }
}

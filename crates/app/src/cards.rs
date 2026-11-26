//! A video's cards (`library::videos::Card`): a title, an end, or a course's
//! personal bests on the Tracks chart, drawn with cairo at the video's size
//! and made into a short ProRes clip by ffmpeg, so Resolve takes it like any
//! other clip. Captions (`Item::caption`) are drawn the same way on a
//! transparent frame, as ProRes 4444 with alpha, and go over their clip.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use gtk::cairo;
use library::brand::{Brand, Rgb};
use library::videos::{Card, CardKind};
use timer_core::tracks::Point;

use crate::tracks_view::{CHART_INK, CHART_SURFACE};

/// The colours, fonts and logos cards are drawn with: a brand kit's
/// (`library::brand`), else Ghost Run's own.
pub struct Look {
    background: Rgb,
    text: Rgb,
    secondary: Rgb,
    accent: Option<Rgb>,
    headline: Option<fontdue::Font>,
    label: Option<fontdue::Font>,
    logo: Option<PathBuf>,
    logo_small: Option<PathBuf>,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            background: CHART_SURFACE,
            text: (1.0, 1.0, 1.0),
            secondary: CHART_INK,
            accent: None,
            headline: None,
            label: None,
            logo: None,
            logo_small: None,
        }
    }
}

impl Look {
    /// A brand kit's look; fonts that can't be read fall back to Ghost
    /// Run's.
    pub fn of(brand: &Brand) -> Self {
        let font = |p: &Option<PathBuf>| {
            let bytes = std::fs::read(p.as_ref()?).ok()?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
        };
        Self {
            background: brand.background,
            text: brand.text,
            secondary: brand.neutral,
            accent: Some(brand.accent),
            headline: font(&brand.headline),
            label: font(&brand.label),
            logo: brand.logo.clone(),
            logo_small: brand.logo_small.clone(),
        }
    }

    /// The brand kit at `path`, if any and readable, else Ghost Run's look.
    pub fn load(path: Option<&Path>) -> Self {
        path.and_then(|p| Brand::load(p).ok())
            .map_or_else(Self::default, |b| Self::of(&b))
    }
}

/// The chart card's data: the course's runs, and its target.
pub type ChartData = (Vec<Point>, Option<f64>);

/// Makes `card` as a clip at `out`: `width` × `height`, `fps` (e.g.
/// `30000/1001`), as many frames as its seconds. Returns the frame count.
pub fn make(
    card: &Card,
    (width, height): (i32, i32),
    fps: &str,
    chart: Option<ChartData>,
    look: &Look,
    out: &Path,
) -> Result<i64> {
    let rate = rate(fps).context("the frame rate couldn't be read")?;
    let frames = ((card.seconds * rate).round() as i64).max(1);
    let image = draw(card, width, height, chart, look)?;
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
    look: &Look,
    out: &Path,
) -> Result<i64> {
    let rate = rate(fps).context("the frame rate couldn't be read")?;
    let frames = ((CAPTION_SECONDS * rate).round() as i64)
        .min(at_most)
        .max(1);
    let image = draw_caption(text, width, height, look)?;
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

/// A caption's picture: the text on a rounded panel, centred low in the
/// frame, everything else transparent.
fn draw_caption(text: &str, width: i32, height: i32, look: &Look) -> Result<Vec<u8>> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let (w, h) = (f64::from(width), f64::from(height));
        let font = look.label.as_ref();
        let mut size = h / 20.0;
        let mut width_of = text_width(&cr, font, text, size)?;
        if width_of > w * 0.8 {
            size *= w * 0.8 / width_of;
            width_of = text_width(&cr, font, text, size)?;
        }
        let pad = size * 0.6;
        let (bw, bh) = (width_of + 2.0 * pad, size + 2.0 * pad);
        let (x, y) = ((w - bw) / 2.0, h * 0.86 - bh);
        rounded(&cr, x, y, bw, bh, bh * 0.2);
        let (r, g, b) = look.background;
        cr.set_source_rgba(r, g, b, 0.82);
        cr.fill()?;
        line(
            &cr,
            font,
            text,
            size,
            x + pad,
            y + pad + size * 0.82,
            look.text,
        )?;
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
fn draw(
    card: &Card,
    width: i32,
    height: i32,
    chart: Option<ChartData>,
    look: &Look,
) -> Result<Vec<u8>> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let (w, h) = (f64::from(width), f64::from(height));
        let (r, g, b) = look.background;
        cr.set_source_rgb(r, g, b);
        cr.paint()?;
        match card.kind {
            CardKind::Title | CardKind::End => {
                let size = h / 9.0;
                let has_subtitle = !card.subtitle.is_empty();
                let y = if has_subtitle { h * 0.48 } else { h * 0.55 };
                let title_w = centred(
                    &cr,
                    look.headline.as_ref(),
                    &card.text,
                    size,
                    w,
                    y,
                    look.text,
                )?;
                if let Some((r, g, b)) = look.accent {
                    cr.set_source_rgb(r, g, b);
                    let bar = title_w.min(w * 0.3);
                    cr.rectangle((w - bar) / 2.0, y + size * 0.34, bar, (h / 180.0).max(2.0));
                    cr.fill()?;
                }
                if has_subtitle {
                    centred(
                        &cr,
                        look.label.as_ref(),
                        &card.subtitle,
                        h / 22.0,
                        w,
                        h * 0.6 + if look.accent.is_some() { h * 0.02 } else { 0.0 },
                        look.secondary,
                    )?;
                }
                // The brand's logo: the full one over a title, the small one
                // under an end.
                let (logo, logo_w, top) = match card.kind {
                    CardKind::Title => (&look.logo, w * 0.22, h * 0.1),
                    _ => (&look.logo_small, w * 0.16, h * 0.8),
                };
                if let Some(path) = logo {
                    // A logo that can't be drawn is left out.
                    let _ = draw_svg(&cr, path, w, logo_w, top);
                }
            }
            CardKind::Chart => {
                let heading = h / 16.0;
                centred(
                    &cr,
                    look.headline.as_ref(),
                    &card.text,
                    heading,
                    w,
                    h * 0.12,
                    look.text,
                )?;
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

/// Draws `text` centred at height `y`, shrunk to fit 90% of the width;
/// returns its width.
fn centred(
    cr: &cairo::Context,
    font: Option<&fontdue::Font>,
    text: &str,
    size: f64,
    w: f64,
    y: f64,
    colour: Rgb,
) -> Result<f64> {
    let mut size = size;
    let mut width = text_width(cr, font, text, size)?;
    if width > w * 0.9 {
        size *= w * 0.9 / width;
        width = text_width(cr, font, text, size)?;
    }
    line(cr, font, text, size, (w - width) / 2.0, y, colour)?;
    Ok(width)
}

/// How wide `text` is at `size` pixels.
fn text_width(
    cr: &cairo::Context,
    font: Option<&fontdue::Font>,
    text: &str,
    size: f64,
) -> Result<f64> {
    Ok(match font {
        Some(f) => text
            .chars()
            .map(|c| f64::from(f.metrics(c, size as f32).advance_width))
            .sum(),
        None => {
            toy_font(cr, size);
            let e = cr.text_extents(text)?;
            e.x_advance()
        }
    })
}

fn toy_font(cr: &cairo::Context, size: f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(size);
}

/// Draws `text` from `x` on the baseline `y`: with a font file through
/// fontdue (glyphs made into a mask), else cairo's own sans.
fn line(
    cr: &cairo::Context,
    font: Option<&fontdue::Font>,
    text: &str,
    size: f64,
    x: f64,
    y: f64,
    (r, g, b): Rgb,
) -> Result<()> {
    cr.set_source_rgb(r, g, b);
    let Some(font) = font else {
        toy_font(cr, size);
        cr.move_to(x, y);
        cr.show_text(text)?;
        return Ok(());
    };
    let px = size as f32;
    let glyphs: Vec<(fontdue::Metrics, Vec<u8>)> =
        text.chars().map(|c| font.rasterize(c, px)).collect();
    let width: f64 = glyphs.iter().map(|(m, _)| f64::from(m.advance_width)).sum();
    // A mask from a line's height above the baseline to a third below it.
    let (above, below) = ((size * 1.2).ceil() as i32, (size * 0.4).ceil() as i32);
    let (mw, mh) = (width.ceil() as i32 + 2, above + below);
    if mw <= 2 {
        return Ok(());
    }
    let mut mask = cairo::ImageSurface::create(cairo::Format::A8, mw, mh)?;
    {
        let stride = mask.stride() as usize;
        let mut data = mask.data().context("drawing text")?;
        let mut pen = 0.0f32;
        for (m, bitmap) in &glyphs {
            let left = (pen + m.xmin as f32).round() as i32;
            let top = above - m.height as i32 - m.ymin;
            for gy in 0..m.height {
                for gx in 0..m.width {
                    let (tx, ty) = (left + gx as i32, top + gy as i32);
                    if tx < 0 || ty < 0 || tx >= mw || ty >= mh {
                        continue;
                    }
                    let d = &mut data[ty as usize * stride + tx as usize];
                    *d = (*d).max(bitmap[gy * m.width + gx]);
                }
            }
            pen += m.advance_width;
        }
    }
    cr.mask_surface(&mask, x, y - f64::from(above))?;
    Ok(())
}

/// Draws the SVG at `path` up to `width` wide (and no taller than an
/// eighth of the frame), centred, its top at `top`.
fn draw_svg(cr: &cairo::Context, path: &Path, w: f64, width: f64, top: f64) -> Result<()> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(&std::fs::read(path)?, &usvg::Options::default())?;
    let size = tree.size();
    let tallest = w * 9.0 / 16.0 / 8.0;
    let scale = (width / f64::from(size.width())).min(tallest / f64::from(size.height()));
    let width = f64::from(size.width()) * scale;
    let (pw, ph) = (
        width.ceil() as u32,
        (f64::from(size.height()) * scale).ceil() as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).context("the logo's size")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale as f32, scale as f32),
        &mut pixmap.as_mut(),
    );
    // tiny-skia's premultiplied RGBA to cairo's premultiplied BGRA.
    let mut image = cairo::ImageSurface::create(cairo::Format::ARgb32, pw as i32, ph as i32)?;
    {
        let stride = image.stride() as usize;
        let mut data = image.data().context("drawing the logo")?;
        for (y, row) in pixmap.data().chunks(pw as usize * 4).enumerate() {
            for (x, p) in row.chunks(4).enumerate() {
                let at = y * stride + x * 4;
                data[at..at + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
    }
    cr.set_source_surface(&image, (w - f64::from(pw)) / 2.0, top)?;
    cr.paint()?;
    Ok(())
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
        let frames = make(&card, (320, 180), "30", None, &Look::default(), &out).unwrap();
        assert_eq!(frames, 15);
        let info = library::probe::probe(&out).unwrap();
        assert_eq!((info.width, info.height), (320, 180));
        assert_eq!(library::probe::frames(&out, &info).unwrap(), 15);
    }

    /// A look with the font Ghost Run carries and a square logo.
    fn branded(dir: &Path) -> Look {
        let font = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../timer-render/fonts/JetBrainsMonoNL-Medium.ttf"
        );
        let logo = dir.join("logo.svg");
        std::fs::write(
            &logo,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="100"><rect width="400" height="100" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        Look::of(&Brand {
            name: "Test".into(),
            text: (1.0, 1.0, 1.0),
            background: (0.0, 0.0, 0.2),
            accent: (0.0, 1.0, 0.0),
            neutral: (0.5, 0.5, 0.5),
            headline: Some(font.into()),
            label: Some(font.into()),
            logo: Some(logo.clone()),
            logo_small: Some(logo),
        })
    }

    #[test]
    fn a_brand_kit_gives_cards_its_colours_fonts_and_logo() {
        let dir = tempfile::tempdir().unwrap();
        let look = branded(dir.path());
        assert!(look.headline.is_some());
        let card = Card {
            kind: CardKind::Title,
            text: "Ridge".into(),
            subtitle: "2026-11-22".into(),
            seconds: 1.0,
            course: None,
        };
        let (w, h) = (640usize, 360usize);
        let image = draw(&card, w as i32, h as i32, None, &look).unwrap();
        let px = |x: usize, y: usize| &image[(y * w + x) * 4..][..4]; // BGRA
        // The background, the logo (red) above, the accent bar (green) under
        // the title, and white text.
        assert_eq!(px(5, 5), [51, 0, 0, 255]);
        assert_eq!(px(w / 2, (h as f64 * 0.12) as usize), [0, 0, 255, 255]);
        let bar_y = (h as f64 * 0.48 + h as f64 / 9.0 * 0.34) as usize + 1;
        assert_eq!(px(w / 2, bar_y), [0, 255, 0, 255]);
        let bright = (0..w * h)
            .filter(|i| image[i * 4..][..3].iter().all(|c| *c > 200))
            .count();
        assert!(bright > 200, "{bright} bright pixels");
        // A kit that can't be read is Ghost Run's own look.
        assert!(
            Look::load(Some(&dir.path().join("missing.toml")))
                .headline
                .is_none()
        );
        if let Ok(out) = std::env::var("GHOST_RUN_CARD_PNG") {
            let kit = std::env::var("GHOST_RUN_BRAND").unwrap();
            let look = Look::load(Some(Path::new(&kit)));
            for (kind, name) in [(CardKind::Title, "title"), (CardKind::End, "end")] {
                let card = Card {
                    kind,
                    ..card.clone()
                };
                make(
                    &card,
                    (1280, 720),
                    "30",
                    None,
                    &look,
                    &Path::new(&out).join(format!("{name}.mov")),
                )
                .unwrap();
            }
            make_caption(
                "Attempt 3 · Day 2 · Landed",
                (1280, 720),
                "30",
                30,
                &look,
                &Path::new(&out).join("caption.mov"),
            )
            .unwrap();
        }
    }

    #[test]
    fn captions_are_short_and_keep_their_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("caption.mov");
        // Four seconds at most, and no longer than its clip.
        let frames = make_caption(
            "Attempt 3 · Landed",
            (320, 180),
            "10",
            1000,
            &Look::default(),
            &out,
        )
        .unwrap();
        assert_eq!(frames, 40);
        let short =
            make_caption("Attempt 4", (320, 180), "10", 12, &Look::default(), &out).unwrap();
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
        let image = draw_caption("Attempt 3", 320, 180, &Look::default()).unwrap();
        let alpha = |x: usize, y: usize| image[(y * 320 + x) * 4 + 3];
        assert_eq!(alpha(10, 10), 0);
        assert!(alpha(160, 145) > 150);
    }
}

//! Draws the timer overlay into transparent video frames: the panel in the
//! top-right corner with the live timer, a row per sector and the lap as each
//! is reached (green faster, red slower than the comparison), and the best and
//! optimal laps.
//!
//! The layout is the Resolve Fusion overlay's (`overlay.py`), in the same
//! units: positions as fractions of the frame (x of its width, y of its
//! height, from the bottom), text sizes as fractions of its height. The Fusion
//! panel also blurred the video behind it; a separate overlay file can't, so
//! the panel is a plain translucent fill.
//!
//! Frames are straight (not premultiplied) RGBA, which is what ProRes 4444
//! stores and Resolve expects.

pub mod encode;
pub mod job;

use std::collections::HashMap;

use fontdue::{Font, FontSettings, Metrics};
use timer_core::overlay::{OverlayPayload, delta_color, delta_text, duration};

const FONT: &[u8] = include_bytes!("../fonts/JetBrainsMonoNL-Medium.ttf");

// Fusion layout (see `FusionOverlayUpdater.update_or_create`).
const LAYOUT_SCALE: f32 = 0.7;
const PANEL_TOP_MARGIN: f32 = 0.016;
const PANEL_RIGHT_MARGIN: f32 = PANEL_TOP_MARGIN * (9.0 / 16.0);
const PANEL_WIDTH: f32 = 0.25;
const PANEL_PADDING_Y: f32 = 0.025 * LAYOUT_SCALE;
const PANEL_MAX_HEIGHT: f32 = 0.82;
const ROW_GAP: f32 = 0.055 * LAYOUT_SCALE;
const PRIMARY_OFFSET_X: f32 = -0.045 * LAYOUT_SCALE;
const DELTA_OFFSET_X: f32 = 0.09 * LAYOUT_SCALE;
const LIVE_SIZE: f32 = 0.035 * LAYOUT_SCALE;
const ROW_SIZE: f32 = 0.025 * LAYOUT_SCALE;
const SUMMARY_SIZE: f32 = 0.024 * LAYOUT_SCALE;
const CORNER_RADIUS: f32 = 0.035;
const BORDER_WIDTH: f32 = 0.0015;

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const AMBER: [f32; 4] = [1.0, 0.76, 0.18, 1.0];
const PANEL_FILL: [f32; 4] = [0.04, 0.055, 0.08, 0.52];
const PANEL_BORDER: [f32; 4] = [0.72, 0.72, 0.72, 0.55];

/// One piece of text on the panel, shown from clip frame `from`.
#[derive(Debug, Clone, PartialEq)]
struct Label {
    text: Option<String>, // `None`: the live timer, which changes each frame
    x: f32,
    y: f32,
    size: f32,
    color: [f32; 4],
    from: i64,
}

/// Where everything goes on a `width` × `height` frame, in pixels from the top left.
#[derive(Debug, Clone)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// The panel's pixel bounds: x, y, width, height.
    pub panel: (u32, u32, u32, u32),
    panel_rect: [f32; 4], // left, top, right, bottom (px, fractional)
    labels: Vec<Label>,
    start_frame: i64,
}

impl Layout {
    pub fn new(payload: &OverlayPayload, width: u32, height: u32) -> Self {
        let rows = payload.rows();
        let mut summaries = Vec::new();
        if let Some(best) = payload.best_lap_seconds {
            summaries.push(format!("BEST       {:>8}", duration(best)));
        }
        if let Some(optimal) = payload.optimal_lap_seconds {
            summaries.push(format!("OPTIMAL    {:>8}", duration(optimal)));
        }

        let panel_right = 1.0 - PANEL_RIGHT_MARGIN;
        let center_x = panel_right - PANEL_WIDTH / 2.0;
        let top_y = 1.0 - PANEL_TOP_MARGIN - PANEL_PADDING_Y;
        let total_rows = 1 + rows.len() + summaries.len();
        let bottom_y = top_y - (total_rows - 1) as f32 * ROW_GAP;
        let panel_height = PANEL_MAX_HEIGHT.min(top_y - bottom_y + PANEL_PADDING_Y * 2.0);
        let center_y = (top_y + bottom_y) / 2.0;
        let primary_x = center_x + PRIMARY_OFFSET_X;
        let delta_x = center_x + DELTA_OFFSET_X;

        let mut labels = vec![Label {
            text: None,
            x: center_x,
            y: top_y,
            size: LIVE_SIZE,
            color: WHITE,
            from: payload.start_frame,
        }];
        for (i, row) in rows.iter().enumerate() {
            let y = top_y - (i + 1) as f32 * ROW_GAP;
            let value = if row.label == "LAP" {
                format!("LAP        {:>8}", duration(row.duration_seconds))
            } else {
                format!("{:<11}{:>8}", row.label, duration(row.duration_seconds))
            };
            labels.push(Label {
                text: Some(value),
                x: primary_x,
                y,
                size: ROW_SIZE,
                color: WHITE,
                from: row.reveal_frame,
            });
            labels.push(Label {
                text: Some(delta_text(row.delta_seconds)),
                x: delta_x,
                y,
                size: ROW_SIZE,
                color: delta_color(row.delta_seconds),
                from: row.reveal_frame,
            });
        }
        for (i, text) in summaries.into_iter().enumerate() {
            labels.push(Label {
                text: Some(text),
                x: primary_x,
                y: top_y - (1 + rows.len() + i) as f32 * ROW_GAP,
                size: SUMMARY_SIZE,
                color: AMBER,
                from: payload.start_frame,
            });
        }

        let (w, h) = (width as f32, height as f32);
        let rect = [
            (center_x - PANEL_WIDTH / 2.0) * w,
            (1.0 - center_y - panel_height / 2.0) * h,
            (center_x + PANEL_WIDTH / 2.0) * w,
            (1.0 - center_y + panel_height / 2.0) * h,
        ];
        let x0 = rect[0].floor().max(0.0) as u32;
        let y0 = rect[1].floor().max(0.0) as u32;
        let x1 = (rect[2].ceil() as u32).min(width);
        let y1 = (rect[3].ceil() as u32).min(height);
        Self {
            width,
            height,
            panel: (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)),
            panel_rect: rect,
            labels,
            start_frame: payload.start_frame,
        }
    }
}

/// Draws frames for one payload. Only the panel's pixels ever change, so
/// [`FrameRenderer::render`] rewrites just those.
pub struct FrameRenderer {
    payload: OverlayPayload,
    layout: Layout,
    font: Font,
    glyphs: HashMap<(char, u32), (Metrics, Vec<u8>)>,
    /// The empty panel, premultiplied RGBA (0–1), panel-sized.
    background: Vec<[f32; 4]>,
    work: Vec<[f32; 4]>,
    panel_buffer: Vec<u8>,
}

impl FrameRenderer {
    pub fn new(payload: OverlayPayload, width: u32, height: u32) -> Self {
        let layout = Layout::new(&payload, width, height);
        let font = Font::from_bytes(FONT, FontSettings::default()).expect("bundled font loads");
        let background = panel_background(&layout);
        let work = background.clone();
        Self {
            payload,
            layout,
            font,
            glyphs: HashMap::new(),
            background,
            work,
            panel_buffer: Vec::new(),
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// A transparent frame (all zero), `width × height × 4` bytes.
    pub fn blank_frame(&self) -> Vec<u8> {
        vec![0; self.layout.width as usize * self.layout.height as usize * 4]
    }

    /// A transparent panel-sized buffer, for [`Self::render_panel`].
    pub fn blank_panel(&self) -> Vec<u8> {
        let (_, _, pw, ph) = self.layout.panel;
        vec![0; pw as usize * ph as usize * 4]
    }

    /// Draws clip frame `frame` into `out`, a whole frame from
    /// [`Self::blank_frame`] (pixels outside the panel are left as they are).
    pub fn render(&mut self, frame: i64, out: &mut [u8]) {
        let (px, py, pw, ph) = self.layout.panel;
        let stride = self.layout.width as usize * 4;
        let row_bytes = pw as usize * 4;
        let mut panel = std::mem::take(&mut self.panel_buffer);
        panel.resize(row_bytes * ph as usize, 0);
        self.render_panel(frame, &mut panel);
        for row in 0..ph as usize {
            let start = (py as usize + row) * stride + px as usize * 4;
            out[start..start + row_bytes]
                .copy_from_slice(&panel[row * row_bytes..(row + 1) * row_bytes]);
        }
        self.panel_buffer = panel;
    }

    /// Draws just the panel's pixels of clip frame `frame` into `out` (from
    /// [`Self::blank_panel`]); the rest of every frame is transparent, so the
    /// encoder can pad the panel out to the full frame itself.
    pub fn render_panel(&mut self, frame: i64, out: &mut [u8]) {
        if frame < self.layout.start_frame {
            out.fill(0);
            return;
        }
        self.work.copy_from_slice(&self.background);
        let live = format!(
            "LIVE        {}",
            duration(self.payload.elapsed_seconds(frame))
        );
        let labels: Vec<Label> = self
            .layout
            .labels
            .iter()
            .filter(|l| frame >= l.from)
            .cloned()
            .collect();
        for label in &labels {
            let text = label.text.as_deref().unwrap_or(&live);
            self.draw_text(text, label);
        }
        for (d, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(&self.work) {
            *d = unpremultiply(*s);
        }
    }

    /// Text centred on the label's point, as Fusion's Text+ places it.
    fn draw_text(&mut self, text: &str, label: &Label) {
        let h = self.layout.height as f32;
        let px_size = label.size * h;
        let key = (px_size * 64.0).round() as u32;
        let advance = self.font.metrics('0', px_size).advance_width;
        let cap = self.glyph('H', key, px_size).0.height as f32;
        let width = advance * text.chars().count() as f32;
        let left = label.x * self.layout.width as f32 - width / 2.0;
        let baseline = (1.0 - label.y) * h + cap / 2.0;
        let (ox, oy, pw, ph) = self.layout.panel;
        for (i, c) in text.chars().enumerate() {
            if c == ' ' {
                continue;
            }
            let (metrics, coverage) = self.glyph(c, key, px_size).clone();
            let gx = (left + i as f32 * advance).round() as i64 + metrics.xmin as i64 - ox as i64;
            let gy = (baseline.round() as i64)
                - (metrics.height as i64 + metrics.ymin as i64)
                - oy as i64;
            for row in 0..metrics.height {
                let y = gy + row as i64;
                if y < 0 || y >= ph as i64 {
                    continue;
                }
                for col in 0..metrics.width {
                    let x = gx + col as i64;
                    if x < 0 || x >= pw as i64 {
                        continue;
                    }
                    let a = coverage[row * metrics.width + col] as f32 / 255.0 * label.color[3];
                    if a > 0.0 {
                        let p = &mut self.work[y as usize * pw as usize + x as usize];
                        *p = over(premultiply(label.color, a), *p);
                    }
                }
            }
        }
    }

    fn glyph(&mut self, c: char, key: u32, px_size: f32) -> &(Metrics, Vec<u8>) {
        let font = &self.font;
        self.glyphs
            .entry((c, key))
            .or_insert_with(|| font.rasterize(c, px_size))
    }
}

/// The empty panel: the rounded fill with its border on top.
fn panel_background(layout: &Layout) -> Vec<[f32; 4]> {
    let (px, py, pw, ph) = layout.panel;
    let [l, t, r, b] = layout.panel_rect;
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let (hw, hh) = ((r - l) / 2.0, (b - t) / 2.0);
    let radius = CORNER_RADIUS * (r - l).min(b - t);
    let border = (BORDER_WIDTH * layout.width as f32).max(1.0);
    let mut pixels = vec![[0.0; 4]; pw as usize * ph as usize];
    for y in 0..ph {
        for x in 0..pw {
            // Signed distance from the pixel centre to the rounded rectangle's edge.
            let dx = (px + x) as f32 + 0.5 - cx;
            let dy = (py + y) as f32 + 0.5 - cy;
            let qx = dx.abs() - (hw - radius);
            let qy = dy.abs() - (hh - radius);
            let outside = qx.max(0.0).hypot(qy.max(0.0));
            let d = outside + qx.max(qy).min(0.0) - radius;
            let fill = (0.5 - d).clamp(0.0, 1.0);
            let ring = fill - (0.5 - (d + border)).clamp(0.0, 1.0);
            let mut p = premultiply(PANEL_FILL, PANEL_FILL[3] * fill);
            p = over(premultiply(PANEL_BORDER, PANEL_BORDER[3] * ring), p);
            pixels[(y * pw + x) as usize] = p;
        }
    }
    pixels
}

fn premultiply(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0] * alpha, color[1] * alpha, color[2] * alpha, alpha]
}

/// `src` over `dst`, both premultiplied.
fn over(src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let k = 1.0 - src[3];
    [
        src[0] + dst[0] * k,
        src[1] + dst[1] * k,
        src[2] + dst[2] * k,
        src[3] + dst[3] * k,
    ]
}

fn unpremultiply(p: [f32; 4]) -> [u8; 4] {
    if p[3] <= 0.0 {
        return [0; 4];
    }
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    [
        byte(p[0] / p[3]),
        byte(p[1] / p[3]),
        byte(p[2] / p[3]),
        byte(p[3]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use timer_core::overlay::ComparisonMode;

    fn payload() -> OverlayPayload {
        OverlayPayload {
            course_id: "course".into(),
            run_id: None,
            start_frame: 100,
            finish_frame: 400,
            source_fps: 100.0,
            marker_frames: [("Start", 100), ("S1", 200), ("Finish", 400)]
                .into_iter()
                .map(|(n, f)| (n.to_owned(), f))
                .collect(),
            sector_reference_seconds: vec![Some(1.2), Some(1.9)],
            best_lap_seconds: Some(3.1),
            optimal_lap_seconds: Some(3.0),
            comparison_mode: ComparisonMode::BestLap,
        }
    }

    fn alpha_in(frame: &[u8], width: u32, (x, y, w, h): (u32, u32, u32, u32)) -> u64 {
        let mut sum = 0;
        for row in y..y + h {
            for col in x..x + w {
                sum += frame[((row * width + col) * 4 + 3) as usize] as u64;
            }
        }
        sum
    }

    #[test]
    fn panel_sits_in_the_top_right_corner() {
        let layout = Layout::new(&payload(), 3840, 2160);
        let (x, y, w, h) = layout.panel;
        assert!((960..=961).contains(&w), "{w}"); // plus a partly covered edge pixel
        // 16:9: the right margin equals the top margin in pixels (0.016 × 2160).
        assert!((3840 - (x + w)).abs_diff(35) <= 1, "{:?}", layout.panel);
        assert!(y.abs_diff(35) <= 1, "{:?}", layout.panel);
        // LIVE + S1, S2, LAP + BEST, OPTIMAL: 5 gaps plus padding.
        let expected = (5.0 * ROW_GAP + 2.0 * PANEL_PADDING_Y) * 2160.0;
        assert!((h as f32 - expected).abs() <= 2.0, "{h} vs {expected}");
    }

    #[test]
    fn nothing_before_the_start_then_rows_as_they_are_reached() {
        let mut r = FrameRenderer::new(payload(), 640, 360);
        let mut frame = r.blank_frame();
        r.render(50, &mut frame);
        assert!(frame.iter().all(|b| *b == 0));

        r.render(150, &mut frame);
        let before_s1 = frame.clone();
        assert!(alpha_in(&frame, 640, r.layout().panel) > 0);
        r.render(200, &mut frame);
        assert_ne!(before_s1, frame, "S1's row appears at its marker");
        let at_finish = {
            r.render(400, &mut frame);
            frame.clone()
        };
        r.render(900, &mut frame);
        assert_eq!(at_finish, frame, "held after the finish");

        // Going back to before the start clears the panel again.
        r.render(0, &mut frame);
        assert!(frame.iter().all(|b| *b == 0));
    }

    #[test]
    fn panel_only_frames_match_the_full_frame() {
        let mut r = FrameRenderer::new(payload(), 640, 360);
        let (px, py, pw, ph) = r.layout().panel;
        let (mut full, mut panel) = (r.blank_frame(), r.blank_panel());
        for n in [0, 150, 250, 400] {
            r.render(n, &mut full);
            r.render_panel(n, &mut panel);
            for row in 0..ph as usize {
                let start = ((py as usize + row) * 640 + px as usize) * 4;
                assert_eq!(
                    &full[start..start + pw as usize * 4],
                    &panel[row * pw as usize * 4..(row + 1) * pw as usize * 4],
                    "frame {n}, row {row}"
                );
            }
        }
    }

    #[test]
    fn outside_the_panel_stays_transparent() {
        let mut r = FrameRenderer::new(payload(), 640, 360);
        let mut frame = r.blank_frame();
        r.render(400, &mut frame);
        let (x, _, _, _) = r.layout().panel;
        assert_eq!(alpha_in(&frame, 640, (0, 0, x, 360)), 0);
    }
}

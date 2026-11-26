//! The top of the Tracks page for one course: the personal best and how long
//! it has stood, the latest run against it, the target, a trend chart of
//! every counted run (dots) with the personal best as a step line, and the
//! sectors losing most lately. The maths is `timer_core::tracks`.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk::cairo;
use timer_core::timing::format_duration;
use timer_core::tracks::{CourseSummary, Point, RECENT_RUNS, SectorGap};

// The app's dark palette (ui/style.css): accent for the personal best, a
// recessive grey for each run, muted ink for the grid, text and target.
const ACCENT: (f64, f64, f64) = (
    0x7a as f64 / 255.0,
    0xa2 as f64 / 255.0,
    0xf7 as f64 / 255.0,
);
const RUN: (f64, f64, f64) = (
    0x8a as f64 / 255.0,
    0x94 as f64 / 255.0,
    0xa3 as f64 / 255.0,
);
const INK: (f64, f64, f64) = (
    0xae as f64 / 255.0,
    0xb8 as f64 / 255.0,
    0xc5 as f64 / 255.0,
);
const SURFACE: (f64, f64, f64) = (
    0x15 as f64 / 255.0,
    0x18 as f64 / 255.0,
    0x1d as f64 / 255.0,
);
const GRID: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.08);

const LEFT: f64 = 70.0;
const RIGHT: f64 = 16.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 26.0;

#[derive(Default)]
struct Chart {
    points: Vec<Point>,
    target: Option<f64>,
    /// Where each point was drawn, for the tooltip.
    drawn: Vec<(f64, f64)>,
}

#[derive(Clone)]
pub struct TrackView {
    pub root: gtk::Box,
    pb: gtk::Label,
    latest: gtk::Label,
    target: gtk::Label,
    pub target_button: gtk::Button,
    chart: gtk::DrawingArea,
    legend: gtk::Label,
    gaps: gtk::Label,
    data: Rc<RefCell<Chart>>,
}

impl TrackView {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let stats = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        let pb = stat_label();
        let latest = stat_label();
        let target = stat_label();
        let target_button = gtk::Button::with_label("Target…");
        target_button.set_tooltip_text(Some("Set a lap time to chase on this course"));
        target_button.set_valign(gtk::Align::Center);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        for w in [
            pb.upcast_ref::<gtk::Widget>(),
            latest.upcast_ref(),
            target.upcast_ref(),
            spacer.upcast_ref(),
            target_button.upcast_ref(),
        ] {
            stats.append(w);
        }
        root.append(&stats);

        let chart = gtk::DrawingArea::new();
        chart.set_content_height(180);
        chart.set_hexpand(true);
        chart.set_has_tooltip(true);
        chart.update_property(&[gtk::accessible::Property::Label(
            "Lap times over time, with the personal best",
        )]);
        root.append(&chart);
        let legend = gtk::Label::new(None);
        legend.set_xalign(0.0);
        legend.add_css_class("muted");
        root.append(&legend);
        let gaps = gtk::Label::new(None);
        gaps.set_xalign(0.0);
        gaps.set_wrap(true);
        root.append(&gaps);

        let data: Rc<RefCell<Chart>> = Rc::default();
        let d = data.clone();
        chart.set_draw_func(move |_, cr, w, h| {
            let mut chart = d.borrow_mut();
            let _ = draw(&mut chart, cr, w as f64, h as f64);
        });
        let d = data.clone();
        chart.connect_query_tooltip(move |_, x, y, _, tooltip| {
            let chart = d.borrow();
            let nearest = chart
                .drawn
                .iter()
                .enumerate()
                .map(|(i, (px, py))| (i, (px - x as f64).hypot(py - y as f64)))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            match nearest {
                Some((i, distance)) if distance <= 14.0 => {
                    tooltip.set_text(Some(&point_text(&chart.points, i)));
                    true
                }
                _ => false,
            }
        });
        Self {
            root,
            pb,
            latest,
            target,
            target_button,
            chart,
            legend,
            gaps,
            data,
        }
    }

    /// Shows one course.
    pub fn show(
        &self,
        summary: Option<&CourseSummary>,
        points: Vec<Point>,
        gaps: &[SectorGap],
        target: Option<f64>,
        now: DateTime<Utc>,
    ) {
        let best = summary.and_then(|s| s.best.as_ref());
        self.pb.set_markup(&match best {
            Some(b) => {
                let stood = b
                    .when
                    .map(|w| format!(" · {}", stood(now - w)))
                    .unwrap_or_default();
                format!("PB <b>{}</b>{stood}", time(b.lap_seconds))
            }
            None => "No counted runs yet".into(),
        });
        self.latest
            .set_markup(&match summary.and_then(|s| s.latest_gap) {
                Some(g) if g <= 0.0 => "Latest run is your PB".into(),
                Some(g) => format!("Latest <b>+{g:.3} s</b> off your PB"),
                None => String::new(),
            });
        self.target.set_markup(&match (target, best) {
            (None, _) => "No target".into(),
            (Some(t), Some(b)) if b.lap_seconds <= t => {
                format!("Target {} · <b>beaten</b>", time(t))
            }
            (Some(t), Some(b)) => format!(
                "Target {} · <b>{:.3} s</b> to go",
                time(t),
                b.lap_seconds - t
            ),
            (Some(t), None) => format!("Target {}", time(t)),
        });
        self.gaps.set_markup(&gaps_text(gaps));
        self.legend.set_markup(&format!(
            "<span foreground=\"#8a94a3\">●</span> each counted run    <span foreground=\"#7aa2f7\">━━</span> personal best{}",
            if target.is_some() { "    ┄┄ target" } else { "" }
        ));
        let has_points = !points.is_empty();
        self.chart.set_visible(has_points);
        self.legend.set_visible(has_points);
        let mut chart = self.data.borrow_mut();
        chart.points = points;
        chart.target = target;
        chart.drawn.clear();
        drop(chart);
        self.chart.queue_draw();
    }
}

fn stat_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l
}

fn time(seconds: f64) -> String {
    format_duration(seconds).unwrap_or_else(|_| "?".into())
}

/// "set today", "stood 12 days", "stood 3 months".
fn stood(age: chrono::Duration) -> String {
    let days = age.num_days();
    match days {
        ..=0 => "set today".into(),
        1 => "stood 1 day".into(),
        2..=59 => format!("stood {days} days"),
        _ => format!("stood {} months", days / 30),
    }
}

fn gaps_text(gaps: &[SectorGap]) -> String {
    let losing: Vec<String> = gaps
        .iter()
        .filter(|g| g.gap >= 0.005)
        .map(|g| format!("S{} +{:.2} s", g.sector, g.gap))
        .collect();
    match losing.split_first() {
        None if gaps.is_empty() => String::new(),
        None => "Lately you're matching your fastest time in every sector.".into(),
        Some((worst, rest)) => {
            let mut text = format!("Where you lose time lately: <b>{worst}</b>");
            for r in rest {
                text.push_str(&format!(", {r}"));
            }
            text.push_str(&format!(
                " on your fastest (mean of your last {RECENT_RUNS} runs)"
            ));
            text
        }
    }
}

fn point_text(points: &[Point], i: usize) -> String {
    let p = &points[i];
    let mut text = format!(
        "{} · {}",
        p.when.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
        time(p.lap_seconds)
    );
    if p.new_best {
        text.push_str(" · new PB");
    }
    text
}

/// Draws the trend chart (as on the page) into `cr`, `w` × `h`: for a
/// video's chart card (`cards`).
pub fn paint(
    points: Vec<Point>,
    target: Option<f64>,
    cr: &cairo::Context,
    w: f64,
    h: f64,
) -> Result<(), cairo::Error> {
    let mut chart = Chart {
        points,
        target,
        drawn: Vec::new(),
    };
    draw(&mut chart, cr, w, h)
}

pub const CHART_SURFACE: (f64, f64, f64) = SURFACE;
pub const CHART_INK: (f64, f64, f64) = INK;

fn rgb(cr: &cairo::Context, (r, g, b): (f64, f64, f64)) {
    cr.set_source_rgb(r, g, b);
}

fn draw(chart: &mut Chart, cr: &cairo::Context, w: f64, h: f64) -> Result<(), cairo::Error> {
    chart.drawn.clear();
    if chart.points.is_empty() {
        return Ok(());
    }
    let laps = chart.points.iter().map(|p| p.lap_seconds);
    let mut lo = laps.clone().fold(f64::INFINITY, f64::min);
    let mut hi = laps.fold(f64::NEG_INFINITY, f64::max);
    if let Some(t) = chart.target {
        lo = lo.min(t);
        hi = hi.max(t);
    }
    let pad = ((hi - lo) * 0.1).max(0.5);
    let (lo, hi) = (lo - pad, hi + pad);
    let t0 = chart.points[0].when.timestamp() as f64;
    let t1 = chart.points[chart.points.len() - 1].when.timestamp() as f64;
    let (plot_w, plot_h) = (w - LEFT - RIGHT, h - TOP - BOTTOM);
    let x = |t: f64| {
        if t1 > t0 {
            LEFT + (t - t0) / (t1 - t0) * plot_w
        } else {
            LEFT + plot_w / 2.0
        }
    };
    let y = |lap: f64| TOP + (hi - lap) / (hi - lo) * plot_h;

    // Grid and lap labels, on round seconds.
    cr.set_line_width(1.0);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(11.0);
    for lap in ticks(lo, hi) {
        let py = y(lap).round() + 0.5;
        cr.set_source_rgba(GRID.0, GRID.1, GRID.2, GRID.3);
        cr.move_to(LEFT, py);
        cr.line_to(w - RIGHT, py);
        cr.stroke()?;
        rgb(cr, INK);
        let text = time(lap).trim_end_matches(".000").to_owned();
        let ext = cr.text_extents(&text)?;
        cr.move_to(LEFT - 8.0 - ext.width(), py + ext.height() / 2.0);
        cr.show_text(&text)?;
    }
    // First and last day under the plot.
    rgb(cr, INK);
    let day = |p: &Point| p.when.with_timezone(&Local).format("%Y-%m-%d").to_string();
    let first = day(&chart.points[0]);
    cr.move_to(LEFT, h - 8.0);
    cr.show_text(&first)?;
    let last = day(&chart.points[chart.points.len() - 1]);
    if last != first {
        let ext = cr.text_extents(&last)?;
        cr.move_to(w - RIGHT - ext.width(), h - 8.0);
        cr.show_text(&last)?;
    }

    // Target: a dashed line in muted ink, labelled at the right.
    if let Some(t) = chart.target {
        let py = y(t).round() + 0.5;
        rgb(cr, INK);
        cr.set_dash(&[4.0, 4.0], 0.0);
        cr.move_to(LEFT, py);
        cr.line_to(w - RIGHT, py);
        cr.stroke()?;
        cr.set_dash(&[], 0.0);
        let ext = cr.text_extents("target")?;
        cr.move_to(w - RIGHT - ext.width(), py - 4.0);
        cr.show_text("target")?;
    }

    // Personal best: a step line from each new best to the next, on to the
    // last run.
    let bests: Vec<&Point> = chart.points.iter().filter(|p| p.new_best).collect();
    rgb(cr, ACCENT);
    cr.set_line_width(2.0);
    cr.set_line_join(cairo::LineJoin::Round);
    for (i, b) in bests.iter().enumerate() {
        let x0 = x(b.when.timestamp() as f64);
        let x1 = bests
            .get(i + 1)
            .map(|n| x(n.when.timestamp() as f64))
            .unwrap_or_else(|| x(t1));
        let py = y(b.lap_seconds);
        if i == 0 {
            cr.move_to(x0, py);
        } else {
            cr.line_to(x0, py);
        }
        cr.line_to(x1, py);
    }
    cr.stroke()?;

    // Each run: a dot with a ring of the surface colour; new bests in accent.
    for p in &chart.points {
        let (px, py) = (x(p.when.timestamp() as f64), y(p.lap_seconds));
        let r = if p.new_best { 5.0 } else { 4.0 };
        rgb(cr, SURFACE);
        cr.arc(px, py, r + 2.0, 0.0, std::f64::consts::TAU);
        cr.fill()?;
        rgb(cr, if p.new_best { ACCENT } else { RUN });
        cr.arc(px, py, r, 0.0, std::f64::consts::TAU);
        cr.fill()?;
        chart.drawn.push((px, py));
    }
    Ok(())
}

/// Round lap times for the grid: the first step (1, 2, 5, 10… seconds) that
/// gives at most four lines between `lo` and `hi`.
fn ticks(lo: f64, hi: f64) -> Vec<f64> {
    let step = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
        .into_iter()
        .find(|s| ((hi - lo) / s).floor() <= 4.0)
        .unwrap_or(600.0);
    let mut out = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        out.push(v);
        v += step;
    }
    out
}

/// A target typed as `2:31.4`, `151.4` or `1:02:03`; empty clears it.
pub fn parse_target(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut seconds = 0.0;
    for part in text.split(':') {
        let n: f64 = part
            .trim()
            .parse()
            .map_err(|_| format!("“{text}” isn't a time: type it like 2:31.4"))?;
        seconds = seconds * 60.0 + n;
    }
    if seconds.is_finite() && seconds > 0.0 {
        Ok(Some(seconds))
    } else {
        Err(format!("“{text}” isn't a time: type it like 2:31.4"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_read_like_lap_times() {
        assert_eq!(parse_target("2:31.4"), Ok(Some(151.4)));
        assert_eq!(parse_target(" 151.4 "), Ok(Some(151.4)));
        assert_eq!(parse_target("1:02:03"), Ok(Some(3723.0)));
        assert_eq!(parse_target(""), Ok(None));
        assert!(parse_target("fast").is_err());
        assert!(parse_target("0").is_err());
    }

    #[test]
    fn sector_gaps_name_the_worst_first() {
        let gap = |sector, gap| SectorGap {
            sector,
            fastest: 30.0,
            recent_mean: 30.0 + gap,
            gap,
        };
        assert_eq!(
            gaps_text(&[gap(2, 3.0), gap(1, 1.0)]),
            "Where you lose time lately: <b>S2 +3.00 s</b>, S1 +1.00 s on your fastest (mean of your last 5 runs)"
        );
        assert_eq!(
            gaps_text(&[gap(1, 0.0)]),
            "Lately you're matching your fastest time in every sector."
        );
        assert_eq!(gaps_text(&[]), "");
    }

    #[test]
    fn grid_lines_fall_on_round_seconds() {
        assert_eq!(ticks(143.9, 158.2), [145.0, 150.0, 155.0]);
        assert_eq!(ticks(60.2, 62.9), [61.0, 62.0]);
    }

    #[test]
    fn a_best_says_how_long_it_has_stood() {
        assert_eq!(stood(chrono::Duration::hours(3)), "set today");
        assert_eq!(stood(chrono::Duration::days(12)), "stood 12 days");
        assert_eq!(stood(chrono::Duration::days(95)), "stood 3 months");
    }
}

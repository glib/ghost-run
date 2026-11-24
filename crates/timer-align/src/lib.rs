//! Places a new clip's markers from a reference run: the same course ridden
//! before, with its markers already placed. The new clip's footage is lined up
//! with the reference run's, and each reference marker lands where the new
//! clip passes the same spot.
//!
//! 1. Both clips are read as small greyscale frames ([`Frames`], 64 × 36).
//! 2. Coarse: about ten frames a second of the reference run (Start − 1 s to
//!    Finish + 1 s) are aligned with the whole new clip by dynamic time
//!    warping. Either run may be up to 3× faster than the other at any point,
//!    and the run may start anywhere in the clip.
//! 3. Fine: each marker is moved to the frame where a short window of frames
//!    around it matches the reference best, allowing for the two runs' speeds
//!    there.
//!
//! Frames are compared by correlation, so a change of exposure between the
//! two days doesn't matter; what they show does.

pub mod video;

/// Small greyscale frames, one byte a pixel, frame after frame.
#[derive(Debug, Clone)]
pub struct Frames {
    pub width: usize,
    pub height: usize,
    pub fps: f64,
    pub data: Vec<u8>,
}

impl Frames {
    pub fn len(&self) -> usize {
        self.data.len() / (self.width * self.height).max(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn frame(&self, i: usize) -> &[u8] {
        let size = self.width * self.height;
        &self.data[i * size..(i + 1) * size]
    }
}

/// A placed marker: its frame in the new clip and how sure the match is.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedMarker {
    pub name: String,
    pub frame: i64,
    /// 0–1: how much better the chosen frame matches than the frames around
    /// it. Low values are worth checking by eye.
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub struct Alignment {
    pub markers: Vec<PlacedMarker>,
    /// Mean difference along the matched footage: 0 is identical, 1 unrelated.
    /// High values suggest the run isn't in the clip.
    pub match_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlignError {
    #[error("the reference clip has {frames} frames, but marker {marker} is at frame {frame}")]
    MarkerOutsideReference {
        marker: String,
        frame: i64,
        frames: usize,
    },
    #[error("the new clip is too short to hold the run")]
    ClipTooShort,
    #[error("no markers to place")]
    NoMarkers,
}

/// Coarse alignment works on about this many frames a second.
const COARSE_RATE: f64 = 10.0;
/// The steepest speed difference allowed between the two runs.
const MAX_SLOPE: usize = 3;
/// Reference footage kept either side of Start and Finish (seconds).
const CONTEXT: f64 = 1.0;
/// How far from the coarse estimate a marker may move (seconds, each way).
const SEARCH: f64 = 0.5;
/// Frames compared around a marker to refine it (seconds, each way).
const WINDOW: f64 = 0.25;

/// Places `markers` (name, reference frame; in order along the course) in `clip`.
pub fn align(
    reference: &Frames,
    markers: &[(String, i64)],
    clip: &Frames,
) -> Result<Alignment, AlignError> {
    let (Some(first), Some(last)) = (markers.first(), markers.last()) else {
        return Err(AlignError::NoMarkers);
    };
    for (name, frame) in markers {
        if *frame < 0 || *frame as usize >= reference.len() {
            return Err(AlignError::MarkerOutsideReference {
                marker: name.clone(),
                frame: *frame,
                frames: reference.len(),
            });
        }
    }

    // Coarse.
    let ref_step = step(reference.fps);
    let clip_step = step(clip.fps);
    let context = (CONTEXT * reference.fps).round() as i64;
    let ref_from = (first.1 - context).max(0) as usize;
    let ref_to = ((last.1 + context) as usize).min(reference.len() - 1);
    let ref_samples: Vec<usize> = (ref_from..=ref_to).step_by(ref_step).collect();
    let clip_samples: Vec<usize> = (0..clip.len()).step_by(clip_step).collect();
    if clip_samples.len() * MAX_SLOPE < ref_samples.len() {
        return Err(AlignError::ClipTooShort);
    }
    let coarse_ref = Descriptors::new(reference, &ref_samples, 2);
    let coarse_clip = Descriptors::new(clip, &clip_samples, 2);
    let (path, match_cost) = subsequence_dtw(&coarse_ref, &coarse_clip);
    let curve = PathCurve::new(&path, ref_samples.len());

    // Fine: full-size frames, converted only where they're compared.
    let mut fine_ref = FineFrames::new(reference);
    let mut fine_clip = FineFrames::new(clip);
    let mut placed = Vec::with_capacity(markers.len());
    for (name, ref_frame) in markers {
        let x = (*ref_frame as usize - ref_from) as f64 / ref_step as f64;
        let estimate = curve.at(x) * clip_step as f64;
        // Clip frames per reference frame, around this marker.
        let half = (COARSE_RATE * CONTEXT).max(1.0);
        let slope = (curve.at(x + half) - curve.at(x - half))
            / (curve.clamp(x + half) - curve.clamp(x - half)).max(1e-9);
        let ratio = (slope * clip_step as f64 / ref_step as f64).clamp(0.2, 5.0);
        let (frame, confidence) = refine(
            &mut fine_ref,
            *ref_frame as usize,
            &mut fine_clip,
            estimate,
            ratio,
        );
        placed.push(PlacedMarker {
            name: name.clone(),
            frame: frame as i64,
            confidence,
        });
    }
    Ok(Alignment {
        markers: placed,
        match_cost,
    })
}

/// Frames skipped between coarse samples, for about [`COARSE_RATE`] a second.
fn step(fps: f64) -> usize {
    ((fps / COARSE_RATE).round() as usize).max(1)
}

/// Each frame as a zero-mean, unit-length vector, so `1 − a·b` compares
/// pictures regardless of brightness and contrast.
struct Descriptors {
    dim: usize,
    values: Vec<f32>,
}

impl Descriptors {
    /// The frames at `indices`, shrunk by `shrink` each way.
    fn new(frames: &Frames, indices: &[usize], shrink: usize) -> Self {
        let (w, h) = (frames.width / shrink, frames.height / shrink);
        let dim = w * h;
        let mut values = Vec::with_capacity(indices.len() * dim);
        for &i in indices {
            let src = frames.frame(i);
            let start = values.len();
            for y in 0..h {
                for x in 0..w {
                    let mut sum = 0u32;
                    for dy in 0..shrink {
                        for dx in 0..shrink {
                            sum +=
                                u32::from(src[(y * shrink + dy) * frames.width + x * shrink + dx]);
                        }
                    }
                    values.push(sum as f32 / (shrink * shrink) as f32);
                }
            }
            normalize(&mut values[start..]);
        }
        Self { dim, values }
    }

    fn len(&self) -> usize {
        self.values.len() / self.dim.max(1)
    }

    fn get(&self, i: usize) -> &[f32] {
        &self.values[i * self.dim..(i + 1) * self.dim]
    }

    /// 0 for the same picture, about 1 for unrelated ones, 2 for opposites.
    fn distance(&self, i: usize, other: &Self, j: usize) -> f32 {
        let (a, b) = (self.get(i), other.get(j));
        1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
    }
}

/// Full-size frame descriptors, made when first asked for.
struct FineFrames<'a> {
    frames: &'a Frames,
    made: std::collections::HashMap<usize, Vec<f32>>,
}

impl<'a> FineFrames<'a> {
    fn new(frames: &'a Frames) -> Self {
        Self {
            frames,
            made: std::collections::HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.frames.len()
    }

    fn get(&mut self, i: usize) -> &[f32] {
        let frames = self.frames;
        self.made.entry(i).or_insert_with(|| {
            let mut v: Vec<f32> = frames.frame(i).iter().map(|&p| f32::from(p)).collect();
            normalize(&mut v);
            v
        })
    }
}

fn fine_distance(a: &mut FineFrames<'_>, i: usize, b: &mut FineFrames<'_>, j: usize) -> f32 {
    let x = a.get(i).to_vec();
    1.0 - x.iter().zip(b.get(j)).map(|(p, q)| p * q).sum::<f32>()
}

fn normalize(v: &mut [f32]) {
    let mean = v.iter().sum::<f32>() / v.len().max(1) as f32;
    v.iter_mut().for_each(|x| *x -= mean);
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-3 {
        v.iter_mut().for_each(|x| *x /= norm);
    } else {
        v.fill(0.0); // a flat frame matches nothing
    }
}

/// Aligns all of `r` with the best-matching stretch of `q`.
///
/// Steps move one sample in one sequence and 1..=3 in the other, so neither
/// run can be more than 3× faster than the other. Each step's cost is
/// weighted by the samples it covers in both sequences, so every alignment of
/// `r` with a stretch of `q` is weighted by `len(r) + len(stretch)`, and the
/// best stretch is the one with the lowest cost per unit of weight.
///
/// The lowest *mean* is found exactly (Dinkelbach's method): one pass finds
/// the path with the lowest total of `cost − λ` per unit of weight, λ becomes
/// that path's mean, and passes repeat until no path beats it. Keeping only
/// the lowest *total* into each cell instead favours paths squeezed through
/// the fewest clip samples, so a rider stopping mid-run once made the whole
/// first half of a run be skipped (#27).
///
/// Returns the path as (r index, q index) cells, and the mean cost along it.
fn subsequence_dtw(r: &Descriptors, q: &Descriptors) -> (Vec<(usize, usize)>, f64) {
    let (n, m) = (r.len(), q.len());
    let mut local = vec![0f32; n * m];
    for i in 0..n {
        for j in 0..m {
            local[i * m + j] = r.distance(i, q, j);
        }
    }
    let mut lambda = 0.0f32;
    let mut best: Option<(Vec<(usize, usize)>, f64)> = None;
    for _ in 0..MAX_PASSES {
        let (path, mean) = dtw_pass(&local, n, m, lambda);
        if best.as_ref().is_some_and(|(_, b)| mean >= b - 1e-6) {
            break;
        }
        lambda = mean as f32;
        best = Some((path, mean));
    }
    best.expect("one pass runs")
}

/// Passes of [`subsequence_dtw`] at most; it usually settles in three or four.
const MAX_PASSES: usize = 12;

/// One pass of [`subsequence_dtw`]: the path with the lowest total of
/// `local − lambda` over its weight, with its mean `local`.
fn dtw_pass(local: &[f32], n: usize, m: usize, lambda: f32) -> (Vec<(usize, usize)>, f64) {
    let k = MAX_SLOPE;
    let d = |i: usize, j: usize| local[i * m + j] - lambda;
    // Steps: 0 = start of the path (row 0); 1..=k: (1, s); k+1..=2k-1: (s, 1) for s in 2..=k.
    let mut back = vec![0u8; n * m];
    // Rolling rows (the last k + 1) of the cost so far and the path's start column.
    let mut cost = vec![vec![f32::INFINITY; m]; k + 1];
    let mut start = vec![vec![0u32; m]; k + 1];
    for i in 0..n {
        let row = i % (k + 1);
        for j in 0..m {
            let here = d(i, j);
            if i == 0 {
                cost[row][j] = 2.0 * here;
                start[row][j] = j as u32;
                continue;
            }
            let mut best = (f32::INFINITY, 0u8, 0u32);
            // (1, s): one reference sample, s clip samples.
            let prev = (i - 1) % (k + 1);
            for s in 1..=k {
                if j < s {
                    break;
                }
                let skipped: f32 = (1..s).map(|t| d(i, j - t)).sum();
                let c = cost[prev][j - s] + skipped + 2.0 * here;
                if c < best.0 {
                    best = (c, s as u8, start[prev][j - s]);
                }
            }
            // (s, 1): s reference samples, one clip sample.
            for s in 2..=k {
                if i < s || j < 1 {
                    break;
                }
                let from = (i - s) % (k + 1);
                let skipped: f32 = (1..s).map(|t| d(i - t, j)).sum();
                let c = cost[from][j - 1] + skipped + 2.0 * here;
                if c < best.0 {
                    best = (c, (k + s - 1) as u8, start[from][j - 1]);
                }
            }
            cost[row][j] = best.0;
            back[i * m + j] = best.1;
            start[row][j] = best.2;
        }
    }

    let last = (n - 1) % (k + 1);
    let weight = |j: usize| (2 + (n - 1) + (j - start[last][j] as usize)) as f64;
    // Lowest total of `local − lambda`; with lambda = 0 (the first pass),
    // the lowest mean of the paths found.
    let score = |j: usize| {
        let c = f64::from(cost[last][j]);
        if lambda == 0.0 { c / weight(j) } else { c }
    };
    let end = (0..m)
        .filter(|&j| cost[last][j].is_finite())
        .min_by(|&a, &b| score(a).total_cmp(&score(b)))
        .expect("a path exists (clip checked long enough)");
    let mean = f64::from(cost[last][end]) / weight(end) + f64::from(lambda);

    let mut path = Vec::new();
    let (mut i, mut j) = (n - 1, end);
    loop {
        path.push((i, j));
        let b = back[i * m + j] as usize;
        if i == 0 || b == 0 {
            break;
        }
        if b <= k {
            for t in 1..b {
                path.push((i, j - t));
            }
            i -= 1;
            j -= b;
        } else {
            let s = b - k + 1;
            for t in 1..s {
                path.push((i - t, j));
            }
            i -= s;
            j -= 1;
        }
    }
    path.reverse();
    (path, mean)
}

/// The alignment as a function: reference sample (fractional) → clip sample.
struct PathCurve {
    /// Mean clip sample matched to each reference sample.
    points: Vec<f64>,
}

impl PathCurve {
    fn new(path: &[(usize, usize)], n: usize) -> Self {
        let mut sums = vec![(0.0, 0u32); n];
        for &(i, j) in path {
            sums[i].0 += j as f64;
            sums[i].1 += 1;
        }
        Self {
            points: sums
                .iter()
                .map(|(s, c)| s / f64::from((*c).max(1)))
                .collect(),
        }
    }

    fn clamp(&self, x: f64) -> f64 {
        x.clamp(0.0, (self.points.len() - 1) as f64)
    }

    fn at(&self, x: f64) -> f64 {
        let x = self.clamp(x);
        let (lo, frac) = (x.floor() as usize, x.fract());
        let hi = (lo + 1).min(self.points.len() - 1);
        self.points[lo] * (1.0 - frac) + self.points[hi] * frac
    }
}

/// The clip frame near `estimate` whose surrounding frames best match those
/// around `ref_frame`, with `ratio` clip frames per reference frame. Returns it
/// with a confidence: 1 − best score ÷ the median score of the frames searched.
fn refine(
    reference: &mut FineFrames<'_>,
    ref_frame: usize,
    clip: &mut FineFrames<'_>,
    estimate: f64,
    ratio: f64,
) -> (usize, f64) {
    let window = (WINDOW * reference.frames.fps).round() as i64;
    let search = (SEARCH * clip.frames.fps).round() as i64;
    let centre = estimate.round() as i64;
    let clip_len = clip.len() as i64;
    let mut scores = Vec::new();
    for candidate in (centre - search)..=(centre + search) {
        if candidate < 0 || candidate >= clip_len {
            continue;
        }
        let (mut total, mut count) = (0.0f64, 0u32);
        for k in -window..=window {
            let r = ref_frame as i64 + k;
            let c = candidate + (k as f64 * ratio).round() as i64;
            if r < 0 || r >= reference.len() as i64 || c < 0 || c >= clip_len {
                continue;
            }
            total += f64::from(fine_distance(reference, r as usize, clip, c as usize));
            count += 1;
        }
        if count > 0 {
            scores.push((candidate, total / f64::from(count)));
        }
    }
    let Some(&(best, best_score)) = scores.iter().min_by(|a, b| a.1.total_cmp(&b.1)) else {
        return (centre.clamp(0, clip_len - 1) as usize, 0.0);
    };
    let mut sorted: Vec<f64> = scores.iter().map(|s| s.1).collect();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let confidence = if median > 1e-9 {
        (1.0 - best_score / median).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (best as usize, confidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long strip of smooth random texture: the "trail".
    fn world(width: usize, height: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 10_000) as f32 / 10_000.0
        };
        // Value noise at three scales.
        let mut out = vec![0.0; width * height];
        for (scale, amp) in [(24usize, 1.0f32), (8, 0.5), (3, 0.25)] {
            let gw = width / scale + 2;
            let gh = height / scale + 2;
            let grid: Vec<f32> = (0..gw * gh).map(|_| rand()).collect();
            for y in 0..height {
                for x in 0..width {
                    let (fx, fy) = (x as f32 / scale as f32, y as f32 / scale as f32);
                    let (x0, y0) = (fx as usize, fy as usize);
                    let (tx, ty) = (fx.fract(), fy.fract());
                    let g = |a: usize, b: usize| grid[b * gw + a];
                    let top = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
                    let bottom = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
                    out[y * width + x] += amp * (top * (1.0 - ty) + bottom * ty);
                }
            }
        }
        out
    }

    const WORLD_W: usize = 2400;
    const WORLD_H: usize = 60;

    /// A clip panning along the world: `positions` gives the view's left edge
    /// for each frame; `gain`/`offset` change the exposure; `noise` adds up to
    /// that much sensor noise either way.
    fn film(
        world: &[f32],
        positions: &[f64],
        y: f64,
        (gain, offset, noise): (f32, f32, f32),
        fps: f64,
    ) -> Frames {
        let (w, h) = (64, 36);
        let mut data = Vec::with_capacity(positions.len() * w * h);
        for (n, &p) in positions.iter().enumerate() {
            for row in 0..h {
                for col in 0..w {
                    let (fx, fy) = (p + col as f64, y + row as f64);
                    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
                    let (tx, ty) = (fx.fract() as f32, fy.fract() as f32);
                    let g = |a: usize, b: usize| {
                        world[b.min(WORLD_H - 1) * WORLD_W + a.min(WORLD_W - 1)]
                    };
                    let top = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
                    let bottom = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
                    let hash = (n * 7919 + row * 131 + col).wrapping_mul(2_654_435_761) % 1000;
                    let grain = (hash as f32 / 500.0 - 1.0) * noise;
                    let v = (top * (1.0 - ty) + bottom * ty) * 90.0 * gain + offset + grain;
                    data.push(v.clamp(0.0, 255.0) as u8);
                }
            }
        }
        Frames {
            width: w,
            height: h,
            fps,
            data,
        }
    }

    /// Positions for a ride: `wait_before` s still at `from`, then moving at
    /// `speed(t)` px/s until `to`, then `wait_after` s still.
    fn ride(
        fps: f64,
        from: f64,
        to: f64,
        wait_before: f64,
        wait_after: f64,
        speed: impl Fn(f64) -> f64,
    ) -> Vec<f64> {
        let mut out = Vec::new();
        let mut p = from;
        for _ in 0..(wait_before * fps) as usize {
            out.push(p);
        }
        let mut t = 0.0;
        while p < to {
            out.push(p);
            p += speed(t) / fps;
            t += 1.0 / fps;
        }
        for _ in 0..(wait_after * fps) as usize {
            out.push(to);
        }
        out
    }

    /// The first frame at or past position `x`.
    fn crossing(positions: &[f64], x: f64) -> i64 {
        positions.iter().position(|p| *p >= x).unwrap() as i64
    }

    #[test]
    fn places_markers_in_a_run_at_another_speed_frame_rate_and_exposure() {
        let world = world(WORLD_W, WORLD_H, 7);
        let gates = [
            ("Start", 300.0),
            ("S1", 900.0),
            ("S2", 1400.0),
            ("Finish", 2000.0),
        ];

        let ref_pos = ride(60.0, 100.0, 2200.0, 2.0, 2.0, |t| {
            120.0 + 30.0 * (t * 0.7).sin()
        });
        let reference = film(&world, &ref_pos, 12.0, (1.0, 40.0, 4.0), 60.0);
        let markers: Vec<(String, i64)> = gates
            .iter()
            .map(|(n, x)| ((*n).to_owned(), crossing(&ref_pos, *x)))
            .collect();

        // Slower and uneven, at 30 fps, a little higher on the trail, darker.
        let new_pos = ride(30.0, 50.0, 2250.0, 6.0, 4.0, |t| {
            90.0 + 50.0 * (t * 0.45).cos()
        });
        let clip = film(&world, &new_pos, 14.0, (0.8, 20.0, 10.0), 30.0);

        let result = align(&reference, &markers, &clip).unwrap();
        assert!(result.match_cost < 0.2, "match cost {}", result.match_cost);
        for (placed, (name, x)) in result.markers.iter().zip(gates) {
            let truth = crossing(&new_pos, x);
            assert_eq!(placed.name, name);
            assert!(
                (placed.frame - truth).abs() <= 1,
                "{name}: placed at {}, true {truth}",
                placed.frame
            );
            assert!(
                placed.confidence > 0.3,
                "{name}: confidence {}",
                placed.confidence
            );
        }
    }

    /// The new rider stops mid-run for 11 s, looking down at the bike (a
    /// view that matches nothing in the reference). Keeping the cheapest
    /// *total* path into each cell once made the aligner skip everything
    /// before the stop, squeezing the whole run into what came after (#27).
    #[test]
    fn places_markers_around_a_long_stop_mid_run() {
        let world = world(WORLD_W, WORLD_H, 7);
        let elsewhere = world_at(99);
        let gates = [
            ("Start", 300.0),
            ("S1", 800.0),
            ("S2", 1500.0),
            ("Finish", 2000.0),
        ];
        let fps = 30.0;
        let ref_pos = ride(fps, 100.0, 2200.0, 2.0, 2.0, |_| 60.0);
        let reference = film(&world, &ref_pos, 12.0, (1.0, 40.0, 4.0), fps);
        let markers: Vec<(String, i64)> = gates
            .iter()
            .map(|(n, x)| ((*n).to_owned(), crossing(&ref_pos, *x)))
            .collect();

        // The same ride, but stopped at 1200 for 11 s.
        let before = ride(fps, 50.0, 1200.0, 6.0, 0.0, |_| 60.0);
        let after = ride(fps, 1200.0, 2250.0, 0.0, 4.0, |_| 60.0);
        let stop = (11.0 * fps) as usize;
        let mut clip = film(&world, &before, 14.0, (0.9, 30.0, 6.0), fps);
        let still = film(&elsewhere, &vec![500.0; stop], 20.0, (1.0, 40.0, 6.0), fps);
        clip.data.extend_from_slice(&still.data);
        let rest = film(&world, &after, 14.0, (0.9, 30.0, 6.0), fps);
        clip.data.extend_from_slice(&rest.data);
        let truth = |x: f64| {
            if x < 1200.0 {
                crossing(&before, x)
            } else {
                (before.len() + stop) as i64 + crossing(&after, x)
            }
        };

        let result = align(&reference, &markers, &clip).unwrap();
        for (placed, (name, x)) in result.markers.iter().zip(gates) {
            assert!(
                (placed.frame - truth(x)).abs() <= 2,
                "{name}: placed at {}, true {}",
                placed.frame,
                truth(x)
            );
        }
    }

    fn world_at(seed: u64) -> Vec<f32> {
        world(WORLD_W, WORLD_H, seed)
    }

    #[test]
    fn a_clip_of_somewhere_else_matches_badly() {
        let world_a = world(WORLD_W, WORLD_H, 7);
        let world_b = world(WORLD_W, WORLD_H, 99);
        let ref_pos = ride(30.0, 100.0, 2200.0, 1.0, 1.0, |_| 150.0);
        let reference = film(&world_a, &ref_pos, 12.0, (1.0, 40.0, 4.0), 30.0);
        let markers = vec![
            ("Start".to_owned(), crossing(&ref_pos, 300.0)),
            ("Finish".to_owned(), crossing(&ref_pos, 2000.0)),
        ];
        let other = film(&world_b, &ref_pos, 12.0, (1.0, 40.0, 4.0), 30.0);
        let result = align(&reference, &markers, &other).unwrap();
        assert!(result.match_cost > 0.5, "match cost {}", result.match_cost);
    }

    #[test]
    fn rejects_markers_outside_the_reference() {
        let frames = Frames {
            width: 64,
            height: 36,
            fps: 30.0,
            data: vec![0; 64 * 36 * 10],
        };
        let err = align(&frames, &[("Start".into(), 20)], &frames).unwrap_err();
        assert!(matches!(err, AlignError::MarkerOutsideReference { .. }));
        assert_eq!(
            align(&frames, &[], &frames).unwrap_err(),
            AlignError::NoMarkers
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Speed at real sizes: a 2-minute 60 fps run against a 10-minute clip.
    /// `cargo test --release -p timer-align -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn two_minute_run_in_a_ten_minute_clip() {
        let noise = |n: usize, seed: u64| {
            let mut state = seed;
            let data = (0..n * 64 * 36)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            Frames {
                width: 64,
                height: 36,
                fps: 60.0,
                data,
            }
        };
        let reference = noise(122 * 60, 1);
        let clip = noise(600 * 60, 2);
        let markers = vec![("Start".to_owned(), 60), ("Finish".to_owned(), 121 * 60)];
        let began = std::time::Instant::now();
        align(&reference, &markers, &clip).unwrap();
        eprintln!("aligned in {:.1} s", began.elapsed().as_secs_f64());
    }
}

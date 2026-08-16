//! Drawn-structure detection — "is there a picture in this spectrogram?"
//!
//! The Thargoid Probe's decoded image is line art: circles, orbital ellipses,
//! radiating spokes, grids. Natural game audio never draws that. But the naive
//! test — "does this region contain strong oriented structure?" — fires on
//! music, because a sustained harmonic *is* a perfectly coherent horizontal
//! line, and there are dozens of them in any melodic passage.
//!
//! So the discriminator is **orientation diversity**. Harmonics, engine tones,
//! and drones are all horizontal. Line art contains verticals, diagonals, and
//! curves in roughly equal measure. A region scores only when it is
//! simultaneously:
//!
//! * **oriented** — locally linear rather than mushy (structure tensor
//!   coherence),
//! * **sparse** — thin strokes on a quiet ground, not a filled block,
//! * **diverse** — its gradients point in many directions, not just one.
//!
//! This runs over the quantized `u8` waterfall the pipeline already maintains.
//! No transforms, no floating-point image, and the work is proportional to the
//! tile grid rather than to the audio.

/// Number of orientation buckets used for the diversity measure. Gradient
/// orientation is modulo 180 degrees, since a line has no direction.
const ORIENTATION_BUCKETS: usize = 12;

/// Side of the neighbourhood coherence is measured over.
///
/// Coherence *must* be local. Accumulating one structure tensor across the whole
/// region asks "does this region have a single dominant orientation", which line
/// art answers with a resounding no — a circle plus spokes measured 0.03 that
/// way. Asking instead "is each small neighbourhood locally linear" is the
/// question that separates strokes from mush.
const COHERENCE_CELL: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct StructureScore {
    /// Structure-tensor coherence, 0..1. High means locally linear.
    pub coherence: f32,
    /// Fraction of the region that is *not* bright. Line art is mostly ground.
    pub sparsity: f32,
    /// Normalized entropy of gradient orientation, 0..1. Near 1 means many
    /// directions; a page of harmonics sits near 0.
    pub orientation_diversity: f32,
    /// How much of the edge energy runs *diagonally*, 0..1.
    ///
    /// This is the shape test that matters. In a spectrogram a **vertical**
    /// stroke is a broadband transient — a click or a thump, every frequency at
    /// one instant. A **horizontal** stroke is a sustained tone: a drone, a
    /// harmonic, engine noise. Neither is a drawing. A **diagonal** stroke is a
    /// frequency sweep over time, which is exactly what a drawn line is.
    pub diagonality: f32,
    /// Combined 0..1.
    pub score: f32,
    /// Pixels that carried enough gradient to be counted.
    pub edge_pixels: usize,
}

impl StructureScore {
    pub fn empty() -> Self {
        Self {
            coherence: 0.0,
            sparsity: 0.0,
            orientation_diversity: 0.0,
            diagonality: 0.0,
            score: 0.0,
            edge_pixels: 0,
        }
    }

    pub fn is_present(&self, threshold: f32) -> bool {
        self.score >= threshold
    }
}

/// Analyze one rectangular region of a quantized spectrogram.
///
/// `image` is row-major, `width` wide, values 0..=255 where higher is louder.
/// Returns [`StructureScore::empty`] when the region is too small or too flat
/// to say anything.
pub fn analyze(image: &[u8], width: usize, height: usize) -> StructureScore {
    if width < 3 || height < 3 || image.len() < width * height {
        return StructureScore::empty();
    }

    // Per-cell structure tensors for local coherence, plus one global
    // orientation histogram for diversity.
    let cells_x = (width - 2).div_ceil(COHERENCE_CELL);
    let cells_y = (height - 2).div_ceil(COHERENCE_CELL);
    let mut cells = vec![[0.0f64; 3]; cells_x * cells_y]; // jxx, jyy, jxy
    let mut buckets = [0.0f64; ORIENTATION_BUCKETS];
    let mut edge_pixels = 0usize;
    let mut bright_pixels = 0usize;

    // Gradient magnitudes below this are quantization noise, not an edge.
    const EDGE_THRESHOLD: f32 = 12.0;
    // Everything above this counts as "ink" for the sparsity measure.
    const INK_THRESHOLD: u8 = 96;

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let at = |dx: isize, dy: isize| -> f32 {
                let xi = (x as isize + dx) as usize;
                let yi = (y as isize + dy) as usize;
                image[yi * width + xi] as f32
            };

            if image[y * width + x] >= INK_THRESHOLD {
                bright_pixels += 1;
            }

            // Sobel.
            let gx = (at(1, -1) + 2.0 * at(1, 0) + at(1, 1))
                - (at(-1, -1) + 2.0 * at(-1, 0) + at(-1, 1));
            let gy = (at(-1, 1) + 2.0 * at(0, 1) + at(1, 1))
                - (at(-1, -1) + 2.0 * at(0, -1) + at(1, -1));

            let magnitude = (gx * gx + gy * gy).sqrt();
            if magnitude < EDGE_THRESHOLD {
                continue;
            }
            edge_pixels += 1;

            let cell = ((y - 1) / COHERENCE_CELL) * cells_x + ((x - 1) / COHERENCE_CELL);
            let c = &mut cells[cell];
            c[0] += (gx * gx) as f64;
            c[1] += (gy * gy) as f64;
            c[2] += (gx * gy) as f64;

            // Orientation modulo pi, weighted by edge strength so faint noise
            // cannot manufacture diversity.
            let mut angle = gy.atan2(gx);
            if angle < 0.0 {
                angle += std::f32::consts::PI;
            }
            let bucket = ((angle / std::f32::consts::PI) * ORIENTATION_BUCKETS as f32) as usize;
            buckets[bucket.min(ORIENTATION_BUCKETS - 1)] += magnitude as f64;
        }
    }

    let interior = (width - 2) * (height - 2);
    if edge_pixels == 0 || interior == 0 {
        return StructureScore::empty();
    }

    // Mean local coherence, weighted by each cell's edge energy so empty cells
    // neither help nor hurt.
    let mut weighted = 0.0f64;
    let mut weight = 0.0f64;
    for c in &cells {
        let trace = c[0] + c[1];
        if trace <= 0.0 {
            continue;
        }
        let diff = c[0] - c[1];
        let local = (diff * diff + 4.0 * c[2] * c[2]).sqrt() / trace;
        weighted += local * trace;
        weight += trace;
    }
    let coherence = if weight > 0.0 {
        (weighted / weight) as f32
    } else {
        0.0
    };

    let sparsity = 1.0 - (bright_pixels as f32 / interior as f32);

    // Normalized Shannon entropy over the orientation histogram.
    let total: f64 = buckets.iter().sum();
    let orientation_diversity = if total > 0.0 {
        let entropy: f64 = buckets
            .iter()
            .filter(|w| **w > 0.0)
            .map(|w| {
                let p = w / total;
                -p * p.ln()
            })
            .sum();
        (entropy / (ORIENTATION_BUCKETS as f64).ln()) as f32
    } else {
        0.0
    };

    // Diagonality, weighted by edge strength.
    //
    // Gradient direction is perpendicular to the edge it belongs to, so a
    // horizontal line (a sustained tone) has a vertical gradient near pi/2, and
    // a vertical line (a broadband click) has a horizontal gradient near 0 or
    // pi. `|sin 2t|` is zero at both and one at pi/4 and 3pi/4 — precisely the
    // diagonals a swept stroke produces.
    let diagonality = if total > 0.0 {
        let weighted: f64 = buckets
            .iter()
            .enumerate()
            .map(|(bucket, weight)| {
                let angle =
                    (bucket as f64 + 0.5) * std::f64::consts::PI / ORIENTATION_BUCKETS as f64;
                weight * (2.0 * angle).sin().abs()
            })
            .sum();
        (weighted / total) as f32
    } else {
        0.0
    };

    // Multiplicative, for the same reason as the keying detector: each property
    // is necessary. A page of harmonics is coherent and sparse but scores near
    // zero on diversity, and a weighted sum would let it through.
    //
    // Diagonality enters with a floor rather than as a bare factor: a drawing
    // may legitimately contain grids and axes — the Thargoid Probe image is full
    // of them — so purely horizontal or vertical content is penalized heavily
    // but not eliminated.
    let score = (coherence.clamp(0.0, 1.0)
        * sparsity.clamp(0.0, 1.0)
        * orientation_diversity.clamp(0.0, 1.0)
        * (0.25 + 0.75 * diagonality.clamp(0.0, 1.0)))
    .clamp(0.0, 1.0);

    StructureScore {
        coherence: coherence.clamp(0.0, 1.0),
        sparsity: sparsity.clamp(0.0, 1.0),
        orientation_diversity,
        diagonality,
        score,
        edge_pixels,
    }
}

/// Sweeps a tile grid over a spectrogram and keeps the best-scoring region.
///
/// Tiling matters: a drawing occupying a corner of a five-minute waterfall would
/// be averaged into invisibility by a whole-image statistic.
#[derive(Debug, Clone)]
pub struct StructureScanner {
    pub tile_width: usize,
    pub tile_height: usize,
}

impl Default for StructureScanner {
    fn default() -> Self {
        Self {
            tile_width: 64,
            tile_height: 64,
        }
    }
}

impl StructureScanner {
    /// Best tile score in the image, with its top-left position.
    pub fn scan(
        &self,
        image: &[u8],
        width: usize,
        height: usize,
    ) -> (StructureScore, usize, usize) {
        let mut best = StructureScore::empty();
        let (mut bx, mut by) = (0usize, 0usize);
        if width < 3 || height < 3 {
            return (best, 0, 0);
        }

        let tw = self.tile_width.min(width);
        let th = self.tile_height.min(height);
        // Half-tile stride, so a drawing straddling a tile boundary is still
        // seen whole by some tile.
        let sx = (tw / 2).max(1);
        let sy = (th / 2).max(1);

        let mut tile = vec![0u8; tw * th];
        let mut y = 0;
        while y + th <= height {
            let mut x = 0;
            while x + tw <= width {
                for row in 0..th {
                    let src = (y + row) * width + x;
                    tile[row * tw..(row + 1) * tw].copy_from_slice(&image[src..src + tw]);
                }
                let score = analyze(&tile, tw, th);
                if score.score > best.score {
                    best = score;
                    bx = x;
                    by = y;
                }
                x += sx;
            }
            y += sy;
        }
        (best, bx, by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 96;
    const H: usize = 96;

    fn blank() -> Vec<u8> {
        vec![10u8; W * H]
    }

    fn plot(img: &mut [u8], x: isize, y: isize, v: u8) {
        if x >= 0 && y >= 0 && (x as usize) < W && (y as usize) < H {
            img[y as usize * W + x as usize] = v;
        }
    }

    fn hline(img: &mut [u8], y: usize, v: u8) {
        for x in 0..W {
            plot(img, x as isize, y as isize, v);
        }
    }

    fn circle(img: &mut [u8], cx: isize, cy: isize, r: isize, v: u8) {
        for step in 0..720 {
            let a = step as f32 * std::f32::consts::TAU / 720.0;
            plot(
                img,
                cx + (r as f32 * a.cos()) as isize,
                cy + (r as f32 * a.sin()) as isize,
                v,
            );
        }
    }

    fn line(img: &mut [u8], x0: isize, y0: isize, x1: isize, y1: isize, v: u8) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            plot(
                img,
                x0 + ((x1 - x0) as f32 * t) as isize,
                y0 + ((y1 - y0) as f32 * t) as isize,
                v,
            );
        }
    }

    #[test]
    fn a_blank_region_scores_nothing() {
        let s = analyze(&blank(), W, H);
        assert_eq!(s.edge_pixels, 0);
        assert_eq!(s.score, 0.0);
        assert!(!s.is_present(0.1));
    }

    #[test]
    fn degenerate_input_is_handled() {
        assert_eq!(analyze(&[], 0, 0).score, 0.0);
        assert_eq!(analyze(&[1, 2, 3], 3, 1).score, 0.0);
        // Claimed size larger than the buffer must not index out of bounds.
        assert_eq!(analyze(&[0u8; 10], 100, 100).score, 0.0);
    }

    #[test]
    fn line_art_scores_well() {
        // A circle with radiating spokes: the probe image in miniature.
        let mut img = blank();
        circle(&mut img, 48, 48, 30, 240);
        circle(&mut img, 48, 48, 18, 240);
        line(&mut img, 48, 48, 90, 20, 240);
        line(&mut img, 48, 48, 6, 20, 240);
        line(&mut img, 48, 48, 48, 92, 240);

        let s = analyze(&img, W, H);
        assert!(s.score > 0.25, "line art should score: {s:?}");
        assert!(
            s.orientation_diversity > 0.7,
            "curves span many angles: {s:?}"
        );
        assert!(s.sparsity > 0.8, "line art is mostly background: {s:?}");
        assert!(s.is_present(0.25));
    }

    #[test]
    fn stacked_harmonics_do_not_score_as_a_picture() {
        // This is the false positive that matters: sustained musical harmonics
        // are perfectly coherent, perfectly sparse horizontal lines.
        let mut img = blank();
        for k in 1..=12 {
            let y = k * 7;
            if y < H {
                hline(&mut img, y, 240);
            }
        }

        let harmonics = analyze(&img, W, H);
        assert!(
            harmonics.orientation_diversity < 0.35,
            "harmonics all point the same way: {harmonics:?}"
        );

        let mut art = blank();
        circle(&mut art, 48, 48, 30, 240);
        circle(&mut art, 48, 48, 18, 240);
        line(&mut art, 48, 48, 90, 20, 240);
        line(&mut art, 48, 48, 6, 20, 240);
        let drawn = analyze(&art, W, H);

        assert!(
            drawn.score > harmonics.score * 3.0,
            "line art {drawn:?} must clearly outrank harmonics {harmonics:?}"
        );
        assert!(!harmonics.is_present(0.25), "{harmonics:?}");
    }

    #[test]
    fn diagonals_beat_verticals_and_horizontals() {
        // The user-visible rule: real strokes are diagonal. A broadband click is
        // vertical, a drone is horizontal, and neither is a drawing.
        let mut diagonal = blank();
        line(&mut diagonal, 10, 10, 85, 85, 240);
        line(&mut diagonal, 10, 85, 85, 10, 240);

        let mut vertical = blank();
        for x in (20..80).step_by(12) {
            line(&mut vertical, x, 5, x, 90, 240);
        }

        let mut horizontal = blank();
        for y in (20..80).step_by(12) {
            hline(&mut horizontal, y as usize, 240);
        }

        let d = analyze(&diagonal, W, H);
        let v = analyze(&vertical, W, H);
        let h = analyze(&horizontal, W, H);

        assert!(d.diagonality > 0.7, "diagonal strokes: {d:?}");
        assert!(
            v.diagonality < 0.35,
            "vertical strokes are not diagonal: {v:?}"
        );
        assert!(
            h.diagonality < 0.35,
            "horizontal strokes are not diagonal: {h:?}"
        );
        assert!(
            d.score > v.score && d.score > h.score,
            "diagonal {d:?} must outrank vertical {v:?} and horizontal {h:?}"
        );
    }

    #[test]
    fn a_broadband_click_does_not_read_as_a_drawing() {
        // One instant, every frequency — a thump. Vertical, and not a signal.
        let mut img = blank();
        for x in [30isize, 31, 60, 61] {
            line(&mut img, x, 0, x, 95, 250);
        }
        let s = analyze(&img, W, H);
        assert!(s.diagonality < 0.35, "a click is vertical: {s:?}");
        assert!(!s.is_present(0.35), "{s:?}");
    }

    #[test]
    fn broadband_noise_does_not_score() {
        let mut img = blank();
        let mut state = 0xABCD_1234u32;
        for p in img.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *p = (state >> 24) as u8;
        }
        let s = analyze(&img, W, H);
        assert!(
            !s.is_present(0.25),
            "noise must not read as a drawing: {s:?}"
        );
        assert!(
            s.coherence < 0.3,
            "noise has no dominant orientation: {s:?}"
        );
    }

    #[test]
    fn a_solid_block_is_not_a_drawing() {
        // Loud but featureless: high energy, no structure, no sparsity.
        let mut img = blank();
        for y in 20..80 {
            for x in 20..80 {
                img[y * W + x] = 240;
            }
        }
        let s = analyze(&img, W, H);
        assert!(!s.is_present(0.25), "a filled block is not line art: {s:?}");
    }

    #[test]
    fn a_single_diagonal_is_coherent_but_not_diverse() {
        let mut img = blank();
        line(&mut img, 0, 0, 95, 95, 240);
        let s = analyze(&img, W, H);
        assert!(s.coherence > 0.5, "one line is highly coherent: {s:?}");
        assert!(
            s.orientation_diversity < 0.4,
            "one line points one way: {s:?}"
        );
    }

    #[test]
    fn the_scanner_finds_a_drawing_in_a_corner() {
        // A wide image with art confined to one tile — a whole-image statistic
        // would average it away.
        let (wide, tall) = (320usize, 96usize);
        let mut img = vec![10u8; wide * tall];
        for step in 0..720 {
            let a = step as f32 * std::f32::consts::TAU / 720.0;
            let x = 250 + (28.0 * a.cos()) as isize;
            let y = 48 + (28.0 * a.sin()) as isize;
            if x >= 0 && y >= 0 && (x as usize) < wide && (y as usize) < tall {
                img[y as usize * wide + x as usize] = 240;
            }
        }

        let scanner = StructureScanner::default();
        let (score, bx, _by) = scanner.scan(&img, wide, tall);
        assert!(
            score.is_present(0.2),
            "corner art should be found: {score:?}"
        );
        // The circle spans x 222..278; the winning tile must overlap it.
        assert!(
            bx < 278 && bx + scanner.tile_width > 222,
            "best tile at x={bx} does not overlap the drawing"
        );

        // The same image without the drawing scores far lower.
        let blank_wide = vec![10u8; wide * tall];
        let (empty, _, _) = scanner.scan(&blank_wide, wide, tall);
        assert!(empty.score < score.score);
    }

    #[test]
    fn the_scanner_handles_images_smaller_than_a_tile() {
        let scanner = StructureScanner {
            tile_width: 64,
            tile_height: 64,
        };
        let (s, _, _) = scanner.scan(&[0u8; 4], 2, 2);
        assert_eq!(s.score, 0.0);
    }

    #[test]
    fn scores_are_always_finite_and_bounded() {
        let mut img = blank();
        circle(&mut img, 48, 48, 30, 255);
        let s = analyze(&img, W, H);
        for v in [s.coherence, s.sparsity, s.orientation_diversity, s.score] {
            assert!(v.is_finite() && (0.0..=1.0).contains(&v), "{s:?}");
        }
    }
}

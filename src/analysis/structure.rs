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
//! * **diverse** — its gradients point in many directions, not just one,
//! * **continuous** — its ink forms *long connected strokes*.
//!
//! The last one was added after the first three failed to separate line art
//! from real game ambience. All three are **local texture** statistics, and
//! cockpit ambience has exactly that texture: harmonics are locally linear,
//! transients are sparse, and the mixture points in many directions. Measured,
//! synthetic line art scored 0.64 and real recordings 0.34–0.46, with live
//! ambience reaching 0.65 — no separation at all.
//!
//! What ambience does *not* have is extent. A drawn stroke runs for hundreds of
//! pixels; ambient texture is fragments a few pixels long. Continuity measures
//! that directly, by joining ink into connected components and asking how much
//! of it belongs to strokes that are both long and thin.
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
    /// Fraction of ink belonging to long, thin, connected strokes, 0..1.
    ///
    /// The measure that separates drawing from texture: ambience is fragments,
    /// a drawing is lines. See the module header.
    pub continuity: f32,
    /// Coherent gain from integrating along drift lines, 0..1.
    ///
    /// Continuity can only follow ink bright enough to cross a threshold. This
    /// sees lines that are not. See [`drift_search`].
    pub drift: f32,
    /// Direction of the strongest drift line, in degrees from horizontal.
    ///
    /// A measurement rather than a score: negative is a downward sweep, positive
    /// upward, and the magnitude approaches 90 for a near-vertical stroke. Zero
    /// when [`StructureScore::drift`] is zero.
    pub drift_angle_deg: f32,
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
            continuity: 0.0,
            drift: 0.0,
            drift_angle_deg: 0.0,
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
/// Median of a sliding window of `u8`, in constant time per step.
///
/// Sorting the window at every pixel made the suppression pass eight times more
/// expensive than the entire rest of the analysis — 2.41 ms per audio second
/// became 19.28, and the test suite went from 17 seconds to 150. A histogram of
/// the 256 possible values, with the median position carried between steps,
/// removes the sort entirely: each step adjusts a few counters.
struct SlidingMedian {
    counts: [u32; 256],
    total: u32,
    /// Current median value, and how many elements sit strictly below it.
    value: usize,
    below: u32,
}

impl Default for SlidingMedian {
    fn default() -> Self {
        // `[u32; 256]` is past the arity `derive(Default)` covers.
        Self {
            counts: [0; 256],
            total: 0,
            value: 0,
            below: 0,
        }
    }
}

impl SlidingMedian {
    fn reset(&mut self) {
        self.counts = [0; 256];
        self.total = 0;
        self.value = 0;
        self.below = 0;
    }

    fn add(&mut self, v: u8) {
        self.counts[v as usize] += 1;
        self.total += 1;
        if (v as usize) < self.value {
            self.below += 1;
        }
    }

    fn remove(&mut self, v: u8) {
        self.counts[v as usize] -= 1;
        self.total -= 1;
        if (v as usize) < self.value {
            self.below -= 1;
        }
    }

    /// The lower median, rebalancing from wherever the pointer was left.
    fn median(&mut self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        let target = self.total / 2;
        while self.below > target {
            self.value -= 1;
            self.below -= self.counts[self.value];
        }
        while self.below + self.counts[self.value] <= target {
            self.below += self.counts[self.value];
            self.value += 1;
        }
        self.value as u8
    }
}

/// Remove sustained tones and transients, leaving what is neither.
///
/// Harmonic/percussive source separation, the standard technique, applied for
/// its **residual** rather than for either component.
///
/// Ship ambience is two things: sustained tones, which run horizontally across a
/// spectrogram, and transients, which run vertically. A median filter along time
/// keeps only what persists — the harmonics. A median filter along frequency
/// keeps only what is broadband at an instant — the transients. Whatever is
/// left over is neither, and a drawn stroke is exactly that: diagonal or curved,
/// too short-lived to be a tone and too narrow to be a click.
///
/// Both filters are medians rather than means because a median ignores the
/// outlier instead of being dragged by it — the same reason the Morse detector
/// clusters on medians.
///
/// Returns a new image; the input is untouched.
pub fn suppress_tones_and_transients(image: &[u8], width: usize, height: usize) -> Vec<u8> {
    /// Half-width of the filter along time. A tone must persist for at least
    /// this many columns on each side to be suppressed as sustained.
    ///
    /// This must be **much larger than a stroke is wide**, or the stroke looks
    /// sustained within its own window and suppresses itself. Measured on the
    /// detector's pooled image, a drawn stroke averages 21 px across, so a
    /// radius of 8 removed every drawing along with the ambience — every source
    /// scored 0.000. At 48 the stroke is a small minority of the window while a
    /// tone spanning the image is still the majority.
    const TIME_RADIUS: usize = 48;
    /// Half-width along frequency, for transients. Same reasoning.
    const FREQ_RADIUS: usize = 48;

    if width < 3 || height < 3 || image.len() < width * height {
        return image.to_vec();
    }

    let mut harmonic = vec![0u8; width * height];
    let mut percussive = vec![0u8; width * height];
    let mut window = SlidingMedian::default();

    // Along time: what persists is a tone.
    for y in 0..height {
        let row = &image[y * width..(y + 1) * width];
        window.reset();
        let mut hi = 0usize;
        for x in 0..width {
            let want_hi = (x + TIME_RADIUS + 1).min(width);
            while hi < want_hi {
                window.add(row[hi]);
                hi += 1;
            }
            if x > TIME_RADIUS {
                window.remove(row[x - TIME_RADIUS - 1]);
            }
            harmonic[y * width + x] = window.median();
        }
    }

    // Along frequency: what is broadband at one instant is a transient.
    for x in 0..width {
        window.reset();
        let mut hi = 0usize;
        for y in 0..height {
            let want_hi = (y + FREQ_RADIUS + 1).min(height);
            while hi < want_hi {
                window.add(image[hi * width + x]);
                hi += 1;
            }
            if y > FREQ_RADIUS {
                window.remove(image[(y - FREQ_RADIUS - 1) * width + x]);
            }
            percussive[y * width + x] = window.median();
        }
    }

    // The residual. Saturating, so suppression can only ever remove ink.
    let mut residual = vec![0u8; width * height];
    for i in 0..width * height {
        let explained = harmonic[i].max(percussive[i]);
        residual[i] = image[i].saturating_sub(explained);
    }
    residual
}

/// Fraction of ink belonging to long, thin, connected strokes.
///
/// Flood-fills the ink into 8-connected components and keeps those that are
/// both long — measured by the diagonal of the bounding box — and thin, meaning
/// the component covers only a small part of that box. A filled blob is long
/// but not thin; a speck is thin but not long; a drawn stroke is both.
fn continuity(image: &[u8], width: usize, height: usize, ink_threshold: u8) -> f32 {
    /// A stroke must span at least this fraction of the region's diagonal.
    const MIN_SPAN: f32 = 0.18;
    /// Ink covering more than this share of its own bounding box is a blob.
    const MAX_FILL: f32 = 0.35;
    /// Mean width, in pixels, above which a component is not a stroke.
    ///
    /// Bounding-box fill is not enough. Ambient texture percolates into one
    /// enormous connected network that is long, extends both ways, and fills
    /// only a small part of its bounding box — it passes every other test. But
    /// its *area per unit length* is large, because it is a mesh rather than a
    /// line. A drawn stroke is a pixel or two wide however far it runs.
    /// Measured on the detector's own pooled image: synthetic line art's single
    /// stroke averages 20.9 px wide, while ambience's percolated mesh averages
    /// 84.4. Pooling by maximum thickens a one-pixel stroke considerably, so
    /// this is far above what a stroke is in the source spectrogram.
    const MAX_MEAN_WIDTH: f32 = 40.0;
    /// A stroke must also extend in *both* axes, by this fraction of its own
    /// longer side.
    ///
    /// This is what separates a drawing from a held note. A sustained harmonic
    /// is a long, thin, perfectly connected horizontal line — it passes every
    /// other test here. A broadband click is the same thing vertically. Only
    /// something *drawn* turns. Without this rule the three field recordings
    /// scored 0.67–0.70 against line art's 0.98, because ordinary ambience is
    /// full of sustained tones.
    const MIN_MINOR: f32 = 0.15;

    let mut seen = vec![false; width * height];
    let mut stack: Vec<usize> = Vec::new();
    let region_diagonal = ((width * width + height * height) as f32).sqrt();
    let mut ink_total = 0usize;
    let mut ink_in_strokes = 0usize;

    for start in 0..width * height {
        if seen[start] || image[start] < ink_threshold {
            continue;
        }
        stack.push(start);
        seen[start] = true;

        let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0usize, usize::MAX, 0usize);
        let mut area = 0usize;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % width, i / width);
            area += 1;
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    let nx = x as isize + dx;
                    let ny = y as isize + dy;
                    if nx < 0 || ny < 0 || nx >= width as isize || ny >= height as isize {
                        continue;
                    }
                    let j = ny as usize * width + nx as usize;
                    if !seen[j] && image[j] >= ink_threshold {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }

        ink_total += area;
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let span = ((w * w + h * h) as f32).sqrt();
        let fill = area as f32 / (w * h) as f32;
        let (minor, major) = if w < h { (w, h) } else { (h, w) };
        let turns = minor as f32 >= MIN_MINOR * major as f32;
        let mean_width = area as f32 / span.max(1.0);
        if span >= MIN_SPAN * region_diagonal
            && fill <= MAX_FILL
            && turns
            && mean_width <= MAX_MEAN_WIDTH
        {
            ink_in_strokes += area;
        }
    }

    if ink_total == 0 {
        0.0
    } else {
        ink_in_strokes as f32 / ink_total as f32
    }
}

/// Slopes tried per side, per axis. Total trials are four times this.
///
/// Resolution is set by the requirement that a line stay inside its own
/// accumulator row: over a 64-pixel tile, adjacent slopes must differ by less
/// than `2/64`, so a range of one covers in about 32 steps.
const SLOPE_STEPS: usize = 32;

/// Shallowest slope worth integrating, in rows per column.
///
/// Below this a "line" is a sustained tone, which is what the suppression pass
/// upstream is already removing; integrating along it would only recover what
/// was deliberately discarded. The transposed pass applies the same floor, which
/// is what excludes broadband transients.
const MIN_SLOPE: f32 = 0.08;

/// A line must cross at least this fraction of the tile to be counted, so the
/// peak cannot be a corner clipped by two or three pixels.
const MIN_COVERAGE: f32 = 0.6;

/// Excess sigma at which drift is called certain.
const DRIFT_FULL_SIGMA: f32 = 4.0;

/// Coherent integration along candidate drift lines — a Radon transform.
///
/// [`continuity`] can only follow ink that crosses a brightness threshold. A
/// stroke fainter than that never becomes ink at all, so a quietly drawn picture
/// scores zero however long and clean its lines are. Integrating *along* a
/// candidate line instead sums the stroke coherently while noise adds in
/// quadrature: a line `n` pixels long stands `sqrt(n)` further out of the noise
/// than any single pixel of it does. Over a 64-pixel tile that is a factor of
/// eight, which is the entire point — it reaches strokes local statistics cannot
/// see.
///
/// This is how SETI finds drifting carriers and how radar finds tracks.
///
/// The subtlety is that the peak is a **maximum over many trials**, and the
/// maximum of `n` noise samples grows like `sqrt(2 ln n)` whether or not
/// anything is there. Reporting raw sigma would therefore manufacture a
/// detection out of an empty tile. What is returned is the excess over that
/// expectation, so noise sits at zero by construction.
///
/// Returns the excess mapped to 0..1 and the angle that produced it.
fn drift_search(image: &[u8], width: usize, height: usize) -> (f32, f32) {
    if width < 8 || height < 8 {
        return (0.0, 0.0);
    }

    // Mean-subtracted once, and shared by every slope: the accumulator sums must
    // be centred on zero or a bright tile would outrank a drawn one.
    let mean = image[..width * height].iter().map(|v| *v as f32).sum::<f32>()
        / (width * height) as f32;
    let centred: Vec<f32> = image[..width * height]
        .iter()
        .map(|v| *v as f32 - mean)
        .collect();
    let transposed: Vec<f32> = (0..width * height)
        .map(|i| centred[(i % height) * width + i / height])
        .collect();

    let mut best = (0.0f32, 0.0f32);
    for (data, w, h, steep) in [
        (&centred, width, height, false),
        (&transposed, height, width, true),
    ] {
        let (sigma, slope, trials) = sweep(data, w, h);
        if trials < 2 {
            continue;
        }
        // Gumbel: the largest of `trials` standard normals lands here on average.
        let expected_max = (2.0 * (trials as f32).ln()).sqrt();
        let drift = ((sigma - expected_max) / DRIFT_FULL_SIGMA).clamp(0.0, 1.0);
        if drift > best.0 {
            // In the transposed pass a slope of `s` rows per column describes a
            // line that is `1/s` in the original, hence the complement.
            let angle = if steep {
                slope.signum() * (90.0 - slope.abs().atan().to_degrees())
            } else {
                slope.atan().to_degrees()
            };
            best = (drift, angle);
        }
    }
    best
}

/// One orientation of [`drift_search`]: the peak in sigma, its slope, and how
/// many independent trials produced it.
fn sweep(data: &[f32], width: usize, height: usize) -> (f32, f32, usize) {
    let centre = width as f32 / 2.0;
    let min_cover = (MIN_COVERAGE * width as f32) as usize;
    let mut acc = vec![0.0f32; height];
    let mut count = vec![0u32; height];
    // Every normalized sum, pooled across slopes: they share a noise scale, so
    // the distribution of the lot is the yardstick the peak is measured against.
    let mut pooled: Vec<f32> = Vec::with_capacity(height * SLOPE_STEPS * 2);
    let mut peak = (f32::NEG_INFINITY, 0.0f32);

    for step in 0..SLOPE_STEPS * 2 {
        // Slopes from -1 to 1, skipping the shallow band around zero.
        let t = step as f32 / (SLOPE_STEPS * 2 - 1) as f32;
        let slope = -1.0 + 2.0 * t;
        if slope.abs() < MIN_SLOPE {
            continue;
        }

        acc.fill(0.0);
        count.fill(0);
        for x in 0..width {
            let shift = ((x as f32 - centre) * slope).round() as isize;
            let lo = shift.max(0) as usize;
            let hi = (height as isize + shift).clamp(0, height as isize) as usize;
            for (offset, slot) in acc.iter_mut().enumerate().take(hi).skip(lo) {
                *slot += data[(offset as isize - shift) as usize * width + x];
                count[offset] += 1;
            }
        }

        for (offset, slot) in acc.iter().enumerate() {
            if (count[offset] as usize) < min_cover {
                continue;
            }
            // Dividing by sqrt(n) rather than n keeps the noise scale constant
            // across offsets that saw different numbers of pixels, so short and
            // long lines are directly comparable.
            let z = slot / (count[offset] as f32).sqrt();
            pooled.push(z);
            if z > peak.0 {
                peak = (z, slope);
            }
        }
    }

    if pooled.len() < 8 {
        return (0.0, 0.0, 0);
    }
    pooled.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = pooled[pooled.len() / 2];
    // IQR rather than a standard deviation: the peak we are about to measure is
    // itself in this sample, and a handful of real strokes must not be allowed
    // to inflate the ruler they are measured with.
    let q1 = pooled[pooled.len() / 4];
    let q3 = pooled[3 * pooled.len() / 4];
    let spread = (q3 - q1) / 1.349;
    if !(spread > 0.0) {
        return (0.0, 0.0, 0);
    }
    ((peak.0 - median) / spread, peak.1, pooled.len())
}

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
    let continuity = continuity(image, width, height, INK_THRESHOLD);

    // Continuity carries the score, with coherence supporting it.
    //
    // Measured over synthetic line art, synthetic mountains, synthetic noise and
    // four real recordings, the previous product of coherence, sparsity,
    // diversity and diagonality did not discriminate at all — it ranked noise
    // (0.335) *above* line art (0.322). Sparsity sat between 0.93 and 0.99 on
    // every source and diagonality between 0.40 and 0.59, so neither carried any
    // information; they are kept as diagnostics, not as factors.
    //
    // Continuity separated the same corpus completely: line art 0.975, mountains
    // 0.384, noise and every real recording 0.000. Coherence remains as a
    // multiplier so that ink which happens to connect still has to be locally
    // linear to count as a stroke.
    let score =
        (continuity.clamp(0.0, 1.0) * (0.5 + 0.5 * coherence.clamp(0.0, 1.0))).clamp(0.0, 1.0);

    let (drift, drift_angle_deg) = drift_search(image, width, height);

    StructureScore {
        coherence: coherence.clamp(0.0, 1.0),
        sparsity: sparsity.clamp(0.0, 1.0),
        orientation_diversity,
        diagonality,
        continuity,
        drift,
        drift_angle_deg,
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

    /// A sustained tone is a horizontal line, and must be suppressed.
    #[test]
    fn a_held_tone_is_removed() {
        let (w, h) = (96, 96);
        let mut img = vec![0u8; w * h];
        for x in 0..w {
            img[40 * w + x] = 255;
        }
        let out = suppress_tones_and_transients(&img, w, h);
        let left: u32 = out.iter().map(|v| *v as u32).sum();
        assert!(left < 2000, "a held tone should mostly vanish, {left} left");
    }

    /// A broadband click is a vertical line, and must be suppressed too.
    #[test]
    fn a_click_is_removed() {
        let (w, h) = (96, 96);
        let mut img = vec![0u8; w * h];
        for y in 0..h {
            img[y * w + 40] = 255;
        }
        let out = suppress_tones_and_transients(&img, w, h);
        let left: u32 = out.iter().map(|v| *v as u32).sum();
        assert!(left < 2000, "a click should mostly vanish, {left} left");
    }

    /// A diagonal stroke is neither, and must survive.
    #[test]
    fn a_diagonal_stroke_survives() {
        let (w, h) = (96, 96);
        let mut img = vec![0u8; w * h];
        for i in 0..w.min(h) {
            img[i * w + i] = 255;
        }
        let before: u32 = img.iter().map(|v| *v as u32).sum();
        let after: u32 = suppress_tones_and_transients(&img, w, h)
            .iter()
            .map(|v| *v as u32)
            .sum();
        assert!(
            after as f32 > before as f32 * 0.8,
            "a drawing must survive suppression: {after} of {before}"
        );
    }

    /// The whole point: a drawing buried under ambience becomes visible.
    #[test]
    fn a_stroke_hidden_among_tones_and_clicks_is_recovered() {
        let (w, h) = (96, 96);
        let mut img = vec![0u8; w * h];
        for x in 0..w {
            img[20 * w + x] = 200;
            img[70 * w + x] = 200;
        }
        for y in 0..h {
            img[y * w + 30] = 200;
            img[y * w + 75] = 200;
        }
        for i in 0..w.min(h) {
            img[i * w + i] = 255;
        }

        // The property that matters is what *share of the surviving ink* is the
        // drawing. Continuity alone cannot show it here: the tones and clicks
        // cross the stroke, so they all join into one component that already
        // satisfies every stroke test. Real ambience does not arrive so tidily.
        let on_diagonal = |img: &[u8]| {
            let ink: u32 = img.iter().filter(|v| **v >= 96).count() as u32;
            let mine: u32 = (0..w.min(h)).filter(|i| img[i * w + i] >= 96).count() as u32;
            if ink == 0 {
                0.0
            } else {
                mine as f32 / ink as f32
            }
        };

        let cleaned = suppress_tones_and_transients(&img, w, h);
        let before = on_diagonal(&img);
        let after = on_diagonal(&cleaned);

        assert!(
            after > before * 2.0,
            "suppression should leave the drawing dominant: {before:.2} -> {after:.2}"
        );
        assert!(
            after > 0.8,
            "after suppression most surviving ink should be the stroke, got {after:.2}"
        );
    }

    /// Continuity is the metric that separates drawing from texture.
    ///
    /// Measured over synthetic line art, mountains, noise and four real
    /// recordings, the previous score ranked noise above line art. These pin
    /// the property that fixed it, so it cannot be quietly dropped.
    #[test]
    fn a_long_stroke_scores_and_scattered_specks_do_not() {
        let (w, h) = (64, 64);

        // One diagonal stroke across the region.
        let mut drawn = vec![0u8; w * h];
        for i in 0..w.min(h) {
            drawn[i * w + i] = 255;
        }
        let stroke = continuity(&drawn, w, h, 96);

        // The same amount of ink, scattered as fragments.
        let mut specks = vec![0u8; w * h];
        for i in 0..w.min(h) {
            let (x, y) = ((i * 7) % w, (i * 13) % h);
            specks[y * w + x] = 255;
        }
        let scattered = continuity(&specks, w, h, 96);

        assert!(
            stroke > 0.9,
            "a stroke spanning the region is all stroke, got {stroke:.2}"
        );
        assert!(
            scattered < 0.1,
            "disconnected specks are not strokes, got {scattered:.2}"
        );
    }

    /// A filled block is long but not thin, and must not count.
    #[test]
    fn a_solid_block_is_not_a_stroke() {
        let (w, h) = (64, 64);
        let mut block = vec![0u8; w * h];
        for y in 10..54 {
            for x in 10..54 {
                block[y * w + x] = 255;
            }
        }
        assert!(
            continuity(&block, w, h, 96) < 0.1,
            "a filled block is a blob, not a drawing"
        );
    }

    /// Empty ink is zero, not a division by zero.
    #[test]
    fn no_ink_is_no_continuity() {
        assert_eq!(continuity(&vec![0u8; 32 * 32], 32, 32, 96), 0.0);
    }

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

    /// A noisy ground for the drift tests: a stroke drawn on a blank field is
    /// not a test of anything, since the whole point is recovering a line that
    /// noise is hiding.
    fn noisy_ground(level: u8, spread: u8, seed: u32) -> Vec<u8> {
        let mut img = vec![0u8; W * H];
        let mut state = seed;
        for p in img.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((state >> 24) as i32 - 128) * spread as i32 / 128;
            *p = (level as i32 + n).clamp(0, 255) as u8;
        }
        img
    }

    #[test]
    fn drift_calibration() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("noise only", noisy_ground(90, 60, 0x1234_5678)),
            ("noise only (2)", noisy_ground(90, 60, 0xDEAD_BEEF)),
            ("faint diagonal", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                line(&mut i, 0, 10, 95, 80, 150);
                i
            }),
            ("very faint diagonal", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                line(&mut i, 0, 10, 95, 80, 115);
                i
            }),
            ("bright diagonal", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                line(&mut i, 0, 10, 95, 80, 255);
                i
            }),
            ("steep diagonal", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                line(&mut i, 10, 0, 80, 95, 150);
                i
            }),
            ("horizontal tones", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                for y in [20usize, 40, 60] {
                    hline(&mut i, y, 200);
                }
                i
            }),
            ("vertical clicks", {
                let mut i = noisy_ground(90, 60, 0x1234_5678);
                for x in [20isize, 40, 60] {
                    line(&mut i, x, 0, x, 95, 200);
                }
                i
            }),
            ("line art", {
                let mut i = blank();
                circle(&mut i, 48, 48, 30, 240);
                line(&mut i, 10, 10, 85, 85, 240);
                line(&mut i, 85, 10, 10, 85, 240);
                i
            }),
        ];
        println!("\n{:<22} {:>7} {:>9} {:>7}", "case", "drift", "angle", "cont");
        for (name, img) in &cases {
            let s = analyze(img, W, H);
            println!(
                "{name:<22} {:>7.3} {:>8.1}° {:>7.3}",
                s.drift, s.drift_angle_deg, s.continuity
            );
        }
        println!();
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

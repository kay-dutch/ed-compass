//! Following a stroke past the point where it stops being obvious.
//!
//! Every detector before this one asks a question about a *pixel*, or about a
//! small neighbourhood, and then thresholds the answer. That approach has one
//! failure mode and this project has spent a long time inside it: the bar has to
//! be high enough that noise does not cross it, which is far higher than a faint
//! signal reaches, so the faint signal is never seen at all.
//!
//! The way out is that a low bar means two completely different things depending
//! on where it is applied:
//!
//! * **Everywhere at once**, it fires on everything. Measured: dropping the
//!   effective threshold made ordinary ship ambience score higher than a real
//!   signal.
//! * **Only in continuation of something already confirmed**, it barely
//!   false-positives at all, because noise does not form long runs that keep
//!   going in the same direction.
//!
//! So this uses two: a **high** bar to decide *something is here*, and a much
//! lower one to decide *and it continues to here*. One finds, the other follows.
//! It is the second half of the Canny edge detector, and it exists for exactly
//! this reason.
//!
//! Both bars are percentiles of the image's own values rather than fixed
//! numbers. That is not fastidiousness: the structure detector applied a fixed
//! ink threshold of 96 to a residual that peaks around 49 on real recordings, so
//! it could never fire on anything a person actually recorded, and every metric
//! behind it went untested for weeks. A detector that cares about absolute
//! brightness is a detector that works only on the material it was tuned against.
//!
//! Strokes are followed **along time**, one column per step, because in a
//! spectrogram that is what they are: a frequency that moves as time passes. A
//! stroke is allowed to drift a little each step and to fade for a few steps
//! before the trail is abandoned — a drawn line survives both, a random walk
//! through noise survives neither.

/// How far a trail may deviate from where its own heading predicts, per column.
///
/// This is the constraint that makes following work at all, and it took a failed
/// test to see why. The first version searched a few rows either side of the
/// *current* row and took the brightest — which on pure noise produced a trail
/// spanning the entire image, because with seven candidates and a permissive bar
/// there is almost always something to hop to. A trail that can hop cannot die.
///
/// Following a *heading* instead removes the freedom: the trail predicts where it
/// should be from where it has been going, and may only correct by a row. Noise
/// cannot chase a prediction, because its next value has no relationship to the
/// last one. A drawn stroke is nothing but that relationship.
const MAX_DEVIATION_ROWS: isize = 1;

/// How fast the heading adapts, as a fraction of each observed step.
///
/// Low enough that one bright neighbour cannot swing the trail, high enough to
/// follow a curve. The Landscape Signal's ridges bend continuously rather than
/// running straight, so a heading that cannot turn would lose them.
const HEADING_GAIN: f32 = 0.30;

/// Rows a heading may reach, per column. A stroke steeper than this is nearly
/// vertical, which in a spectrogram is a broadband transient rather than a line.
const MAX_HEADING: f32 = 3.0;

/// Columns a trail may coast through without finding anything before it is
/// abandoned.
///
/// A drawn stroke crossing a louder feature disappears for a moment and comes
/// back; noise does not come back.
const MAX_MISSES: usize = 6;

/// Shortest run worth calling a stroke, as a fraction of the image width.
const MIN_LENGTH: f32 = 0.10;

/// Percentile that seeds a trail, and the percentile it may be followed down to.
///
/// The gap between them is the whole mechanism. Seeds are rare and confident;
/// continuation is permissive and only ever reached from a seed.
const SEED_PERCENTILE: f32 = 0.995;
const FOLLOW_PERCENTILE: f32 = 0.90;

/// One followed stroke.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// First and last column the trail occupies.
    pub x0: usize,
    pub x1: usize,
    /// Row at each column from `x0` to `x1` inclusive.
    pub rows: Vec<usize>,
    /// Rows spanned, lowest and highest.
    pub y0: usize,
    pub y1: usize,
    /// Mean image value along the trail.
    pub mean: f32,
}

impl Track {
    pub fn len(&self) -> usize {
        self.x1 - self.x0 + 1
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Total vertical travel, in rows.
    ///
    /// Reported rather than required. A previous detector excluded shallow
    /// slopes to reject slow drifts in engine noise, and in doing so excluded the
    /// Landscape Signal's own ridges, which sit at almost exactly the slope that
    /// was being thrown away. Measure it, name it, and let something further up
    /// decide what it means.
    pub fn drift_rows(&self) -> usize {
        self.y1 - self.y0
    }
}

/// What the tracer found in one image.
#[derive(Debug, Clone, Default)]
pub struct TraceResult {
    pub tracks: Vec<Track>,
    /// Length of the longest unbroken trail, as a fraction of the width.
    pub longest: f32,
    /// Fraction of columns belonging to *any* trail, 0..1.
    ///
    /// The more useful of the two, and the one to draw a box from. A stroke
    /// crossed by a broadband transient is genuinely interrupted — the tracer
    /// stops and picks it up again on the far side, which is honest rather than
    /// bridging a gap it cannot see across. Coverage counts both halves; the
    /// longest run counts one and calls the stroke half as good as it is.
    pub covered: f32,
    /// The seed and follow levels actually used, for diagnosis.
    pub seed_level: u8,
    pub follow_level: u8,
}

/// Follow every stroke that starts from a confident seed.
pub fn trace(image: &[u8], width: usize, height: usize) -> TraceResult {
    if width < 8 || height < 4 || image.len() < width * height {
        return TraceResult::default();
    }

    let (seed_level, follow_level) = levels(image);
    // A flat image has no levels worth separating, and stretching one would
    // manufacture strokes out of quantisation steps.
    if seed_level <= follow_level {
        return TraceResult::default();
    }

    let min_length = ((width as f32) * MIN_LENGTH).max(4.0) as usize;
    let mut claimed = vec![false; width * height];
    let mut tracks: Vec<Track> = Vec::new();

    // Seeds in descending brightness, so the clearest evidence claims its stroke
    // before a weaker seed nearby can wander onto it.
    let mut seeds: Vec<usize> = (0..width * height)
        .filter(|&i| image[i] >= seed_level)
        .collect();
    seeds.sort_by_key(|&i| std::cmp::Reverse(image[i]));

    for seed in seeds {
        if claimed[seed] {
            continue;
        }
        let (sx, sy) = (seed % width, seed / width);

        let trail = Trail {
            image,
            width,
            height,
            follow_level,
            seed_level,
            claimed: &claimed,
        };
        let right = trail.follow(sx, sy, 1);
        let left = trail.follow(sx, sy, -1);

        let x0 = sx - left.len();
        let x1 = sx + right.len();
        if x1 - x0 + 1 < min_length {
            continue;
        }

        let mut rows = Vec::with_capacity(x1 - x0 + 1);
        rows.extend(left.iter().rev().copied());
        rows.push(sy);
        rows.extend(right.iter().copied());

        let mut sum = 0.0f32;
        let (mut y0, mut y1) = (usize::MAX, 0usize);
        for (offset, &row) in rows.iter().enumerate() {
            let x = x0 + offset;
            claimed[row * width + x] = true;
            sum += image[row * width + x] as f32;
            y0 = y0.min(row);
            y1 = y1.max(row);
        }

        tracks.push(Track {
            x0,
            x1,
            mean: sum / rows.len() as f32,
            rows,
            y0,
            y1,
        });
    }

    let longest = tracks
        .iter()
        .map(|t| t.len() as f32 / width as f32)
        .fold(0.0f32, f32::max);

    let mut columns = vec![false; width];
    for t in &tracks {
        for column in columns.iter_mut().take(t.x1 + 1).skip(t.x0) {
            *column = true;
        }
    }
    let covered = columns.iter().filter(|c| **c).count() as f32 / width as f32;

    TraceResult {
        tracks,
        longest,
        covered,
        seed_level,
        follow_level,
    }
}

/// Everything a trail needs to walk: the image it is crossing, and the two
/// levels that decide whether it continues.
struct Trail<'a> {
    image: &'a [u8],
    width: usize,
    height: usize,
    follow_level: u8,
    seed_level: u8,
    claimed: &'a [bool],
}

impl Trail<'_> {
    /// Walk one direction from a seed, returning the row at each column.
    fn follow(&self, sx: usize, sy: usize, step: isize) -> Vec<usize> {
        let (image, width, height) = (self.image, self.width, self.height);
        let (follow_level, seed_level, claimed) =
            (self.follow_level, self.seed_level, self.claimed);
        let mut rows = Vec::new();
        let mut y = sy as f32;
        // Rows per column, in the direction of travel. Unknown at the seed, so the
        // trail starts by looking straight ahead and learns as it goes.
        let mut heading = 0.0f32;
        let mut x = sx as isize + step;
        let mut misses = 0usize;

        while x >= 0 && (x as usize) < width {
            // Where the trail expects to be, not where it happens to be bright.
            let predicted = y + heading * step as f32;
            let centre = predicted.round() as isize;
            let lo = (centre - MAX_DEVIATION_ROWS).max(0);
            let hi = (centre + MAX_DEVIATION_ROWS).min(height as isize - 1);
            if lo > hi {
                break;
            }
            let mut best = (0u8, centre.clamp(0, height as isize - 1));
            for candidate in lo..=hi {
                let v = image[candidate as usize * width + x as usize];
                if v > best.0 {
                    best = (v, candidate);
                }
            }

            // Continuation is a weak claim; *re-acquisition* is not.
            //
            // While the trail is coasting it does not know where the stroke is, only
            // where it ought to be, so accepting the first thing above the follow
            // level lands it on whatever noise is nearest the prediction — measured,
            // a trail crossed a four-column gap and then wandered twenty rows off the
            // stroke it had been following. Picking the trail back up therefore
            // demands the same evidence that started it.
            let required = if misses == 0 {
                follow_level
            } else {
                seed_level
            };
            if best.0 >= required && !claimed[best.1 as usize * width + x as usize] {
                let observed = (best.1 as f32 - y) * step as f32;
                heading = (heading * (1.0 - HEADING_GAIN) + observed * HEADING_GAIN)
                    .clamp(-MAX_HEADING, MAX_HEADING);
                y = best.1 as f32;
                misses = 0;
            } else {
                // Coast: keep the current row and spend a miss. A stroke crossing
                // something louder vanishes for a moment and returns.
                misses += 1;
                if misses > MAX_MISSES {
                    // The coasted tail was never really there, so give it back.
                    rows.truncate(rows.len().saturating_sub(MAX_MISSES));
                    break;
                }
                // Coast along the heading rather than standing still, so a stroke
                // crossing a louder feature is picked up where it re-emerges.
                y = predicted.clamp(0.0, height as f32 - 1.0);
            }
            rows.push(y.round().clamp(0.0, height as f32 - 1.0) as usize);
            x += step;
        }
        rows
    }
}

/// Seed and follow levels, as percentiles of this image's own values.
fn levels(image: &[u8]) -> (u8, u8) {
    let mut histogram = [0u32; 256];
    for v in image {
        histogram[*v as usize] += 1;
    }
    let total = image.len() as f32;
    let at = |fraction: f32| -> u8 {
        let target = (total * fraction) as u32;
        let mut seen = 0u32;
        for (value, count) in histogram.iter().enumerate() {
            seen += *count;
            if seen >= target {
                return value as u8;
            }
        }
        255
    };
    (at(SEED_PERCENTILE), at(FOLLOW_PERCENTILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 256;
    const H: usize = 128;

    /// Deterministic noise, so a stroke has somewhere to hide.
    fn noisy(level: u8, spread: u8, seed: u32) -> Vec<u8> {
        let mut img = vec![0u8; W * H];
        let mut state = seed;
        for p in img.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((state >> 24) as i32 - 128) * spread as i32 / 128;
            *p = (level as i32 + n).clamp(0, 255) as u8;
        }
        img
    }

    /// Draw a stroke that starts bright and fades, which is the case the whole
    /// module exists for.
    fn fading_stroke(img: &mut [u8], from_value: u8, to_value: u8) {
        for x in 0..W {
            let t = x as f32 / (W - 1) as f32;
            let y = 40 + (t * 30.0) as usize;
            let v = from_value as f32 + (to_value as f32 - from_value as f32) * t;
            let p = y * W + x;
            img[p] = img[p].max(v as u8);
        }
    }

    #[test]
    fn noise_alone_produces_no_long_trails() {
        let img = noisy(90, 50, 0xBEEF);
        let r = trace(&img, W, H);
        assert!(
            r.longest < 0.5,
            "noise produced a trail {:.0}% of the width",
            r.longest * 100.0
        );
        assert!(
            r.covered < 0.6,
            "and should not carpet the image, covered {:.0}%",
            r.covered * 100.0
        );
    }

    /// The claim the module is for: a stroke is followed past the point where it
    /// would ever have been detected on its own.
    #[test]
    fn a_stroke_is_followed_where_it_fades_below_its_seed() {
        let mut img = noisy(90, 50, 0x1234);
        // Bright enough to seed at one end, well inside the noise at the other.
        fading_stroke(&mut img, 255, 130);
        let r = trace(&img, W, H);
        assert!(
            r.longest > 0.7,
            "should follow most of the stroke, got {:.0}% (seed {} follow {})",
            r.longest * 100.0,
            r.seed_level,
            r.follow_level
        );
    }

    /// Scale invariance. The fixed ink threshold of 96 against a residual peaking
    /// at 49 is the bug that made an entire detector inert on real recordings;
    /// this asserts the same mistake cannot be made here.
    #[test]
    fn halving_every_value_changes_nothing() {
        let mut bright = noisy(90, 50, 0x77);
        fading_stroke(&mut bright, 255, 130);
        let dim: Vec<u8> = bright.iter().map(|v| v / 2).collect();

        let a = trace(&bright, W, H);
        let b = trace(&dim, W, H);
        assert!(
            (a.longest - b.longest).abs() < 0.1,
            "brightness must not matter: {:.2} bright, {:.2} dim",
            a.longest,
            b.longest
        );
        assert!(b.longest > 0.7, "and the dim one must still be found");
    }

    #[test]
    fn a_trail_survives_a_short_interruption() {
        let mut img = noisy(90, 40, 0x99);
        fading_stroke(&mut img, 255, 200);
        // Erase a few columns, as a louder feature crossing it would.
        for x in 120..124 {
            for y in 0..H {
                img[y * W + x] = 60;
            }
        }
        let r = trace(&img, W, H);
        // The trail is cut by the blackout and resumes beyond it — two tracks,
        // together covering nearly everything. That is the honest reading: the
        // stroke really is interrupted there.
        assert!(
            r.covered > 0.9,
            "the stroke should still be accounted for, covered {:.0}%",
            r.covered * 100.0
        );
        assert!(
            r.tracks.len() >= 2,
            "and reported as interrupted rather than bridged"
        );
    }

    #[test]
    fn drift_is_measured_and_not_required() {
        let mut img = noisy(90, 40, 0xAB);
        // A perfectly flat line: a held tone, not a drawing.
        for x in 0..W {
            img[50 * W + x] = 255;
        }
        let r = trace(&img, W, H);
        let flat = r.tracks.iter().find(|t| t.len() > W / 2);
        assert!(flat.is_some(), "a tone is still followed");
        assert_eq!(
            flat.unwrap().drift_rows(),
            0,
            "and reported as having no drift, for something else to judge"
        );
    }

    #[test]
    fn degenerate_input_is_handled() {
        assert_eq!(trace(&[], 0, 0).longest, 0.0);
        assert_eq!(trace(&vec![0u8; W * H], W, H).longest, 0.0);
        assert_eq!(trace(&vec![200u8; W * H], W, H).longest, 0.0);
        assert_eq!(trace(&[0u8; 16], 4, 4).longest, 0.0);
    }
}

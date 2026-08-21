//! Epoch folding — averaging a spectrogram against its own period.
//!
//! Every other detector in this project asks what a signal *looks like*. This
//! one asks whether it **comes back**, which for the signals we hunt is a far
//! stronger question, and the only one ordinary ship ambience cannot accidentally
//! answer yes to.
//!
//! Fold a recording at period `P` — cut it into `P`-length strips and average
//! them on top of one another — and anything that repeats at `P` adds
//! coherently, growing with the number of cycles `N`, while everything else adds
//! in quadrature and grows only as `sqrt(N)`. The contrast between them improves
//! as `sqrt(N)`. The long-term tier holds an hour, which for the Landscape
//! Signal's 109.5-second cycle is 33 repetitions and about 15 dB — the
//! difference between a mountain buried in engine noise and a mountain.
//!
//! This is how radio astronomy finds pulsars, and the situation is close enough
//! to be worth stating plainly: a faint, strictly periodic source, observed for
//! far longer than one period, against a background that is loud but has no
//! period of its own.
//!
//! Two things make it fit here particularly well:
//!
//! * **It cancels the thing we could not otherwise reject.** Ambience defeated
//!   every shape metric tried, because ambient texture genuinely is locally
//!   linear, sparse and directionally diverse. It is not, however, periodic, and
//!   folding is indifferent to what noise looks like.
//! * **It costs almost nothing.** One pass over the long-term tier — a few
//!   hundred thousand additions — against an hour of audio.
//!
//! The period does not have to be known. Folding at the wrong period smears a
//! real signal across phase and flattens the result, so folding at many
//! candidates and keeping the sharpest is itself a period search — the standard
//! one, and more sensitive than autocorrelation for a signal that is weak but
//! strictly repeating.

use crate::analysis::spectrogram::SpectrogramHistory;

/// One cycle, averaged over however many were available.
#[derive(Debug, Clone)]
pub struct Folded {
    /// Frequency rows, matching the history's frame width.
    pub bands: usize,
    /// Columns across one cycle.
    pub phases: usize,
    /// `bands * phases` mean values in dB, row-major, row 0 first.
    pub mean_db: Vec<f32>,
    /// How many complete cycles went into it. The improvement over a single
    /// cycle is roughly the square root of this.
    pub cycles: f32,
    /// The period folded at, in seconds.
    pub period_seconds: f32,
}

impl Folded {
    /// Quantize to the `u8` image the structure detector consumes.
    ///
    /// Scaled by the fold's *own* range rather than an absolute dB window: the
    /// point of folding is that what survives is faint, and a fixed window would
    /// throw away exactly the contrast that was just bought. This is the
    /// normalisation whose absence made the structure detector unable to score a
    /// real recording at all.
    pub fn to_image(&self) -> Vec<u8> {
        let finite: Vec<f32> = self.mean_db.iter().copied().filter(|v| v.is_finite()).collect();
        if finite.is_empty() {
            return vec![0; self.bands * self.phases];
        }
        let mut sorted = finite.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Robust ends, so one hot cell cannot set the scale for the picture.
        let low = sorted[sorted.len() / 100];
        let high = sorted[sorted.len() - 1 - sorted.len() / 100];
        let span = (high - low).max(1e-3);
        self.mean_db
            .iter()
            .map(|v| {
                if !v.is_finite() {
                    return 0;
                }
                (((v - low) / span).clamp(0.0, 1.0) * 255.0) as u8
            })
            .collect()
    }

    /// How much the folded image stands out from a flat one.
    ///
    /// The statistic an epoch-folding search maximises. A real period produces a
    /// fold where the phase axis carries structure; a wrong one smears it flat.
    /// Measured as the spread *along phase* relative to the spread *within* each
    /// phase bin, so a band that is simply loud contributes nothing.
    pub fn sharpness(&self) -> f32 {
        if self.bands == 0 || self.phases < 4 {
            return 0.0;
        }
        let mut between = 0.0f64;
        let mut within = 0.0f64;
        let mut counted = 0usize;
        for band in 0..self.bands {
            let row = &self.mean_db[band * self.phases..(band + 1) * self.phases];
            let finite: Vec<f32> = row.iter().copied().filter(|v| v.is_finite()).collect();
            if finite.len() < 4 {
                continue;
            }
            let mean = finite.iter().sum::<f32>() / finite.len() as f32;
            let variance = finite.iter().map(|v| (v - mean).powi(2)).sum::<f32>()
                / finite.len() as f32;
            // Neighbour differences estimate the noise left after folding; the
            // full variance includes any real structure along phase.
            let mut diffs = 0.0f32;
            for pair in finite.windows(2) {
                diffs += (pair[1] - pair[0]).powi(2);
            }
            let noise = diffs / (2.0 * (finite.len() - 1) as f32);
            between += variance as f64;
            within += noise as f64;
            counted += 1;
        }
        if counted == 0 || within <= 0.0 {
            return 0.0;
        }
        // 1.0 means "no more structure along phase than between neighbours",
        // which is what noise gives.
        ((between / within) as f32 - 1.0).max(0.0)
    }
}

/// Fold `history` at `period_seconds`.
///
/// Returns `None` when there is not enough history for at least two complete
/// cycles — one cycle cannot evidence its own repetition, and folding it would
/// simply return the recording.
pub fn fold(
    history: &SpectrogramHistory,
    fps: f32,
    period_seconds: f32,
    phases: usize,
) -> Option<Folded> {
    let bands = history.frame_width();
    let frames = history.len();
    if bands == 0 || frames == 0 || fps <= 0.0 || period_seconds <= 0.0 || phases < 4 {
        return None;
    }
    let frames_per_cycle = period_seconds * fps;
    let cycles = frames as f32 / frames_per_cycle;
    if cycles < 2.0 {
        return None;
    }

    // Never ask for more phase bins than the cycle has frames to fill them.
    //
    // Asking for 256 bins from a 125-frame cycle leaves every other bin empty,
    // and the fold comes out as a comb of vertical stripes that looks like
    // structure and is purely an artefact of the sampling. Averaging cannot
    // invent resolution the recording does not have.
    let phases = phases.min(frames_per_cycle.floor() as usize).max(4);

    let mut sum = vec![0.0f64; bands * phases];
    let mut count = vec![0u32; bands * phases];
    let range = history.range();

    for (index, frame) in history.iter().enumerate() {
        // Phase of this frame within the cycle.
        let phase = ((index as f32 / frames_per_cycle).fract() * phases as f32) as usize;
        let phase = phase.min(phases - 1);
        for (band, q) in frame.iter().enumerate().take(bands) {
            let slot = band * phases + phase;
            sum[slot] += range.dequantize(*q) as f64;
            count[slot] += 1;
        }
    }

    let mean_db = sum
        .iter()
        .zip(count.iter())
        .map(|(s, c)| {
            if *c == 0 {
                f32::NAN
            } else {
                (*s / *c as f64) as f32
            }
        })
        .collect();

    Some(Folded {
        bands,
        phases,
        mean_db,
        cycles,
        period_seconds,
    })
}

/// Search a range of periods and keep the sharpest fold.
///
/// This is the period search, not merely a refinement of one: folding at a wrong
/// period smears any real repetition across phase, so sharpness peaks at the
/// true period and nowhere else. Slower than autocorrelation and considerably
/// more sensitive, which is the trade this project wants — the whole difficulty
/// is signals too faint for the cheap methods.
pub fn search(
    history: &SpectrogramHistory,
    fps: f32,
    min_period: f32,
    max_period: f32,
    phases: usize,
) -> Option<Folded> {
    let frames = history.len();
    if frames == 0 || fps <= 0.0 {
        return None;
    }
    let span_seconds = frames as f32 / fps;
    let max_period = max_period.min(span_seconds / 2.0);
    if max_period <= min_period {
        return None;
    }

    // Step finely enough that a cycle does not drift more than one phase bin
    // across the whole observation, which is what sets the resolution of any
    // folding search.
    let cycles = span_seconds / max_period;
    let step = (max_period / (phases as f32 * cycles.max(1.0))).max(0.05);

    let mut best: Option<Folded> = None;
    let mut period = min_period;
    while period <= max_period {
        if let Some(folded) = fold(history, fps, period, phases)
            && best
                .as_ref()
                .is_none_or(|b| folded.sharpness() > b.sharpness())
        {
            best = Some(folded);
        }
        period += step;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::spectrogram::DbRange;

    const BANDS: usize = 32;

    /// Build a history at 1 fps: `f(band, second) -> dB`.
    fn history(seconds: usize, mut f: impl FnMut(usize, usize) -> f32) -> SpectrogramHistory {
        let mut h = SpectrogramHistory::new(BANDS, seconds.max(1), DbRange::default());
        for t in 0..seconds {
            let frame: Vec<f32> = (0..BANDS).map(|b| f(b, t)).collect();
            h.push_db(&frame);
        }
        h
    }

    fn noise(seed: u32) -> impl FnMut(usize, usize) -> f32 {
        let mut state = seed;
        move |_, _| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            -80.0 + ((state >> 24) as f32 / 255.0) * 20.0
        }
    }

    #[test]
    fn one_cycle_cannot_be_folded() {
        let h = history(100, noise(1));
        assert!(fold(&h, 1.0, 60.0, 32).is_none(), "60 s in a 100 s window");
        assert!(fold(&h, 1.0, 40.0, 32).is_some(), "40 s fits twice");
    }

    #[test]
    fn noise_folds_flat() {
        let h = history(1000, noise(0xABCD));
        let f = fold(&h, 1.0, 50.0, 32).expect("foldable");
        assert!(
            f.sharpness() < 1.0,
            "noise must not look periodic, sharpness {:.2}",
            f.sharpness()
        );
    }

    /// The property the whole method rests on: a repeating pattern buried in
    /// noise that is louder than it is, recovered by averaging cycles.
    #[test]
    fn a_signal_quieter_than_the_noise_is_recovered_by_folding() {
        let period = 50.0;
        let mut rng = noise(0x1234);
        let h = history(2000, |band, t| {
            let floor = rng(band, t);
            // A bump in a few bands, at one phase of the cycle, 6 dB — well
            // under the 20 dB of noise it is sitting in.
            let phase = t % period as usize;
            if (10..14).contains(&band) && (20..26).contains(&phase) {
                floor + 6.0
            } else {
                floor
            }
        });

        let right = fold(&h, 1.0, period, 50).expect("foldable");
        let wrong = fold(&h, 1.0, period * 1.37, 50).expect("foldable");
        assert!(
            right.sharpness() > wrong.sharpness() * 3.0,
            "the true period must stand out: {:.2} at {period} s against {:.2} at the wrong one",
            right.sharpness(),
            wrong.sharpness()
        );
        assert!(
            right.cycles >= 39.0,
            "2000 s at 50 s should be 40 cycles, got {:.1}",
            right.cycles
        );
    }

    #[test]
    fn the_search_finds_the_period_without_being_told() {
        let period = 60.0;
        let mut rng = noise(0x9999);
        let h = history(2400, |band, t| {
            let floor = rng(band, t);
            let phase = t % period as usize;
            if (8..12).contains(&band) && (5..15).contains(&phase) {
                floor + 8.0
            } else {
                floor
            }
        });
        let found = search(&h, 1.0, 30.0, 200.0, 48).expect("a fold");
        assert!(
            (found.period_seconds - period).abs() <= 3.0,
            "expected about {period} s, found {:.1} s",
            found.period_seconds
        );
    }

    #[test]
    fn the_image_uses_the_folds_own_range() {
        // Faint structure must still fill the output range — the failure that
        // made the structure detector blind to real recordings.
        let mut rng = noise(0x77);
        let h = history(1000, |band, t| {
            let floor = rng(band, t);
            if (4..8).contains(&band) && (t % 50) < 10 {
                floor + 3.0
            } else {
                floor
            }
        });
        let f = fold(&h, 1.0, 50.0, 32).expect("foldable");
        let image = f.to_image();
        let max = image.iter().copied().max().unwrap_or(0);
        assert!(
            max > 200,
            "a faint fold must still reach the top of the range, peaked at {max}"
        );
    }

    #[test]
    fn degenerate_input_is_handled() {
        let h = history(10, noise(3));
        assert!(fold(&h, 0.0, 10.0, 32).is_none());
        assert!(fold(&h, 1.0, 0.0, 32).is_none());
        assert!(fold(&h, 1.0, 2.0, 2).is_none());
        assert!(search(&h, 1.0, 100.0, 50.0, 32).is_none());
    }
}

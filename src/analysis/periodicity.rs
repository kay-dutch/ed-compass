//! Periodicity estimation by autocorrelation.
//!
//! The Landscape Signal repeats every ~109.5 seconds. Finding that period in
//! live audio is the single most specific test we can run: broadband noise, game
//! music, and engine hum have nothing at that lag.
//!
//! This runs on the long-term spectral summary (~1 frame/s), not on raw PCM.
//! At that rate the raw lag resolution is one second, so the peak is refined by
//! parabolic interpolation — without it a 109.5 s period could only ever be
//! reported as 109 or 110.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex32;

#[derive(Debug, Clone, PartialEq)]
pub struct PeriodicityResult {
    pub period_seconds: f32,
    /// Normalized autocorrelation at the peak, 0..1.
    pub confidence: f32,
    /// How far the peak stands above the median of the searched range. This is
    /// what separates a genuine repeat from a broad, slowly-decaying curve.
    pub prominence: f32,
    /// The selected fundamental lag in frames, before interpolation.
    pub peak_lag_frames: usize,
}

/// How close to the tallest peak a shorter-lag local maximum must be before it
/// is preferred as the fundamental.
const FUNDAMENTAL_TOLERANCE: f32 = 0.9;

/// Normalized autocorrelation, index = lag in frames.
///
/// The series is mean-subtracted first, so a constant offset (which every dB
/// series has) does not swamp the result. `r[0]` is 1.0 by construction; a
/// constant or empty input yields an empty vector rather than a division by
/// zero.
pub fn autocorrelation(series: &[f32]) -> Vec<f32> {
    let n = series.len();
    if n < 2 {
        return Vec::new();
    }
    let mean = series.iter().map(|v| *v as f64).sum::<f64>() / n as f64;
    let centered: Vec<f32> = series.iter().map(|v| (*v as f64 - mean) as f32).collect();

    let variance: f64 = centered.iter().map(|v| (*v as f64) * (*v as f64)).sum();
    if variance <= 1e-20 {
        return Vec::new(); // perfectly flat: no periodicity to speak of
    }

    // Zero-pad past 2n so the circular correlation is a linear one.
    let padded = (2 * n).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(padded);
    let ifft = planner.plan_fft_inverse(padded);

    let mut input = fft.make_input_vec();
    input[..n].copy_from_slice(&centered);
    let mut spectrum = fft.make_output_vec();
    if fft.process(&mut input, &mut spectrum).is_err() {
        return Vec::new();
    }
    for c in spectrum.iter_mut() {
        *c = Complex32::new(c.norm_sqr(), 0.0);
    }
    let mut corr = ifft.make_output_vec();
    if ifft.process(&mut spectrum, &mut corr).is_err() {
        return Vec::new();
    }

    let zero = corr[0];
    if zero.abs() <= f32::EPSILON {
        return Vec::new();
    }
    // Divide by the count at each lag as well as by r[0]: without it, long lags
    // are penalized purely for having fewer overlapping samples and a real
    // 109 s repeat in a 300 s window looks weaker than it is.
    corr[..n]
        .iter()
        .enumerate()
        .map(|(lag, v)| {
            let overlap = (n - lag) as f32 / n as f32;
            (v / zero / overlap).clamp(-1.0, 1.0)
        })
        .collect()
}

/// Refine a discrete peak to sub-frame resolution by fitting a parabola through
/// the peak and its two neighbours. Returns the offset from `peak` in frames,
/// in `[-0.5, 0.5]`.
fn parabolic_offset(curve: &[f32], peak: usize) -> f32 {
    if peak == 0 || peak + 1 >= curve.len() {
        return 0.0;
    }
    let (a, b, c) = (curve[peak - 1], curve[peak], curve[peak + 1]);
    let denom = a - 2.0 * b + c;
    if denom.abs() < 1e-12 {
        return 0.0;
    }
    (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
}

/// Search for a dominant repeat period within a lag window.
///
/// `fps` is the frame rate of `series`. Returns `None` when the series is too
/// short to contain the requested lags, or holds no usable variation.
pub fn estimate_period(
    series: &[f32],
    fps: f32,
    min_period_seconds: f32,
    max_period_seconds: f32,
) -> Option<PeriodicityResult> {
    if fps <= 0.0 || min_period_seconds <= 0.0 || max_period_seconds <= min_period_seconds {
        return None;
    }
    let curve = autocorrelation(series);
    if curve.is_empty() {
        return None;
    }

    let min_lag = ((min_period_seconds * fps).round() as usize).max(1);
    // Never trust a lag beyond half the series: fewer than two repeats is not
    // evidence of a period.
    let max_lag = ((max_period_seconds * fps).round() as usize).min(curve.len() / 2);
    if max_lag <= min_lag {
        return None;
    }

    let range = &curve[min_lag..=max_lag];
    let (offset, &global_max) = range
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    // Autocorrelation peaks just as strongly at every multiple of the true
    // period, so taking the global maximum reports 219 s for a 109.5 s signal
    // as readily as the right answer. Walk up from the shortest lag instead and
    // take the first local maximum that is nearly as tall as the best one — the
    // fundamental, not one of its harmonics.
    let mut peak_lag = min_lag + offset;
    if global_max > 0.0 {
        let threshold = global_max * FUNDAMENTAL_TOLERANCE;
        for lag in min_lag..=max_lag {
            let rising_into = lag == min_lag || curve[lag] >= curve[lag - 1];
            let falling_out = lag == max_lag || curve[lag] >= curve[lag + 1];
            if curve[lag] >= threshold && rising_into && falling_out {
                peak_lag = lag;
                break;
            }
        }
    }
    let peak_value = curve[peak_lag];

    let mut sorted: Vec<f32> = range.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];

    let refined = peak_lag as f32 + parabolic_offset(&curve, peak_lag);
    Some(PeriodicityResult {
        period_seconds: refined / fps,
        confidence: peak_value.clamp(0.0, 1.0),
        prominence: peak_value - median,
        peak_lag_frames: peak_lag,
    })
}

/// The Landscape Signal's documented repeat period, in seconds.
pub const LANDSCAPE_PERIOD_SECONDS: f32 = 109.5;

/// Minimum autocorrelation confidence for a period to be believed.
///
/// Set from measurement. Ship ambience at Eratosthenes produced periods
/// scattered between 49.8 s and 125.6 s at confidence 0.45–0.53, while the
/// genuine Landscape Signal returns 109.67 s at 0.98. The old bar of 0.3 let
/// every one of those ambient readings through.
pub const LANDSCAPE_MIN_CONFIDENCE: f32 = 0.80;

/// Minimum prominence — how far the peak stands above the rest of the curve.
///
/// The reference measures 0.98; ambient loops sit far lower even when they do
/// repeat, because their autocorrelation is a broad hill rather than a spike.
pub const LANDSCAPE_MIN_PROMINENCE: f32 = 0.50;

/// Whether a result is consistent with the Landscape Signal.
///
/// This is the strongest evidence the tool has. Neither the structure score nor
/// the keying score separates the real signal from ordinary ship ambience —
/// measured, they overlap completely — but the period does, by a wide margin.
pub fn matches_landscape(result: &PeriodicityResult, tolerance_seconds: f32) -> bool {
    (result.period_seconds - LANDSCAPE_PERIOD_SECONDS).abs() <= tolerance_seconds
        && result.confidence >= LANDSCAPE_MIN_CONFIDENCE
        && result.prominence >= LANDSCAPE_MIN_PROMINENCE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A square-ish repeating pulse: one burst per period.
    fn pulse_train(len: usize, period: f32, width: f32, level: f32, floor: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let phase = (i as f32) % period;
                if phase < width { level } else { floor }
            })
            .collect()
    }

    #[test]
    fn autocorrelation_of_a_constant_is_empty() {
        assert!(autocorrelation(&vec![5.0; 100]).is_empty());
        assert!(autocorrelation(&[]).is_empty());
        assert!(autocorrelation(&[1.0]).is_empty());
    }

    #[test]
    fn autocorrelation_peaks_at_zero_lag() {
        let s: Vec<f32> = (0..256).map(|i| (i as f32 * 0.3).sin()).collect();
        let c = autocorrelation(&s);
        assert!((c[0] - 1.0).abs() < 1e-4, "r[0] should be 1, got {}", c[0]);
        assert!(c[1..].iter().all(|&v| v <= c[0] + 1e-4));
    }

    #[test]
    fn finds_a_known_period_in_a_pulse_train() {
        // 600 s at 1 fps with a repeat every 40 s.
        let s = pulse_train(600, 40.0, 5.0, 0.0, -60.0);
        let r = estimate_period(&s, 1.0, 10.0, 200.0).unwrap();
        assert!(
            (r.period_seconds - 40.0).abs() < 0.5,
            "got {}",
            r.period_seconds
        );
        assert!(r.confidence > 0.5, "confidence {}", r.confidence);
        assert!(r.prominence > 0.2, "prominence {}", r.prominence);
    }

    #[test]
    fn finds_the_landscape_period_at_one_frame_per_second() {
        // Two and a half cycles of a 109.5 s repeat — what ~5 minutes of the
        // long-term tier would hold.
        let len = 600;
        let s: Vec<f32> = (0..len)
            .map(|i| {
                let phase = (i as f32) % LANDSCAPE_PERIOD_SECONDS;
                // A rising ramp then silence: crude, but it has the right period.
                if phase < 30.0 { -40.0 + phase } else { -80.0 }
            })
            .collect();
        let r = estimate_period(&s, 1.0, 30.0, 300.0).unwrap();
        assert!(
            (r.period_seconds - LANDSCAPE_PERIOD_SECONDS).abs() < 1.0,
            "expected ~109.5 s, got {}",
            r.period_seconds
        );
        assert!(matches_landscape(&r, 1.0), "{r:?}");
    }

    #[test]
    fn parabolic_interpolation_beats_integer_lag_resolution() {
        // A 40.5 s period cannot be represented by an integer lag at 1 fps.
        let period = 40.5f32;
        let s: Vec<f32> = (0..800)
            .map(|i| (std::f32::consts::TAU * i as f32 / period).sin())
            .collect();
        let r = estimate_period(&s, 1.0, 10.0, 100.0).unwrap();
        assert_ne!(
            r.peak_lag_frames as f32, period,
            "the integer peak is off by design"
        );
        assert!(
            (r.period_seconds - period).abs() < 0.3,
            "interpolated to {}, expected {period}",
            r.period_seconds
        );
    }

    #[test]
    fn noise_yields_low_confidence() {
        let mut state = 0xC0FFEEu32;
        let noise: Vec<f32> = (0..600)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / 8_388_608.0 - 1.0
            })
            .collect();
        let r = estimate_period(&noise, 1.0, 30.0, 200.0).unwrap();
        assert!(
            r.confidence < 0.4,
            "noise should not look periodic: {}",
            r.confidence
        );
        assert!(!matches_landscape(&r, 1.0));
    }

    #[test]
    fn refuses_lags_it_cannot_see_twice() {
        // 100 frames cannot evidence a 200 s period.
        let s = pulse_train(100, 20.0, 3.0, 0.0, -50.0);
        assert!(estimate_period(&s, 1.0, 150.0, 300.0).is_none());
    }

    #[test]
    fn rejects_nonsense_parameters() {
        let s = pulse_train(300, 30.0, 4.0, 0.0, -50.0);
        assert!(estimate_period(&s, 0.0, 10.0, 100.0).is_none());
        assert!(estimate_period(&s, 1.0, 100.0, 10.0).is_none());
        assert!(estimate_period(&s, 1.0, -5.0, 100.0).is_none());
        assert!(estimate_period(&[], 1.0, 10.0, 100.0).is_none());
    }

    #[test]
    fn a_flat_series_has_no_period() {
        assert!(estimate_period(&vec![-60.0; 500], 1.0, 10.0, 200.0).is_none());
    }

    #[test]
    fn measured_ship_ambience_is_rejected() {
        // Real readings captured at Eratosthenes: the periods wander and the
        // confidence never approaches what the genuine signal produces.
        for (period, confidence, prominence) in [
            (49.8f32, 0.48f32, 0.30f32),
            (75.0, 0.49, 0.31),
            (124.1, 0.53, 0.35),
            (125.6, 0.49, 0.33),
            // Even landing near the right period is not enough on its own.
            (109.6, 0.53, 0.35),
        ] {
            let r = PeriodicityResult {
                period_seconds: period,
                confidence,
                prominence,
                peak_lag_frames: period as usize,
            };
            assert!(
                !matches_landscape(&r, 2.0),
                "ambient reading {r:?} must not be called the Landscape Signal"
            );
        }
    }

    #[test]
    fn the_measured_reference_is_accepted() {
        // CMDR Serbanstein's recording, as this tool measures it.
        let r = PeriodicityResult {
            period_seconds: 109.67,
            confidence: 0.98,
            prominence: 0.98,
            peak_lag_frames: 110,
        };
        assert!(matches_landscape(&r, 2.0), "the genuine signal must pass");
    }

    #[test]
    fn landscape_match_requires_more_than_a_lucky_lag() {
        let weak = PeriodicityResult {
            period_seconds: 109.5,
            confidence: 0.05,
            prominence: 0.01,
            peak_lag_frames: 110,
        };
        assert!(
            !matches_landscape(&weak, 1.0),
            "a weak peak at the right lag is not a match"
        );

        let wrong_period = PeriodicityResult {
            period_seconds: 60.0,
            confidence: 0.9,
            prominence: 0.5,
            peak_lag_frames: 60,
        };
        assert!(!matches_landscape(&wrong_period, 1.0));
    }

    #[test]
    fn results_are_always_finite() {
        let s = pulse_train(400, 33.0, 2.0, 0.0, -70.0);
        let r = estimate_period(&s, 1.0, 10.0, 150.0).unwrap();
        assert!(r.period_seconds.is_finite());
        assert!(r.confidence.is_finite());
        assert!(r.prominence.is_finite());
    }
}

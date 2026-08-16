//! Direction finding from inter-channel differences.
//!
//! Two estimators, chosen by how many directional channels the endpoint gives us:
//!
//! * **Stereo** — invert the constant-power pan law from the level difference
//!   between the two front speakers. Confidence comes from magnitude-squared
//!   coherence: an amplitude-panned source keeps the channels coherent, whereas
//!   two unrelated sounds in the two ears produce a meaningless "bearing".
//! * **Surround** — treat each speaker as a unit vector at its azimuth and sum,
//!   weighted by band-limited level. Gerzon defined two such sums and this uses
//!   both, for the things each is actually good at:
//!
//!   - The **velocity vector** (amplitude weights) gives the *bearing*. It is
//!     exact for a pairwise amplitude-panned source, which is how game engines
//!     place ambient sounds.
//!   - The **energy vector** (power weights) gives the *confidence*. Its
//!     normalized length falls as energy smears across the layout, which is
//!     what "how localized is this really" should mean.
//!
//!   Using the energy vector for the angle instead costs up to ~11° of bias on
//!   the 7.1 rear pair, which sits 90° apart — measured, not theorized.
//!
//! Everything here is horizontal-only and expressed in the ship's frame. See
//! the module docs in `format.rs` for the azimuth convention, and the spec for
//! why a galactic bearing still needs the commander to rotate the ship.

use realfft::num_complex::Complex32;

use crate::audio::format::ChannelInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectionMethod {
    /// Constant-power pan law inverted across two front speakers.
    StereoPanLaw,
    /// Vector sum over three or more directional speakers.
    SurroundVector,
    /// Not enough directional information to say anything.
    Insufficient,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionEstimate {
    /// Ship-relative azimuth in degrees, `(-180, 180]`. Meaningless when
    /// `method` is `Insufficient`.
    pub azimuth_deg: f32,
    /// 0..1. For stereo this is coherence; for surround, energy concentration.
    pub confidence: f32,
    pub method: DirectionMethod,
    /// True when the layout cannot distinguish front from rear, which is always
    /// the case for plain stereo.
    pub front_back_ambiguous: bool,
}

impl DirectionEstimate {
    pub fn insufficient() -> Self {
        Self {
            azimuth_deg: 0.0,
            confidence: 0.0,
            method: DirectionMethod::Insufficient,
            front_back_ambiguous: true,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.method != DirectionMethod::Insufficient
    }
}

/// Wrap degrees into `(-180, 180]`.
pub fn wrap_deg(deg: f32) -> f32 {
    let mut d = deg % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d <= -180.0 {
        d += 360.0;
    }
    // `-180.0 % 360.0` is -180, which the branch above lifts to 180; guard the
    // exact-zero case so we never return -0.0.
    if d == 0.0 { 0.0 } else { d }
}

/// Smallest absolute angle between two bearings, in degrees.
pub fn angular_error_deg(a: f32, b: f32) -> f32 {
    wrap_deg(a - b).abs()
}

/// Weighted vector sum over the directional channels, shared by both estimators.
///
/// Returns `(azimuth_deg, normalized_length)`, or `None` when no channel carries
/// a bearing or the total weight is zero — silence must not produce a confident
/// direction.
fn vector_sum(
    powers: &[f32],
    layout: &[ChannelInfo],
    amplitude_weights: bool,
) -> Option<(f32, f32)> {
    let mut x = 0.0f64; // starboard
    let mut y = 0.0f64; // forward
    let mut total = 0.0f64;

    for (power, info) in powers.iter().zip(layout.iter()) {
        let Some(az) = info.azimuth_deg else { continue };
        let p = (*power as f64).max(0.0);
        if p == 0.0 {
            continue;
        }
        let w = if amplitude_weights { p.sqrt() } else { p };
        let rad = (az as f64).to_radians();
        x += w * rad.sin();
        y += w * rad.cos();
        total += w;
    }

    if total <= 0.0 {
        return None;
    }
    let magnitude = (x * x + y * y).sqrt();
    let azimuth = x.atan2(y).to_degrees() as f32;
    Some((
        wrap_deg(azimuth),
        ((magnitude / total) as f32).clamp(0.0, 1.0),
    ))
}

/// Power-weighted vector sum. The normalized length is the concentration used
/// as a confidence value.
pub fn energy_vector(powers: &[f32], layout: &[ChannelInfo]) -> Option<(f32, f32)> {
    vector_sum(powers, layout, false)
}

/// Amplitude-weighted vector sum. Exact for a pairwise amplitude-panned source,
/// so this is what produces the reported bearing.
pub fn velocity_vector(powers: &[f32], layout: &[ChannelInfo]) -> Option<(f32, f32)> {
    vector_sum(powers, layout, true)
}

/// Invert the constant-power pan law between two speakers at `±speaker_deg`.
///
/// With `a_l = cos θ` and `a_r = sin θ` for `θ ∈ [0, π/2]`, the pan position is
/// `(θ − π/4) / (π/4)` in `[-1, 1]`, which maps linearly onto the speaker arc.
/// Returns `None` for silence.
pub fn pan_law_azimuth(power_l: f32, power_r: f32, speaker_deg: f32) -> Option<f32> {
    let al = power_l.max(0.0).sqrt();
    let ar = power_r.max(0.0).sqrt();
    if al == 0.0 && ar == 0.0 {
        return None;
    }
    let theta = ar.atan2(al); // 0 = hard left, π/4 = centre, π/2 = hard right
    let pan = (theta - std::f32::consts::FRAC_PI_4) / std::f32::consts::FRAC_PI_4;
    Some(wrap_deg(pan.clamp(-1.0, 1.0) * speaker_deg))
}

/// Magnitude-squared coherence in `[0, 1]` from an accumulated cross-spectrum.
///
/// `cross` is `Σ A·conj(B)` over the band, `power_a`/`power_b` are `Σ|A|²` and
/// `Σ|B|²` over the same bins.
pub fn coherence(cross: Complex32, power_a: f32, power_b: f32) -> f32 {
    let denom = power_a * power_b;
    if denom <= 0.0 {
        return 0.0;
    }
    (cross.norm_sqr() / denom).clamp(0.0, 1.0)
}

/// Pick an estimator for the layout and produce a bearing.
///
/// `cross_lr` is the band cross-spectrum between the first two directional
/// channels, used only for the stereo confidence; pass `None` if unavailable
/// and the estimate falls back to energy concentration.
pub fn estimate(
    powers: &[f32],
    layout: &[ChannelInfo],
    cross_lr: Option<Complex32>,
) -> DirectionEstimate {
    debug_assert_eq!(powers.len(), layout.len(), "one power per channel");

    let directional: Vec<usize> = layout
        .iter()
        .enumerate()
        .filter(|(_, c)| c.azimuth_deg.is_some())
        .map(|(i, _)| i)
        .collect();

    match directional.len() {
        0 | 1 => DirectionEstimate::insufficient(),
        2 => {
            let (li, ri) = (directional[0], directional[1]);
            // Order by azimuth so a layout listing right before left still works.
            let (li, ri) = if layout[li].azimuth_deg.unwrap() <= layout[ri].azimuth_deg.unwrap() {
                (li, ri)
            } else {
                (ri, li)
            };
            let spread = (layout[ri].azimuth_deg.unwrap() - layout[li].azimuth_deg.unwrap()) / 2.0;
            let Some(azimuth_deg) = pan_law_azimuth(powers[li], powers[ri], spread) else {
                return DirectionEstimate::insufficient();
            };
            let confidence = match cross_lr {
                Some(cross) => coherence(cross, powers[li], powers[ri]),
                // Without a cross-spectrum we can only report that *something*
                // is there, not that the two channels are related.
                None => energy_vector(powers, layout).map(|(_, c)| c).unwrap_or(0.0),
            };
            DirectionEstimate {
                azimuth_deg,
                confidence,
                method: DirectionMethod::StereoPanLaw,
                front_back_ambiguous: true,
            }
        }
        _ => match velocity_vector(powers, layout) {
            // Bearing from the velocity vector, confidence from the energy
            // vector — see the module docs for why they are not the same sum.
            Some((azimuth_deg, _)) => DirectionEstimate {
                azimuth_deg,
                confidence: energy_vector(powers, layout).map(|(_, c)| c).unwrap_or(0.0),
                method: DirectionMethod::SurroundVector,
                front_back_ambiguous: false,
            },
            None => DirectionEstimate::insufficient(),
        },
    }
}

/// Generalized cross-correlation with phase transform.
///
/// Returns the lag `τ` in samples for which `b(t) ≈ a(t − τ)` — positive means
/// `b` is delayed relative to `a` — and the normalized peak height. Game audio
/// is usually panned by
/// amplitude alone, in which case this reports lag 0 — a disagreement between
/// this and the pan-law bearing is a signal that something more interesting is
/// happening, which is why both are surfaced.
pub fn gcc_phat(a: &[f32], b: &[f32], max_lag: usize) -> Option<(isize, f32)> {
    let n = a.len().min(b.len());
    if n == 0 || max_lag == 0 {
        return None;
    }
    let padded = (2 * n).next_power_of_two();

    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(padded);
    let ifft = planner.plan_fft_inverse(padded);

    let mut buf_a = fft.make_input_vec();
    let mut buf_b = fft.make_input_vec();
    buf_a[..n].copy_from_slice(&a[..n]);
    buf_b[..n].copy_from_slice(&b[..n]);

    let mut spec_a = fft.make_output_vec();
    let mut spec_b = fft.make_output_vec();
    fft.process(&mut buf_a, &mut spec_a).ok()?;
    fft.process(&mut buf_b, &mut spec_b).ok()?;

    let mut cross = spec_a;
    for (c, b) in cross.iter_mut().zip(spec_b.iter()) {
        // B·conj(A), so a positive peak index means `b` is the delayed one.
        let x = c.conj() * *b;
        // Phase transform: keep only the phase, discarding the magnitude that
        // otherwise lets one loud band dominate the correlation.
        let m = x.norm();
        *c = if m > 1e-20 {
            x / m
        } else {
            Complex32::new(0.0, 0.0)
        };
    }

    let mut corr = ifft.make_output_vec();
    ifft.process(&mut cross, &mut corr).ok()?;

    let max_lag = max_lag.min(padded / 2 - 1);
    let mut best_lag = 0isize;
    let mut best = f32::NEG_INFINITY;
    for lag in -(max_lag as isize)..=(max_lag as isize) {
        let idx = if lag >= 0 {
            lag as usize
        } else {
            padded - (-lag) as usize
        };
        let v = corr[idx];
        if v > best {
            best = v;
            best_lag = lag;
        }
    }
    // `corr` is unnormalized by the inverse transform's length.
    Some((best_lag, best / padded as f32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::format::{MASK_5_1, MASK_7_1, MASK_STEREO, channel_layout};

    fn near(a: f32, b: f32, tol: f32) {
        assert!((a - b).abs() <= tol, "{a} not within {tol} of {b}");
    }

    #[test]
    fn wrap_covers_the_seam() {
        near(wrap_deg(0.0), 0.0, 1e-6);
        near(wrap_deg(180.0), 180.0, 1e-6);
        near(wrap_deg(-180.0), 180.0, 1e-6);
        near(wrap_deg(190.0), -170.0, 1e-6);
        near(wrap_deg(-190.0), 170.0, 1e-6);
        near(wrap_deg(720.0), 0.0, 1e-6);
    }

    #[test]
    fn angular_error_takes_the_short_way_round() {
        near(angular_error_deg(170.0, -170.0), 20.0, 1e-4);
        near(angular_error_deg(-30.0, 30.0), 60.0, 1e-4);
    }

    #[test]
    fn equal_stereo_power_reads_dead_ahead() {
        let az = pan_law_azimuth(1.0, 1.0, 30.0).unwrap();
        near(az, 0.0, 1e-4);
    }

    #[test]
    fn hard_panned_stereo_reads_the_speaker_position() {
        near(pan_law_azimuth(1.0, 0.0, 30.0).unwrap(), -30.0, 1e-4);
        near(pan_law_azimuth(0.0, 1.0, 30.0).unwrap(), 30.0, 1e-4);
    }

    #[test]
    fn pan_law_is_monotonic_across_the_arc() {
        let mut previous = f32::NEG_INFINITY;
        for i in 0..=20 {
            let r = i as f32 / 20.0;
            let az = pan_law_azimuth(1.0 - r, r, 30.0).unwrap();
            assert!(az > previous, "azimuth must increase as power moves right");
            previous = az;
        }
    }

    #[test]
    fn silence_yields_no_stereo_bearing() {
        assert!(pan_law_azimuth(0.0, 0.0, 30.0).is_none());
    }

    #[test]
    fn velocity_vector_is_exact_for_pairwise_panning() {
        // Two rear speakers 90 degrees apart, amplitude-panned to 150 deg.
        let layout = channel_layout(MASK_7_1, 8);
        let (br, bl) = (5usize, 4usize); // +135 and -135
        let theta = (15.0f32).to_radians();
        let mut powers = vec![0.0; 8];
        powers[br] = theta.cos().powi(2);
        powers[bl] = theta.sin().powi(2);

        let (velocity, _) = velocity_vector(&powers, &layout).unwrap();
        let (energy, _) = energy_vector(&powers, &layout).unwrap();
        near(velocity, 150.0, 0.5);
        // The energy vector is biased inward on a widely-spaced pair.
        assert!(
            angular_error_deg(energy, 150.0) > 8.0,
            "expected measurable energy-vector bias, got {energy}"
        );
    }

    #[test]
    fn energy_vector_points_at_a_single_active_speaker() {
        let layout = channel_layout(MASK_7_1, 8);
        // Index 7 is SR at +90°.
        let mut powers = vec![0.0; 8];
        powers[7] = 1.0;
        let (az, conc) = energy_vector(&powers, &layout).unwrap();
        near(az, 90.0, 1e-3);
        near(conc, 1.0, 1e-6);
    }

    #[test]
    fn energy_vector_resolves_the_rear() {
        let layout = channel_layout(MASK_5_1, 6);
        let mut powers = vec![0.0; 6];
        powers[4] = 1.0; // BL at -110°
        powers[5] = 1.0; // BR at +110°
        let (az, _) = energy_vector(&powers, &layout).unwrap();
        near(az, 180.0, 1e-3);
    }

    #[test]
    fn energy_vector_ignores_lfe() {
        let layout = channel_layout(MASK_5_1, 6);
        let mut powers = vec![0.0; 6];
        powers[2] = 1.0; // FC at 0°
        powers[3] = 1000.0; // LFE, must not move the bearing
        let (az, conc) = energy_vector(&powers, &layout).unwrap();
        near(az, 0.0, 1e-4);
        near(conc, 1.0, 1e-6);
    }

    #[test]
    fn energy_smeared_everywhere_has_low_concentration() {
        let layout = channel_layout(MASK_7_1, 8);
        let powers = vec![1.0; 8];
        let (_, conc) = energy_vector(&powers, &layout).unwrap();
        assert!(
            conc < 0.3,
            "uniform energy should not look directional: {conc}"
        );
    }

    #[test]
    fn energy_vector_rejects_silence() {
        let layout = channel_layout(MASK_7_1, 8);
        assert!(energy_vector(&[0.0; 8], &layout).is_none());
    }

    #[test]
    fn negative_powers_do_not_produce_a_bearing() {
        // Defensive: numerical noise must never make a direction appear.
        let layout = channel_layout(MASK_STEREO, 2);
        assert!(energy_vector(&[-1.0, -1.0], &layout).is_none());
    }

    #[test]
    fn coherence_is_one_for_identical_spectra() {
        let a = Complex32::new(3.0, 4.0);
        let cross = a * a.conj();
        near(coherence(cross, a.norm_sqr(), a.norm_sqr()), 1.0, 1e-5);
    }

    #[test]
    fn coherence_is_zero_for_no_cross_term() {
        near(coherence(Complex32::new(0.0, 0.0), 1.0, 1.0), 0.0, 1e-6);
        near(coherence(Complex32::new(1.0, 0.0), 0.0, 1.0), 0.0, 1e-6);
    }

    #[test]
    fn estimate_picks_pan_law_for_stereo() {
        let layout = channel_layout(MASK_STEREO, 2);
        let e = estimate(&[1.0, 0.0], &layout, None);
        assert_eq!(e.method, DirectionMethod::StereoPanLaw);
        assert!(e.front_back_ambiguous);
        near(e.azimuth_deg, -30.0, 1e-3);
    }

    #[test]
    fn estimate_picks_energy_vector_for_surround() {
        let layout = channel_layout(MASK_7_1, 8);
        let mut powers = vec![0.0; 8];
        powers[6] = 1.0; // SL at -90°
        let e = estimate(&powers, &layout, None);
        assert_eq!(e.method, DirectionMethod::SurroundVector);
        assert!(!e.front_back_ambiguous);
        near(e.azimuth_deg, -90.0, 1e-3);
    }

    #[test]
    fn mono_content_in_a_stereo_stream_reads_centre() {
        // The spec calls this out: it must read 0°, not garbage.
        let layout = channel_layout(MASK_STEREO, 2);
        let a = Complex32::new(1.0, 0.5);
        let cross = a * a.conj();
        let e = estimate(&[a.norm_sqr(), a.norm_sqr()], &layout, Some(cross));
        near(e.azimuth_deg, 0.0, 1e-4);
        near(e.confidence, 1.0, 1e-4);
    }

    #[test]
    fn a_mono_stream_has_no_bearing() {
        let layout = channel_layout(0, 1);
        // A single channel at 0° cannot discriminate anything.
        let e = estimate(&[1.0], &layout, None);
        assert_eq!(e.method, DirectionMethod::Insufficient);
        assert_eq!(e.confidence, 0.0);
    }

    #[test]
    fn zero_power_event_reports_insufficient_not_nan() {
        let layout = channel_layout(MASK_7_1, 8);
        let e = estimate(&[0.0; 8], &layout, None);
        assert_eq!(e.method, DirectionMethod::Insufficient);
        assert!(e.azimuth_deg.is_finite());
        assert!(e.confidence.is_finite());
    }

    #[test]
    fn gcc_phat_finds_a_known_delay() {
        let n = 2048;
        let delay = 17usize;
        // Broadband content, so the phase transform has something to lock onto.
        let a: Vec<f32> = (0..n)
            .map(|i| ((i as f32 * 0.37).sin() + (i as f32 * 1.13).sin()) * 0.5)
            .collect();
        let mut b = vec![0.0; n];
        b[delay..].copy_from_slice(&a[..n - delay]);

        let (lag, peak) = gcc_phat(&a, &b, 64).unwrap();
        assert_eq!(lag, delay as isize, "b lags a by {delay}");
        assert!(peak > 0.0, "peak should be positive, got {peak}");
    }

    #[test]
    fn gcc_phat_reports_zero_lag_for_amplitude_panning() {
        let n = 1024;
        let a: Vec<f32> = (0..n).map(|i| (i as f32 * 0.41).sin()).collect();
        let b: Vec<f32> = a.iter().map(|v| v * 0.3).collect();
        let (lag, _) = gcc_phat(&a, &b, 32).unwrap();
        assert_eq!(lag, 0, "pure level difference implies no time difference");
    }

    #[test]
    fn gcc_phat_handles_degenerate_input() {
        assert!(gcc_phat(&[], &[], 8).is_none());
        assert!(gcc_phat(&[1.0, 2.0], &[1.0, 2.0], 0).is_none());
        // All-silent input must not panic or produce NaN.
        let (_, peak) = gcc_phat(&[0.0; 256], &[0.0; 256], 16).unwrap();
        assert!(peak.is_finite());
    }
}

//! Synthetic audio sources, so the whole analysis chain can be developed and
//! validated with no Elite Dangerous, no audio hardware, and no Windows.
//!
//! The important one is [`TestSignal::Landscape`]. It renders a signal whose
//! *spectrogram* draws a mountain range, on the documented 109.5 second cycle,
//! panned to a chosen azimuth. That gives the detector, the periodicity
//! estimator, and the direction finder a ground truth to be measured against.
//!
//! This is a stand-in with the right structure, not a reproduction of the real
//! signal. Its purpose is to have the correct period, the correct feature
//! offsets, and a genuinely mountain-shaped time-frequency footprint.

use crate::audio::StreamFormat;
use crate::audio::format::ChannelInfo;

/// The Landscape Signal's documented repeat period, in seconds.
pub const LANDSCAPE_PERIOD_SECONDS: f32 = 109.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestSignal {
    Silence,
    Sine {
        hz: f32,
    },
    Noise,
    Sweep {
        start_hz: f32,
        end_hz: f32,
        seconds: f32,
    },
    /// Mountain-shaped spectrogram on the Landscape Signal's cycle.
    Landscape,
    /// Binary keying: alternating tones on a fixed symbol clock, in the
    /// `Wail | Header | Data` chunk shape of the Thargoid Probe tightbeam.
    Tightbeam,
    /// Line art drawn into the spectrogram — a circle with radiating spokes,
    /// the probe image in miniature. Ground truth for structure detection.
    Picture,
}

/// One frequency ramp in the synthetic spectrogram — a single pen stroke.
#[derive(Debug, Clone, Copy)]
struct Stroke {
    start_s: f32,
    end_s: f32,
    start_hz: f32,
    end_hz: f32,
    amplitude: f32,
}

impl Stroke {
    fn duration(&self) -> f32 {
        self.end_s - self.start_s
    }

    /// Instantaneous frequency at time `t` within the cycle.
    fn hz_at(&self, t: f32) -> f32 {
        let progress = ((t - self.start_s) / self.duration()).clamp(0.0, 1.0);
        self.start_hz + (self.end_hz - self.start_hz) * progress
    }

    /// Raised-cosine envelope over the first and last 10%, so strokes fade in
    /// and out instead of clicking.
    fn envelope(&self, t: f32) -> f32 {
        let progress = (t - self.start_s) / self.duration();
        if !(0.0..=1.0).contains(&progress) {
            return 0.0;
        }
        let edge = 0.1;
        let shape = if progress < edge {
            progress / edge
        } else if progress > 1.0 - edge {
            (1.0 - progress) / edge
        } else {
            1.0
        };
        0.5 - 0.5 * (shape * std::f32::consts::PI).cos()
    }
}

/// The mountain range, laid out against the documented feature offsets:
/// lower tilted A at 0:25, tilted A at 0:31, mountain at 1:20, ridge at 1:23,
/// tail at 1:28 — each offset being where that feature ends.
fn landscape_strokes() -> Vec<Stroke> {
    vec![
        // Lower tilted A — a short rising line ending at 25 s.
        Stroke {
            start_s: 20.0,
            end_s: 25.0,
            start_hz: 380.0,
            end_hz: 900.0,
            amplitude: 0.10,
        },
        // Tilted A — steeper, ending at 31 s.
        Stroke {
            start_s: 26.5,
            end_s: 31.0,
            start_hz: 520.0,
            end_hz: 1450.0,
            amplitude: 0.12,
        },
        // The mountain: a long climb and a symmetric descent, cresting before
        // 1:20. This is the feature the signal is named for.
        Stroke {
            start_s: 48.0,
            end_s: 66.0,
            start_hz: 300.0,
            end_hz: 2600.0,
            amplitude: 0.16,
        },
        Stroke {
            start_s: 66.0,
            end_s: 80.0,
            start_hz: 2600.0,
            end_hz: 700.0,
            amplitude: 0.16,
        },
        // Ridge — a short flat-topped shoulder ending at 1:23.
        Stroke {
            start_s: 80.0,
            end_s: 83.0,
            start_hz: 1850.0,
            end_hz: 2050.0,
            amplitude: 0.11,
        },
        // Tail — the curl, falling away to 1:28.
        Stroke {
            start_s: 84.5,
            end_s: 88.0,
            start_hz: 1000.0,
            end_hz: 480.0,
            amplitude: 0.09,
        },
    ]
}

/// A circle with radiating spokes, drawn into the time-frequency plane.
///
/// The circle is approximated by many short frequency ramps, which is what a
/// curve *is* in a spectrogram: a sequence of swept tones. Together with the
/// spokes this produces gradients at many orientations — the property that
/// separates a drawing from a page of musical harmonics.
fn picture_strokes() -> Vec<Stroke> {
    let mut strokes = Vec::new();
    let (centre_t, centre_hz) = (12.0f32, 1400.0f32);
    let (radius_t, radius_hz) = (6.0f32, 900.0f32);

    // The circle, as 24 arc segments.
    const SEGMENTS: usize = 24;
    for i in 0..SEGMENTS {
        let a0 = i as f32 * std::f32::consts::TAU / SEGMENTS as f32;
        let a1 = (i + 1) as f32 * std::f32::consts::TAU / SEGMENTS as f32;
        let (t0, t1) = (
            centre_t + radius_t * a0.cos(),
            centre_t + radius_t * a1.cos(),
        );
        let (f0, f1) = (
            centre_hz + radius_hz * a0.sin(),
            centre_hz + radius_hz * a1.sin(),
        );
        if t1 <= t0 {
            continue; // only left-to-right segments exist in time
        }
        strokes.push(Stroke {
            start_s: t0,
            end_s: t1,
            start_hz: f0,
            end_hz: f1,
            amplitude: 0.10,
        });
    }

    // Radiating spokes at assorted angles.
    for (dt, df) in [
        (5.0f32, 1500.0f32),
        (5.0, -900.0),
        (6.5, 300.0),
        (6.0, -1500.0),
    ] {
        strokes.push(Stroke {
            start_s: centre_t,
            end_s: centre_t + dt,
            start_hz: centre_hz,
            end_hz: (centre_hz + df).max(120.0),
            amplitude: 0.10,
        });
    }
    strokes
}

/// Amplitude gains that place a source at `azimuth_deg` across a speaker layout.
///
/// Pairwise constant-power panning between the two speakers that bracket the
/// target. This is the inverse of what the direction finder does, which is
/// exactly the point: it makes the round trip measurable.
///
/// Channels with no azimuth (LFE, height) receive zero — the direction finder
/// ignores them, so feeding them signal would only be misleading.
pub fn pan_gains(azimuth_deg: f32, layout: &[ChannelInfo]) -> Vec<f32> {
    let mut gains = vec![0.0f32; layout.len()];

    let mut directional: Vec<(usize, f32)> = layout
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.azimuth_deg.map(|a| (i, a)))
        .collect();

    match directional.len() {
        0 => return gains,
        1 => {
            gains[directional[0].0] = 1.0;
            return gains;
        }
        _ => {}
    }
    directional.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let target = crate::analysis::direction::wrap_deg(azimuth_deg);

    // Find the adjacent pair that brackets the target, treating the layout as a
    // circle so a source behind the listener lands between the rear speakers.
    let n = directional.len();
    for k in 0..n {
        let (i0, a0) = directional[k];
        let (i1, a1) = directional[(k + 1) % n];
        // Arc from a0 counter-clockwise to a1, wrapping through ±180.
        let span = (a1 - a0).rem_euclid(360.0);
        let offset = (target - a0).rem_euclid(360.0);
        if offset <= span && span > 0.0 {
            let ratio = offset / span;
            let theta = ratio * std::f32::consts::FRAC_PI_2;
            gains[i0] = theta.cos();
            gains[i1] = theta.sin();
            return gains;
        }
    }

    // Unreachable for any sane layout, but never leave the caller with silence.
    gains[directional[0].0] = 1.0;
    gains
}

/// Symbol length of the synthetic tightbeam, in seconds.
pub const TIGHTBEAM_SYMBOL_SECONDS: f32 = 0.25;

/// The two keying tones, in Hz.
pub const TIGHTBEAM_TONES_HZ: [f32; 2] = [1200.0, 2400.0];

/// One full synthetic tightbeam cycle: a wail, then five data chunks.
const TIGHTBEAM_CYCLE_SECONDS: f32 = 40.0;

/// Period of the synthetic picture, so it repeats like a real transmission.
pub const PICTURE_PERIOD_SECONDS: f32 = 30.0;

/// Deterministic synthetic source. Renders interleaved multichannel audio.
pub struct SyntheticSource {
    format: StreamFormat,
    signal: TestSignal,
    azimuth_deg: f32,
    gains: Vec<f32>,
    /// Absolute sample index, so the cycle is continuous across render calls.
    sample: u64,
    phase: f64,
    stroke_phases: Vec<f64>,
    strokes: Vec<Stroke>,
    noise_state: u32,
}

impl SyntheticSource {
    pub fn new(signal: TestSignal, format: StreamFormat, azimuth_deg: f32) -> Self {
        let layout = format.layout();
        let strokes = match signal {
            TestSignal::Landscape => landscape_strokes(),
            TestSignal::Picture => picture_strokes(),
            _ => Vec::new(),
        };
        Self {
            gains: pan_gains(azimuth_deg, &layout),
            stroke_phases: vec![0.0; strokes.len()],
            strokes,
            format,
            signal,
            azimuth_deg,
            sample: 0,
            phase: 0.0,
            noise_state: 0x9E3779B9,
        }
    }

    pub fn format(&self) -> &StreamFormat {
        &self.format
    }

    pub fn signal(&self) -> TestSignal {
        self.signal
    }

    pub fn azimuth_deg(&self) -> f32 {
        self.azimuth_deg
    }

    /// Per-channel amplitude gains in use.
    pub fn gains(&self) -> &[f32] {
        &self.gains
    }

    pub fn samples_rendered(&self) -> u64 {
        self.sample
    }

    fn next_noise(&mut self) -> f32 {
        // Deterministic LCG: reproducible test runs matter more than spectral
        // purity here.
        self.noise_state = self
            .noise_state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.noise_state >> 8) as f32 / 8_388_608.0 - 1.0
    }

    /// The mono source value for the next sample, advancing internal phase.
    fn next_mono(&mut self) -> f32 {
        let sr = self.format.sample_rate as f64;
        let t = self.sample as f64 / sr;
        match self.signal {
            TestSignal::Silence => 0.0,
            TestSignal::Sine { hz } => {
                self.phase += std::f64::consts::TAU * hz as f64 / sr;
                (self.phase.sin() * 0.5) as f32
            }
            TestSignal::Noise => self.next_noise() * 0.25,
            TestSignal::Sweep {
                start_hz,
                end_hz,
                seconds,
            } => {
                let progress = if seconds > 0.0 {
                    ((t as f32 % seconds) / seconds).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let hz = start_hz + (end_hz - start_hz) * progress;
                self.phase += std::f64::consts::TAU * hz as f64 / sr;
                (self.phase.sin() * 0.5) as f32
            }
            TestSignal::Tightbeam => {
                let cycle_t = (t as f32) % TIGHTBEAM_CYCLE_SECONDS;
                // A long opening wail, then keyed data. The wail is deliberate:
                // it skews the dwell-time distribution exactly as the real
                // signal does, so the detector has to cope with it.
                let (hz, gain) = if cycle_t < 1.5 {
                    (TIGHTBEAM_TONES_HZ[0], 0.35)
                } else if cycle_t < 32.0 {
                    let symbol = ((cycle_t - 1.5) / TIGHTBEAM_SYMBOL_SECONDS) as usize;
                    // A deterministic bit pattern, in triplets.
                    let bit = symbol.is_multiple_of(2) || symbol % 7 == 3;
                    (TIGHTBEAM_TONES_HZ[usize::from(bit)], 0.35)
                } else {
                    (0.0, 0.0) // inter-cycle silence
                };
                if gain == 0.0 {
                    self.phase = 0.0;
                    return 0.0;
                }
                self.phase += std::f64::consts::TAU * hz as f64 / sr;
                (self.phase.sin() * gain) as f32
            }
            TestSignal::Picture => {
                let cycle_t = (t as f32) % PICTURE_PERIOD_SECONDS;
                let mut value = 0.0f32;
                for (i, stroke) in self.strokes.iter().enumerate() {
                    let envelope = stroke.envelope(cycle_t);
                    if envelope <= 0.0 {
                        self.stroke_phases[i] = 0.0;
                        continue;
                    }
                    let hz = stroke.hz_at(cycle_t);
                    self.stroke_phases[i] += std::f64::consts::TAU * hz as f64 / sr;
                    value += stroke.amplitude * envelope * self.stroke_phases[i].sin() as f32;
                }
                value
            }
            TestSignal::Landscape => {
                let cycle_t = (t as f32) % LANDSCAPE_PERIOD_SECONDS;
                let mut value = 0.0f32;
                for (i, stroke) in self.strokes.iter().enumerate() {
                    let envelope = stroke.envelope(cycle_t);
                    if envelope <= 0.0 {
                        // Reset so the stroke starts from a known phase on the
                        // next cycle rather than accumulating drift forever.
                        self.stroke_phases[i] = 0.0;
                        continue;
                    }
                    let hz = stroke.hz_at(cycle_t);
                    self.stroke_phases[i] += std::f64::consts::TAU * hz as f64 / sr;
                    value += stroke.amplitude * envelope * self.stroke_phases[i].sin() as f32;
                }
                value
            }
        }
    }

    /// Render `frames` of interleaved audio, appending to `out`.
    pub fn render(&mut self, frames: usize, out: &mut Vec<f32>) {
        let channels = self.format.channels;
        out.reserve(frames * channels);
        for _ in 0..frames {
            let mono = self.next_mono();
            for c in 0..channels {
                out.push(mono * self.gains[c]);
            }
            self.sample += 1;
        }
    }

    /// Convenience for tests: render into a fresh buffer.
    pub fn render_seconds(&mut self, seconds: f32) -> Vec<f32> {
        let frames = (seconds * self.format.sample_rate as f32).round() as usize;
        let mut out = Vec::with_capacity(frames * self.format.channels);
        self.render(frames, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::direction::{DirectionMethod, angular_error_deg, estimate};
    use crate::analysis::statistics::SignalStats;
    use crate::analysis::stft::{Stft, argmax};
    use crate::audio::SampleFormat;
    use crate::audio::format::{MASK_5_1, MASK_7_1, MASK_STEREO, channel_layout};

    fn stereo() -> StreamFormat {
        StreamFormat::new(48_000, 2, MASK_STEREO, SampleFormat::F32)
    }

    fn surround() -> StreamFormat {
        StreamFormat::new(48_000, 8, MASK_7_1, SampleFormat::F32)
    }

    /// Per-channel power of an interleaved buffer.
    fn channel_powers(interleaved: &[f32], channels: usize) -> Vec<f32> {
        let mut p = vec![0.0f32; channels];
        for frame in interleaved.chunks_exact(channels) {
            for (c, s) in frame.iter().enumerate() {
                p[c] += s * s;
            }
        }
        p
    }

    #[test]
    fn silence_renders_silence() {
        let mut s = SyntheticSource::new(TestSignal::Silence, stereo(), 0.0);
        let buf = s.render_seconds(0.1);
        assert_eq!(buf.len(), 4800 * 2);
        assert!(buf.iter().all(|&v| v == 0.0));
        assert!(SignalStats::compute(buf).is_silent());
    }

    #[test]
    fn sine_lands_on_the_requested_frequency() {
        let mut s = SyntheticSource::new(TestSignal::Sine { hz: 1000.0 }, stereo(), 0.0);
        let buf = s.render_seconds(0.5);
        let mono: Vec<f32> = buf.as_chunks::<2>().0.iter().map(|f| f[0]).collect();

        let size = 4096;
        let mut stft = Stft::new(size, size);
        let mut spectrum = stft.make_spectrum();
        stft.process(&mono[..size], &mut spectrum);
        let mut db = vec![0.0; spectrum.len()];
        stft.magnitudes_db(&spectrum, &mut db);

        let peak = argmax(&db).unwrap();
        let hz = stft.bin_hz(peak, 48_000);
        assert!((hz - 1000.0).abs() < 30.0, "peak at {hz} Hz");
    }

    #[test]
    fn noise_is_deterministic_across_runs() {
        let a = SyntheticSource::new(TestSignal::Noise, stereo(), 0.0).render_seconds(0.05);
        let b = SyntheticSource::new(TestSignal::Noise, stereo(), 0.0).render_seconds(0.05);
        assert_eq!(a, b, "test runs must be reproducible");
        assert!(!SignalStats::compute(a).is_silent());
    }

    #[test]
    fn sweep_moves_upward_over_time() {
        let mut s = SyntheticSource::new(
            TestSignal::Sweep {
                start_hz: 200.0,
                end_hz: 8000.0,
                seconds: 2.0,
            },
            stereo(),
            0.0,
        );
        let buf = s.render_seconds(2.0);
        let mono: Vec<f32> = buf.as_chunks::<2>().0.iter().map(|f| f[0]).collect();

        let size = 8192;
        let mut stft = Stft::new(size, size);
        let mut spectrum = stft.make_spectrum();
        let mut db = vec![0.0; spectrum.len()];

        stft.process(&mono[..size], &mut spectrum);
        stft.magnitudes_db(&spectrum, &mut db);
        let early = stft.bin_hz(argmax(&db).unwrap(), 48_000);

        let late_start = mono.len() - size;
        stft.process(&mono[late_start..], &mut spectrum);
        stft.magnitudes_db(&spectrum, &mut db);
        let late = stft.bin_hz(argmax(&db).unwrap(), 48_000);

        assert!(late > early * 3.0, "swept from {early} Hz to {late} Hz");
    }

    #[test]
    fn output_never_clips() {
        for signal in [
            TestSignal::Sine { hz: 440.0 },
            TestSignal::Noise,
            TestSignal::Sweep {
                start_hz: 100.0,
                end_hz: 5000.0,
                seconds: 1.0,
            },
            TestSignal::Landscape,
        ] {
            let mut s = SyntheticSource::new(signal, stereo(), 0.0);
            let buf = s.render_seconds(2.0);
            let stats = SignalStats::compute(buf);
            assert!(stats.peak <= 1.0, "{signal:?} peaked at {}", stats.peak);
            assert_eq!(stats.clipped_samples, 0, "{signal:?} clipped");
        }
    }

    #[test]
    fn rendering_is_continuous_across_calls() {
        let mut a = SyntheticSource::new(TestSignal::Sine { hz: 500.0 }, stereo(), 0.0);
        let whole = a.render_seconds(0.2);

        let mut b = SyntheticSource::new(TestSignal::Sine { hz: 500.0 }, stereo(), 0.0);
        let mut split = Vec::new();
        b.render(4800, &mut split);
        b.render(4800, &mut split);

        assert_eq!(whole.len(), split.len());
        for (i, (x, y)) in whole.iter().zip(split.iter()).enumerate() {
            assert!((x - y).abs() < 1e-6, "sample {i} diverged: {x} vs {y}");
        }
    }

    #[test]
    fn centre_pan_is_balanced_in_stereo() {
        let g = pan_gains(0.0, &channel_layout(MASK_STEREO, 2));
        assert!((g[0] - g[1]).abs() < 1e-6);
        // Constant power: the gains square-sum to 1.
        assert!((g[0] * g[0] + g[1] * g[1] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn hard_pan_puts_everything_in_one_speaker() {
        let layout = channel_layout(MASK_STEREO, 2);
        let left = pan_gains(-30.0, &layout);
        assert!(left[0] > 0.999 && left[1] < 1e-3, "{left:?}");
        let right = pan_gains(30.0, &layout);
        assert!(right[1] > 0.999 && right[0] < 1e-3, "{right:?}");
    }

    #[test]
    fn lfe_never_receives_signal() {
        let layout = channel_layout(MASK_5_1, 6);
        for azimuth in [-170.0, -90.0, 0.0, 45.0, 179.0] {
            let g = pan_gains(azimuth, &layout);
            assert_eq!(g[3], 0.0, "LFE got signal at azimuth {azimuth}");
        }
    }

    #[test]
    fn panning_round_trips_through_the_direction_finder_in_stereo() {
        let layout = channel_layout(MASK_STEREO, 2);
        for target in [-30.0, -20.0, -10.0, 0.0, 10.0, 20.0, 30.0] {
            let mut s = SyntheticSource::new(TestSignal::Noise, stereo(), target);
            let buf = s.render_seconds(0.2);
            let powers = channel_powers(&buf, 2);
            let e = estimate(&powers, &layout, None);
            assert_eq!(e.method, DirectionMethod::StereoPanLaw);
            assert!(
                angular_error_deg(e.azimuth_deg, target) < 1.0,
                "target {target}, measured {}",
                e.azimuth_deg
            );
        }
    }

    #[test]
    fn panning_round_trips_through_the_direction_finder_in_surround() {
        let layout = channel_layout(MASK_7_1, 8);
        // Every quadrant, including behind — which stereo cannot resolve at all.
        for target in [-135.0, -90.0, -45.0, 0.0, 45.0, 90.0, 135.0, 180.0] {
            let mut s = SyntheticSource::new(TestSignal::Noise, surround(), target);
            let buf = s.render_seconds(0.2);
            let powers = channel_powers(&buf, 8);
            let e = estimate(&powers, &layout, None);
            assert_eq!(e.method, DirectionMethod::SurroundVector);
            assert!(
                angular_error_deg(e.azimuth_deg, target) < 10.0,
                "target {target}, measured {} (confidence {})",
                e.azimuth_deg,
                e.confidence
            );
            assert!(
                e.confidence > 0.7,
                "confidence {} at {target}",
                e.confidence
            );
        }
    }

    #[test]
    fn surround_beats_stereo_for_rear_sources() {
        // The reason the README tells you to switch the endpoint to 7.1.
        let target = 150.0;
        let mut surround_source = SyntheticSource::new(TestSignal::Noise, surround(), target);
        let buf = surround_source.render_seconds(0.2);
        let powers = channel_powers(&buf, 8);
        let e = estimate(&powers, &channel_layout(MASK_7_1, 8), None);
        assert!(
            angular_error_deg(e.azimuth_deg, target) < 10.0,
            "target {target}, measured {}",
            e.azimuth_deg
        );

        // Stereo cannot even represent it: the panner clamps to the front arc.
        let stereo_gains = pan_gains(target, &channel_layout(MASK_STEREO, 2));
        let e = estimate(
            &[stereo_gains[0].powi(2), stereo_gains[1].powi(2)],
            &channel_layout(MASK_STEREO, 2),
            None,
        );
        assert!(e.front_back_ambiguous);
        assert!(angular_error_deg(e.azimuth_deg, target) > 90.0);
    }

    #[test]
    fn landscape_repeats_on_the_documented_period() {
        let format = StreamFormat::new(8_000, 1, 0, SampleFormat::F32);
        let mut s = SyntheticSource::new(TestSignal::Landscape, format, 0.0);
        // Render two full cycles and compare the second against the first.
        let buf = s.render_seconds(LANDSCAPE_PERIOD_SECONDS * 2.0);
        let period_samples = (LANDSCAPE_PERIOD_SECONDS * 8_000.0) as usize;

        // Compare envelopes rather than samples: stroke phase resets each cycle,
        // so the fine structure differs while the picture does not.
        let window = 800; // 0.1 s
        let rms = |slice: &[f32]| -> f32 {
            (slice.iter().map(|v| v * v).sum::<f32>() / slice.len() as f32).sqrt()
        };
        let mut compared = 0;
        for start in (0..period_samples - window).step_by(window * 10) {
            let a = rms(&buf[start..start + window]);
            let b = rms(&buf[start + period_samples..start + period_samples + window]);
            assert!(
                (a - b).abs() < 0.02,
                "cycle mismatch at {start}: {a} vs {b}"
            );
            compared += 1;
        }
        assert!(compared > 50, "only compared {compared} windows");
    }

    #[test]
    fn landscape_has_the_documented_feature_offsets() {
        let format = StreamFormat::new(8_000, 1, 0, SampleFormat::F32);
        let mut s = SyntheticSource::new(TestSignal::Landscape, format, 0.0);
        let buf = s.render_seconds(LANDSCAPE_PERIOD_SECONDS);
        let sr = 8_000usize;

        let rms_at = |t: f32| -> f32 {
            let start = (t * sr as f32) as usize;
            let end = (start + sr / 2).min(buf.len());
            let slice = &buf[start..end];
            (slice.iter().map(|v| v * v).sum::<f32>() / slice.len() as f32).sqrt()
        };

        // Active inside each documented feature.
        for t in [22.0, 29.0, 60.0, 81.0, 86.0] {
            assert!(
                rms_at(t) > 0.01,
                "expected signal at {t} s, got {}",
                rms_at(t)
            );
        }
        // Quiet in the gaps between them, and after the tail ends at 1:28.
        for t in [5.0, 40.0, 95.0, 105.0] {
            assert!(
                rms_at(t) < 0.005,
                "expected quiet at {t} s, got {}",
                rms_at(t)
            );
        }
    }

    #[test]
    fn landscape_spectrogram_climbs_and_falls_like_a_mountain() {
        let format = StreamFormat::new(16_000, 1, 0, SampleFormat::F32);
        let mut s = SyntheticSource::new(TestSignal::Landscape, format, 0.0);
        let buf = s.render_seconds(LANDSCAPE_PERIOD_SECONDS);

        let size = 2048;
        let mut stft = Stft::new(size, size);
        let mut spectrum = stft.make_spectrum();
        let mut db = vec![0.0; spectrum.len()];

        let peak_hz_at =
            |stft: &mut Stft, spectrum: &mut Vec<_>, db: &mut Vec<f32>, t: f32| -> f32 {
                let start = (t * 16_000.0) as usize;
                stft.process(&buf[start..start + size], spectrum);
                stft.magnitudes_db(spectrum, db);
                stft.bin_hz(argmax(db).unwrap(), 16_000)
            };

        // The mountain climbs from 48 s to its crest at 66 s, then descends.
        let early = peak_hz_at(&mut stft, &mut spectrum, &mut db, 50.0);
        let crest = peak_hz_at(&mut stft, &mut spectrum, &mut db, 65.0);
        let late = peak_hz_at(&mut stft, &mut spectrum, &mut db, 78.0);

        assert!(crest > early * 2.0, "climb: {early} Hz -> {crest} Hz");
        assert!(crest > late * 2.0, "descent: {crest} Hz -> {late} Hz");
    }

    #[test]
    fn landscape_can_be_panned() {
        let layout = channel_layout(MASK_7_1, 8);
        let target = -60.0;
        let mut s = SyntheticSource::new(TestSignal::Landscape, surround(), target);
        // Render across the mountain, where there is reliably signal.
        let mut buf = Vec::new();
        s.render((50.0 * 48_000.0) as usize, &mut buf);
        buf.clear();
        s.render((10.0 * 48_000.0) as usize, &mut buf);

        let powers = channel_powers(&buf, 8);
        let e = estimate(&powers, &layout, None);
        assert!(
            angular_error_deg(e.azimuth_deg, target) < 10.0,
            "target {target}, measured {}",
            e.azimuth_deg
        );
    }

    #[test]
    fn a_mono_layout_gets_all_the_signal() {
        let format = StreamFormat::new(48_000, 1, 0, SampleFormat::F32);
        let mut s = SyntheticSource::new(TestSignal::Sine { hz: 440.0 }, format, 45.0);
        assert_eq!(s.gains(), &[1.0]);
        let buf = s.render_seconds(0.05);
        assert!(!SignalStats::compute(buf).is_silent());
    }
}

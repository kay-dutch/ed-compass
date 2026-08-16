//! Short-time Fourier transform — the primary instrument.
//!
//! `Stft` transforms one frame; `StftStream` buffers a sample stream and emits
//! hop-aligned frames. Frames are counted so that a spectrogram column can
//! always be mapped back to an absolute position on the capture timeline, which
//! is what lets a detection reach back into the PCM ring for its pre-roll.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};

use crate::analysis::statistics::DB_FLOOR;

/// Periodic Hann window — the correct variant for spectral analysis, as
/// opposed to the symmetric one used for filter design.
pub fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / size as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect()
}

pub struct Stft {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Scales a windowed bin magnitude back to the amplitude of the sinusoid
    /// that produced it.
    amplitude_scale: f32,
    size: usize,
    hop: usize,
    scratch: Vec<f32>,
}

impl Stft {
    pub fn new(size: usize, hop: usize) -> Self {
        assert!(size >= 2, "fft size must be at least 2");
        assert!(hop > 0 && hop <= size, "hop must be in 1..=size");
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(size);
        let window = hann_window(size);
        // A full-scale sinusoid on a bin centre peaks at A·Σw/2.
        let amplitude_scale = 2.0 / window.iter().sum::<f32>();
        Self {
            fft,
            window,
            amplitude_scale,
            size,
            hop,
            scratch: vec![0.0; size],
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Number of bins produced, `size/2 + 1` including DC and Nyquist.
    pub fn bins(&self) -> usize {
        self.size / 2 + 1
    }

    pub fn make_spectrum(&self) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); self.bins()]
    }

    pub fn bin_hz(&self, bin: usize, sample_rate: u32) -> f32 {
        bin as f32 * sample_rate as f32 / self.size as f32
    }

    pub fn hz_to_bin(&self, hz: f32, sample_rate: u32) -> usize {
        let b = (hz * self.size as f32 / sample_rate as f32).round();
        (b.max(0.0) as usize).min(self.bins() - 1)
    }

    /// Seconds advanced per frame.
    pub fn frame_seconds(&self, sample_rate: u32) -> f32 {
        self.hop as f32 / sample_rate as f32
    }

    /// Window one frame and transform it. `frame` must be exactly `size` long.
    pub fn process(&mut self, frame: &[f32], out: &mut [Complex32]) {
        assert_eq!(
            frame.len(),
            self.size,
            "frame must be exactly fft_size long"
        );
        assert_eq!(out.len(), self.bins(), "output must be exactly bins() long");
        for (dst, (src, w)) in self
            .scratch
            .iter_mut()
            .zip(frame.iter().zip(self.window.iter()))
        {
            *dst = src * w;
        }
        // The plan and buffers are sized together at construction, so the only
        // documented failure mode cannot occur here.
        self.fft
            .process(&mut self.scratch, out)
            .expect("fft buffers are sized by the plan itself");
    }

    /// Bin magnitudes in dBFS, written into `out`.
    pub fn magnitudes_db(&self, spectrum: &[Complex32], out: &mut [f32]) {
        debug_assert_eq!(spectrum.len(), out.len());
        for (dst, c) in out.iter_mut().zip(spectrum.iter()) {
            let amplitude = c.norm() * self.amplitude_scale;
            *dst = if amplitude <= 1e-12 {
                DB_FLOOR
            } else {
                (20.0 * amplitude.log10()).max(DB_FLOOR)
            };
        }
    }

    /// Bin powers (squared, scaled amplitude), written into `out`.
    pub fn powers(&self, spectrum: &[Complex32], out: &mut [f32]) {
        debug_assert_eq!(spectrum.len(), out.len());
        let s = self.amplitude_scale * self.amplitude_scale;
        for (dst, c) in out.iter_mut().zip(spectrum.iter()) {
            *dst = c.norm_sqr() * s;
        }
    }
}

/// Buffers a sample stream and emits hop-aligned frames.
pub struct StftStream {
    stft: Stft,
    pending: Vec<f32>,
    /// Frames emitted since construction — the spectrogram timeline.
    frames_emitted: u64,
}

impl StftStream {
    pub fn new(size: usize, hop: usize) -> Self {
        Self {
            stft: Stft::new(size, hop),
            pending: Vec::with_capacity(size * 2),
            frames_emitted: 0,
        }
    }

    pub fn stft(&self) -> &Stft {
        &self.stft
    }

    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    /// Sample index at which the frame with the given index begins.
    pub fn frame_start_sample(&self, frame_index: u64) -> u64 {
        frame_index * self.stft.hop as u64
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
    }

    /// Transform the next available frame, if there is one. Returns the index
    /// of the frame produced.
    pub fn next_frame(&mut self, out: &mut [Complex32]) -> Option<u64> {
        if self.pending.len() < self.stft.size {
            return None;
        }
        // Borrow-splitting: `process` needs `&mut self.stft` while reading
        // `self.pending`, so take the frame slice through a raw split.
        let (frame, rest) = self.pending.split_at(self.stft.size);
        let _ = rest;
        let frame: Vec<f32> = frame.to_vec();
        self.stft.process(&frame, out);
        self.pending.drain(..self.stft.hop);
        let index = self.frames_emitted;
        self.frames_emitted += 1;
        Some(index)
    }

    /// Drop buffered samples without resetting the frame counter — used when a
    /// timeline gap makes the partial frame meaningless.
    pub fn discard_partial(&mut self) {
        self.pending.clear();
    }
}

/// Spectral flatness (Wiener entropy) of a power spectrum, in `0..=1`.
///
/// Near 1 for noise, near 0 for a pure tone. This is what separates "a hiss got
/// louder" from "something is transmitting".
pub fn spectral_flatness(powers: &[f32]) -> f32 {
    let usable: Vec<f32> = powers
        .iter()
        .copied()
        .filter(|p| p.is_finite() && *p > 0.0)
        .collect();
    if usable.len() < 2 {
        return 0.0;
    }
    // Geometric mean via logs, so a long spectrum cannot underflow the product.
    let log_sum: f64 = usable.iter().map(|p| (*p as f64).ln()).sum();
    let geometric = (log_sum / usable.len() as f64).exp();
    let arithmetic = usable.iter().map(|p| *p as f64).sum::<f64>() / usable.len() as f64;
    if arithmetic <= 0.0 {
        return 0.0;
    }
    ((geometric / arithmetic) as f32).clamp(0.0, 1.0)
}

/// Bin index of the largest value in a range, or `None` if the range is empty.
pub fn argmax(values: &[f32]) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn sine(freq: f32, len: usize, amplitude: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amplitude * (std::f32::consts::TAU * freq * i as f32 / SR as f32).sin())
            .collect()
    }

    #[test]
    fn hann_window_is_periodic_and_starts_at_zero() {
        let w = hann_window(8);
        assert!((w[0]).abs() < 1e-6);
        // A periodic Hann does not return to zero at the last sample.
        assert!(w[7] > 0.0);
        // Symmetric about the centre.
        for i in 1..4 {
            assert!((w[i] - w[8 - i]).abs() < 1e-6);
        }
    }

    #[test]
    fn bin_count_and_frequency_mapping() {
        let s = Stft::new(4096, 2048);
        assert_eq!(s.bins(), 2049);
        assert!((s.bin_hz(0, SR)).abs() < 1e-6);
        assert!(
            (s.bin_hz(2048, SR) - 24_000.0).abs() < 1e-3,
            "last bin is Nyquist"
        );
        assert_eq!(s.hz_to_bin(0.0, SR), 0);
        assert_eq!(s.hz_to_bin(24_000.0, SR), 2048);
        // Out-of-range requests clamp rather than panic.
        assert_eq!(s.hz_to_bin(1e9, SR), 2048);
        assert_eq!(s.hz_to_bin(-100.0, SR), 0);
    }

    #[test]
    fn frame_rate_matches_the_spec_defaults() {
        let s = Stft::new(4096, 2048);
        // 48 kHz with a 2048 hop is 23.4 frames per second.
        assert!((1.0 / s.frame_seconds(SR) - 23.4375).abs() < 1e-3);
    }

    #[test]
    fn full_scale_sine_reads_zero_dbfs_at_its_bin() {
        let size = 4096;
        let mut stft = Stft::new(size, size / 2);
        // Land exactly on a bin centre so there is no scalloping loss.
        let bin = 100;
        let freq = bin as f32 * SR as f32 / size as f32;
        let mut spectrum = stft.make_spectrum();
        stft.process(&sine(freq, size, 1.0), &mut spectrum);

        let mut db = vec![0.0; spectrum.len()];
        stft.magnitudes_db(&spectrum, &mut db);
        assert_eq!(argmax(&db), Some(bin));
        assert!((db[bin]).abs() < 0.2, "expected ~0 dBFS, got {}", db[bin]);
    }

    #[test]
    fn half_amplitude_sine_reads_minus_six_dbfs() {
        let size = 2048;
        let mut stft = Stft::new(size, size / 2);
        let bin = 64;
        let freq = bin as f32 * SR as f32 / size as f32;
        let mut spectrum = stft.make_spectrum();
        stft.process(&sine(freq, size, 0.5), &mut spectrum);
        let mut db = vec![0.0; spectrum.len()];
        stft.magnitudes_db(&spectrum, &mut db);
        assert!((db[bin] + 6.02).abs() < 0.2, "got {}", db[bin]);
    }

    #[test]
    fn silence_reads_the_floor_everywhere() {
        let size = 512;
        let mut stft = Stft::new(size, size);
        let mut spectrum = stft.make_spectrum();
        stft.process(&vec![0.0; size], &mut spectrum);
        let mut db = vec![0.0; spectrum.len()];
        stft.magnitudes_db(&spectrum, &mut db);
        assert!(db.iter().all(|&v| v == DB_FLOOR));
        assert!(db.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn stream_emits_hop_aligned_frames() {
        let (size, hop) = (256, 64);
        let mut stream = StftStream::new(size, hop);
        let mut spectrum = stream.stft().make_spectrum();

        stream.push(&vec![0.0; size - 1]);
        assert_eq!(
            stream.next_frame(&mut spectrum),
            None,
            "not a full frame yet"
        );

        stream.push(&[0.0]);
        assert_eq!(stream.next_frame(&mut spectrum), Some(0));
        assert_eq!(stream.next_frame(&mut spectrum), None, "needs another hop");

        stream.push(&vec![0.0; hop]);
        assert_eq!(stream.next_frame(&mut spectrum), Some(1));
        assert_eq!(stream.frames_emitted(), 2);
        assert_eq!(stream.frame_start_sample(1), hop as u64);
    }

    #[test]
    fn stream_drains_a_long_push_into_many_frames() {
        let (size, hop) = (128, 32);
        let mut stream = StftStream::new(size, hop);
        let mut spectrum = stream.stft().make_spectrum();
        stream.push(&vec![0.1; 1024]);
        let mut count = 0;
        while stream.next_frame(&mut spectrum).is_some() {
            count += 1;
        }
        // (1024 - 128) / 32 + 1
        assert_eq!(count, 29);
    }

    #[test]
    fn stream_carries_a_tone_through_to_the_right_bin() {
        let (size, hop) = (2048, 512);
        let mut stream = StftStream::new(size, hop);
        let bin = 200;
        let freq = bin as f32 * SR as f32 / size as f32;
        stream.push(&sine(freq, size * 4, 1.0));

        let mut spectrum = stream.stft().make_spectrum();
        let mut db = vec![0.0; spectrum.len()];
        let mut seen = 0;
        while stream.next_frame(&mut spectrum).is_some() {
            stream.stft().magnitudes_db(&spectrum, &mut db);
            assert_eq!(argmax(&db), Some(bin));
            seen += 1;
        }
        assert!(seen >= 4);
    }

    #[test]
    fn discarding_a_partial_frame_keeps_the_frame_counter() {
        let mut stream = StftStream::new(64, 16);
        let mut spectrum = stream.stft().make_spectrum();
        stream.push(&vec![0.0; 64]);
        stream.next_frame(&mut spectrum).unwrap();
        stream.discard_partial();
        assert_eq!(stream.frames_emitted(), 1);
        assert_eq!(stream.next_frame(&mut spectrum), None);
    }

    #[test]
    fn flatness_separates_tones_from_noise() {
        let size = 2048;
        let mut stft = Stft::new(size, size);
        let mut spectrum = stft.make_spectrum();
        let mut powers = vec![0.0; spectrum.len()];

        stft.process(&sine(1000.0, size, 1.0), &mut spectrum);
        stft.powers(&spectrum, &mut powers);
        let tonal = spectral_flatness(&powers);

        // Deterministic pseudo-noise; no rng dependency.
        let mut state = 0x12345678u32;
        let noise: Vec<f32> = (0..size)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / 8_388_608.0 - 1.0
            })
            .collect();
        stft.process(&noise, &mut spectrum);
        stft.powers(&spectrum, &mut powers);
        let noisy = spectral_flatness(&powers);

        assert!(tonal < 0.05, "a pure tone should be far from flat: {tonal}");
        assert!(
            noisy > tonal * 5.0,
            "noise {noisy} should be flatter than tone {tonal}"
        );
    }

    #[test]
    fn flatness_handles_degenerate_input() {
        assert_eq!(spectral_flatness(&[]), 0.0);
        assert_eq!(spectral_flatness(&[1.0]), 0.0);
        assert_eq!(spectral_flatness(&[0.0, 0.0, 0.0]), 0.0);
        // A perfectly flat spectrum is flatness 1.
        assert!((spectral_flatness(&[1.0, 1.0, 1.0, 1.0]) - 1.0).abs() < 1e-5);
        assert!(spectral_flatness(&[f32::NAN, 1.0, 1.0]).is_finite());
    }

    #[test]
    fn argmax_ignores_nan_and_handles_empty() {
        assert_eq!(argmax(&[]), None);
        assert_eq!(argmax(&[1.0, 5.0, 3.0]), Some(1));
        assert_eq!(argmax(&[f32::NAN, 2.0]), Some(1));
        assert_eq!(argmax(&[f32::NAN]), None);
    }

    #[test]
    #[should_panic(expected = "hop must be in 1..=size")]
    fn rejects_a_hop_larger_than_the_frame() {
        Stft::new(64, 65);
    }
}

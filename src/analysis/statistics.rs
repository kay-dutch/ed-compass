//! Signal-health statistics and the amplitude histogram.
//!
//! These answer "is the capture working and sane" — clipping, silence, DC
//! offset, dynamic range. They do not find signals; the spectrogram does that.
//! Keeping the distinction explicit is deliberate: an amplitude histogram is
//! blind to spectrogram-domain structure like the Landscape Signal.

/// Amplitudes at or below this are treated as digital silence.
pub const SILENCE_FLOOR: f32 = 1e-6;

/// Reported level for silence, and the clamp for every dBFS conversion.
pub const DB_FLOOR: f32 = -120.0;

/// Linear amplitude to dBFS, floored so silence never yields `-inf` or `NaN`.
pub fn to_dbfs(amplitude: f32) -> f32 {
    let a = amplitude.abs();
    if a <= SILENCE_FLOOR {
        DB_FLOOR
    } else {
        (20.0 * a.log10()).max(DB_FLOOR)
    }
}

/// Power (already squared) to dBFS.
pub fn power_to_dbfs(power: f32) -> f32 {
    if power <= SILENCE_FLOOR * SILENCE_FLOOR {
        DB_FLOOR
    } else {
        (10.0 * power.log10()).max(DB_FLOOR)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalStats {
    pub sample_count: usize,
    pub rms: f32,
    pub rms_dbfs: f32,
    /// Largest absolute amplitude.
    pub peak: f32,
    pub peak_dbfs: f32,
    pub min: f32,
    pub max: f32,
    /// Mean sample value. A non-zero value on AC content indicates DC offset.
    pub dc_offset: f32,
    /// Sign changes per sample, in `0..=1`.
    pub zero_crossing_rate: f32,
    /// Samples at or beyond full scale.
    pub clipped_samples: usize,
}

impl SignalStats {
    pub fn empty() -> Self {
        Self {
            sample_count: 0,
            rms: 0.0,
            rms_dbfs: DB_FLOOR,
            peak: 0.0,
            peak_dbfs: DB_FLOOR,
            min: 0.0,
            max: 0.0,
            dc_offset: 0.0,
            zero_crossing_rate: 0.0,
            clipped_samples: 0,
        }
    }

    /// True when the window holds essentially nothing — drives the
    /// `● NO SIGNAL` indicator.
    pub fn is_silent(&self) -> bool {
        self.sample_count == 0 || self.peak <= SILENCE_FLOOR
    }

    /// Single pass over the window. Accumulates in `f64` so a 150-second
    /// window at 48 kHz does not lose precision in the sum of squares.
    pub fn compute<I: IntoIterator<Item = f32>>(samples: I) -> Self {
        let mut count = 0usize;
        let mut sum = 0.0f64;
        let mut sum_sq = 0.0f64;
        let mut peak = 0.0f32;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut crossings = 0usize;
        let mut clipped = 0usize;
        let mut previous = 0.0f32;

        for s in samples {
            // Guard against a device handing us a denormal or NaN.
            let s = if s.is_finite() { s } else { 0.0 };
            if count > 0 && previous * s < 0.0 {
                crossings += 1;
            }
            sum += s as f64;
            sum_sq += (s as f64) * (s as f64);
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            if a >= 1.0 {
                clipped += 1;
            }
            if s < min {
                min = s;
            }
            if s > max {
                max = s;
            }
            previous = s;
            count += 1;
        }

        if count == 0 {
            return Self::empty();
        }

        let rms = (sum_sq / count as f64).sqrt() as f32;
        Self {
            sample_count: count,
            rms,
            rms_dbfs: to_dbfs(rms),
            peak,
            peak_dbfs: to_dbfs(peak),
            min,
            max,
            dc_offset: (sum / count as f64) as f32,
            zero_crossing_rate: if count > 1 {
                crossings as f32 / (count - 1) as f32
            } else {
                0.0
            },
            clipped_samples: clipped,
        }
    }
}

/// Per-block summary, so windowed statistics never rescan the audio.
///
/// Rescanning a 150-second ring twice per snapshot at 10 Hz costs over a
/// billion sample-touches per second of audio — measured at 24% of a CPU core,
/// which was 98% of the application's total cost. Summaries are accumulated
/// once as audio arrives and merged on demand instead.
#[derive(Debug, Clone, PartialEq)]
struct BlockSummary {
    frames: usize,
    sum: f64,
    sum_sq: f64,
    peak: f32,
    min: f32,
    max: f32,
    crossings: usize,
    clipped: usize,
    /// First and last sample, so a sign change across a block seam still counts.
    first: f32,
    last: f32,
    histogram: Vec<u32>,
}

/// Rolling signal-health statistics over a short trailing window.
///
/// The window is deliberately short: "is the capture healthy right now" is a
/// level-meter question, and an RMS averaged over two and a half minutes
/// answers nothing useful.
#[derive(Debug, Clone)]
pub struct HealthWindow {
    blocks: std::collections::VecDeque<BlockSummary>,
    frames: usize,
    capacity_frames: usize,
    bin_count: usize,
}

impl HealthWindow {
    pub fn new(seconds: f32, sample_rate: u32, bin_count: usize) -> Self {
        assert!(bin_count >= 2, "a histogram needs at least two bins");
        Self {
            blocks: std::collections::VecDeque::new(),
            frames: 0,
            capacity_frames: (seconds.max(0.05) * sample_rate as f32).ceil() as usize,
            bin_count,
        }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Fold in one block of mono samples. This is the only pass over the audio.
    pub fn push<I: IntoIterator<Item = f32>>(&mut self, samples: I) {
        let mut block = BlockSummary {
            frames: 0,
            sum: 0.0,
            sum_sq: 0.0,
            peak: 0.0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            crossings: 0,
            clipped: 0,
            first: 0.0,
            last: 0.0,
            histogram: vec![0; self.bin_count],
        };

        let n = self.bin_count;
        let mut previous = 0.0f32;
        for s in samples {
            let s = if s.is_finite() { s } else { 0.0 };
            if block.frames == 0 {
                block.first = s;
            } else if previous * s < 0.0 {
                block.crossings += 1;
            }
            block.sum += s as f64;
            block.sum_sq += (s as f64) * (s as f64);
            let a = s.abs();
            if a > block.peak {
                block.peak = a;
            }
            if a >= 1.0 {
                block.clipped += 1;
            }
            if s < block.min {
                block.min = s;
            }
            if s > block.max {
                block.max = s;
            }
            // Inline binning: a separate histogram pass would double the cost.
            let idx = (((s + 1.0) * 0.5) * n as f32).floor();
            let idx = if idx < 0.0 {
                0
            } else if idx >= n as f32 {
                n - 1
            } else {
                idx as usize
            };
            block.histogram[idx] += 1;
            previous = s;
            block.frames += 1;
        }

        if block.frames == 0 {
            return;
        }
        block.last = previous;

        // A sign change across the seam belongs to the incoming block.
        if let Some(prev_block) = self.blocks.back()
            && prev_block.last * block.first < 0.0
        {
            block.crossings += 1;
        }

        self.frames += block.frames;
        self.blocks.push_back(block);
        while self.frames > self.capacity_frames && self.blocks.len() > 1 {
            if let Some(old) = self.blocks.pop_front() {
                self.frames -= old.frames;
            }
        }
    }

    /// Merge the resident block summaries. Cost is proportional to the number of
    /// blocks (tens), not to the number of samples (millions).
    pub fn stats(&self) -> SignalStats {
        if self.blocks.is_empty() {
            return SignalStats::empty();
        }
        let mut count = 0usize;
        let mut sum = 0.0f64;
        let mut sum_sq = 0.0f64;
        let mut peak = 0.0f32;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut crossings = 0usize;
        let mut clipped = 0usize;

        for b in &self.blocks {
            count += b.frames;
            sum += b.sum;
            sum_sq += b.sum_sq;
            peak = peak.max(b.peak);
            min = min.min(b.min);
            max = max.max(b.max);
            crossings += b.crossings;
            clipped += b.clipped;
        }

        let rms = (sum_sq / count as f64).sqrt() as f32;
        SignalStats {
            sample_count: count,
            rms,
            rms_dbfs: to_dbfs(rms),
            peak,
            peak_dbfs: to_dbfs(peak),
            min,
            max,
            dc_offset: (sum / count as f64) as f32,
            zero_crossing_rate: if count > 1 {
                crossings as f32 / (count - 1) as f32
            } else {
                0.0
            },
            clipped_samples: clipped,
        }
    }

    /// Merged amplitude histogram over the window.
    pub fn histogram(&self) -> Vec<u64> {
        let mut out = vec![0u64; self.bin_count];
        for b in &self.blocks {
            for (dst, src) in out.iter_mut().zip(b.histogram.iter()) {
                *dst += *src as u64;
            }
        }
        out
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.frames = 0;
    }
}

/// Distribution of instantaneous sample amplitudes across `[-1, +1]`.
///
/// X axis is amplitude, Y axis is the count of samples in that range. This is
/// explicitly *not* a frequency histogram.
#[derive(Debug, Clone, PartialEq)]
pub struct AmplitudeHistogram {
    bins: Vec<u64>,
    total: u64,
    /// Samples that fell outside `[-1, +1]` and were clamped into the end bins.
    out_of_range: u64,
}

impl AmplitudeHistogram {
    pub fn new(bin_count: usize) -> Self {
        assert!(bin_count >= 2, "a histogram needs at least two bins");
        Self {
            bins: vec![0; bin_count],
            total: 0,
            out_of_range: 0,
        }
    }

    pub fn bin_count(&self) -> usize {
        self.bins.len()
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn out_of_range(&self) -> u64 {
        self.out_of_range
    }

    pub fn counts(&self) -> &[u64] {
        &self.bins
    }

    pub fn clear(&mut self) {
        self.bins.fill(0);
        self.total = 0;
        self.out_of_range = 0;
    }

    /// Half-open `[low, high)` amplitude range of a bin. The final bin is
    /// closed at +1.0.
    pub fn bin_range(&self, index: usize) -> (f32, f32) {
        let n = self.bins.len() as f32;
        let width = 2.0 / n;
        let low = -1.0 + index as f32 * width;
        (low, low + width)
    }

    /// Which bin an amplitude lands in. Out-of-range values clamp to the ends
    /// rather than being dropped, so the total always matches the sample count.
    pub fn bin_index(&self, amplitude: f32) -> usize {
        let n = self.bins.len();
        if !amplitude.is_finite() {
            return n / 2;
        }
        let normalized = (amplitude + 1.0) * 0.5; // [-1, 1] -> [0, 1]
        let idx = (normalized * n as f32).floor();
        if idx < 0.0 {
            0
        } else if idx >= n as f32 {
            n - 1
        } else {
            idx as usize
        }
    }

    pub fn add(&mut self, amplitude: f32) {
        if !(-1.0..=1.0).contains(&amplitude) {
            self.out_of_range += 1;
        }
        let i = self.bin_index(amplitude);
        self.bins[i] += 1;
        self.total += 1;
    }

    pub fn add_all<I: IntoIterator<Item = f32>>(&mut self, samples: I) {
        for s in samples {
            self.add(s);
        }
    }

    /// Bin counts as fractions of the total, summing to 1.0 (or all zeros when
    /// empty).
    pub fn normalized(&self) -> Vec<f32> {
        if self.total == 0 {
            return vec![0.0; self.bins.len()];
        }
        let inv = 1.0 / self.total as f32;
        self.bins.iter().map(|&c| c as f32 * inv).collect()
    }

    /// Tallest bin as a fraction, for scaling the display.
    pub fn peak_fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        self.bins.iter().copied().max().unwrap_or(0) as f32 / self.total as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32, tol: f32) {
        assert!((a - b).abs() <= tol, "{a} not within {tol} of {b}");
    }

    #[test]
    fn empty_window_is_reported_as_empty_not_as_a_crash() {
        let s = SignalStats::compute(std::iter::empty());
        assert_eq!(s.sample_count, 0);
        assert_eq!(s.rms_dbfs, DB_FLOOR);
        assert!(s.is_silent());
        assert!(s.rms.is_finite() && s.zero_crossing_rate.is_finite());
    }

    #[test]
    fn silence_reads_as_silence() {
        let s = SignalStats::compute(vec![0.0f32; 1000]);
        assert_eq!(s.rms, 0.0);
        assert_eq!(s.peak, 0.0);
        assert_eq!(s.rms_dbfs, DB_FLOOR);
        assert_eq!(s.zero_crossing_rate, 0.0);
        assert!(s.is_silent());
    }

    #[test]
    fn constant_dc_has_no_crossings_and_a_measurable_offset() {
        let s = SignalStats::compute(vec![0.5f32; 512]);
        near(s.rms, 0.5, 1e-6);
        near(s.dc_offset, 0.5, 1e-6);
        assert_eq!(s.zero_crossing_rate, 0.0);
        assert_eq!(s.min, 0.5);
        assert_eq!(s.max, 0.5);
        assert!(!s.is_silent());
    }

    #[test]
    fn full_scale_positive_and_negative() {
        let p = SignalStats::compute(vec![1.0f32; 100]);
        near(p.peak_dbfs, 0.0, 1e-4);
        assert_eq!(p.clipped_samples, 100);
        assert_eq!(p.max, 1.0);

        let n = SignalStats::compute(vec![-1.0f32; 100]);
        near(n.peak_dbfs, 0.0, 1e-4);
        assert_eq!(n.min, -1.0);
        assert_eq!(n.zero_crossing_rate, 0.0);
    }

    #[test]
    fn sine_rms_is_amplitude_over_root_two() {
        let n = 48_000;
        let sine: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 100.0 * i as f32 / n as f32).sin())
            .collect();
        let s = SignalStats::compute(sine);
        near(s.rms, 1.0 / 2.0f32.sqrt(), 1e-3);
        near(s.peak, 1.0, 1e-3);
        near(s.dc_offset, 0.0, 1e-4);
        // 100 cycles over 48000 samples => 200 crossings.
        near(s.zero_crossing_rate, 200.0 / 47_999.0, 1e-4);
    }

    #[test]
    fn alternating_signal_crosses_every_sample() {
        let alt: Vec<f32> = (0..100)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let s = SignalStats::compute(alt);
        near(s.zero_crossing_rate, 1.0, 1e-6);
    }

    #[test]
    fn single_sample_has_no_crossing_rate() {
        let s = SignalStats::compute(vec![0.7f32]);
        assert_eq!(s.sample_count, 1);
        assert_eq!(s.zero_crossing_rate, 0.0);
    }

    #[test]
    fn non_finite_samples_are_neutralized() {
        let s = SignalStats::compute(vec![f32::NAN, 0.5, f32::INFINITY, -0.5]);
        assert_eq!(s.sample_count, 4);
        assert!(s.rms.is_finite());
        assert!(s.peak.is_finite());
        near(s.peak, 0.5, 1e-6);
    }

    #[test]
    fn dbfs_conversions_are_bounded() {
        near(to_dbfs(1.0), 0.0, 1e-5);
        near(to_dbfs(0.5), -6.0206, 1e-3);
        assert_eq!(to_dbfs(0.0), DB_FLOOR);
        assert_eq!(to_dbfs(-0.0), DB_FLOOR);
        assert_eq!(to_dbfs(1e-30), DB_FLOOR);
        near(to_dbfs(-0.5), -6.0206, 1e-3);
        near(power_to_dbfs(1.0), 0.0, 1e-5);
        assert_eq!(power_to_dbfs(0.0), DB_FLOOR);
    }

    #[test]
    fn histogram_bins_span_minus_one_to_plus_one() {
        let h = AmplitudeHistogram::new(100);
        let (low, _) = h.bin_range(0);
        let (_, high) = h.bin_range(99);
        near(low, -1.0, 1e-6);
        near(high, 1.0, 1e-6);
        // 100 bins across a range of 2.0 is 0.02 per bin. (The spec's worked
        // example lists 0.05-wide bins, which is a 40-bin histogram — it is an
        // illustration of the binning, not of the default bin count.)
        let (a, b) = h.bin_range(0);
        near(b - a, 0.02, 1e-6);

        let forty = AmplitudeHistogram::new(40);
        let (a, b) = forty.bin_range(0);
        near(a, -1.0, 1e-6);
        near(b, -0.95, 1e-6);
    }

    #[test]
    fn histogram_bin_edges_are_half_open_at_the_low_side() {
        let h = AmplitudeHistogram::new(4); // edges at -1, -0.5, 0, 0.5, 1
        assert_eq!(h.bin_index(-1.0), 0);
        assert_eq!(h.bin_index(-0.75), 0);
        assert_eq!(h.bin_index(-0.5), 1);
        assert_eq!(h.bin_index(-0.25), 1);
        assert_eq!(h.bin_index(0.0), 2);
        assert_eq!(h.bin_index(0.49), 2);
        assert_eq!(h.bin_index(0.5), 3);
        assert_eq!(h.bin_index(1.0), 3, "the top bin is closed at +1.0");
    }

    #[test]
    fn out_of_range_samples_clamp_and_are_counted() {
        let mut h = AmplitudeHistogram::new(10);
        h.add(-4.0);
        h.add(4.0);
        assert_eq!(h.counts()[0], 1);
        assert_eq!(h.counts()[9], 1);
        assert_eq!(h.total(), 2);
        assert_eq!(h.out_of_range(), 2);
    }

    #[test]
    fn non_finite_samples_land_mid_scale_without_panicking() {
        let mut h = AmplitudeHistogram::new(10);
        h.add(f32::NAN);
        assert_eq!(h.total(), 1);
        assert_eq!(h.counts().iter().sum::<u64>(), 1);
    }

    #[test]
    fn normalization_sums_to_one() {
        let mut h = AmplitudeHistogram::new(8);
        h.add_all(vec![-0.9, -0.3, 0.0, 0.2, 0.2, 0.85]);
        let n = h.normalized();
        near(n.iter().sum::<f32>(), 1.0, 1e-6);
        // 0.0 and both 0.2 samples share bin 4 (edges at 0.0 and 0.25).
        assert_eq!(h.counts()[4], 3);
        near(h.peak_fraction(), 3.0 / 6.0, 1e-6);
    }

    #[test]
    fn empty_histogram_normalizes_to_zeros() {
        let h = AmplitudeHistogram::new(16);
        assert_eq!(h.normalized(), vec![0.0; 16]);
        assert_eq!(h.peak_fraction(), 0.0);
    }

    #[test]
    fn silence_concentrates_in_the_centre_bins() {
        let mut h = AmplitudeHistogram::new(100);
        h.add_all(vec![0.0f32; 1000]);
        assert_eq!(h.counts()[50], 1000);
    }

    #[test]
    fn clearing_resets_every_counter() {
        let mut h = AmplitudeHistogram::new(4);
        h.add_all(vec![0.1, 5.0]);
        h.clear();
        assert_eq!(h.total(), 0);
        assert_eq!(h.out_of_range(), 0);
        assert!(h.counts().iter().all(|&c| c == 0));
    }
}

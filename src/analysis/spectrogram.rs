//! The two spectrogram memory tiers.
//!
//! Retaining raw PCM for hours is unaffordable; retaining *pictures* of it is
//! cheap. So the raw ring stays short (one Landscape cycle plus margin) and the
//! spectral history is kept in two quantized tiers instead:
//!
//! | tier | rate | width | cost |
//! |------|------|-------|------|
//! | display waterfall | STFT frame rate (~23/s) | `bins` (2049) | ~48 KB/s |
//! | long-term summary | ~1/s | `bands` (256) | ~256 B/s, ~0.9 MB/hour |
//!
//! Periodicity detection runs on the long-term tier. That is what makes an
//! all-evening session affordable — autocorrelating for a 109.5 s period does
//! not need 23 columns per second, and it certainly does not need raw samples.

/// dB values are quantized into a byte across this range. Anything quieter is
/// pinned to 0, anything louder to 255.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DbRange {
    pub min: f32,
    pub max: f32,
}

impl Default for DbRange {
    fn default() -> Self {
        // -120 dBFS is the analysis floor; +0 dBFS is full scale. 0.47 dB per
        // step, far finer than any threshold we test against.
        Self {
            min: -120.0,
            max: 0.0,
        }
    }
}

impl DbRange {
    pub fn quantize(&self, db: f32) -> u8 {
        if !db.is_finite() {
            return 0;
        }
        let t = (db - self.min) / (self.max - self.min);
        (t.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    pub fn dequantize(&self, q: u8) -> f32 {
        self.min + (q as f32 / 255.0) * (self.max - self.min)
    }
}

/// A fixed-capacity ring of quantized spectral frames.
///
/// Frames are stored contiguously at `frame_width` bytes each. `total_frames`
/// counts every frame ever pushed, so a column can be addressed by an absolute
/// index for as long as it remains resident.
#[derive(Debug)]
pub struct SpectrogramHistory {
    data: Vec<u8>,
    frame_width: usize,
    capacity: usize,
    write: usize,
    len: usize,
    total_frames: u64,
    range: DbRange,
}

impl SpectrogramHistory {
    pub fn new(frame_width: usize, capacity: usize, range: DbRange) -> Self {
        assert!(frame_width > 0, "frames must have at least one bin");
        assert!(capacity > 0, "history must hold at least one frame");
        Self {
            data: vec![0; frame_width * capacity],
            frame_width,
            capacity,
            write: 0,
            len: 0,
            total_frames: 0,
            range,
        }
    }

    pub fn frame_width(&self) -> usize {
        self.frame_width
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn oldest_frame(&self) -> u64 {
        self.total_frames - self.len as u64
    }

    pub fn range(&self) -> DbRange {
        self.range
    }

    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    /// Quantize and append a frame of dB values.
    pub fn push_db(&mut self, db: &[f32]) {
        assert_eq!(db.len(), self.frame_width, "frame width mismatch");
        let start = self.write * self.frame_width;
        for (dst, src) in self.data[start..start + self.frame_width]
            .iter_mut()
            .zip(db)
        {
            *dst = self.range.quantize(*src);
        }
        self.write = (self.write + 1) % self.capacity;
        self.len = (self.len + 1).min(self.capacity);
        self.total_frames += 1;
    }

    /// Frame by position from the oldest resident (0 = oldest).
    pub fn frame_at(&self, offset: usize) -> Option<&[u8]> {
        if offset >= self.len {
            return None;
        }
        let first = (self.write + self.capacity - self.len) % self.capacity;
        let slot = (first + offset) % self.capacity;
        let start = slot * self.frame_width;
        Some(&self.data[start..start + self.frame_width])
    }

    /// Frame by absolute index on the spectrogram timeline.
    pub fn frame_by_index(&self, index: u64) -> Option<&[u8]> {
        let oldest = self.oldest_frame();
        if index < oldest || index >= self.total_frames {
            return None;
        }
        self.frame_at((index - oldest) as usize)
    }

    /// Iterate resident frames, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        (0..self.len).filter_map(move |i| self.frame_at(i))
    }

    /// Dequantized dB for one bin across all resident frames, oldest first.
    /// This is the series the periodicity estimator consumes.
    pub fn bin_series(&self, bin: usize) -> Vec<f32> {
        assert!(bin < self.frame_width, "bin {bin} out of range");
        self.iter().map(|f| self.range.dequantize(f[bin])).collect()
    }

    /// Mean dB across all bins for each resident frame — a single broadband
    /// energy track, which is what the 109.5 s period shows up in most clearly.
    pub fn energy_series(&self) -> Vec<f32> {
        let inv = 1.0 / self.frame_width as f32;
        self.iter()
            .map(|f| {
                let sum: f32 = f.iter().map(|&q| self.range.dequantize(q)).sum();
                sum * inv
            })
            .collect()
    }
}

/// Log-spaced band edges spanning `low_hz..=high_hz`, `bands + 1` values.
///
/// Log spacing matches how spectral structure actually distributes — a linear
/// 256-band split would spend most of its resolution above 12 kHz where there
/// is nothing to see.
pub fn log_band_edges(bands: usize, low_hz: f32, high_hz: f32) -> Vec<f32> {
    assert!(bands >= 1, "need at least one band");
    assert!(
        low_hz > 0.0 && high_hz > low_hz,
        "band range must be positive and ordered"
    );
    let ratio = (high_hz / low_hz).powf(1.0 / bands as f32);
    (0..=bands).map(|i| low_hz * ratio.powi(i as i32)).collect()
}

/// Averages STFT frames down to the long-term tier: many bins to few bands,
/// many frames to one.
#[derive(Debug)]
pub struct LongTermSummarizer {
    /// For each band, the half-open bin range `[start, end)` it draws from.
    band_bins: Vec<(usize, usize)>,
    accumulator: Vec<f32>,
    frames_accumulated: usize,
    frames_per_summary: usize,
    edges: Vec<f32>,
}

impl LongTermSummarizer {
    /// `frames_per_summary` frames of `bins` are averaged into one frame of
    /// `bands`.
    pub fn new(
        bands: usize,
        bins: usize,
        sample_rate: u32,
        low_hz: f32,
        frames_per_summary: usize,
    ) -> Self {
        assert!(bins >= 2, "need at least two bins");
        assert!(
            frames_per_summary >= 1,
            "need at least one frame per summary"
        );
        let nyquist = sample_rate as f32 / 2.0;
        let low = low_hz.max(1.0).min(nyquist * 0.5);
        let edges = log_band_edges(bands, low, nyquist);
        let bin_hz = nyquist / (bins - 1) as f32;

        let band_bins = (0..bands)
            .map(|b| {
                let start = (edges[b] / bin_hz).floor() as usize;
                let end = (edges[b + 1] / bin_hz).ceil() as usize;
                // Every band must own at least one bin, even down where the
                // log spacing is finer than the FFT resolution.
                let start = start.min(bins - 1);
                let end = end.clamp(start + 1, bins);
                (start, end)
            })
            .collect();

        Self {
            band_bins,
            accumulator: vec![0.0; bands],
            frames_accumulated: 0,
            frames_per_summary,
            edges,
        }
    }

    pub fn bands(&self) -> usize {
        self.accumulator.len()
    }

    pub fn band_edges(&self) -> &[f32] {
        &self.edges
    }

    /// Centre frequency of a band, for axis labelling.
    pub fn band_center_hz(&self, band: usize) -> f32 {
        (self.edges[band] * self.edges[band + 1]).sqrt()
    }

    /// Feed one STFT frame of dB values. Returns a summary frame once
    /// `frames_per_summary` have been accumulated.
    pub fn push(&mut self, db: &[f32]) -> Option<Vec<f32>> {
        for (acc, &(start, end)) in self.accumulator.iter_mut().zip(self.band_bins.iter()) {
            let slice = &db[start..end.min(db.len()).max(start + 1).min(db.len())];
            if slice.is_empty() {
                continue;
            }
            // Mean dB rather than summed power: we are tracking the shape of
            // the spectrum over time, and a mean keeps one loud bin from
            // swamping the band.
            *acc += slice.iter().sum::<f32>() / slice.len() as f32;
        }
        self.frames_accumulated += 1;

        if self.frames_accumulated < self.frames_per_summary {
            return None;
        }
        let inv = 1.0 / self.frames_accumulated as f32;
        let out: Vec<f32> = self.accumulator.iter().map(|v| v * inv).collect();
        self.accumulator.fill(0.0);
        self.frames_accumulated = 0;
        Some(out)
    }

    /// Discard a partially accumulated summary, e.g. across a timeline gap.
    pub fn reset(&mut self) {
        self.accumulator.fill(0.0);
        self.frames_accumulated = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_round_trips_within_a_step() {
        let r = DbRange::default();
        for db in [-120.0, -90.0, -42.5, -6.0, 0.0] {
            let back = r.dequantize(r.quantize(db));
            assert!((back - db).abs() < 0.25, "{db} -> {back}");
        }
    }

    #[test]
    fn quantization_clamps_out_of_range_and_non_finite() {
        let r = DbRange::default();
        assert_eq!(r.quantize(-500.0), 0);
        assert_eq!(r.quantize(40.0), 255);
        assert_eq!(r.quantize(f32::NEG_INFINITY), 0);
        assert_eq!(r.quantize(f32::NAN), 0);
    }

    #[test]
    fn history_starts_empty() {
        let h = SpectrogramHistory::new(4, 3, DbRange::default());
        assert!(h.is_empty());
        assert_eq!(h.frame_at(0), None);
        assert_eq!(h.total_frames(), 0);
        assert_eq!(h.iter().count(), 0);
    }

    #[test]
    fn history_evicts_oldest_and_keeps_order() {
        let r = DbRange::default();
        let mut h = SpectrogramHistory::new(2, 3, r);
        for i in 0..5 {
            h.push_db(&[-100.0 + i as f32 * 10.0, -50.0]);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.total_frames(), 5);
        assert_eq!(h.oldest_frame(), 2);

        // Frames 2, 3, 4 survive, in order.
        let first = r.dequantize(h.frame_at(0).unwrap()[0]);
        let last = r.dequantize(h.frame_at(2).unwrap()[0]);
        assert!((first - (-80.0)).abs() < 0.5, "got {first}");
        assert!((last - (-60.0)).abs() < 0.5, "got {last}");
        assert_eq!(h.frame_at(3), None);
    }

    #[test]
    fn absolute_indexing_tracks_eviction() {
        let mut h = SpectrogramHistory::new(1, 2, DbRange::default());
        for _ in 0..4 {
            h.push_db(&[-10.0]);
        }
        assert!(h.frame_by_index(1).is_none(), "frame 1 has been evicted");
        assert!(h.frame_by_index(2).is_some());
        assert!(h.frame_by_index(3).is_some());
        assert!(
            h.frame_by_index(4).is_none(),
            "frame 4 has not been written"
        );
    }

    #[test]
    fn history_memory_matches_the_spec_budget() {
        // 2049 bins × 23.4 fps × 300 s ≈ 14 MB.
        let frames = (300.0f32 * 48_000.0 / 2048.0).ceil() as usize;
        let h = SpectrogramHistory::new(2049, frames, DbRange::default());
        let mb = h.bytes() as f32 / 1_048_576.0;
        assert!((13.0..16.0).contains(&mb), "waterfall tier is {mb} MB");
    }

    #[test]
    fn long_term_tier_is_under_a_megabyte_per_hour() {
        let h = SpectrogramHistory::new(256, 3600, DbRange::default());
        let mb = h.bytes() as f32 / 1_048_576.0;
        assert!(mb < 1.0, "long-term tier is {mb} MB per hour");
    }

    #[test]
    fn bin_series_walks_one_bin_through_time() {
        let mut h = SpectrogramHistory::new(3, 4, DbRange::default());
        h.push_db(&[-10.0, -20.0, -30.0]);
        h.push_db(&[-11.0, -21.0, -31.0]);
        let s = h.bin_series(1);
        assert_eq!(s.len(), 2);
        assert!((s[0] - (-20.0)).abs() < 0.5);
        assert!((s[1] - (-21.0)).abs() < 0.5);
    }

    #[test]
    fn energy_series_averages_across_bins() {
        let mut h = SpectrogramHistory::new(2, 4, DbRange::default());
        h.push_db(&[-20.0, -40.0]);
        let e = h.energy_series();
        assert_eq!(e.len(), 1);
        assert!((e[0] - (-30.0)).abs() < 0.5, "got {}", e[0]);
    }

    #[test]
    fn log_band_edges_span_the_range_geometrically() {
        let e = log_band_edges(4, 100.0, 1600.0);
        assert_eq!(e.len(), 5);
        assert!((e[0] - 100.0).abs() < 1e-3);
        assert!((e[4] - 1600.0).abs() < 1e-2);
        // Each step is a constant ratio (here, doubling).
        for i in 0..4 {
            assert!((e[i + 1] / e[i] - 2.0).abs() < 1e-4);
        }
    }

    #[test]
    fn summarizer_emits_once_per_group_of_frames() {
        let mut s = LongTermSummarizer::new(8, 1025, 48_000, 20.0, 3);
        let frame = vec![-50.0f32; 1025];
        assert!(s.push(&frame).is_none());
        assert!(s.push(&frame).is_none());
        let out = s.push(&frame).unwrap();
        assert_eq!(out.len(), 8);
        for v in out {
            assert!(
                (v - (-50.0)).abs() < 1e-3,
                "constant input averages to itself"
            );
        }
    }

    #[test]
    fn summarizer_bands_are_ordered_and_cover_the_spectrum() {
        let s = LongTermSummarizer::new(256, 2049, 48_000, 20.0, 24);
        assert_eq!(s.bands(), 256);
        let edges = s.band_edges();
        assert!((edges[0] - 20.0).abs() < 1e-3);
        assert!((edges[256] - 24_000.0).abs() < 1.0);
        for w in edges.windows(2) {
            assert!(w[1] > w[0], "band edges must increase");
        }
        // Centres sit inside their bands.
        for b in 0..256 {
            let c = s.band_center_hz(b);
            assert!(c > edges[b] && c < edges[b + 1]);
        }
    }

    #[test]
    fn summarizer_localizes_energy_to_the_right_band() {
        let bins = 2049;
        let sr = 48_000;
        let mut s = LongTermSummarizer::new(32, bins, sr, 20.0, 1);
        let mut frame = vec![-120.0f32; bins];
        // 1 kHz lands at bin 1000 * 4096 / 48000 ≈ 85.
        frame[85] = 0.0;
        let out = s.push(&frame).unwrap();
        let loudest = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        let centre = s.band_center_hz(loudest);
        assert!(
            (500.0..2000.0).contains(&centre),
            "energy landed at {centre} Hz"
        );
    }

    #[test]
    fn summarizer_reset_discards_partial_accumulation() {
        let mut s = LongTermSummarizer::new(4, 1025, 48_000, 20.0, 2);
        assert!(s.push(&vec![-10.0; 1025]).is_none());
        s.reset();
        // With the partial dropped, the next push starts a fresh group.
        assert!(s.push(&vec![-80.0; 1025]).is_none());
        let out = s.push(&vec![-80.0; 1025]).unwrap();
        for v in out {
            assert!(
                (v - (-80.0)).abs() < 1e-3,
                "reset should have dropped the -10 dB frame"
            );
        }
    }
}

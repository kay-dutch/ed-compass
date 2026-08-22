//! Generic novelty detection in the time-frequency plane.
//!
//! The design goal is that it finds signals nobody has catalogued, not just the
//! Landscape Signal. So there is no template here: the detector learns what the
//! background looks like, flags sustained departures from it, groups them into
//! blobs, and scores them by how structured they are.
//!
//! The one subtlety that matters more than the rest is the background model.
//! A moving *average* with a 60-second time constant adapts to an 80-second
//! mountain and erases the very thing it is meant to find — and the Landscape
//! Signal's mountain feature is exactly that long.
//!
//! The model is therefore a running **median** per bin, not a mean. A median is
//! decided by the middle of the distribution, so a signal occupying even a third
//! of the window cannot move it: the loud samples simply sort to one end. This
//! is what radio astronomy does per channel, and for the same reason.
//!
//! The same histogram also yields a **robust spread**, the interquartile
//! range, and that turns out to be necessary rather than a bonus. Noise in dB
//! is not tight: an exponential power distribution has a spread of several dB,
//! so a fixed "8 dB above background" threshold is meaningful only if you know
//! what the background's own scatter is. Measured, steady synthetic noise
//! produced 10 dB excursions and fired the detector. The threshold is therefore
//! whichever is larger — the configured dB, or a few times the bin's own
//! spread — which also answers the observation that ambience is a different
//! shape in different frequency bands: each bin now carries its own scale.
//!
//! It replaces two mechanisms that a mean needed to survive at all — asymmetric
//! rise and fall rates, and decision-directed freezing where a bin already above
//! its background stopped adapting for a bounded time. Both were compensations
//! for the mean being draggable. The median needs neither, and a genuine
//! permanent change is absorbed as soon as it occupies half the window, with no
//! special case to time out.

use crate::analysis::stft::spectral_flatness;
use crate::config::{Config, IgnoreBand};

/// Time-frequency geometry of the STFT frames being fed in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGeometry {
    pub sample_rate: u32,
    pub fft_size: usize,
    pub hop: usize,
}

impl FrameGeometry {
    pub fn bins(&self) -> usize {
        self.fft_size / 2 + 1
    }

    pub fn bin_hz(&self, bin: usize) -> f32 {
        bin as f32 * self.sample_rate as f32 / self.fft_size as f32
    }

    pub fn frame_seconds(&self) -> f32 {
        self.hop as f32 / self.sample_rate as f32
    }

    pub fn nyquist_hz(&self) -> f32 {
        self.sample_rate as f32 / 2.0
    }

    pub fn seconds_to_frames(&self, seconds: f32) -> usize {
        (seconds / self.frame_seconds()).round().max(1.0) as usize
    }
}

/// Quantisation of the dB axis for the per-bin histograms.
///
/// Half a decibel is finer than any threshold in the detector, and 256 buckets
/// spanning 128 dB covers the whole usable range above [`DB_FLOOR`] while
/// keeping a bucket index in one byte — which matters, because the window holds
/// one byte per bin per frame.
/// How often the per-bin spread is recomputed, in frames.
///
/// A full histogram walk per bin is far more work than the median's carried
/// pointer, and the scatter of the background changes over minutes, not frames.
const SPREAD_INTERVAL_FRAMES: usize = 32;

const BUCKETS: usize = 256;
const BUCKET_DB: f32 = 0.5;
const BUCKET_MIN_DB: f32 = -128.0;

fn to_bucket(db: f32) -> u8 {
    let scaled = (db - BUCKET_MIN_DB) / BUCKET_DB;
    scaled.clamp(0.0, (BUCKETS - 1) as f32) as u8
}

fn from_bucket(bucket: u8) -> f32 {
    BUCKET_MIN_DB + bucket as f32 * BUCKET_DB
}

/// Per-bin estimate of the background level, in dB.
///
/// A running median over a sliding window, held per bin as a 256-bucket
/// histogram with the median position carried between frames. Updating one bin
/// costs an increment, a decrement, and a short walk — the same technique the
/// structure detector uses for its median filters.
#[derive(Debug, Clone)]
pub struct BackgroundModel {
    /// Decoded median per bin, in dB.
    level_db: Vec<f32>,
    /// Sliding window of bucket indices, `bins` values per frame.
    ring: Vec<u8>,
    /// Frame slot the next write lands in.
    write: usize,
    /// Frames written so far, saturating at `window`.
    filled: usize,
    window: usize,
    /// `bins * BUCKETS` counts.
    hist: Vec<u16>,
    /// Carried median bucket per bin, and how many samples sit below it.
    median_bucket: Vec<u8>,
    below: Vec<u32>,
    /// Robust standard deviation per bin, from the interquartile range.
    ///
    /// Recomputed periodically rather than every frame: it needs a full walk of
    /// each bin's histogram, and the scatter of the background moves far more
    /// slowly than the background itself.
    spread_db: Vec<f32>,
    spread_countdown: usize,
    frames_seen: usize,
    warmup_frames: usize,
}

impl BackgroundModel {
    /// `time_constant` governs how fast the estimate rises. Falls are eight
    /// times faster, which is what makes it a floor tracker.
    ///
    /// `freeze_above_db` is the excess at which a bin stops adapting upward
    /// altogether, and `max_freeze_seconds` bounds how long that can last.
    /// `time_constant` is the length of the median window: the model describes
    /// the middle of the last `time_constant` seconds.
    ///
    /// `freeze_above_db` and `max_freeze_seconds` are accepted and ignored. They
    /// configured the decision-directed freeze that a mean-based model needed to
    /// avoid absorbing its own signal; a median does not need it. They remain in
    /// the signature so the configuration file and its callers are unchanged.
    pub fn new(
        bins: usize,
        frame_seconds: f32,
        time_constant: f32,
        _freeze_above_db: f32,
        _max_freeze_seconds: f32,
    ) -> Self {
        assert!(bins > 0, "background model needs at least one bin");
        assert!(frame_seconds > 0.0 && time_constant > 0.0);

        // The window is several times the configured time constant, because a
        // median only holds a signal that occupies *less than half* of it. At
        // one-to-one, a signal lasting the time constant fills the window and
        // becomes the background — the very failure this model replaced. At
        // four-to-one, anything up to twice the time constant survives, which
        // covers the Landscape Signal's 80-second mountain with room to spare.
        const WINDOW_FACTOR: f32 = 4.0;
        let window = (WINDOW_FACTOR * time_constant / frame_seconds)
            .ceil()
            .max(2.0) as usize;
        Self {
            level_db: vec![f32::NAN; bins],
            ring: vec![0; bins * window],
            write: 0,
            filled: 0,
            window,
            hist: vec![0; bins * BUCKETS],
            median_bucket: vec![0; bins],
            below: vec![0; bins],
            spread_db: vec![0.0; bins],
            spread_countdown: 0,
            frames_seen: 0,
            // Detection is suppressed until the window has filled, so starting
            // the application mid-signal does not fire instantly.
            warmup_frames: (time_constant / frame_seconds).ceil() as usize,
        }
    }

    pub fn is_warm(&self) -> bool {
        self.frames_seen >= self.warmup_frames
    }

    pub fn warmup_progress(&self) -> f32 {
        if self.warmup_frames == 0 {
            return 1.0;
        }
        (self.frames_seen as f32 / self.warmup_frames as f32).clamp(0.0, 1.0)
    }

    pub fn level_db(&self) -> &[f32] {
        &self.level_db
    }

    /// Fold a frame in and write per-bin excess (`frame − background`) to `out`.
    pub fn update(&mut self, frame_db: &[f32], out: &mut [f32]) {
        debug_assert_eq!(frame_db.len(), self.level_db.len());
        debug_assert_eq!(out.len(), self.level_db.len());

        let bins = self.level_db.len();
        let evicting = self.filled == self.window;
        let slot = self.write * bins;

        for i in 0..bins {
            let x = frame_db[i];
            let x = if x.is_finite() {
                x
            } else {
                from_bucket(self.median_bucket[i])
            };
            let bucket = to_bucket(x);

            // Out with the oldest sample in this bin, in with the newest.
            if evicting {
                let old = self.ring[slot + i];
                self.hist[i * BUCKETS + old as usize] -= 1;
                if old < self.median_bucket[i] {
                    self.below[i] -= 1;
                }
            }
            self.ring[slot + i] = bucket;
            self.hist[i * BUCKETS + bucket as usize] += 1;
            if bucket < self.median_bucket[i] {
                self.below[i] += 1;
            }

            // Walk the carried median pointer to wherever the middle now is.
            let count = if evicting {
                self.window
            } else {
                self.filled + 1
            } as u32;
            let target = count / 2;
            let row = i * BUCKETS;
            let mut m = self.median_bucket[i] as usize;
            let mut below = self.below[i];
            while below > target {
                m -= 1;
                below -= self.hist[row + m] as u32;
            }
            while below + self.hist[row + m] as u32 <= target && m + 1 < BUCKETS {
                below += self.hist[row + m] as u32;
                m += 1;
            }
            self.median_bucket[i] = m as u8;
            self.below[i] = below;

            let level = from_bucket(m as u8);
            self.level_db[i] = level;
            out[i] = x - level;
        }

        self.write = (self.write + 1) % self.window;
        self.filled = (self.filled + 1).min(self.window);
        self.frames_seen += 1;

        if self.spread_countdown == 0 {
            self.recompute_spread();
            self.spread_countdown = SPREAD_INTERVAL_FRAMES;
        } else {
            self.spread_countdown -= 1;
        }
    }

    /// Robust spread per bin, from the interquartile range.
    ///
    /// `IQR / 1.349` is the standard deviation of a Gaussian with the same
    /// quartiles, so the number is comparable to a sigma without being movable
    /// by the outliers a signal consists of.
    fn recompute_spread(&mut self) {
        let count = self.filled as u32;
        if count < 4 {
            self.spread_db.fill(0.0);
            return;
        }
        let (q1_target, q3_target) = (count / 4, (3 * count) / 4);
        for (i, spread) in self.spread_db.iter_mut().enumerate() {
            let row = &self.hist[i * BUCKETS..(i + 1) * BUCKETS];
            let mut seen = 0u32;
            let (mut q1, mut q3) = (0usize, BUCKETS - 1);
            let mut have_q1 = false;
            for (bucket, n) in row.iter().enumerate() {
                seen += *n as u32;
                if !have_q1 && seen > q1_target {
                    q1 = bucket;
                    have_q1 = true;
                }
                if seen > q3_target {
                    q3 = bucket;
                    break;
                }
            }
            let iqr = (q3 as f32 - q1 as f32) * BUCKET_DB;
            *spread = iqr / 1.349;
        }
    }

    /// Robust spread of each bin's background, in dB.
    pub fn spread_db(&self) -> &[f32] {
        &self.spread_db
    }

    /// Always zero. The freeze mechanism is gone with the mean it protected;
    /// the reading is kept so the UI that surfaces it does not have to change.
    pub fn frozen_bins(&self) -> usize {
        0
    }

    pub fn reset(&mut self) {
        self.level_db.fill(f32::NAN);
        self.ring.fill(0);
        self.hist.fill(0);
        self.median_bucket.fill(0);
        self.below.fill(0);
        self.spread_db.fill(0.0);
        self.spread_countdown = 0;
        self.write = 0;
        self.filled = 0;
        self.frames_seen = 0;
    }
}

/// A completed detection: a connected blob in the time-frequency plane.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectionEvent {
    pub start_frame: u64,
    pub end_frame: u64,
    pub start_seconds: f64,
    pub duration_seconds: f32,
    pub low_hz: f32,
    pub high_hz: f32,
    /// Frequency of the strongest bin over the event's life.
    pub peak_hz: f32,
    pub low_bin: usize,
    pub high_bin: usize,
    pub peak_excess_db: f32,
    pub mean_excess_db: f32,
    /// Movement of the peak from first frame to last. Sweeps and chirps show a
    /// large value; a steady tone shows ~0.
    pub drift_hz: f32,
    /// Mean spectral flatness within the event band. Near 0 is tonal, near 1 is
    /// noise-like.
    pub mean_flatness: f32,
    /// Composite 0..1 ranking. See `score_event`.
    pub score: f32,
}

impl DetectionEvent {
    pub fn bandwidth_hz(&self) -> f32 {
        self.high_hz - self.low_hz
    }
}

#[derive(Debug, Clone)]
struct OpenEvent {
    start_frame: u64,
    last_active_frame: u64,
    low_bin: usize,
    high_bin: usize,
    first_peak_bin: usize,
    last_peak_bin: usize,
    peak_bin: usize,
    peak_excess: f32,
    excess_sum: f64,
    excess_count: usize,
    flatness_sum: f64,
    flatness_count: usize,
    active_frames: usize,
}

/// Streaming detector over STFT frames.
pub struct NoveltyDetector {
    geometry: FrameGeometry,
    background: BackgroundModel,
    threshold_db: f32,
    noise_sigmas: f32,
    min_frames: usize,
    gap_frames: usize,
    /// Bins excluded by the configured ignore bands.
    ignored: Vec<bool>,
    detect_min_hz: f32,
    detect_max_hz: f32,
    open: Vec<OpenEvent>,
    frame_index: u64,
    excess: Vec<f32>,
    /// Bins of slack when matching a run to an open event, so a drifting tone
    /// stays one event instead of fragmenting into dozens.
    drift_tolerance_bins: usize,
}

impl NoveltyDetector {
    pub fn new(geometry: FrameGeometry, cfg: &Config) -> Self {
        let bins = geometry.bins();
        let frame_seconds = geometry.frame_seconds();
        let mut detector = Self {
            background: BackgroundModel::new(
                bins,
                frame_seconds,
                cfg.background_time_constant_seconds,
                // Freeze slightly before the detection threshold, so a signal
                // is protected from the moment it starts emerging.
                cfg.novelty_threshold_db * 0.75,
                cfg.background_max_freeze_seconds,
            ),
            threshold_db: cfg.novelty_threshold_db,
            noise_sigmas: cfg.novelty_sigmas.max(0.0),
            min_frames: geometry.seconds_to_frames(cfg.min_event_seconds),
            gap_frames: geometry.seconds_to_frames(cfg.event_gap_tolerance_seconds),
            ignored: vec![false; bins],
            // The band the detector is *told* to watch.
            //
            // This was missing entirely, and the omission was invisible because
            // every other consumer of the setting honoured it: the structure
            // scan is built from these bounds and so is the direction finder,
            // while novelty detection quietly ran from DC to Nyquist. In the
            // field that meant a detection reported at 4.4–6.7 kHz on a
            // configuration whose band ends at 2600 Hz, and a capture folder
            // holding a gigabyte of ship noise.
            detect_min_hz: cfg.detect_min_hz,
            detect_max_hz: cfg.detect_max_hz,
            open: Vec::new(),
            frame_index: 0,
            excess: vec![0.0; bins],
            drift_tolerance_bins: 2,
            geometry,
        };
        detector.set_ignore_bands(&cfg.ignore_bands);
        detector
    }

    pub fn geometry(&self) -> FrameGeometry {
        self.geometry
    }

    pub fn background(&self) -> &BackgroundModel {
        &self.background
    }

    /// Per-bin excess from the most recent frame — what the waterfall overlay
    /// draws.
    pub fn excess_db(&self) -> &[f32] {
        &self.excess
    }

    pub fn open_event_count(&self) -> usize {
        self.open.len()
    }

    /// Frequency span of the events open right now, in Hz.
    ///
    /// The events list only gains an entry once an event *closes*, which is too
    /// late for anything that wants to react while a signal is still arriving —
    /// a half-minute event would be over before the overlay noticed it. This is
    /// the same information a frame earlier.
    pub fn open_event_band(&self) -> Option<(f32, f32)> {
        let mut low = f32::INFINITY;
        let mut high = f32::NEG_INFINITY;
        for e in &self.open {
            low = low.min(self.geometry.bin_hz(e.low_bin));
            high = high.max(self.geometry.bin_hz(e.high_bin));
        }
        (low.is_finite() && high > low).then_some((low, high))
    }

    /// Recompute which bins are excluded from detection.
    ///
    /// Two reasons a bin is excluded, and both have to be applied here: it falls
    /// outside the configured detection band, or it sits inside a band the user
    /// has muted. Applying the detect band anywhere else would not survive this
    /// function, which rewrites the whole mask.
    ///
    /// Note this suppresses *detection* only. The background model still tracks
    /// every bin, so the excess display and the spectrogram are unaffected.
    pub fn set_ignore_bands(&mut self, bands: &[IgnoreBand]) {
        let (lo, hi) = (self.detect_min_hz, self.detect_max_hz);
        for (bin, flag) in self.ignored.iter_mut().enumerate() {
            let hz = self.geometry.bin_hz(bin);
            let out_of_band = hz < lo || hz > hi;
            *flag = out_of_band || bands.iter().any(|b| b.contains(hz));
        }
    }

    /// Feed one frame. `powers` is the linear power spectrum for the same frame,
    /// used for the tonality term. Returns any events that closed on this frame.
    pub fn push_frame(&mut self, frame_db: &[f32], powers: &[f32]) -> Vec<DetectionEvent> {
        assert_eq!(frame_db.len(), self.ignored.len(), "frame width mismatch");
        self.background.update(frame_db, &mut self.excess);
        let frame = self.frame_index;
        self.frame_index += 1;

        if !self.background.is_warm() {
            return Vec::new();
        }

        for (start, end) in self.active_runs() {
            self.absorb_run(frame, start, end, powers);
        }
        self.close_stale(frame)
    }

    /// Contiguous spans of bins over threshold, ignoring muted bands.
    fn active_runs(&self) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut start: Option<usize> = None;
        for (bin, &excess) in self.excess.iter().enumerate() {
            // The bar is the configured dB, or a few times this bin's own
            // scatter, whichever is higher. Noise in dB is wide — measured,
            // synthetic noise reached 10 dB above its median and fired the
            // detector against an 8 dB threshold — and how wide it is differs
            // from band to band, so a single fixed number cannot serve both a
            // quiet bin and a busy one.
            let spread = self.background.spread_db().get(bin).copied().unwrap_or(0.0);
            let bar = self.threshold_db.max(self.noise_sigmas * spread);
            let hot = !self.ignored[bin] && excess.is_finite() && excess >= bar;
            match (hot, start) {
                (true, None) => start = Some(bin),
                (false, Some(s)) => {
                    runs.push((s, bin));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            runs.push((s, self.excess.len()));
        }
        runs
    }

    /// Attach a run to an overlapping open event, merging any others it now
    /// bridges, or open a new one.
    fn absorb_run(&mut self, frame: u64, start: usize, end: usize, powers: &[f32]) {
        let slack = self.drift_tolerance_bins;
        let lo = start.saturating_sub(slack);
        let hi = end + slack;

        let matches: Vec<usize> = self
            .open
            .iter()
            .enumerate()
            .filter(|(_, e)| e.low_bin < hi && lo < e.high_bin + 1)
            .map(|(i, _)| i)
            .collect();

        let (peak_bin, peak_excess) = (start..end).map(|b| (b, self.excess[b])).fold(
            (start, f32::NEG_INFINITY),
            |acc, (b, v)| {
                if v > acc.1 { (b, v) } else { acc }
            },
        );
        let mean_excess = self.excess[start..end].iter().sum::<f32>() / (end - start) as f32;
        let flatness = spectral_flatness(&powers[start.min(powers.len())..end.min(powers.len())]);

        let target = match matches.first() {
            Some(&first) => {
                // Merge the rest into the first, newest-last so indices stay
                // valid while removing.
                for &other in matches[1..].iter().rev() {
                    let merged = self.open.remove(other);
                    let dst = &mut self.open[first];
                    dst.start_frame = dst.start_frame.min(merged.start_frame);
                    dst.last_active_frame = dst.last_active_frame.max(merged.last_active_frame);
                    dst.low_bin = dst.low_bin.min(merged.low_bin);
                    dst.high_bin = dst.high_bin.max(merged.high_bin);
                    dst.excess_sum += merged.excess_sum;
                    dst.excess_count += merged.excess_count;
                    dst.flatness_sum += merged.flatness_sum;
                    dst.flatness_count += merged.flatness_count;
                    dst.active_frames = dst.active_frames.max(merged.active_frames);
                    if merged.peak_excess > dst.peak_excess {
                        dst.peak_excess = merged.peak_excess;
                        dst.peak_bin = merged.peak_bin;
                    }
                }
                first
            }
            None => {
                self.open.push(OpenEvent {
                    start_frame: frame,
                    last_active_frame: frame,
                    low_bin: start,
                    high_bin: end,
                    first_peak_bin: peak_bin,
                    last_peak_bin: peak_bin,
                    peak_bin,
                    peak_excess: f32::NEG_INFINITY,
                    excess_sum: 0.0,
                    excess_count: 0,
                    flatness_sum: 0.0,
                    flatness_count: 0,
                    active_frames: 0,
                });
                self.open.len() - 1
            }
        };

        let e = &mut self.open[target];
        e.low_bin = e.low_bin.min(start);
        e.high_bin = e.high_bin.max(end);
        e.last_peak_bin = peak_bin;
        if peak_excess > e.peak_excess {
            e.peak_excess = peak_excess;
            e.peak_bin = peak_bin;
        }
        e.excess_sum += mean_excess as f64;
        e.excess_count += 1;
        if flatness > 0.0 {
            e.flatness_sum += flatness as f64;
            e.flatness_count += 1;
        }
        if e.last_active_frame != frame {
            e.last_active_frame = frame;
            e.active_frames += 1;
        } else if e.active_frames == 0 {
            e.active_frames = 1;
        }
    }

    /// Close events that have been quiet longer than the gap tolerance.
    fn close_stale(&mut self, frame: u64) -> Vec<DetectionEvent> {
        let gap = self.gap_frames as u64;
        let mut closed = Vec::new();
        let mut i = 0;
        while i < self.open.len() {
            if frame.saturating_sub(self.open[i].last_active_frame) > gap {
                let e = self.open.remove(i);
                if e.active_frames >= self.min_frames {
                    closed.push(self.finish(e));
                }
            } else {
                i += 1;
            }
        }
        closed
    }

    /// Close everything still open — used at end of stream or before a reset.
    pub fn flush(&mut self) -> Vec<DetectionEvent> {
        let open = std::mem::take(&mut self.open);
        open.into_iter()
            .filter(|e| e.active_frames >= self.min_frames)
            .map(|e| self.finish(e))
            .collect()
    }

    /// Drop in-progress events without disturbing the background estimate.
    /// Called when a timeline gap makes continuity across it a lie.
    pub fn reset_events(&mut self) {
        self.open.clear();
    }

    fn finish(&self, e: OpenEvent) -> DetectionEvent {
        let g = self.geometry;
        let duration_seconds = e.active_frames as f32 * g.frame_seconds();
        let mean_excess_db = if e.excess_count > 0 {
            (e.excess_sum / e.excess_count as f64) as f32
        } else {
            0.0
        };
        let mean_flatness = if e.flatness_count > 0 {
            (e.flatness_sum / e.flatness_count as f64) as f32
        } else {
            1.0 // no tonality evidence: assume the least interesting case
        };
        let mut event = DetectionEvent {
            start_frame: e.start_frame,
            end_frame: e.last_active_frame,
            start_seconds: e.start_frame as f64 * g.frame_seconds() as f64,
            duration_seconds,
            low_hz: g.bin_hz(e.low_bin),
            high_hz: g.bin_hz(e.high_bin),
            peak_hz: g.bin_hz(e.peak_bin),
            low_bin: e.low_bin,
            high_bin: e.high_bin,
            peak_excess_db: if e.peak_excess.is_finite() {
                e.peak_excess
            } else {
                0.0
            },
            mean_excess_db,
            drift_hz: g.bin_hz(e.last_peak_bin) - g.bin_hz(e.first_peak_bin),
            mean_flatness,
            score: 0.0,
        };
        event.score = score_event(&event, g.nyquist_hz());
        event
    }
}

/// Composite 0..1 ranking.
///
/// The weights encode what "interesting" means for this hunt: a strong,
/// sustained, tonal, narrowband, possibly-sweeping departure from the
/// background. Broadband noise that merely got louder scores low even when it
/// is very loud, which is the point.
pub fn score_event(event: &DetectionEvent, nyquist_hz: f32) -> f32 {
    let excess = (event.peak_excess_db / 24.0).clamp(0.0, 1.0);
    let duration = (event.duration_seconds / 10.0).clamp(0.0, 1.0);
    let tonality = (1.0 - event.mean_flatness).clamp(0.0, 1.0);
    let narrowness = if nyquist_hz > 0.0 {
        1.0 - (event.bandwidth_hz() / (nyquist_hz / 4.0)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let drift = (event.drift_hz.abs() / 1000.0).clamp(0.0, 1.0);

    (0.35 * excess + 0.20 * duration + 0.25 * tonality + 0.10 * narrowness + 0.10 * drift)
        .clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEOM: FrameGeometry = FrameGeometry {
        sample_rate: 48_000,
        fft_size: 1024,
        hop: 512,
    };

    fn cfg() -> Config {
        let mut c = Config::default();
        c.fft_size = GEOM.fft_size;
        c.hop = GEOM.hop;
        // Short constants so tests do not need thousands of frames.
        c.background_time_constant_seconds = 1.0;
        c.min_event_seconds = 0.2;
        c.event_gap_tolerance_seconds = 0.1;
        // These tests are about event mechanics — drift, dropouts, merging — and
        // use a tone at 4687 Hz. The band is opened explicitly so that intent
        // stays visible: with the shipped 180–2600 Hz band they would be testing
        // the band filter by accident, which is what `band_limits` is for.
        c.detect_min_hz = 0.0;
        c.detect_max_hz = GEOM.nyquist_hz();
        c
    }

    /// Bins outside the configured band must not produce detections.
    ///
    /// This is the bug that shipped: `detect_min_hz` and `detect_max_hz` shaped
    /// the structure scan and the direction finder, while novelty detection ran
    /// across the whole spectrum. In the field it reported an event at
    /// 4.4–6.7 kHz on a configuration whose band ends at 2600 Hz, and filled a
    /// gigabyte of disk with ship noise.
    #[test]
    fn detection_is_confined_to_the_configured_band() {
        let mut c = cfg();
        c.detect_min_hz = 500.0;
        c.detect_max_hz = 2_000.0;
        let mut d = NoveltyDetector::new(GEOM, &c);
        warm_up(&mut d);

        // 4687 Hz, far above the band, and very loud.
        let mut out_of_band = flat_frame(-90.0);
        out_of_band[100] = -20.0;
        for _ in 0..200 {
            assert!(
                d.push_frame(&out_of_band, &flat_powers()).is_empty(),
                "a 70 dB excess outside the band must not detect"
            );
        }
        assert_eq!(d.open_event_count(), 0, "nor open an event");

        // Below the band, equally loud.
        let mut too_low = flat_frame(-90.0);
        too_low[2] = -20.0; // ~94 Hz
        for _ in 0..200 {
            assert!(d.push_frame(&too_low, &flat_powers()).is_empty());
        }
        assert_eq!(d.open_event_count(), 0);

        // And inside it, the same excess is found.
        let mut in_band = flat_frame(-90.0);
        in_band[20] = -20.0; // 937 Hz
        for _ in 0..20 {
            d.push_frame(&in_band, &flat_powers());
        }
        assert!(
            d.open_event_count() > 0,
            "the band it was told to watch must still work"
        );
    }

    fn flat_frame(db: f32) -> Vec<f32> {
        vec![db; GEOM.bins()]
    }

    fn flat_powers() -> Vec<f32> {
        vec![1.0; GEOM.bins()]
    }

    /// Run enough quiet frames that the background model settles.
    fn warm_up(d: &mut NoveltyDetector) {
        let quiet = flat_frame(-90.0);
        let powers = flat_powers();
        for _ in 0..200 {
            d.push_frame(&quiet, &powers);
        }
        assert!(d.background().is_warm());
    }

    #[test]
    fn geometry_maps_bins_and_frames() {
        assert_eq!(GEOM.bins(), 513);
        assert!((GEOM.bin_hz(0)).abs() < 1e-6);
        assert!((GEOM.bin_hz(512) - 24_000.0).abs() < 1e-3);
        assert!((GEOM.frame_seconds() - 512.0 / 48_000.0).abs() < 1e-9);
        assert_eq!(GEOM.seconds_to_frames(1.0), 94);
        assert_eq!(GEOM.seconds_to_frames(0.0), 1, "never zero frames");
    }

    #[test]
    fn background_seeds_from_the_first_frame() {
        let mut b = BackgroundModel::new(4, 0.01, 1.0, 6.0, 300.0);
        let mut excess = vec![0.0; 4];
        b.update(&[-60.0, -60.0, -60.0, -60.0], &mut excess);
        assert!(
            excess.iter().all(|&e| e.abs() < 1e-6),
            "first frame has no excess"
        );
        assert!(b.level_db().iter().all(|&l| (l + 60.0).abs() < 1e-3));
    }

    #[test]
    fn a_rise_reads_as_excess_and_a_drop_does_not_linger() {
        // Replaces a test of the old asymmetric rise/fall rates. Those existed
        // to keep a mean tracking the floor; a median tracks the middle without
        // needing different speeds in each direction. What must still be true is
        // the observable behaviour on either side.
        let mut b = BackgroundModel::new(1, 0.1, 10.0, f32::INFINITY, 300.0);
        let mut excess = vec![0.0; 1];
        for _ in 0..60 {
            b.update(&[-60.0], &mut excess);
        }

        b.update(&[-40.0], &mut excess);
        assert!(
            excess[0] > 19.0,
            "a sudden rise should register as excess: {}",
            excess[0]
        );
        let after_rise = b.level_db()[0];
        assert!(
            after_rise < -59.0,
            "one loud frame must not move the background: {after_rise}"
        );

        // A sustained drop becomes the new middle rather than reading as a
        // permanent negative excess.
        for _ in 0..120 {
            b.update(&[-80.0], &mut excess);
        }
        assert!(
            excess[0].abs() < 0.6,
            "a settled quieter room is the new background: {}",
            excess[0]
        );
    }

    #[test]
    fn a_long_signal_does_not_get_adapted_away() {
        // The failure this model exists to prevent: a long signal must still
        // read as excess at the end, not just at the start.
        //
        // A median holds while the signal occupies *less than half the window*,
        // which is the honest statement of the guarantee. The Landscape
        // Signal's mountain lasts 80 s of its 109.5 s cycle, but it sweeps
        // across frequency, so no single bin carries it for anything like that
        // long — and it is bins, not the whole spectrum, that this models.
        let mut b = BackgroundModel::new(1, 1.0, 300.0, 6.0, 300.0);
        let mut excess = vec![0.0; 1];
        for _ in 0..200 {
            b.update(&[-70.0], &mut excess);
        }
        let mut last = 0.0;
        for _ in 0..80 {
            b.update(&[-50.0], &mut excess);
            last = excess[0];
        }
        assert!(
            last > 19.0,
            "after 80 s the signal should be undiminished, reads {last} dB above background"
        );
    }

    #[test]
    fn a_permanent_level_change_is_eventually_absorbed() {
        // A fan switching on is background, not a signal. Once it occupies more
        // than half the window it becomes the median, and stops being reported.
        //
        // The mean-based model needed a timed freeze to reach this state. The
        // median arrives at it on its own, which is why the timeout is gone.
        let mut b = BackgroundModel::new(1, 1.0, 60.0, 6.0, 300.0);
        let mut excess = vec![0.0; 1];
        for _ in 0..100 {
            b.update(&[-70.0], &mut excess);
        }
        assert!(excess[0].abs() < 0.6, "settled on the quiet level");

        for _ in 0..200 {
            b.update(&[-50.0], &mut excess);
        }
        assert!(
            excess[0].abs() < 1.0,
            "a permanent change must become the new background, reads {}",
            excess[0]
        );
    }

    #[test]
    fn the_background_recovers_after_a_signal_passes() {
        // The old model froze adaptation during a signal and had to release it
        // afterwards. The median has no state to release: once the signal is
        // out of the window, the quiet level is the middle again.
        let mut b = BackgroundModel::new(1, 1.0, 60.0, 6.0, 300.0);
        let mut excess = vec![0.0; 1];
        for _ in 0..100 {
            b.update(&[-70.0], &mut excess);
        }
        for _ in 0..20 {
            b.update(&[-50.0], &mut excess);
        }
        assert!(excess[0] > 19.0, "the signal reads while it is present");

        for _ in 0..100 {
            b.update(&[-70.0], &mut excess);
        }
        assert!(
            excess[0].abs() < 0.6,
            "and the background is back to quiet afterwards, reads {}",
            excess[0]
        );
        assert!(
            (b.level_db()[0] + 70.0).abs() < 0.6,
            "level should be the quiet floor, is {}",
            b.level_db()[0]
        );
    }

    #[test]
    fn warmup_suppresses_detection() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        assert!(!d.background().is_warm());
        let mut loud = flat_frame(-90.0);
        loud[100] = -20.0;
        // Even a huge excess produces nothing while the model is settling.
        for _ in 0..5 {
            assert!(d.push_frame(&loud, &flat_powers()).is_empty());
        }
        assert!(d.background().warmup_progress() < 1.0);
    }

    #[test]
    fn steady_background_produces_no_events() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);
        let quiet = flat_frame(-90.0);
        for _ in 0..200 {
            assert!(d.push_frame(&quiet, &flat_powers()).is_empty());
        }
        assert_eq!(d.open_event_count(), 0);
    }

    #[test]
    fn detects_a_sustained_tone_and_reports_its_frequency() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);

        let bin = 100; // 100 * 48000 / 1024 ≈ 4687 Hz
        let mut tone = flat_frame(-90.0);
        tone[bin] = -60.0; // 30 dB above the floor
        let mut powers = vec![1e-9; GEOM.bins()];
        powers[bin] = 1.0;

        for _ in 0..60 {
            d.push_frame(&tone, &powers);
        }
        let quiet = flat_frame(-90.0);
        let mut events = Vec::new();
        for _ in 0..40 {
            events.extend(d.push_frame(&quiet, &flat_powers()));
        }

        assert_eq!(
            events.len(),
            1,
            "expected exactly one event, got {events:?}"
        );
        let e = &events[0];
        let expected_hz = GEOM.bin_hz(bin);
        assert!(
            (e.peak_hz - expected_hz).abs() < 100.0,
            "peak at {} Hz, expected {expected_hz}",
            e.peak_hz
        );
        assert!(e.peak_excess_db > 20.0, "excess {}", e.peak_excess_db);
        assert!(e.duration_seconds > 0.5, "duration {}", e.duration_seconds);
        assert!(
            e.score > 0.4,
            "a strong narrowband tone should score well: {}",
            e.score
        );
    }

    #[test]
    fn ignores_muted_bands() {
        let mut c = cfg();
        let bin = 100;
        let hz = GEOM.bin_hz(bin);
        c.ignore_bands.push(IgnoreBand {
            low_hz: hz - 200.0,
            high_hz: hz + 200.0,
        });

        let mut d = NoveltyDetector::new(GEOM, &c);
        warm_up(&mut d);

        let mut tone = flat_frame(-90.0);
        tone[bin] = -20.0;
        let mut all = Vec::new();
        for _ in 0..100 {
            all.extend(d.push_frame(&tone, &flat_powers()));
        }
        all.extend(d.flush());
        assert!(all.is_empty(), "muted band still fired: {all:?}");
    }

    #[test]
    fn brief_blips_are_below_the_minimum_duration() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);

        let mut tone = flat_frame(-90.0);
        tone[200] = -30.0;
        // min_event_seconds is 0.2 s ≈ 19 frames; fire for 3.
        for _ in 0..3 {
            d.push_frame(&tone, &flat_powers());
        }
        let quiet = flat_frame(-90.0);
        let mut events = Vec::new();
        for _ in 0..40 {
            events.extend(d.push_frame(&quiet, &flat_powers()));
        }
        events.extend(d.flush());
        assert!(
            events.is_empty(),
            "a 3-frame blip should not qualify: {events:?}"
        );
    }

    #[test]
    fn a_short_dropout_does_not_split_one_event_in_two() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);

        let mut tone = flat_frame(-90.0);
        tone[150] = -50.0;
        let quiet = flat_frame(-90.0);

        let mut events = Vec::new();
        for _ in 0..40 {
            events.extend(d.push_frame(&tone, &flat_powers()));
        }
        // Gap tolerance is 0.1 s ≈ 9 frames.
        for _ in 0..4 {
            events.extend(d.push_frame(&quiet, &flat_powers()));
        }
        for _ in 0..40 {
            events.extend(d.push_frame(&tone, &flat_powers()));
        }
        assert!(events.is_empty(), "nothing should have closed yet");
        for _ in 0..40 {
            events.extend(d.push_frame(&quiet, &flat_powers()));
        }
        assert_eq!(
            events.len(),
            1,
            "the dropout should not have split it: {events:?}"
        );
    }

    #[test]
    fn tracks_a_sweep_as_one_event_with_drift() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);

        let mut events = Vec::new();
        for step in 0..60 {
            let mut frame = flat_frame(-90.0);
            let bin = 50 + step; // one bin per frame
            frame[bin] = -50.0;
            events.extend(d.push_frame(&frame, &flat_powers()));
        }
        let quiet = flat_frame(-90.0);
        for _ in 0..40 {
            events.extend(d.push_frame(&quiet, &flat_powers()));
        }

        assert_eq!(events.len(), 1, "a sweep should stay one event: {events:?}");
        let drift = events[0].drift_hz;
        // 59 bins of movement at ~46.9 Hz per bin.
        assert!(drift > 2000.0, "expected a large upward drift, got {drift}");
    }

    #[test]
    fn reset_events_drops_work_in_progress() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);
        let mut tone = flat_frame(-90.0);
        tone[300] = -40.0;
        for _ in 0..50 {
            d.push_frame(&tone, &flat_powers());
        }
        assert!(d.open_event_count() > 0);
        d.reset_events();
        assert_eq!(d.open_event_count(), 0);
        assert!(d.flush().is_empty());
    }

    #[test]
    fn scoring_prefers_structure_over_loudness() {
        let nyquist = GEOM.nyquist_hz();
        let tonal = DetectionEvent {
            start_frame: 0,
            end_frame: 100,
            start_seconds: 0.0,
            duration_seconds: 10.0,
            low_hz: 1000.0,
            high_hz: 1100.0,
            peak_hz: 1050.0,
            low_bin: 21,
            high_bin: 23,
            peak_excess_db: 20.0,
            mean_excess_db: 15.0,
            drift_hz: 0.0,
            mean_flatness: 0.02,
            score: 0.0,
        };
        let broadband = DetectionEvent {
            low_hz: 0.0,
            high_hz: nyquist,
            mean_flatness: 0.95,
            peak_excess_db: 30.0,
            ..tonal.clone()
        };
        let a = score_event(&tonal, nyquist);
        let b = score_event(&broadband, nyquist);
        assert!(a > b, "tonal {a} should outrank louder broadband {b}");
        assert!((0.0..=1.0).contains(&a) && (0.0..=1.0).contains(&b));
    }

    #[test]
    fn scores_stay_in_range_for_extreme_inputs() {
        let e = DetectionEvent {
            start_frame: 0,
            end_frame: 0,
            start_seconds: 0.0,
            duration_seconds: 1e6,
            low_hz: 0.0,
            high_hz: 0.0,
            peak_hz: 0.0,
            low_bin: 0,
            high_bin: 0,
            peak_excess_db: 1e6,
            mean_excess_db: 0.0,
            drift_hz: -1e9,
            mean_flatness: -5.0,
            score: 0.0,
        };
        let s = score_event(&e, GEOM.nyquist_hz());
        assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
        assert!(score_event(&e, 0.0).is_finite());
    }

    #[test]
    fn non_finite_frame_values_do_not_poison_the_model() {
        let mut d = NoveltyDetector::new(GEOM, &cfg());
        warm_up(&mut d);
        let mut frame = flat_frame(-90.0);
        frame[10] = f32::NAN;
        frame[11] = f32::NEG_INFINITY;
        let events = d.push_frame(&frame, &flat_powers());
        assert!(events.is_empty());
        assert!(d.background().level_db().iter().all(|v| v.is_finite()));
    }
}

//! The analysis engine: interleaved audio in, snapshots and detections out.
//!
//! This is the whole chain in one place — ring, per-channel STFT, novelty
//! detection, direction finding, and the two spectrogram tiers — deliberately
//! free of any Windows or UI dependency so it can be driven equally well by
//! WASAPI, by a synthetic source, or by a WAV file.
//!
//! It owns no threads. The caller decides when to push audio and when to take a
//! snapshot, which keeps the real-time capture path and the display rate
//! genuinely independent.

use realfft::num_complex::Complex32;

use crate::analysis::direction::{self, DirectionEstimate};
use crate::analysis::keying::{KeyingDetection, KeyingDetector};
use crate::analysis::kurtosis::{self, SpectralKurtosis};
use crate::analysis::morse::{MorseDetection, MorseDetector};
use crate::analysis::novelty::{DetectionEvent, FrameGeometry, NoveltyDetector};
use crate::analysis::periodicity::{self, PeriodicityResult};
use crate::analysis::spectrogram::{DbRange, LongTermSummarizer, SpectrogramHistory};
use crate::analysis::statistics::{HealthWindow, SignalStats, power_to_dbfs};
use crate::analysis::stft::StftStream;
use crate::analysis::structure::{StructureScanner, StructureScore};
use crate::audio::format;
use crate::audio::{PcmRing, StreamFormat};
use crate::config::Config;

/// Per-block decay applied to the peak hold. At the ~20 blocks/s a shared-mode
/// endpoint delivers, this fades to 1/e in roughly five seconds.
const PEAK_DECAY_PER_BLOCK: f32 = 0.99;

/// How far a candidate tone must stand above its neighbouring bins.
///
/// A transmitted tone is a narrow spike; low-frequency rumble is a broad hill
/// whose peak happens to be somewhere on top of it.
const PROMINENCE_MIN_RATIO: f64 = 12.0;

/// Frequency rows in the structure-scan image, log-spaced.
const SCAN_ROWS: usize = 256;

/// Time columns in the structure-scan image.
const SCAN_COLUMNS: usize = 512;

/// Half-open bin ranges for log-spaced scan rows, lowest frequency last so the
/// image reads the same way up as a spectrogram.
///
/// The band is the *detection* band, not the display band: the scanner should
/// concentrate where signals live regardless of how wide a view is on screen.
fn log_scan_rows(
    bins: usize,
    sample_rate: u32,
    rows: usize,
    min_hz: f32,
    max_hz: f32,
) -> Vec<(usize, usize)> {
    if bins < 2 || rows < 2 {
        return Vec::new();
    }
    let nyquist = sample_rate as f32 / 2.0;
    let bin_hz = nyquist / (bins - 1) as f32;
    let top = max_hz.clamp(2.0, nyquist);
    let bottom = min_hz.clamp(1.0, top * 0.5);
    let ratio = (top / bottom).powf(1.0 / rows as f32);

    (0..rows)
        .map(|row| {
            // Row 0 is the top of the image, so the highest frequency.
            let upper = rows - row;
            let hi_hz = bottom * ratio.powi(upper as i32);
            let lo_hz = bottom * ratio.powi(upper as i32 - 1);
            let lo = (lo_hz / bin_hz).floor().max(0.0) as usize;
            let hi = (hi_hz / bin_hz).ceil() as usize;
            let lo = lo.min(bins - 1);
            (lo, hi.clamp(lo + 1, bins))
        })
        .collect()
}

/// A detection with everything the writer and the UI need attached.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub event: DetectionEvent,
    pub direction: DirectionEstimate,
    /// Absolute frame on the capture timeline where the event began.
    pub start_sample: u64,
    pub end_sample: u64,
    /// True if a timeline gap fell inside the event, making its structure
    /// unreliable.
    pub spans_gap: bool,
}

/// Everything the UI reads. Cheap to clone; produced at `analysis_update_hz`.
#[derive(Debug, Clone)]
pub struct AnalysisSnapshot {
    pub format: StreamFormat,
    pub stats: SignalStats,
    pub histogram: Vec<u64>,
    /// Most recent mono spectrum, dBFS per bin.
    pub spectrum_db: Vec<f32>,
    pub background_db: Vec<f32>,
    pub excess_db: Vec<f32>,
    pub direction: DirectionEstimate,
    pub periodicity: Option<PeriodicityResult>,
    /// Seconds of audio processed, including synthesized gap fill.
    pub timeline_seconds: f64,
    pub frames_analyzed: u64,
    pub gap_count: u64,
    pub gap_seconds: f64,
    pub overrun_count: u64,
    pub warmup_progress: f32,
    pub open_events: usize,
    pub is_silent: bool,

    // ---- primary detectors ----
    /// Binary keying, when enough symbols have been seen to judge.
    pub keying: Option<KeyingDetection>,
    /// A single low tone switched on and off — Thargoid Sensor Morse.
    pub morse: Option<MorseDetection>,
    /// Best drawn-structure score across the waterfall, with the tile it came
    /// from as `(x, y)` in spectrogram pixels.
    pub structure: StructureScore,
    pub structure_tile: (usize, usize),
    /// Bins whose power distribution is not Gaussian, and the strongest such
    /// departure in sigmas. Diagnostics for the spectral-kurtosis experiment.
    pub kurtosis_hot_bins: usize,
    pub kurtosis_peak: f32,
}

pub struct AnalysisEngine {
    cfg: Config,
    format: StreamFormat,
    geometry: FrameGeometry,

    ring: PcmRing,
    channel_streams: Vec<StftStream>,
    spectra: Vec<Vec<Complex32>>,
    channel_powers: Vec<Vec<f32>>,
    mono_powers: Vec<f32>,
    mono_db: Vec<f32>,

    detector: NoveltyDetector,
    waterfall: SpectrogramHistory,
    /// The same spectrogram with each bin's learned background subtracted.
    ///
    /// This is what a faint signal actually looks for. Raw level is dominated by
    /// whatever is constantly loud — ship rumble, life support — which both
    /// hides the signal and swallows the display's dynamic range. Subtracting
    /// the background leaves only what changed.
    excess_waterfall: SpectrogramHistory,
    longterm: SpectrogramHistory,
    summarizer: LongTermSummarizer,
    /// Rolling health statistics, accumulated as audio arrives rather than by
    /// rescanning the ring.
    health: HealthWindow,
    /// The rate the long-term tier actually emits at. Integer decimation of the
    /// STFT frame rate rarely lands exactly on `cfg.longterm_fps`, and using the
    /// configured value instead of the real one scales every period reading.
    longterm_fps: f32,
    /// Whether per-channel analysis is running. When off the engine keeps one
    /// transform and a mono ring, which is roughly an eightfold saving on a
    /// 7.1 endpoint.
    direction_finding: bool,
    /// Channels the ring actually stores: all of them, or one.
    ring_channels: usize,
    /// Scratch mono downmix, reused every block.
    mono: Vec<f32>,
    /// Scratch copy of the per-bin excess, reused every frame.
    excess_scratch: Vec<f32>,

    /// Primary detector: binary keying. Fed one dominant-bin index per frame.
    keying: Option<KeyingDetector>,
    morse: Option<MorseDetector>,
    /// Primary detector: drawn structure in the spectrogram.
    structure_scanner: Option<StructureScanner>,
    structure: StructureScore,
    structure_tile: (usize, usize),
    kurtosis: SpectralKurtosis,
    /// Strongest non-Gaussian departure seen at any point, and how many bins
    /// were beyond three sigma when it happened. The final frame is a poor
    /// measurement: a run ending in a silent stretch reports nothing at all.
    peak_kurtosis: f32,
    peak_kurtosis_bins: usize,
    /// Best structure score seen, and when.
    ///
    /// The live score describes only the last few seconds. For a recording — or
    /// a long session — the question is whether anything appeared *at any
    /// point*, and reporting only the final instant answers a different
    /// question. A file ending in silence scores zero however much it contained.
    peak_structure: StructureScore,
    peak_structure_at: f64,
    peak_keying: f32,
    peak_keying_at: f64,
    /// Frames until the next structure scan. Sweeping the waterfall every frame
    /// would be wasteful when the picture takes seconds to accumulate.
    structure_countdown: usize,
    structure_interval: usize,
    /// Reused scan image: log-spaced frequency rows by time columns.
    scan_image: Vec<u8>,
    /// Precomputed bin range feeding each scan row.
    scan_rows: Vec<(usize, usize)>,

    /// Per-channel power accumulated over bins currently above threshold. This
    /// is what a closing event's bearing is computed from.
    event_powers: Vec<f64>,
    event_cross: Complex32,
    event_frames: usize,
    /// Live bearing, updated every frame that has any activity.
    /// Bearing of the material that triggered the current event. This is what a
    /// detection records, so it must stay scoped to the event.
    event_direction: DirectionEstimate,
    /// Bearing of whatever is loudest right now, updated every frame regardless
    /// of whether anything is above the detection threshold. This is what the
    /// compass and the overlay's rose show.
    live_direction: DirectionEstimate,
    /// Smoothed per-channel power and L/R cross-spectrum feeding the live
    /// bearing. Smoothed as *powers*, not as an angle: angles wrap, and
    /// averaging across the wrap point produces a bearing pointing at nothing.
    live_powers: Vec<f64>,
    live_cross: Complex32,

    deinterleaved: Vec<Vec<f32>>,
    frames_analyzed: u64,
    gap_count: u64,
    gap_frames_total: u64,
    overrun_count: u64,
    /// Sample index of the most recent gap, so events overlapping it are marked.
    last_gap_end_sample: u64,
    /// Decaying peak hold, so "is there any signal at all" is answerable without
    /// rescanning the whole ring on every UI frame.
    recent_peak: f32,
}

impl AnalysisEngine {
    pub fn new(cfg: Config, format: StreamFormat) -> Self {
        let channels = format.channels;
        let direction_finding = cfg.direction_finding;
        // With direction finding off, one transform replaces one-per-channel and
        // the ring holds a mono downmix.
        let streams = if direction_finding { channels } else { 1 };
        let ring_channels = if direction_finding { channels } else { 1 };
        let geometry = FrameGeometry {
            sample_rate: format.sample_rate,
            fft_size: cfg.fft_size,
            hop: cfg.hop,
        };
        let bins = geometry.bins();

        let frames_per_second = 1.0 / geometry.frame_seconds();
        let waterfall_frames = (cfg.waterfall_seconds * frames_per_second).ceil().max(1.0) as usize;
        // One hour of the long-term tier, which costs under a megabyte.
        let longterm_frames = (3600.0 * cfg.longterm_fps).ceil().max(1.0) as usize;
        let frames_per_summary = (frames_per_second / cfg.longterm_fps).round().max(1.0) as usize;

        Self {
            ring: PcmRing::with_seconds(cfg.pcm_ring_seconds, format.sample_rate, ring_channels),
            channel_streams: (0..streams)
                .map(|_| StftStream::new(cfg.fft_size, cfg.hop))
                .collect(),
            spectra: vec![vec![Complex32::new(0.0, 0.0); bins]; streams],
            channel_powers: vec![vec![0.0; bins]; streams],
            mono_powers: vec![0.0; bins],
            mono_db: vec![0.0; bins],
            detector: NoveltyDetector::new(geometry, &cfg),
            waterfall: SpectrogramHistory::new(bins, waterfall_frames, DbRange::default()),
            excess_waterfall: SpectrogramHistory::new(
                bins,
                waterfall_frames,
                // Excess is a small positive range, not an absolute level.
                DbRange {
                    min: -3.0,
                    max: 30.0,
                },
            ),
            longterm: SpectrogramHistory::new(
                cfg.longterm_bands,
                longterm_frames,
                DbRange::default(),
            ),
            summarizer: LongTermSummarizer::new(
                cfg.longterm_bands,
                bins,
                format.sample_rate,
                20.0,
                frames_per_summary,
            ),
            longterm_fps: frames_per_second / frames_per_summary as f32,
            health: HealthWindow::new(
                cfg.health_window_seconds,
                format.sample_rate,
                cfg.histogram_bins,
            ),
            direction_finding,
            ring_channels,
            mono: Vec::new(),
            excess_scratch: Vec::new(),
            keying: cfg.detect_keying.then(|| {
                KeyingDetector::new(geometry.frame_seconds(), format.sample_rate, cfg.fft_size)
            }),
            morse: cfg.detect_morse.then(|| {
                MorseDetector::new(
                    geometry.frame_seconds(),
                    format.sample_rate,
                    cfg.fft_size,
                    cfg.morse_min_hz,
                    cfg.morse_max_hz,
                )
            }),
            structure_scanner: cfg.detect_structure.then(StructureScanner::default),
            structure: StructureScore::empty(),
            structure_tile: (0, 0),
            kurtosis: SpectralKurtosis::new(bins, kurtosis::WINDOW_FRAMES),
            peak_kurtosis: 0.0,
            peak_kurtosis_bins: 0,
            peak_structure: StructureScore::empty(),
            peak_structure_at: 0.0,
            peak_keying: 0.0,
            peak_keying_at: 0.0,
            structure_countdown: 0,
            // Every couple of seconds; the drawing is not going anywhere.
            structure_interval: (frames_per_second * 2.0).round().max(1.0) as usize,
            scan_image: Vec::new(),
            scan_rows: log_scan_rows(
                bins,
                format.sample_rate,
                SCAN_ROWS,
                cfg.detect_min_hz,
                cfg.detect_max_hz,
            ),
            event_powers: vec![0.0; streams],
            event_cross: Complex32::new(0.0, 0.0),
            event_frames: 0,
            event_direction: DirectionEstimate::insufficient(),
            live_direction: DirectionEstimate::insufficient(),
            live_powers: vec![0.0; streams],
            live_cross: Complex32::new(0.0, 0.0),
            deinterleaved: vec![Vec::new(); if direction_finding { channels } else { 0 }],
            frames_analyzed: 0,
            gap_count: 0,
            gap_frames_total: 0,
            overrun_count: 0,
            last_gap_end_sample: 0,
            recent_peak: 0.0,
            geometry,
            format,
            cfg,
        }
    }

    pub fn format(&self) -> &StreamFormat {
        &self.format
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn geometry(&self) -> FrameGeometry {
        self.geometry
    }

    pub fn ring(&self) -> &PcmRing {
        &self.ring
    }

    /// Shape of the audio actually held in the ring, which is what a triggered
    /// capture writes. Mono unless direction finding is on.
    pub fn ring_format(&self) -> StreamFormat {
        StreamFormat::new(
            self.format.sample_rate,
            self.ring_channels,
            if self.ring_channels == self.format.channels {
                self.format.channel_mask
            } else {
                0
            },
            self.format.sample_format,
        )
    }

    pub fn direction_finding(&self) -> bool {
        self.direction_finding
    }

    pub fn waterfall(&self) -> &SpectrogramHistory {
        &self.waterfall
    }

    /// The background-subtracted spectrogram: only what changed.
    pub fn excess_waterfall(&self) -> &SpectrogramHistory {
        &self.excess_waterfall
    }

    pub fn long_term(&self) -> &SpectrogramHistory {
        &self.longterm
    }

    pub fn summarizer(&self) -> &LongTermSummarizer {
        &self.summarizer
    }

    /// How far the background model has settled, 0..1.
    pub fn warmup_progress(&self) -> f32 {
        self.detector.background().warmup_progress()
    }

    /// Whether the stream currently holds essentially nothing. Cheap: reads the
    /// decaying peak hold rather than rescanning the ring.
    pub fn is_silent(&self) -> bool {
        self.recent_peak <= crate::analysis::statistics::SILENCE_FLOOR
    }

    /// Replace the muted frequency ranges without disturbing the background
    /// estimate — muting a band mid-session must not restart the warm-up.
    pub fn set_ignore_bands(&mut self, bands: &[crate::config::IgnoreBand]) {
        self.detector.set_ignore_bands(bands);
    }

    /// Turn the primary detectors on or off without disturbing the background
    /// model, so toggling does not restart the warm-up.
    pub fn set_detectors(&mut self, keying: bool, structure: bool) {
        match (keying, self.keying.is_some()) {
            (true, false) => {
                self.keying = Some(KeyingDetector::new(
                    self.geometry.frame_seconds(),
                    self.format.sample_rate,
                    self.cfg.fft_size,
                ));
            }
            (false, true) => self.keying = None,
            _ => {}
        }
        match (structure, self.structure_scanner.is_some()) {
            (true, false) => self.structure_scanner = Some(StructureScanner::default()),
            (false, true) => {
                self.structure_scanner = None;
                self.structure = StructureScore::empty();
            }
            _ => {}
        }
    }

    pub fn note_overrun(&mut self) {
        self.overrun_count += 1;
    }

    /// Feed interleaved audio. Returns any detections that completed.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Vec<Detection> {
        if samples.is_empty() {
            return Vec::new();
        }
        let block_peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        // Peak hold with a ~5 s decay, the same behaviour as a level meter.
        self.recent_peak = (self.recent_peak * PEAK_DECAY_PER_BLOCK).max(block_peak);

        // A single downmix pass feeds the health window and, unless direction
        // finding is on, the transform and the ring as well.
        self.mono.clear();
        format::downmix_mono(samples, self.format.channels, &mut self.mono);
        self.health.push(self.mono.iter().copied());

        if self.direction_finding {
            self.ring.push_interleaved(samples);
            for buf in self.deinterleaved.iter_mut() {
                buf.clear();
            }
            format::deinterleave(samples, &mut self.deinterleaved);
            for (stream, buf) in self
                .channel_streams
                .iter_mut()
                .zip(self.deinterleaved.iter())
            {
                stream.push(buf);
            }
        } else {
            self.ring.push_interleaved(&self.mono);
            self.channel_streams[0].push(&self.mono);
        }
        self.drain_frames()
    }

    /// Record a timeline gap: the endpoint went away, or loopback went idle.
    /// Silence is inserted so the clock stays honest, and in-flight events are
    /// abandoned because their continuity across the gap would be a fiction.
    pub fn push_gap(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        self.gap_count += 1;
        self.gap_frames_total += frames as u64;
        self.ring.push_silence(frames);
        self.last_gap_end_sample = self.ring.total_frames();

        for stream in self.channel_streams.iter_mut() {
            stream.discard_partial();
        }
        self.detector.reset_events();
        self.summarizer.reset();
        if let Some(k) = self.keying.as_mut() {
            // Symbol timing across a gap would be fiction.
            k.reset();
        }
        self.reset_event_accumulator();

        // The silence still has to travel through the analysis chain, or the
        // spectrogram would splice the two sides of the gap together.
        let silence = vec![0.0f32; frames];
        for stream in self.channel_streams.iter_mut() {
            stream.push(&silence);
        }
        let _ = self.drain_frames();
    }

    fn reset_event_accumulator(&mut self) {
        self.event_powers.fill(0.0);
        self.event_cross = Complex32::new(0.0, 0.0);
        self.event_frames = 0;
    }

    /// Transform and analyze every complete frame now available.
    fn drain_frames(&mut self) -> Vec<Detection> {
        let streams = self.channel_streams.len();
        let mut detections = Vec::new();

        loop {
            // Every channel advances in lockstep; if the first has no frame
            // ready, none do.
            let mut produced = false;
            for c in 0..streams {
                if self.channel_streams[c]
                    .next_frame(&mut self.spectra[c])
                    .is_some()
                {
                    produced = true;
                } else if produced {
                    // Should be impossible: channels are fed identical counts.
                    debug_assert!(false, "channel streams fell out of lockstep");
                }
            }
            if !produced {
                break;
            }

            for c in 0..streams {
                let (spectrum, powers) = (&self.spectra[c], &mut self.channel_powers[c]);
                self.channel_streams[c].stft().powers(spectrum, powers);
            }

            // Mono view for detection. With one stream this is a straight copy.
            let inv = 1.0 / streams as f32;
            for bin in 0..self.mono_powers.len() {
                let sum: f32 = (0..streams).map(|c| self.channel_powers[c][bin]).sum();
                self.mono_powers[bin] = sum * inv;
                self.mono_db[bin] = power_to_dbfs(self.mono_powers[bin]);
            }

            let closed = {
                let mono_db = std::mem::take(&mut self.mono_db);
                let events = self.detector.push_frame(&mono_db, &self.mono_powers);
                self.mono_db = mono_db;
                events
            };

            self.kurtosis.update(&self.mono_powers);
            if self.kurtosis.ready() {
                let sigma = self.kurtosis.sigma();
                let peak = sigma.iter().fold(0.0f32, |a, b| a.max(b.abs()));
                if peak > self.peak_kurtosis {
                    self.peak_kurtosis = peak;
                    self.peak_kurtosis_bins = sigma.iter().filter(|s| s.abs() >= 3.0).count();
                }
            }
            self.waterfall.push_db(&self.mono_db);
            {
                // The detector has already computed this for the current frame.
                let excess = std::mem::take(&mut self.excess_scratch);
                let mut excess = excess;
                excess.clear();
                excess.extend_from_slice(self.detector.excess_db());
                self.excess_waterfall.push_db(&excess);
                self.excess_scratch = excess;
            }
            self.update_primary_detectors();
            if let Some(summary) = self.summarizer.push(&self.mono_db) {
                self.longterm.push_db(&summary);
            }
            if self.direction_finding {
                self.accumulate_direction();
                self.update_live_direction();
            }
            self.frames_analyzed += 1;

            for event in closed {
                detections.push(self.finish_detection(event));
            }
            if detections.len() > 1 || self.detector.open_event_count() == 0 {
                self.reset_event_accumulator();
            }
        }
        detections
    }

    /// Feed the two primary detectors.
    ///
    /// Both ride on work already done: the keying detector takes one `argmax`
    /// over the spectrum just transformed, and the structure scanner reads the
    /// quantized waterfall already maintained for the display.
    fn update_primary_detectors(&mut self) {
        // Morse reads the spectrum directly rather than the excess: its tone
        // sits below `detect_min_hz`, where the background model is not applied,
        // and the detector learns its own floor for exactly that band.
        if let Some(morse) = self.morse.as_mut() {
            morse.push(&self.mono_db);
        }

        if self.keying.is_some() {
            // Decide first, borrowing only immutably, then hand the verdict to
            // the detector — it needs `&mut self` and the tests above need `&self`.
            let excess = self.detector.excess_db();
            let threshold = self.cfg.novelty_threshold_db;

            // Dominant bin among those standing above the background.
            let mut peak_bin = 0usize;
            let mut peak = f32::NEG_INFINITY;
            let mut above_background = false;
            for (bin, &value) in excess.iter().enumerate() {
                if value.is_finite() && value >= threshold && value > peak {
                    peak = value;
                    peak_bin = bin;
                    above_background = true;
                }
            }

            // Standing above the background is not enough — three further tests
            // each kill a false positive seen in the field.
            let active = above_background
                // 1. Frequency floor. Ship and drive rumble lives below a few
                //    hundred hertz and its peak wanders between adjacent low
                //    bins, which reads as keying. Real transmitted tones sit far
                //    higher: the Thargoid tightbeam keys around 1200/2400 Hz.
                //    Measured in flight: "tones" of 117, 152 and 293 Hz at 0.51
                //    confidence, entirely from cockpit rumble.
                && self.geometry.bin_hz(peak_bin) >= self.cfg.keying_min_hz
                // 2. Local prominence. Comparing the peak against the mean of the
                //    whole spectrum is dominated by empty high bins, so any
                //    low-frequency hump looks tonal. Compare it against its own
                //    neighbourhood instead: a tone is a narrow spike, a rumble is
                //    a broad hill.
                && self.local_prominence(peak_bin) >= PROMINENCE_MIN_RATIO;

            if let Some(keying) = self.keying.as_mut() {
                keying.push(peak_bin, active);
            }
        }

        if let Some(scanner) = self.structure_scanner.as_ref() {
            if self.structure_countdown > 0 {
                self.structure_countdown -= 1;
                return;
            }
            self.structure_countdown = self.structure_interval;

            let frames = self.waterfall.len();
            if frames < 16 || self.scan_rows.len() < 16 {
                return;
            }

            // Build the scan image at a fixed, modest size rather than sweeping
            // two thousand raw bins: the structure metrics do not need that
            // resolution, and the published decodes are all read log-scaled at a
            // few hundred rows anyway. Pooling is by maximum, so a one-pixel
            // stroke survives being squeezed — averaging would erase it.
            let rows = self.scan_rows.len();
            let span = frames.min(SCAN_COLUMNS);
            let start = frames - span;
            let frames_per_column = (span as f32 / SCAN_COLUMNS as f32).max(1.0);
            let columns = (span as f32 / frames_per_column).ceil() as usize;

            self.scan_image.clear();
            self.scan_image.resize(rows * columns, 0);
            for col in 0..columns {
                let from = start + (col as f32 * frames_per_column) as usize;
                let to = (start + ((col + 1) as f32 * frames_per_column) as usize).min(frames);
                for frame_index in from..to.max(from + 1) {
                    let Some(frame) = self.waterfall.frame_at(frame_index) else {
                        continue;
                    };
                    for (row, &(lo, hi)) in self.scan_rows.iter().enumerate() {
                        let mut peak = 0u8;
                        for q in &frame[lo..hi.min(frame.len())] {
                            peak = peak.max(*q);
                        }
                        let cell = &mut self.scan_image[row * columns + col];
                        *cell = (*cell).max(peak);
                    }
                }
            }

            // Strip the two things ambience is made of before looking for a
            // drawing: sustained tones and transients. What survives is neither,
            // which is where a drawn stroke lives.
            let cleaned = crate::analysis::structure::suppress_tones_and_transients(
                &self.scan_image,
                columns,
                rows,
            );
            let (score, x, y) = scanner.scan(&cleaned, columns, rows);
            // Integrating along candidate lines reaches strokes too faint to
            // become ink at all, which the tile sweep above cannot see.
            let (drift, drift_angle, drift_lines) =
                crate::analysis::structure::drift_scan(&cleaned, columns, rows);
            let score = score.with_drift(drift, drift_angle, drift_lines);
            if score.score > self.peak_structure.score {
                self.peak_structure = score.clone();
                self.peak_structure_at = self.elapsed_seconds();
            }
            self.structure = score;
            self.structure_tile = (x, y);
        }
    }

    /// How far a bin stands above its own neighbourhood.
    ///
    /// The neighbourhood excludes the bins immediately either side of the peak,
    /// which belong to the same spectral leakage skirt and would otherwise mask
    /// exactly what is being measured.
    fn local_prominence(&self, bin: usize) -> f64 {
        const SKIRT: usize = 3;
        const WINDOW: usize = 40;
        let bins = self.mono_powers.len();
        if bins == 0 {
            return 0.0;
        }
        let low = bin.saturating_sub(WINDOW);
        let high = (bin + WINDOW + 1).min(bins);

        let mut sum = 0.0f64;
        let mut count = 0usize;
        for (i, p) in self.mono_powers[low..high].iter().enumerate() {
            let index = low + i;
            if index.abs_diff(bin) <= SKIRT {
                continue;
            }
            sum += (*p as f64).max(0.0);
            count += 1;
        }
        if count == 0 || sum <= 0.0 {
            // Nothing to compare against; treat as prominent so a genuinely
            // isolated tone is not discarded.
            return f64::INFINITY;
        }
        self.mono_powers[bin] as f64 / (sum / count as f64)
    }

    /// Seconds of audio processed so far.
    fn elapsed_seconds(&self) -> f64 {
        self.format.frames_to_seconds(self.ring.total_frames())
    }

    /// Best structure score seen during the run, and when it occurred.
    pub fn peak_structure(&self) -> (&StructureScore, f64) {
        (&self.peak_structure, self.peak_structure_at)
    }

    /// Best keying confidence seen during the run, and when.
    pub fn peak_keying(&self) -> (f32, f64) {
        (self.peak_keying, self.peak_keying_at)
    }

    /// The most recent keying assessment, if there is enough evidence.
    pub fn keying(&self) -> Option<KeyingDetection> {
        self.keying.as_ref().and_then(|k| k.evaluate())
    }

    /// The most recent Morse assessment, if there is enough evidence.
    pub fn morse(&self) -> Option<MorseDetection> {
        self.morse.as_ref().and_then(|m| m.evaluate())
    }

    /// The most recent drawn-structure score.
    pub fn structure(&self) -> &StructureScore {
        &self.structure
    }

    /// Track the bearing of whatever is playing, detection or not.
    ///
    /// [`Self::accumulate_direction`] only looks at bins that clear
    /// `novelty_threshold_db`, because a *detection* must be attributed to the
    /// material that caused it. Nothing clears that bar during ordinary
    /// listening, so a compass fed from it sits dead — which is exactly how it
    /// looked in the cockpit, indistinguishable from a broken instrument.
    ///
    /// This one takes the whole detection band every frame, and smooths the
    /// powers rather than the angle: bearings wrap at ±180°, and averaging
    /// across the wrap point yields a needle pointing at nothing.
    fn update_live_direction(&mut self) {
        let channels = self.format.channels;
        if channels < 2 {
            return;
        }
        let bins = self.geometry.bins();
        let lo = self.cfg.detect_min_hz;
        let hi = self.cfg.detect_max_hz;

        // One frame's worth of energy across the band we care about.
        let mut powers = vec![0.0f64; channels];
        let mut cross = Complex32::new(0.0, 0.0);
        for bin in 0..bins {
            let hz = self.geometry.bin_hz(bin);
            if hz < lo || hz > hi {
                continue;
            }
            for (c, power) in powers.iter_mut().enumerate() {
                *power += self.channel_powers[c][bin] as f64;
            }
            cross += self.spectra[0][bin] * self.spectra[1][bin].conj();
        }

        // A second or so of memory, so the needle settles instead of twitching
        // at the frame rate.
        const SMOOTHING: f64 = 0.05;
        for (live, now) in self.live_powers.iter_mut().zip(&powers) {
            *live += (now - *live) * SMOOTHING;
        }
        let a = SMOOTHING as f32;
        self.live_cross = self.live_cross * (1.0 - a) + cross * a;

        let smoothed: Vec<f32> = self.live_powers.iter().map(|p| *p as f32).collect();
        let layout = self.format.layout();
        self.live_direction = direction::estimate(&smoothed, &layout, Some(self.live_cross));
    }

    /// Accumulate per-channel power over the bins currently above threshold.
    fn accumulate_direction(&mut self) {
        let threshold = self.cfg.novelty_threshold_db;
        let excess = self.detector.excess_db();
        let mut any = false;
        let channels = self.format.channels;

        // Indexing rather than zipping: `bin` addresses three separate arrays
        // (excess, per-channel powers, per-channel spectra) whose rows are
        // themselves indexed by channel.
        for (bin, &value) in excess.iter().enumerate() {
            if !value.is_finite() || value < threshold {
                continue;
            }
            any = true;
            for c in 0..channels {
                self.event_powers[c] += self.channel_powers[c][bin] as f64;
            }
            if channels >= 2 {
                self.event_cross += self.spectra[0][bin] * self.spectra[1][bin].conj();
            }
        }

        if any {
            self.event_frames += 1;
            let powers: Vec<f32> = self.event_powers.iter().map(|p| *p as f32).collect();
            let layout = self.format.layout();
            let cross = if channels >= 2 {
                Some(self.event_cross)
            } else {
                None
            };
            self.event_direction = direction::estimate(&powers, &layout, cross);
        }
    }

    fn finish_detection(&self, event: DetectionEvent) -> Detection {
        let hop = self.cfg.hop as u64;
        let start_sample = event.start_frame * hop;
        let end_sample = event.end_frame * hop + self.cfg.fft_size as u64;
        Detection {
            direction: self.event_direction,
            spans_gap: self.last_gap_end_sample > start_sample
                && self.last_gap_end_sample <= end_sample,
            start_sample,
            end_sample,
            event,
        }
    }

    /// Close any events still open — end of stream, or before a device change.
    pub fn flush(&mut self) -> Vec<Detection> {
        self.detector
            .flush()
            .into_iter()
            .map(|e| self.finish_detection(e))
            .collect()
    }

    /// The long-term tier's true frame rate, after integer decimation.
    pub fn long_term_fps(&self) -> f32 {
        self.longterm_fps
    }

    /// Current best estimate of a repeating period, from the long-term tier.
    pub fn periodicity(&self) -> Option<PeriodicityResult> {
        let series = self.longterm.energy_series();
        periodicity::estimate_period(&series, self.longterm_fps, 30.0, 600.0)
    }

    /// Build a snapshot for the UI. Recomputes the windowed statistics, so call
    /// it at `analysis_update_hz`, not per frame.
    pub fn snapshot(&mut self) -> AnalysisSnapshot {
        // Merging a few dozen block summaries, not rescanning millions of
        // samples. This is the difference between 24% of a core and 0.5%.
        let stats = self.health.stats();

        AnalysisSnapshot {
            format: self.format.clone(),
            stats,
            histogram: self.health.histogram(),
            spectrum_db: self.mono_db.clone(),
            background_db: self.detector.background().level_db().to_vec(),
            excess_db: self.detector.excess_db().to_vec(),
            direction: self.live_direction,
            periodicity: self.periodicity(),
            timeline_seconds: self.format.frames_to_seconds(self.ring.total_frames()),
            frames_analyzed: self.frames_analyzed,
            gap_count: self.gap_count,
            gap_seconds: self.format.frames_to_seconds(self.gap_frames_total),
            overrun_count: self.overrun_count,
            warmup_progress: self.detector.background().warmup_progress(),
            open_events: self.detector.open_event_count(),
            is_silent: stats.is_silent(),
            keying: self.keying(),
            morse: self.morse(),
            structure: self.structure.clone(),
            structure_tile: self.structure_tile,
            kurtosis_hot_bins: self.peak_kurtosis_bins,
            kurtosis_peak: self.peak_kurtosis,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::direction::angular_error_deg;
    use crate::audio::SampleFormat;
    use crate::audio::format::{MASK_7_1, MASK_STEREO};
    use crate::audio::synthetic::{
        LANDSCAPE_PERIOD_SECONDS, SyntheticSource, TIGHTBEAM_SYMBOL_SECONDS, TIGHTBEAM_TONES_HZ,
        TestSignal,
    };

    /// A configuration that runs the full chain quickly at a reduced sample rate.
    fn fast_config() -> Config {
        let mut c = Config::default();
        c.fft_size = 1024;
        c.hop = 512;
        c.pcm_ring_seconds = 4.0;
        c.waterfall_seconds = 30.0;
        c.longterm_fps = 2.0;
        c.longterm_bands = 64;
        c.background_time_constant_seconds = 5.0;
        c.background_max_freeze_seconds = 600.0;
        c.min_event_seconds = 0.5;
        c
    }

    fn format(channels: usize, mask: u32) -> StreamFormat {
        StreamFormat::new(8_000, channels, mask, SampleFormat::F32)
    }

    fn feed(
        engine: &mut AnalysisEngine,
        source: &mut SyntheticSource,
        seconds: f32,
    ) -> Vec<Detection> {
        let sr = source.format().sample_rate as f32;
        let chunk = (sr * 0.1) as usize; // 100 ms at a time, like a real device
        let total = (seconds * sr) as usize;
        let mut out = Vec::new();
        let mut buf = Vec::new();
        let mut done = 0;
        while done < total {
            let n = chunk.min(total - done);
            buf.clear();
            source.render(n, &mut buf);
            out.extend(engine.push_interleaved(&buf));
            done += n;
        }
        out
    }

    #[test]
    fn silence_produces_no_detections() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut source = SyntheticSource::new(TestSignal::Silence, f, 0.0);
        let detections = feed(&mut engine, &mut source, 30.0);
        assert!(detections.is_empty(), "{detections:?}");

        let snap = engine.snapshot();
        assert!(snap.is_silent);
        assert_eq!(snap.gap_count, 0);
        assert!(snap.frames_analyzed > 0);
        assert!(snap.warmup_progress >= 1.0);
    }

    #[test]
    fn steady_noise_alone_does_not_trigger() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut source = SyntheticSource::new(TestSignal::Noise, f, 0.0);
        let detections = feed(&mut engine, &mut source, 40.0);
        // The background learns the noise, so nothing should stand out.
        assert!(detections.is_empty(), "steady noise fired: {detections:?}");
    }

    #[test]
    fn snapshot_reports_the_stream_shape_and_timeline() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut source = SyntheticSource::new(TestSignal::Sine { hz: 300.0 }, f, 0.0);
        feed(&mut engine, &mut source, 2.0);

        let snap = engine.snapshot();
        assert_eq!(snap.format.channels, 2);
        assert!((snap.timeline_seconds - 2.0).abs() < 0.01);
        assert_eq!(snap.spectrum_db.len(), 513);
        assert_eq!(snap.background_db.len(), 513);
        assert_eq!(snap.histogram.len(), 100);
        assert!(!snap.is_silent);
    }

    #[test]
    fn a_gap_advances_the_clock_and_is_counted() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut source = SyntheticSource::new(TestSignal::Noise, f, 0.0);
        feed(&mut engine, &mut source, 1.0);
        engine.push_gap(8_000); // one second
        feed(&mut engine, &mut source, 1.0);

        let snap = engine.snapshot();
        assert_eq!(snap.gap_count, 1);
        assert!((snap.gap_seconds - 1.0).abs() < 1e-6);
        assert!(
            (snap.timeline_seconds - 3.0).abs() < 0.01,
            "gap must advance the timeline, got {}",
            snap.timeline_seconds
        );
    }

    #[test]
    fn detects_a_tone_burst_and_reports_its_bearing() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.direction_finding = true; // secondary feature, opt-in
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        let mut quiet = SyntheticSource::new(TestSignal::Silence, f.clone(), 0.0);
        feed(&mut engine, &mut quiet, 20.0); // settle the background

        let target = -20.0;
        let mut tone = SyntheticSource::new(TestSignal::Sine { hz: 900.0 }, f.clone(), target);
        feed(&mut engine, &mut tone, 4.0);
        let mut detections = feed(&mut engine, &mut quiet, 5.0);
        detections.extend(engine.flush());

        assert_eq!(
            detections.len(),
            1,
            "expected one detection: {detections:?}"
        );
        let d = &detections[0];
        assert!(
            (d.event.peak_hz - 900.0).abs() < 60.0,
            "peak at {} Hz",
            d.event.peak_hz
        );
        assert!(
            d.event.duration_seconds > 2.0,
            "{}",
            d.event.duration_seconds
        );
        assert!(d.direction.is_usable());
        assert!(
            angular_error_deg(d.direction.azimuth_deg, target) < 10.0,
            "bearing {} vs target {target}",
            d.direction.azimuth_deg
        );
        assert!(!d.spans_gap);
        // The detection maps back onto the capture timeline for the pre-roll.
        assert!(d.end_sample > d.start_sample);
    }

    #[test]
    fn a_detection_overlapping_a_gap_is_flagged() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut quiet = SyntheticSource::new(TestSignal::Silence, f.clone(), 0.0);
        feed(&mut engine, &mut quiet, 20.0);

        let mut tone = SyntheticSource::new(TestSignal::Sine { hz: 700.0 }, f.clone(), 0.0);
        feed(&mut engine, &mut tone, 3.0);
        engine.push_gap(4_000);
        feed(&mut engine, &mut tone, 3.0);
        let mut detections = feed(&mut engine, &mut quiet, 5.0);
        detections.extend(engine.flush());

        assert!(!detections.is_empty());
        // The gap severs the event, so at least one side must be marked.
        assert!(
            detections.iter().any(|d| d.spans_gap) || detections.len() >= 2,
            "a gap must not be papered over: {detections:?}"
        );
    }

    /// Acceptance test 7: detect the synthetic Landscape Signal, recover its
    /// 109.5 s period to within a second, and its azimuth to within 10 degrees.
    #[test]
    fn acceptance_detects_the_landscape_signal_period_and_bearing() {
        let f = format(8, MASK_7_1);
        let mut cfg = fast_config();
        cfg.longterm_fps = 1.0;
        cfg.pcm_ring_seconds = 2.0; // keep the test's memory small
        cfg.background_time_constant_seconds = 20.0;
        cfg.background_max_freeze_seconds = 600.0;
        cfg.direction_finding = true; // this acceptance test checks the bearing

        let mut engine = AnalysisEngine::new(cfg, f.clone());
        let target = -55.0;
        let mut source = SyntheticSource::new(TestSignal::Landscape, f, target);

        // Three full cycles: one to settle, two for the autocorrelation.
        let mut detections = feed(&mut engine, &mut source, LANDSCAPE_PERIOD_SECONDS * 3.0);
        detections.extend(engine.flush());

        assert!(
            !detections.is_empty(),
            "the mountain should have been detected"
        );

        let period = engine
            .periodicity()
            .expect("long-term tier should support a period estimate");
        assert!(
            (period.period_seconds - LANDSCAPE_PERIOD_SECONDS).abs() < 1.0,
            "period {} s, expected {LANDSCAPE_PERIOD_SECONDS} s (confidence {}, prominence {})",
            period.period_seconds,
            period.confidence,
            period.prominence
        );
        assert!(
            periodicity::matches_landscape(&period, 1.0),
            "should be recognized as the Landscape Signal: {period:?}"
        );

        let best = detections
            .iter()
            .max_by(|a, b| a.event.score.partial_cmp(&b.event.score).unwrap())
            .unwrap();
        assert!(
            angular_error_deg(best.direction.azimuth_deg, target) < 10.0,
            "bearing {} vs target {target} (confidence {})",
            best.direction.azimuth_deg,
            best.direction.confidence
        );
    }

    /// The compass must read something while merely listening.
    ///
    /// The live bearing used to come from an accumulator that only ran for bins
    /// clearing `novelty_threshold_db`, because a *detection* has to be
    /// attributed to the material that caused it. Nothing clears that bar in
    /// ordinary listening, so the needle sat at `insufficient()` forever — a
    /// dead instrument, indistinguishable from a broken one, which is exactly
    /// how it looked in the cockpit.
    #[test]
    fn the_live_bearing_tracks_a_source_without_any_detection() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.direction_finding = true;
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        // A steady tone off to the left. Steady is the point: it never rises
        // far enough above its own background to open an event.
        let mut source = SyntheticSource::new(TestSignal::Sine { hz: 900.0 }, f, -25.0);
        let detections = feed(&mut engine, &mut source, 4.0);

        let d = engine.snapshot().direction;
        assert!(
            d.is_usable(),
            "the compass must have an answer while just listening, got {d:?}"
        );
        assert!(
            d.azimuth_deg < -3.0,
            "a source on the left must read left, got {:+.1}°",
            d.azimuth_deg
        );
        assert!(d.confidence > 0.5, "a cleanly panned tone is not ambiguous");

        // And this is true whether or not anything was detected — the point is
        // that the bearing does not depend on it.
        let _ = detections;
    }

    /// A centred source reads centred, rather than reading as nothing.
    #[test]
    fn a_centred_source_is_measured_not_dropped() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.direction_finding = true;
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        let mut source = SyntheticSource::new(TestSignal::Sine { hz: 900.0 }, f, 0.0);
        feed(&mut engine, &mut source, 4.0);

        let d = engine.snapshot().direction;
        assert!(d.is_usable(), "centred is a measurement, not a failure");
        assert!(
            d.azimuth_deg.abs() < 3.0,
            "a centred source must read centred, got {:+.1}°",
            d.azimuth_deg
        );
    }

    #[test]
    fn the_fast_path_keeps_one_transform_and_a_mono_ring() {
        // On a 7.1 endpoint the fast path is the difference between eight
        // transforms per frame and one, and between 220 MB of ring and 27 MB.
        // Pinned explicitly rather than inherited: direction finding is on by
        // default now, and this test is about what happens when it is not.
        let mut cfg = fast_config();
        cfg.direction_finding = false;

        let engine = AnalysisEngine::new(cfg, format(8, MASK_7_1));
        assert!(!engine.direction_finding());
        assert_eq!(
            engine.ring().channels(),
            1,
            "the ring should hold a downmix"
        );
        assert_eq!(
            engine.ring_format().channels,
            1,
            "captures are mono in fast mode"
        );
    }

    #[test]
    fn enabling_direction_finding_restores_every_channel() {
        let mut cfg = fast_config();
        cfg.direction_finding = true;
        let engine = AnalysisEngine::new(cfg, format(8, MASK_7_1));
        assert!(engine.direction_finding());
        assert_eq!(engine.ring().channels(), 8);
        assert_eq!(engine.ring_format().channels, 8);
        assert_eq!(engine.ring_format().layout_name(), "7.1");
    }

    #[test]
    fn the_fast_path_still_detects_and_still_finds_the_period() {
        // Dropping to mono must not cost us the primary function.
        let f = format(8, MASK_7_1);
        let mut cfg = fast_config();
        cfg.longterm_fps = 1.0;
        cfg.pcm_ring_seconds = 2.0;
        cfg.background_time_constant_seconds = 20.0;
        cfg.background_max_freeze_seconds = 600.0;
        // This test covers the mono fast path specifically.
        cfg.direction_finding = false;

        let mut engine = AnalysisEngine::new(cfg, f.clone());
        let mut source = SyntheticSource::new(TestSignal::Landscape, f, -55.0);
        let mut detections = feed(&mut engine, &mut source, LANDSCAPE_PERIOD_SECONDS * 3.0);
        detections.extend(engine.flush());

        assert!(
            !detections.is_empty(),
            "mono path still has to detect the mountain"
        );
        let period = engine.periodicity().expect("period estimate");
        assert!(
            (period.period_seconds - LANDSCAPE_PERIOD_SECONDS).abs() < 1.0,
            "period {} s in the fast path",
            period.period_seconds
        );
    }

    /// Primary function 1: a binary transmission is present.
    #[test]
    fn acceptance_detects_binary_keying_in_a_tightbeam() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.min_event_seconds = 0.2;
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        // Settle the background on silence so the tones stand out.
        let mut quiet = SyntheticSource::new(TestSignal::Silence, f.clone(), 0.0);
        feed(&mut engine, &mut quiet, 15.0);

        let mut beam = SyntheticSource::new(TestSignal::Tightbeam, f, 0.0);
        feed(&mut engine, &mut beam, 25.0);

        let k = engine.keying().expect("a tightbeam should produce symbols");
        assert!(
            k.is_present(0.5),
            "the transmission should be detected: {k:?}"
        );
        assert!(k.tones_hz.len() >= 2, "two keying tones expected: {k:?}");

        let mut tones = k.tones_hz.clone();
        tones.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(
            (tones[0] - TIGHTBEAM_TONES_HZ[0]).abs() < 150.0,
            "low tone {} vs {}",
            tones[0],
            TIGHTBEAM_TONES_HZ[0]
        );
        assert!(
            (tones[1] - TIGHTBEAM_TONES_HZ[1]).abs() < 150.0,
            "high tone {} vs {}",
            tones[1],
            TIGHTBEAM_TONES_HZ[1]
        );
        assert!(
            (k.symbol_rate_hz - 1.0 / TIGHTBEAM_SYMBOL_SECONDS).abs() < 2.0,
            "symbol rate {} vs {}",
            k.symbol_rate_hz,
            1.0 / TIGHTBEAM_SYMBOL_SECONDS
        );
    }

    #[test]
    fn ordinary_audio_does_not_report_binary_keying() {
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut noise = SyntheticSource::new(TestSignal::Noise, f.clone(), 0.0);
        feed(&mut engine, &mut noise, 20.0);
        let mut sweep = SyntheticSource::new(
            TestSignal::Sweep {
                start_hz: 200.0,
                end_hz: 3000.0,
                seconds: 3.0,
            },
            f,
            0.0,
        );
        feed(&mut engine, &mut sweep, 20.0);

        if let Some(k) = engine.keying() {
            assert!(
                !k.is_present(0.5),
                "noise and sweeps are not transmissions: {k:?}"
            );
        }
    }

    /// Primary function 2: a picture is present in the spectrogram.
    #[test]
    fn acceptance_detects_a_drawing_in_the_spectrogram() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.waterfall_seconds = 40.0;
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        let mut picture = SyntheticSource::new(TestSignal::Picture, f, 0.0);
        feed(&mut engine, &mut picture, 35.0);

        let s = engine.structure().clone();
        assert!(s.is_present(0.85), "the drawing should be found: {s:?}");
        // Continuity, not orientation diversity. The scanner returns the
        // highest-*scoring* tile, and the score is now driven by continuity, so
        // the tile it picks is the one with the longest strokes rather than the
        // one with the most varied gradients. Diversity remains a diagnostic —
        // measured, it sits between 0.45 and 0.97 on line art and on ambience
        // alike, which is why it no longer decides anything.
        assert!(
            s.continuity > 0.8,
            "a drawing is made of long connected strokes: {s:?}"
        );
    }

    #[test]
    fn a_page_of_harmonics_outranks_nothing() {
        // The false positive that matters: sustained tones are coherent lines.
        // A sweep plus steady tones must score below a real drawing.
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.waterfall_seconds = 40.0;

        let mut tonal_engine = AnalysisEngine::new(cfg.clone(), f.clone());
        let mut tone = SyntheticSource::new(TestSignal::Sine { hz: 800.0 }, f.clone(), 0.0);
        feed(&mut tonal_engine, &mut tone, 35.0);
        let tonal = tonal_engine.structure().clone();

        let mut art_engine = AnalysisEngine::new(cfg, f.clone());
        let mut picture = SyntheticSource::new(TestSignal::Picture, f, 0.0);
        feed(&mut art_engine, &mut picture, 35.0);
        let art = art_engine.structure().clone();

        assert!(
            art.score > tonal.score,
            "the drawing {art:?} must outrank a held tone {tonal:?}"
        );
    }

    #[test]
    fn low_frequency_rumble_never_reads_as_a_transmission() {
        // Regression from the field: cockpit rumble produced "tones" at 117,
        // 152 and 293 Hz with 0.51 confidence. Two guards now reject it — a
        // frequency floor, and prominence measured against the neighbouring
        // bins rather than the mean of a mostly-empty spectrum.
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());

        let mut quiet = SyntheticSource::new(TestSignal::Silence, f.clone(), 0.0);
        feed(&mut engine, &mut quiet, 15.0);

        // A wandering low-frequency drone, the shape of ship rumble.
        for step in 0..40 {
            let hz = [120.0f32, 150.0, 290.0][step % 3];
            let mut rumble = SyntheticSource::new(TestSignal::Sine { hz }, f.clone(), 0.0);
            feed(&mut engine, &mut rumble, 0.5);
        }

        match engine.keying() {
            None => {}
            Some(k) => assert!(
                !k.is_present(0.5),
                "low rumble must not read as a transmission: {k:?}"
            ),
        }
    }

    #[test]
    fn a_transmission_above_the_floor_is_still_detected() {
        // The floor must not cost us the real thing: the tightbeam keys at
        // 1200/2400 Hz, far above it.
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.min_event_seconds = 0.2;
        let mut engine = AnalysisEngine::new(cfg, f.clone());

        let mut quiet = SyntheticSource::new(TestSignal::Silence, f.clone(), 0.0);
        feed(&mut engine, &mut quiet, 15.0);
        let mut beam = SyntheticSource::new(TestSignal::Tightbeam, f, 0.0);
        feed(&mut engine, &mut beam, 25.0);

        let k = engine.keying().expect("symbols");
        assert!(k.is_present(0.5), "{k:?}");
        assert!(
            k.tones_hz.iter().all(|t| *t >= 400.0),
            "every reported tone must clear the floor: {:?}",
            k.tones_hz
        );
    }

    #[test]
    fn broadband_noise_never_reads_as_a_transmission() {
        // Regression: successive STFT frames overlap by half their samples, so
        // even noise has a peak bin that persists a frame or two. Without the
        // tonality gate this measured 0.94 confidence on pure noise.
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(fast_config(), f.clone());
        let mut noise = SyntheticSource::new(TestSignal::Noise, f, 0.0);
        feed(&mut engine, &mut noise, 30.0);

        match engine.keying() {
            None => {}
            Some(k) => assert!(!k.is_present(0.5), "noise must not read as keying: {k:?}"),
        }
    }

    #[test]
    fn detectors_can_be_switched_off() {
        let mut cfg = fast_config();
        cfg.detect_keying = false;
        cfg.detect_structure = false;
        let f = format(2, MASK_STEREO);
        let mut engine = AnalysisEngine::new(cfg, f.clone());
        let mut beam = SyntheticSource::new(TestSignal::Tightbeam, f, 0.0);
        feed(&mut engine, &mut beam, 5.0);

        assert!(engine.keying().is_none());
        assert_eq!(engine.structure().score, 0.0);
    }

    #[test]
    fn long_term_rate_reflects_actual_decimation_not_the_request() {
        // 8 kHz with a 512 hop is 15.625 frames/s. Asking for 1 fps decimates by
        // 16, which really yields 0.9766 fps. Reporting the requested rate
        // instead would scale every period reading by 2.3%.
        let mut cfg = fast_config();
        cfg.longterm_fps = 1.0;
        let engine = AnalysisEngine::new(cfg, format(2, MASK_STEREO));
        let expected = (8_000.0 / 512.0) / 16.0;
        assert!(
            (engine.long_term_fps() - expected).abs() < 1e-4,
            "expected {expected}, got {}",
            engine.long_term_fps()
        );
        assert!(engine.long_term_fps() < 1.0);
    }

    #[test]
    fn memory_stays_bounded_over_a_long_run() {
        let f = format(2, MASK_STEREO);
        let mut cfg = fast_config();
        cfg.pcm_ring_seconds = 2.0;
        cfg.waterfall_seconds = 5.0;
        let mut engine = AnalysisEngine::new(cfg, f.clone());
        let mut source = SyntheticSource::new(TestSignal::Noise, f, 0.0);

        feed(&mut engine, &mut source, 20.0);
        let ring_bytes = engine.ring().bytes();
        let waterfall_frames = engine.waterfall().len();
        let longterm_frames = engine.long_term().len();

        feed(&mut engine, &mut source, 60.0);
        assert_eq!(engine.ring().bytes(), ring_bytes, "the ring must not grow");
        assert_eq!(
            engine.waterfall().len(),
            waterfall_frames,
            "the waterfall must have reached its cap and stayed there"
        );
        assert!(engine.waterfall().len() <= engine.waterfall().capacity());
        assert!(engine.long_term().len() >= longterm_frames);
        assert!(engine.long_term().len() <= engine.long_term().capacity());
    }
}

//! Binary keying detection — "is something transmitting data here?"
//!
//! The Thargoid Probe tightbeam is not spectrogram art. It is a data stream:
//! triplets of high and low audio tones, clocked at a regular symbol rate, in
//! five chunks of `Wail | Header | Data`. That has a signature no natural game
//! audio produces:
//!
//! * energy parks on a **small alphabet of discrete frequencies** rather than
//!   sliding around,
//! * it **alternates** between them,
//! * and the dwell times **cluster on a symbol period** instead of varying.
//!
//! Music fails the first test (harmonics move, and there are many of them),
//! engine noise fails the second (it sits still), and a chirp fails all three.
//!
//! The whole detector runs on the dominant-bin index of each STFT frame — one
//! `argmax` over a spectrum the pipeline already computed. There are no extra
//! transforms and the state is a few hundred bytes.

/// How much recent history the keying assessment covers.
///
/// Long enough to hold plenty of symbols, short enough that the reading tracks
/// what is happening now rather than what happened minutes ago.
pub const KEYING_HISTORY_SECONDS: f32 = 45.0;

/// One observed symbol: a run of frames on the same tone.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Symbol {
    bin: usize,
    frames: usize,
    /// Frame index at which this symbol closed, so it can be aged out.
    closed_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyingDetection {
    /// Frequencies the transmission rests on, strongest first.
    pub tones_hz: Vec<f32>,
    /// Estimated symbols per second.
    pub symbol_rate_hz: f32,
    /// How tightly dwell times cluster on one period, 0..1.
    pub timing_regularity: f32,
    /// Fraction of active frames explained by the tone alphabet, 0..1.
    pub alphabet_purity: f32,
    /// How consistently the transmission returns to the *same* frequencies,
    /// 0..1.
    ///
    /// This is what separates a transmission from wandering ambience. A keyed
    /// signal reuses a fixed alphabet for its whole duration, so the tones in
    /// the second half of a window match those in the first. Ship ambience
    /// drifts, and a swept stroke — like the Landscape Signal's — never revisits
    /// a frequency at all.
    pub tone_stability: f32,
    /// Transitions between tones per second — separates keying from a held note.
    pub transitions_per_second: f32,
    /// Combined 0..1.
    pub confidence: f32,
    /// Symbols observed in the window.
    pub symbol_count: usize,
}

impl KeyingDetection {
    /// Whether this looks like a real transmission rather than incidental
    /// structure.
    pub fn is_present(&self, threshold: f32) -> bool {
        self.confidence >= threshold
    }
}

/// Streaming keying detector over dominant-bin observations.
#[derive(Debug)]
pub struct KeyingDetector {
    /// Completed symbols, oldest first.
    symbols: std::collections::VecDeque<Symbol>,
    /// The run currently being accumulated.
    current: Option<Symbol>,
    /// Frames of silence seen since the last active frame.
    idle_frames: usize,
    frame_seconds: f32,
    bin_hz: f32,
    /// How many symbols of history to keep.
    capacity: usize,
    /// Frames processed, which is the clock symbols are aged against.
    frames_seen: u64,
    /// Symbols older than this are forgotten.
    ///
    /// Without it the detector is a latch rather than a live reading: evicting
    /// only on capacity means one good burst keeps reporting "present" until
    /// hundreds of fresh symbols push it out — and if the signal stops, none
    /// ever arrive, so it stays lit indefinitely.
    history_frames: u64,
    /// Bins this far apart are treated as the same tone, absorbing the jitter of
    /// a tone that straddles two bins.
    bin_tolerance: usize,
    /// A run shorter than this is frame-to-frame flicker, not a symbol. Real
    /// keying dwells on a tone; noise changes its peak bin every frame.
    min_symbol_frames: usize,
    /// Idle gap after which the current run is closed.
    max_idle_frames: usize,
}

impl KeyingDetector {
    pub fn new(frame_seconds: f32, sample_rate: u32, fft_size: usize) -> Self {
        Self {
            symbols: std::collections::VecDeque::new(),
            current: None,
            idle_frames: 0,
            frame_seconds,
            bin_hz: sample_rate as f32 / fft_size as f32,
            capacity: 512,
            frames_seen: 0,
            history_frames: (KEYING_HISTORY_SECONDS / frame_seconds).ceil().max(1.0) as u64,
            bin_tolerance: 2,
            min_symbol_frames: 2,
            max_idle_frames: (0.5 / frame_seconds).ceil().max(1.0) as usize,
        }
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// Feed one frame.
    ///
    /// `peak_bin` is the loudest bin and `active` says whether the frame carried
    /// anything above the background at all. An inactive frame ends the current
    /// symbol rather than extending it.
    pub fn push(&mut self, peak_bin: usize, active: bool) {
        self.frames_seen += 1;
        // Age first, so a detector receiving only silence still decays.
        self.expire();

        if !active {
            self.idle_frames += 1;
            if self.idle_frames >= self.max_idle_frames {
                self.close_current();
            }
            return;
        }
        self.idle_frames = 0;

        match &mut self.current {
            Some(run) if peak_bin.abs_diff(run.bin) <= self.bin_tolerance => {
                run.frames += 1;
            }
            _ => {
                self.close_current();
                self.current = Some(Symbol {
                    bin: peak_bin,
                    frames: 1,
                    closed_at: 0,
                });
            }
        }
    }

    fn close_current(&mut self) {
        if let Some(run) = self.current.take()
            && run.frames >= self.min_symbol_frames
        {
            let closed_at = self.frames_seen;
            self.symbols.push_back(Symbol { closed_at, ..run });
            while self.symbols.len() > self.capacity {
                self.symbols.pop_front();
            }
        }
    }

    /// Forget symbols that have fallen out of the history window.
    fn expire(&mut self) {
        let cutoff = self.frames_seen.saturating_sub(self.history_frames);
        while self.symbols.front().is_some_and(|s| s.closed_at < cutoff) {
            self.symbols.pop_front();
        }
    }

    pub fn reset(&mut self) {
        self.symbols.clear();
        self.current = None;
        self.idle_frames = 0;
    }

    /// Seconds of history the assessment covers.
    pub fn history_seconds(&self) -> f32 {
        self.history_frames as f32 * self.frame_seconds
    }

    /// Assess the observed symbols. Returns `None` until there is enough to say
    /// anything — a handful of transitions proves nothing.
    pub fn evaluate(&self) -> Option<KeyingDetection> {
        const MIN_SYMBOLS: usize = 8;
        if self.symbols.len() < MIN_SYMBOLS {
            return None;
        }

        // Cluster symbols onto tones, merging bins within tolerance.
        let mut tones: Vec<(usize, usize)> = Vec::new(); // (bin, frames)
        for s in &self.symbols {
            match tones
                .iter_mut()
                .find(|(bin, _)| s.bin.abs_diff(*bin) <= self.bin_tolerance)
            {
                Some((_, frames)) => *frames += s.frames,
                None => tones.push((s.bin, s.frames)),
            }
        }
        tones.sort_by_key(|t| std::cmp::Reverse(t.1));

        let total_frames: usize = tones.iter().map(|(_, f)| *f).sum();
        if total_frames == 0 {
            return None;
        }

        // Keying uses a small alphabet. Take the top few and ask how much of the
        // transmission they explain.
        const ALPHABET: usize = 3;
        let kept: Vec<(usize, usize)> = tones.iter().take(ALPHABET).copied().collect();
        let alphabet_purity =
            kept.iter().map(|(_, f)| *f).sum::<usize>() as f32 / total_frames as f32;

        // Dwell times should cluster. Use the median as the symbol period and
        // measure spread around it — robust to the long "wail" that opens each
        // chunk, which a mean would be dragged by.
        let mut durations: Vec<usize> = self.symbols.iter().map(|s| s.frames).collect();
        durations.sort_unstable();
        let median = durations[durations.len() / 2].max(1);
        let within = durations
            .iter()
            .filter(|d| {
                let ratio = **d as f32 / median as f32;
                (0.5..=2.0).contains(&ratio)
            })
            .count();
        let timing_regularity = within as f32 / durations.len() as f32;

        let symbol_rate_hz = 1.0 / (median as f32 * self.frame_seconds);

        // Tone stability: do the second half's tones match the first half's?
        let tone_stability = {
            let mid = self.symbols.len() / 2;
            let (first, second): (Vec<&Symbol>, Vec<&Symbol>) = {
                let all: Vec<&Symbol> = self.symbols.iter().collect();
                (all[..mid].to_vec(), all[mid..].to_vec())
            };
            if first.is_empty() || second.is_empty() {
                0.0
            } else {
                let mut early: Vec<usize> = Vec::new();
                for sym in &first {
                    if !early
                        .iter()
                        .any(|b| sym.bin.abs_diff(*b) <= self.bin_tolerance)
                    {
                        early.push(sym.bin);
                    }
                }
                let matched: usize = second
                    .iter()
                    .filter(|sym| {
                        early
                            .iter()
                            .any(|b| sym.bin.abs_diff(*b) <= self.bin_tolerance)
                    })
                    .map(|sym| sym.frames)
                    .sum();
                let total: usize = second.iter().map(|sym| sym.frames).sum();
                if total == 0 {
                    0.0
                } else {
                    matched as f32 / total as f32
                }
            }
        };

        // Transitions per second, so a single held tone cannot score.
        let span_frames: usize = self.symbols.iter().map(|s| s.frames).sum();
        let span_seconds = span_frames as f32 * self.frame_seconds;
        let transitions_per_second = if span_seconds > 0.0 {
            (self.symbols.len() - 1) as f32 / span_seconds
        } else {
            0.0
        };

        // These combine multiplicatively, not as a weighted sum. Each is
        // *necessary*: a weighted sum lets a frequency sweep score 0.62 on
        // regular timing alone despite only 6% of its energy sitting on any
        // small alphabet — measured, and the reason this is written this way.
        let distinct = kept.len().min(tones.len());
        let alphabet_term = if distinct >= 2 { 1.0 } else { 0.0 };
        // Transitions saturate quickly: even 2/s is unmistakably keyed.
        let transition_term = (transitions_per_second / 2.0).clamp(0.0, 1.0);
        // Timing supports rather than gates — a real transmission can carry a
        // long opening wail that skews its own dwell distribution.
        let timing_term = 0.6 + 0.4 * timing_regularity;

        // Stability joins the other necessary properties multiplicatively. It is
        // the one that distinguishes a transmission from anything that merely
        // wanders across a few tones: measured, ship ambience reached 0.89
        // confidence without it, above a genuine keyed signal.
        let confidence =
            (alphabet_term * alphabet_purity * transition_term * timing_term * tone_stability)
                .clamp(0.0, 1.0);

        Some(KeyingDetection {
            tones_hz: kept
                .iter()
                .map(|(bin, _)| *bin as f32 * self.bin_hz)
                .collect(),
            symbol_rate_hz,
            timing_regularity,
            alphabet_purity,
            tone_stability,
            transitions_per_second,
            confidence,
            symbol_count: self.symbols.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_SECONDS: f32 = 512.0 / 48_000.0; // ~10.7 ms
    const SAMPLE_RATE: u32 = 48_000;
    const FFT: usize = 1024;

    fn detector() -> KeyingDetector {
        KeyingDetector::new(FRAME_SECONDS, SAMPLE_RATE, FFT)
    }

    /// No keying, whether because nothing qualified as a symbol or because what
    /// did scored too low. Both are a negative result.
    fn assert_not_keying(d: &KeyingDetector, what: &str) {
        match d.evaluate() {
            None => {}
            Some(r) => assert!(!r.is_present(0.5), "{what} read as a transmission: {r:?}"),
        }
    }

    /// Feed an alternating two-tone pattern with a fixed symbol length.
    fn feed_keyed(
        d: &mut KeyingDetector,
        bins: &[usize],
        frames_per_symbol: usize,
        symbols: usize,
    ) {
        for i in 0..symbols {
            let bin = bins[i % bins.len()];
            for _ in 0..frames_per_symbol {
                d.push(bin, true);
            }
        }
    }

    #[test]
    fn nothing_is_reported_before_there_is_evidence() {
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 4, 3);
        assert!(d.evaluate().is_none(), "three symbols prove nothing");
    }

    #[test]
    fn a_clean_two_tone_transmission_is_detected() {
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 4, 40);
        d.push(0, false);
        for _ in 0..60 {
            d.push(0, false);
        }

        let r = d.evaluate().expect("enough symbols");
        assert!(r.confidence > 0.8, "clean keying should be obvious: {r:?}");
        assert!(r.is_present(0.5));
        assert_eq!(
            r.tones_hz.len().min(2),
            2,
            "two tones expected: {:?}",
            r.tones_hz
        );

        // 40 * 48000/1024 = 1875 Hz, 80 -> 3750 Hz.
        let mut tones = r.tones_hz.clone();
        tones.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((tones[0] - 1875.0).abs() < 60.0, "{tones:?}");
        assert!((tones[1] - 3750.0).abs() < 60.0, "{tones:?}");

        // Four frames per symbol at 10.7 ms => ~23 symbols/s.
        assert!(
            (r.symbol_rate_hz - 23.4).abs() < 3.0,
            "symbol rate {}",
            r.symbol_rate_hz
        );
        assert!(r.timing_regularity > 0.9, "{r:?}");
    }

    #[test]
    fn a_single_held_tone_is_not_keying() {
        let mut d = detector();
        // Same bin throughout: one long symbol, no alphabet, no transitions.
        for _ in 0..400 {
            d.push(55, true);
        }
        // Even forcing symbol closure with idle gaps, it stays one tone.
        for _ in 0..8 {
            for _ in 0..60 {
                d.push(0, false);
            }
            for _ in 0..20 {
                d.push(55, true);
            }
        }
        assert_not_keying(&d, "a steady held tone");
    }

    #[test]
    fn single_frame_flicker_never_becomes_a_symbol() {
        // Noise changes its peak bin every frame. That is not a transmission,
        // and treating it as a stream of one-frame symbols made noise look
        // perfectly "regular".
        // Genuinely per-frame flicker: each frame lands far from the last, so
        // no run ever reaches two frames.
        let mut d = detector();
        let mut state = 0xDEAD_BEEFu32;
        for _ in 0..500 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            d.push((state >> 18) as usize % 400, true);
        }
        // A handful survive by chance when the generator happens to land twice
        // within bin tolerance; 500 frames of flicker must not yield 500 symbols.
        assert!(
            d.symbol_count() < 20,
            "one-frame runs must be discarded, got {}",
            d.symbol_count()
        );
        assert_not_keying(&d, "per-frame flicker");
    }

    #[test]
    fn a_frequency_sweep_is_not_keying() {
        let mut d = detector();
        // The peak marches steadily upward — many tones, no alphabet.
        for step in 0..300 {
            d.push(20 + step / 2, true);
        }
        let r = d.evaluate().expect("a sweep does produce symbols");
        assert!(
            r.alphabet_purity < 0.3,
            "a sweep should not concentrate on a few tones: {r:?}"
        );
        assert!(!r.is_present(0.5), "{r:?}");
    }

    #[test]
    fn wandering_noise_is_not_keying() {
        let mut d = detector();
        // Pseudo-random peak bin: no alphabet, no timing.
        let mut state = 0x1234_5678u32;
        for _ in 0..400 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            d.push((state >> 20) as usize % 400, true);
        }
        assert_not_keying(&d, "wandering noise");
    }

    #[test]
    fn bin_jitter_does_not_split_one_tone_in_two() {
        let mut d = detector();
        // A tone straddling two bins should still read as one symbol.
        for i in 0..40 {
            let bin = if i % 2 == 0 { 40 } else { 41 };
            for _ in 0..4 {
                d.push(bin, true);
            }
        }
        assert!(
            d.symbol_count() <= 2,
            "jitter within tolerance should not create symbols: {}",
            d.symbol_count()
        );
    }

    #[test]
    fn silence_closes_a_symbol_rather_than_extending_it() {
        let mut d = detector();
        for _ in 0..5 {
            d.push(30, true);
        }
        // A long idle gap ends the run.
        for _ in 0..60 {
            d.push(0, false);
        }
        assert_eq!(d.symbol_count(), 1);
        for _ in 0..5 {
            d.push(30, true);
        }
        for _ in 0..60 {
            d.push(0, false);
        }
        assert_eq!(d.symbol_count(), 2, "the gap must separate the two bursts");
    }

    #[test]
    fn a_fixed_alphabet_is_stable_and_a_sweep_is_not() {
        // The property that separates a transmission from wandering ambience.
        let mut keyed = detector();
        feed_keyed(&mut keyed, &[40, 80], 4, 40);
        let k = keyed.evaluate().unwrap();
        assert!(
            k.tone_stability > 0.9,
            "keying reuses its alphabet throughout: {k:?}"
        );

        // A slow sweep: every symbol lands somewhere new, so the second half
        // shares nothing with the first.
        let mut sweep = detector();
        for step in 0..80 {
            for _ in 0..4 {
                sweep.push(20 + step * 3, true);
            }
        }
        let s = sweep.evaluate().unwrap();
        assert!(
            s.tone_stability < 0.2,
            "a sweep never returns to a frequency: {s:?}"
        );
        assert!(!s.is_present(0.5), "{s:?}");
    }

    #[test]
    fn drifting_tones_score_below_a_fixed_alphabet() {
        // Ambience that wanders slowly across a handful of nearby tones scored
        // 0.85-0.89 in the field, above a genuine signal. Stability must pull it
        // down.
        let mut drifting = detector();
        for block in 0..20 {
            // The alphabet itself moves as the window advances.
            let a = 40 + block * 2;
            let b = 90 + block * 2;
            for (i, bin) in [a, b].iter().enumerate() {
                let _ = i;
                for _ in 0..4 {
                    drifting.push(*bin, true);
                }
            }
        }
        let d = drifting.evaluate().unwrap();

        let mut fixed = detector();
        feed_keyed(&mut fixed, &[40, 90], 4, 40);
        let f = fixed.evaluate().unwrap();

        assert!(
            f.tone_stability > d.tone_stability,
            "a fixed alphabet {f:?} must beat a drifting one {d:?}"
        );
        assert!(f.confidence > d.confidence);
    }

    #[test]
    fn irregular_timing_lowers_confidence() {
        let mut regular = detector();
        feed_keyed(&mut regular, &[40, 80], 4, 40);

        let mut irregular = detector();
        let lengths = [1usize, 9, 2, 14, 3, 21, 1, 17];
        for i in 0..40 {
            let bin = if i % 2 == 0 { 40 } else { 80 };
            for _ in 0..lengths[i % lengths.len()] {
                irregular.push(bin, true);
            }
        }

        let a = regular.evaluate().unwrap();
        let b = irregular.evaluate().unwrap();
        assert!(
            a.timing_regularity > b.timing_regularity,
            "regular {a:?} should beat irregular {b:?}"
        );
        assert!(a.confidence > b.confidence);
    }

    #[test]
    fn a_detection_decays_once_the_signal_stops() {
        // The bug this exists to prevent: the detector reported a transmission
        // indefinitely after the signal ended, because symbols were only ever
        // evicted on capacity. Muting the source produced no new symbols, so
        // the stale ones latched the indicator on.
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 4, 40);
        assert!(
            d.evaluate().is_some_and(|r| r.is_present(0.5)),
            "should detect while transmitting"
        );

        // Silence for longer than the history window.
        let quiet_frames = (KEYING_HISTORY_SECONDS / FRAME_SECONDS) as usize + 100;
        for _ in 0..quiet_frames {
            d.push(0, false);
        }

        assert_eq!(d.symbol_count(), 0, "history must age out, not persist");
        assert_not_keying(&d, "a transmission that stopped");
    }

    #[test]
    fn history_covers_a_useful_window() {
        let d = detector();
        assert!(
            (d.history_seconds() - KEYING_HISTORY_SECONDS).abs() < 1.0,
            "history is {} s",
            d.history_seconds()
        );
    }

    #[test]
    fn continuing_transmission_keeps_reporting() {
        // Ageing must not break a signal that is still going.
        let mut d = detector();
        for _ in 0..8 {
            feed_keyed(&mut d, &[40, 80], 4, 20);
            assert!(
                d.evaluate().is_some_and(|r| r.is_present(0.5)),
                "a sustained transmission must stay detected"
            );
        }
    }

    #[test]
    fn reset_clears_history() {
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 4, 40);
        assert!(d.symbol_count() > 0);
        d.reset();
        assert_eq!(d.symbol_count(), 0);
        assert!(d.evaluate().is_none());
    }

    #[test]
    fn history_is_bounded() {
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 2, 5000);
        assert!(
            d.symbol_count() <= 512,
            "history must not grow without limit"
        );
    }

    #[test]
    fn results_are_always_finite() {
        let mut d = detector();
        feed_keyed(&mut d, &[40, 80], 4, 40);
        let r = d.evaluate().unwrap();
        assert!(r.confidence.is_finite());
        assert!(r.symbol_rate_hz.is_finite());
        assert!(r.timing_regularity.is_finite());
        assert!(r.alphabet_purity.is_finite());
        assert!(r.tone_stability.is_finite());
        assert!(r.transitions_per_second.is_finite());
        assert!(r.tones_hz.iter().all(|t| t.is_finite()));
    }
}

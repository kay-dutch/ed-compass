//! On/off keying — "is a single tone being switched on and off deliberately?"
//!
//! A different signal from the one [`super::keying`] hunts. That detector looks
//! for the Thargoid *Probe* tightbeam: several tones, alternating, on a fast
//! clock. Thargoid *Sensor* Morse is the opposite shape — **one** tone,
//! switched on and off, slowly — and the keying detector scores it zero by
//! construction, because its alphabet term requires two distinct tones.
//!
//! Measured from the genuine article (`FinalMessage.wav`, 211 s, lossless):
//!
//! | | |
//! |---|---|
//! | tone | 111.3 Hz; the 60–200 Hz band carries −4.6 dB |
//! | dot | 551 ms, 218 of them |
//! | dash | 1711 ms, 32 of them |
//! | ratio | **3.11** (textbook Morse is 3.00) |
//! | gaps | median 107 ms |
//! | opening tone | 9.0 s — one outlier, far longer than any dash |
//!
//! Three things follow, and each cost a wrong first attempt:
//!
//! * The tone is **below `detect_min_hz`, and far below `keying_min_hz`**. The
//!   low-frequency floor that stops ship rumble triggering the other detectors
//!   is exactly what would hide this one, so this detector owns its own band.
//! * Marks must be clustered, **not split at the midpoint of their range**.
//!   That opening 9-second tone puts 249 marks on one side of the midpoint and
//!   1 on the other, turning a ratio of 3.11 into 13.55.
//! * The clusters are **wide** — dot ±224 ms — so a score that demands tight
//!   clusters rejects the real signal.
//!
//! The scoring deliberately does **not** require a ratio near three. Measured:
//! removing that window changes nothing about the results — the genuine article
//! still scores 1.00, all three field recordings still score nothing — because
//! the frame floor and the balance term were doing the work. So the window came
//! out. Thargoid Sensor Morse is the signal this was built from, but the tool
//! exists to find signals nobody has catalogued, and a detector that insists on
//! 3:1 can only ever confirm what is already known. Any tone keyed into two
//! well-populated, resolvable lengths lights the lamp; the ratio is reported as
//! evidence rather than used as a gate.
//!
//! Detection only. Turning marks into letters, and letters into the coordinate
//! pairs that draw a picture, is a separate job better done on a saved
//! recording than on a live stream.

/// How much history an assessment covers.
///
/// The reference sends roughly one mark per second, so a minute holds plenty
/// while still tracking what is happening now.
pub const MORSE_HISTORY_SECONDS: f32 = 60.0;

/// Shortest run counted as a mark or gap rather than a flicker.
const MIN_RUN_FRAMES: usize = 2;

/// Marks needed before two clusters mean anything.
const MIN_MARKS: usize = 12;

/// A dot must be this many frames long before its duration is a measurement.
///
/// Ambience crossing the on/off threshold produces runs of two or three frames.
/// Cluster those and you get "3 frames versus 9 frames" — a ratio of exactly
/// 3.00, textbook Morse, manufactured entirely by frame quantisation. All three
/// field recordings scored 1.00 that way before this bar existed, while the
/// genuine article's dot is ten frames.
///
/// The same lesson as the keying detector's Nyquist floor: timing measured in a
/// handful of frames describes the instrument, not the audio.
const MIN_DOT_FRAMES: f32 = 6.0;

/// A completed run of frames, on or off.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    on: bool,
    frames: usize,
}

/// What the detector currently believes.
#[derive(Debug, Clone, PartialEq)]
pub struct MorseDetection {
    /// 0..1. Necessary properties combined multiplicatively.
    pub confidence: f32,
    /// Where in the band the marks were loudest.
    pub tone_hz: f32,
    pub dot_seconds: f32,
    pub dash_seconds: f32,
    /// Dash divided by dot. Morse is 3.0; the reference measured 3.11.
    pub ratio: f32,
    pub marks: usize,
    /// Share of marks in the smaller of the two classes.
    pub balance: f32,
}

impl MorseDetection {
    pub fn is_present(&self, threshold: f32) -> bool {
        self.confidence >= threshold
    }
}

/// Watches one narrow low band for a tone being switched on and off.
#[derive(Debug)]
pub struct MorseDetector {
    frame_seconds: f32,
    lo_bin: usize,
    hi_bin: usize,
    bin_hz: f32,
    /// How far above the band's own quiet level counts as "on", in dB.
    on_threshold_db: f32,
    runs: std::collections::VecDeque<Run>,
    current: Option<Run>,
    /// Slowly-learned quiet level, so a loud ship does not read as one endless
    /// mark. Falls quickly and rises slowly, so a long mark cannot drag the
    /// floor up and silence its own detection.
    floor_db: f32,
    /// Bin that was loudest while marks were on, for reporting.
    loud_bin: usize,
}

impl MorseDetector {
    pub fn new(
        frame_seconds: f32,
        sample_rate: u32,
        fft_size: usize,
        lo_hz: f32,
        hi_hz: f32,
    ) -> Self {
        let bin_hz = sample_rate as f32 / fft_size as f32;
        let lo_bin = (lo_hz / bin_hz).floor().max(1.0) as usize;
        let hi_bin = ((hi_hz / bin_hz).ceil() as usize).max(lo_bin + 1);
        Self {
            frame_seconds,
            lo_bin,
            hi_bin,
            bin_hz,
            on_threshold_db: 6.0,
            runs: std::collections::VecDeque::new(),
            current: None,
            floor_db: f32::NAN,
            loud_bin: lo_bin,
        }
    }

    /// Feed one frame's spectrum, in dB per bin.
    pub fn push(&mut self, spectrum_db: &[f32]) {
        let hi = self.hi_bin.min(spectrum_db.len());
        if self.lo_bin >= hi {
            return;
        }
        let (mut peak, mut peak_bin) = (f32::NEG_INFINITY, self.lo_bin);
        for (offset, value) in spectrum_db[self.lo_bin..hi].iter().enumerate() {
            if *value > peak {
                peak = *value;
                peak_bin = self.lo_bin + offset;
            }
        }
        if !peak.is_finite() {
            return;
        }

        if self.floor_db.is_nan() {
            self.floor_db = peak;
        } else if peak < self.floor_db {
            self.floor_db += (peak - self.floor_db) * 0.25;
        } else {
            self.floor_db += (peak - self.floor_db) * 0.002;
        }

        let on = peak - self.floor_db >= self.on_threshold_db;
        if on {
            self.loud_bin = peak_bin;
        }

        match &mut self.current {
            Some(run) if run.on == on => run.frames += 1,
            Some(run) => {
                let finished = *run;
                self.current = Some(Run { on, frames: 1 });
                if finished.frames >= MIN_RUN_FRAMES {
                    self.runs.push_back(finished);
                }
            }
            None => self.current = Some(Run { on, frames: 1 }),
        }

        let capacity = (MORSE_HISTORY_SECONDS / self.frame_seconds) as usize;
        let mut held: usize = self.runs.iter().map(|r| r.frames).sum();
        while held > capacity {
            match self.runs.pop_front() {
                Some(r) => held -= r.frames,
                None => break,
            }
        }
    }

    /// The current assessment, if there is enough evidence for one.
    pub fn evaluate(&self) -> Option<MorseDetection> {
        let mut marks: Vec<f32> = self
            .runs
            .iter()
            .filter(|r| r.on)
            .map(|r| r.frames as f32 * self.frame_seconds)
            .collect();
        if marks.len() < MIN_MARKS {
            return None;
        }
        marks.sort_by(f32::total_cmp);

        let (dot, dash, short, long) = cluster_two(&marks)?;
        if dot <= 0.0 || dash <= dot {
            return None;
        }
        // Reject timing the analysis cannot resolve. See `MIN_DOT_FRAMES`.
        if dot < MIN_DOT_FRAMES * self.frame_seconds {
            return None;
        }
        let ratio = dash / dot;

        // Morse is 3:1. The window is wide because an envelope threshold
        // shortens marks and the reference itself measured 3.11, but it closes
        // on anything that is not two clearly different lengths.
        let ratio_term = if ratio >= 1.5 { 1.0 } else { 0.0 };

        // Both lengths must be present in quantity. One dash among fifty dots
        // is a glitch, not an alphabet. The reference runs 32 dashes to 218
        // dots — 13% — so the bar sits below that.
        let balance = short.min(long) as f32 / marks.len() as f32;
        let balance_term = (balance / 0.08).clamp(0.0, 1.0);

        // Deliberately *not* a tightness term. The genuine article's clusters
        // are wide — dot ±224 ms — and demanding narrow ones rejects it.
        let confidence = (ratio_term * balance_term).clamp(0.0, 1.0);

        Some(MorseDetection {
            confidence,
            tone_hz: self.loud_bin as f32 * self.bin_hz,
            dot_seconds: dot,
            dash_seconds: dash,
            ratio,
            marks: marks.len(),
            balance,
        })
    }
}

/// Split sorted durations into two classes, returning their medians and sizes.
///
/// Two-medians seeded at the 10th and 90th percentiles. Every part of that is
/// forced by the reference recording:
///
/// * **Not the range.** Its single 9-second opening tone puts 249 marks on one
///   side of the midpoint and 1 on the other — a ratio of 13.55 instead of 3.11.
/// * **Not the quartiles.** 218 of its 251 marks are dots, so p25 and p75 land
///   on the *same* value and the two classes never separate at all.
/// * **Medians, not means.** The outlier drags a mean; it cannot drag a median
///   past the mass of real dashes.
///
/// Equal seeds mean every mark is the same length, which is a metronome rather
/// than an alphabet, and returns `None`.
fn cluster_two(sorted: &[f32]) -> Option<(f32, f32, usize, usize)> {
    if sorted.len() < 2 {
        return None;
    }
    let at = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    let mut centres = [at(0.10), at(0.90)];
    if centres[0] == centres[1] {
        return None;
    }

    let mut labels = vec![0usize; sorted.len()];
    for _ in 0..50 {
        let mut moved = false;
        for (i, value) in sorted.iter().enumerate() {
            let pick = usize::from((value - centres[1]).abs() < (value - centres[0]).abs());
            if labels[i] != pick {
                labels[i] = pick;
                moved = true;
            }
        }
        for (class, centre) in centres.iter_mut().enumerate() {
            let mut members: Vec<f32> = sorted
                .iter()
                .zip(&labels)
                .filter(|(_, l)| **l == class)
                .map(|(v, _)| *v)
                .collect();
            if !members.is_empty() {
                members.sort_by(f32::total_cmp);
                *centre = members[members.len() / 2];
            }
        }
        if !moved {
            break;
        }
    }

    let mut low: Vec<f32> = Vec::new();
    let mut high: Vec<f32> = Vec::new();
    for (value, label) in sorted.iter().zip(&labels) {
        if *label == 0 { &mut low } else { &mut high }.push(*value);
    }
    if low.is_empty() || high.is_empty() {
        return None;
    }
    let median = |v: &[f32]| v[v.len() / 2];
    let (a, b) = (median(&low), median(&high));
    let (dot, dash) = if a <= b { (a, b) } else { (b, a) };
    Some((dot, dash, low.len(), high.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_SECONDS: f32 = 2048.0 / 48_000.0; // 42.7 ms, the real hop
    const SAMPLE_RATE: u32 = 48_000;
    const FFT: usize = 4096;

    fn detector() -> MorseDetector {
        MorseDetector::new(FRAME_SECONDS, SAMPLE_RATE, FFT, 60.0, 200.0)
    }

    fn frame(loud: bool) -> Vec<f32> {
        let mut v = vec![-90.0f32; FFT / 2 + 1];
        let bin = (111.3 / (SAMPLE_RATE as f32 / FFT as f32)) as usize;
        v[bin] = if loud { -30.0 } else { -88.0 };
        v
    }

    fn send(d: &mut MorseDetector, pattern: &[(bool, f32)]) {
        for (on, seconds) in pattern {
            let frames = (seconds / FRAME_SECONDS).round().max(1.0) as usize;
            for _ in 0..frames {
                d.push(&frame(*on));
            }
        }
    }

    /// The reference's own shape: 551 ms dots, 1711 ms dashes, a 9 s opening
    /// tone, and roughly one dash to seven dots.
    fn reference_message() -> Vec<(bool, f32)> {
        let (dot, dash, gap) = (0.551, 1.711, 0.107);
        let mut p = vec![(false, 2.0), (true, 9.0), (false, gap)];
        for i in 0..28 {
            p.push((true, if i % 7 == 3 { dash } else { dot }));
            p.push((false, gap));
        }
        p
    }

    #[test]
    fn nothing_is_reported_before_there_is_evidence() {
        let mut d = detector();
        send(&mut d, &[(false, 2.0), (true, 0.55), (false, 0.1)]);
        assert!(d.evaluate().is_none(), "one mark is not a message");
    }

    #[test]
    fn the_reference_shape_is_detected() {
        let mut d = detector();
        send(&mut d, &reference_message());

        let r = d.evaluate().expect("a keyed message must be reported");
        assert!(r.is_present(0.5), "the real signal must score: {r:?}");
        assert!(
            (r.ratio - 3.0).abs() < 1.2,
            "dash/dot should land near three, got {:.2}",
            r.ratio
        );
    }

    /// An unknown signal with an un-Morse-like ratio must still be found.
    ///
    /// The point of the tool is signals nobody has catalogued. A detector tuned
    /// to 3:1 can only confirm what is already known, so the ratio is evidence,
    /// not a gate.
    #[test]
    fn keying_at_an_unfamiliar_ratio_is_still_reported() {
        let mut d = detector();
        let (short, long, gap) = (0.5, 4.5, 0.2); // 9:1 — nothing like Morse
        let mut p = vec![(false, 1.0)];
        for i in 0..30 {
            p.push((true, if i % 4 == 0 { long } else { short }));
            p.push((false, gap));
        }
        send(&mut d, &p);

        let r = d
            .evaluate()
            .expect("an unfamiliar but deliberate keying must be reported");
        assert!(r.is_present(0.5), "unknown ratios are the point: {r:?}");
        assert!(
            r.ratio > 6.0,
            "and the ratio is reported as it is, {:.1}",
            r.ratio
        );
    }

    /// The bug the reference recording exposed, kept as a test.
    #[test]
    fn one_long_opening_tone_does_not_wreck_the_ratio() {
        // Splitting marks at the midpoint of their range puts everything on one
        // side of a 9-second outlier and turns a ratio of 3.11 into 13.55.
        // The reference's own distribution: 218 dots, 32 dashes, one long
        // opening tone.
        let mut sorted: Vec<f32> = std::iter::repeat_n(0.551, 218)
            .chain(std::iter::repeat_n(1.711, 32))
            .chain(std::iter::once(9.0))
            .collect();
        sorted.sort_by(f32::total_cmp);

        let (dot, dash, _, _) = cluster_two(&sorted).expect("two classes");
        let ratio = dash / dot;
        assert!(
            (ratio - 3.1).abs() < 0.6,
            "clustering must survive the opening tone, got ratio {ratio:.2}"
        );
    }

    /// The whole reason this module exists.
    #[test]
    fn a_single_tone_is_enough_unlike_the_fsk_detector() {
        let mut d = detector();
        send(&mut d, &reference_message());
        let r = d.evaluate().expect("reported");
        assert!(
            r.confidence > 0.0,
            "one tone switched on and off is the entire signal shape here"
        );
    }

    /// Frame-quantised flicker is not Morse, however good its ratio looks.
    ///
    /// All three field recordings scored 1.00 before this was enforced: their
    /// "dots" were three frames and their "dashes" nine, giving a ratio of
    /// exactly 3.00 out of nothing but quantisation.
    #[test]
    fn marks_a_few_frames_long_are_refused() {
        let mut d = detector();
        let mut p = vec![(false, 1.0)];
        for i in 0..40 {
            let frames = if i % 5 == 0 { 9.0 } else { 3.0 };
            p.push((true, frames * FRAME_SECONDS));
            p.push((false, 3.0 * FRAME_SECONDS));
        }
        send(&mut d, &p);

        match d.evaluate() {
            None => {}
            Some(r) => assert!(
                !r.is_present(0.5),
                "a 3-frame dot is the frame rate, not a dot: {r:?}"
            ),
        }
    }

    #[test]
    fn a_steady_tone_is_not_morse() {
        let mut d = detector();
        send(&mut d, &[(false, 1.0), (true, 40.0)]);
        match d.evaluate() {
            None => {}
            Some(r) => assert!(!r.is_present(0.5), "a held note is not keying: {r:?}"),
        }
    }

    #[test]
    fn marks_of_one_length_are_not_morse() {
        // A metronome: evenly spaced identical blips, no dot/dash alphabet.
        let mut d = detector();
        let mut p = vec![(false, 1.0)];
        for _ in 0..24 {
            p.push((true, 0.55));
            p.push((false, 0.4));
        }
        send(&mut d, &p);
        match d.evaluate() {
            None => {}
            Some(r) => assert!(
                !r.is_present(0.5),
                "one mark length carries no alphabet: {r:?}"
            ),
        }
    }

    #[test]
    fn a_lone_dash_among_dots_is_not_an_alphabet() {
        let mut d = detector();
        let mut p = vec![(false, 1.0)];
        for i in 0..30 {
            p.push((true, if i == 15 { 1.7 } else { 0.55 }));
            p.push((false, 0.15));
        }
        send(&mut d, &p);
        match d.evaluate() {
            None => {}
            Some(r) => assert!(
                !r.is_present(0.5),
                "one dash in thirty marks is a glitch, not a symbol: {r:?}"
            ),
        }
    }
}

//! Triggered capture to disk.
//!
//! Nothing is written during normal operation. The raw PCM ring is the pre-roll,
//! held in memory, and only a detection that scores above `trigger_score` causes
//! anything to reach the disk. Three guard rails keep an unattended overnight
//! session from filling the drive: a cooldown between captures, an hourly cap,
//! and a total budget enforced by [`crate::retention`], which evicts the least
//! valuable captures rather than simply the oldest.
//!
//! Every capture is multichannel audio plus a JSON sidecar carrying the system,
//! the coordinates, the detector scores, and the bearing — the sidecar is the
//! actual research record, it is four thousandths of one percent of the size,
//! and it is kept forever even when its audio is evicted.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::analysis::direction::DirectionMethod;
use crate::audio::StreamFormat;
use crate::config::Config;
use crate::journal::GameState;
use crate::pipeline::Detection;

/// The JSON written next to every WAV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSidecar {
    pub captured_utc: String,
    pub audio_file: String,
    /// Set when the disk budget reclaimed the recording. The observation below
    /// is still good; only the waveform is gone.
    #[serde(default)]
    pub audio_evicted: bool,

    // Where the commander was.
    pub star_system: Option<String>,
    pub star_pos: Option<[f64; 3]>,
    pub body: Option<String>,
    pub music_track: Option<String>,
    pub in_supercruise: bool,

    // What the stream was.
    pub sample_rate: u32,
    pub channels: usize,
    pub channel_layout: String,
    pub device: String,

    // What was detected.
    pub start_seconds: f64,
    pub duration_seconds: f32,
    pub low_hz: f32,
    pub high_hz: f32,
    pub peak_hz: f32,
    pub peak_excess_db: f32,
    pub mean_excess_db: f32,
    pub drift_hz: f32,
    pub mean_flatness: f32,
    pub score: f32,

    // Where it came from.
    pub azimuth_deg: Option<f32>,
    pub azimuth_confidence: f32,
    pub azimuth_method: String,
    pub front_back_ambiguous: bool,

    /// True when a timeline gap fell inside the event, so its structure — and
    /// any period derived from it — is unreliable.
    pub spans_gap: bool,
    pub period_seconds: Option<f32>,
    pub period_confidence: Option<f32>,
    pub matches_landscape: bool,
}

/// Sidecar for a capture triggered by a primary detector or by hand, where
/// there is no novelty event to describe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectorSidecar {
    pub captured_utc: String,
    pub audio_file: String,
    /// Set when the disk budget reclaimed the recording.
    #[serde(default)]
    pub audio_evicted: bool,
    /// What caused the capture: "keying", "structure", or "manual".
    pub reason: String,

    pub star_system: Option<String>,
    pub star_pos: Option<[f64; 3]>,
    pub body: Option<String>,
    pub music_track: Option<String>,
    pub in_supercruise: bool,

    pub sample_rate: u32,
    pub channels: usize,
    pub device: String,
    pub seconds: f32,

    // Whatever the detectors said at the moment of capture.
    pub keying_confidence: Option<f32>,
    pub keying_tones_hz: Vec<f32>,
    pub keying_symbol_rate_hz: Option<f32>,
    pub structure_score: f32,
    /// What the *folded* cycle scored, and the fold it came from.
    ///
    /// Recorded separately because the two can disagree completely, and when they
    /// do the fold is the one that matters. A capture triggered by the fold used
    /// to record `structure_score: 0.0` — the live scan's opinion — which made
    /// the file look self-contradictory to anyone reading it afterwards.
    pub folded_structure_score: f32,
    pub folded_period_seconds: Option<f32>,
    pub folded_cycles: Option<f32>,
    pub structure_coherence: f32,
    pub structure_sparsity: f32,
    pub structure_orientation_diversity: f32,
    pub period_seconds: Option<f32>,
    pub period_confidence: Option<f32>,
    pub matches_landscape: bool,
}

/// Why a trigger was declined, for the log and the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerDecision {
    Accept,
    BelowThreshold,
    CoolingDown,
    HourlyLimit,
}

impl TriggerDecision {
    pub fn accepted(self) -> bool {
        self == TriggerDecision::Accept
    }
}

/// Everything a triggered capture needs, grouped so the call site reads as one
/// intent rather than nine positional arguments.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest<'a> {
    pub detection: &'a Detection,
    /// Interleaved audio, matching `format.channels`.
    pub samples: &'a [f32],
    pub format: &'a StreamFormat,
    pub device: &'a str,
    pub game: &'a GameState,
    pub period: Option<&'a crate::analysis::periodicity::PeriodicityResult>,
    pub timestamp: &'a str,
}

pub struct CaptureWriter {
    dir: PathBuf,
    trigger_score: f32,
    cooldown: Duration,
    max_per_hour: u32,
    budget_bytes: u64,
    protect_best: usize,
    /// "flac" or "wav".
    format_name: String,
    /// When each recent capture happened, oldest first.
    recent: std::collections::VecDeque<Instant>,
    captures_written: u64,
}

impl CaptureWriter {
    pub fn new(dir: impl Into<PathBuf>, cfg: &Config) -> Self {
        Self {
            dir: dir.into(),
            trigger_score: cfg.trigger_score,
            cooldown: Duration::from_secs_f32(cfg.capture_cooldown_seconds.max(0.0)),
            max_per_hour: cfg.max_captures_per_hour,
            budget_bytes: cfg.disk_budget_mb.saturating_mul(1_048_576),
            protect_best: cfg.protect_best_captures,
            format_name: cfg.capture_format.clone(),
            recent: std::collections::VecDeque::new(),
            captures_written: 0,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn captures_written(&self) -> u64 {
        self.captures_written
    }

    /// Would this detection be captured right now?
    ///
    /// `now` is passed in rather than read from the clock so the rate limiting
    /// is testable without sleeping.
    pub fn evaluate(&mut self, score: f32, now: Instant) -> TriggerDecision {
        if score < self.trigger_score {
            return TriggerDecision::BelowThreshold;
        }
        let hour_ago = now.checked_sub(Duration::from_secs(3600));
        if let Some(cutoff) = hour_ago {
            while self.recent.front().is_some_and(|t| *t < cutoff) {
                self.recent.pop_front();
            }
        }
        if let Some(last) = self.recent.back()
            && now.duration_since(*last) < self.cooldown
        {
            return TriggerDecision::CoolingDown;
        }
        if self.recent.len() as u32 >= self.max_per_hour {
            return TriggerDecision::HourlyLimit;
        }
        TriggerDecision::Accept
    }

    /// Record that a capture was taken, for rate-limiting purposes.
    fn note_capture(&mut self, now: Instant) {
        self.recent.push_back(now);
        self.captures_written += 1;
    }

    /// Write a detection's audio and sidecar.
    pub fn write(&mut self, req: CaptureRequest<'_>, now: Instant) -> Result<PathBuf> {
        let CaptureRequest {
            detection,
            samples,
            format,
            device,
            game,
            period,
            timestamp,
        } = req;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating capture directory {}", self.dir.display()))?;

        let stem = format!(
            "{}_{}",
            sanitize(timestamp),
            sanitize(game.star_system.as_deref().unwrap_or("unknown"))
        );
        let wav_path = self.dir.join(format!("{stem}.{}", self.format_name));
        let json_path = self.dir.join(format!("{stem}.json"));

        write_audio(&wav_path, samples, format)?;

        let e = &detection.event;
        let d = &detection.direction;
        let sidecar = CaptureSidecar {
            captured_utc: timestamp.to_owned(),
            audio_file: wav_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            audio_evicted: false,
            star_system: game.star_system.clone(),
            star_pos: game.star_pos,
            body: game.body.clone(),
            music_track: game.music_track.clone(),
            in_supercruise: game.in_supercruise,
            sample_rate: format.sample_rate,
            channels: format.channels,
            channel_layout: format.layout_name().to_owned(),
            device: device.to_owned(),
            start_seconds: e.start_seconds,
            duration_seconds: e.duration_seconds,
            low_hz: e.low_hz,
            high_hz: e.high_hz,
            peak_hz: e.peak_hz,
            peak_excess_db: e.peak_excess_db,
            mean_excess_db: e.mean_excess_db,
            drift_hz: e.drift_hz,
            mean_flatness: e.mean_flatness,
            score: e.score,
            azimuth_deg: d.is_usable().then_some(d.azimuth_deg),
            azimuth_confidence: d.confidence,
            azimuth_method: match d.method {
                DirectionMethod::StereoPanLaw => "stereo-pan-law",
                DirectionMethod::SurroundVector => "surround-vector",
                DirectionMethod::Insufficient => "none",
            }
            .to_owned(),
            front_back_ambiguous: d.front_back_ambiguous,
            spans_gap: detection.spans_gap,
            period_seconds: period.map(|p| p.period_seconds),
            period_confidence: period.map(|p| p.confidence),
            matches_landscape: period
                .is_some_and(|p| crate::analysis::periodicity::matches_landscape(p, 2.0)),
        };
        std::fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)
            .with_context(|| format!("writing {}", json_path.display()))?;

        self.note_capture(now);
        self.enforce_budget();
        log::info!(
            "captured {} ({:.1} s, score {:.2}) to {}",
            e.peak_hz,
            e.duration_seconds,
            e.score,
            wav_path.display()
        );
        Ok(wav_path)
    }

    /// Write an arbitrary span of audio with a detector sidecar.
    ///
    /// Used when a primary detector fires or the commander presses "keep" —
    /// neither of which has a novelty event to describe, but both of which are
    /// exactly the moments worth keeping.
    pub fn write_span(
        &mut self,
        samples: &[f32],
        format: &StreamFormat,
        device: &str,
        game: &GameState,
        sidecar: DetectorSidecar,
        now: Instant,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating capture directory {}", self.dir.display()))?;

        let stem = format!(
            "{}_{}_{}",
            sanitize(&sidecar.captured_utc),
            sanitize(&sidecar.reason),
            sanitize(game.star_system.as_deref().unwrap_or("unknown"))
        );
        let wav_path = self.dir.join(format!("{stem}.{}", self.format_name));

        write_audio(&wav_path, samples, format)?;

        let mut sidecar = sidecar;
        sidecar.audio_file = wav_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        sidecar.device = device.to_owned();
        sidecar.sample_rate = format.sample_rate;
        sidecar.channels = format.channels;
        std::fs::write(
            self.dir.join(format!("{stem}.json")),
            serde_json::to_string_pretty(&sidecar)?,
        )
        .with_context(|| format!("writing the sidecar for {}", wav_path.display()))?;

        self.note_capture(now);
        self.enforce_budget();
        log::info!(
            "kept {:.1} s ({}) to {}",
            sidecar.seconds,
            sidecar.reason,
            wav_path.display()
        );
        Ok(wav_path)
    }

    /// Bring the capture directory back within its budget.
    ///
    /// Delegates to [`crate::retention`], which evicts by value rather than by
    /// age and never deletes a sidecar. A failure there is logged, not
    /// propagated: losing a capture to a full disk is bad, but abandoning the
    /// hunt because one delete failed is worse.
    pub fn enforce_budget(&self) {
        crate::retention::enforce(
            &self.dir,
            &crate::retention::Policy {
                budget_bytes: self.budget_bytes,
                protect_best: self.protect_best,
            },
        );
    }

    /// Bytes of captured audio currently on disk.
    pub fn bytes_on_disk(&self) -> u64 {
        crate::retention::audio_bytes(&self.dir)
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }
}

/// Write the samples in whichever container the configuration asked for.
///
/// FLAC is chosen by the extension rather than by a flag, so the file always
/// says what it is.
fn write_audio(path: &Path, samples: &[f32], format: &StreamFormat) -> Result<()> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("flac") => write_flac(path, samples, format),
        _ => write_wav(path, samples, format),
    }
}

fn write_wav(path: &Path, samples: &[f32], format: &StreamFormat) -> Result<()> {
    let spec = hound::WavSpec {
        channels: format.channels as u16,
        sample_rate: format.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for s in samples {
        writer.write_sample(*s).context("writing audio samples")?;
    }
    writer.finalize().context("finalizing the WAV file")?;
    Ok(())
}

/// Encode to FLAC, roughly halving the size of a capture.
///
/// FLAC stores integers, so the float stream is quantised to 24 bits first.
/// That is a quantisation, not a rounding to nothing: 24 bits is 144 dB of
/// range, against detections that sit some 20 dB above a background which is
/// itself far above the noise floor. The compression after that point is
/// lossless — decoding returns exactly these integers.
fn write_flac(path: &Path, samples: &[f32], format: &StreamFormat) -> Result<()> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    const BITS: usize = 24;
    const FULL_SCALE: f32 = 8_388_607.0; // 2^23 - 1

    let ints: Vec<i32> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * FULL_SCALE).round() as i32)
        .collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow::anyhow!("the FLAC encoder settings are invalid: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(
        &ints,
        format.channels,
        BITS,
        format.sample_rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow::anyhow!("encoding FLAC: {e:?}"))?;

    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow::anyhow!("serialising FLAC: {e:?}"))?;
    std::fs::write(path, sink.as_slice()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Make a string safe for a filename on Windows.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::direction::DirectionEstimate;
    use crate::analysis::novelty::DetectionEvent;
    use crate::audio::SampleFormat;
    use crate::audio::format::MASK_STEREO;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ed-compass-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn detection(score: f32) -> Detection {
        Detection {
            event: DetectionEvent {
                start_frame: 10,
                end_frame: 100,
                start_seconds: 1.0,
                duration_seconds: 4.0,
                low_hz: 800.0,
                high_hz: 1200.0,
                peak_hz: 1000.0,
                low_bin: 20,
                high_bin: 30,
                peak_excess_db: 18.0,
                mean_excess_db: 12.0,
                drift_hz: 25.0,
                mean_flatness: 0.05,
                score,
            },
            direction: DirectionEstimate {
                azimuth_deg: -38.0,
                confidence: 0.81,
                method: DirectionMethod::SurroundVector,
                front_back_ambiguous: false,
            },
            start_sample: 5_120,
            end_sample: 51_200,
            spans_gap: false,
        }
    }

    fn game() -> GameState {
        GameState {
            star_system: Some("Stuemeae JM-W c1-5825".into()),
            star_pos: Some([0.0, 0.0, 25899.0]),
            system_address: Some(2_724_879_894_859),
            body: None,
            music_track: Some("Exploration".into()),
            in_supercruise: true,
            game_running: true,
            last_event_utc: Some("3311-08-13T14:22:07Z".into()),
        }
    }

    fn format() -> StreamFormat {
        StreamFormat::new(48_000, 2, MASK_STEREO, SampleFormat::F32)
    }

    #[test]
    fn a_weak_detection_never_reaches_the_disk() {
        let mut cfg = Config::default();
        cfg.trigger_score = 0.6;
        let mut w = CaptureWriter::new(temp_dir("weak"), &cfg);
        assert_eq!(
            w.evaluate(0.59, Instant::now()),
            TriggerDecision::BelowThreshold
        );
        assert!(w.evaluate(0.6, Instant::now()).accepted());
    }

    #[test]
    fn the_cooldown_blocks_a_burst_of_triggers() {
        let mut cfg = Config::default();
        cfg.capture_cooldown_seconds = 60.0;
        let mut w = CaptureWriter::new(temp_dir("cooldown"), &cfg);
        let t0 = Instant::now();

        assert!(w.evaluate(0.9, t0).accepted());
        w.note_capture(t0);
        assert_eq!(
            w.evaluate(0.9, t0 + Duration::from_secs(30)),
            TriggerDecision::CoolingDown
        );
        assert!(w.evaluate(0.9, t0 + Duration::from_secs(61)).accepted());
    }

    #[test]
    fn the_hourly_cap_holds_and_then_rolls_off() {
        let mut cfg = Config::default();
        cfg.capture_cooldown_seconds = 0.0;
        cfg.max_captures_per_hour = 3;
        let mut w = CaptureWriter::new(temp_dir("hourly"), &cfg);
        let t0 = Instant::now();

        for i in 0..3 {
            let t = t0 + Duration::from_secs(i * 60);
            assert!(
                w.evaluate(0.9, t).accepted(),
                "capture {i} should be allowed"
            );
            w.note_capture(t);
        }
        assert_eq!(
            w.evaluate(0.9, t0 + Duration::from_secs(200)),
            TriggerDecision::HourlyLimit
        );
        // Once the first falls out of the window, there is room again.
        assert!(w.evaluate(0.9, t0 + Duration::from_secs(3601)).accepted());
    }

    #[test]
    fn writes_a_wav_and_a_sidecar_that_round_trips() {
        let dir = temp_dir("write");
        let mut cfg = Config::default();
        cfg.disk_budget_mb = 1024;
        cfg.capture_format = "wav".into();
        let mut w = CaptureWriter::new(&dir, &cfg);

        let format = format();
        let samples: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let v = (i as f32 * 0.01).sin() * 0.5;
                [v, v * 0.5]
            })
            .collect();

        let path = w
            .write(
                CaptureRequest {
                    detection: &detection(0.82),
                    samples: &samples,
                    format: &format,
                    device: "Speakers (Realtek)",
                    game: &game(),
                    period: None,
                    timestamp: "3311-08-13T14:22:07Z",
                },
                Instant::now(),
            )
            .unwrap();

        assert!(path.exists());
        assert_eq!(w.captures_written(), 1);
        assert_eq!(
            path.extension().unwrap(),
            "wav",
            "this test pinned the container"
        );

        // The WAV is readable and has the right shape.
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.len() as usize, samples.len());

        let json = std::fs::read_to_string(path.with_extension("json")).unwrap();
        let sidecar: CaptureSidecar = serde_json::from_str(&json).unwrap();
        assert_eq!(
            sidecar.star_system.as_deref(),
            Some("Stuemeae JM-W c1-5825")
        );
        assert_eq!(sidecar.star_pos, Some([0.0, 0.0, 25899.0]));
        assert_eq!(sidecar.azimuth_deg, Some(-38.0));
        assert_eq!(sidecar.azimuth_method, "surround-vector");
        assert_eq!(sidecar.channel_layout, "stereo");
        assert!(!sidecar.matches_landscape);
        assert!((sidecar.peak_hz - 1000.0).abs() < 1e-6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unusable_bearing_is_recorded_as_absent_not_as_zero() {
        let dir = temp_dir("nobearing");
        let mut w = CaptureWriter::new(&dir, &Config::default());
        let mut d = detection(0.9);
        d.direction = DirectionEstimate::insufficient();

        let path = w
            .write(
                CaptureRequest {
                    detection: &d,
                    samples: &[0.0; 200],
                    format: &format(),
                    device: "dev",
                    game: &GameState::default(),
                    period: None,
                    timestamp: "3311-01-01T00:00:00Z",
                },
                Instant::now(),
            )
            .unwrap();
        let json = std::fs::read_to_string(path.with_extension("json")).unwrap();
        let sidecar: CaptureSidecar = serde_json::from_str(&json).unwrap();
        assert_eq!(
            sidecar.azimuth_deg, None,
            "0 deg is a bearing; absent is not"
        );
        assert_eq!(sidecar.azimuth_method, "none");
        assert_eq!(sidecar.star_system, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disk_budget_evicts_oldest_first_with_its_sidecar() {
        let dir = temp_dir("budget");
        std::fs::create_dir_all(&dir).unwrap();

        // Three 4 KB captures against a budget that fits one. Their scores are
        // deliberately *inverted* against their ages: the oldest is the best.
        for (i, score) in [(0, 0.95), (1, 0.40), (2, 0.10)] {
            let wav = dir.join(format!("cap{i}.wav"));
            std::fs::write(&wav, vec![0u8; 4096]).unwrap();
            std::fs::write(
                wav.with_extension("json"),
                format!("{{\"score\": {score}, \"star_system\": \"Orrere\"}}"),
            )
            .unwrap();
            // Distinct mtimes so the ordering is unambiguous.
            std::thread::sleep(Duration::from_millis(15));
        }

        let mut cfg = Config::default();
        cfg.disk_budget_mb = 0; // set the raw byte budget below instead
        cfg.protect_best_captures = 0;
        let mut w = CaptureWriter::new(&dir, &cfg);
        w.budget_bytes = 5000;
        w.enforce_budget();

        assert!(
            dir.join("cap0.wav").exists(),
            "the strongest capture must survive even though it is the oldest"
        );
        assert!(!dir.join("cap2.wav").exists(), "the weakest goes first");
        assert!(!dir.join("cap1.wav").exists(), "then the next weakest");

        // Every record survives its recording, and says so.
        for i in 0..3 {
            let json = dir.join(format!("cap{i}.json"));
            assert!(json.exists(), "sidecar {i} must never be deleted");
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
            assert_eq!(v["star_system"], "Orrere");
            assert_eq!(
                v.get("audio_evicted")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
                i != 0,
                "sidecar {i} should be marked evicted only if its audio went"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flac_capture_round_trips_through_our_own_reader() {
        let dir = temp_dir("flac");
        let mut cfg = Config::default();
        cfg.trigger_score = 0.0;
        cfg.capture_cooldown_seconds = 0.0;
        cfg.capture_format = "flac".into();
        let mut w = CaptureWriter::new(&dir, &cfg);

        let format = format();
        // A tone, so a silent or scrambled decode cannot pass.
        let samples: Vec<f32> = (0..format.sample_rate as usize * 2 * 2)
            .map(|i| {
                let t = (i / 2) as f32 / format.sample_rate as f32;
                (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
            })
            .collect();

        let path = w
            .write(
                CaptureRequest {
                    detection: &detection(0.9),
                    samples: &samples,
                    format: &format,
                    device: "Speakers",
                    game: &game(),
                    period: None,
                    timestamp: "3311-08-13T14:22:07Z",
                },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(path.extension().unwrap(), "flac");

        // Smaller than the WAV it replaces: that is the entire reason for it.
        let flac_bytes = std::fs::metadata(&path).unwrap().len();
        let wav_bytes = (samples.len() * 4) as u64;
        assert!(
            flac_bytes < wav_bytes,
            "FLAC ({flac_bytes}) should be smaller than raw float ({wav_bytes})"
        );

        // And the tool can read its own captures back.
        let mut decoded = crate::audio::file_input::load(&path).expect("decoding our own FLAC");
        assert_eq!(decoded.format().channels, 2);
        assert_eq!(decoded.format().sample_rate, 48_000);
        assert_eq!(
            decoded.total_frames(),
            samples.len() / 2,
            "every frame must survive the round trip"
        );
        let mut buf = Vec::new();
        decoded.render(decoded.total_frames(), &mut buf);
        let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            peak > 0.4,
            "the decoded audio must carry the tone, got peak {peak}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_budget_disables_eviction() {
        let dir = temp_dir("nobudget");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.wav"), vec![0u8; 8192]).unwrap();

        let mut cfg = Config::default();
        cfg.disk_budget_mb = 0;
        let w = CaptureWriter::new(&dir, &cfg);
        w.enforce_budget();
        assert!(dir.join("a.wav").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filenames_survive_awkward_system_names() {
        assert_eq!(sanitize("Stuemeae JM-W c1-5825"), "Stuemeae-JM-W-c1-5825");
        assert_eq!(sanitize("3311-08-13T14:22:07Z"), "3311-08-13T14-22-07Z");
        assert_eq!(
            sanitize("Col 285 Sector ZZ-Y b1/2"),
            "Col-285-Sector-ZZ-Y-b1-2"
        );
        assert!(!sanitize("a\\b:c*d?e").contains(['\\', ':', '*', '?']));
    }
}

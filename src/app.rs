//! Application runtime: owns the capture source, the analysis engine, the
//! journal watcher, and the capture writer, and glues them together.
//!
//! Deliberately free of UI: `pump()` drains whatever capture has produced and
//! returns; the caller decides how often to call it and when to take a snapshot.
//! That is what keeps the display rate independent of the capture rate, and it
//! lets the headless mode and the GUI share one implementation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, TryRecvError};

use crate::audio::StreamFormat;
use crate::audio::capture::{CaptureHandle, CaptureMessage};
use crate::capture_writer::{CaptureWriter, DetectorSidecar, TriggerDecision};

/// What the recordings and exports are costing, for the control panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    pub capture_bytes: u64,
    pub capture_budget: u64,
    pub export_bytes: u64,
    pub export_budget: u64,
    /// Observations held, including those whose audio has been evicted. This is
    /// the number that only ever goes up.
    pub records: usize,
}
use crate::config::Config;
use crate::journal::{GameState, JournalWatcher};
use crate::pipeline::{AnalysisEngine, AnalysisSnapshot, Detection};

/// How far from 109.5 s still counts as the Landscape Signal.
///
/// The tool measures the reference at 109.67 s, so a couple of seconds is
/// generous without being loose.
/// How recently something must have been detected for SIGNAL to be lit.
///
/// Long enough to catch your eye after it has passed, short enough that the lamp
/// still describes the present.
const SIGNAL_RECENCY_SECONDS: f64 = 15.0;

const LANDSCAPE_TOLERANCE_SECONDS: f32 = 2.0;

/// A detection as shown in the event list.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub detection: Detection,
    pub star_system: Option<String>,
    pub star_pos: Option<[f64; 3]>,
    pub timestamp: String,
    pub captured_to: Option<PathBuf>,
    pub decision: TriggerDecision,
}

/// A detection waiting for its post-roll to arrive before being written.
struct PendingCapture {
    detection: Detection,
    /// Absolute frame at which enough post-roll exists.
    ready_at: u64,
    start_sample: u64,
    end_sample: u64,
    star_system: Option<String>,
    star_pos: Option<[f64; 3]>,
    timestamp: String,
}

/// Why the status indicator reads what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Starting,
    Warming,
    Capturing,
    NoSignal,
    Anomaly,
    DeviceLost,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "● STARTING",
            Status::Warming => "● LEARNING BACKGROUND",
            Status::Capturing => "● CAPTURING",
            Status::NoSignal => "● NO SIGNAL",
            Status::Anomaly => "● ANOMALY",
            Status::DeviceLost => "● DEVICE LOST",
        }
    }
}

pub struct App {
    cfg: Config,
    device_label: String,
    rx: Receiver<CaptureMessage>,
    /// Held so capture stops when the app is dropped.
    _capture: CaptureHandle,

    engine: Option<AnalysisEngine>,
    journal: Option<JournalWatcher>,
    writer: CaptureWriter,
    /// When the last capture was written, and where.
    ///
    /// Export consults this before writing. A capture from moments ago already
    /// contains the moment being asked about — the spans are longer than the gap
    /// — so writing a second copy would spend a hundred and fifty seconds of
    /// disk to save what is already saved, and telling someone their audio
    /// "failed" when it is sitting on disk is worse than either.
    last_capture: Option<(Instant, PathBuf)>,
    disk_usage: DiskUsage,
    disk_usage_at: Option<Instant>,

    pending: Vec<PendingCapture>,
    events: Vec<EventRecord>,
    status: Status,
    error: Option<String>,
    /// Frame at which the most recent anomaly ended, for the status indicator.
    last_anomaly_frame: u64,
    last_journal_poll: Instant,
    config_path: Option<PathBuf>,

    /// Analysis suspended. Capture keeps running so the stream stays open, but
    /// nothing is fed to the engine.
    paused: bool,
    /// The negotiated format, kept so the engine can be rebuilt when a setting
    /// changes its buffer shapes.
    last_format: Option<StreamFormat>,
    /// Whether each detector currently reports present, for the indicators.
    keying_present: bool,
    structure_present: bool,
    /// Keying is firing, but the game is playing music — so it may be the music.
    keying_suspect: bool,
    /// The period matches the Landscape Signal with high confidence. This is the
    /// only indicator that reliably separates the real signal from ship
    /// ambience — structure and keying both overlap with it.
    landscape_present: bool,
}

impl App {
    pub fn new(
        cfg: Config,
        device_label: String,
        capture: CaptureHandle,
        rx: Receiver<CaptureMessage>,
        capture_dir: PathBuf,
    ) -> Self {
        let journal = if cfg.journal_enabled {
            let dir = if cfg.journal_path.is_empty() {
                JournalWatcher::default_dir()
            } else {
                Some(PathBuf::from(&cfg.journal_path))
            };
            match dir {
                Some(d) => Some(JournalWatcher::new(d)),
                None => {
                    log::warn!("could not determine the journal directory; running without it");
                    None
                }
            }
        } else {
            None
        };

        Self {
            writer: CaptureWriter::new(capture_dir, &cfg),
            last_capture: None,
            disk_usage: DiskUsage::default(),
            disk_usage_at: None,
            cfg,
            device_label,
            rx,
            _capture: capture,
            engine: None,
            journal,
            pending: Vec::new(),
            events: Vec::new(),
            status: Status::Starting,
            error: None,
            last_anomaly_frame: 0,
            last_journal_poll: Instant::now(),
            config_path: None,
            paused: false,
            last_format: None,
            keying_present: false,
            structure_present: false,
            keying_suspect: false,
            landscape_present: false,
        }
    }

    // ---- runtime controls ----

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Suspend or resume analysis. The stream stays open either way, so
    /// resuming does not re-negotiate the device or restart the warm-up.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused != paused {
            log::info!("analysis {}", if paused { "paused" } else { "resumed" });
        }
        self.paused = paused;
    }

    /// Switch the spectrogram between raw level and background-subtracted.
    pub fn set_show_excess(&mut self, show: bool) {
        self.cfg.spectrogram_show_excess = show;
    }

    pub fn detect_keying(&self) -> bool {
        self.cfg.detect_keying
    }

    pub fn detect_structure(&self) -> bool {
        self.cfg.detect_structure
    }

    pub fn overlay_enabled(&self) -> bool {
        self.cfg.overlay_enabled
    }

    /// Turn the in-game overlay on or off. Analysis is unaffected either way.
    pub fn set_overlay_enabled(&mut self, on: bool) {
        self.cfg.overlay_enabled = on;
    }

    /// Toggle either primary detector. Cheap: the background model is untouched.
    pub fn set_detectors(&mut self, keying: bool, structure: bool) {
        self.cfg.detect_keying = keying;
        self.cfg.detect_structure = structure;
        if let Some(engine) = self.engine.as_mut() {
            engine.set_detectors(keying, structure);
        }
        if !keying {
            self.keying_present = false;
        }
        if !structure {
            self.structure_present = false;
        }
    }

    pub fn direction_finding(&self) -> bool {
        self.cfg.direction_finding
    }

    /// Turn direction finding on or off.
    ///
    /// Unlike the detectors this changes the shape of every buffer — the ring
    /// gains or loses channels and the transform count changes — so the engine
    /// is rebuilt and the accumulated history is lost.
    pub fn set_direction_finding(&mut self, enabled: bool) {
        if self.cfg.direction_finding == enabled {
            return;
        }
        self.cfg.direction_finding = enabled;
        log::info!(
            "direction finding {} — rebuilding the analysis engine, history is lost",
            if enabled { "on" } else { "off" }
        );
        self.rebuild_engine();
    }

    fn rebuild_engine(&mut self) {
        if let Some(format) = self.last_format.clone() {
            self.engine = Some(AnalysisEngine::new(self.cfg.clone(), format));
            self.pending.clear();
            // A format means the endpoint is delivering again, so any recorded
            // failure is history. Clearing it together with the status keeps
            // the two from contradicting each other — "warming up" beside a
            // stale "device lost" message helps nobody.
            self.error = None;
            self.status = Status::Warming;
        }
    }

    /// Whether each primary detector currently reports something present.
    /// Frequency span of whatever is being detected right now, if anything.
    ///
    /// The overlay's lowest rung reports where something was found rather than
    /// what it was, because the whole point of that rung is things the named
    /// detectors cannot describe.
    pub fn active_band_hz(&self) -> Option<(f32, f32)> {
        self.engine.as_ref().and_then(|e| e.active_band_hz())
    }

    pub fn detections_present(&self) -> (bool, bool) {
        (self.keying_present, self.structure_present)
    }

    /// True when keying is firing while music plays, so the reading is suspect.
    pub fn keying_suspect(&self) -> bool {
        self.keying_suspect
    }

    /// The period matches the Landscape Signal. The strongest claim the tool
    /// makes, and the one worth acting on.
    pub fn landscape_present(&self) -> bool {
        self.landscape_present
    }

    /// Thargoid Sensor Morse, if it is being heard.
    pub fn morse(&self) -> Option<crate::analysis::morse::MorseDetection> {
        self.engine.as_ref().and_then(|e| e.morse())
    }

    /// Whether the SIGNAL lamp should be lit.
    ///
    /// One lamp, several recognised signals. The Landscape Signal is identified
    /// by its period and Thargoid Sensor Morse by its dot/dash ratio; both mean
    /// "something known is transmitting", so both light the same indicator and
    /// the detail line says which.
    /// Is SIGNAL lit?
    ///
    /// Lit while something was detected **recently**, timed from when it
    /// happened rather than from when the software noticed.
    ///
    /// The window is short on purpose. Lighting the lamp for anything still on
    /// screen sounds right and is not: detections are sparse but the display
    /// holds two minutes, so at least one is nearly always present and the lamp
    /// never goes out. Meanwhile the timeline strip showed the truth — a few
    /// marks, correctly placed. The lamp answers "is something happening"; the
    /// strip answers "when did things happen". Those are different windows, and
    /// giving them the same one made the lamp useless.
    pub fn signal_present(&self) -> bool {
        if self.landscape_present
            || self
                .morse()
                .is_some_and(|m| m.is_present(self.cfg.morse_threshold))
        {
            return true;
        }
        let Some(engine) = self.engine.as_ref() else {
            return false;
        };
        let now = engine.timeline_seconds();
        engine
            .traced_strokes()
            .iter()
            .any(|s| now - s.end_seconds <= SIGNAL_RECENCY_SECONDS)
    }

    /// The current period estimate, for display.
    pub fn periodicity(&self) -> Option<crate::analysis::periodicity::PeriodicityResult> {
        self.engine.as_ref().and_then(|e| e.periodicity())
    }

    /// Track what the primary detectors report, and keep the audio for anything
    /// newly present.
    ///
    /// Deliberately silent. An audible alert was tried and removed: it played
    /// through the same endpoint being captured, so the tool fed its own
    /// two-tone chirp back into its own keying detector.
    fn check_detectors(&mut self) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let keying = engine
            .keying()
            .is_some_and(|k| k.is_present(self.cfg.keying_threshold));
        // Two routes to the same claim, and the folded one is the only one that
        // has ever worked on real audio.
        //
        // The live scan reads the last few seconds of spectrogram, which on real
        // recordings scores drawn signals and ordinary ship ambience about
        // equally. The fold averages an hour of the excess tier against its own
        // period: a signal that repeats survives, and ambience — which does not
        // repeat — averages toward flat. Measured, the fold scored a synthetic
        // Landscape at 0.54 while two real ambience recordings scored 0.000.
        //
        // Either is allowed to light the lamp. The fold needs several cycles
        // before it says anything at all, so it is silent early in a session and
        // grows more sensitive the longer the ship sits still.
        let live_structure = engine.structure().is_present(self.cfg.structure_threshold);
        let folded_structure = engine
            .folded_structure()
            .is_present(self.cfg.structure_threshold);
        let structure = live_structure || folded_structure;

        // Music is the keying detector's main false positive, so a detection
        // made while a track is selected is marked suspect in the display.
        self.keying_suspect = keying && self.game_state().music_playing();

        self.landscape_present = engine.periodicity().is_some_and(|p| {
            crate::analysis::periodicity::matches_landscape(&p, LANDSCAPE_TOLERANCE_SECONDS)
        });

        // Keep the audio on the *rising edge*. The primary detectors used to
        // light up without recording anything, because only the novelty-event
        // scorer could trigger a capture — so the one thing worth keeping was
        // the one thing not kept.
        let was_landscape = self.landscape_present;
        let rising_keying = keying && !self.keying_present;
        let rising_structure = structure && !self.structure_present;
        let rising_landscape = self.landscape_present && !was_landscape;
        if rising_landscape || rising_keying || rising_structure {
            let reason = if rising_landscape {
                "landscape"
            } else if rising_keying {
                "keying"
            } else {
                "structure"
            };
            if let Err(e) = self.keep_recent(self.cfg.detector_capture_seconds, reason, false) {
                log::warn!("could not keep the detected audio: {e:#}");
            }
        }

        self.keying_present = keying;
        self.structure_present = structure;
    }

    /// Write the most recent `seconds` of audio to disk with a sidecar.
    ///
    /// `forced` skips the cooldown and the hourly cap. Those exist to stop the
    /// *detectors* filling the disk while nobody is watching; a person pressing
    /// Export is not that. Applied to a deliberate action they refuse the one
    /// moment somebody actually wanted — which is exactly what happened the first
    /// time this was tried in the field, on a signal that was visible on screen
    /// and is now gone. The disk budget still applies, so this cannot run away.
    pub fn keep_recent(&mut self, seconds: f32, reason: &str, forced: bool) -> Result<PathBuf> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no stream yet"))?;

        let now = Instant::now();
        if !forced {
            // A score of 1.0 clears the threshold; what is being tested here is
            // the rate limiting, not the evidence.
            let decision = self.writer.evaluate(1.0, now);
            if !decision.accepted() {
                anyhow::bail!("declined ({decision:?})");
            }
        }

        let format = engine.ring_format();
        let ring = engine.ring();
        let wanted = format.seconds_to_frames(seconds).min(ring.len_frames());
        if wanted == 0 {
            anyhow::bail!("there is no audio to keep yet");
        }
        let mut samples = Vec::new();
        ring.copy_latest(wanted, &mut samples);

        let period = engine.periodicity();
        let keying = engine.keying();
        let structure = engine.structure().clone();
        let game = self.game_state();
        let device = self.device_label.clone();

        let sidecar = DetectorSidecar {
            audio_evicted: false,
            captured_utc: chrono::Utc::now().to_rfc3339(),
            audio_file: String::new(),
            reason: reason.to_owned(),
            star_system: game.star_system.clone(),
            star_pos: game.star_pos,
            body: game.body.clone(),
            music_track: game.music_track.clone(),
            in_supercruise: game.in_supercruise,
            sample_rate: format.sample_rate,
            channels: format.channels,
            device: device.clone(),
            seconds: wanted as f32 / format.sample_rate as f32,
            keying_confidence: keying.as_ref().map(|k| k.confidence),
            keying_tones_hz: keying
                .as_ref()
                .map(|k| k.tones_hz.clone())
                .unwrap_or_default(),
            keying_symbol_rate_hz: keying.as_ref().map(|k| k.symbol_rate_hz),
            structure_score: structure.score,
            folded_structure_score: engine.folded_structure().score,
            folded_period_seconds: engine.folded().map(|f| f.period_seconds),
            folded_cycles: engine.folded().map(|f| f.cycles),
            structure_coherence: structure.coherence,
            structure_sparsity: structure.sparsity,
            structure_orientation_diversity: structure.orientation_diversity,
            period_seconds: period.as_ref().map(|p| p.period_seconds),
            period_confidence: period.as_ref().map(|p| p.confidence),
            matches_landscape: period
                .as_ref()
                .is_some_and(|p| crate::analysis::periodicity::matches_landscape(p, 2.0)),
        };

        let written = self
            .writer
            .write_span(&samples, &format, &device, &game, sidecar, now);
        if let Ok(path) = &written {
            self.last_capture = Some((now, path.clone()));
        }
        written
    }

    /// A capture written within `within`, if there is one.
    ///
    /// Used by Export to report what is already on disk rather than duplicating
    /// it. Detector captures count: they are the same audio.
    pub fn recent_capture(&self, within: std::time::Duration) -> Option<&Path> {
        self.last_capture
            .as_ref()
            .filter(|(at, _)| at.elapsed() <= within)
            .map(|(_, p)| p.as_path())
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Where to persist the selected device when the user switches endpoints.
    pub fn set_config_path(&mut self, path: PathBuf) {
        self.config_path = Some(path);
    }

    /// Tear down the current stream and open another endpoint.
    ///
    /// The analysis engine is discarded rather than reused: a new endpoint can
    /// have a different sample rate and channel layout, which would make every
    /// buffer and every bin mapping wrong.
    pub fn switch_device(&mut self, device: &crate::audio::device::AudioDevice) -> Result<()> {
        log::info!("switching to {}", device.display_name());
        let (tx, rx) = crossbeam_channel::bounded(256);
        let handle = crate::audio::capture::start(device, tx)?;

        // Replacing the handle drops the old one, which joins its thread.
        self._capture = handle;
        self.rx = rx;
        self.engine = None;
        self.pending.clear();
        self.error = None;
        self.status = Status::Starting;
        self.last_anomaly_frame = 0;
        self.device_label = device.display_name();

        self.cfg.device = device.id.clone();
        if let Some(path) = &self.config_path
            && let Err(e) = self.cfg.save(path)
        {
            log::warn!("could not persist the device selection: {e:#}");
        }
        Ok(())
    }

    /// Add a frequency range to the ignore list and apply it immediately.
    pub fn mute_band(&mut self, low_hz: f32, high_hz: f32) {
        let band = crate::config::IgnoreBand {
            low_hz: low_hz.min(high_hz),
            high_hz: low_hz.max(high_hz),
        };
        log::info!("muting {:.0}–{:.0} Hz", band.low_hz, band.high_hz);
        self.cfg.ignore_bands.push(band);
        if let Some(engine) = self.engine.as_mut() {
            engine.set_ignore_bands(&self.cfg.ignore_bands);
        }
    }

    pub fn clear_muted_bands(&mut self) {
        self.cfg.ignore_bands.clear();
        if let Some(engine) = self.engine.as_mut() {
            engine.set_ignore_bands(&[]);
        }
    }

    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    pub fn engine(&self) -> Option<&AnalysisEngine> {
        self.engine.as_ref()
    }

    pub fn format(&self) -> Option<&StreamFormat> {
        self.engine.as_ref().map(|e| e.format())
    }

    pub fn events(&self) -> &[EventRecord] {
        &self.events
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn game_state(&self) -> GameState {
        self.journal
            .as_ref()
            .map(|j| j.state().clone())
            .unwrap_or_default()
    }

    pub fn captures_written(&self) -> u64 {
        self.writer.captures_written()
    }

    /// How much disk the recordings and exports are using.
    ///
    /// Scanning a directory is far too slow to do every frame, so the answer is
    /// cached and refreshed at most every few seconds. `force` is for after a
    /// deliberate clean-up, when the stale number would be the wrong answer at
    /// exactly the moment someone is looking at it.
    pub fn disk_usage(&mut self, force: bool) -> DiskUsage {
        const REFRESH: Duration = Duration::from_secs(5);
        if force || self.disk_usage_at.is_none_or(|t| t.elapsed() >= REFRESH) {
            let exports = self
                .cfg
                .export_dir
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| self.writer.dir().with_file_name("exports"));
            self.disk_usage = DiskUsage {
                capture_bytes: self.writer.bytes_on_disk(),
                capture_budget: self.writer.budget_bytes(),
                export_bytes: crate::retention::extension_bytes(&exports, "png"),
                export_budget: self.cfg.export_budget_mb.saturating_mul(1_048_576),
                records: crate::retention::record_count(self.writer.dir()),
            };
            self.disk_usage_at = Some(Instant::now());
        }
        self.disk_usage
    }

    /// Bring both folders back inside their budgets right now.
    pub fn clean_up_disk(&mut self) {
        self.writer.enforce_budget();
        let exports = self
            .cfg
            .export_dir
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.writer.dir().with_file_name("exports"));
        crate::retention::enforce_simple_budget(
            &exports,
            "png",
            self.cfg.export_budget_mb.saturating_mul(1_048_576),
        );
        self.disk_usage(true);
    }

    /// Drain everything capture has produced. Returns the number of new
    /// detections.
    pub fn pump(&mut self) -> usize {
        // The journal moves far more slowly than audio; once a second is plenty.
        if self.last_journal_poll.elapsed() >= std::time::Duration::from_secs(1) {
            if let Some(j) = self.journal.as_mut() {
                j.poll();
            }
            self.last_journal_poll = Instant::now();
        }

        let mut new_detections = 0;
        // Bounded, not `loop {}`. A producer faster than the consumer — an
        // offline file replayed at full speed, or a device catching up after a
        // stall — refills the channel quicker than it drains, so an unbounded
        // drain never sees `Empty` and never returns. Observed: a looped file
        // analysis that should have stopped after 25 s ran past ten minutes.
        const MAX_MESSAGES_PER_PUMP: usize = 512;
        for _ in 0..MAX_MESSAGES_PER_PUMP {
            match self.rx.try_recv() {
                Ok(CaptureMessage::Format(format)) => {
                    log::info!("stream format: {}", format.describe());
                    let bytes = self.cfg.pcm_ring_bytes(format.sample_rate, format.channels);
                    log::info!(
                        "pcm ring: {:.0} s, {:.1} MB; direction finding uses {} of {} channels",
                        self.cfg.pcm_ring_seconds,
                        bytes as f64 / 1_048_576.0,
                        format.directional_channels(),
                        format.channels
                    );
                    if format.directional_channels() < 2 {
                        log::warn!(
                            "this endpoint gives no usable bearing; see the README on switching \
                             the Windows output to 7.1"
                        );
                    }
                    self.last_format = Some(format.clone());
                    self.engine = Some(AnalysisEngine::new(self.cfg.clone(), format));
                    self.status = Status::Warming;
                }
                Ok(CaptureMessage::Audio(samples)) if self.paused => {
                    // Drop it. The stream stays open; only analysis is idle.
                    let _ = samples;
                }
                Ok(CaptureMessage::Audio(samples)) => {
                    if let Some(engine) = self.engine.as_mut() {
                        for d in engine.push_interleaved(&samples) {
                            self.queue(d);
                            new_detections += 1;
                        }
                    }
                }
                Ok(CaptureMessage::Gap { frames, idle }) => {
                    if let Some(engine) = self.engine.as_mut() {
                        engine.push_gap(frames);
                    }
                    if !idle {
                        log::debug!("filled a {frames}-frame device gap");
                    }
                }
                Ok(CaptureMessage::Error(e)) => {
                    log::error!("capture error: {e}");
                    self.error = Some(e);
                    self.status = Status::DeviceLost;
                }
                Ok(CaptureMessage::Stopped) => {
                    if self.error.is_none() {
                        self.status = Status::DeviceLost;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status = Status::DeviceLost;
                    break;
                }
            }
        }

        self.flush_pending();
        self.check_detectors();
        self.update_status();
        new_detections
    }

    /// Queue a detection, waiting for its post-roll before writing anything.
    fn queue(&mut self, detection: Detection) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let format = engine.format();
        let game = self.game_state();
        let timestamp = chrono::Utc::now().to_rfc3339();

        let pre_roll = format.seconds_to_frames(self.cfg.capture_pre_roll_seconds) as u64;
        let post_roll = format.seconds_to_frames(self.cfg.capture_post_roll_seconds) as u64;
        let start_sample = detection.start_sample.saturating_sub(pre_roll);
        let end_sample = detection.end_sample + post_roll;

        self.last_anomaly_frame = engine.ring().total_frames();

        let decision = self.writer.evaluate(detection.event.score, Instant::now());
        if !decision.accepted() {
            log::info!(
                "detection at {:.0} Hz scored {:.2}; not captured ({decision:?})",
                detection.event.peak_hz,
                detection.event.score
            );
            self.events.push(EventRecord {
                detection,
                star_system: game.star_system,
                star_pos: game.star_pos,
                timestamp,
                captured_to: None,
                decision,
            });
            return;
        }

        self.pending.push(PendingCapture {
            ready_at: end_sample,
            start_sample,
            end_sample,
            star_system: game.star_system,
            star_pos: game.star_pos,
            timestamp,
            detection,
        });
    }

    /// Write any queued capture whose post-roll has now arrived.
    fn flush_pending(&mut self) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let available = engine.ring().total_frames();
        let ready: Vec<usize> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, p)| available >= p.ready_at)
            .map(|(i, _)| i)
            .collect();

        for index in ready.into_iter().rev() {
            let p = self.pending.remove(index);
            match self.write_capture(&p) {
                Ok(path) => self.events.push(EventRecord {
                    detection: p.detection,
                    star_system: p.star_system,
                    star_pos: p.star_pos,
                    timestamp: p.timestamp,
                    captured_to: Some(path),
                    decision: TriggerDecision::Accept,
                }),
                Err(e) => {
                    log::error!("could not write capture: {e:#}");
                    self.events.push(EventRecord {
                        detection: p.detection,
                        star_system: p.star_system,
                        star_pos: p.star_pos,
                        timestamp: p.timestamp,
                        captured_to: None,
                        decision: TriggerDecision::Accept,
                    });
                }
            }
        }
    }

    fn write_capture(&mut self, p: &PendingCapture) -> Result<PathBuf> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no stream"))?;
        let ring = engine.ring();

        // The pre-roll may already have been evicted if the ring is short
        // relative to the configured roll; take what is actually there.
        let start = p.start_sample.max(ring.oldest_frame());
        let end = p.end_sample.min(ring.total_frames());
        if end <= start {
            anyhow::bail!("the requested span is no longer resident in the ring");
        }
        if start > p.start_sample {
            log::warn!(
                "pre-roll truncated by {:.1} s: the ring is shorter than the requested roll",
                (start - p.start_sample) as f64 / engine.format().sample_rate as f64
            );
        }

        let mut samples = Vec::new();
        ring.copy_range(start, (end - start) as usize, &mut samples)?;

        // The ring is mono unless direction finding is on, so the WAV header
        // must describe the ring, not the endpoint.
        let format = engine.ring_format();
        let period = engine.periodicity();
        let game = self.game_state();
        let device = self.device_label.clone();
        self.writer.write(
            crate::capture_writer::CaptureRequest {
                detection: &p.detection,
                samples: &samples,
                format: &format,
                device: &device,
                game: &game,
                period: period.as_ref(),
                timestamp: &p.timestamp,
            },
            Instant::now(),
        )
    }

    fn update_status(&mut self) {
        if matches!(self.status, Status::DeviceLost) {
            return;
        }
        let Some(engine) = self.engine.as_ref() else {
            self.status = Status::Starting;
            return;
        };
        let recent = engine
            .ring()
            .total_frames()
            .saturating_sub(self.last_anomaly_frame);
        let recent_seconds = engine.format().frames_to_seconds(recent);

        self.status = if !self.detector_warm() {
            Status::Warming
        } else if self.last_anomaly_frame > 0 && recent_seconds < 10.0 {
            Status::Anomaly
        } else if self.silent() {
            Status::NoSignal
        } else {
            Status::Capturing
        };
    }

    fn detector_warm(&self) -> bool {
        self.warmup_progress() >= 1.0
    }

    /// How far the background model has settled, 0..1.
    pub fn warmup_progress(&self) -> f32 {
        self.engine
            .as_ref()
            .map(|e| e.warmup_progress())
            .unwrap_or(0.0)
    }

    fn silent(&self) -> bool {
        self.engine.as_ref().map(|e| e.is_silent()).unwrap_or(true)
    }

    /// Build a fresh snapshot for display. Call at `analysis_update_hz`.
    pub fn snapshot(&mut self) -> Option<AnalysisSnapshot> {
        self.engine.as_mut().map(|e| e.snapshot())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::audio::SampleFormat;
    use crate::audio::capture::start_synthetic;
    use crate::audio::format::MASK_7_1;
    use crate::audio::synthetic::{SyntheticSource, TestSignal};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ed-compass-app-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.fft_size = 1024;
        c.hop = 512;
        c.pcm_ring_seconds = 4.0;
        c.waterfall_seconds = 20.0;
        c.longterm_fps = 2.0;
        c.longterm_bands = 32;
        c.background_time_constant_seconds = 1.0;
        c.background_max_freeze_seconds = 300.0;
        c.min_event_seconds = 0.3;
        c.capture_pre_roll_seconds = 0.5;
        c.capture_post_roll_seconds = 0.5;
        c.capture_cooldown_seconds = 0.0;
        c.journal_enabled = false;
        c
    }

    fn app_with(signal: TestSignal, dir: PathBuf) -> App {
        let cfg = config();
        let format = StreamFormat::new(8_000, 8, MASK_7_1, SampleFormat::F32);
        let (tx, rx) = crossbeam_channel::unbounded();
        let capture = start_synthetic(SyntheticSource::new(signal, format, -45.0), tx);
        App::new(cfg, "synthetic".into(), capture, rx, dir)
    }

    /// Pump until a condition holds or the deadline passes.
    ///
    /// Deadlines are deliberately generous. These drive a real capture thread,
    /// so on a loaded machine a tight bound fails for lack of CPU rather than
    /// for anything wrong with the code — which is a flaky test, not a useful
    /// one.
    fn pump_until(app: &mut App, seconds: f32, mut done: impl FnMut(&App) -> bool) -> bool {
        let deadline = Instant::now() + std::time::Duration::from_secs_f32(seconds);
        while Instant::now() < deadline {
            app.pump();
            if done(app) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn the_format_message_starts_the_engine() {
        let dir = temp_dir("format");
        let mut app = app_with(TestSignal::Silence, dir.clone());
        assert_eq!(app.status(), Status::Starting);
        assert!(pump_until(&mut app, 15.0, |a| a.format().is_some()));

        let f = app.format().unwrap();
        assert_eq!(f.channels, 8);
        assert_eq!(f.layout_name(), "7.1");
        assert_ne!(app.status(), Status::Starting);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn silence_reports_no_signal_and_writes_nothing() {
        let dir = temp_dir("silent");
        let mut app = app_with(TestSignal::Silence, dir.clone());
        pump_until(&mut app, 15.0, |a| a.status() == Status::NoSignal);

        assert_eq!(app.status(), Status::NoSignal);
        assert!(app.events().is_empty());
        assert_eq!(app.captures_written(), 0);
        assert!(!dir.exists(), "nothing should have touched the disk");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_is_available_once_audio_flows() {
        let dir = temp_dir("snapshot");
        let mut app = app_with(TestSignal::Noise, dir.clone());
        assert!(pump_until(&mut app, 15.0, |a| a.format().is_some()));

        let snap = app.snapshot().unwrap();
        assert_eq!(snap.format.channels, 8);
        assert!(snap.timeline_seconds > 0.0);
        assert_eq!(snap.spectrum_db.len(), 513);
        assert!(!snap.is_silent);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capture_error_is_surfaced_rather_than_swallowed() {
        let dir = temp_dir("error");
        let cfg = config();
        let (tx, rx) = crossbeam_channel::unbounded();
        let format = StreamFormat::new(8_000, 2, 0, SampleFormat::F32);
        let capture = start_synthetic(
            SyntheticSource::new(TestSignal::Silence, format, 0.0),
            tx.clone(),
        );
        let mut app = App::new(cfg, "dev".into(), capture, rx, dir.clone());

        // Let the stream establish itself first. Sending the error immediately
        // raced the capture thread's own `Format` message, and a format means
        // the device is working — so the rebuild cleared the very error this
        // test was asserting, roughly half the time.
        assert!(
            pump_until(&mut app, 15.0, |a| a.format().is_some()),
            "the stream never started"
        );

        tx.send(CaptureMessage::Error("the endpoint went away".into()))
            .unwrap();
        assert!(
            pump_until(&mut app, 15.0, |a| a.error().is_some()),
            "the error never arrived; a bare status assertion after a silent \
             timeout reports the wrong cause"
        );

        assert_eq!(app.status(), Status::DeviceLost);
        assert_eq!(app.error(), Some("the endpoint went away"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_recovers_from_warming_to_a_steady_state() {
        let dir = temp_dir("warming");
        let mut app = app_with(TestSignal::Noise, dir.clone());
        assert!(pump_until(&mut app, 15.0, |a| a.status() == Status::Warming));
        assert!(
            pump_until(&mut app, 25.0, |a| a.status() == Status::Capturing),
            "should settle into steady capture, stuck at {:?}",
            app.status()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

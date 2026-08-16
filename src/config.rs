//! Configuration: a `config.toml` living next to the executable.
//!
//! Defaults are written out on first run so the file is self-documenting by
//! example. CLI flags override the file; only the selected device is persisted
//! back from the UI.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A frequency range excluded from novelty detection, in Hz.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IgnoreBand {
    pub low_hz: f32,
    pub high_hz: f32,
}

impl IgnoreBand {
    pub fn contains(&self, hz: f32) -> bool {
        hz >= self.low_hz && hz <= self.high_hz
    }
}

/// Bumped when the overlay layout changes shape, not when it gains options.
///
/// 2: indicators moved to a left-hand column with the spectrogram filling the
///    full height, anchored to the game window's top-left corner.
pub const OVERLAY_LAYOUT_REVISION: u32 = 2;

/// Whether files can actually be created in a directory.
///
/// Determined by trying, not by inspecting the path: on Windows the answer
/// depends on the ACL, on whether the process is elevated, and on virtualisation
/// rules that no amount of looking at the string will tell you.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ed-compass-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The per-user place for application data, created if it is missing.
fn user_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    // Only reached in development; the capture backend is Windows-only.
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"));

    let dir = base?.join("ED Compass");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Endpoint id, or empty for the default render endpoint in loopback mode.
    pub device: String,

    // ---- buffers ----
    /// Raw multichannel PCM retained in memory. 150 s covers one 109.5 s
    /// Landscape cycle plus margin and is the pre-roll for a triggered capture.
    pub pcm_ring_seconds: f32,
    pub fft_size: usize,
    pub hop: usize,
    pub waterfall_seconds: f32,
    /// Lowest frequency drawn on the log spectrogram axis.
    pub spectrogram_min_hz: f32,
    /// Highest frequency drawn. 22050 matches the community decode guides for
    /// Audacity and Sonic Visualiser; it is clamped to Nyquist at render time.
    pub spectrogram_max_hz: f32,
    /// Lowest frequency the *detectors* look at.
    ///
    /// Kept separate from the display band on purpose: you may want to look at a
    /// wide spectrum while the detectors concentrate on where signals actually
    /// live. Scanning 20 Hz to Nyquist dilutes every metric — sparsity and
    /// diagonality get averaged over mostly-empty space, and low-frequency
    /// rumble contributes edges it has no business contributing.
    pub detect_min_hz: f32,
    /// Highest frequency the detectors look at.
    pub detect_max_hz: f32,
    /// Subtract each frequency row's median from the rendered spectrogram.
    ///
    /// Steady low-frequency rumble is the loudest thing in most captures and it
    /// never changes, so it both hides faint structure and swallows the colour
    /// ramp. Removing each row's median deletes anything constant and leaves
    /// only what varied.
    pub spectrogram_median_subtract: bool,
    /// Show the background-subtracted spectrogram rather than raw level.
    ///
    /// Raw level is dominated by whatever is constantly loud. Subtracting the
    /// learned background removes the ship and leaves only what changed.
    pub spectrogram_show_excess: bool,
    pub longterm_fps: f32,
    pub longterm_bands: usize,
    pub histogram_bins: usize,
    pub analysis_update_hz: f32,
    /// Trailing window the signal-health readouts cover. Short on purpose: it
    /// is a level meter, not a session average.
    pub health_window_seconds: f32,

    // ---- what to compute ----
    /// Estimate a bearing from inter-channel differences.
    ///
    /// Off by default. It costs one FFT per channel per frame instead of one
    /// total, and forces the PCM ring to hold every channel — together the
    /// dominant cost of the application. The presence detectors do not need it.
    pub direction_finding: bool,
    /// Detect binary keying: alternation between a small set of discrete tones,
    /// as used by the Thargoid Probe tightbeam.
    pub detect_keying: bool,
    /// Detect drawn structure in the spectrogram — strokes, arcs, and curves
    /// that natural audio does not produce.
    pub detect_structure: bool,
    /// Keying confidence at or above which a transmission is reported present.
    ///
    /// Calibrated against measured data rather than chosen. A synthetic keyed
    /// tightbeam scores 0.96. CMDR Serbanstein's genuine Landscape Signal
    /// recording — which is a drawing, not a transmission — scores 0.51 to 0.78
    /// because its swept strokes dwell like symbols. 0.85 separates them.
    pub keying_threshold: f32,
    /// Ignore candidate keying tones below this frequency.
    ///
    /// Ship and drive rumble dominates the bottom few hundred hertz and its
    /// peak bin wanders, which mimics keying. Known transmissions key well
    /// above this.
    pub keying_min_hz: f32,
    /// Structure score at or above which a drawing is reported present.
    ///
    /// Treat this as advisory. Measured, the structure score does not separate
    /// the Landscape Signal (0.554) from ordinary ship ambience (0.39–0.65) —
    /// any threshold that silences one silences the other. The period is what
    /// identifies the signal; see `matches_landscape`.
    pub structure_threshold: f32,
    /// How much audio to keep when a primary detector fires. Long enough to
    /// hold more than one Landscape cycle.
    pub detector_capture_seconds: f32,

    // ---- overlay ----
    /// Which view to open in: "full", "compact", or "overlay".
    pub view: String,
    /// Overlay centre as a fraction of the game window width. The default 0.375
    /// is a quarter of the way from the centre toward the left edge.
    pub overlay_x_fraction: f32,
    /// Overlay top edge as a fraction of the game window height.
    pub overlay_y_fraction: f32,
    pub overlay_width: f32,
    /// Height of the lamp strip, before any spectrogram is added.
    pub overlay_height: f32,
    /// Show the in-game overlay when Elite has focus.
    ///
    /// It is not a mode you switch into: the control window stays open and the
    /// overlay comes and goes with the game, so there is never a state you have
    /// to kill the process to leave.
    pub overlay_enabled: bool,
    /// Which generation of the overlay layout the saved geometry belongs to.
    ///
    /// Position and size are yours to change, so they must survive an upgrade —
    /// but when the layout itself is redesigned, keeping the old numbers gives a
    /// window sized for a arrangement that no longer exists. Bumping
    /// [`OVERLAY_LAYOUT_REVISION`] resets just the geometry, once.
    pub overlay_layout_revision: u32,
    /// Draw a spectrogram beside the overlay lamps, at full overlay height.
    pub overlay_spectrogram: bool,
    /// Share of the overlay width taken by the indicator column on the left.
    /// The spectrogram fills the rest, at the overlay's full height.
    pub overlay_label_fraction: f32,
    /// Seconds of history the overlay spectrogram covers.
    ///
    /// Independent of the main window: a cockpit strip wants a short, fast view,
    /// not two and a half minutes squeezed into a few hundred pixels.
    pub overlay_spectrogram_seconds: f32,
    /// Where exported spectrogram images are written. Relative to the working
    /// directory unless absolute.
    pub export_dir: Option<String>,
    pub export_width: usize,
    /// Scale the export height so stroke angles match the community's published
    /// spectrograms, which span 20 Hz to 22050 Hz.
    ///
    /// With this on, `export_height` means "height if the full 20–22050 Hz band
    /// were shown", and the actual height is scaled down when a narrower band is
    /// displayed. Without it, cropping the band silently steepens every slope.
    pub export_match_published_aspect: bool,
    /// Height of exported images.
    ///
    /// This sets the apparent *angle* of every sloped stroke, because the slope
    /// in pixels is `(log-frequency span / height) / (time span / width)`.
    /// Narrowing the frequency band without reducing the height makes slopes
    /// steeper: cropping 20–22050 Hz down to 200–2400 Hz is a 2.82x reduction in
    /// log span, so at the same pixel height every slope steepens by 2.82x.
    /// See `matched_export_height` to reproduce another view's proportions.
    pub export_height: usize,

    // ---- novelty detection ----
    pub novelty_threshold_db: f32,
    pub background_time_constant_seconds: f32,
    /// How long a bin may stay above its background before the model gives up
    /// and adapts anyway. Must comfortably exceed the longest signal we expect
    /// to see — the Landscape Signal's mountain runs about 80 s.
    pub background_max_freeze_seconds: f32,
    pub min_event_seconds: f32,
    /// How long an event may drop below threshold before it is considered over.
    pub event_gap_tolerance_seconds: f32,
    pub trigger_score: f32,
    pub ignore_bands: Vec<IgnoreBand>,

    // ---- triggered capture ----
    pub capture_pre_roll_seconds: f32,
    pub capture_post_roll_seconds: f32,
    pub capture_cooldown_seconds: f32,
    pub max_captures_per_hour: u32,
    pub disk_budget_mb: u64,
    /// How many of the best captures are held back from eviction, whatever
    /// their age. Ranked by detector score, with a confirmed Landscape Signal
    /// outranking everything else.
    pub protect_best_captures: usize,
    /// Budget for exported spectrogram PNGs, which are renderings of data held
    /// elsewhere and so are trimmed oldest-first with no ranking.
    pub export_budget_mb: u64,
    /// Container for captured audio: "flac" or "wav".
    ///
    /// FLAC is lossless and roughly halves the size, so the same budget holds
    /// about twice as much evidence. WAV is there for anyone who would rather
    /// have a file every tool on earth can open without thinking.
    pub capture_format: String,

    // ---- journal ----
    pub journal_enabled: bool,
    /// Empty means the default `%USERPROFILE%\Saved Games\...` location.
    pub journal_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: String::new(),

            pcm_ring_seconds: 150.0,
            fft_size: 4096,
            hop: 2048,
            waterfall_seconds: 140.0,
            // Measured from CMDR Serbanstein's reference recording: the signal's
            // energy lies between 20 Hz and ~1.9 kHz, and everything above
            // 2.4 kHz is empty. Showing 22 kHz wastes a third of the image
            // height on nothing and shrinks the strokes to invisibility.
            spectrogram_min_hz: 200.0,
            spectrogram_max_hz: 2_400.0,
            // Raw level, not background-subtracted. Cropping the band already
            // removes the rumble that excess mode existed to suppress, and the
            // thin strokes read far more clearly without it.
            spectrogram_median_subtract: true,
            spectrogram_show_excess: false,
            // The measured band of the Landscape Signal, with margin.
            detect_min_hz: 180.0,
            detect_max_hz: 2_600.0,
            longterm_fps: 1.0,
            longterm_bands: 256,
            histogram_bins: 100,
            analysis_update_hz: 10.0,
            health_window_seconds: 2.0,

            direction_finding: false,
            detect_keying: true,
            detect_structure: true,
            // Raised from 0.85 after measurement: ship ambience at Eratosthenes
            // scored 0.85–0.89, above the old bar and above the genuine
            // Landscape Signal's 0.68. A real keyed tightbeam scores 0.96.
            // Lowered from 0.93 once tone stability was added. Measured after:
            // a keyed tightbeam scores 0.96, the Landscape Signal's swept
            // strokes drop to 0.52, and noise produces no symbols at all. The
            // wide gap matters because a Thargoid probe transmits **once** — it
            // is not periodic, so keying is the only detector that can catch it
            // and it needs headroom.
            keying_threshold: 0.75,
            keying_min_hz: 400.0,
            structure_threshold: 0.35,
            detector_capture_seconds: 130.0,

            view: "compact".into(),
            overlay_enabled: true,
            overlay_layout_revision: OVERLAY_LAYOUT_REVISION,
            // Hard against the top-left corner: nothing of Elite's own HUD
            // lives there, and it leaves the centre and right panels clear.
            overlay_x_fraction: 0.0,
            overlay_y_fraction: 0.0,
            overlay_width: 440.0,
            overlay_height: 104.0,
            overlay_spectrogram: true,
            overlay_label_fraction: 0.34,
            overlay_spectrogram_seconds: 140.0,
            export_dir: None,
            export_width: 8192,
            export_match_published_aspect: true,
            export_height: 1600,

            novelty_threshold_db: 8.0,
            background_time_constant_seconds: 60.0,
            background_max_freeze_seconds: 300.0,
            min_event_seconds: 2.0,
            event_gap_tolerance_seconds: 1.0,
            trigger_score: 0.6,
            ignore_bands: Vec::new(),

            capture_pre_roll_seconds: 30.0,
            capture_post_roll_seconds: 15.0,
            capture_cooldown_seconds: 60.0,
            max_captures_per_hour: 10,
            disk_budget_mb: 2048,
            protect_best_captures: 20,
            export_budget_mb: 512,
            capture_format: "flac".into(),

            journal_enabled: true,
            journal_path: String::new(),
        }
    }
}

impl Config {
    /// `config.toml` beside the executable, falling back to the working
    /// directory if the executable path cannot be determined.
    /// Where the configuration lives — and with it the captures and exports,
    /// which are resolved relative to this file.
    ///
    /// Beside the executable when that directory can be written to, which keeps
    /// a portable unzip self-contained: settings and recordings stay in the
    /// folder you extracted, and deleting it removes every trace.
    ///
    /// When it cannot be written to — an installed copy under Program Files, a
    /// read-only share — it falls back to the per-user application data
    /// directory. Without the fallback, an ordinary user installing to the
    /// default location gets a tool that silently cannot save its own settings.
    pub fn default_path() -> PathBuf {
        let beside_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        if let Some(dir) = &beside_exe
            && is_writable(dir)
        {
            return dir.join("config.toml");
        }
        user_data_dir()
            .or(beside_exe)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("config.toml")
    }

    /// Load the file, or write out the defaults and return those.
    ///
    /// An existing file is also brought up to date: missing keys are filled from
    /// the defaults and written back. Without this, upgrading leaves the old
    /// file in place and every option added since stays invisible — you would
    /// have to read the source to learn it existed.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config from {}", path.display()))?;
            let mut cfg: Config = toml::from_str(&text)
                .with_context(|| format!("parsing config at {}", path.display()))?;
            cfg.migrate_overlay_layout();
            cfg.validate()?;

            // Re-serializing produces every key. If that differs from what is on
            // disk the file predates some options, so refresh it — values are
            // preserved, only missing keys are added.
            if let Ok(current) = toml::to_string_pretty(&cfg)
                && current != text
            {
                log::info!("adding newly-introduced keys to {}", path.display());
                if let Err(e) = std::fs::write(path, current) {
                    log::warn!("could not refresh {}: {e}", path.display());
                }
            }
            Ok(cfg)
        } else {
            let cfg = Config::default();
            // A config we cannot write is not fatal; the defaults still work.
            if let Err(e) = cfg.save(path) {
                log::warn!(
                    "could not write default config to {}: {e:#}",
                    path.display()
                );
            }
            Ok(cfg)
        }
    }

    /// Restore the overlay geometry when it was saved for an older layout.
    ///
    /// Only the geometry: everything else the file says is left alone.
    fn migrate_overlay_layout(&mut self) {
        if self.overlay_layout_revision >= OVERLAY_LAYOUT_REVISION {
            return;
        }
        log::info!(
            "overlay layout changed; restoring its default position and size \
             (revision {} -> {OVERLAY_LAYOUT_REVISION})",
            self.overlay_layout_revision
        );
        // The overlay stopped being a view you switch into at the same time,
        // so a config that opens straight into it has nowhere to go.
        if self.view == "overlay" {
            self.view = "compact".into();
        }
        let d = Config::default();
        self.overlay_x_fraction = d.overlay_x_fraction;
        self.overlay_y_fraction = d.overlay_y_fraction;
        self.overlay_width = d.overlay_width;
        self.overlay_height = d.overlay_height;
        self.overlay_label_fraction = d.overlay_label_fraction;
        self.overlay_layout_revision = OVERLAY_LAYOUT_REVISION;
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(path, text)
            .with_context(|| format!("writing config to {}", path.display()))?;
        Ok(())
    }

    /// Reject values that would panic or silently misbehave downstream.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.fft_size >= 64, "fft_size must be at least 64");
        anyhow::ensure!(
            self.fft_size.is_power_of_two(),
            "fft_size must be a power of two, got {}",
            self.fft_size
        );
        anyhow::ensure!(
            self.hop > 0 && self.hop <= self.fft_size,
            "hop must be in 1..=fft_size"
        );
        anyhow::ensure!(self.pcm_ring_seconds > 0.0, "pcm_ring_seconds must be > 0");
        anyhow::ensure!(self.histogram_bins >= 2, "histogram_bins must be >= 2");
        anyhow::ensure!(
            self.spectrogram_min_hz > 0.0
                && self.spectrogram_max_hz > self.spectrogram_min_hz * 2.0,
            "spectrogram_max_hz ({}) must be more than twice spectrogram_min_hz ({})",
            self.spectrogram_max_hz,
            self.spectrogram_min_hz
        );
        anyhow::ensure!(
            self.detect_min_hz > 0.0 && self.detect_max_hz > self.detect_min_hz * 1.5,
            "detect_max_hz ({}) must be well above detect_min_hz ({})",
            self.detect_max_hz,
            self.detect_min_hz
        );
        anyhow::ensure!(self.longterm_bands >= 8, "longterm_bands must be >= 8");
        anyhow::ensure!(self.longterm_fps > 0.0, "longterm_fps must be > 0");
        anyhow::ensure!(
            self.analysis_update_hz > 0.0,
            "analysis_update_hz must be > 0"
        );
        anyhow::ensure!(
            self.health_window_seconds > 0.0,
            "health_window_seconds must be > 0"
        );
        anyhow::ensure!(
            matches!(self.capture_format.as_str(), "flac" | "wav"),
            "capture_format must be \"flac\" or \"wav\", got {:?}",
            self.capture_format
        );
        anyhow::ensure!(
            matches!(self.view.as_str(), "full" | "compact"),
            "view must be \"full\" or \"compact\", got {:?}. The overlay is no \
             longer a view you start into — it appears whenever Elite has focus, \
             and `overlay_enabled` turns it off",
            self.view
        );
        anyhow::ensure!(
            self.overlay_width >= 80.0 && self.overlay_height >= 30.0,
            "the overlay must be at least 80x30"
        );
        anyhow::ensure!(
            (0.1..=0.9).contains(&self.overlay_label_fraction),
            "overlay_label_fraction must be between 0.1 and 0.9, got {}",
            self.overlay_label_fraction
        );
        anyhow::ensure!(
            self.overlay_spectrogram_seconds > 0.0,
            "overlay_spectrogram_seconds must be > 0"
        );
        anyhow::ensure!(
            self.background_time_constant_seconds > 0.0,
            "background_time_constant_seconds must be > 0"
        );
        anyhow::ensure!(
            self.background_max_freeze_seconds > self.background_time_constant_seconds,
            "background_max_freeze_seconds ({}) must exceed background_time_constant_seconds ({})",
            self.background_max_freeze_seconds,
            self.background_time_constant_seconds
        );
        for b in &self.ignore_bands {
            anyhow::ensure!(
                b.low_hz < b.high_hz,
                "ignore_band low_hz must be below high_hz ({} >= {})",
                b.low_hz,
                b.high_hz
            );
        }
        Ok(())
    }

    /// Bytes held by the raw PCM ring for a given stream shape. Surfaced at
    /// startup and in the UI so a 7.1 endpoint's cost is never a surprise.
    pub fn pcm_ring_bytes(&self, sample_rate: u32, channels: usize) -> usize {
        let frames = (self.pcm_ring_seconds * sample_rate as f32).ceil() as usize;
        frames * channels * std::mem::size_of::<f32>()
    }

    /// Pixel width of the spectrogram panel inside the overlay.
    ///
    /// It occupies everything the indicator column does not, at full height —
    /// the previous stacked layout left most of the window empty.
    pub fn overlay_spectrogram_size(&self) -> (f32, f32) {
        if !self.overlay_spectrogram {
            return (0.0, 0.0);
        }
        (
            (self.overlay_width * (1.0 - self.overlay_label_fraction)).max(16.0),
            self.overlay_height.max(16.0),
        )
    }

    /// Export height that reproduces the stroke angles of a different frequency
    /// band at the same width.
    ///
    /// The community's published spectrograms span 20 Hz to 22050 Hz. Viewing a
    /// narrower band magnifies frequency, which steepens every slope; scaling
    /// the height by the ratio of log spans cancels that exactly.
    pub fn matched_export_height(&self, reference_min_hz: f32, reference_max_hz: f32) -> usize {
        let ours = (self.spectrogram_max_hz / self.spectrogram_min_hz).ln();
        let theirs = (reference_max_hz / reference_min_hz).ln();
        if !ours.is_finite() || !theirs.is_finite() || ours <= 0.0 || theirs <= 0.0 {
            return self.export_height;
        }
        ((self.export_height as f32 * ours / theirs).round() as usize).max(64)
    }

    pub fn is_ignored(&self, hz: f32) -> bool {
        self.ignore_bands.iter().any(|b| b.contains(hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.device = "endpoint-id".into();
        cfg.ignore_bands.push(IgnoreBand {
            low_hz: 40.0,
            high_hz: 120.0,
        });

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn an_old_config_gains_newly_introduced_keys() {
        let dir = std::env::temp_dir().join(format!(
            "ed-compass-cfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // A file from an older build: one setting, deliberately not the default.
        std::fs::write(
            &path,
            "device = \"chosen-endpoint\"\npcm_ring_seconds = 42.0\n",
        )
        .unwrap();

        let cfg = Config::load_or_create(&path).unwrap();
        assert_eq!(cfg.device, "chosen-endpoint", "existing values survive");
        assert_eq!(cfg.pcm_ring_seconds, 42.0);

        let refreshed = std::fs::read_to_string(&path).unwrap();
        assert!(
            refreshed.contains("overlay_x_fraction"),
            "new keys must appear"
        );
        assert!(refreshed.contains("keying_min_hz"));
        assert!(refreshed.contains("detect_keying"));
        assert!(
            refreshed.contains("chosen-endpoint") && refreshed.contains("42.0"),
            "the refresh must not discard what was set"
        );

        // A second load is a no-op.
        let before = std::fs::metadata(&path).unwrap().len();
        let again = Config::load_or_create(&path).unwrap();
        assert_eq!(again, cfg);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // Forward compatibility: an old config file must still load.
        let cfg: Config = toml::from_str("device = \"x\"").unwrap();
        assert_eq!(cfg.device, "x");
        assert_eq!(cfg.fft_size, Config::default().fft_size);
    }

    #[test]
    fn rejects_non_power_of_two_fft() {
        let mut cfg = Config::default();
        cfg.fft_size = 4000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_hop_larger_than_fft() {
        let mut cfg = Config::default();
        cfg.hop = cfg.fft_size + 1;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_inverted_ignore_band() {
        let mut cfg = Config::default();
        cfg.ignore_bands.push(IgnoreBand {
            low_hz: 500.0,
            high_hz: 100.0,
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn the_spectrogram_fills_the_overlay_height_and_the_spare_width() {
        let mut cfg = Config::default();
        cfg.overlay_width = 440.0;
        cfg.overlay_height = 104.0;
        cfg.overlay_label_fraction = 0.34;

        let (w, h) = cfg.overlay_spectrogram_size();
        assert!((w - 290.4).abs() < 0.1, "width {w}");
        assert_eq!(h, 104.0, "it must use the full height, not a strip");

        cfg.overlay_spectrogram = false;
        assert_eq!(cfg.overlay_spectrogram_size(), (0.0, 0.0));
    }

    #[test]
    fn writability_is_established_by_trying_it() {
        let dir = std::env::temp_dir();
        assert!(is_writable(&dir), "the temp directory must be writable");
        assert!(
            !is_writable(&dir.join("ed-compass-no-such-directory-a7f3")),
            "a directory that does not exist cannot be written to"
        );
        // And the probe must not survive the check.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".ed-compass-write-probe")
            })
            .collect();
        assert!(leftovers.is_empty(), "the probe file was left behind");
    }

    #[test]
    fn the_config_path_is_always_absolute_and_named() {
        let p = Config::default_path();
        assert_eq!(p.file_name().unwrap(), "config.toml");
        assert!(p.parent().is_some(), "it must live in a directory: {p:?}");
    }

    #[test]
    fn a_config_from_the_old_overlay_layout_has_its_geometry_restored() {
        let dir =
            std::env::temp_dir().join(format!("ed-compass-overlay-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");

        // What the previous layout wrote: centre-left, sized for stacked lamps.
        let mut old = Config::default();
        old.overlay_layout_revision = 1;
        old.overlay_x_fraction = 0.375;
        old.overlay_y_fraction = 0.02;
        old.overlay_height = 78.0;
        old.view = "overlay".into(); // a view that no longer exists
        old.detect_keying = false; // a real preference, not geometry
        old.save(&path).expect("save");

        let loaded = Config::load_or_create(&path).expect("load");
        let d = Config::default();
        assert_eq!(loaded.overlay_x_fraction, d.overlay_x_fraction);
        assert_eq!(loaded.overlay_height, d.overlay_height);
        assert_eq!(loaded.overlay_layout_revision, OVERLAY_LAYOUT_REVISION);
        assert_eq!(
            loaded.view, "compact",
            "the overlay view has no window to open"
        );
        assert!(
            !loaded.detect_keying,
            "unrelated settings must be preserved"
        );

        // And the migration is not repeated once it has been written back.
        let mut moved = loaded;
        moved.overlay_x_fraction = 0.5;
        moved.save(&path).expect("save");
        let again = Config::load_or_create(&path).expect("reload");
        assert_eq!(again.overlay_x_fraction, 0.5, "a later move must stick");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_overlay_defaults_to_the_top_left_corner() {
        let cfg = Config::default();
        assert_eq!(cfg.overlay_x_fraction, 0.0);
        assert_eq!(cfg.overlay_y_fraction, 0.0);
    }

    #[test]
    fn an_out_of_range_label_fraction_is_rejected() {
        let mut cfg = Config::default();
        cfg.overlay_label_fraction = 0.95;
        assert!(cfg.validate().is_err());
        cfg.overlay_label_fraction = 0.0;
        assert!(cfg.validate().is_err());
        cfg.overlay_label_fraction = 0.34;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn matched_height_cancels_the_slope_change_from_cropping() {
        // Cropping 20-22050 down to 200-2400 magnifies frequency by 2.82x, so
        // the height must shrink by the same factor to keep stroke angles equal
        // to the published images.
        let mut cfg = Config::default();
        cfg.spectrogram_min_hz = 200.0;
        cfg.spectrogram_max_hz = 2400.0;
        cfg.export_height = 1600;

        let matched = cfg.matched_export_height(20.0, 22_050.0);
        let ratio = 1600.0 / matched as f32;
        assert!(
            (ratio - 2.82).abs() < 0.1,
            "expected a 2.82x reduction, got {ratio} (height {matched})"
        );

        // Showing the same band as the reference needs no correction at all.
        cfg.spectrogram_min_hz = 20.0;
        cfg.spectrogram_max_hz = 22_050.0;
        assert_eq!(cfg.matched_export_height(20.0, 22_050.0), 1600);
    }

    #[test]
    fn matched_height_survives_nonsense() {
        let cfg = Config::default();
        assert!(cfg.matched_export_height(0.0, 0.0) > 0);
        assert!(cfg.matched_export_height(100.0, 50.0) > 0);
    }

    #[test]
    fn ring_footprint_matches_spec_examples() {
        let cfg = Config::default();
        // 48 kHz stereo for 150 s ≈ 57.6 MB, 8 ch ≈ 230.4 MB.
        assert_eq!(cfg.pcm_ring_bytes(48_000, 2), 150 * 48_000 * 2 * 4);
        assert_eq!(cfg.pcm_ring_bytes(48_000, 8), 150 * 48_000 * 8 * 4);
    }

    #[test]
    fn ignore_bands_gate_frequencies() {
        let mut cfg = Config::default();
        cfg.ignore_bands.push(IgnoreBand {
            low_hz: 40.0,
            high_hz: 120.0,
        });
        assert!(cfg.is_ignored(60.0));
        assert!(!cfg.is_ignored(1000.0));
    }
}

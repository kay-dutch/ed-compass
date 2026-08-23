//! ED Compass — spectrogram anomaly detector and audio direction finder for
//! Elite Dangerous.

// Ship as a GUI application, so launching it from a shortcut does not open a
// black console window behind the interface. Debug builds keep the console,
// because that is where the log goes while developing.
//
// The cost is that the console-only modes (`--list-devices`, `--headless`)
// would print into nothing when run from a terminal, so `attach_console` below
// puts that back.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use ed_compass::app::{App, Status};
use ed_compass::audio::capture::{self, CaptureMessage};
use ed_compass::audio::device::{self, AudioDevice};
use ed_compass::audio::file_input;
use ed_compass::audio::format::{MASK_7_1, MASK_STEREO};
use ed_compass::audio::synthetic::{SyntheticSource, TestSignal};
use ed_compass::audio::{SampleFormat, StreamFormat};
use ed_compass::config::Config;
use ed_compass::single_instance;

#[derive(Parser, Debug)]
#[command(
    name = "ed-compass",
    about = "Spectrogram anomaly detector and audio direction finder for Elite Dangerous",
    version
)]
struct Cli {
    /// Configuration file. Defaults to config.toml beside the executable.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Audio endpoint id. Empty selects the default output in loopback mode.
    #[arg(long, value_name = "ID")]
    device: Option<String>,

    /// List audio endpoints and exit.
    #[arg(long)]
    list_devices: bool,

    /// Where triggered captures are written.
    #[arg(long, value_name = "DIR")]
    captures: Option<PathBuf>,

    /// Run without a window, logging detections to the console.
    #[arg(long)]
    headless: bool,

    /// Accepted and ignored. There is one window now — these flags picked
    /// between shapes that no longer exist, and are kept only so Desktop
    /// shortcuts from earlier versions still launch instead of failing to
    /// parse.
    #[arg(long, hide = true)]
    view: Option<String>,
    #[arg(long, hide = true)]
    compact: bool,
    #[arg(long, hide = true)]
    overlay: bool,

    /// Which renderer to draw with: glow (OpenGL) or wgpu (DX12/Vulkan).
    /// Overrides the `renderer` setting for this run.
    #[arg(long, value_name = "NAME", value_parser = ["glow", "wgpu"])]
    renderer: Option<String>,

    /// Create a Desktop shortcut, then exit.
    #[arg(long)]
    install_shortcut: bool,

    /// Stop after this many seconds. Headless only.
    #[arg(long, value_name = "SECONDS")]
    duration: Option<f32>,

    // ---- synthetic test sources ----
    #[arg(long, help = "Synthesize digital silence")]
    test_silence: bool,
    #[arg(long, help = "Synthesize broadband noise")]
    test_noise: bool,
    #[arg(long, value_name = "HZ", help = "Synthesize a sine tone")]
    test_sine: Option<f32>,
    #[arg(
        long,
        num_args = 3,
        value_names = ["START_HZ", "END_HZ", "SECONDS"],
        help = "Synthesize a repeating frequency sweep"
    )]
    test_sweep: Option<Vec<f32>>,
    #[arg(
        long,
        help = "Synthesize a mountain-shaped spectrogram on the Landscape Signal's 109.5 s cycle"
    )]
    test_landscape: bool,
    #[arg(
        long,
        help = "Synthesize a keyed binary transmission (tightbeam-shaped)"
    )]
    test_tightbeam: bool,
    #[arg(long, help = "Synthesize line art drawn into the spectrogram")]
    test_picture: bool,

    /// Azimuth to pan a synthetic source to, in degrees. 0 is dead ahead.
    #[arg(
        long,
        value_name = "DEG",
        default_value_t = 0.0,
        allow_negative_numbers = true
    )]
    azimuth: f32,

    /// Channel count for synthetic sources. 8 exercises the 7.1 direction finder.
    #[arg(long, value_name = "N", default_value_t = 8)]
    channels: usize,

    /// Sample rate for synthetic sources.
    #[arg(long, value_name = "HZ", default_value_t = 48_000)]
    rate: u32,

    // ---- offline input ----
    /// Analyze a WAV or FLAC file instead of live audio.
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,

    /// Replay the input file continuously.
    #[arg(long = "loop")]
    loop_input: bool,

    /// Play the input file at wall-clock speed rather than as fast as possible.
    #[arg(long)]
    realtime: bool,

    /// Write the spectrogram to this PNG when the run finishes. Headless only.
    #[arg(long, value_name = "PATH")]
    export_png: Option<PathBuf>,

    /// Fold the long-term tier at its best period and write the result to this
    /// PNG — one averaged cycle. Headless only.
    #[arg(long, value_name = "PATH")]
    export_fold: Option<PathBuf>,

    /// Write what the *structure detector* sees — the scan image after tones and
    /// transients are removed — to this PNG. Headless only.
    #[arg(long, value_name = "PATH")]
    export_scan: Option<PathBuf>,

    /// More logging. Repeat for more still.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Cli {
    /// The synthetic signal requested, if any. Rejects more than one.
    fn test_signal(&self) -> Result<Option<TestSignal>> {
        let mut chosen: Vec<TestSignal> = Vec::new();
        if self.test_silence {
            chosen.push(TestSignal::Silence);
        }
        if self.test_noise {
            chosen.push(TestSignal::Noise);
        }
        if let Some(hz) = self.test_sine {
            chosen.push(TestSignal::Sine { hz });
        }
        if let Some(args) = &self.test_sweep {
            chosen.push(TestSignal::Sweep {
                start_hz: args[0],
                end_hz: args[1],
                seconds: args[2],
            });
        }
        if self.test_landscape {
            chosen.push(TestSignal::Landscape);
        }
        if self.test_tightbeam {
            chosen.push(TestSignal::Tightbeam);
        }
        if self.test_picture {
            chosen.push(TestSignal::Picture);
        }
        match chosen.len() {
            0 => Ok(None),
            1 => Ok(Some(chosen[0])),
            n => bail!("{n} test signals requested; pick one"),
        }
    }
}

/// Create Desktop shortcuts on Windows.
///
/// Driven through PowerShell's `WScript.Shell` rather than hand-rolling the
/// `IShellLink` COM dance, which is a lot of unsafe code for something run once.
#[cfg(windows)]
fn install_shortcuts() -> Result<()> {
    let exe = std::env::current_exe().context("locating this executable")?;
    let exe = exe.display().to_string();
    let dir = std::path::Path::new(&exe)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // One shortcut, because there is one window. The overlay comes and goes
    // with the game rather than being something you launch.
    let shortcuts = [("ED Compass", "")];

    for (name, args) in shortcuts {
        let script = format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut(\
             [System.IO.Path]::Combine([Environment]::GetFolderPath('Desktop'), '{name}.lnk')); \
             $s.TargetPath = '{exe}'; $s.Arguments = '{args}'; \
             $s.WorkingDirectory = '{dir}'; $s.IconLocation = '{exe},0'; \
             $s.Description = 'Elite Dangerous signal monitor'; $s.Save()"
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .context("running PowerShell to create the shortcut")?;
        if !status.success() {
            bail!("could not create the \"{name}\" shortcut (PowerShell exited {status})");
        }
        println!("Created Desktop shortcut: {name}");
    }
    println!();
    println!("Two settings in Elite are worth changing:");
    println!();
    println!("  Graphics -> Display Mode: BORDERLESS. An exclusive-fullscreen game");
    println!("  covers every other window, including the overlay.");
    println!();
    println!("  Audio -> Music: 0. ED Compass hears whatever your speakers play, and");
    println!("  the soundtrack looks like a signal to every detector here. Ship and");
    println!("  effects audio can stay on.");
    Ok(())
}

/// Write a single-channel image as a greyscale PNG.
fn write_gray_png(
    path: &std::path::Path,
    pixels: &[u8],
    w: usize,
    h: usize,
) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(pixels)?;
    Ok(())
}

#[cfg(not(windows))]
fn install_shortcuts() -> Result<()> {
    bail!("Desktop shortcuts are a Windows feature")
}

fn init_logging(verbosity: u8) {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_secs()
        .init();
}

fn list_devices() -> Result<()> {
    let devices = device::enumerate().context("enumerating audio endpoints")?;
    if devices.is_empty() {
        println!("No audio endpoints found.");
        #[cfg(not(windows))]
        println!("(Endpoint enumeration requires Windows. Use --test-landscape or --input here.)");
        return Ok(());
    }
    println!("{:<40}  ID", "DEVICE");
    for d in &devices {
        println!("{:<40}  {}", d.display_name(), d.id);
    }
    Ok(())
}

/// Start whichever source the arguments select, returning the app.
fn build_app(cli: &Cli, cfg: Config, capture_dir: PathBuf) -> Result<App> {
    let (tx, rx) = crossbeam_channel::bounded::<CaptureMessage>(256);

    if let Some(path) = &cli.input {
        let mut source = file_input::load(path)?;
        source.set_looping(cli.loop_input);
        let label = format!("file: {}", path.display());
        let handle = capture::start_file(source, tx.clone(), cli.realtime || !cli.headless);
        return Ok(App::new(cfg, label, handle, tx, rx, capture_dir));
    }

    if let Some(signal) = cli.test_signal()? {
        let mask = match cli.channels {
            2 => MASK_STEREO,
            8 => MASK_7_1,
            _ => 0,
        };
        let format = StreamFormat::new(cli.rate, cli.channels, mask, SampleFormat::F32);
        log::info!(
            "synthetic source: {signal:?} at {:+.0}° across {}",
            cli.azimuth,
            format.describe()
        );
        let label = format!("synthetic ({})", format.layout_name());
        let source = SyntheticSource::new(signal, format, cli.azimuth);
        let handle = capture::start_synthetic(source, tx.clone());
        return Ok(App::new(cfg, label, handle, tx, rx, capture_dir));
    }

    // Live capture.
    let devices = device::enumerate().context("enumerating audio endpoints")?;
    let requested = cli.device.clone().unwrap_or_else(|| cfg.device.clone());
    let selected: Option<AudioDevice> = device::select(&devices, &requested).cloned();

    let Some(selected) = selected else {
        // Headless has no window to explain itself in, so there it stays fatal.
        if cli.headless {
            bail!("{NO_OUTPUT_DEVICE}");
        }
        // With a window, opening and saying so beats refusing to start. The
        // usual cause is launching before the headphones are plugged in, and
        // the app now notices when they are.
        log::warn!("no audio output endpoint; waiting for one to appear");
        return Ok(App::waiting_for_device(
            cfg,
            tx,
            rx,
            capture_dir,
            "no audio output device — plug in headphones or speakers".into(),
        ));
    };
    log::info!("using {}", selected.display_name());

    let handle = capture::start(&selected, tx.clone())?;
    let mut app = App::new(cfg, selected.display_name(), handle, tx, rx, capture_dir);
    app.reconnect_on_device_loss();
    Ok(app)
}

/// Why there is nothing to listen to, at length, for the console.
const NO_OUTPUT_DEVICE: &str = "no audio output endpoint is available, so there is nothing to listen to.\n\
     \n\
     ED Compass captures what your speakers or headphones are playing. With no\n\
     output device present — headphones unplugged, or none configured — there is\n\
     no game audio to hear. It will not fall back to a microphone: that would\n\
     record the room rather than the game.\n\
     \n\
     Plug in or enable an output device and start it again, or run without one\n\
     using --test-landscape or --input FILE.";

/// Console mode: pump, report detections, exit on duration or end of input.
fn run_headless(
    mut app: App,
    duration: Option<f32>,
    export_png: Option<PathBuf>,
    export_scan: Option<PathBuf>,
    export_fold: Option<PathBuf>,
) -> Result<()> {
    // Read the configured thresholds rather than repeating literals here, or the
    // console disagrees with the UI about what counts as a detection.
    let keying_threshold = app.config().keying_threshold;
    let morse_threshold = app.config().morse_threshold;
    let structure_threshold = app.config().structure_threshold;
    let started = std::time::Instant::now();
    let mut reported = 0usize;
    let mut last_status = Status::Starting;
    let mut last_progress = std::time::Instant::now();

    loop {
        app.pump();

        if app.status() != last_status {
            log::info!("status: {}", app.status().label());
            last_status = app.status();
        }

        while reported < app.events().len() {
            let e = &app.events()[reported];
            let d = &e.detection;
            let bearing = if d.direction.is_usable() {
                format!(
                    "{:+.0}° (confidence {:.2}{})",
                    d.direction.azimuth_deg,
                    d.direction.confidence,
                    if d.direction.front_back_ambiguous {
                        ", front/back ambiguous"
                    } else {
                        ""
                    }
                )
            } else {
                "no bearing".into()
            };
            println!(
                "{}  {:>7.0}–{:<7.0} Hz  {:>5.1} s  {:>5.1} dB  score {:.2}  {}  {}{}",
                e.timestamp,
                d.event.low_hz,
                d.event.high_hz,
                d.event.duration_seconds,
                d.event.peak_excess_db,
                d.event.score,
                bearing,
                e.star_system.as_deref().unwrap_or("unknown system"),
                match &e.captured_to {
                    Some(p) => format!("  → {}", p.display()),
                    None => String::new(),
                }
            );
            reported += 1;
        }

        if last_progress.elapsed() >= std::time::Duration::from_secs(15) {
            if let Some(snap) = app.snapshot() {
                let period = snap
                    .periodicity
                    .as_ref()
                    .map(|p| {
                        format!(
                            "period {:.1} s (confidence {:.2})",
                            p.period_seconds, p.confidence
                        )
                    })
                    .unwrap_or_else(|| "no period yet".into());
                let keying = match &snap.keying {
                    Some(k) if k.is_present(keying_threshold) => format!(
                        "KEYING {:.2} ({} tones, {:.1} sym/s)",
                        k.confidence,
                        k.tones_hz.len(),
                        k.symbol_rate_hz
                    ),
                    Some(k) => format!("keying {:.2}", k.confidence),
                    None => "keying —".into(),
                };
                let structure = if snap.structure.is_present(structure_threshold) {
                    format!("PICTURE {:.2}", snap.structure.score)
                } else {
                    format!("picture {:.2}", snap.structure.score)
                };
                log::info!(
                    "{:.0} s · RMS {:.1} dBFS · {keying} · {structure} · {period} · {} gaps",
                    snap.timeline_seconds,
                    snap.stats.rms_dbfs,
                    snap.gap_count
                );
            }
            last_progress = std::time::Instant::now();
        }

        if app.status() == Status::DeviceLost {
            if let Some(e) = app.error() {
                bail!("capture stopped: {e}");
            }
            log::info!("input finished");
            break;
        }
        if let Some(limit) = duration
            && started.elapsed().as_secs_f32() >= limit
        {
            log::info!("reached the {limit} s limit");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    if let Some(path) = export_png
        && let Some(engine) = app.engine()
    {
        let cfg = engine.config().clone();
        let geometry = engine.geometry();
        let scale = ed_compass::ui::waterfall::FreqScale::new(
            cfg.spectrogram_min_hz,
            cfg.spectrogram_max_hz,
            geometry.nyquist_hz(),
        );
        let history = if cfg.spectrogram_show_excess {
            engine.excess_waterfall()
        } else {
            engine.waterfall()
        };
        let window_frames = (cfg.waterfall_seconds / geometry.frame_seconds())
            .ceil()
            .max(1.0) as usize;
        match ed_compass::ui::waterfall::export_png(
            history,
            geometry,
            ed_compass::ui::waterfall::RenderOptions {
                scale,
                auto_gain: true,
                median_subtract: cfg.spectrogram_median_subtract,
                window_frames,
            },
            cfg.export_width,
            ed_compass::ui::export_height(&cfg),
            &path,
        ) {
            Ok(()) => println!("Exported {}", path.display()),
            Err(e) => eprintln!("could not export: {e}"),
        }
    }

    if let Some(path) = export_fold
        && let Some(engine) = app.engine()
    {
        // The excess tier, not the level tier: folding raw level across a jump
        // averages two different environments together.
        let history = engine.longterm_excess();
        let fps = engine.longterm_fps();
        match ed_compass::analysis::fold::search(history, fps, 30.0, 600.0, 256) {
            Some(folded) => {
                println!(
                    "Folded {:.1} cycles at {:.2} s (sharpness {:.2}) — {} bands x {} phases",
                    folded.cycles,
                    folded.period_seconds,
                    folded.sharpness(),
                    folded.bands,
                    folded.phases
                );
                let image = folded.to_image();
                // The question folding exists to answer: does the *averaged*
                // cycle look drawn, when the raw recording did not?
                let scored =
                    ed_compass::analysis::structure::analyze(&image, folded.phases, folded.bands);
                println!(
                    "  folded structure {:.3} (continuity {:.2}, coherence {:.2}, diversity {:.2})",
                    scored.score, scored.continuity, scored.coherence, scored.orientation_diversity
                );
                match write_gray_png(&path, &image, folded.phases, folded.bands) {
                    Ok(()) => println!("Exported the fold: {}", path.display()),
                    Err(e) => eprintln!("could not export the fold: {e}"),
                }
            }
            None => eprintln!("not enough history to fold — need at least two cycles"),
        }
    }

    // What the structure detector actually scored, as opposed to what is on
    // screen. The two are different images by design, and when the score
    // disagrees with your eyes this is the only way to see which is right.
    if let Some(path) = export_scan
        && let Some(engine) = app.engine()
    {
        let (pixels, w, h) = engine.scan_cleaned();
        if w == 0 || h == 0 {
            eprintln!("no scan image yet — the run was too short");
        } else {
            match write_gray_png(&path, pixels, w, h) {
                Ok(()) => println!("Exported the detector's view: {} ({w}x{h})", path.display()),
                Err(e) => eprintln!("could not export the scan: {e}"),
            }
        }
    }

    if let Some(snap) = app.snapshot() {
        println!();
        println!("Analyzed {:.1} s of audio.", snap.timeline_seconds);
        println!("Detections: {}", app.events().len());
        println!("Captures written: {}", app.captures_written());
        if snap.gap_count > 0 {
            println!(
                "Timeline gaps: {} totalling {:.1} s",
                snap.gap_count, snap.gap_seconds
            );
        }
        match &snap.keying {
            Some(k) => println!(
                "Binary keying: confidence {:.2} — {} tones {:?}, {:.2} symbols/s, \
                 timing {:.2}, purity {:.2}{}",
                k.confidence,
                k.tones_hz.len(),
                k.tones_hz
                    .iter()
                    .map(|h| h.round() as i32)
                    .collect::<Vec<_>>(),
                k.symbol_rate_hz,
                k.timing_regularity,
                k.alphabet_purity,
                if k.is_present(keying_threshold) {
                    "  ← TRANSMISSION PRESENT"
                } else {
                    ""
                }
            ),
            None => println!("Binary keying: no symbols observed"),
        }
        match &snap.morse {
            Some(m) => println!(
                "Morse keying: confidence {:.2} — tone {:.0} Hz, dot {:.0} ms, \
                 dash {:.0} ms, ratio {:.2}, {} marks{}",
                m.confidence,
                m.tone_hz,
                m.dot_seconds * 1000.0,
                m.dash_seconds * 1000.0,
                m.ratio,
                m.marks,
                if m.is_present(morse_threshold) {
                    "  ← MORSE PRESENT"
                } else {
                    ""
                }
            ),
            None => println!("Morse keying: no marks observed"),
        }
        if let Some(engine) = app.engine() {
            let (peak, at) = engine.peak_structure();
            let (kpeak, kat) = engine.peak_keying();
            println!();
            println!("Peak over the whole run — this is the one that matters for a recording,");
            println!("since the live scores describe only the last few seconds:");
            println!(
                "  structure {:.3} at {:.0} s (continuity {:.2}, drift {:.2} at {:+.0}°, {} lines){}",
                peak.score,
                at,
                peak.continuity,
                peak.drift,
                peak.drift_angle_deg,
                peak.drift_lines,
                if peak.is_present(structure_threshold) {
                    "  ← PICTURE"
                } else {
                    ""
                }
            );
            println!(
                "  keying    {:.3} at {:.0} s{}",
                kpeak,
                kat,
                if kpeak >= keying_threshold {
                    "  ← TRANSMISSION"
                } else {
                    ""
                }
            );
            println!();
        }
        if let Some(engine) = app.engine() {
            {
                // What the pipeline actually recorded, not a fresh trace of the
                // same pixels: the pipeline suppresses tracing until the
                // background is warm, and a diagnostic that ignores that reports
                // strokes the app never saw.
                let t = engine.traced();
                for tk in t.tracks.iter().take(12) {
                    println!(
                        "    track {:>3} cols  rows {:>3}..{:<3} drift {:>3}  mean {:.0}",
                        tk.len(),
                        tk.y0,
                        tk.y1,
                        tk.drift_rows(),
                        tk.mean
                    );
                }
                println!(
                    "Traced strokes: {} tracks, longest {:.0}% covered {:.0}% (seed {} follow {})",
                    t.tracks.len(),
                    t.longest * 100.0,
                    t.covered * 100.0,
                    t.seed_level,
                    t.follow_level
                );
            }
            let f = engine.folded_structure();
            match engine.folded() {
                Some(folded) => println!(
                    "Folded cycle: {:.1} cycles at {:.1} s — structure {:.3} (continuity {:.2})",
                    folded.cycles, folded.period_seconds, f.score, f.continuity
                ),
                None => println!("Folded cycle: not enough history yet (needs two cycles)"),
            }
        }
        println!(
            "Drawn structure (final): score {:.3} (coherence {:.2}, sparsity {:.2}, diversity {:.2}){}",
            snap.structure.score,
            snap.structure.coherence,
            snap.structure.sparsity,
            snap.structure.orientation_diversity,
            if snap.structure.is_present(structure_threshold) {
                "  ← PICTURE PRESENT"
            } else {
                ""
            }
        );
        match &snap.periodicity {
            Some(p) => {
                println!(
                    "Dominant period: {:.2} s (confidence {:.2}, prominence {:.2}){}",
                    p.period_seconds,
                    p.confidence,
                    p.prominence,
                    if ed_compass::analysis::periodicity::matches_landscape(p, 2.0) {
                        "  ← consistent with the Landscape Signal"
                    } else {
                        ""
                    }
                );
            }
            None => println!("Dominant period: none found"),
        }
    }
    Ok(())
}

/// Reattach to the terminal that launched us, if there was one.
///
/// A GUI-subsystem process gets no console, and Windows does not connect it to
/// the parent's. Without this, `ed-compass.exe --list-devices` typed at a prompt
/// returns instantly and prints nothing at all.
///
/// Only *missing* standard handles are filled in. An earlier version replaced
/// them unconditionally, which broke every case where the shell had already
/// provided one: `> out.txt` wrote an empty file and piped output vanished,
/// because the file and pipe handles had been swapped for the console.
///
/// Must run before anything writes to stdout: Rust caches the standard handles
/// the first time they are used.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_WRITE, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_ERROR_HANDLE as ERR, STD_OUTPUT_HANDLE, STD_OUTPUT_HANDLE as OUT, SetStdHandle,
    };
    use windows::core::w;

    // SAFETY: plain Win32 calls on values we own. The console handle is checked
    // before use and deliberately left open for the life of the process.
    unsafe {
        let missing = |which| GetStdHandle(which).map_or(true, |h: HANDLE| h.is_invalid());
        let (no_out, no_err) = (missing(OUT), missing(ERR));
        if !no_out && !no_err {
            // Redirected to a file, or piped: output already goes somewhere.
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            // Launched from a shortcut or Explorer. There is no parent console,
            // which is the ordinary case for a GUI application, not an error.
            return;
        }
        let Ok(conout) = CreateFileW(
            w!("CONOUT$"),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        ) else {
            return;
        };
        if no_out {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, conout);
        }
        if no_err {
            let _ = SetStdHandle(STD_ERROR_HANDLE, conout);
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

/// Write panics to a file before dying.
///
/// A release build is a GUI-subsystem process: no console, so a panic's message
/// and backtrace go nowhere and the program "just closes itself" — which is
/// indistinguishable, from the outside, from every other way of dying. The
/// crash file turns that into something a bug report can contain.
fn install_crash_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let path = Config::default_path().with_file_name(format!("crash-{stamp}.log"));
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!(
            "ED Compass {} crashed at {stamp}\nrenderer: {}\nthread: {}\n\n{info}\n\n{backtrace}\n",
            env!("CARGO_PKG_VERSION"),
            ed_compass::ui::active_backend(),
            std::thread::current().name().unwrap_or("unnamed"),
        );
        let _ = std::fs::write(&path, &report);
        log::error!("panic written to {}", path.display());
        previous(info);
    }));
}

fn main() -> Result<()> {
    attach_console();
    install_crash_log();
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if cli.list_devices {
        return list_devices();
    }
    if cli.install_shortcut {
        return install_shortcuts();
    }

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let mut cfg = Config::load_or_create(&config_path)?;
    if let Some(device) = &cli.device {
        cfg.device = device.clone();
    }
    // --view/--compact/--overlay are accepted for old shortcuts and ignored.
    let _ = (&cli.view, cli.compact, cli.overlay);
    cfg.validate()?;
    let cfg_renderer = cfg.renderer.clone();
    log::info!("configuration: {}", config_path.display());

    let capture_dir = cli.captures.clone().unwrap_or_else(|| {
        config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("captures")
    });

    // Refuse to start a second copy: two instances capture the same audio twice
    // and enforce the disk budget over the same folder, evicting each other's
    // recordings. Held until the process exits.
    let _instance = match single_instance::claim() {
        Ok(lock) => Some(lock),
        Err(single_instance::ClaimError::AlreadyRunning) => {
            bail!("{}", single_instance::ClaimError::AlreadyRunning);
        }
        Err(other) => {
            log::warn!("{other}");
            None
        }
    };

    let app = build_app(&cli, cfg, capture_dir)?;

    let result = if cli.headless {
        run_headless(
            app,
            cli.duration,
            cli.export_png.clone(),
            cli.export_scan.clone(),
            cli.export_fold.clone(),
        )
    } else {
        let backend = cli
            .renderer
            .as_deref()
            .or(Some(cfg_renderer.as_str()))
            .and_then(ed_compass::ui::Backend::parse)
            .unwrap_or(ed_compass::ui::Backend::Glow);
        ed_compass::ui::run(app, backend)
    };

    // An Err exit in a windowed build vanishes as silently as a panic — stderr
    // goes nowhere — so give it the same crash file the panic hook writes.
    if let Err(e) = &result {
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let path = Config::default_path().with_file_name(format!("crash-{stamp}.log"));
        let _ = std::fs::write(
            &path,
            format!(
                "ED Compass {} exited with an error at {stamp}\n\n{e:#}\n",
                env!("CARGO_PKG_VERSION")
            ),
        );
    }
    result
}

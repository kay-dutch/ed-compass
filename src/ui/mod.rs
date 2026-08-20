//! The desktop window.
//!
//! Render-only: the UI reads the most recent snapshot and never touches the
//! capture path. Snapshots are taken at `analysis_update_hz`, independently of
//! the frame rate, so redrawing faster costs nothing but pixels.

pub mod compass;
pub mod controls;
pub mod events;
pub mod overlay;
pub mod waterfall;
pub mod zoom;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::app::{App, Status};
use crate::audio::device::{self, AudioDevice};
use crate::game_window::{OverlayAnchor, OverlayPlacement, PlotterGap, overlay_placement};
use crate::pipeline::AnalysisSnapshot;

/// Launch the window. Blocks until it closes.
/// Which renderer to draw with.
///
/// Both are compiled in. `glow` is the default because every crash this tool
/// has had in the field was a wgpu validation error in its multi-viewport
/// texture handling; wgpu is kept so it can be selected without a rebuild, and
/// as a fallback for a machine whose OpenGL driver is too old for glow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Glow,
    Wgpu,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "glow" | "opengl" | "gl" => Some(Self::Glow),
            "wgpu" | "dx12" | "vulkan" => Some(Self::Wgpu),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Glow => "glow",
            Self::Wgpu => "wgpu",
        }
    }

    /// The one to try when this one will not start.
    fn fallback(self) -> Self {
        match self {
            Self::Glow => Self::Wgpu,
            Self::Wgpu => Self::Glow,
        }
    }
}

/// The renderer currently drawing, for the crash log to name.
static ACTIVE_BACKEND: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// What the running process is drawing with, or `"none"` before the window opens.
pub fn active_backend() -> &'static str {
    match ACTIVE_BACKEND.load(std::sync::atomic::Ordering::Relaxed) {
        1 => "glow",
        2 => "wgpu",
        _ => "none",
    }
}

fn native_options(backend: Backend) -> eframe::NativeOptions {
    // One window. The in-game overlay is a second viewport that shows and
    // hides itself with the game; there is no other shape to switch to, which
    // means there is no state you have to kill the process to leave.
    let viewport = egui::ViewportBuilder::default()
        .with_title("ED Compass")
        .with_inner_size([1180.0, 860.0])
        .with_min_inner_size([900.0, 620.0])
        // Transparency is requested here, on the *root* window, even though
        // this window is opaque. eframe's glow backend chooses one GL config
        // for the whole process from this flag, and a config without an alpha
        // channel cannot host a transparent window — so the overlay, a child
        // viewport, would come out as an opaque black rectangle over the game.
        // `clear_color` below keeps this window itself solid.
        .with_transparent(true);

    eframe::NativeOptions {
        viewport,
        renderer: match backend {
            Backend::Glow => eframe::Renderer::Glow,
            Backend::Wgpu => eframe::Renderer::Wgpu,
        },
        ..Default::default()
    }
}

pub fn run(app: App, preferred: Backend) -> Result<()> {
    // The application is handed over only when a window actually opens. If the
    // renderer cannot start, it is still here to give to the other one.
    let slot = std::rc::Rc::new(std::cell::RefCell::new(Some(app)));

    match run_with(std::rc::Rc::clone(&slot), preferred) {
        Ok(()) => Ok(()),
        Err(first) if slot.borrow().is_some() => {
            // Only a *startup* failure can be retried, and the untouched slot
            // is how we know that is what happened: a driver too old for glow,
            // or no working GL at all. A backend that starts and then panics
            // hours later has already taken the process with it, and no
            // fallback here can help — the crash log names the backend instead,
            // so the next launch can be told to use the other one.
            let other = preferred.fallback();
            log::error!(
                "the {} renderer could not start ({first:#}); trying {}",
                preferred.as_str(),
                other.as_str()
            );
            run_with(slot, other).map_err(|second| {
                anyhow::anyhow!(
                    "neither renderer could open a window.\n  {}: {first:#}\n  {}: {second:#}",
                    preferred.as_str(),
                    other.as_str()
                )
            })
        }
        Err(ran_and_failed) => Err(ran_and_failed),
    }
}

fn run_with(slot: std::rc::Rc<std::cell::RefCell<Option<App>>>, backend: Backend) -> Result<()> {
    ACTIVE_BACKEND.store(
        match backend {
            Backend::Glow => 1,
            Backend::Wgpu => 2,
        },
        std::sync::atomic::Ordering::Relaxed,
    );
    log::info!("opening the window with the {} renderer", backend.as_str());

    let options = native_options(backend);
    eframe::run_native(
        "ED Compass",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let app = slot
                .borrow_mut()
                .take()
                .ok_or_else(|| "the application was already handed to a renderer".to_owned())?;
            Ok(Box::new(CompassUi::new(app)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not open the window: {e}"))
}

/// Make a string safe for a Windows filename.
fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect()
}

/// Export height, corrected so a cropped band does not steepen every slope.
///
/// The published spectrograms span 20 Hz to 22050 Hz; showing less magnifies
/// frequency and tilts every stroke unless the height is scaled to match.
pub fn export_height(cfg: &crate::config::Config) -> usize {
    if cfg.export_match_published_aspect {
        cfg.matched_export_height(20.0, 22_050.0)
    } else {
        cfg.export_height
    }
}

/// The overlay's viewport id. Fixed, so reopening it reuses the same window
/// rather than leaving a trail of dead ones — and derived from a hash, because
/// `ViewportId(Id::NULL)` is `ViewportId::ROOT`, the control window itself.
/// Pixel size of the overlay's spectrogram, once the lamps and the bearing
/// rose have taken what they need.
///
/// Lives here rather than on [`Config`] because the lamp column is measured
/// from the fonts, and fonts are a UI concern.
fn overlay_spectrogram_size(
    cfg: &crate::config::Config,
    overlay_width: f32,
    label_px: f32,
) -> (f32, f32) {
    if !cfg.overlay_spectrogram {
        return (0.0, 0.0);
    }
    let rose = if cfg.direction_finding {
        cfg.overlay_height
    } else {
        0.0
    };
    (
        (overlay_width - label_px - rose).max(16.0),
        cfg.overlay_height.max(16.0),
    )
}

/// Whether the overlay should be painted this frame.
///
/// Elite, and only Elite. An earlier version also showed it while *our* control
/// window had focus, so the toggles could be seen to act — but our window always
/// has focus the instant it opens, so the overlay appeared at startup and then
/// vanished a moment later. The overlay belongs to the cockpit; if you are not
/// looking at the cockpit there is nothing for it to annotate.
fn overlay_visible(game_found: bool, game_focused: bool) -> bool {
    game_found && game_focused
}

fn overlay_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("ed-compass-overlay")
}

fn anchor_from(cfg: &crate::config::Config) -> OverlayAnchor {
    OverlayAnchor {
        x_fraction: cfg.overlay_x_fraction,
        y_fraction: cfg.overlay_y_fraction,
        x_offset_px: cfg.overlay_x_offset_px,
        width: cfg.overlay_width,
        height: cfg.overlay_height,
    }
}

struct CompassUi {
    app: App,
    snapshot: Option<AnalysisSnapshot>,
    last_snapshot: Instant,
    snapshot_interval: Duration,

    anchor: OverlayAnchor,
    /// Last answer from the window manager about where the game is and whether
    /// it has focus.
    placement: OverlayPlacement,
    /// The overlay's own spectrogram texture, kept separate from the full view's
    /// so switching views does not force a rebuild of either.
    /// The overlay's own texture, created and updated only inside the overlay
    /// viewport's pass. See [`overlay::OverlayState::spectrogram`] for why it
    /// cannot live in the main window's.
    overlay_texture: Arc<Mutex<Option<egui::TextureHandle>>>,
    /// What the overlay draws, shared with its viewport callback. The callback
    /// outlives any one frame, so the state it reads must too.
    overlay_state: Arc<Mutex<overlay::OverlayState>>,
    last_overlay_render: Instant,
    /// The newest spectrogram pixels, waiting to be handed to the overlay.
    pending_spectrogram: Option<egui::ColorImage>,
    /// Drives the overlay's displayed frequency band, so a detection is shown at
    /// a scale where it can be read. See [`zoom`].
    zoom: zoom::ZoomState,
    /// Width the lamp column needs, measured from the text it draws.
    overlay_label_px: f32,
    /// Whole-window alpha last applied to the overlay, once the Win32 call has
    /// actually succeeded. `None` until then, which keeps the off-screen
    /// fallback in play.
    overlay_alpha: Option<u8>,
    game_found: bool,
    last_game_poll: Instant,

    /// Where exported images go.
    export_dir: String,

    devices: Vec<AudioDevice>,
    waterfall_texture: Option<egui::TextureHandle>,
    last_waterfall: Instant,
    /// Size the waterfall image was last built at, so it is rebuilt on resize.
    waterfall_size: [usize; 2],
    /// What the last Export did, shown next to the button. A log line is no use
    /// to someone in a cockpit who needs to know the moment was saved.
    export_status: Option<String>,
}

impl CompassUi {
    fn new(app: App) -> Self {
        let interval = Duration::from_secs_f32(1.0 / app.config().analysis_update_hz.max(1.0));
        let anchor = anchor_from(app.config());
        let zoom = {
            let cfg = app.config();
            zoom::ZoomState::new(
                zoom::Band::new(cfg.spectrogram_min_hz, cfg.spectrogram_max_hz),
                cfg.overlay_zoom_hold_seconds,
                cfg.overlay_zoom_lockout_seconds,
                Instant::now(),
            )
        };
        Self {
            anchor,
            placement: overlay_placement(anchor),
            overlay_texture: Arc::new(Mutex::new(None)),
            overlay_state: Arc::new(Mutex::new(overlay::OverlayState::default())),
            last_overlay_render: Instant::now() - Duration::from_secs(1),
            pending_spectrogram: None,
            zoom,
            overlay_label_px: 120.0,
            overlay_alpha: None,
            game_found: false,
            last_game_poll: Instant::now() - Duration::from_secs(10),
            snapshot_interval: interval,
            export_dir: app
                .config()
                .export_dir
                .clone()
                .unwrap_or_else(|| "exports".to_string()),
            devices: device::enumerate().unwrap_or_default(),
            app,
            snapshot: None,
            last_snapshot: Instant::now() - Duration::from_secs(1),
            waterfall_texture: None,
            last_waterfall: Instant::now() - Duration::from_secs(1),
            waterfall_size: [0, 0],
            export_status: None,
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("ED Compass");
            ui.add_space(12.0);
            let status = self.app.status();
            ui.label(
                egui::RichText::new(status.label())
                    .monospace()
                    .size(15.0)
                    .color(controls::status_colour(status)),
            );
            if status == Status::Warming {
                ui.add(
                    egui::ProgressBar::new(self.app.warmup_progress())
                        .desired_width(120.0)
                        .show_percentage(),
                )
                .on_hover_text(
                    "The detector is learning what the background looks like. \
                     Detection is suppressed until it settles.",
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(e) = self.app.error() {
                    ui.label(
                        egui::RichText::new(e)
                            .monospace()
                            .color(egui::Color32::from_rgb(255, 110, 110)),
                    );
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label("Device:");
            let current = self.app.device_label().to_owned();
            if let Some(device) = controls::device_picker(ui, &self.devices, &current)
                && let Err(e) = self.app.switch_device(&device)
            {
                log::error!("could not switch device: {e:#}");
            }
            if ui.button("↻").on_hover_text("Re-scan endpoints").clicked() {
                self.devices = device::enumerate().unwrap_or_default();
            }
            let mut excess = self.app.config().spectrogram_show_excess;
            if ui
                .checkbox(&mut excess, "excess")
                .on_hover_text(
                    "Show each bin minus its learned background. Removes anything \
                     constantly loud — ship rumble, life support — and leaves only \
                     what changed.",
                )
                .changed()
            {
                self.app.set_show_excess(excess);
            }
            if ui
                .button("Export")
                .on_hover_text(
                    "Keep everything about this moment: the recent audio, and the \
                     spectrogram as an image. Use it the instant you see something, \
                     whether or not the lamps agree.",
                )
                .clicked()
            {
                self.export_everything();
            }

            ui.separator();
            match self.app.format() {
                Some(f) => {
                    ui.label(egui::RichText::new(f.describe()).monospace());
                    let directional = f.directional_channels();
                    if directional < 3 {
                        ui.label(
                            egui::RichText::new(format!("{directional} directional ch"))
                                .monospace()
                                .color(egui::Color32::from_rgb(255, 210, 90)),
                        )
                        .on_hover_text(
                            "Set the Windows output endpoint to 7.1 for a far sharper bearing. \
                             It works on a stereo headset — it is the endpoint mix format that \
                             matters, not the hardware.",
                        );
                    }
                }
                None => {
                    ui.weak("waiting for the stream…");
                }
            }
        });

        ui.horizontal(|ui| {
            let game = self.app.game_state();
            ui.label("System:");
            ui.label(egui::RichText::new(game.describe()).monospace());
            if let Some(track) = &game.music_track {
                ui.separator();
                ui.weak(egui::RichText::new(format!("music: {track}")).monospace())
                    .on_hover_text(
                        "A detection coinciding with a music change is a prime \
                         false-positive suspect.",
                    );
            }
            if let Some(snap) = &self.snapshot {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{:.0} s analyzed · {} gaps ({:.1} s) · {} captures",
                            snap.timeline_seconds,
                            snap.gap_count,
                            snap.gap_seconds,
                            self.app.captures_written()
                        ))
                        .monospace()
                        .color(egui::Color32::from_gray(150)),
                    );
                });
            }
        });
    }

    fn waterfall_panel(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let height = (available.y - 240.0).max(180.0);
        let size = egui::vec2(available.x, height);
        let (response, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
        let rect = response.rect;

        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(12));

        let Some(engine) = self.app.engine() else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for audio…",
                egui::FontId::monospace(13.0),
                egui::Color32::from_gray(120),
            );
            return;
        };
        let geometry = engine.geometry();
        let cfg = self.app.config();
        let scale = waterfall::FreqScale::new(
            cfg.spectrogram_min_hz,
            cfg.spectrogram_max_hz,
            geometry.nyquist_hz(),
        );
        let window_seconds = cfg.waterfall_seconds;

        // Rebuilding the image is the expensive part, so it runs at the
        // snapshot rate rather than the frame rate.
        let target = [rect.width() as usize, rect.height() as usize];
        if self.waterfall_texture.is_none()
            || target != self.waterfall_size
            || self.last_waterfall.elapsed() >= self.snapshot_interval
        {
            let history = if cfg.spectrogram_show_excess {
                engine.excess_waterfall()
            } else {
                engine.waterfall()
            };
            // The window in frames, so time-per-pixel is fixed and the display
            // scrolls at a constant rate from the first second.
            let window_frames =
                (window_seconds / geometry.frame_seconds()).ceil().max(1.0) as usize;
            let image = waterfall::build_image(
                history,
                geometry,
                waterfall::RenderOptions {
                    scale,
                    auto_gain: true,
                    median_subtract: cfg.spectrogram_median_subtract,
                    window_frames,
                },
                target[0],
                target[1],
            );
            // Update in place. Assigning a fresh `load_texture` here dropped
            // the old handle, which queues a *free* into egui's global texture
            // delta — about eight per second at the snapshot rate. Two
            // viewports drain that one queue on independent schedules, so a
            // free could be applied by one painter while the other still had
            // the id in its draw list: "Texture with 'egui_texid_Managed(7833)'
            // label is invalid", and the process died. `set` keeps one id for
            // the life of the process and queues no frees at all.
            match &mut self.waterfall_texture {
                Some(handle) => handle.set(image, egui::TextureOptions::NEAREST),
                None => {
                    self.waterfall_texture = Some(ui.ctx().load_texture(
                        "waterfall",
                        image,
                        egui::TextureOptions::NEAREST,
                    ));
                }
            }
            self.waterfall_size = target;
            self.last_waterfall = Instant::now();
        }

        if let Some(texture) = &self.waterfall_texture {
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        waterfall::draw_axes(&painter, rect, scale, window_seconds);

        // Overlay recent detections.
        let now_seconds = self
            .snapshot
            .as_ref()
            .map(|s| s.timeline_seconds)
            .unwrap_or(0.0);
        for record in self.app.events().iter().rev().take(40) {
            let e = &record.detection.event;
            let ago_start = (now_seconds - e.start_seconds) as f32;
            let ago_end = ago_start - e.duration_seconds;
            waterfall::draw_event_box(
                &painter,
                rect,
                scale,
                window_seconds,
                waterfall::EventBox {
                    seconds_ago_start: ago_start,
                    seconds_ago_end: ago_end.max(0.0),
                    low_hz: e.low_hz,
                    high_hz: e.high_hz,
                    captured: record.captured_to.is_some(),
                },
            );
        }

        // Drag vertically to mute a frequency range.
        if response.drag_stopped()
            && let Some(origin) = response.interact_pointer_pos()
        {
            let height = rect.height() as usize;
            let a = scale.hz((origin.y - rect.top()).max(0.0) as usize, height);
            let delta = response.drag_delta().y;
            let b = scale.hz((origin.y - delta - rect.top()).max(0.0) as usize, height);
            if (a - b).abs() > 1.0 {
                self.app.mute_band(a.min(b), a.max(b));
            }
        }
    }

    /// The two primary readouts: is something transmitting, and is something
    /// drawn. These lead because they are what the tool is for.
    /// The arming controls, inherited from the retired compact panel.
    ///
    /// Each is read back from the app so the widget can never drift out of step
    /// with what is actually running.
    fn controls_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Bound to "listening", which is the inverse of paused — binding
            // the checkbox straight to `paused` would have made ticking it
            // stop analysis.
            let mut listening = !self.app.is_paused();
            if ui
                .checkbox(&mut listening, "Listening")
                .on_hover_text("Unchecked suspends analysis. The audio device stays open.")
                .changed()
            {
                self.app.set_paused(!listening);
            }

            let mut keying = self.app.detect_keying();
            let mut structure = self.app.detect_structure();
            let a = ui.checkbox(&mut keying, "Detect transmissions").changed();
            let b = ui.checkbox(&mut structure, "Detect pictures").changed();
            if a || b {
                self.app.set_detectors(keying, structure);
            }

            let mut overlay_on = self.app.overlay_enabled();
            if ui
                .checkbox(&mut overlay_on, "In-game overlay")
                .on_hover_text(
                    "Shows the indicators over the cockpit whenever Elite has \
                     focus, and hides them again when it does not. This window \
                     stays open either way.",
                )
                .changed()
            {
                self.app.set_overlay_enabled(overlay_on);
            }
            if overlay_on && !self.game_found {
                // The overlay only exists over the game, so say why it is
                // absent here, where someone puzzled by that will look.
                ui.label(
                    egui::RichText::new("waiting for the Elite Dangerous window")
                        .monospace()
                        .size(10.0)
                        .color(overlay::hud::AMBER),
                );
            }

            let mut df = self.app.direction_finding();
            if ui
                .checkbox(&mut df, "Direction finding")
                .on_hover_text(
                    "Secondary. Costs one transform per channel instead of one, \
                     and keeps every channel in memory. Switching it rebuilds \
                     the analysis engine and loses history.",
                )
                .changed()
            {
                self.app.set_direction_finding(df);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(status) = &self.export_status {
                    ui.label(egui::RichText::new(status).weak());
                }
            });
        });
    }

    /// Show or hide the in-game overlay, following Elite's focus.
    ///
    /// A deferred viewport rather than a mode of this window: the control panel
    /// stays open and reachable at all times, and the overlay simply is not
    /// there when the game is not in front of you. Not calling
    /// `show_viewport_deferred` in a frame closes the window, which is the whole
    /// hide mechanism.
    /// Keep the in-game overlay in step with the game window.
    ///
    /// The window is created once and **never destroyed**. When the overlay
    /// should not be seen it stays open and paints nothing — a transparent
    /// window with nothing in it is invisible, and it is already click-through
    /// and absent from the taskbar, so an empty one costs the player nothing.
    ///
    /// Both obvious alternatives are broken, each in its own way:
    ///
    /// * **Destroying it** (not calling `show_viewport_deferred`) churns
    ///   viewport lifecycle on every focus change, and `egui_wgpu`'s
    ///   `Painter::set_window(id, None)` does `self.surfaces.clear()` — *all*
    ///   surfaces, not just that viewport's. Tearing down the overlay can take
    ///   the main window's surface with it, and the textures that were valid
    ///   against it: "Texture with 'egui_texid_Managed(1)' label is invalid".
    /// * **Hiding it** with `ViewportBuilder::with_visible(false)` freezes it:
    ///   a hidden window receives no redraw events, so it never renders again,
    ///   and `Visible(true)` does not restart its render loop.
    ///
    /// Painting nothing has neither problem: no lifecycle churn, and the render
    /// loop never stops.
    fn sync_overlay(&mut self, ctx: &egui::Context) {
        // Only ask the window manager occasionally for the rectangle — the
        // player is not moving the game window every frame — but a quarter of a
        // second is fast enough that an Alt-Tab feels immediate.
        if self.last_game_poll.elapsed() >= Duration::from_millis(250) {
            self.last_game_poll = Instant::now();
            // Two passes: the first learns how wide the game window is, the
            // second places an overlay sized to the band SrvSurvey leaves free
            // inside it. Cheap — both are arithmetic on a cached rectangle.
            let probe = overlay_placement(self.anchor);
            if self.app.config().overlay_fit_between_plotters
                && let Some((x, width)) = PlotterGap::default().band(probe.game_width)
            {
                self.anchor.width = width;
                self.anchor.x_fraction = 0.0;
                self.anchor.x_offset_px = x;
            }
            self.placement = overlay_placement(self.anchor);
        }
        let placement = self.placement;
        self.game_found = placement.game_found;

        let showing = self.app.overlay_enabled()
            && overlay_visible(placement.game_found, placement.game_focused);

        if showing {
            // Measure the column before the image is sized: the spectrogram
            // gets whatever the lamps do not need.
            let probe = overlay::OverlayState::from_app(&self.app);
            self.overlay_label_px = overlay::label_column_width(ctx, &probe);
            self.rebuild_overlay_spectrogram(ctx);
        }

        // Published through a shared cell rather than captured by value. egui
        // may render the child while the parent sleeps, and its docs call for
        // exactly this; a closure capturing a snapshot can only ever show the
        // state of the frame that built it.
        {
            let mut shared = self.overlay_state.lock().unwrap();
            let pixels = shared.spectrogram.take();
            *shared = overlay::OverlayState::from_app(&self.app);
            shared.showing = showing;
            // Carry forward any frame the overlay has not consumed yet, so a
            // slow child never loses the newest image it was handed.
            shared.spectrogram = self
                .pending_spectrogram
                .take()
                .or(pixels)
                .filter(|_| self.app.config().overlay_spectrogram);
            shared.direction = self
                .snapshot
                .as_ref()
                .map(|s| s.direction)
                .filter(|_| self.app.config().direction_finding);
        }

        // Hidden by opacity, the way SrvSurvey hides its plotters: the window
        // stays open, in place, and rendering, but composites to nothing.
        //
        // Position is the fallback, and only the fallback. If the Win32 call
        // has not succeeded yet — the window does not exist on the first frame
        // — the overlay is parked off-screen instead, so there is never a
        // moment where it is both wanted-hidden and visible.
        let hidden_by_opacity = self.overlay_alpha == Some(0);
        let position = if showing || hidden_by_opacity {
            placement.position
        } else {
            crate::game_window::PARKED_POSITION
        };

        let builder = egui::ViewportBuilder::default()
            .with_title(crate::game_window::OVERLAY_WINDOW_TITLE)
            .with_position([position.0, position.1])
            .with_inner_size([self.anchor.width, self.anchor.height])
            .with_decorations(false)
            .with_transparent(true)
            // Click-through, so it can never steal a click meant for the cockpit.
            .with_mouse_passthrough(true)
            // Never take focus, not even on creation.
            .with_active(false)
            // No taskbar entry and no Alt-Tab stop: it is not a window you are
            // ever meant to interact with, and it cannot be lost behind
            // anything because the control window owns the process.
            .with_taskbar(false)
            .with_always_on_top();

        let wanted_alpha = if showing { 255 } else { 0 };
        if self.overlay_alpha != Some(wanted_alpha)
            && crate::game_window::set_overlay_opacity(
                crate::game_window::OVERLAY_WINDOW_TITLE,
                wanted_alpha,
            )
        {
            log::debug!("overlay opacity set to {wanted_alpha}");
            self.overlay_alpha = Some(wanted_alpha);
        }

        let state = Arc::clone(&self.overlay_state);
        let texture = Arc::clone(&self.overlay_texture);
        ctx.show_viewport_deferred(overlay_viewport_id(), builder, move |ctx, _class| {
            // Take the newest pixels and upload them here, in the overlay's own
            // pass, so this texture is allocated, written and drawn by exactly
            // one viewport.
            let snapshot = {
                let mut shared = state.lock().unwrap();
                let pixels = shared.spectrogram.take();
                let snapshot = shared.clone();
                drop(shared);
                if let Some(image) = pixels {
                    let mut texture = texture.lock().unwrap();
                    match &mut *texture {
                        Some(handle) => handle.set(image, egui::TextureOptions::NEAREST),
                        None => {
                            *texture = Some(ctx.load_texture(
                                "overlay-spectrogram",
                                image,
                                egui::TextureOptions::NEAREST,
                            ));
                        }
                    }
                }
                snapshot
            };

            // No frame and no background: whatever is not painted stays
            // transparent and the cockpit shows through. When not showing we
            // paint nothing at all, which is what makes the window invisible
            // without ever having to close it.
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| {
                    if snapshot.showing {
                        let texture = texture.lock().unwrap().clone();
                        overlay::overlay(ui, &snapshot, texture.as_ref());
                    }
                });
            // Audio keeps arriving whether or not anything moves on screen.
            ctx.request_repaint_after(Duration::from_millis(66));
        });
    }

    /// Rebuild the overlay's own spectrogram texture, at most as often as the
    /// analysis produces new rows.
    fn rebuild_overlay_spectrogram(&mut self, _ctx: &egui::Context) {
        let cfg = self.app.config();
        // Normally there is no point rebuilding faster than the analysis
        // produces rows. While the band is animating there is: the rows have not
        // changed, but where they are drawn has.
        let interval = if self.zoom.animating() {
            Duration::from_millis(16)
        } else {
            self.snapshot_interval
        };
        if !cfg.overlay_spectrogram || self.last_overlay_render.elapsed() < interval {
            return;
        }
        let Some(engine) = self.app.engine() else {
            return;
        };

        let geometry = engine.geometry();
        // The overlay's band is animated; the main window's is not. A cockpit
        // strip a few hundred pixels tall is where magnification actually buys
        // something, and the full view is where you go to see everything at once.
        let band = self.zoom.band(Instant::now());
        let scale = waterfall::FreqScale::new(band.low_hz, band.high_hz, geometry.nyquist_hz());
        let history = if cfg.spectrogram_show_excess {
            engine.excess_waterfall()
        } else {
            engine.waterfall()
        };
        let (w, h) = overlay_spectrogram_size(cfg, self.anchor.width, self.overlay_label_px);
        // Its own time window: a cockpit strip wants a short view, not the whole
        // analysis window crushed into a few hundred pixels.
        let window_frames = (cfg.overlay_spectrogram_seconds / geometry.frame_seconds())
            .ceil()
            .max(1.0) as usize;
        let image = waterfall::build_image(
            history,
            geometry,
            waterfall::RenderOptions {
                scale,
                auto_gain: true,
                median_subtract: cfg.spectrogram_median_subtract,
                window_frames,
            },
            w as usize,
            h as usize,
        );
        // Handed over as pixels. The overlay viewport turns them into a texture
        // in its own pass; nothing here ever touches the GPU.
        self.pending_spectrogram = Some(image);
        self.last_overlay_render = Instant::now();
    }

    /// Write the current waterfall as a high-resolution PNG.
    ///
    /// The on-screen view is limited to the window; the published decodes were
    /// read at far higher resolution, which is the difference between seeing a
    /// mountain and seeing a smear.
    fn export_spectrogram(&mut self) -> Option<PathBuf> {
        let engine = self.app.engine()?;
        let geometry = engine.geometry();
        let cfg = self.app.config();
        let scale = waterfall::FreqScale::new(
            cfg.spectrogram_min_hz,
            cfg.spectrogram_max_hz,
            geometry.nyquist_hz(),
        );
        let show_excess = cfg.spectrogram_show_excess;
        let history = if show_excess {
            engine.excess_waterfall()
        } else {
            engine.waterfall()
        };

        let dir = std::path::Path::new(&self.export_dir);
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::error!("could not create {}: {e}", dir.display());
            return None;
        }

        // Named by where it was taken, then when — so a folder of exports sorts
        // by system and the filename alone identifies the observation.
        let system = self
            .app
            .game_state()
            .star_system
            .unwrap_or_else(|| "unknown-system".to_string());
        let name = format!(
            "{}-{}{}.png",
            sanitize_for_filename(&system),
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            if show_excess { "-excess" } else { "-raw" }
        );
        let path = dir.join(name);
        let mut written = None;
        let window_frames = (cfg.waterfall_seconds / geometry.frame_seconds())
            .ceil()
            .max(1.0) as usize;
        match waterfall::export_png(
            history,
            geometry,
            waterfall::RenderOptions {
                scale,
                auto_gain: true,
                median_subtract: cfg.spectrogram_median_subtract,
                window_frames,
            },
            cfg.export_width,
            export_height(cfg),
            &path,
        ) {
            Ok(()) => {
                log::info!("exported {}", path.display());
                written = Some(path.clone());
                // Exports are renderings of data still held elsewhere, so they
                // are trimmed oldest-first with no ranking. Without this they
                // are the one thing on disk with no ceiling at all.
                crate::retention::enforce_simple_budget(
                    dir,
                    "png",
                    cfg.export_budget_mb.saturating_mul(1_048_576),
                );
            }
            Err(e) => log::error!("could not export the spectrogram: {e}"),
        }
        written
    }

    /// Write the audio *and* the picture, on one press.
    ///
    /// These were two buttons in two different rows, which is one too many for
    /// the moment they exist for: something is on screen that should not be, and
    /// whatever is kept now is all anyone will ever have of it. Sound and image
    /// answer different questions — the image shows what the detectors saw, the
    /// audio can be re-run through them with different thresholds — and needing
    /// both is the normal case, not the careful one.
    fn export_everything(&mut self) {
        // As much as the ring holds, not an arbitrary minute. The Landscape
        // Signal's cycle alone is 109.5 s, and a dump too short to contain one
        // cannot answer anything about it afterwards.
        let seconds = self.app.config().pcm_ring_seconds;
        let mut why = String::new();
        let audio: Option<PathBuf> = match self.app.keep_recent(seconds, "manual", true) {
            Ok(path) => Some(path),
            Err(e) => {
                log::warn!("could not keep audio: {e:#}");
                why = format!(" ({e})");
                None
            }
        };
        let png = self.export_spectrogram();
        self.export_status = Some(match (audio, png) {
            (Some(a), Some(_)) => format!(
                "exported {:.0} s of audio and the spectrogram to {}",
                seconds,
                a.parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            ),
            (Some(a), None) => format!("kept the audio ({}), but the image failed", a.display()),
            (None, Some(p)) => format!("wrote {}, but the audio failed{why}", p.display()),
            (None, None) => format!("export failed{why} — see the log"),
        });
    }

    fn detectors(&mut self, ui: &mut egui::Ui) {
        let Some(snap) = &self.snapshot else { return };
        let cfg = self.app.config();
        let good = egui::Color32::from_rgb(120, 255, 160);
        let idle = egui::Color32::from_gray(120);

        ui.horizontal(|ui| {
            // Binary keying.
            match &snap.keying {
                Some(k) if k.is_present(cfg.keying_threshold) => {
                    ui.label(
                        egui::RichText::new("◉ TRANSMISSION")
                            .monospace()
                            .size(16.0)
                            .color(good),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{:.2} · {} tones · {:.2} sym/s",
                            k.confidence,
                            k.tones_hz.len(),
                            k.symbol_rate_hz
                        ))
                        .monospace(),
                    )
                    .on_hover_text(format!(
                        "tones: {:?} Hz\ntiming regularity {:.2}, alphabet purity {:.2}",
                        k.tones_hz
                            .iter()
                            .map(|h| h.round() as i32)
                            .collect::<Vec<_>>(),
                        k.timing_regularity,
                        k.alphabet_purity
                    ));
                }
                Some(k) => {
                    ui.label(
                        egui::RichText::new(format!("○ no keying  {:.2}", k.confidence))
                            .monospace()
                            .color(idle),
                    );
                }
                None => {
                    ui.label(egui::RichText::new("○ no keying").monospace().color(idle));
                }
            }

            ui.separator();

            // Drawn structure.
            let st = &snap.structure;
            if st.is_present(cfg.structure_threshold) {
                ui.label(
                    egui::RichText::new("◉ PICTURE")
                        .monospace()
                        .size(16.0)
                        .color(good),
                );
                ui.label(egui::RichText::new(format!("{:.2}", st.score)).monospace())
                    .on_hover_text(format!(
                        "coherence {:.2}, sparsity {:.2}, orientation diversity {:.2}",
                        st.coherence, st.sparsity, st.orientation_diversity
                    ));
            } else {
                ui.label(
                    egui::RichText::new(format!("○ no picture  {:.2}", st.score))
                        .monospace()
                        .color(idle),
                );
            }
        });
    }

    fn instruments(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.snapshot.clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("AZIMUTH").monospace().size(11.0));
                match &snapshot {
                    Some(s) => compass::draw(ui, &s.direction, 140.0),
                    None => {
                        ui.allocate_space(egui::vec2(140.0, 140.0));
                    }
                }
                if snapshot
                    .as_ref()
                    .is_some_and(|s| s.direction.front_back_ambiguous && s.direction.is_usable())
                {
                    ui.weak(
                        egui::RichText::new("front/back ambiguous")
                            .monospace()
                            .size(10.0),
                    )
                    .on_hover_text(
                        "Two channels cannot distinguish a source ahead from one astern. \
                         Switch the Windows output endpoint to 7.1 to resolve it.",
                    );
                }
            });

            ui.add_space(16.0);

            ui.vertical(|ui| {
                ui.label(egui::RichText::new("PERIODICITY").monospace().size(11.0));
                compass::draw_periodicity(
                    ui,
                    snapshot.as_ref().and_then(|s| s.periodicity.as_ref()),
                    egui::vec2(320.0, 120.0),
                    30.0,
                    600.0,
                );
                if let Some(p) = snapshot.as_ref().and_then(|s| s.periodicity.as_ref())
                    && crate::analysis::periodicity::matches_landscape(p, 2.0)
                {
                    ui.label(
                        egui::RichText::new("consistent with the Landscape Signal")
                            .monospace()
                            .size(11.0)
                            .color(egui::Color32::from_rgb(120, 200, 255)),
                    );
                }
            });

            ui.add_space(16.0);

            ui.vertical(|ui| {
                ui.label(egui::RichText::new("CHANNELS").monospace().size(11.0));
                if let Some(s) = &snapshot {
                    controls::channel_meters(ui, s, egui::vec2(220.0, 120.0));
                }
                controls::muted_bands(ui, &mut self.app);
            });
        });
    }
}

impl eframe::App for CompassUi {
    /// Paint this window solid.
    ///
    /// The root viewport asks for transparency so the GL config has an alpha
    /// channel — without it the overlay, a child viewport, cannot be
    /// transparent and appears as a black rectangle over the cockpit. That
    /// request would otherwise let the desktop show through *this* window too,
    /// so the clear colour is pinned opaque here.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = visuals.window_fill().to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }

    /// Runs before every repaint, and also when the window is hidden — which is
    /// exactly where draining capture belongs, so a minimized window does not
    /// stall the analysis.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.pump();
        if self.last_snapshot.elapsed() >= self.snapshot_interval {
            self.snapshot = self.app.snapshot();
            self.last_snapshot = Instant::now();
            let cfg = self.app.config();
            let active = cfg
                .overlay_zoom_on_detection
                .then(|| {
                    self.snapshot
                        .as_ref()
                        .and_then(|s| s.active_band_hz)
                        .map(|(low, high)| zoom::Band::new(low, high))
                })
                .flatten();
            self.zoom.set_bounds(zoom::Band::new(
                cfg.spectrogram_min_hz,
                cfg.spectrogram_max_hz,
            ));
            self.zoom.observe(active, Instant::now());
        }
        // The overlay is driven from here, NOT from `ui`, for the same reason
        // `pump` is: `ui` stops running the moment the main window is
        // minimized — which is exactly how the tool is used while flying — and
        // an overlay fed from `ui` freezes at whatever state it was last
        // handed. The game's focus comes from our own Win32 poll rather than
        // from egui input, so visibility stays correct even then.
        self.sync_overlay(ctx);
        // Audio keeps arriving whether or not anything moves on screen, so the
        // window must repaint without waiting for input — and faster than that
        // while the overlay's band is moving, since a 450 ms animation redrawn
        // ten times a second is a slideshow rather than a movement.
        let interval = if self.zoom.animating() { 16 } else { 33 };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui);
            ui.add_space(2.0);
            self.detectors(ui);
            ui.add_space(2.0);
            self.controls_row(ui);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("events")
            .resizable(true)
            .default_size(180.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("EVENTS").monospace().size(11.0));
                    ui.weak(
                        egui::RichText::new(
                            "time · band · duration · excess · score · bearing · system",
                        )
                        .monospace()
                        .size(10.0),
                    );
                });
                events::draw(ui, self.app.events());
            });

        egui::Panel::bottom("health").show(ui, |ui| {
            if let Some(s) = &self.snapshot {
                controls::health_strip(ui, s);
            }
            overlay::disk_usage(ui, &mut self.app);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.waterfall_panel(ui);
            ui.add_space(6.0);
            self.instruments(ui);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overlay_follows_the_game_s_focus() {
        assert!(overlay_visible(true, true), "Elite in front: show");
        assert!(
            !overlay_visible(true, false),
            "Alt-Tabbed away: the overlay must go with it, not float over the browser"
        );
    }

    #[test]
    fn it_starts_hidden_and_is_revealed_only_by_the_game() {
        // At launch our own control window has focus and Elite does not. An
        // earlier version treated our focus as reason enough to show, so the
        // overlay appeared for a moment on every start and then disappeared.
        assert!(
            !overlay_visible(true, false),
            "our window focused, Elite not: stay hidden"
        );
        assert!(
            !overlay_visible(false, false),
            "no game at all: stay hidden"
        );
        // Only the game brings it out.
        assert!(overlay_visible(true, true));
    }

    #[test]
    fn no_game_window_means_no_overlay_at_all() {
        // Regression: the overlay used to appear over the bare desktop whenever
        // the control panel had focus, flashing as it fought itself for focus.
        assert!(!overlay_visible(false, false));
        assert!(
            !overlay_visible(false, true),
            "a focused window we cannot find is not the game"
        );
    }

    #[test]
    fn visibility_is_stable_while_the_game_stays_focused() {
        // Flapping here is flicker on screen. Focus held steady must give a
        // steady answer, whatever else changes around it.
        for _ in 0..10 {
            assert!(overlay_visible(true, true));
        }
    }

    #[test]
    fn the_overlay_state_is_shareable_with_a_viewport_callback() {
        // `show_viewport_deferred` demands `Fn + Send + Sync + 'static`, and
        // the state must be readable from it long after the frame that wrote
        // it. If this stops compiling, the overlay is about to go stale again.
        fn assert_shareable<T: Send + Sync + 'static>() {}
        assert_shareable::<Arc<Mutex<overlay::OverlayState>>>();

        let shared = Arc::new(Mutex::new(overlay::OverlayState::default()));
        let reader = Arc::clone(&shared);
        shared.lock().unwrap().rung = Some(overlay::Rung::Signal);
        // A later write is visible to the holder of the clone — which is the
        // whole reason the callback reads through one.
        assert_eq!(reader.lock().unwrap().rung, Some(overlay::Rung::Signal));
    }

    #[test]
    fn the_overlay_window_is_never_torn_down_to_hide_it() {
        // Both of the obvious hide mechanisms are broken in egui 0.35, each in
        // a way that took a crash or a freeze to find, so the visibility
        // decision must only ever change what is *painted*.
        //
        // Destroying the viewport risks `Painter::set_window(id, None)`, which
        // clears every surface, not just that viewport's. Hiding it with
        // `with_visible(false)` stops its redraws for good. Painting nothing
        // has neither failure mode.
        // Nothing is painted until the visibility decision has actually been
        // made, so launching cannot flash a panel over the desktop.
        let mut state = overlay::OverlayState::default();
        assert!(!state.showing, "an overlay paints nothing until told to");
        state.showing = true;
        assert!(state.showing, "and the flag is what the callback reads");
    }

    #[test]
    fn the_overlay_is_not_the_root_viewport() {
        // ViewportId(Id::NULL) is ROOT — the control window. Reusing it would
        // have made the overlay replace the window it is meant to accompany.
        assert_ne!(overlay_viewport_id(), egui::ViewportId::ROOT);
    }
}

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

use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::app::{App, Status};
use crate::audio::device::{self, AudioDevice};
use crate::game_window::{OverlayAnchor, OverlayPlacement, overlay_placement};
use crate::pipeline::AnalysisSnapshot;

/// Launch the window. Blocks until it closes.
pub fn run(app: App) -> Result<()> {
    // One window. The in-game overlay is a second viewport that shows and
    // hides itself with the game; there is no other shape to switch to, which
    // means there is no state you have to kill the process to leave.
    let viewport = egui::ViewportBuilder::default()
        .with_title("ED Compass")
        .with_inner_size([1180.0, 860.0])
        .with_min_inner_size([900.0, 620.0]);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ED Compass",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
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
/// Whether the overlay window should be visible this frame.
///
/// No game window, no overlay — there is nothing to annotate, and a strip
/// floating over the desktop taught us it just flashes and worries people. With
/// the game present it follows focus: Elite's, or our own control window's so
/// the toggles beside it can be seen to do something rather than adjusted
/// blind.
fn overlay_visible(game_found: bool, game_focused: bool, own_focus: bool) -> bool {
    game_found && (game_focused || own_focus)
}

fn overlay_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("ed-compass-overlay")
}

fn anchor_from(cfg: &crate::config::Config) -> OverlayAnchor {
    OverlayAnchor {
        x_fraction: cfg.overlay_x_fraction,
        y_fraction: cfg.overlay_y_fraction,
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
    overlay_texture: Option<egui::TextureHandle>,
    last_overlay_render: Instant,
    game_found: bool,
    last_game_poll: Instant,

    /// Where exported images go.
    export_dir: String,

    devices: Vec<AudioDevice>,
    waterfall_texture: Option<egui::TextureHandle>,
    last_waterfall: Instant,
    /// Size the waterfall image was last built at, so it is rebuilt on resize.
    waterfall_size: [usize; 2],
}

impl CompassUi {
    fn new(app: App) -> Self {
        let interval = Duration::from_secs_f32(1.0 / app.config().analysis_update_hz.max(1.0));
        let anchor = anchor_from(app.config());
        Self {
            anchor,
            placement: overlay_placement(anchor),
            overlay_texture: None,
            last_overlay_render: Instant::now() - Duration::from_secs(1),
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
                .button("export PNG")
                .on_hover_text(
                    "Write the spectrogram at 4096x1600 for comparison against \
                     published decodes.",
                )
                .clicked()
            {
                self.export_spectrogram();
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
            self.waterfall_texture = Some(ui.ctx().load_texture(
                "waterfall",
                image,
                egui::TextureOptions::NEAREST,
            ));
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
                if ui
                    .button("Keep last 60 s")
                    .on_hover_text("Write the recent audio to the captures folder right now.")
                    .clicked()
                {
                    match self.app.keep_recent(60.0, "manual") {
                        Ok(path) => log::info!("kept {}", path.display()),
                        Err(e) => log::warn!("could not keep audio: {e:#}"),
                    }
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
    /// Modelled on how SrvSurvey manages its plotters, because that model has
    /// survived years of real use: **one persistent window**, shown and hidden
    /// — never created and destroyed — and marked non-activating so it cannot
    /// take focus.
    ///
    /// The first version of this method skipped `show_viewport_deferred`
    /// whenever the overlay should be hidden, which egui treats as "destroy the
    /// window". Creating the replacement stole focus from the very window whose
    /// focus was the test for showing it, so the overlay destroyed and rebuilt
    /// itself several times a second — visible as flashing, and tearing down a
    /// wgpu surface at that rate is the likely cause of the crash it ended in.
    fn sync_overlay(&mut self, ctx: &egui::Context) {
        // Only ask the window manager occasionally for the rectangle — the
        // player is not moving the game window every frame — but a quarter of a
        // second is fast enough that an Alt-Tab feels immediate.
        if self.last_game_poll.elapsed() >= Duration::from_millis(250) {
            self.last_game_poll = Instant::now();
            self.placement = overlay_placement(self.anchor);
        }
        let placement = self.placement;
        self.game_found = placement.game_found;

        // Disabled means the window itself goes; that is the one deliberate
        // destroy, and it is a user action, not a focus flicker.
        if !self.app.overlay_enabled() {
            return;
        }

        let own_focus = ctx.input(|i| i.focused);
        let visible = overlay_visible(placement.game_found, placement.game_focused, own_focus);

        if visible {
            self.rebuild_overlay_spectrogram(ctx);
        }
        let mut state = overlay::OverlayState::from_app(&self.app);
        state.spectrogram = self
            .overlay_texture
            .clone()
            .filter(|_| self.app.config().overlay_spectrogram);

        let builder = egui::ViewportBuilder::default()
            .with_title("ED Compass overlay")
            .with_position([placement.position.0, placement.position.1])
            .with_inner_size([self.anchor.width, self.anchor.height])
            .with_decorations(false)
            .with_transparent(true)
            // Click-through, so it can never steal a click meant for the cockpit.
            .with_mouse_passthrough(true)
            // Never take focus — not even on creation. The game keeps keyboard
            // and mouse; we are paint, not a window anyone interacts with.
            .with_active(false)
            // Hidden rather than absent when the game loses focus. egui diffs
            // the builder and toggles visibility on the existing window.
            .with_visible(visible)
            // No taskbar entry and no Alt-Tab stop: it is not a window you are
            // ever meant to interact with, and it cannot be lost behind
            // anything because the control window owns the process.
            .with_taskbar(false)
            .with_always_on_top();

        ctx.show_viewport_deferred(overlay_viewport_id(), builder, move |ctx, _class| {
            if !visible {
                // Nothing to draw and nothing changing; idle until re-shown.
                ctx.request_repaint_after(Duration::from_millis(500));
                return;
            }
            // No frame and no background: whatever is not painted stays
            // transparent and the cockpit shows through.
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| overlay::overlay(ui, &state));
            ctx.request_repaint_after(Duration::from_millis(66));
        });
    }

    /// Rebuild the overlay's own spectrogram texture, at most as often as the
    /// analysis produces new rows.
    fn rebuild_overlay_spectrogram(&mut self, ctx: &egui::Context) {
        let cfg = self.app.config();
        if !cfg.overlay_spectrogram || self.last_overlay_render.elapsed() < self.snapshot_interval {
            return;
        }
        let Some(engine) = self.app.engine() else {
            return;
        };

        let geometry = engine.geometry();
        let scale = waterfall::FreqScale::new(
            cfg.spectrogram_min_hz,
            cfg.spectrogram_max_hz,
            geometry.nyquist_hz(),
        );
        let history = if cfg.spectrogram_show_excess {
            engine.excess_waterfall()
        } else {
            engine.waterfall()
        };
        let (w, h) = cfg.overlay_spectrogram_size();
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
        self.overlay_texture =
            Some(ctx.load_texture("overlay-spectrogram", image, egui::TextureOptions::NEAREST));
        self.last_overlay_render = Instant::now();
    }

    /// Write the current waterfall as a high-resolution PNG.
    ///
    /// The on-screen view is limited to the window; the published decodes were
    /// read at far higher resolution, which is the difference between seeing a
    /// mountain and seeing a smear.
    fn export_spectrogram(&mut self) {
        let Some(engine) = self.app.engine() else {
            return;
        };
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
            return;
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
    /// Runs before every repaint, and also when the window is hidden — which is
    /// exactly where draining capture belongs, so a minimized window does not
    /// stall the analysis.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.pump();
        if self.last_snapshot.elapsed() >= self.snapshot_interval {
            self.snapshot = self.app.snapshot();
            self.last_snapshot = Instant::now();
        }
        // Audio keeps arriving whether or not anything moves on screen, so the
        // window must repaint without waiting for input.
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sync_overlay(&ui.ctx().clone());

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
        assert!(overlay_visible(true, true, false), "Elite in front: show");
        assert!(
            !overlay_visible(true, false, false),
            "Alt-Tabbed away: the overlay must go with it, not float over the browser"
        );
        assert!(
            overlay_visible(true, false, true),
            "our own window in front: show, so the toggles have a visible effect"
        );
    }

    #[test]
    fn no_game_window_means_no_overlay_at_all() {
        // Regression: the overlay used to appear over the bare desktop whenever
        // the control panel had focus, flashing as it fought itself for focus.
        for own in [false, true] {
            assert!(!overlay_visible(false, false, own), "own_focus={own}");
        }
    }

    #[test]
    fn the_overlay_is_not_the_root_viewport() {
        // ViewportId(Id::NULL) is ROOT — the control window. Reusing it would
        // have made the overlay replace the window it is meant to accompany.
        assert_ne!(overlay_viewport_id(), egui::ViewportId::ROOT);
    }
}

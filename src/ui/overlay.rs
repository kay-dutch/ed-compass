//! The in-game overlay, and the small widgets shared with the main window.
//!
//! There used to be a third window shape — a "compact" control panel — whose
//! job was to be small enough to keep near the game. The overlay made it
//! redundant: it appears by itself whenever Elite has focus, the way
//! SrvSurvey's panels do, and the main window holds the controls. One window
//! plus the overlay is the whole model.
//!
//! The overlay is drawn from a plain [`OverlayState`] rather than from [`App`],
//! because it renders in a viewport callback that cannot borrow the
//! application.

use eframe::egui;

use crate::analysis::direction::DirectionEstimate;
use crate::app::App;

/// Elite's own HUD palette, sampled from a cockpit screenshot rather than
/// guessed at, so the overlay reads as part of the game's interface instead of
/// as a foreign window sitting on top of it.
pub mod hud {
    use egui::Color32;

    /// The bright orange of active HUD text — system name, target panel.
    pub const ORANGE: Color32 = Color32::from_rgb(209, 110, 0);
    /// The dimmer amber Elite uses for secondary labels.
    pub const AMBER: Color32 = Color32::from_rgb(177, 87, 0);
    /// An unlit element: the near-black brown of a cold radar ring.
    pub const IDLE: Color32 = Color32::from_rgb(88, 44, 6);
    /// Warning red, as on the heat and hull gauges.
    pub const RED: Color32 = Color32::from_rgb(147, 0, 4);
    /// The pale cyan of a resolved contact, kept for reference; the lamps used
    /// it first and it read as part of the scenery. Not currently used.
    pub const CYAN: Color32 = Color32::from_rgb(203, 249, 251);
    /// Bright green for a lit lamp. Deliberately *not* an Elite colour: the
    /// cockpit is orange on black, so green is the one thing guaranteed to be
    /// nothing else on screen — and peak human photopic sensitivity sits at
    /// ~555 nm, green, which is what a peripheral-vision alarm wants.
    pub const GREEN: Color32 = Color32::from_rgb(80, 255, 120);
}

/// The headline number: the measured period — and the signal's name, once the
/// period identifies it as one we know.
pub fn period_detail(app: &App) -> String {
    match app.periodicity() {
        // The lamp only lights at confidence ≥ 0.80, so when a match is named
        // the number worth showing is the period, not the confidence.
        Some(p) if app.landscape_present() => format!("Landscape {:.1}s", p.period_seconds),
        Some(p) => format!("{:.1}s conf {:.2}", p.period_seconds, p.confidence),
        None => "collecting…".into(),
    }
}

/// Text shown under each indicator.
pub fn detail_lines(app: &App) -> (String, String) {
    let cfg = app.config();
    let Some(engine) = app.engine() else {
        return ("waiting".into(), "waiting".into());
    };

    let keying = match engine.keying() {
        // Always show the numbers. Replacing them with a warning hid the one
        // thing needed to judge whether the warning was justified.
        Some(k) if k.is_present(cfg.keying_threshold) => format!(
            "{:.0} Hz · {:.1}/s{}",
            k.tones_hz.first().copied().unwrap_or(0.0),
            k.symbol_rate_hz,
            if app.keying_suspect() {
                " · music?"
            } else {
                ""
            }
        ),
        Some(k) => format!("{:.2}", k.confidence),
        None => "—".into(),
    };
    let structure = format!("{:.2}", engine.structure().score);
    (keying, structure)
}

/// How much disk the recordings are costing, and a way to reclaim it.
///
/// Shown because the alternative is finding out from Windows. The record count
/// is deliberately separate from the audio size: records are never deleted, so
/// that number only goes up, and it is the one that represents the work.
pub fn disk_usage(ui: &mut egui::Ui, app: &mut App) {
    let usage = app.disk_usage(false);

    let bar = |ui: &mut egui::Ui, label: &str, used: u64, budget: u64| {
        let fraction = if budget == 0 {
            0.0
        } else {
            (used as f32 / budget as f32).clamp(0.0, 1.0)
        };
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("{label:<9}"))
                    .monospace()
                    .size(10.0),
            );
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(120.0)
                    .desired_height(8.0),
            );
            ui.label(
                egui::RichText::new(format!("{} / {}", mib(used), mib(budget)))
                    .monospace()
                    .size(10.0),
            );
        });
    };

    bar(ui, "captures", usage.capture_bytes, usage.capture_budget);
    bar(ui, "exports", usage.export_bytes, usage.export_budget);

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} observations kept", usage.records))
                .monospace()
                .size(10.0)
                .weak(),
        )
        .on_hover_text(
            "Every detection keeps its record — system, coordinates, scores, \
             period — forever. Only the audio is ever reclaimed, weakest first.",
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("clean up")
                .on_hover_text("Apply the budgets now instead of waiting for the next capture.")
                .clicked()
            {
                app.clean_up_disk();
            }
        });
    });
}

/// Bytes as whole mebibytes, which is the only precision worth showing here.
fn mib(bytes: u64) -> String {
    format!("{} MB", bytes / 1_048_576)
}

/// Width the indicator column needs for the text it is about to draw.
///
/// Measured, never configured. This was an `overlay_label_fraction` setting,
/// and that was wrong twice over: the right value is a property of the font and
/// the strings, not a preference, and correcting the default silently did
/// nothing for anyone whose config had already been written — leaving 70 px of
/// dead panel that only a photograph revealed.
///
/// Quantised coarsely: every change of this value resizes the spectrogram image
/// beside it, and a texture that resizes on every frame is churn the renderer
/// has to absorb. At 20 px steps it moves a handful of times in a session.
pub fn label_column_width(ctx: &egui::Context, state: &OverlayState) -> f32 {
    /// Text inset from the column's left edge; see `hud_lamp`.
    const TEXT_X: f32 = 18.0;
    /// Breathing room before the rose or spectrogram butts up against it.
    const RIGHT_MARGIN: f32 = 10.0;

    let width_of = |text: &str, size: f32| {
        ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                text.to_owned(),
                egui::FontId::monospace(size),
                egui::Color32::WHITE,
            )
            .rect
            .width()
        })
    };

    let mut needed: f32 = 0.0;
    for label in ["SIGNAL", "TRANSMIT", "STRUCTURE"] {
        needed = needed.max(width_of(label, 11.0));
    }
    for detail in [
        &state.period_detail,
        &state.keying_detail,
        &state.structure_detail,
    ] {
        needed = needed.max(width_of(detail, 9.0));
    }

    ((TEXT_X + needed + RIGHT_MARGIN) / 20.0).ceil() * 20.0
}

/// Why the overlay is asking to be looked at, when it is not a detection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverlayAttention {
    /// Not detecting yet: starting, or still learning the background.
    NotReady,
    /// Broken: the audio endpoint is gone and nothing is being heard at all.
    Broken,
}

impl OverlayAttention {
    /// The states worth colouring the border for. `None` means "running
    /// normally" — whether or not anything has been detected.
    pub fn of(status: crate::app::Status) -> Option<Self> {
        use crate::app::Status;
        match status {
            Status::Starting | Status::Warming => Some(Self::NotReady),
            Status::DeviceLost => Some(Self::Broken),
            Status::Capturing | Status::NoSignal | Status::Anomaly => None,
        }
    }
}

/// Everything the overlay draws, flattened out of [`App`].
///
/// The overlay renders from a viewport callback that must outlive this frame and
/// be `Send + Sync`, so it cannot hold a reference to the application. Copying
/// the handful of values it needs is both cheaper and simpler than the
/// alternatives.
#[derive(Clone, Default)]
pub struct OverlayState {
    pub landscape: bool,
    pub keying: bool,
    pub keying_suspect: bool,
    pub structure: bool,
    pub period_detail: String,
    pub keying_detail: String,
    pub structure_detail: String,
    /// Pixels for the spectrogram, when the parent has produced a new frame.
    ///
    /// Deliberately an image and not a `TextureHandle`: a texture allocated in
    /// the main window's pass and drawn in the overlay's is a texture whose
    /// lifetime spans two viewports, and wgpu killed the process for it
    /// ("Texture with 'egui_texid_Managed(3)' label is invalid"). The overlay
    /// uploads these pixels inside its own pass and owns the result.
    pub spectrogram: Option<egui::ColorImage>,
    /// True when analysis is not actually running — warming up, starting, or
    /// the device is gone. Dark lamps otherwise mean "nothing found", and there
    /// is no way to tell that from "not listening".
    pub attention: Option<OverlayAttention>,
    /// The current bearing, present only while direction finding is enabled.
    /// `None` also removes the rose entirely, giving its width back to the
    /// spectrogram.
    pub direction: Option<DirectionEstimate>,
}

impl OverlayState {
    /// Read the current state out of the application.
    pub fn from_app(app: &App) -> Self {
        let (keying, structure) = app.detections_present();
        let (keying_detail, structure_detail) = detail_lines(app);
        Self {
            landscape: app.landscape_present(),
            keying,
            keying_suspect: app.keying_suspect(),
            structure,
            period_detail: period_detail(app),
            keying_detail,
            structure_detail,
            spectrogram: None,
            attention: OverlayAttention::of(app.status()),
            direction: None,
        }
    }
}

/// The in-game overlay: indicators down the left, spectrogram filling the rest.
///
/// Laid out by hand with the painter rather than with egui's layout, because
/// the spectrogram has to occupy the window's full height exactly — a stacked
/// layout left most of the panel empty, which is what it was replaced for.
pub fn overlay(ui: &mut egui::Ui, state: &OverlayState, spectrogram: Option<&egui::TextureHandle>) {
    let anything = state.landscape || state.keying || state.structure;

    let rect = ui.max_rect();
    let painter = ui.painter().clone();

    // Elite's cockpit ground is black; a translucent black panel with an amber
    // edge is what the game's own frames look like. Dimmer when idle so it all
    // but disappears, brighter the moment it has something to report.
    painter.rect_filled(
        rect,
        2.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, if anything { 200 } else { 130 }),
    );
    // The border is the one element with width to spare, so it carries the
    // state that would otherwise need its own label.
    let edge = match state.attention {
        Some(OverlayAttention::Broken) => hud::RED,
        Some(OverlayAttention::NotReady) => hud::AMBER,
        None if anything => hud::ORANGE,
        None => hud::IDLE,
    };
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(if state.attention.is_some() { 2.0 } else { 1.0 }, edge),
        egui::StrokeKind::Inside,
    );

    let mut column = rect.shrink(4.0);
    let mut right_edge = rect.max.x;
    if let Some(texture) = spectrogram {
        let width = texture.size_vec2().x;
        let image =
            egui::Rect::from_min_max(egui::pos2(rect.right() - width, rect.top()), rect.max);
        painter.image(
            texture.id(),
            image,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        // A single dividing rule, in the HUD's own amber.
        painter.line_segment(
            [image.left_top(), image.left_bottom()],
            egui::Stroke::new(1.0, hud::IDLE),
        );
        column.max.x = image.left() - 4.0;
        right_edge = image.left();
    }

    if let Some(estimate) = &state.direction {
        // A square rose in the gap the spectrogram left for it.
        let side = rect.height() - 8.0;
        let rose = egui::Rect::from_min_size(
            egui::pos2(right_edge - side - 2.0, rect.top() + 4.0),
            egui::vec2(side, side),
        );
        hud_rose(&painter, rose, estimate);
        column.max.x = rose.left() - 4.0;
    }

    let row_h = column.height() / 3.0;
    let row = |i: f32| {
        egui::Rect::from_min_size(
            egui::pos2(column.left(), column.top() + i * row_h),
            egui::vec2(column.width(), row_h),
        )
    };

    hud_lamp(
        &painter,
        row(0.0),
        // "SIGNAL", not the name of any one signal: the Landscape is simply
        // the first periodic transmission anyone has found, and the lamp is for
        // whatever repeats. The detail line names the match when there is one.
        "SIGNAL",
        &state.period_detail,
        state.landscape,
        hud::GREEN,
    );
    let keying_colour = if state.keying_suspect {
        hud::AMBER
    } else {
        hud::GREEN
    };
    hud_lamp(
        &painter,
        row(1.0),
        "TRANSMIT",
        &state.keying_detail,
        state.keying,
        keying_colour,
    );
    hud_lamp(
        &painter,
        row(2.0),
        "STRUCTURE",
        &state.structure_detail,
        state.structure,
        hud::GREEN,
    );
}

/// One indicator row: dot, name, and its supporting number underneath.
fn hud_lamp(
    painter: &egui::Painter,
    row: egui::Rect,
    label: &str,
    detail: &str,
    lit: bool,
    lit_colour: egui::Color32,
) {
    let colour = if lit { lit_colour } else { hud::IDLE };
    let centre = egui::pos2(row.left() + 7.0, row.center().y);
    painter.circle_filled(centre, 3.5, colour);
    if lit {
        // A soft ring, so a lit lamp registers in peripheral vision while you
        // are flying rather than needing to be looked at.
        painter.circle_stroke(
            centre,
            6.5,
            egui::Stroke::new(1.0, colour.gamma_multiply(0.5)),
        );
    }

    let x = row.left() + 18.0;
    painter.text(
        egui::pos2(x, row.center().y - 1.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        egui::FontId::monospace(11.0),
        if lit { lit_colour } else { hud::AMBER },
    );
    if !detail.is_empty() {
        painter.text(
            egui::pos2(x, row.center().y + 1.0),
            egui::Align2::LEFT_TOP,
            detail,
            egui::FontId::monospace(9.0),
            if lit { hud::ORANGE } else { hud::IDLE },
        );
    }
}

/// Degrees around the nose within which the rose draws no needle.
///
/// Balanced ambience — which is most of what a cockpit plays — pans dead
/// centre, so a centred bearing is almost always noise. Drawing it anyway kept
/// the needle permanently green, which teaches the eye to ignore the one
/// instrument that should light rarely. The cost is that a source genuinely
/// dead ahead reads as nothing until the ship yaws a few degrees.
pub const ROSE_DEADBAND_DEG: f32 = 3.0;

/// The bearing the rose should show, if any.
pub fn rose_bearing(estimate: &DirectionEstimate) -> Option<f32> {
    (estimate.is_usable() && estimate.azimuth_deg.abs() >= ROSE_DEADBAND_DEG)
        .then_some(estimate.azimuth_deg)
}

/// A miniature bearing rose: the full view's compass, reduced to what reads at
/// cockpit-glance size.
///
/// Same conventions as [`super::compass::draw`]: up is the ship's nose, the
/// needle's length carries the confidence, and a front/back-ambiguous bearing
/// (all a stereo mix can give) shows a dimmer mirrored ghost.
fn hud_rose(painter: &egui::Painter, rect: egui::Rect, estimate: &DirectionEstimate) {
    use super::compass::azimuth_to_vec;

    let centre = rect.center() - egui::vec2(0.0, 5.0);
    let radius = rect.width() / 2.0 - 8.0;

    painter.circle_stroke(centre, radius, egui::Stroke::new(1.0, hud::IDLE));
    // Cardinal ticks, the fore tick doubled so "up is forward" needs no label.
    for spoke in [0.0f32, 90.0, 180.0, -90.0] {
        let v = azimuth_to_vec(spoke);
        let (inner, colour) = if spoke == 0.0 {
            (0.75, hud::AMBER)
        } else {
            (0.85, hud::IDLE)
        };
        painter.line_segment(
            [centre + v * radius * inner, centre + v * radius],
            egui::Stroke::new(1.0, colour),
        );
    }

    if let Some(azimuth_deg) = rose_bearing(estimate) {
        let confidence = estimate.confidence.clamp(0.0, 1.0);
        let needle = azimuth_to_vec(azimuth_deg) * radius * (0.25 + 0.75 * confidence);
        painter.line_segment(
            [centre, centre + needle],
            egui::Stroke::new(2.0, hud::GREEN.gamma_multiply(0.4 + 0.6 * confidence)),
        );
        if estimate.front_back_ambiguous {
            let mirror = azimuth_to_vec(180.0 - azimuth_deg) * radius * 0.5;
            painter.line_segment(
                [centre, centre + mirror],
                egui::Stroke::new(1.0, hud::GREEN.gamma_multiply(0.25)),
            );
        }
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{:+.0}\u{00b0}", azimuth_deg),
            egui::FontId::monospace(9.0),
            hud::ORANGE,
        );
    } else {
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 1.0),
            egui::Align2::CENTER_BOTTOM,
            "\u{2014}",
            egui::FontId::monospace(9.0),
            hud::IDLE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_centred_bearing_draws_no_needle() {
        use crate::analysis::direction::{DirectionEstimate, DirectionMethod};
        let mut e = DirectionEstimate {
            azimuth_deg: 0.0,
            confidence: 0.9,
            method: DirectionMethod::StereoPanLaw,
            front_back_ambiguous: true,
        };
        // Balanced ambience pans centre; a permanently green needle is noise.
        assert_eq!(rose_bearing(&e), None);
        e.azimuth_deg = 2.9;
        assert_eq!(rose_bearing(&e), None, "inside the dead-band");
        e.azimuth_deg = -2.9;
        assert_eq!(rose_bearing(&e), None, "the dead-band is symmetric");

        e.azimuth_deg = 3.0;
        assert_eq!(rose_bearing(&e), Some(3.0), "at the edge it shows");
        e.azimuth_deg = -38.0;
        assert_eq!(rose_bearing(&e), Some(-38.0));

        e.method = DirectionMethod::Insufficient;
        assert_eq!(rose_bearing(&e), None, "unusable estimates never show");
    }

    /// The column must fit the text it draws, and not much more.
    ///
    /// The width is measured from the real fonts at runtime, so this checks the
    /// measuring function itself: too small clips the labels, too large is the
    /// dead space a photograph of the cockpit caught us shipping.
    #[test]
    fn the_label_column_fits_the_text_it_draws() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});

        let mut state = OverlayState {
            period_detail: "109.7s conf 0.98".into(),
            keying_detail: "22050 Hz · 123.4/s".into(),
            structure_detail: "0.34".into(),
            ..Default::default()
        };
        let wide = label_column_width(&ctx, &state);

        let text = ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                "22050 Hz · 123.4/s".to_owned(),
                egui::FontId::monospace(9.0),
                egui::Color32::WHITE,
            )
            .rect
            .width()
        });
        assert!(
            wide >= 18.0 + text,
            "{wide} px cannot hold {text} px of text"
        );
        assert!(wide <= 18.0 + text + 30.0, "{wide} px is dead space");

        // Short details give a narrower column — that width is the whole point.
        state.period_detail = "0.11".into();
        state.keying_detail = "0.52".into();
        let narrow = label_column_width(&ctx, &state);
        assert!(narrow < wide, "narrow {narrow} should beat wide {wide}");

        // And it never collapses below the fixed labels.
        state.structure_detail = String::new();
        let bare = label_column_width(&ctx, &state);
        assert!(bare >= 18.0 + 55.0, "must still fit STRUCTURE, got {bare}");
    }

    #[test]
    fn overlay_state_carries_pixels_not_a_gpu_texture() {
        // The process died with "Texture ... is invalid" because a texture
        // allocated in the main window's pass was drawn in the overlay's. The
        // state that crosses between them must stay plain CPU pixels; the
        // overlay uploads them inside its own pass.
        fn assert_sendable<T: Send + Sync + 'static>() {}
        assert_sendable::<OverlayState>();

        let mut state = OverlayState::default();
        state.spectrogram = Some(egui::ColorImage::filled([4, 2], egui::Color32::RED));
        let carried = state.spectrogram.expect("pixels");
        assert_eq!(carried.size, [4, 2]);
    }

    #[test]
    fn the_overlay_palette_is_elite_s_own() {
        // Sampled from a cockpit screenshot: orange HUD text on black, with
        // cyan reserved for contacts. Guessed colours read as a foreign window.
        assert_eq!(hud::ORANGE, egui::Color32::from_rgb(209, 110, 0));
        assert!(hud::ORANGE.r() > hud::ORANGE.g() && hud::ORANGE.b() == 0);
        assert!(
            hud::AMBER.r() < hud::ORANGE.r(),
            "amber is the dimmer label colour"
        );

        let sum = |c: egui::Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(
            sum(hud::GREEN) > sum(hud::IDLE) * 3,
            "a lit lamp must carry"
        );
        assert!(
            hud::GREEN.g() > hud::GREEN.r() && hud::GREEN.g() > hud::GREEN.b(),
            "the lit colour must actually be green, the eye's peak sensitivity"
        );
    }
}

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
    /// The pale cyan of a resolved contact. Elite reserves it for "something is
    /// there", which is exactly what a lit detector means.
    pub const CYAN: Color32 = Color32::from_rgb(203, 249, 251);
}

/// The headline number: the measured period and its confidence.
pub fn period_detail(app: &App) -> String {
    match app.periodicity() {
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
    pub spectrogram: Option<egui::TextureHandle>,
    /// Share of the width given to the indicator column.
    pub label_fraction: f32,
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
            label_fraction: app.config().overlay_label_fraction,
        }
    }
}

/// The in-game overlay: indicators down the left, spectrogram filling the rest.
///
/// Laid out by hand with the painter rather than with egui's layout, because
/// the spectrogram has to occupy the window's full height exactly — a stacked
/// layout left most of the panel empty, which is what it was replaced for.
pub fn overlay(ui: &mut egui::Ui, state: &OverlayState) {
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
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, if anything { hud::ORANGE } else { hud::IDLE }),
        egui::StrokeKind::Inside,
    );

    let mut column = rect.shrink(4.0);
    if let Some(texture) = &state.spectrogram {
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
        "LANDSCAPE",
        &state.period_detail,
        state.landscape,
        hud::CYAN,
    );
    let keying_colour = if state.keying_suspect {
        hud::AMBER
    } else {
        hud::CYAN
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
        hud::CYAN,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(sum(hud::CYAN) > sum(hud::IDLE) * 3, "a lit lamp must carry");
        assert!(
            hud::CYAN.b() > hud::CYAN.r(),
            "contacts read cool against the orange"
        );
    }
}

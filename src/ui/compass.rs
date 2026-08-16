//! The compass rose and the periodicity panel.
//!
//! Both are deliberately explicit about their own limits. A stereo endpoint
//! cannot tell front from rear, so the rose says so rather than drawing a
//! confident needle into a hemisphere it cannot see; and a periodicity peak is
//! drawn against the 109.5-second marker so a near-miss is visible as a
//! near-miss rather than being rounded into a match.

use eframe::egui;

use crate::analysis::direction::{DirectionEstimate, DirectionMethod};
use crate::analysis::periodicity::{LANDSCAPE_PERIOD_SECONDS, PeriodicityResult};

/// Convert a ship-relative azimuth to a screen direction.
///
/// Screen y grows downward, so forward (0°) is up: `(sin θ, −cos θ)`.
pub fn azimuth_to_vec(azimuth_deg: f32) -> egui::Vec2 {
    let rad = azimuth_deg.to_radians();
    egui::vec2(rad.sin(), -rad.cos())
}

/// Draw the compass rose. `size` is the widget's side length.
pub fn draw(ui: &mut egui::Ui, estimate: &DirectionEstimate, size: f32) {
    let (response, painter) = ui.allocate_painter(egui::vec2(size, size), egui::Sense::hover());
    let rect = response.rect;
    let centre = rect.center();
    let radius = size * 0.40;

    let ring = egui::Color32::from_gray(90);
    let faint = egui::Color32::from_gray(60);
    let text_colour = egui::Color32::from_gray(190);
    let font = egui::FontId::monospace(10.0);

    painter.circle_stroke(centre, radius, egui::Stroke::new(1.0, ring));
    // Rear half dashed when the layout cannot resolve it.
    for spoke in (0..360).step_by(30) {
        let v = azimuth_to_vec(spoke as f32);
        let behind = !(-90..=90).contains(&(((spoke + 180) % 360) - 180));
        let colour = if behind && estimate.front_back_ambiguous {
            faint
        } else {
            ring
        };
        painter.line_segment(
            [centre + v * (radius * 0.9), centre + v * radius],
            egui::Stroke::new(1.0, colour),
        );
    }

    for (label, azimuth) in [("0", 0.0f32), ("+90", 90.0), ("180", 180.0), ("-90", -90.0)] {
        let p = centre + azimuth_to_vec(azimuth) * (radius + 12.0);
        painter.text(
            p,
            egui::Align2::CENTER_CENTER,
            label,
            font.clone(),
            text_colour,
        );
    }

    if estimate.method == DirectionMethod::Insufficient {
        painter.text(
            centre,
            egui::Align2::CENTER_CENTER,
            "no bearing",
            font,
            egui::Color32::from_gray(120),
        );
        return;
    }

    // The needle's length carries the confidence, so a weak estimate is
    // visibly weak rather than looking as certain as a strong one.
    let confidence = estimate.confidence.clamp(0.0, 1.0);
    let needle = azimuth_to_vec(estimate.azimuth_deg) * radius * (0.25 + 0.75 * confidence);
    let colour = confidence_colour(confidence);
    painter.line_segment([centre, centre + needle], egui::Stroke::new(3.0, colour));
    painter.circle_filled(centre, 3.0, colour);

    // The mirror bearing, when front and rear are indistinguishable.
    if estimate.front_back_ambiguous {
        let mirror = azimuth_to_vec(180.0 - estimate.azimuth_deg) * radius * 0.5;
        painter.line_segment(
            [centre, centre + mirror],
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60),
            ),
        );
    }

    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 2.0),
        egui::Align2::CENTER_BOTTOM,
        format!("{:+.0}°  conf {confidence:.2}", estimate.azimuth_deg),
        egui::FontId::monospace(12.0),
        text_colour,
    );
}

pub fn confidence_colour(confidence: f32) -> egui::Color32 {
    if confidence >= 0.7 {
        egui::Color32::from_rgb(120, 255, 160)
    } else if confidence >= 0.4 {
        egui::Color32::from_rgb(255, 210, 90)
    } else {
        egui::Color32::from_rgb(220, 120, 120)
    }
}

/// Draw the periodicity panel: the autocorrelation peak against the 109.5 s
/// marker.
pub fn draw_periodicity(
    ui: &mut egui::Ui,
    result: Option<&PeriodicityResult>,
    size: egui::Vec2,
    min_seconds: f32,
    max_seconds: f32,
) {
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    let font = egui::FontId::monospace(10.0);
    let text_colour = egui::Color32::from_gray(190);

    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(18));

    let x_of = |seconds: f32| {
        let t = ((seconds - min_seconds) / (max_seconds - min_seconds)).clamp(0.0, 1.0);
        rect.left() + t * rect.width()
    };

    // The Landscape Signal's period, always drawn — it is the reference the
    // whole panel exists to test against.
    let marker_x = x_of(LANDSCAPE_PERIOD_SECONDS);
    painter.line_segment(
        [
            egui::pos2(marker_x, rect.top()),
            egui::pos2(marker_x, rect.bottom() - 12.0),
        ],
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(120, 200, 255, 120),
        ),
    );
    painter.text(
        egui::pos2(marker_x + 3.0, rect.top() + 2.0),
        egui::Align2::LEFT_TOP,
        "109.5s",
        font.clone(),
        egui::Color32::from_rgb(120, 200, 255),
    );

    painter.text(
        egui::pos2(rect.left() + 2.0, rect.bottom() - 2.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{min_seconds:.0}s"),
        font.clone(),
        text_colour,
    );
    painter.text(
        egui::pos2(rect.right() - 2.0, rect.bottom() - 2.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{max_seconds:.0}s"),
        font.clone(),
        text_colour,
    );

    let Some(p) = result else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "collecting…",
            font,
            egui::Color32::from_gray(120),
        );
        return;
    };

    let x = x_of(p.period_seconds);
    let height = (rect.height() - 16.0) * p.confidence.clamp(0.0, 1.0);
    let colour = confidence_colour(p.confidence);
    painter.line_segment(
        [
            egui::pos2(x, rect.bottom() - 12.0),
            egui::pos2(x, rect.bottom() - 12.0 - height),
        ],
        egui::Stroke::new(3.0, colour),
    );
    painter.text(
        egui::pos2(rect.center().x, rect.top() + 2.0),
        egui::Align2::CENTER_TOP,
        format!(
            "{:.1} s  conf {:.2}  prom {:.2}",
            p.period_seconds, p.confidence, p.prominence
        ),
        egui::FontId::monospace(11.0),
        text_colour,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "{a} != {b}");
    }

    #[test]
    fn forward_points_up_on_screen() {
        let v = azimuth_to_vec(0.0);
        near(v.x, 0.0);
        near(v.y, -1.0);
    }

    #[test]
    fn starboard_points_right_and_port_points_left() {
        let right = azimuth_to_vec(90.0);
        near(right.x, 1.0);
        near(right.y, 0.0);

        let left = azimuth_to_vec(-90.0);
        near(left.x, -1.0);
        near(left.y, 0.0);
    }

    #[test]
    fn astern_points_down() {
        let v = azimuth_to_vec(180.0);
        near(v.x.abs(), 0.0);
        near(v.y, 1.0);
    }

    #[test]
    fn the_direction_vector_is_always_unit_length() {
        for azimuth in [-180.0f32, -135.0, -45.0, 0.0, 37.0, 90.0, 179.0] {
            let v = azimuth_to_vec(azimuth);
            near(v.length(), 1.0);
        }
    }

    #[test]
    fn confidence_colour_grades_from_red_to_green() {
        let low = confidence_colour(0.1);
        let mid = confidence_colour(0.5);
        let high = confidence_colour(0.9);
        assert!(low.r() > low.g(), "low confidence should read as a warning");
        assert!(high.g() > high.r(), "high confidence should read as good");
        assert_ne!(mid, low);
        assert_ne!(mid, high);
    }
}

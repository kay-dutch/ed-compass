//! The detection list.
//!
//! One row per detection, newest first, carrying everything needed to decide
//! whether it is worth investigating: when, where in the galaxy, what band,
//! how far above background, which bearing, and whether the audio was kept.

use eframe::egui;

use crate::analysis::direction::DirectionMethod;
use crate::app::EventRecord;
use crate::capture_writer::TriggerDecision;

/// Short reason a detection was not written to disk.
pub fn decision_label(decision: TriggerDecision) -> &'static str {
    match decision {
        TriggerDecision::Accept => "",
        TriggerDecision::BelowThreshold => "below threshold",
        TriggerDecision::CoolingDown => "cooling down",
        TriggerDecision::HourlyLimit => "hourly limit",
    }
}

/// Compact frequency range, e.g. `1.2–3.4 kHz`.
pub fn format_band(low_hz: f32, high_hz: f32) -> String {
    if high_hz >= 1000.0 {
        format!("{:.1}–{:.1} kHz", low_hz / 1000.0, high_hz / 1000.0)
    } else {
        format!("{low_hz:.0}–{high_hz:.0} Hz")
    }
}

/// Bearing as shown in the list, or a reason there is none.
pub fn format_bearing(
    azimuth_deg: f32,
    confidence: f32,
    method: DirectionMethod,
    ambiguous: bool,
) -> String {
    match method {
        DirectionMethod::Insufficient => "—".into(),
        _ => format!(
            "{azimuth_deg:+.0}°{} {confidence:.2}",
            if ambiguous { "?" } else { " " }
        ),
    }
}

/// Trim a timestamp to the wall-clock part, which is all a list row needs.
pub fn short_time(timestamp: &str) -> String {
    timestamp
        .split('T')
        .nth(1)
        .map(|t| t.chars().take(8).collect())
        .unwrap_or_else(|| timestamp.to_owned())
}

pub fn draw(ui: &mut egui::Ui, events: &[EventRecord]) {
    if events.is_empty() {
        ui.weak("No detections yet.");
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for record in events.iter().rev() {
                row(ui, record);
            }
        });
}

fn row(ui: &mut egui::Ui, record: &EventRecord) {
    let e = &record.detection.event;
    let d = &record.detection.direction;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(short_time(&record.timestamp)).monospace());

        ui.label(
            egui::RichText::new(format!("{:>14}", format_band(e.low_hz, e.high_hz))).monospace(),
        );
        ui.label(egui::RichText::new(format!("{:>6.1}s", e.duration_seconds)).monospace());
        ui.label(egui::RichText::new(format!("{:>6.1}dB", e.peak_excess_db)).monospace());

        // The score is the only number in this row that says how interesting the
        // event is, so it is the only one given emphasis. The bearing used to
        // carry it, coloured by *its own* confidence — which stereo pan law
        // reports as 1.00 whenever a source is centred, meaning the brightest
        // thing in every row was a constant.
        let score_colour = if e.score >= 0.6 {
            egui::Color32::from_rgb(255, 210, 90)
        } else {
            egui::Color32::from_gray(120)
        };
        ui.label(
            egui::RichText::new(format!("{:.2}", e.score))
                .monospace()
                .strong()
                .color(score_colour),
        );

        ui.label(
            egui::RichText::new(format!(
                "{:>10}",
                format_bearing(
                    d.azimuth_deg,
                    d.confidence,
                    d.method,
                    d.front_back_ambiguous
                )
            ))
            .monospace()
            .color(egui::Color32::from_gray(130)),
        );

        ui.label(
            egui::RichText::new(record.star_system.as_deref().unwrap_or("unknown system"))
                .monospace()
                .color(egui::Color32::from_gray(160)),
        );

        if record.detection.spans_gap {
            ui.label(
                egui::RichText::new("GAP")
                    .monospace()
                    .color(egui::Color32::from_rgb(255, 150, 90)),
            )
            .on_hover_text(
                "A timeline gap fell inside this event, so its structure and any period \
                 derived from it are unreliable.",
            );
        }

        match &record.captured_to {
            Some(path) => {
                ui.label(
                    egui::RichText::new("★ CAPTURED")
                        .monospace()
                        .color(egui::Color32::from_rgb(120, 255, 160)),
                )
                .on_hover_text(path.display().to_string());
            }
            None => {
                let reason = decision_label(record.decision);
                if !reason.is_empty() {
                    ui.weak(egui::RichText::new(reason).monospace());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_switch_units_at_a_kilohertz() {
        assert_eq!(format_band(1200.0, 3400.0), "1.2–3.4 kHz");
        assert_eq!(format_band(80.0, 400.0), "80–400 Hz");
    }

    #[test]
    fn an_unusable_bearing_shows_a_dash_not_zero_degrees() {
        assert_eq!(
            format_bearing(0.0, 0.0, DirectionMethod::Insufficient, true),
            "—"
        );
    }

    #[test]
    fn an_ambiguous_bearing_is_marked() {
        let stereo = format_bearing(-38.0, 0.81, DirectionMethod::StereoPanLaw, true);
        let surround = format_bearing(-38.0, 0.81, DirectionMethod::SurroundVector, false);
        assert!(stereo.contains('?'), "{stereo}");
        assert!(!surround.contains('?'), "{surround}");
        assert!(stereo.contains("-38"));
    }

    #[test]
    fn timestamps_are_trimmed_to_the_clock() {
        assert_eq!(short_time("3311-08-13T14:22:07Z"), "14:22:07");
        assert_eq!(short_time("3311-08-13T14:22:07.123+00:00"), "14:22:07");
        // A timestamp in an unexpected shape is passed through, not mangled.
        assert_eq!(short_time("no-t-here"), "no-t-here");
    }

    #[test]
    fn an_accepted_capture_needs_no_explanation() {
        assert_eq!(decision_label(TriggerDecision::Accept), "");
        assert!(!decision_label(TriggerDecision::CoolingDown).is_empty());
        assert!(!decision_label(TriggerDecision::HourlyLimit).is_empty());
        assert!(!decision_label(TriggerDecision::BelowThreshold).is_empty());
    }
}

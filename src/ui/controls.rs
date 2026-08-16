//! Device selection, muted bands, and the signal-health strip.

use eframe::egui;

use crate::analysis::statistics::DB_FLOOR;
use crate::app::{App, Status};
use crate::audio::device::AudioDevice;
use crate::pipeline::AnalysisSnapshot;

pub fn status_colour(status: Status) -> egui::Color32 {
    match status {
        Status::Capturing => egui::Color32::from_rgb(120, 255, 160),
        Status::Anomaly => egui::Color32::from_rgb(255, 210, 90),
        Status::Warming | Status::Starting => egui::Color32::from_rgb(150, 190, 255),
        Status::NoSignal => egui::Color32::from_gray(140),
        Status::DeviceLost => egui::Color32::from_rgb(255, 110, 110),
    }
}

/// Device picker. Returns a device to switch to, if the user chose one.
pub fn device_picker(
    ui: &mut egui::Ui,
    devices: &[AudioDevice],
    current: &str,
) -> Option<AudioDevice> {
    let mut chosen = None;
    egui::ComboBox::from_id_salt("device-picker")
        .selected_text(current)
        .width(340.0)
        .show_ui(ui, |ui| {
            if devices.is_empty() {
                ui.label("no endpoints found");
            }
            for device in devices {
                let label = device.display_name();
                if ui.selectable_label(label == current, &label).clicked() && label != current {
                    chosen = Some(device.clone());
                }
            }
        });
    chosen
}

/// The muted-band list, with a control to clear it.
pub fn muted_bands(ui: &mut egui::Ui, app: &mut App) {
    let bands = app.config().ignore_bands.clone();
    if bands.is_empty() {
        ui.weak("no muted bands");
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for b in &bands {
            ui.label(
                egui::RichText::new(format!("{:.0}–{:.0} Hz", b.low_hz, b.high_hz))
                    .monospace()
                    .color(egui::Color32::from_gray(160)),
            );
        }
        if ui.button("clear").clicked() {
            app.clear_muted_bands();
        }
    });
}

/// Level readouts and the amplitude histogram — signal health, not detection.
pub fn health_strip(ui: &mut egui::Ui, snapshot: &AnalysisSnapshot) {
    ui.horizontal(|ui| {
        let s = &snapshot.stats;
        ui.label(
            egui::RichText::new(format!(
                "RMS {:>6.1} dBFS   Peak {:>6.1} dBFS   ZCR {:.3}",
                s.rms_dbfs, s.peak_dbfs, s.zero_crossing_rate
            ))
            .monospace(),
        );
        if s.clipped_samples > 0 {
            ui.label(
                egui::RichText::new(format!("CLIPPING ({})", s.clipped_samples))
                    .monospace()
                    .color(egui::Color32::from_rgb(255, 110, 110)),
            );
        }
        if s.dc_offset.abs() > 0.01 {
            ui.label(
                egui::RichText::new(format!("DC {:+.3}", s.dc_offset))
                    .monospace()
                    .color(egui::Color32::from_rgb(255, 210, 90)),
            );
        }
        ui.separator();
        histogram(ui, &snapshot.histogram, egui::vec2(220.0, 34.0));
    });
}

/// The amplitude histogram: distribution of instantaneous sample values across
/// `[-1, +1]`. Explicitly not a frequency plot.
pub fn histogram(ui: &mut egui::Ui, counts: &[u64], size: egui::Vec2) {
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(18));
    if counts.is_empty() {
        return;
    }

    let peak = counts.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "silent",
            egui::FontId::monospace(10.0),
            egui::Color32::from_gray(110),
        );
        return;
    }

    let bar = rect.width() / counts.len() as f32;
    for (i, &c) in counts.iter().enumerate() {
        // Log scaling: the centre bin dwarfs the tails on any real signal, and
        // a linear plot would show one spike and nothing else.
        let t = ((c as f32 + 1.0).ln() / (peak as f32 + 1.0).ln()).clamp(0.0, 1.0);
        let h = t * (rect.height() - 2.0);
        let x = rect.left() + i as f32 * bar;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, rect.bottom() - h),
                egui::pos2(x + bar.max(1.0), rect.bottom()),
            ),
            0.0,
            egui::Color32::from_rgb(90, 130, 190),
        );
    }
    // Zero-amplitude marker.
    let mid = rect.left() + rect.width() * 0.5;
    painter.line_segment(
        [egui::pos2(mid, rect.top()), egui::pos2(mid, rect.bottom())],
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 50),
        ),
    );
}

/// Per-channel level meters, labelled with each speaker's azimuth.
pub fn channel_meters(ui: &mut egui::Ui, snapshot: &AnalysisSnapshot, size: egui::Vec2) {
    let layout = snapshot.format.layout();
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(18));

    if layout.is_empty() {
        return;
    }
    let column = rect.width() / layout.len() as f32;
    // One shared level for now: the snapshot carries a mono summary, so the
    // meters show presence and layout rather than per-channel level.
    let level = ((snapshot.stats.rms_dbfs - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);

    for (i, info) in layout.iter().enumerate() {
        let x = rect.left() + i as f32 * column;
        let usable = info.azimuth_deg.is_some();
        let h = if usable {
            level * (rect.height() - 14.0)
        } else {
            0.0
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x + 1.0, rect.bottom() - 12.0 - h),
                egui::pos2(x + column - 1.0, rect.bottom() - 12.0),
            ),
            0.0,
            if usable {
                egui::Color32::from_rgb(90, 160, 190)
            } else {
                egui::Color32::from_gray(60)
            },
        );
        painter.text(
            egui::pos2(x + column * 0.5, rect.bottom() - 1.0),
            egui::Align2::CENTER_BOTTOM,
            info.name,
            egui::FontId::monospace(9.0),
            if usable {
                egui::Color32::from_gray(190)
            } else {
                egui::Color32::from_gray(110)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_a_distinct_enough_colour() {
        let capturing = status_colour(Status::Capturing);
        let lost = status_colour(Status::DeviceLost);
        let anomaly = status_colour(Status::Anomaly);
        assert!(
            capturing.g() > capturing.r(),
            "capturing should read as good"
        );
        assert!(lost.r() > lost.g(), "device lost should read as bad");
        assert_ne!(capturing, anomaly);
        assert_ne!(anomaly, lost);
    }

    #[test]
    fn no_signal_is_muted_not_alarming() {
        let c = status_colour(Status::NoSignal);
        // Grey: nothing is wrong, there is simply nothing to hear.
        assert_eq!(c.r(), c.g());
        assert_eq!(c.g(), c.b());
    }
}

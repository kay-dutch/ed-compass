//! The "new version available" strip across the top of the main window.
//!
//! Laid out after the ArtCraft launcher's: one line saying what is available,
//! the action on the right, and the same line turning into a progress bar while
//! the update runs. It never appears in the overlay — an update is something to
//! do between sessions, not to be told about over the cockpit.

use eframe::egui;

use super::overlay::hud;
use crate::update::{Phase, Updater};

/// Whether there is anything to show.
pub fn visible(updater: &Updater) -> bool {
    updater.offer().is_some()
}

pub fn draw(ui: &mut egui::Ui, updater: &mut Updater) {
    let Some(release) = updater.offer() else {
        return;
    };
    egui::Frame::new()
        .fill(hud::ORANGE.gamma_multiply(0.08))
        .stroke(egui::Stroke::new(1.0, hud::ORANGE.gamma_multiply(0.6)))
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                tag(ui);
                ui.add_space(6.0);
                match updater.error() {
                    Some(e) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Updating to {} failed: {e}",
                                release.version
                            ))
                            .color(egui::Color32::from_rgb(255, 110, 110)),
                        );
                    }
                    None => {
                        ui.label(
                            egui::RichText::new(format!(
                                "ED-Compass {} is available.",
                                release.version
                            ))
                            .strong(),
                        );
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(phase) = updater.phase() {
                        let (fraction, caption) = describe(&phase);
                        let mut bar = egui::ProgressBar::new(fraction.unwrap_or(0.0))
                            .desired_width(240.0)
                            .text(caption);
                        if fraction.is_none() {
                            bar = bar.animate(true);
                        }
                        ui.add(bar);
                        return;
                    }

                    if ui
                        .small_button("✕")
                        .on_hover_text("Not now. A newer version than this one is offered again.")
                        .clicked()
                    {
                        updater.dismiss();
                    }

                    if updater.can_install() {
                        let label = if updater.error().is_some() {
                            "Try again"
                        } else {
                            "Update and restart"
                        };
                        if ui
                            .button(egui::RichText::new(label).strong())
                            .on_hover_text(
                                "Downloads the new version, checks it against its published \
                                 checksum, installs it and opens ED-Compass again.",
                            )
                            .clicked()
                        {
                            updater.start_update();
                        }
                    } else if ui
                        .button(egui::RichText::new("Download").strong())
                        .on_hover_text(
                            "This copy was not installed by the installer — it is the portable \
                             zip, or a development build — so it is not replaced automatically. \
                             Opens the release page.",
                        )
                        .clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.page));
                    }

                    if ui.link("What's new").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&release.page));
                    }
                });
            });
        });
}

/// The small orange label at the start of the strip.
fn tag(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(hud::ORANGE)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("UPDATE")
                    .monospace()
                    .size(11.0)
                    .color(egui::Color32::BLACK),
            );
        });
}

/// Progress fraction (`None` when there is no meaningful one) and caption.
fn describe(phase: &Phase) -> (Option<f32>, String) {
    match *phase {
        Phase::Downloading { done, total } if total > 0 => (
            Some((done as f32 / total as f32).clamp(0.0, 1.0)),
            format!(
                "Downloading {:.1} of {:.1} MB",
                megabytes(done),
                megabytes(total)
            ),
        ),
        Phase::Downloading { .. } => (None, "Downloading…".into()),
        Phase::Verifying => (None, "Verifying…".into()),
        Phase::Installing => (None, "Installing, then restarting…".into()),
    }
}

fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_reads_in_megabytes_and_never_overshoots() {
        let (f, caption) = describe(&Phase::Downloading {
            done: 3_250_586,
            total: 7_340_032,
        });
        assert_eq!(caption, "Downloading 3.1 of 7.0 MB");
        assert!((f.unwrap() - 0.443).abs() < 0.01);

        // A server that sends more than it announced must not draw past the end.
        let (f, _) = describe(&Phase::Downloading { done: 10, total: 5 });
        assert_eq!(f, Some(1.0));

        // Without a length there is no honest fraction, only activity.
        assert_eq!(describe(&Phase::Downloading { done: 10, total: 0 }).0, None);
        assert_eq!(describe(&Phase::Verifying).0, None);
    }
}

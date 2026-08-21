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
    /// Blue for the two supporting detectors.
    ///
    /// TRANSMIT and STRUCTURE both light on ordinary ship ambience — measured,
    /// not assumed — so they are hints, not findings. Only SIGNAL has been
    /// checked against a known recording, and it keeps green to itself. Sharing
    /// one colour taught the eye that green means "maybe", which is the
    /// opposite of what an alarm is for.
    pub const BLUE: Color32 = Color32::from_rgb(77, 166, 255);
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

/// What the lowest rung has to say: the band something was last found in.
///
/// The rung exists precisely for things the named detectors cannot describe, so
/// the honest detail is *where* rather than *what*.
pub fn anomaly_detail(app: &App) -> String {
    match app.active_band_hz() {
        Some((low, high)) if high >= 1000.0 => {
            format!("{:.1}–{:.1} kHz", low / 1000.0, high / 1000.0)
        }
        Some((low, high)) => format!("{low:.0}–{high:.0} Hz"),
        None => "—".into(),
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
    for label in ["SIGNAL", "CYPHER", "ANOMALY"] {
        needed = needed.max(width_of(label, 11.0));
    }
    for detail in [
        &state.signal_detail,
        &state.cypher_detail,
        &state.anomaly_detail,
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
/// How far the evidence goes, lowest rung first.
///
/// The three lamps used to name three *categories* — a period match, keying, a
/// drawing — which meant the overlay went dark whenever the detectors found
/// something they could not put in one of those boxes. That is the worst
/// possible failure for a tool whose whole purpose is finding signals nobody has
/// catalogued: it stayed silent for exactly the case it exists for, while the
/// main window said ANOMALY.
///
/// So the lamps are a **ladder of confidence** instead, and each rung is
/// strictly stronger evidence than the one below:
///
/// | rung | what it means |
/// |---|---|
/// | `Anomaly` | something departed from the learned background |
/// | `Cypher` | it carries deliberate structure — keyed, or drawn |
/// | `Signal` | it matches something we can name |
///
/// They light cumulatively, so the display reads as a bar filling rather than as
/// three independent claims. `Signal` implies `Cypher` implies `Anomaly` by
/// construction, whether or not each underlying detector happens to be firing
/// this instant — a recognised signal is an encoded one, and an encoded one is a
/// departure from noise.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Rung {
    Anomaly,
    Cypher,
    Signal,
}

impl Rung {
    /// Highest rung the evidence reaches, if any.
    pub fn of(anomaly: bool, cypher: bool, signal: bool) -> Option<Self> {
        if signal {
            Some(Self::Signal)
        } else if cypher {
            Some(Self::Cypher)
        } else if anomaly {
            Some(Self::Anomaly)
        } else {
            None
        }
    }
}

/// Everything the overlay draws.
///
/// `Default` deliberately paints nothing: the shared cell is created before the
/// first game-window poll, and an overlay that paints itself before anyone has
/// decided it should be seen is an overlay that flashes over the desktop at
/// launch.
#[derive(Clone, Default)]
pub struct OverlayState {
    /// Highest rung the evidence reaches. `None` means nothing found.
    pub rung: Option<Rung>,
    /// How strong the evidence is at each rung, 0..1, weakest rung first:
    /// anomaly, cypher, signal.
    ///
    /// Kept alongside `rung` rather than replacing it because the two answer
    /// different questions — *how far* the evidence goes, and *how good* it is —
    /// and the panel shows both: position on the ladder, and brightness.
    pub strength: [f32; 3],
    /// Keying is firing while music plays, so the reading is suspect. Colours
    /// the CYPHER rung amber rather than clearing it — "this looks like music"
    /// is worth more than "something is encoded".
    pub keying_suspect: bool,
    pub anomaly_detail: String,
    pub cypher_detail: String,
    pub signal_detail: String,
    /// Pixels for the spectrogram, when the parent has produced a new frame.
    ///
    /// Deliberately an image and not a `TextureHandle`: a texture allocated in
    /// the main window's pass and drawn in the overlay's is a texture whose
    /// lifetime spans two viewports, and wgpu killed the process for it
    /// ("Texture with 'egui_texid_Managed(3)' label is invalid"). The overlay
    /// uploads these pixels inside its own pass and owns the result.
    pub spectrogram: Option<egui::ColorImage>,
    /// False when the overlay window is open but should show nothing. The
    /// window is never closed — see `CompassUi::sync_overlay` — so this is what
    /// makes it invisible.
    pub showing: bool,
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
        let signal = app.signal_present();
        let cypher = keying || structure;
        let anomaly = matches!(app.status(), crate::app::Status::Anomaly);
        // Whichever of the two is actually firing gets to describe the rung.
        let cypher_detail = if keying {
            keying_detail
        } else {
            structure_detail
        };

        // Evidence behind each rung, taken from the detectors' own numbers
        // rather than from whether they cleared a threshold. This is what lets a
        // faint reading show as a faint lamp instead of as silence.
        let keying_confidence = app
            .engine()
            .and_then(|e| e.keying())
            .map(|k| k.confidence)
            .unwrap_or(0.0);
        let structure_score = app.engine().map(|e| e.structure().score).unwrap_or(0.0);
        let morse_confidence = app.morse().map(|m| m.confidence).unwrap_or(0.0);
        let period_confidence = app
            .periodicity()
            .filter(|_| app.landscape_present())
            .map(|p| p.confidence)
            .unwrap_or(0.0);

        let signal_strength = morse_confidence.max(period_confidence);
        let cypher_strength = keying_confidence.max(structure_score);
        // The quiet rung. Deliberately not proportional to anything: it is lit
        // much of the time, and its job is to say "something changed here", not
        // to compete for attention with the rungs above it.
        let anomaly_strength: f32 = if anomaly { 0.4 } else { 0.0 };

        // Cumulative, like the lamps: a rung is never dimmer than the one above
        // it, or the ladder would read as broken when the top is the brightest.
        let signal_strength = if signal {
            signal_strength.max(0.5)
        } else {
            0.0
        };
        let cypher_strength = if cypher {
            cypher_strength.max(0.4).max(signal_strength)
        } else {
            signal_strength
        };
        let anomaly_strength = anomaly_strength.max(cypher_strength);

        Self {
            rung: Rung::of(anomaly, cypher, signal),
            strength: [anomaly_strength, cypher_strength, signal_strength],
            keying_suspect: app.keying_suspect(),
            anomaly_detail: anomaly_detail(app),
            cypher_detail,
            signal_detail: period_detail(app),
            spectrogram: None,
            // Set by the caller, which owns the visibility decision.
            showing: false,
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
    let anything = state.rung.is_some();

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

    // Read top-down as a ladder: SIGNAL at the top is the strongest claim, and
    // the lamps below it stay lit, so the display fills upward rather than
    // showing three unrelated verdicts.
    //
    // "SIGNAL" names no particular signal: the Landscape is simply the first
    // periodic transmission anyone found, and the rung is for whatever we can
    // identify. The detail line names the match when there is one.
    let reached = state.rung;
    let lit = |r: Rung| reached.is_some_and(|got| got >= r);
    // A rung that the ladder has reached is lit at its own strength; one it has
    // not is dark regardless.
    let level = |r: Rung, slot: usize| {
        if lit(r) {
            state.strength[slot].max(0.05)
        } else {
            0.0
        }
    };

    hud_lamp(
        &painter,
        row(0.0),
        "SIGNAL",
        &state.signal_detail,
        level(Rung::Signal, 2),
        hud::GREEN,
    );
    // Amber overrides for a suspect detection: "this looks like music" is worth
    // more than "something is encoded".
    let cypher_colour = if state.keying_suspect {
        hud::AMBER
    } else {
        hud::BLUE
    };
    hud_lamp(
        &painter,
        row(1.0),
        "CYPHER",
        &state.cypher_detail,
        level(Rung::Cypher, 1),
        cypher_colour,
    );
    // The bottom rung is deliberately the quietest colour on the panel. It is
    // lit a great deal — ordinary ship ambience produces a departure every
    // twenty seconds or so — and a bright lamp that is usually on teaches you to
    // ignore the panel. What carries information is how far up the ladder goes.
    hud_lamp(
        &painter,
        row(2.0),
        "ANOMALY",
        &state.anomaly_detail,
        level(Rung::Anomaly, 0),
        hud::AMBER,
    );
}

/// One indicator row: dot, name, and its supporting number underneath.
/// One indicator row, lit in proportion to the evidence behind it.
///
/// `strength` is 0..1, and it is not a threshold. This tool exists to find
/// signals nobody has catalogued, which means the expensive mistake is staying
/// dark on something real — a commander who glances at a dim lamp and finds
/// nothing has lost two seconds, while one flown past an undiscovered signal has
/// lost it for good. A binary lamp throws away everything the detectors know
/// short of certainty: a score of 0.84 against a threshold of 0.85 used to look
/// exactly like silence.
///
/// So the pilot is the classifier and this is an attention director. Brightness
/// carries the confidence, the number underneath carries the detail, and the
/// decision stays with the person who can look at the waterfall and judge.
fn hud_lamp(
    painter: &egui::Painter,
    row: egui::Rect,
    label: &str,
    detail: &str,
    strength: f32,
    lit_colour: egui::Color32,
) {
    let strength = strength.clamp(0.0, 1.0);
    // Below this there is genuinely nothing to say, and a lamp that never rests
    // is a lamp nobody reads.
    let lit = strength >= 0.05;
    // Never fully dim while lit: the faintest evidence still has to be visible
    // against the cockpit behind it.
    let intensity = 0.35 + 0.65 * strength;
    let colour = if lit {
        lit_colour.gamma_multiply(intensity)
    } else {
        hud::IDLE
    };
    let centre = egui::pos2(row.left() + 7.0, row.center().y);
    painter.circle_filled(centre, 3.5, colour);
    if lit {
        // A soft ring, so a lit lamp registers in peripheral vision while you
        // are flying rather than needing to be looked at. It grows with the
        // evidence, which is what makes strength readable at a glance.
        painter.circle_stroke(
            centre,
            5.0 + 2.5 * strength,
            egui::Stroke::new(1.0, colour.gamma_multiply(0.5)),
        );
    }

    let x = row.left() + 18.0;
    painter.text(
        egui::pos2(x, row.center().y - 1.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        egui::FontId::monospace(11.0),
        if lit {
            lit_colour.gamma_multiply(intensity)
        } else {
            hud::AMBER
        },
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

/// What the rose should draw for a given estimate.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RoseNeedle {
    /// Off-axis by more than the dead-band: a bearing worth flying towards.
    Bearing(f32),
    /// Inside the dead-band. Balanced ambience pans dead centre, so this is
    /// the null result, and it is drawn in a colour that says so.
    Centred(f32),
}

/// What the rose should draw, if anything.
///
/// `None` only when there is no usable estimate at all — a mono source, or
/// direction finding still warming up. A centred bearing is a real measurement
/// and is shown as one, just not as a find.
pub fn rose_needle(estimate: &DirectionEstimate) -> Option<RoseNeedle> {
    if !estimate.is_usable() {
        return None;
    }
    Some(if estimate.azimuth_deg.abs() >= ROSE_DEADBAND_DEG {
        RoseNeedle::Bearing(estimate.azimuth_deg)
    } else {
        RoseNeedle::Centred(estimate.azimuth_deg)
    })
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

    if let Some(needle_state) = rose_needle(estimate) {
        // Green means "look at this"; red means "measured, and it is nothing".
        // Keeping the eye trained on green is the whole point of the dead-band.
        let (azimuth_deg, colour, ghost) = match needle_state {
            RoseNeedle::Bearing(a) => (a, hud::GREEN, true),
            // Dark orange, the same colour as every other inactive element:
            // present and readable, but it does not pull the eye. Red did.
            RoseNeedle::Centred(a) => (a, hud::AMBER, false),
        };
        let confidence = estimate.confidence.clamp(0.0, 1.0);
        let needle = azimuth_to_vec(azimuth_deg) * radius * (0.25 + 0.75 * confidence);
        painter.line_segment(
            [centre, centre + needle],
            egui::Stroke::new(2.0, colour.gamma_multiply(0.4 + 0.6 * confidence)),
        );
        // No mirrored ghost for a centred needle: front and back are the same
        // direction there, so the second line would say nothing.
        if ghost && estimate.front_back_ambiguous {
            let mirror = azimuth_to_vec(180.0 - azimuth_deg) * radius * 0.5;
            painter.line_segment(
                [centre, centre + mirror],
                egui::Stroke::new(1.0, colour.gamma_multiply(0.25)),
            );
        }
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{:+.0}\u{00b0}", azimuth_deg),
            egui::FontId::monospace(9.0),
            colour,
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
    fn a_centred_bearing_is_shown_but_marked_as_nothing() {
        use crate::analysis::direction::{DirectionEstimate, DirectionMethod};
        let mut e = DirectionEstimate {
            azimuth_deg: 0.0,
            confidence: 0.9,
            method: DirectionMethod::StereoPanLaw,
            front_back_ambiguous: true,
        };
        // Balanced ambience pans centre, so a centred needle is the null
        // result — drawn, but never in the colour that means "look here".
        assert_eq!(rose_needle(&e), Some(RoseNeedle::Centred(0.0)));
        e.azimuth_deg = 2.9;
        assert_eq!(rose_needle(&e), Some(RoseNeedle::Centred(2.9)));
        e.azimuth_deg = -2.9;
        assert_eq!(
            rose_needle(&e),
            Some(RoseNeedle::Centred(-2.9)),
            "the dead-band is symmetric"
        );

        e.azimuth_deg = 3.0;
        assert_eq!(
            rose_needle(&e),
            Some(RoseNeedle::Bearing(3.0)),
            "at the edge it becomes a bearing"
        );
        e.azimuth_deg = -38.0;
        assert_eq!(rose_needle(&e), Some(RoseNeedle::Bearing(-38.0)));

        e.method = DirectionMethod::Insufficient;
        assert_eq!(rose_needle(&e), None, "no usable estimate draws nothing");
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
            signal_detail: "109.7s conf 0.98".into(),
            cypher_detail: "22050 Hz · 123.4/s".into(),
            anomaly_detail: "0.34".into(),
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
        state.signal_detail = "0.11".into();
        state.cypher_detail = "0.52".into();
        let narrow = label_column_width(&ctx, &state);
        assert!(narrow < wide, "narrow {narrow} should beat wide {wide}");

        // And it never collapses below the fixed labels.
        state.anomaly_detail = String::new();
        let bare = label_column_width(&ctx, &state);
        assert!(bare >= 18.0 + 40.0, "must still fit ANOMALY, got {bare}");
    }

    /// The property the whole design rests on: the lamps fill upward, and a
    /// higher rung can never be lit while a lower one is dark.
    #[test]
    fn the_ladder_is_monotonic() {
        // Every combination of what the detectors might be saying.
        for signal in [false, true] {
            for cypher in [false, true] {
                for anomaly in [false, true] {
                    let rung = Rung::of(anomaly, cypher, signal);
                    let lit = |r: Rung| rung.is_some_and(|got| got >= r);
                    if lit(Rung::Signal) {
                        assert!(lit(Rung::Cypher), "SIGNAL lit with CYPHER dark");
                    }
                    if lit(Rung::Cypher) {
                        assert!(lit(Rung::Anomaly), "CYPHER lit with ANOMALY dark");
                    }
                    assert_eq!(
                        rung.is_some(),
                        anomaly || cypher || signal,
                        "a lamp must light for any evidence at all"
                    );
                }
            }
        }
    }

    /// The failure that prompted the redesign: the main window said ANOMALY
    /// while every overlay lamp stayed dark, which from a cockpit is
    /// indistinguishable from the tool being broken.
    #[test]
    fn an_unnamed_anomaly_still_lights_the_panel() {
        let rung = Rung::of(true, false, false);
        assert_eq!(rung, Some(Rung::Anomaly));
        assert!(
            rung.is_some(),
            "the panel must not go dark on a real detection"
        );
    }

    /// A recognised signal reaches the top whether or not the lower detectors
    /// happen to be firing this instant.
    #[test]
    fn a_named_signal_reaches_the_top_rung_alone() {
        assert_eq!(Rung::of(false, false, true), Some(Rung::Signal));
        assert_eq!(Rung::of(false, true, false), Some(Rung::Cypher));
        assert_eq!(Rung::of(false, false, false), None);
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
    fn only_the_validated_detector_gets_the_find_colour() {
        // SIGNAL is the one measurement checked against a known recording;
        // TRANSMIT and STRUCTURE also light on ordinary ship ambience. They
        // must not share a colour, or green stops meaning anything.
        assert_ne!(hud::GREEN, hud::BLUE);
        assert!(
            hud::BLUE.b() > hud::BLUE.g() && hud::BLUE.b() > hud::BLUE.r(),
            "the supporting detectors read blue"
        );
        assert!(
            hud::GREEN.g() > hud::GREEN.b() * 2,
            "the validated detector keeps green to itself"
        );

        // Both must still carry against an unlit lamp on a black panel.
        let sum = |c: egui::Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(
            sum(hud::BLUE) > sum(hud::IDLE) * 3,
            "a lit hint must be legible"
        );
    }

    #[test]
    fn the_centred_needle_recedes_and_only_a_real_bearing_stands_out() {
        let sum = |c: egui::Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        // A centred needle is the null result: it must be legible but must not
        // compete with a real detection for attention.
        // Green 455 against amber 264: brighter, and a different hue family,
        // which is what keeps the eye hunting for green rather than scanning.
        assert!(
            sum(hud::GREEN) > sum(hud::AMBER),
            "the find colour must be the brighter of the two"
        );
        assert!(
            hud::GREEN.g() > hud::GREEN.r() * 2 && hud::GREEN.g() > hud::GREEN.b() * 2,
            "a real bearing reads green"
        );
        assert!(
            hud::AMBER.r() > hud::AMBER.g() && hud::AMBER.b() == 0,
            "a centred bearing reads as dark orange, like every other idle element"
        );
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

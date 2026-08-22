//! The waterfall spectrogram — the primary display.
//!
//! The stored history is far wider and taller than any window: 300 seconds at
//! ~23 frames/s is 7000 columns, against 2049 bins. Both axes are resampled to
//! the widget, and the frequency axis is resampled *logarithmically*, because
//! that is where the structure is — a linear axis spends three quarters of its
//! height above 6 kHz where a mountain-shaped signal has nothing to show.

use eframe::egui;

use crate::analysis::novelty::FrameGeometry;
use crate::analysis::spectrogram::SpectrogramHistory;
use crate::ui::overlay::Rung;

/// Lowest frequency drawn by default. Below this is mains hum and DC.
pub const DEFAULT_MIN_HZ: f32 = 20.0;

/// Highest frequency drawn by default.
///
/// 22050 Hz, not the 24 kHz Nyquist of a 48 kHz stream — this is the value the
/// community's decode guides specify for Audacity and Sonic Visualiser, so a
/// spectrogram from this tool lines up with published reference images instead
/// of being subtly taller.
pub const DEFAULT_MAX_HZ: f32 = 22_050.0;

/// The logarithmic frequency axis shared by the waterfall, its gridlines, and
/// the event overlays.
///
/// Log spacing is not cosmetic. The published decodes — the Landscape Signal's
/// mountain, the Thargoid Probe's image — are only legible on a log axis; a
/// linear one squeezes everything of interest into the bottom tenth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqScale {
    pub min_hz: f32,
    pub max_hz: f32,
}

impl Default for FreqScale {
    fn default() -> Self {
        Self {
            min_hz: DEFAULT_MIN_HZ,
            max_hz: DEFAULT_MAX_HZ,
        }
    }
}

impl FreqScale {
    /// Build a scale, clamped to something drawable for the given Nyquist.
    ///
    /// Asking for more than Nyquist would draw empty rows, and an inverted or
    /// non-positive range would produce `NaN` on a log axis.
    pub fn new(min_hz: f32, max_hz: f32, nyquist_hz: f32) -> Self {
        // `f32::clamp` propagates NaN rather than clamping it, so every input
        // has to be checked for finiteness before it reaches a range.
        let sane = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };

        let nyquist = {
            let n = sane(nyquist_hz, DEFAULT_MAX_HZ);
            if n > 1.0 { n } else { DEFAULT_MAX_HZ }
        };
        let max = sane(max_hz, DEFAULT_MAX_HZ).clamp(2.0, nyquist);
        let min = sane(min_hz, DEFAULT_MIN_HZ).clamp(1.0, max * 0.5);
        Self {
            min_hz: min,
            max_hz: max,
        }
    }

    /// Row index (0 = top) for a frequency.
    pub fn row(&self, hz: f32, height: usize) -> usize {
        if height == 0 {
            return 0;
        }
        let hz = hz.clamp(self.min_hz, self.max_hz);
        let t = (hz / self.min_hz).ln() / (self.max_hz / self.min_hz).ln();
        let row = ((1.0 - t) * (height - 1) as f32).round() as isize;
        row.clamp(0, height as isize - 1) as usize
    }

    /// Frequency at a row — the inverse of [`FreqScale::row`].
    pub fn hz(&self, row: usize, height: usize) -> f32 {
        if height <= 1 {
            return self.max_hz;
        }
        let t = 1.0 - row as f32 / (height - 1) as f32;
        self.min_hz * (self.max_hz / self.min_hz).powf(t)
    }
}

/// Map a normalized 0..1 intensity to colour.
///
/// A perceptual ramp — black through blue and magenta to yellow — so a weak
/// stroke against the background floor is still visible, which a plain
/// greyscale ramp does not manage.
pub fn colormap(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    // Piecewise linear through five anchors.
    const ANCHORS: [[f32; 3]; 5] = [
        [0.0, 0.0, 0.05],
        [0.15, 0.05, 0.35],
        [0.55, 0.10, 0.50],
        [0.90, 0.35, 0.20],
        [1.00, 0.95, 0.55],
    ];
    let scaled = t * (ANCHORS.len() - 1) as f32;
    let i = (scaled.floor() as usize).min(ANCHORS.len() - 2);
    let f = scaled - i as f32;
    let (a, b) = (ANCHORS[i], ANCHORS[i + 1]);
    [
        (((a[0] + (b[0] - a[0]) * f) * 255.0) as u8),
        (((a[1] + (b[1] - a[1]) * f) * 255.0) as u8),
        (((a[2] + (b[2] - a[2]) * f) * 255.0) as u8),
    ]
}

/// Choose the dB window the colour ramp spans.
///
/// A fixed −100…0 dBFS window wastes most of the ramp: the signals worth seeing
/// sit near the noise floor, so they land in the bottom fifth of the colours and
/// are all but invisible. Sonic Visualiser normalises to what is actually
/// present, and so does this — the ramp is stretched between two percentiles of
/// the visible data.
///
/// Returns quantized `(low, high)` bounds, never inverted.
pub fn auto_gain_bounds(counts: &[u32; 256], low_pct: f32, high_pct: f32) -> (u8, u8) {
    let total: u64 = counts.iter().map(|c| *c as u64).sum();
    if total == 0 {
        return (0, 255);
    }
    let pick = |pct: f32| -> u8 {
        let target = (total as f32 * pct.clamp(0.0, 1.0)) as u64;
        let mut acc = 0u64;
        for (value, count) in counts.iter().enumerate() {
            acc += *count as u64;
            if acc >= target {
                return value as u8;
            }
        }
        255
    };
    let low = pick(low_pct);
    let high = pick(high_pct);
    // Always leave a usable span, however flat the data.
    if high > low + 4 {
        (low, high)
    } else {
        (low, low.saturating_add(5).max(5))
    }
}

/// How to render a spectrogram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderOptions {
    pub scale: FreqScale,
    /// Stretch the colour ramp over the data actually present.
    pub auto_gain: bool,
    /// Subtract each frequency row's median across the visible window.
    ///
    /// Anything constantly present at a given frequency — ship rumble, life
    /// support, a steady drone — has a high median and subtracts to nothing,
    /// leaving only what varied. It is strictly better than the live background
    /// model for a *rendered* image, because here we can see the whole window at
    /// once rather than only the past.
    pub median_subtract: bool,
    /// Time span the image covers, in frames.
    pub window_frames: usize,
}

impl RenderOptions {
    pub fn new(scale: FreqScale, window_frames: usize) -> Self {
        Self {
            scale,
            auto_gain: true,
            median_subtract: true,
            window_frames,
        }
    }
}

/// Median of a row, via a 256-bin histogram — the values are already quantized,
/// so this is O(n) rather than a sort.
fn row_median(row: &[u8]) -> u8 {
    if row.is_empty() {
        return 0;
    }
    let mut counts = [0u32; 256];
    for v in row {
        counts[*v as usize] += 1;
    }
    let target = row.len() as u32 / 2;
    let mut acc = 0u32;
    for (value, count) in counts.iter().enumerate() {
        acc += *count;
        if acc > target {
            return value as u8;
        }
    }
    255
}

/// Build an RGB image of the history, newest column on the right.
///
/// Columns are decimated by taking the maximum over each group rather than the
/// mean: a one-frame chirp inside a group must survive being squeezed into a
/// single pixel, and averaging would erase it.
pub fn build_image(
    history: &SpectrogramHistory,
    geometry: FrameGeometry,
    options: RenderOptions,
    width: usize,
    height: usize,
) -> egui::ColorImage {
    let (rgb, w, h) = render_rgb(history, geometry, options, width, height);
    egui::ColorImage::from_rgb([w, h], &rgb)
}

/// Render the spectrogram to raw RGB. Shared by the on-screen waterfall and the
/// high-resolution PNG export, so what you export is what you saw.
/// Render the spectrogram to raw RGB.
///
/// Time-per-pixel stays fixed at `window_frames / width`, and data is anchored
/// to the right. While the buffer is still filling the left side stays blank
/// rather than the image stretching to fit — otherwise the scroll rate changes
/// as the session runs, and every interval read off the image is wrong until
/// the buffer happens to be full.
pub fn render_rgb(
    history: &SpectrogramHistory,
    geometry: FrameGeometry,
    options: RenderOptions,
    width: usize,
    height: usize,
) -> (Vec<u8>, usize, usize) {
    let RenderOptions {
        scale,
        auto_gain,
        median_subtract,
        window_frames,
    } = options;
    let width = width.max(1);
    let height = height.max(1);
    let mut rgb = vec![0u8; width * height * 3];

    let frames = history.len();
    if frames == 0 {
        return (rgb, width, height);
    }

    let range = history.range();
    let nyquist = geometry.nyquist_hz();
    let bins = history.frame_width();

    // Which history frames feed each output column.
    let per_column = (frames as f32 / width as f32).max(1.0);
    // Which bins feed each output row, precomputed once.
    let row_bins: Vec<(usize, usize)> = (0..height)
        .map(|row| {
            // Bins are linear in frequency, so the mapping is against Nyquist
            // even though the row positions come from the log scale.
            let hi_hz = scale.hz(row, height);
            let lo_hz = scale.hz((row + 1).min(height - 1), height);
            let lo = ((lo_hz / nyquist) * (bins - 1) as f32).floor().max(0.0) as usize;
            let hi = ((hi_hz / nyquist) * (bins - 1) as f32).ceil() as usize;
            (lo.min(bins - 1), hi.clamp(lo + 1, bins))
        })
        .collect();

    // Pool once into a quantized buffer, then colour it. Two passes are needed
    // anyway for auto-gain, and pooling twice would be the expensive half.
    let mut pooled = vec![0u8; width * height];
    let mut blank = vec![true; width];
    #[allow(unused_assignments)]
    let mut counts = [0u32; 256];

    let window = window_frames.max(1);
    // Frames older than the window simply are not shown; frames not yet
    // captured leave blank columns on the left.
    let missing = window.saturating_sub(frames);

    for col in 0..width {
        // Position within the fixed window, then offset into what we actually
        // hold. This is what keeps time-per-pixel constant.
        let win_start = (col as f64 * window as f64 / width as f64) as isize;
        let win_end = ((col + 1) as f64 * window as f64 / width as f64).ceil() as isize;
        let start = win_start - missing as isize;
        let end = win_end - missing as isize;
        if end <= 0 {
            continue; // before any data we hold
        }
        let start = start.max(0) as usize;
        let end = (end as usize).min(frames).max(start + 1).min(frames);
        if start >= frames {
            continue;
        }
        blank[col] = false;
        let _ = per_column;

        for (row, &(lo, hi)) in row_bins.iter().enumerate() {
            let mut peak = 0u8;
            for f in start..end {
                let Some(frame) = history.frame_at(f) else {
                    continue;
                };
                for q in &frame[lo..hi.min(frame.len())] {
                    peak = peak.max(*q);
                }
            }
            pooled[row * width + col] = peak;
            counts[peak as usize] += 1;
        }
    }

    if median_subtract {
        // Remove each frequency row's steady level. A constant band has a high
        // median and vanishes; a stroke crossing the row barely moves it.
        for row in 0..height {
            let start = row * width;
            let slice = &mut pooled[start..start + width];
            let median = row_median(slice);
            for v in slice.iter_mut() {
                *v = v.saturating_sub(median);
            }
        }
        // Counts were gathered before subtraction, so redo them.
        counts = [0u32; 256];
        for v in pooled.iter() {
            counts[*v as usize] += 1;
        }
    }

    let (low, high) = if auto_gain {
        // Ignore the quietest fifth — that is the floor — and clip only the
        // loudest 0.1%. Cutting at the median clipped so much of the upper half
        // that whole regions saturated to flat yellow and the strokes vanished
        // into it.
        auto_gain_bounds(&counts, 0.20, 0.999)
    } else {
        (range.quantize(-100.0), range.quantize(0.0))
    };
    let span = (high as f32 - low as f32).max(1.0);

    for (i, q) in pooled.iter().enumerate() {
        if blank[i % width] {
            continue; // leave not-yet-captured time black
        }
        let t = ((*q as f32 - low as f32) / span).clamp(0.0, 1.0);
        let [r, g, b] = colormap(t);
        rgb[i * 3] = r;
        rgb[i * 3 + 1] = g;
        rgb[i * 3 + 2] = b;
    }

    (rgb, width, height)
}

/// Write the spectrogram as a PNG at arbitrary resolution.
///
/// The on-screen waterfall is limited to the window size; comparing against the
/// community's published decodes needs far more pixels than that.
pub fn export_png(
    history: &SpectrogramHistory,
    geometry: FrameGeometry,
    options: RenderOptions,
    width: usize,
    height: usize,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let (rgb, w, h) = render_rgb(history, geometry, options, width, height);
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer
        .write_image_data(&rgb)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}

/// Draw frequency gridlines and labels over the waterfall.
pub fn draw_axes(painter: &egui::Painter, rect: egui::Rect, scale: FreqScale, seconds: f32) {
    let faint = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 40);
    let label = egui::Color32::from_gray(190);
    let font = egui::FontId::monospace(10.0);

    for hz in [50.0f32, 100.0, 500.0, 1000.0, 5000.0, 10_000.0, 20_000.0] {
        if hz > scale.max_hz || hz < scale.min_hz {
            continue;
        }
        let row = scale.row(hz, rect.height() as usize);
        let y = rect.top() + row as f32;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(1.0, faint),
        );
        let text = if hz >= 1000.0 {
            format!("{:.0}k", hz / 1000.0)
        } else {
            format!("{hz:.0}")
        };
        painter.text(
            egui::pos2(rect.left() + 3.0, y),
            egui::Align2::LEFT_BOTTOM,
            text,
            font.clone(),
            label,
        );
    }

    // Time ticks along the bottom, counting back from now.
    for fraction in [0.0f32, 0.25, 0.5, 0.75] {
        let x = rect.left() + rect.width() * fraction;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, faint),
        );
        painter.text(
            egui::pos2(x + 3.0, rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            format!("-{:.0}s", seconds * (1.0 - fraction)),
            font.clone(),
            label,
        );
    }
    painter.text(
        egui::pos2(rect.right() - 3.0, rect.bottom() - 2.0),
        egui::Align2::RIGHT_BOTTOM,
        "now",
        font,
        label,
    );
}

/// Project timed spans onto a fixed number of slices across a time window.
///
/// `spans` are `(start_seconds, end_seconds, rung)` on the same clock as `now`.
/// The result runs oldest first, one entry per slice, each holding the strongest
/// rung that overlaps it.
///
/// Pulled out of the drawing code and given tests because the version written
/// inline stopped marking anything at all, and inline arithmetic inside a
/// closure is where that kind of mistake hides.
pub fn project_spans(
    now: f64,
    window_seconds: f64,
    slices: usize,
    spans: &[(f64, f64, Rung)],
) -> Vec<Option<Rung>> {
    let mut out = vec![None; slices];
    if slices == 0 || window_seconds <= 0.0 {
        return out;
    }
    let oldest = now - window_seconds;
    for &(from, to, rung) in spans {
        let (from, to) = (from.min(to), from.max(to));
        // Anything wholly outside the window is not drawn; anything overlapping
        // it is clipped, so a long detection does not vanish while it is still
        // partly on screen.
        if to < oldest || from > now {
            continue;
        }
        let position = |t: f64| -> usize {
            let clamped = t.clamp(oldest, now);
            let fraction = (clamped - oldest) / window_seconds;
            ((fraction * (slices - 1) as f64).round() as usize).min(slices - 1)
        };
        let (a, b) = (position(from), position(to));
        for slot in out.iter_mut().take(b + 1).skip(a) {
            if Some(rung) > *slot {
                *slot = Some(rung);
            }
        }
    }
    out
}

/// Paint the lamp history into the bottom rows of a spectrogram image.
///
/// Into the *image*, not over it with the painter. The two were drawn on
/// different clocks — the spectrogram rebuilt on the snapshot interval and
/// cached, the strip repainted every frame — so they scrolled at different rates
/// and the strip visibly lagged the rows it was describing. Baked into the same
/// buffer they cannot disagree: one bitmap, formed together, displayed together.
///
/// `slices` runs oldest first and shares the image's own time axis, so a mark
/// sits directly beneath the column that produced it.
pub fn paint_timeline(image: &mut egui::ColorImage, slices: &[Option<Rung>]) {
    if slices.is_empty() {
        return;
    }
    const HEIGHT: usize = 5;
    let [width, height] = [image.width(), image.height()];
    if width == 0 || height <= HEIGHT {
        return;
    }
    for x in 0..width {
        let index = x * slices.len() / width;
        let colour = match slices[index.min(slices.len() - 1)] {
            Some(Rung::Signal) => egui::Color32::from_rgb(80, 255, 120),
            Some(Rung::Cypher) => egui::Color32::from_rgb(77, 166, 255),
            Some(Rung::Anomaly) => egui::Color32::from_rgb(177, 87, 0),
            // Not black: a strip that disappears where nothing happened cannot
            // be told apart from the spectrogram above it.
            None => egui::Color32::from_gray(40),
        };
        for y in height - HEIGHT..height {
            image[(x, y)] = colour;
        }
    }
}

/// Draw the lamp history as a strip along the bottom of a spectrogram.
///
/// `slices` runs oldest first and shares the spectrogram's own time axis, so a
/// coloured mark sits directly beneath the rows that produced it. That
/// alignment is the whole value: a lamp says something happened, this says
/// *when*, against the picture.
pub fn draw_timeline(painter: &egui::Painter, rect: egui::Rect, slices: &[Option<Rung>]) {
    if slices.is_empty() {
        return;
    }
    const HEIGHT: f32 = 5.0;
    let strip = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - HEIGHT),
        rect.right_bottom(),
    );
    let width = strip.width() / slices.len() as f32;
    for (i, rung) in slices.iter().enumerate() {
        let colour = match rung {
            Some(Rung::Signal) => egui::Color32::from_rgb(80, 255, 120),
            Some(Rung::Cypher) => egui::Color32::from_rgb(77, 166, 255),
            Some(Rung::Anomaly) => egui::Color32::from_rgb(177, 87, 0),
            // Not black: a strip that disappears where nothing happened cannot
            // be told apart from the spectrogram above it.
            None => egui::Color32::from_gray(40),
        };
        let x = strip.left() + i as f32 * width;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x, strip.top()),
                egui::pos2(x + width.max(1.0), strip.bottom()),
            ),
            0.0,
            colour,
        );
    }
}

/// One detection's extent on the waterfall, in time-ago and frequency.
#[derive(Debug, Clone, Copy)]
pub struct EventBox {
    pub seconds_ago_start: f32,
    pub seconds_ago_end: f32,
    pub low_hz: f32,
    pub high_hz: f32,
    /// Whether the audio was written to disk, which changes the outline colour.
    pub captured: bool,
    /// A stroke the tracer followed, rather than a detected event.
    ///
    /// Drawn differently on purpose: an event box says "something crossed a
    /// threshold in this band", which on real recordings covers most of the
    /// display, while a traced stroke is the extent of one followed line. They
    /// are different claims and should not look alike.
    pub traced: bool,
}

/// Outline a detected event on the waterfall.
pub fn draw_event_box(
    painter: &egui::Painter,
    rect: egui::Rect,
    scale: FreqScale,
    window_seconds: f32,
    event: EventBox,
) {
    let EventBox {
        seconds_ago_start,
        seconds_ago_end,
        low_hz,
        high_hz,
        captured,
        traced,
    } = event;
    if window_seconds <= 0.0 || seconds_ago_start > window_seconds {
        return;
    }
    let x_of = |ago: f32| rect.right() - (ago / window_seconds).clamp(0.0, 1.0) * rect.width();
    let height = rect.height() as usize;
    let y_top = rect.top() + scale.row(high_hz, height) as f32;
    let y_bottom = rect.top() + scale.row(low_hz, height) as f32;

    let box_rect = egui::Rect::from_min_max(
        egui::pos2(x_of(seconds_ago_start), y_top - 2.0),
        egui::pos2(x_of(seconds_ago_end), y_bottom + 2.0),
    );
    let colour = if traced {
        // Cyan: not a threshold crossing, a line that was followed.
        egui::Color32::from_rgb(90, 220, 255)
    } else if captured {
        egui::Color32::from_rgb(120, 255, 160)
    } else {
        egui::Color32::from_rgb(255, 210, 90)
    };
    painter.rect_stroke(
        box_rect,
        2.0,
        egui::Stroke::new(if traced { 1.0 } else { 1.5 }, colour),
        egui::StrokeKind::Outside,
    );
}

#[cfg(test)]
mod tests {

    /// Both halves of a field report: the strip stayed dark while a detection was
    /// visible, then turned entirely green the moment that detection scrolled off
    /// the left edge.
    ///
    /// One cause. Clamping each end of a span independently meant a span wholly
    /// in the past had its start clamped to the first slice and its end to the
    /// last — painting everything. And a span that failed to resolve was skipped
    /// rather than clipped, so nothing was drawn while it was on screen.
    #[test]
    fn a_span_that_has_scrolled_away_marks_nothing() {
        let now = 300.0;
        let window = 140.0;
        // Ended eighty seconds before the window even begins.
        let gone = [(60.0, 80.0, Rung::Signal)];
        let out = project_spans(now, window, 100, &gone);
        assert!(
            out.iter().all(|s| s.is_none()),
            "a detection off the left edge must mark nothing, got {} marks",
            out.iter().filter(|s| s.is_some()).count()
        );
    }

    #[test]
    fn a_visible_span_marks_its_own_place_and_nothing_else() {
        let now = 300.0;
        let window = 140.0;
        // Ran from 54 s ago to 40 s ago. Oldest visible is 160 s, so it should
        // land at (246-160)/140 = 61% through the strip and end at 71%.
        let out = project_spans(now, window, 100, &[(246.0, 260.0, Rung::Signal)]);
        let marked: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .map(|(i, _)| i)
            .collect();
        assert!(!marked.is_empty(), "a visible detection must be drawn");
        let (first, last) = (marked[0], marked[marked.len() - 1]);
        assert!(
            (59..=63).contains(&first) && (69..=73).contains(&last),
            "expected roughly slices 61..71, got {first}..{last}"
        );
    }

    /// Partly off the edge is clipped, not dropped: a long detection must not
    /// vanish while half of it is still on screen.
    #[test]
    fn a_span_hanging_off_the_edge_is_clipped() {
        let out = project_spans(300.0, 140.0, 100, &[(100.0, 200.0, Rung::Anomaly)]);
        assert_eq!(out[0], Some(Rung::Anomaly), "the visible part is drawn");
        assert!(out.iter().any(|s| s.is_none()), "but only the visible part");
    }

    #[test]
    fn the_strongest_rung_wins_where_spans_overlap() {
        let out = project_spans(
            300.0,
            140.0,
            100,
            &[(250.0, 270.0, Rung::Anomaly), (255.0, 260.0, Rung::Signal)],
        );
        assert!(out.contains(&Some(Rung::Signal)));
        assert!(out.contains(&Some(Rung::Anomaly)));
    }

    #[test]
    fn degenerate_projections_are_empty_rather_than_wrong() {
        assert!(project_spans(300.0, 140.0, 0, &[(1.0, 2.0, Rung::Signal)]).is_empty());
        assert!(
            project_spans(300.0, 0.0, 10, &[(1.0, 2.0, Rung::Signal)])
                .iter()
                .all(|s| s.is_none())
        );
        assert!(
            project_spans(300.0, 140.0, 10, &[])
                .iter()
                .all(|s| s.is_none())
        );
    }
    use super::*;
    use crate::analysis::spectrogram::DbRange;

    const GEOM: FrameGeometry = FrameGeometry {
        sample_rate: 48_000,
        fft_size: 1024,
        hop: 512,
    };

    fn scale() -> FreqScale {
        FreqScale::new(DEFAULT_MIN_HZ, DEFAULT_MAX_HZ, GEOM.nyquist_hz())
    }

    fn opts(window_frames: usize) -> RenderOptions {
        RenderOptions::new(scale(), window_frames)
    }

    #[test]
    fn the_default_scale_is_the_guides_twenty_to_22050() {
        let s = scale();
        assert_eq!(s.min_hz, 20.0);
        assert_eq!(s.max_hz, 22_050.0);
        // Deliberately below the 24 kHz Nyquist of a 48 kHz stream.
        assert!(s.max_hz < GEOM.nyquist_hz());
    }

    #[test]
    fn the_scale_never_exceeds_nyquist() {
        // A 44.1 kHz stream cannot show 22050 plus headroom.
        let s = FreqScale::new(20.0, 30_000.0, 22_050.0);
        assert_eq!(s.max_hz, 22_050.0);
        let narrow = FreqScale::new(20.0, 22_050.0, 8_000.0);
        assert_eq!(narrow.max_hz, 8_000.0);
    }

    #[test]
    fn a_nonsense_scale_is_clamped_rather_than_producing_nan() {
        for (min, max, nyquist) in [
            (0.0f32, 22_050.0f32, 24_000.0f32),
            (-5.0, 22_050.0, 24_000.0),
            (5000.0, 100.0, 24_000.0),
            (20.0, 22_050.0, 0.0),
            (f32::NAN, 22_050.0, 24_000.0),
        ] {
            let s = FreqScale::new(min, max, nyquist);
            assert!(s.min_hz > 0.0 && s.max_hz > s.min_hz, "{s:?}");
            for row in [0usize, 50, 199] {
                assert!(s.hz(row, 200).is_finite(), "{s:?} row {row}");
            }
            assert!(s.row(1000.0, 200) < 200);
        }
    }

    #[test]
    fn colormap_runs_dark_to_bright() {
        let low = colormap(0.0);
        let high = colormap(1.0);
        let sum = |c: [u8; 3]| c[0] as u32 + c[1] as u32 + c[2] as u32;
        assert!(sum(low) < sum(high));
        let mut previous = 0;
        for i in 0..=20 {
            let s = sum(colormap(i as f32 / 20.0));
            assert!(s >= previous, "brightness dipped at {i}");
            previous = s;
        }
    }

    #[test]
    fn colormap_clamps_out_of_range_input() {
        assert_eq!(colormap(-5.0), colormap(0.0));
        assert_eq!(colormap(5.0), colormap(1.0));
    }

    #[test]
    fn the_frequency_axis_is_logarithmic_and_inverts_cleanly() {
        let s = scale();
        let height = 256;
        assert_eq!(s.row(s.max_hz, height), 0);
        assert_eq!(s.row(s.min_hz, height), height - 1);

        for hz in [25.0f32, 100.0, 1000.0, 10_000.0, 21_000.0] {
            let row = s.row(hz, height);
            let back = s.hz(row, height);
            let ratio = back / hz;
            assert!(
                (0.97..1.03).contains(&ratio),
                "{hz} Hz round-tripped to {back}"
            );
        }
    }

    #[test]
    fn log_spacing_gives_the_low_end_real_estate_a_linear_axis_would_not() {
        let s = scale();
        let height = 256;
        // 20 Hz - 1 kHz should outrank 11 kHz - 22 kHz, despite covering a far
        // smaller slice of the linear span.
        let low_rows = s.row(20.0, height) - s.row(1000.0, height);
        let high_rows = s.row(11_000.0, height) - s.row(22_000.0, height);
        assert!(low_rows > high_rows * 4, "{low_rows} vs {high_rows}");
    }

    #[test]
    fn out_of_range_frequencies_clamp_rather_than_panic() {
        let s = scale();
        assert_eq!(s.row(0.0, 100), 99);
        assert_eq!(s.row(1e9, 100), 0);
        assert_eq!(s.row(1000.0, 0), 0);
        assert!(s.hz(0, 1).is_finite());
    }

    #[test]
    fn an_empty_history_renders_a_blank_image_of_the_right_size() {
        let history = SpectrogramHistory::new(513, 10, DbRange::default());
        let image = build_image(&history, GEOM, opts(history.len()), 200, 100);
        assert_eq!(image.size, [200, 100]);
    }

    #[test]
    fn a_loud_bin_becomes_a_bright_row() {
        let mut history = SpectrogramHistory::new(513, 20, DbRange::default());
        // 1 kHz is bin 1000 * 1024 / 48000 = 21.
        let mut frame = vec![-120.0f32; 513];
        frame[21] = 0.0;
        for _ in 0..20 {
            history.push_db(&frame);
        }

        let (w, h) = (64, 128);
        let s = scale();
        let image = build_image(
            &history,
            GEOM,
            RenderOptions {
                scale: s,
                median_subtract: false,
                ..opts(history.len())
            },
            w,
            h,
        );
        let row = s.row(1000.0, h);

        let brightness = |r: usize| -> u32 {
            (0..w)
                .map(|c| {
                    let p = image.pixels[r * w + c];
                    p.r() as u32 + p.g() as u32 + p.b() as u32
                })
                .sum()
        };
        let quiet_row = if row > h / 2 { 5 } else { h - 5 };
        assert!(
            brightness(row) > brightness(quiet_row) * 3,
            "signal row {row} was not distinctly brighter"
        );
    }

    #[test]
    fn column_decimation_keeps_a_transient_rather_than_averaging_it_away() {
        let mut history = SpectrogramHistory::new(513, 200, DbRange::default());
        let quiet = vec![-120.0f32; 513];
        let mut loud = quiet.clone();
        loud[21] = 0.0;
        for i in 0..200 {
            history.push_db(if i == 100 { &loud } else { &quiet });
        }

        // Squeeze 200 frames into 10 columns: the single loud frame must survive.
        let image = build_image(
            &history,
            GEOM,
            RenderOptions {
                median_subtract: false,
                ..opts(history.len())
            },
            10,
            128,
        );
        let brightest = image
            .pixels
            .iter()
            .map(|p| p.r() as u32 + p.g() as u32 + p.b() as u32)
            .max()
            .unwrap();
        let floor = {
            let [r, g, b] = colormap(0.0);
            r as u32 + g as u32 + b as u32
        };
        assert!(
            brightest > floor + 100,
            "the transient was averaged away (brightest {brightest}, floor {floor})"
        );
    }

    #[test]
    fn auto_gain_stretches_the_ramp_over_what_is_present() {
        // A faint signal sitting just above a floor: fixed bounds would map it
        // into the bottom of the ramp and make it invisible.
        let mut counts = [0u32; 256];
        counts[20] = 9000; // the floor
        counts[40] = 900; // faint structure
        counts[250] = 5; // one bright transient

        let (low, high) = auto_gain_bounds(&counts, 0.50, 0.998);
        assert!(low <= 20, "the floor should sit at or below the low bound");
        assert!(high < 250, "a rare transient must not set the top: {high}");
        assert!(high > low + 4, "the ramp needs a usable span");
        // The faint structure lands in the upper half rather than the bottom.
        let t = (40.0 - low as f32) / (high as f32 - low as f32).max(1.0);
        assert!(t > 0.3, "faint structure mapped to {t}, still too dark");
    }

    #[test]
    fn auto_gain_survives_degenerate_input() {
        let empty = [0u32; 256];
        let (low, high) = auto_gain_bounds(&empty, 0.5, 0.998);
        assert!(high > low);

        let mut flat = [0u32; 256];
        flat[7] = 1000; // every pixel identical
        let (low, high) = auto_gain_bounds(&flat, 0.5, 0.998);
        assert!(high > low, "a flat image still needs a non-zero span");
    }

    #[test]
    fn export_writes_a_readable_png() {
        use crate::analysis::spectrogram::DbRange;

        let mut history = SpectrogramHistory::new(513, 200, DbRange::default());
        let mut frame = vec![-110.0f32; 513];
        for i in 0..200 {
            frame[21] = if i % 20 < 10 { -30.0 } else { -110.0 };
            history.push_db(&frame);
        }

        let path = std::env::temp_dir().join(format!(
            "ed-compass-export-{}-{:?}.png",
            std::process::id(),
            std::thread::current().id()
        ));
        export_png(&history, GEOM, opts(history.len()), 640, 320, &path).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 1000, "png looks empty: {} bytes", meta.len());
        let header = std::fs::read(&path).unwrap();
        assert_eq!(&header[1..4], b"PNG", "not a PNG file");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_time_axis_is_linear() {
        // Frequency is logarithmic; time must not be. Evenly spaced events in
        // must come out evenly spaced, or every interval read off the image —
        // including the 109.5 s period — is wrong.
        use crate::analysis::spectrogram::DbRange;

        let mut history = SpectrogramHistory::new(513, 1200, DbRange::default());
        let quiet = vec![-110.0f32; 513];
        let mut loud = quiet.clone();
        loud[200] = 0.0;
        // An impulse every 100 frames.
        for i in 0..1200 {
            history.push_db(if i % 100 == 0 { &loud } else { &quiet });
        }

        let (w, h) = (600usize, 200usize);
        let (rgb, _, _) = render_rgb(
            &history,
            GEOM,
            RenderOptions {
                median_subtract: false,
                ..opts(history.len())
            },
            w,
            h,
        );

        // Column brightness, then the columns containing an impulse.
        let brightness: Vec<u32> = (0..w)
            .map(|c| {
                (0..h)
                    .map(|r| {
                        let i = (r * w + c) * 3;
                        rgb[i] as u32 + rgb[i + 1] as u32 + rgb[i + 2] as u32
                    })
                    .sum()
            })
            .collect();
        let peak = *brightness.iter().max().unwrap();
        let hot: Vec<usize> = (0..w).filter(|c| brightness[*c] > peak / 2).collect();

        // Group adjacent hot columns into events.
        let mut events: Vec<usize> = Vec::new();
        for c in hot {
            if events.last().is_none_or(|last| c > last + 2) {
                events.push(c);
            }
        }
        assert!(
            events.len() >= 8,
            "expected many impulses, found {}",
            events.len()
        );

        let gaps: Vec<usize> = events.windows(2).map(|w| w[1] - w[0]).collect();
        let min = *gaps.iter().min().unwrap();
        let max = *gaps.iter().max().unwrap();
        assert!(
            max - min <= 1,
            "time axis is not linear: impulse spacing varies {min}..{max} px ({gaps:?})"
        );
    }

    #[test]
    fn median_subtraction_removes_a_steady_band_and_keeps_a_sweep() {
        // The case that motivated it: a loud constant rumble hiding a faint
        // diagonal. Steady content must vanish; the sweep must survive.
        use crate::analysis::spectrogram::DbRange;

        let bins = 513;
        let mut history = SpectrogramHistory::new(bins, 600, DbRange::default());
        for i in 0..600 {
            let mut frame = vec![-110.0f32; bins];
            // A loud, permanent low band — the rumble.
            for bin in frame.iter_mut().take(40) {
                *bin = -20.0;
            }
            // A faint sweep climbing across the window, 25 dB quieter.
            let swept = 80 + i / 3;
            if swept < bins {
                frame[swept] = -45.0;
            }
            history.push_db(&frame);
        }

        let (w, h) = (400usize, 200usize);
        let scale = FreqScale::new(20.0, 24_000.0, GEOM.nyquist_hz());
        let base = RenderOptions {
            scale,
            auto_gain: true,
            median_subtract: false,
            window_frames: 600,
        };

        let brightness = |rgb: &[u8], row: usize| -> u32 {
            (0..w)
                .map(|c| {
                    let i = (row * w + c) * 3;
                    rgb[i] as u32 + rgb[i + 1] as u32 + rgb[i + 2] as u32
                })
                .sum()
        };

        let rumble_row = scale.row(GEOM.bin_hz(20), h);
        let (plain, _, _) = render_rgb(&history, GEOM, base, w, h);
        let (subtracted, _, _) = render_rgb(
            &history,
            GEOM,
            RenderOptions {
                median_subtract: true,
                ..base
            },
            w,
            h,
        );

        assert!(
            brightness(&subtracted, rumble_row) < brightness(&plain, rumble_row) / 2,
            "the steady band should be largely removed"
        );

        // The sweep still has to be somewhere: total brightness must not collapse.
        let total = |rgb: &[u8]| -> u64 { rgb.iter().map(|v| *v as u64).sum() };
        assert!(
            total(&subtracted) > 0,
            "subtraction must not blank the whole image"
        );
    }

    #[test]
    fn row_median_finds_the_steady_level() {
        // Mostly a constant band with a couple of bright crossings.
        let mut row = vec![90u8; 100];
        row[10] = 250;
        row[60] = 240;
        assert_eq!(
            row_median(&row),
            90,
            "a few crossings must not move the median"
        );
        assert_eq!(row_median(&[]), 0);
    }

    #[test]
    fn a_partly_filled_buffer_does_not_stretch_to_fit() {
        // The scroll rate must not change as the session runs. Half a window of
        // data belongs in the right half of the image, with the left blank —
        // stretching it across the full width makes early frames appear to move
        // faster than later ones.
        use crate::analysis::spectrogram::DbRange;

        let mut history = SpectrogramHistory::new(513, 1000, DbRange::default());
        let mut frame = vec![-110.0f32; 513];
        frame[200] = 0.0;
        for _ in 0..500 {
            history.push_db(&frame);
        }

        let (w, h) = (400usize, 100usize);
        // Window of 1000 frames, but only 500 held.
        let (rgb, _, _) = render_rgb(
            &history,
            GEOM,
            RenderOptions {
                median_subtract: false,
                ..opts(1000)
            },
            w,
            h,
        );

        let column_lit = |c: usize| -> bool {
            (0..h).any(|r| {
                let i = (r * w + c) * 3;
                rgb[i] as u32 + rgb[i + 1] as u32 + rgb[i + 2] as u32 > 30
            })
        };
        assert!(
            !column_lit(10),
            "the left should be blank, not stretched data"
        );
        assert!(!column_lit(150), "still before the data starts");
        assert!(column_lit(300), "the right half should carry the data");
        assert!(
            column_lit(w - 2),
            "the newest frame belongs at the right edge"
        );
    }

    #[test]
    fn a_narrowed_scale_zooms_rather_than_reordering() {
        // Restricting the range must keep the axis monotonic and still put the
        // higher frequency nearer the top.
        let s = FreqScale::new(200.0, 4000.0, 24_000.0);
        assert!(s.row(3000.0, 128) < s.row(500.0, 128));
        assert_eq!(s.row(100.0, 128), s.row(200.0, 128), "below range clamps");
        assert_eq!(s.row(9000.0, 128), s.row(4000.0, 128), "above range clamps");
    }
}

//! Overlay frequency zoom: show the band a detection is actually in.
//!
//! The overlay strip is a few hundred pixels tall and spans the whole detection
//! band, so a signal occupying two hundred hertz of it gets a handful of rows.
//! The data does not have that problem — the waterfall keeps every bin at full
//! resolution and the display band is only a render parameter — so narrowing the
//! band while something is detected is not magnification. It re-renders the same
//! history across the full height, and detail that was never on screen appears.
//!
//! The whole difficulty is *when* to move, not where to. Ambience produces
//! detections constantly — measured on field recordings, about one every twenty
//! seconds with no signal present — and a view that chased each one would animate
//! without pause and be worse than useless. So the view is **rate-limited rather
//! than event-driven**:
//!
//! > At most one move every [`Config::overlay_zoom_lockout_seconds`]. Whenever a
//! > move is allowed, it goes to whatever the correct band is *at that moment*.
//!
//! That single rule replaces every special case. A detection arriving mid-lockout
//! interrupts nothing; when the lockout lifts the view makes one move to wherever
//! things now stand, which may be that band, a wider band covering several live
//! detections, or all the way back out. It cannot oscillate, because it never
//! makes two moves inside one window.
//!
//! A consequence worth knowing: the lockout usually outlasts the hold. A
//! detection ending after five seconds wants the view back at twenty and gets it
//! at thirty. The effective dwell is the lockout, not the hold.

use std::time::{Duration, Instant};

/// How long an animation takes.
///
/// Long enough to read as movement rather than a cut, short enough not to be
/// something you wait through. It was 450 ms, which was fine in principle and
/// wrong in practice: the overlay viewport repainted every 66 ms, so the move
/// got seven frames and read as a glitch rather than a movement. The repaint
/// rate is fixed separately; this is slower as well, because a cockpit is a
/// place where sudden motion in the corner of your eye is a cost.
///
/// The frequency axis is logarithmic, so the interpolation is too — moving
/// linearly in hertz would crawl across the low end and race across the top.
const ANIMATION: Duration = Duration::from_millis(900);

/// Padding applied either side of a detected band, in octaves.
///
/// Zooming to exactly the measured band puts the signal hard against both edges,
/// and a stroke that sweeps even slightly outside it leaves the view at the exact
/// moment it is worth watching.
const PAD_OCTAVES: f32 = 0.5;

/// Narrowest band the view will ever show, in octaves.
///
/// Without a floor a narrow tone demands an absurd magnification: the Morse band
/// is 60–200 Hz, and a detection a few hertz wide inside it would ask for a
/// fortyfold zoom that shows one stripe and no context.
const MIN_SPAN_OCTAVES: f32 = 1.0;

/// A frequency range being displayed, in Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub low_hz: f32,
    pub high_hz: f32,
}

impl Band {
    pub fn new(low_hz: f32, high_hz: f32) -> Self {
        Self { low_hz, high_hz }
    }

    /// Interpolate in log frequency, which is how the axis is drawn.
    fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: f32, b: f32| {
            let (a, b) = (a.max(1.0), b.max(1.0));
            (a.ln() + (b.ln() - a.ln()) * t).exp()
        };
        Self {
            low_hz: mix(self.low_hz, other.low_hz),
            high_hz: mix(self.high_hz, other.high_hz),
        }
    }

    /// Pad, widen to the minimum span, and clamp back inside `bounds`.
    fn presented(self, bounds: Band) -> Self {
        let low = self.low_hz.max(1.0);
        let high = self.high_hz.max(low * 1.01);
        let (mut lo, mut hi) = (low.ln(), high.ln());

        let pad = PAD_OCTAVES * std::f32::consts::LN_2;
        lo -= pad;
        hi += pad;

        let floor = MIN_SPAN_OCTAVES * std::f32::consts::LN_2;
        let short = floor - (hi - lo);
        if short > 0.0 {
            lo -= short / 2.0;
            hi += short / 2.0;
        }

        // Sliding rather than clipping: a band that would overhang an edge stays
        // its requested width and moves inward, so the zoom factor a detection
        // near the floor gets is the same one it would get in the middle.
        let (blo, bhi) = (bounds.low_hz.max(1.0).ln(), bounds.high_hz.max(2.0).ln());
        let width = (hi - lo).min(bhi - blo);
        if lo < blo {
            lo = blo;
            hi = blo + width;
        }
        if hi > bhi {
            hi = bhi;
            lo = bhi - width;
        }
        Self {
            low_hz: lo.exp(),
            high_hz: hi.exp(),
        }
    }
}

/// Drives the overlay's displayed band.
#[derive(Debug)]
pub struct ZoomState {
    /// The band the overlay spans when nothing is detected.
    bounds: Band,
    /// Where the view is settled, and where it is heading.
    from: Band,
    to: Band,
    /// When the current animation began, if one is running.
    started: Option<Instant>,
    /// When the last animation finished, for the lockout.
    settled_at: Instant,
    /// The most recent band anything was detected in, and when it was last live.
    last_band: Option<Band>,
    last_seen: Instant,
    hold: Duration,
    lockout: Duration,
}

impl ZoomState {
    pub fn new(bounds: Band, hold_seconds: f32, lockout_seconds: f32, now: Instant) -> Self {
        Self {
            bounds,
            from: bounds,
            to: bounds,
            started: None,
            // Settled far enough in the past that the first detection is free to
            // move immediately rather than waiting out a lockout it never had.
            settled_at: now - Duration::from_secs(3600),
            last_band: None,
            last_seen: now - Duration::from_secs(3600),
            hold: Duration::from_secs_f32(hold_seconds.max(0.0)),
            lockout: Duration::from_secs_f32(lockout_seconds.max(0.0)),
        }
    }

    /// The full band, restored if the configuration changes under us.
    pub fn set_bounds(&mut self, bounds: Band) {
        if bounds != self.bounds {
            self.bounds = bounds;
            self.from = bounds;
            self.to = bounds;
            self.started = None;
            self.last_band = None;
        }
    }

    /// Feed the currently detected band, if anything is detected right now.
    pub fn observe(&mut self, active: Option<Band>, now: Instant) {
        if let Some(band) = active {
            self.last_band = Some(match self.last_band {
                // While a detection is live the remembered band only grows, so a
                // signal that drifts across frequency does not drag the view
                // after it once the lockout lifts.
                Some(prev) if now.duration_since(self.last_seen) <= self.hold => {
                    Band::new(prev.low_hz.min(band.low_hz), prev.high_hz.max(band.high_hz))
                }
                _ => band,
            });
            self.last_seen = now;
        }
    }

    /// The band to render this frame.
    pub fn band(&mut self, now: Instant) -> Band {
        // Finish any animation in flight before considering another move.
        if let Some(started) = self.started {
            let elapsed = now.saturating_duration_since(started);
            if elapsed >= ANIMATION {
                self.started = None;
                self.from = self.to;
                self.settled_at = now;
            } else {
                let t = elapsed.as_secs_f32() / ANIMATION.as_secs_f32();
                return self.from.lerp(self.to, ease(t));
            }
        }

        let wanted = self.wanted(now);
        if wanted != self.to && now.saturating_duration_since(self.settled_at) >= self.lockout {
            self.from = self.to;
            self.to = wanted;
            self.started = Some(now);
            // The frame that *starts* a move still draws where the view was.
            // Returning the destination here would snap to it and then animate
            // from the wrong end, which is the one thing this is meant to avoid.
            return self.from;
        }
        self.to
    }

    /// True while an animation is running, so the UI knows to keep repainting.
    pub fn animating(&self) -> bool {
        self.started.is_some()
    }

    /// Where the view should be, ignoring whether it is allowed to go there.
    fn wanted(&self, now: Instant) -> Band {
        match self.last_band {
            Some(band) if now.saturating_duration_since(self.last_seen) <= self.hold => {
                band.presented(self.bounds)
            }
            _ => self.bounds,
        }
    }
}

/// Smoothstep. A linear zoom starts and stops abruptly, which reads as a glitch
/// rather than as movement.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Band = Band {
        low_hz: 200.0,
        high_hz: 2400.0,
    };

    fn state(now: Instant) -> ZoomState {
        ZoomState::new(FULL, 15.0, 30.0, now)
    }

    /// Drive the state as the UI does — every frame — from `t0` to `t0 + secs`,
    /// optionally reporting a detection each frame. Returns the last band shown.
    fn run(
        z: &mut ZoomState,
        t0: Instant,
        secs: f32,
        mut active: impl FnMut(f32) -> Option<Band>,
    ) -> Band {
        let mut last = z.band(t0);
        let frames = (secs * 20.0) as u64;
        for frame in 1..=frames {
            let now = t0 + Duration::from_millis(frame * 50);
            if let Some(band) = active(frame as f32 / 20.0) {
                z.observe(Some(band), now);
            }
            last = z.band(now);
        }
        last
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() <= b.abs() * 0.02 + 0.5
    }

    /// Nothing detected, nothing moves.
    #[test]
    fn an_idle_overlay_shows_the_whole_band() {
        let t0 = Instant::now();
        let mut z = state(t0);
        for s in [0u64, 1, 10, 100, 1000] {
            let b = z.band(t0 + Duration::from_secs(s));
            assert_eq!(b, FULL, "at {s} s");
        }
    }

    #[test]
    fn a_detection_zooms_to_its_band_with_padding() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(400.0, 800.0)), t0);
        // Half an octave either side of 400..800.
        let settled = run(&mut z, t0, 2.0, |_| None);
        assert!(
            near(settled.low_hz, 400.0 / 2f32.sqrt()) && near(settled.high_hz, 800.0 * 2f32.sqrt()),
            "expected about 283..1131 Hz, got {settled:?}"
        );
    }

    #[test]
    fn the_animation_moves_smoothly_and_monotonically() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(400.0, 800.0)), t0);
        let mut last = z.band(t0);
        assert_eq!(last, FULL, "the first frame is still where it was");
        for step in 1..=10 {
            let now = t0 + ANIMATION * step / 10 + Duration::from_millis(1);
            let b = z.band(now);
            assert!(
                b.low_hz >= last.low_hz - 0.01 && b.high_hz <= last.high_hz + 0.01,
                "the view must close in steadily: {last:?} then {b:?}"
            );
            last = b;
        }
    }

    /// The rule the whole design turns on.
    #[test]
    fn a_second_detection_cannot_move_the_view_during_the_lockout() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(400.0, 800.0)), t0);
        let zoomed = run(&mut z, t0, 2.0, |_| None);

        // Ambience firing constantly, somewhere else entirely.
        let mut z2 = state(t0);
        z2.observe(Some(Band::new(400.0, 800.0)), t0);
        let held = run(&mut z2, t0, 25.0, |t| {
            (t > 3.0).then_some(Band::new(1800.0, 2000.0))
        });
        assert_eq!(held, zoomed, "the view moved mid-lockout");
    }

    /// Measured on field recordings: ambience produces a detection about every
    /// twenty seconds. Without the lockout the overlay would animate constantly.
    #[test]
    fn constant_ambience_produces_at_most_one_move_per_lockout() {
        let t0 = Instant::now();
        let mut z = state(t0);
        let mut moves = 0;
        let mut previous = z.band(t0);
        for tick in 0..(300 * 10) {
            let now = t0 + Duration::from_millis(tick * 100);
            // A fresh detection somewhere different every 20 seconds.
            if tick % 200 == 0 {
                let low = 300.0 + (tick % 1000) as f32;
                z.observe(Some(Band::new(low, low + 150.0)), now);
            }
            let b = z.band(now);
            if !z.animating() && b != previous {
                moves += 1;
            }
            previous = b;
        }
        // Five minutes, a 30 s lockout: ten moves is the ceiling.
        assert!(
            moves <= 10,
            "{moves} moves in five minutes; the lockout is not holding"
        );
    }

    #[test]
    fn the_view_returns_after_the_hold_and_the_lockout_have_both_passed() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(400.0, 800.0)), t0);
        let zoomed = run(&mut z, t0, 2.0, |_| None);
        assert_ne!(zoomed, FULL);

        // Hold expires at 15 s, but the lockout runs to 30 s from the settle.
        let at_20 = run(&mut z, t0, 20.0, |_| None);
        assert_eq!(at_20, zoomed, "the lockout outlasts the hold, by design");

        let settled = run(&mut z, t0, 40.0, |_| None);
        assert_eq!(settled, FULL, "and then it comes back out");
    }

    /// A detection still live when the lockout lifts must not send the view home.
    #[test]
    fn an_ongoing_detection_keeps_the_view_where_it_is() {
        let t0 = Instant::now();
        let mut z = state(t0);
        let b = run(&mut z, t0, 60.0, |_| Some(Band::new(400.0, 800.0)));
        assert_ne!(
            b, FULL,
            "it is still detecting; the view should have stayed"
        );
    }

    #[test]
    fn a_narrow_detection_is_widened_to_something_readable() {
        let t0 = Instant::now();
        let mut z = state(t0);
        // A tone a few hertz wide, as Thargoid Sensor Morse is.
        z.observe(Some(Band::new(110.0, 113.0)), t0);
        let b = run(&mut z, t0, 2.0, |_| None);
        let octaves = (b.high_hz / b.low_hz).log2();
        assert!(
            octaves >= MIN_SPAN_OCTAVES - 0.01,
            "spans only {octaves:.2} octaves: {b:?}"
        );
    }

    #[test]
    fn a_band_at_the_edge_keeps_its_width_instead_of_being_clipped() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(200.0, 260.0)), t0);
        let b = run(&mut z, t0, 2.0, |_| None);
        assert!(
            b.low_hz >= FULL.low_hz - 0.01 && b.high_hz <= FULL.high_hz + 0.01,
            "must stay inside the bounds: {b:?}"
        );
        let octaves = (b.high_hz / b.low_hz).log2();
        assert!(
            octaves >= MIN_SPAN_OCTAVES - 0.01,
            "a band against the floor still gets its full width, got {octaves:.2}"
        );
    }

    /// A signal sweeping across frequency should not drag the view behind it.
    #[test]
    fn a_drifting_signal_widens_the_remembered_band_rather_than_chasing_it() {
        let t0 = Instant::now();
        let mut z = state(t0);
        // Swept slowly and continuously. The view cannot follow it — the lockout
        // forbids that, and a view chasing a sweep is exactly what looks bad —
        // so what is being checked is that when it *is* finally allowed to move,
        // it covers everywhere the signal has been rather than only where it is.
        let b = run(&mut z, t0, 40.0, |t| {
            let low = 400.0 + t * 12.5;
            Some(Band::new(low, low + 100.0))
        });
        assert!(
            b.low_hz <= 400.0 && b.high_hz >= 900.0,
            "should cover the whole sweep, got {b:?}"
        );
    }

    #[test]
    fn changing_the_configured_bounds_resets_the_view() {
        let t0 = Instant::now();
        let mut z = state(t0);
        z.observe(Some(Band::new(400.0, 800.0)), t0);
        assert_ne!(run(&mut z, t0, 2.0, |_| None), FULL);

        let wider = Band::new(20.0, 20_000.0);
        z.set_bounds(wider);
        assert_eq!(z.band(t0 + Duration::from_secs(3)), wider);
    }

    #[test]
    fn degenerate_bands_do_not_produce_nonsense() {
        let t0 = Instant::now();
        let mut z = state(t0);
        for band in [
            Band::new(0.0, 0.0),
            Band::new(-100.0, 50.0),
            Band::new(5000.0, 10.0),
            Band::new(f32::NAN, f32::NAN),
        ] {
            z.observe(Some(band), t0);
            let b = run(&mut z, t0, 2.0, |_| None);
            assert!(
                b.low_hz.is_finite() && b.high_hz.is_finite(),
                "{band:?} produced {b:?}"
            );
        }
    }
}

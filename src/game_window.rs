//! Locating the Elite Dangerous window, so the overlay can sit on top of it.
//!
//! This is the same technique every Elite overlay uses — SrvSurvey, EDMCOverlay
//! and the rest: find the game's window, read its rectangle, and place a
//! borderless always-on-top window over it. Nothing is injected into the game,
//! no graphics API is hooked, and the game process is never opened or written
//! to. We only ask the window manager where a window is.
//!
//! The unavoidable consequence is that **the game must run in borderless or
//! windowed mode**. An exclusive-fullscreen application owns the display outright
//! and nothing can be drawn above it — that is a property of the technique, not
//! of this implementation.

/// A screen rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn is_usable(&self) -> bool {
        self.width() > 200 && self.height() > 200
    }
}

/// Where the overlay sits within the game window.
///
/// Fractions rather than pixels, so the placement survives a resolution change
/// or moving to a different monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayAnchor {
    /// Horizontal centre of the overlay as a fraction of the window width.
    ///
    /// Clamped so the overlay always stays inside the window; a fraction of 0
    /// pins it against the left edge, from which [`Self::x_offset_px`] then
    /// nudges it clear of Elite's own icons.
    pub x_fraction: f32,
    /// Top edge as a fraction of the window height.
    pub y_fraction: f32,
    /// Pixels added rightward after the fractional position is clamped.
    ///
    /// Elite draws its info icons and alert messages in the extreme top-left,
    /// so "flush in the corner" sat on top of them. A pixel offset rather than
    /// a larger fraction because the icons hug the corner at every resolution —
    /// the clearance needed is absolute, not proportional.
    pub x_offset_px: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for OverlayAnchor {
    fn default() -> Self {
        Self {
            x_fraction: 0.0,
            y_fraction: 0.0,
            // A quarter of the overlay's own width: enough to clear Elite's
            // top-left info icons, decided by eye in the cockpit.
            x_offset_px: 220.0,
            width: 880.0,
            height: 104.0,
        }
    }
}

impl OverlayAnchor {
    /// Top-left position for the overlay window over the given game rectangle.
    pub fn position_in(&self, game: Rect) -> (f32, f32) {
        let centre_x = game.left as f32 + game.width() as f32 * self.x_fraction;
        let x = centre_x - self.width / 2.0;
        let y = game.top as f32 + game.height() as f32 * self.y_fraction;

        // Never let the overlay slide off the window it belongs to. The pixel
        // offset is applied after the first clamp — offsetting the unclamped
        // value would let a left-anchored overlay swallow its own shift — and
        // then clamped again so the offset cannot push it off the right edge.
        let max_x = (game.right as f32 - self.width).max(game.left as f32);
        let max_y = (game.bottom as f32 - self.height).max(game.top as f32);
        let x = x.clamp(game.left as f32, max_x);
        (
            (x + self.x_offset_px).clamp(game.left as f32, max_x),
            y.clamp(game.top as f32, max_y),
        )
    }
}

/// Where the overlay waits when it should not be seen.
///
/// Invisibility by geometry, deliberately. Three cleverer mechanisms each
/// failed in their own way: destroying the window crashed the renderer (egui's
/// `Painter::set_window(id, None)` clears *every* surface), hiding it with
/// `with_visible(false)` froze it for good (a hidden window gets no redraws),
/// and painting nothing depended on the window being transparent — which
/// silently stopped being true when the rendering backend changed.
///
/// A window parked here is off every real desktop. Windows accepts positions
/// well outside the virtual screen, and 32000 is inside the 16-bit range the
/// window manager works in while being far beyond any monitor arrangement.
/// Nothing about this can be broken by a driver, a compositor or a backend.
pub const PARKED_POSITION: (f32, f32) = (32_000.0, 32_000.0);

/// The horizontal band that SrvSurvey's top-edge plotters leave free.
///
/// Numbers taken from SrvSurvey's own source rather than measured off a
/// screenshot. Its `plotters.json` anchors overlays as
/// `"<left|center|right>:<±px>, <top|middle|bottom>:<±px>"`, and the plotters
/// that sit along the top are:
///
/// * `left:8` — PlotBodyInfo (320 wide), PlotFSSInfo (300), PlotGalMap (240).
///   The widest reaches `8 + 320 = 328`.
/// * `center:0` — PlotJumpInfo (600 wide), PlotGuardianStatus (500),
///   PlotBioStatus (480), PlotFSS (420). The widest spans
///   `centre ± 300`.
///
/// So the free band runs from 328 to `centre - 300`, less a margin at each end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotterGap {
    /// Right edge of the widest top-left plotter.
    pub left_edge: f32,
    /// Width of the widest centred top plotter.
    pub centre_width: f32,
    /// Clearance left on each side, so the panels do not touch.
    pub margin: f32,
    /// Below this the band is not worth using and the configured width wins.
    pub min_width: f32,
    /// Grow the band by this fraction of its own width at each end.
    ///
    /// Sizing against the *widest* plotter of each cluster is conservative:
    /// PlotJumpInfo at 600 only appears during a jump, and PlotBodyInfo at 320
    /// only near a body. Left strictly inside those bounds the overlay is
    /// narrower than it needs to be almost all the time, so it is allowed to
    /// reach a little into ground the widest plotters would claim.
    pub expand_each_side: f32,
}

impl Default for PlotterGap {
    fn default() -> Self {
        Self {
            left_edge: 328.0,
            centre_width: 600.0,
            margin: 8.0,
            min_width: 260.0,
            expand_each_side: 0.10,
        }
    }
}

impl PlotterGap {
    /// Where the free band starts and how wide it is, for a game window of this
    /// width. `None` when the band is too narrow to be worth having.
    pub fn band(&self, game_width: f32) -> Option<(f32, f32)> {
        let left = self.left_edge + self.margin;
        let right = game_width / 2.0 - self.centre_width / 2.0 - self.margin;
        let width = right - left;
        if width < self.min_width {
            return None;
        }
        // Grow outward from the strict band, never past the window edge.
        let grow = width * self.expand_each_side;
        let left = (left - grow).max(0.0);
        let width = (width + 2.0 * grow).min(game_width - left);
        Some((left, width))
    }
}

/// The game's window, and whether the player is actually looking at it.
///
/// Focus matters as much as position: an overlay that stays up while you are in
/// a browser is a window in the way, not an overlay. SrvSurvey ties its panels
/// to the game having focus, and so do we.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameWindow {
    pub rect: Rect,
    /// True when the game is the foreground window.
    pub focused: bool,
}

/// The overlay window's title, used to find it again for the Win32 calls that
/// egui does not expose.
pub const OVERLAY_WINDOW_TITLE: &str = "ED Compass overlay";

/// Window titles Elite Dangerous is known to use, in order of preference.
pub const GAME_WINDOW_TITLES: [&str; 2] = ["Elite - Dangerous (CLIENT)", "Elite - Dangerous"];

#[cfg(windows)]
mod imp {
    use super::Rect;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetForegroundWindow, GetWindowRect, IsIconic, IsWindowVisible,
    };
    use windows::core::PCWSTR;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    use super::GameWindow;

    fn rect_of(hwnd: HWND) -> Option<Rect> {
        let mut r = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
        Some(Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
    }

    /// Set the overlay's whole-window opacity, 0 hidden and 255 solid.
    ///
    /// This is what SrvSurvey does — its plotters set `Form.Opacity = 0` rather
    /// than hiding or closing — and underneath, `Form.Opacity` is exactly this
    /// call. The window stays open, keeps its position and keeps rendering; it
    /// simply composites to nothing.
    ///
    /// Two properties make it safe here. The overlay already carries
    /// `WS_EX_LAYERED`, because winit sets it alongside `WS_EX_TRANSPARENT` for
    /// a click-through window, so no style change is needed. And winit
    /// implements transparency with `DwmEnableBlurBehindWindow` rather than
    /// `UpdateLayeredWindow`, so a constant alpha composes with the per-pixel
    /// alpha instead of replacing it.
    ///
    /// Returns false if the window is not there yet or the call failed, so the
    /// caller can fall back to something that cannot fail.
    pub fn set_overlay_opacity(title: &str, alpha: u8) -> bool {
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::UI::WindowsAndMessaging::{LWA_ALPHA, SetLayeredWindowAttributes};

        let name = wide(title);
        // SAFETY: a null class with a NUL-terminated title; the handle is only
        // compared and passed back to Win32, never dereferenced.
        let Ok(hwnd) = (unsafe { FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) }) else {
            return false;
        };
        if hwnd.is_invalid() {
            return false;
        }
        // SAFETY: `hwnd` is our own window, and it already has WS_EX_LAYERED.
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) }.is_ok()
    }

    /// Find the game window by title, if it is running and visible.
    pub fn find_game_window(titles: &[&str]) -> Option<GameWindow> {
        // SAFETY: returns a borrowed handle or null; never dereferenced here.
        let foreground = unsafe { GetForegroundWindow() };
        for title in titles {
            let name = wide(title);
            // A missing window is the ordinary case — the game is not running —
            // so a failure here must try the next title, not abandon the search.
            let Ok(hwnd) = (unsafe { FindWindowW(PCWSTR::null(), PCWSTR(name.as_ptr())) }) else {
                continue;
            };
            if hwnd.is_invalid() {
                continue;
            }
            if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
                continue;
            }
            // A minimized game is not a game window. Windows still reports a
            // rectangle for it — often a stale or off-screen one — so without
            // this the overlay would appear over the desktop the moment our own
            // window took focus. SrvSurvey makes the same check and returns an
            // empty rectangle for a minimized Elite.
            if unsafe { IsIconic(hwnd) }.as_bool() {
                continue;
            }
            if let Some(rect) = rect_of(hwnd)
                && rect.is_usable()
            {
                return Some(GameWindow {
                    rect,
                    focused: hwnd == foreground,
                });
            }
        }
        None
    }
}

#[cfg(not(windows))]
mod imp {
    use super::GameWindow;

    /// No layered windows off Windows; the caller falls back to moving it.
    pub fn set_overlay_opacity(_title: &str, _alpha: u8) -> bool {
        false
    }

    /// The game only exists on Windows as far as this tool is concerned.
    pub fn find_game_window(_titles: &[&str]) -> Option<GameWindow> {
        None
    }
}

pub use imp::{find_game_window, set_overlay_opacity};

/// What the overlay needs to know each time it is considered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayPlacement {
    /// Top-left corner in screen pixels.
    pub position: (f32, f32),
    /// Width of the game window, so the overlay can be fitted to it.
    pub game_width: f32,
    /// Whether the game window was found at all.
    pub game_found: bool,
    /// Whether the game currently has focus.
    pub game_focused: bool,
}

/// Where the overlay should be, given the game window if it can be found.
///
/// Falls back to the top-left area of a nominal 1920×1080 desktop when the game
/// is not running, so a preview can still be positioned rather than vanishing.
pub fn overlay_placement(anchor: OverlayAnchor) -> OverlayPlacement {
    match find_game_window(&GAME_WINDOW_TITLES) {
        Some(game) => OverlayPlacement {
            position: anchor.position_in(game.rect),
            game_width: game.rect.width() as f32,
            game_found: true,
            game_focused: game.focused,
        },
        None => {
            let fallback = Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            };
            OverlayPlacement {
                position: anchor.position_in(fallback),
                game_width: fallback.width() as f32,
                game_found: false,
                game_focused: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game() -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    }

    #[test]
    fn rect_geometry() {
        let r = game();
        assert_eq!(r.width(), 1920);
        assert_eq!(r.height(), 1080);
        assert!(r.is_usable());
        assert!(
            !Rect {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10
            }
            .is_usable()
        );
    }

    #[test]
    fn the_default_clears_the_top_left_icons() {
        // A quarter of the overlay's own width in from the left edge: right of
        // Elite's info icons, still far left of centre.
        let a = OverlayAnchor::default();
        let (x, y) = a.position_in(game());
        assert_eq!((x, y), (220.0, 0.0));

        // Measured from the window's corner, not the desktop's.
        let moved = Rect {
            left: 640,
            top: 200,
            right: 2560,
            bottom: 1280,
        };
        assert_eq!(a.position_in(moved), (860.0, 200.0));
    }

    #[test]
    fn the_parked_position_is_off_every_plausible_desktop() {
        let (x, y) = PARKED_POSITION;

        // Beyond any real monitor arrangement, including a wide multi-head
        // desktop, so no part of the overlay can be on screen while parked.
        assert!(x >= 16_000.0, "parked x {x} could land on a wide desktop");
        assert!(y >= 16_000.0, "parked y {y} could land on a tall desktop");

        // Positive rather than negative: a negative position would land on a
        // monitor arranged to the left of the primary one, which is a common
        // setup and would put the overlay right back on screen.
        assert!(
            x > 0.0 && y > 0.0,
            "negative coordinates hit left/upper monitors"
        );

        // Inside the range the window manager works in.
        assert!(
            x < 32_768.0 && y < 32_768.0,
            "outside the 16-bit window space"
        );
    }

    #[test]
    fn the_free_band_matches_srvsurvey_s_own_layout() {
        let gap = PlotterGap::default();

        // 2560x1440: the strict band runs 336..972, 636 wide. It is then grown
        // by a tenth of its own width at each end, because the plotters it
        // avoids are the widest possible rather than the ones usually on screen.
        let (x, w) = gap.band(2560.0).expect("a 2560-wide window has room");
        assert_eq!(x, 336.0 - 63.6);
        assert!((w - 763.2).abs() < 0.01, "width {w}");

        // Strictly inside the band with no expansion.
        let strict = PlotterGap {
            expand_each_side: 0.0,
            ..PlotterGap::default()
        };
        let (x, w) = strict.band(2560.0).unwrap();
        assert_eq!((x, w), (336.0, 636.0));
        assert!(x + w <= 980.0, "must not reach PlotJumpInfo at 980");
        assert!(x >= 328.0, "must not reach PlotBodyInfo ending at 328");

        // 1920 is tighter but still usable, and never runs off the left edge.
        let (x, w) = gap.band(1920.0).expect("1920 still has room");
        assert!(x >= 0.0 && x + w <= 1920.0, "x={x} w={w}");

        // A small window has no useful band at all.
        assert_eq!(gap.band(1280.0), None);
    }

    #[test]
    fn the_offset_cannot_push_the_overlay_off_the_right_edge() {
        let a = OverlayAnchor {
            x_offset_px: 10_000.0,
            ..Default::default()
        };
        let (x, _) = a.position_in(game());
        assert!(
            (x - (1920.0 - a.width)).abs() < 0.01,
            "clamped to the right edge, got {x}"
        );
    }

    #[test]
    fn placement_follows_a_moved_window() {
        let a = OverlayAnchor::default();
        let moved = Rect {
            left: 100,
            top: 50,
            right: 2020,
            bottom: 1130,
        };
        let (x, y) = a.position_in(moved);
        let (bx, by) = a.position_in(game());
        assert!((x - (bx + 100.0)).abs() < 0.01);
        assert!((y - (by + 50.0)).abs() < 0.01);
    }

    #[test]
    fn placement_scales_with_resolution() {
        let a = OverlayAnchor {
            x_fraction: 0.375,
            x_offset_px: 0.0,
            ..Default::default()
        };
        let small = Rect {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let (x, _) = a.position_in(small);
        let centre = x + a.width / 2.0;
        assert!(
            (centre - 480.0).abs() < 1.0,
            "centre at {centre}, expected 480"
        );
    }

    #[test]
    fn the_overlay_never_leaves_the_game_window() {
        let tiny = Rect {
            left: 0,
            top: 0,
            right: 260,
            bottom: 240,
        };
        let a = OverlayAnchor::default();
        let (x, y) = a.position_in(tiny);
        assert!(x >= 0.0 && y >= 0.0, "({x}, {y})");
        assert!(x <= tiny.right as f32, "overlay ran off the right edge");

        // An extreme anchor is clamped rather than flung off screen.
        let far = OverlayAnchor {
            x_fraction: 5.0,
            ..OverlayAnchor::default()
        };
        let (x, _) = far.position_in(game());
        assert!(x <= 1920.0 - far.width + 0.01, "x={x}");
    }

    #[test]
    fn a_custom_anchor_is_honoured() {
        let right = OverlayAnchor {
            x_fraction: 0.75,
            y_fraction: 0.5,
            x_offset_px: 0.0,
            ..Default::default()
        };
        let (x, y) = right.position_in(game());
        assert!((x + right.width / 2.0 - 1440.0).abs() < 1.0);
        assert!((y - 540.0).abs() < 1.0);
    }

    #[test]
    fn there_is_always_a_position_even_with_no_game_running() {
        let p = overlay_placement(OverlayAnchor::default());
        assert!(p.position.0.is_finite() && p.position.1.is_finite());
        // On a machine without Elite running this is the fallback path.
        if !p.game_found {
            assert!(p.position.0 >= 0.0 && p.position.1 >= 0.0);
            assert!(
                !p.game_focused,
                "a window that is not there cannot be focused"
            );
        }
    }
}

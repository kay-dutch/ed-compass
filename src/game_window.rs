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
    /// Clamped so the overlay always stays inside the window, which means a
    /// fraction of 0 pins it flush against the left edge. That is the default:
    /// the top-left corner is the one large area Elite's own HUD leaves empty,
    /// so the overlay costs no cockpit visibility there.
    pub x_fraction: f32,
    /// Top edge as a fraction of the window height.
    pub y_fraction: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for OverlayAnchor {
    fn default() -> Self {
        Self {
            x_fraction: 0.0,
            y_fraction: 0.0,
            width: 440.0,
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

        // Never let the overlay slide off the window it belongs to.
        let max_x = (game.right as f32 - self.width).max(game.left as f32);
        let max_y = (game.bottom as f32 - self.height).max(game.top as f32);
        (
            x.clamp(game.left as f32, max_x),
            y.clamp(game.top as f32, max_y),
        )
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

/// Window titles Elite Dangerous is known to use, in order of preference.
pub const GAME_WINDOW_TITLES: [&str; 2] = ["Elite - Dangerous (CLIENT)", "Elite - Dangerous"];

#[cfg(windows)]
mod imp {
    use super::Rect;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetForegroundWindow, GetWindowRect, IsWindowVisible,
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

    /// The game only exists on Windows as far as this tool is concerned.
    pub fn find_game_window(_titles: &[&str]) -> Option<GameWindow> {
        None
    }
}

pub use imp::find_game_window;

/// What the overlay needs to know each time it is considered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayPlacement {
    /// Top-left corner in screen pixels.
    pub position: (f32, f32),
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
    fn the_default_touches_the_top_left_corner() {
        let a = OverlayAnchor::default();
        let (x, y) = a.position_in(game());
        assert_eq!((x, y), (0.0, 0.0), "it must sit flush in the corner");

        // And in the corner of the window, not of the desktop.
        let moved = Rect {
            left: 640,
            top: 200,
            right: 2560,
            bottom: 1280,
        };
        assert_eq!(a.position_in(moved), (640.0, 200.0));
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

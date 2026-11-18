// SPDX-License-Identifier: MIT
//! Display selection and repositioning rules for the settings window.
//!
//! Coordinates are global points with the origin at the primary display's
//! bottom-left corner and `y` growing upward, matching AppKit. The rules are
//! pure so multi-display behaviour is testable without a window server.

/// Width and height that must stay inside the usable area of a display for a
/// window to count as reachable there.
const REACHABLE: f64 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    fn max_x(&self) -> f64 {
        self.x + self.width
    }
    fn max_y(&self) -> f64 {
        self.y + self.height
    }
    /// Squared distance from a point to this rectangle; zero inside it.
    fn distance_squared(&self, (x, y): (f64, f64)) -> f64 {
        let dx = (self.x - x).max(x - self.max_x()).max(0.0);
        let dy = (self.y - y).max(y - self.max_y()).max(0.0);
        dx * dx + dy * dy
    }
    /// Width and height shared with another rectangle.
    fn overlap(&self, other: &Rect) -> (f64, f64) {
        (
            (self.max_x().min(other.max_x()) - self.x.max(other.x)).max(0.0),
            (self.max_y().min(other.max_y()) - self.y.max(other.y)).max(0.0),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    /// The whole display.
    pub frame: Rect,
    /// The display without the menu bar and Dock.
    pub visible: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowState {
    pub frame: Rect,
    pub miniaturized: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Restore the window from the Dock before ordering it front.
    pub restore: bool,
    /// New bottom-left origin, or `None` to keep the user's position.
    pub origin: Option<(f64, f64)>,
}

/// The display the user is working on: the one under the pointer. The pointer
/// can be reported on a display's outer edge, so the nearest display wins.
pub fn screen_at(screens: &[Screen], pointer: (f64, f64)) -> Option<usize> {
    screens
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            a.frame
                .distance_squared(pointer)
                .total_cmp(&b.frame.distance_squared(pointer))
        })
        .map(|(index, _)| index)
}

/// Decides how an explicit open request presents an existing window.
///
/// A window that is already reachable on the target display keeps the position
/// the user chose. A window on another display, or left outside every display
/// after a display was detached, moves to the target display. A full-screen
/// window owns its Space and is never moved.
pub fn plan(window: WindowState, screens: &[Screen], target: Option<usize>) -> Plan {
    let origin = if window.fullscreen {
        None
    } else {
        target.and_then(|target| relocation(window.frame, screens, target))
    };
    Plan {
        restore: window.miniaturized,
        origin,
    }
}

fn relocation(window: Rect, screens: &[Screen], target: usize) -> Option<(f64, f64)> {
    let visible = screens.get(target)?.visible;
    let host = screens
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let (width, height) = window.overlap(&screen.frame);
            (index, width * height)
        })
        .filter(|(_, area)| *area > 0.0)
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(index, _)| index);
    let (width, height) = window.overlap(&visible);
    if host == Some(target)
        && width >= window.width.min(REACHABLE)
        && height >= window.height.min(REACHABLE)
        && window.max_y() <= visible.max_y()
    {
        return None;
    }
    // Centre the window. One that exceeds the usable area is pinned to the
    // top-left corner so its title bar stays reachable.
    let x = visible.x + ((visible.width - window.width) / 2.0).max(0.0);
    let top = visible.max_y() - ((visible.height - window.height) / 2.0).max(0.0);
    Some((x.floor(), (top - window.height).floor()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: (f64, f64) = (680.0, 700.0);

    /// A primary display, a larger display to its left and one above it.
    fn three_displays() -> [Screen; 3] {
        [
            Screen {
                frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                visible: Rect::new(0.0, 70.0, 1920.0, 985.0),
            },
            Screen {
                frame: Rect::new(-2560.0, 0.0, 2560.0, 1440.0),
                visible: Rect::new(-2560.0, 0.0, 2560.0, 1415.0),
            },
            Screen {
                frame: Rect::new(0.0, 1080.0, 1920.0, 1080.0),
                visible: Rect::new(0.0, 1080.0, 1920.0, 1055.0),
            },
        ]
    }
    fn window_at(x: f64, y: f64) -> WindowState {
        WindowState {
            frame: Rect::new(x, y, WINDOW.0, WINDOW.1),
            miniaturized: false,
            fullscreen: false,
        }
    }

    #[test]
    fn pointer_selects_a_secondary_display_with_negative_coordinates() {
        let screens = three_displays();
        assert_eq!(screen_at(&screens, (960.0, 500.0)), Some(0));
        assert_eq!(screen_at(&screens, (-1200.0, 1300.0)), Some(1));
        assert_eq!(screen_at(&screens, (300.0, 1500.0)), Some(2));
    }

    #[test]
    fn pointer_on_an_outer_edge_selects_the_nearest_display() {
        let screens = three_displays();
        // The menu bar row of the upper display can be reported on its top edge.
        assert_eq!(screen_at(&screens, (300.0, 2160.0)), Some(2));
        assert_eq!(screen_at(&screens, (-2561.0, 20.0)), Some(1));
        assert_eq!(screen_at(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn window_on_another_display_moves_to_the_centre_of_the_pointer_display() {
        let screens = three_displays();
        let plan = plan(window_at(620.0, 200.0), &screens, Some(1));
        assert_eq!(
            plan,
            Plan {
                restore: false,
                origin: Some((-1620.0, 357.0)),
            }
        );
    }

    #[test]
    fn window_already_on_the_pointer_display_keeps_the_position_the_user_chose() {
        let screens = three_displays();
        assert_eq!(plan(window_at(40.0, 90.0), &screens, Some(0)).origin, None);
        // Mostly on the primary display, so that display hosts it.
        assert_eq!(
            plan(window_at(-100.0, 90.0), &screens, Some(0)).origin,
            None
        );
    }

    #[test]
    fn window_left_outside_every_display_returns_to_the_pointer_display() {
        let screens = three_displays();
        let detached = window_at(5000.0, 300.0);
        assert_eq!(
            plan(detached, &screens, Some(0)).origin,
            Some((620.0, 212.0))
        );
        // Only a sliver remains on the target display: not reachable.
        let sliver = window_at(1900.0, 200.0);
        assert_eq!(plan(sliver, &screens, Some(0)).origin, Some((620.0, 212.0)));
    }

    #[test]
    fn window_with_its_title_bar_above_the_usable_area_is_relocated() {
        let screens = three_displays();
        // Most of the body is visible, but the title bar cannot be reached.
        let clipped_top = window_at(620.0, 500.0);
        assert_eq!(
            plan(clipped_top, &screens, Some(0)).origin,
            Some((620.0, 212.0))
        );
    }

    #[test]
    fn minimized_window_is_restored_and_fullscreen_window_is_not_moved() {
        let screens = three_displays();
        let minimized = WindowState {
            miniaturized: true,
            ..window_at(620.0, 200.0)
        };
        assert_eq!(
            plan(minimized, &screens, Some(1)),
            Plan {
                restore: true,
                origin: Some((-1620.0, 357.0)),
            }
        );
        let fullscreen = WindowState {
            frame: screens[0].frame,
            miniaturized: false,
            fullscreen: true,
        };
        assert_eq!(
            plan(fullscreen, &screens, Some(1)),
            Plan {
                restore: false,
                origin: None,
            }
        );
    }

    #[test]
    fn window_larger_than_the_usable_area_keeps_its_title_bar_reachable() {
        let small = [Screen {
            frame: Rect::new(0.0, 0.0, 640.0, 480.0),
            visible: Rect::new(0.0, 40.0, 640.0, 415.0),
        }];
        let origin = plan(window_at(3000.0, 3000.0), &small, Some(0)).origin;
        // Left edge and top edge align with the usable area.
        assert_eq!(origin, Some((0.0, 455.0 - WINDOW.1)));
    }

    #[test]
    fn unknown_target_display_keeps_the_window_in_place() {
        let screens = three_displays();
        assert_eq!(plan(window_at(620.0, 200.0), &screens, None).origin, None);
        assert_eq!(
            plan(window_at(620.0, 200.0), &screens, Some(9)).origin,
            None
        );
        assert_eq!(plan(window_at(620.0, 200.0), &[], Some(0)).origin, None);
    }
}

//! Turns scroll gestures into whole wheel steps for the Mac. The Mac
//! scrolls exactly one point per step, and a step is the only scroll input
//! it takes, so a fraction of a point waits until the next delta completes
//! a step.

use winit::event::{MouseScrollDelta, TouchPhase};

/// A mouse-wheel notch becomes this many points.
const POINTS_PER_LINE: f64 = 32.0;

/// A sum of fractions, such as ten times 0.1, can land just below a whole
/// step. A rest this close to a step counts as the step.
const EPSILON: f64 = 1e-6;

/// The points to scroll for a winit delta: positive scrolls right and down.
/// `scale` is the window's scale factor, so that the content on the Mac
/// follows the fingers 1:1.
pub fn points(delta: MouseScrollDelta, scale: f64) -> (f64, f64) {
    // winit's positive values move the content right and down, which
    // scrolls left and up.
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (-x as f64 * POINTS_PER_LINE, -y as f64 * POINTS_PER_LINE),
        MouseScrollDelta::PixelDelta(p) => (-p.x / scale, -p.y / scale),
    }
}

/// The part of the scroll that has not made a whole step yet.
#[derive(Default)]
pub struct Steps {
    rest: (f64, f64),
}

impl Steps {
    /// Adds `points` (positive scrolls right and down), and returns the whole
    /// steps now due on each axis. At the end of a gesture, the rest rounds
    /// to the nearest step, so nothing carries into the next gesture.
    pub fn add(&mut self, (dx, dy): (f64, f64), phase: TouchPhase) -> (i32, i32) {
        let end = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let take = |rest: &mut f64, d: f64| {
            *rest += d;
            let steps = if end { rest.round() } else { (*rest + EPSILON.copysign(*rest)).trunc() };
            *rest = if end { 0.0 } else { *rest - steps };
            steps as i32
        };
        (take(&mut self.rest.0, dx), take(&mut self.rest.1, dy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn ten_scroll_deltas_of_a_tenth_send_one_step() {
        let mut s = Steps::default();
        let sent: Vec<_> = (0..10).map(|_| s.add((0.0, 0.1), TouchPhase::Moved)).collect();
        assert_eq!(sent[..9], [(0, 0); 9]);
        assert_eq!(sent[9], (0, 1));
    }

    #[test]
    fn the_end_of_a_gesture_sends_a_rest_of_0_6_as_one_step_and_drops_0_4() {
        let mut s = Steps::default();
        assert_eq!(s.add((-2.6, 2.4), TouchPhase::Moved), (-2, 2));
        assert_eq!(s.add((0.0, 0.0), TouchPhase::Ended), (-1, 0));
        // Nothing carries into the next gesture.
        assert_eq!(s.add((0.0, 0.7), TouchPhase::Moved), (0, 0));
    }

    #[test]
    fn a_wheel_notch_towards_the_user_scrolls_32_steps_down() {
        // winit and Wayland report it as -1 line.
        assert_eq!(points(MouseScrollDelta::LineDelta(0.0, -1.0), 1.0), (0.0, 32.0));
    }

    #[test]
    fn two_fingers_left_by_30_pixels_at_scale_2_scroll_15_points_right() {
        let left = MouseScrollDelta::PixelDelta(PhysicalPosition::new(-30.0, 0.0));
        assert_eq!(points(left, 2.0), (15.0, 0.0));
    }
}

//! The local pointer in window pixels, its canvas pixel, and the edge it
//! leaves the window through.
//!
//! The compositor sends no position with a leave, and the last motion inside
//! the window can lie many pixels short of the edge after a fast flick. So
//! the leave carries the pointer on along its last step to the edge, and the
//! Mac pointer reaches the edge of its display: the Dock at the bottom, the
//! menu bar at the top.

use winit::dpi::{PhysicalPosition, PhysicalSize};

use super::render::Viewport;

/// A leave is an exit through an edge when the last step, taken this many
/// times, reaches it. A flick speeds up, so the step that left the window can
/// be longer than the last step inside it.
const EXIT_STEPS: f64 = 4.0;

/// The pointer position in window pixels, and the step that brought it there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    pub pos: PhysicalPosition<f64>,
    step: (f64, f64),
}

impl Track {
    /// The pointer appears at `pos`, with no step yet.
    pub fn at(pos: PhysicalPosition<f64>) -> Self {
        Self { pos, step: (0.0, 0.0) }
    }

    /// The pointer moved to `pos`.
    pub fn moved(self, pos: PhysicalPosition<f64>) -> Self {
        Self { pos, step: (pos.x - self.pos.x, pos.y - self.pos.y) }
    }

    /// The point on the edge of a window of `size` where the last step,
    /// carried on, leaves it. `None` when that takes more than [`EXIT_STEPS`]
    /// steps: then the pointer did not leave through an edge, for example
    /// when another window opened over it.
    pub fn exit(self, size: PhysicalSize<u32>) -> Option<PhysicalPosition<f64>> {
        let Track { pos, step: (dx, dy) } = self;
        // The steps to the edge that the motion on one axis heads for, and
        // that edge. A pointer that is already beyond it, in a drag, is
        // there at 0 steps.
        let to_edge = |pos: f64, step: f64, end: u32| match step {
            s if s > 0.0 => Some((((end as f64 - pos) / s).max(0.0), end as f64)),
            s if s < 0.0 => Some(((-pos / s).max(0.0), 0.0)),
            _ => None,
        };
        let x = to_edge(pos.x, dx, size.width).map(|(t, x)| (t, PhysicalPosition::new(x, pos.y + t * dy)));
        let y = to_edge(pos.y, dy, size.height).map(|(t, y)| (t, PhysicalPosition::new(pos.x + t * dx, y)));
        let (t, exit) = x.into_iter().chain(y).min_by(|a, b| a.0.total_cmp(&b.0))?;
        (t <= EXIT_STEPS).then_some(exit)
    }
}

/// The canvas pixel of an `image` drawn at `vp` under the window position
/// `pos`. A position outside the image lands on its nearest edge pixel.
pub fn canvas(pos: PhysicalPosition<f64>, vp: Viewport, (iw, ih): (u32, u32)) -> (u16, u16) {
    let x = ((pos.x as f32 - vp.x) / vp.w * iw as f32).clamp(0.0, iw.saturating_sub(1) as f32);
    let y = ((pos.y as f32 - vp.y) / vp.h * ih as f32).clamp(0.0, ih.saturating_sub(1) as f32);
    (x as u16, y as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: PhysicalSize<u32> = PhysicalSize::new(1000, 800);
    /// A 2000x1600 image that fills the 1000x800 window.
    const IMAGE: (u32, u32) = (2000, 1600);
    const FULL: Viewport = Viewport { x: 0.0, y: 0.0, w: 1000.0, h: 800.0 };

    fn track(from: (f64, f64), to: (f64, f64)) -> Track {
        Track::at(PhysicalPosition::new(from.0, from.1)).moved(PhysicalPosition::new(to.0, to.1))
    }

    fn canvas_at_exit(t: Track, vp: Viewport) -> Option<(u16, u16)> {
        t.exit(WINDOW).map(|p| canvas(p, vp, IMAGE))
    }

    #[test]
    fn a_leave_through_the_bottom_after_a_last_event_at_096_of_the_height_sends_y_height_minus_1() {
        // y = 768 is 0.96 of 800, which alone would send canvas y 1536.
        assert_eq!(canvas(PhysicalPosition::new(500.0, 768.0), FULL, IMAGE), (1000, 1536));
        assert_eq!(canvas_at_exit(track((500.0, 740.0), (500.0, 768.0)), FULL), Some((1000, 1599)));
    }

    #[test]
    fn a_leave_through_the_top_after_a_last_event_at_y_30_sends_y_0() {
        assert_eq!(canvas_at_exit(track((400.0, 70.0), (400.0, 30.0)), FULL), Some((800, 0)));
    }

    #[test]
    fn a_leave_through_the_left_after_a_last_event_at_x_20_sends_x_0() {
        assert_eq!(canvas_at_exit(track((60.0, 400.0), (20.0, 400.0)), FULL), Some((0, 800)));
    }

    #[test]
    fn a_leave_through_the_right_after_a_last_event_at_x_975_sends_x_width_minus_1() {
        assert_eq!(canvas_at_exit(track((950.0, 400.0), (975.0, 400.0)), FULL), Some((1999, 800)));
    }

    #[test]
    fn a_diagonal_leave_keeps_the_crossing_point_on_the_edge() {
        // From (500, 760) the step (10, 20) reaches y = 800 after 2 steps,
        // at x = 520.
        assert_eq!(track((490.0, 740.0), (500.0, 760.0)).exit(WINDOW), Some(PhysicalPosition::new(520.0, 800.0)));
    }

    #[test]
    fn a_flick_into_the_bottom_right_corner_leaves_through_the_edge_it_reaches_first() {
        // x = 990 is 0.5 steps from the right edge, y = 780 is 1 step from
        // the bottom.
        assert_eq!(track((970.0, 760.0), (990.0, 780.0)).exit(WINDOW), Some(PhysicalPosition::new(1000.0, 790.0)));
    }

    #[test]
    fn a_leave_through_the_bottom_of_a_letterboxed_image_sends_y_height_minus_1() {
        // A 2000x800 image fills a 1000x400 band in the middle of the
        // 1000x800 window, so the pointer leaves the window 200 px below it.
        let band = Viewport { x: 0.0, y: 200.0, w: 1000.0, h: 400.0 };
        let exit = track((500.0, 740.0), (500.0, 768.0)).exit(WINDOW);
        assert_eq!(exit.map(|p| canvas(p, band, (2000, 800))), Some((1000, 799)));
    }

    #[test]
    fn a_leave_far_from_where_the_last_step_heads_is_no_edge_exit() {
        // At y = 400 a 10 px step down needs 40 steps to reach the bottom.
        assert_eq!(track((500.0, 390.0), (500.0, 400.0)).exit(WINDOW), None);
    }

    #[test]
    fn a_leave_without_a_step_is_no_edge_exit() {
        assert_eq!(Track::at(PhysicalPosition::new(500.0, 790.0)).exit(WINDOW), None);
    }

    #[test]
    fn a_leave_during_a_drag_beyond_the_bottom_stays_on_the_bottom_edge() {
        // While a button is held, motion goes on outside the window.
        assert_eq!(track((500.0, 820.0), (500.0, 850.0)).exit(WINDOW), Some(PhysicalPosition::new(500.0, 800.0)));
    }
}

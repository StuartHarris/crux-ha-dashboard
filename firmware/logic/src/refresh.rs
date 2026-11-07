//! Decides how to refresh the panel for a new frame.
//!
//! A full refresh drives all 16 grey levels and clears ghost images, but the
//! panel flashes for about a second. A partial refresh uses the fast
//! black-and-white waveform on the area that changed. It cannot draw the
//! grey levels between black and white, and it leaves some ghost images.

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};
use ui::Framebuffer;

/// The number of partial refreshes after which the policy asks for a full
/// refresh, to clear ghost images.
pub const MAX_PARTIALS: u32 = 30;

/// How to refresh the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The panel already shows the frame.
    None,
    /// Draw the whole frame with all the grey levels.
    Full,
    /// Draw this area of the frame with the black-and-white waveform.
    Partial(Rectangle),
}

/// Keeps what the panel shows and decides how to refresh it.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// The `full_refresh_seq` of the last full refresh, or `None` before the
    /// first one.
    shown_seq: Option<u32>,
    /// The number of partial refreshes since the last full refresh.
    partials: u32,
}

impl Policy {
    /// Decides how to refresh the panel, which shows `shown`, to show `next`.
    /// `seq` is the `full_refresh_seq` of the view model of `next`.
    ///
    /// The refresh is full if the core asked for one (the number changed), if
    /// the changed area has a grey level that is not black or white, or after
    /// [`MAX_PARTIALS`] partial refreshes. Otherwise the refresh is partial,
    /// on the area that changed. The policy records the refresh, so call this
    /// function only for a refresh that the shell does.
    pub fn decide(&mut self, seq: u32, shown: &Framebuffer, next: &Framebuffer) -> Refresh {
        if self.shown_seq != Some(seq) {
            return self.full(seq);
        }
        let Some(area) = shown.changed_area(next) else {
            return Refresh::None;
        };
        if self.partials >= MAX_PARTIALS || has_grey(next, area) {
            return self.full(seq);
        }
        self.partials += 1;
        Refresh::Partial(area)
    }

    /// Forgets what the panel shows, so the next refresh is full. Use it
    /// after the panel lost power.
    pub const fn reset(&mut self) {
        self.shown_seq = None;
    }

    const fn full(&mut self, seq: u32) -> Refresh {
        self.shown_seq = Some(seq);
        self.partials = 0;
        Refresh::Full
    }
}

/// Returns `true` if a pixel in `area` is not black or white.
fn has_grey(frame: &Framebuffer, area: Rectangle) -> bool {
    area.points()
        .filter_map(|point| frame.pixel(point))
        .any(|color| color != Gray4::BLACK && color != Gray4::WHITE)
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};
    use ui::Framebuffer;

    use super::{MAX_PARTIALS, Policy, Refresh};

    fn with_pixel(point: Point, color: Gray4) -> Framebuffer {
        let mut frame = Framebuffer::new();
        let Ok(()) = Pixel(point, color).draw(&mut frame);
        frame
    }

    #[test]
    fn the_first_refresh_is_full() {
        let frame = Framebuffer::new();
        assert_eq!(Policy::default().decide(0, &frame, &frame), Refresh::Full);
    }

    #[test]
    fn a_new_sequence_number_gives_a_full_refresh() {
        let mut policy = Policy::default();
        let frame = Framebuffer::new();
        policy.decide(1, &frame, &frame);
        assert_eq!(policy.decide(1, &frame, &frame), Refresh::None);
        assert_eq!(policy.decide(2, &frame, &frame), Refresh::Full);
    }

    #[test]
    fn a_small_change_gives_a_partial_refresh_of_its_area() {
        let mut policy = Policy::default();
        let shown = Framebuffer::new();
        policy.decide(1, &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::BLACK);
        assert_eq!(
            policy.decide(1, &shown, &next),
            Refresh::Partial(Rectangle::new(Point::new(10, 20), Size::new(1, 1)))
        );
    }

    #[test]
    fn a_grey_pixel_gives_a_full_refresh() {
        let mut policy = Policy::default();
        let shown = Framebuffer::new();
        policy.decide(1, &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::new(7));
        assert_eq!(policy.decide(1, &shown, &next), Refresh::Full);
    }

    #[test]
    fn many_partial_refreshes_give_a_full_refresh() {
        let mut policy = Policy::default();
        let shown = Framebuffer::new();
        let next = with_pixel(Point::new(1, 1), Gray4::BLACK);
        policy.decide(1, &shown, &shown);
        for _ in 0..MAX_PARTIALS {
            assert!(matches!(
                policy.decide(1, &shown, &next),
                Refresh::Partial(_)
            ));
        }
        assert_eq!(policy.decide(1, &shown, &next), Refresh::Full);
        assert!(matches!(
            policy.decide(1, &shown, &next),
            Refresh::Partial(_)
        ));
    }

    #[test]
    fn reset_gives_a_full_refresh() {
        let mut policy = Policy::default();
        let frame = Framebuffer::new();
        policy.decide(1, &frame, &frame);
        policy.reset();
        assert_eq!(policy.decide(1, &frame, &frame), Refresh::Full);
    }
}

//! Decides how a shell refreshes the panel for a new frame.
//!
//! A full refresh drives all 16 grey levels and clears ghost images, but the
//! panel flashes for about a second. A partial refresh uses the fast
//! black-and-white waveform on the area that changed. It cannot draw the
//! grey levels between black and white.
//!
//! The core decides when the panel needs a full refresh to clear ghost
//! images, with `ViewModel::full_refresh_seq`. This module adds only the
//! limit of the panel: a change with grey pixels needs a full refresh. The
//! simulator and the firmware both use [`Refresher`], so they refresh in the
//! same way.

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

use crate::Framebuffer;

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

/// Keeps the `full_refresh_seq` of the last full refresh, and decides how to
/// refresh the panel.
///
/// The core does not know about a full refresh that the refresher gives for
/// grey pixels. The core therefore does not start its count of partial
/// renders again, and its next full refresh comes earlier than necessary.
/// The panel shows black and white only, so this is rare.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refresher {
    /// The `full_refresh_seq` of the last full refresh, or `None` before the
    /// first one.
    shown_seq: Option<u32>,
}

impl Refresher {
    /// Decides how to refresh the panel, which shows `shown`, to show `next`.
    /// `seq` is the `full_refresh_seq` of the view model of `next`.
    ///
    /// The refresh is full in these cases:
    ///
    /// - The core asked for a full refresh: `seq` is different from the
    ///   number of the last full refresh, or no full refresh occurred. The
    ///   refresh is full also if the frame did not change, because the core
    ///   asks for a full refresh to clear ghost images, and only the flash of
    ///   a full refresh clears them.
    /// - The area that changed has a pixel that is not black or white.
    ///
    /// Otherwise, the refresh is partial, on the area that changed. If no
    /// pixel changed, there is no refresh. The refresher records a full
    /// refresh, so call this function only for a refresh that the shell
    /// does.
    pub fn decide(&mut self, seq: u32, shown: &Framebuffer, next: &Framebuffer) -> Refresh {
        if self.shown_seq != Some(seq) {
            self.shown_seq = Some(seq);
            return Refresh::Full;
        }
        match shown.changed_area(next) {
            None => Refresh::None,
            Some(area) if has_grey(next, area) => Refresh::Full,
            Some(area) => Refresh::Partial(area),
        }
    }

    /// Forgets what the panel shows, so the next refresh is full. Use it
    /// after the panel lost power or a refresh failed.
    pub const fn reset(&mut self) {
        self.shown_seq = None;
    }
}

/// Returns `true` if a pixel of `frame` in `area` is not black or white.
#[must_use]
pub fn has_grey(frame: &Framebuffer, area: Rectangle) -> bool {
    area.points()
        .filter_map(|point| frame.pixel(point))
        .any(|color| color != Gray4::BLACK && color != Gray4::WHITE)
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

    use super::{Refresh, Refresher, has_grey};
    use crate::Framebuffer;

    fn with_pixel(point: Point, color: Gray4) -> Framebuffer {
        let mut frame = Framebuffer::new();
        let Ok(()) = Pixel(point, color).draw(&mut frame);
        frame
    }

    #[test]
    fn the_first_refresh_is_full() {
        let frame = Framebuffer::new();
        assert_eq!(
            Refresher::default().decide(0, &frame, &frame),
            Refresh::Full
        );
    }

    #[test]
    fn a_new_sequence_number_gives_a_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = Framebuffer::new();
        refresher.decide(1, &frame, &frame);
        assert_eq!(refresher.decide(1, &frame, &frame), Refresh::None);
        assert_eq!(refresher.decide(2, &frame, &frame), Refresh::Full);
        assert_eq!(refresher.decide(2, &frame, &frame), Refresh::None);
    }

    #[test]
    fn a_new_sequence_number_with_the_same_frame_is_a_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = with_pixel(Point::new(3, 3), Gray4::BLACK);
        refresher.decide(1, &frame, &frame);
        assert_eq!(refresher.decide(2, &frame, &frame.clone()), Refresh::Full);
    }

    #[test]
    fn a_small_change_gives_a_partial_refresh_of_its_area() {
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        refresher.decide(1, &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::BLACK);
        assert_eq!(
            refresher.decide(1, &shown, &next),
            Refresh::Partial(Rectangle::new(Point::new(10, 20), Size::new(1, 1)))
        );
    }

    #[test]
    fn many_partial_refreshes_stay_partial() {
        // The core decides when to clear ghost images, not the refresher.
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        let next = with_pixel(Point::new(1, 1), Gray4::BLACK);
        refresher.decide(1, &shown, &shown);
        for _ in 0..100 {
            assert!(matches!(
                refresher.decide(1, &shown, &next),
                Refresh::Partial(_)
            ));
        }
    }

    #[test]
    fn a_grey_pixel_gives_a_full_refresh() {
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        refresher.decide(1, &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::new(7));
        assert_eq!(refresher.decide(1, &shown, &next), Refresh::Full);
    }

    #[test]
    fn reset_gives_a_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = Framebuffer::new();
        refresher.decide(1, &frame, &frame);
        refresher.reset();
        assert_eq!(refresher.decide(1, &frame, &frame), Refresh::Full);
    }

    #[test]
    fn grey_is_any_level_between_black_and_white() {
        let area = Rectangle::new(Point::new(0, 0), Size::new(4, 4));
        assert!(!has_grey(&Framebuffer::new(), area));
        assert!(!has_grey(&with_pixel(Point::new(1, 1), Gray4::BLACK), area));
        for luma in 1..15 {
            let frame = with_pixel(Point::new(3, 3), Gray4::new(luma));
            assert!(has_grey(&frame, area), "{luma}");
        }
        // A grey pixel out of the area does not count.
        let frame = with_pixel(Point::new(10, 10), Gray4::new(5));
        assert!(!has_grey(&frame, area));
    }
}

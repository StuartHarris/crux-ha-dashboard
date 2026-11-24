//! Decides how a shell refreshes the panel for a new frame.
//!
//! A full refresh drives all 16 grey levels and clears ghost images, but the
//! panel flashes for about a second. A partial refresh uses the fast
//! black-and-white waveform on the area that changed. It cannot draw the
//! grey levels between black and white.
//!
//! The core decides when the panel needs a full refresh to clear ghost
//! images, and the [`Depth`] of the full refresh, with
//! `ViewModel::full_refresh`. This module adds only the limit of the panel:
//! a change with grey pixels needs a light full refresh. The simulator and
//! the firmware both use [`Refresher`], so they refresh in the same way.

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

use shared::{Depth, FullRefresh};

use crate::{Framebuffer, pattern};

/// How to refresh the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The panel already shows the frame.
    None,
    /// Draw the whole frame with all the grey levels, after a clear of
    /// this depth.
    Full(Depth),
    /// Draw this area of the frame with the black-and-white waveform.
    Partial(Rectangle),
    /// Draw `area` of the frame with the black-and-white waveform, then
    /// drive each pixel of `dots` to its level (black or white) for a short
    /// time more. The panel does not flash. A shell that cannot do this does
    /// a partial refresh of `area`.
    ///
    /// `dots` had the pending pattern, which the next frame removes. The
    /// waveform drives only the pixels that change, so the small dots of the
    /// pattern can leave a residue. `dots` is inside `area`, and it does not
    /// grow with other changes of the frame. Refer to [`pattern`].
    Clean {
        /// The area that changed.
        area: Rectangle,
        /// The cells of the pattern that the next frame removes.
        dots: Rectangle,
    },
}

/// The smallest number of removed dots that gives [`Refresh::Clean`].
///
/// The next frame must remove this number of dots of the pending pattern. A
/// dot of text or of an icon can look like a dot of the pattern, so one dot
/// is not enough. A tile has hundreds of dots.
pub const CLEAN_MIN_DOTS: usize = 8;

/// Keeps the sequence number of the last full refresh of the core, and
/// decides how to refresh the panel.
///
/// The core does not know about a full refresh that the refresher gives for
/// grey pixels. That full refresh is light, and the core counts the render
/// as it counts a partial render. Refer to `shared::features::refresh`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refresher {
    /// The sequence number of the last full refresh of the core, or `None`
    /// before the first one and after [`Refresher::reset`].
    shown_seq: Option<u32>,
}

impl Refresher {
    /// Decides how to refresh the panel, which shows `shown`, to show `next`.
    /// `full` is the `full_refresh` of the view model of `next`.
    ///
    /// The refresh is full in these cases:
    ///
    /// - The core asked for a full refresh: the sequence number of `full` is
    ///   different from the number of the last full refresh. The depth is
    ///   the depth of `full`. The refresh is full also if the frame did not
    ///   change, because the core asks for a full refresh to clear ghost
    ///   images, and only the flash of a full refresh clears them.
    /// - No full refresh occurred, or [`Refresher::reset`] forgot it. The
    ///   refresh is deep, because the shell does not know what the panel
    ///   shows.
    /// - The area that changed has a pixel that is not black or white. The
    ///   refresh is light.
    ///
    /// If the next frame removes at least [`CLEAN_MIN_DOTS`] dots of the
    /// pending pattern in the area that changed, the refresh is
    /// [`Refresh::Clean`]. Otherwise, the refresh is partial, on the area
    /// that changed. If no pixel changed, there is no refresh. The refresher
    /// records a full refresh, so call this function only for a refresh that
    /// the shell does.
    pub fn decide(
        &mut self,
        full: FullRefresh,
        shown: &Framebuffer,
        next: &Framebuffer,
    ) -> Refresh {
        match self.shown_seq.replace(full.seq) {
            None => return Refresh::Full(Depth::Deep),
            Some(seq) if seq != full.seq => return Refresh::Full(full.depth),
            Some(_) => {}
        }
        match shown.changed_area(next) {
            None => Refresh::None,
            Some(area) if has_grey(next, area) => Refresh::Full(Depth::Light),
            Some(area) => match pattern::removed_dots(shown, next, area) {
                pattern::Removed {
                    count,
                    area: Some(dots),
                } if count >= CLEAN_MIN_DOTS => Refresh::Clean { area, dots },
                _ => Refresh::Partial(area),
            },
        }
    }

    /// Forgets what the panel shows, so the next refresh is a deep full
    /// refresh. Use it after the panel lost power or a refresh failed.
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

    use embedded_graphics::primitives::PrimitiveStyle;

    use shared::{Depth, FullRefresh};

    use super::{Refresh, Refresher, has_grey};
    use crate::{Framebuffer, pattern::tests::with_pattern};

    /// Returns the light full refresh with the number `seq`.
    const fn light(seq: u32) -> FullRefresh {
        FullRefresh {
            seq,
            depth: Depth::Light,
        }
    }

    /// Returns the deep full refresh with the number `seq`.
    const fn deep(seq: u32) -> FullRefresh {
        FullRefresh {
            seq,
            depth: Depth::Deep,
        }
    }

    fn with_pixel(point: Point, color: Gray4) -> Framebuffer {
        let mut frame = Framebuffer::new();
        let Ok(()) = Pixel(point, color).draw(&mut frame);
        frame
    }

    #[test]
    fn the_first_refresh_is_deep() {
        // The shell does not know what the panel shows, so the depth that
        // the core asks for does not count.
        let frame = Framebuffer::new();
        assert_eq!(
            Refresher::default().decide(light(0), &frame, &frame),
            Refresh::Full(Depth::Deep)
        );
    }

    #[test]
    fn a_new_sequence_number_gives_a_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = Framebuffer::new();
        refresher.decide(light(1), &frame, &frame);
        assert_eq!(refresher.decide(light(1), &frame, &frame), Refresh::None);
        assert_eq!(
            refresher.decide(light(2), &frame, &frame),
            Refresh::Full(Depth::Light)
        );
        assert_eq!(refresher.decide(light(2), &frame, &frame), Refresh::None);
    }

    #[test]
    fn a_new_sequence_number_gives_the_depth_of_the_core() {
        let mut refresher = Refresher::default();
        let frame = Framebuffer::new();
        refresher.decide(light(1), &frame, &frame);
        assert_eq!(
            refresher.decide(deep(2), &frame, &frame),
            Refresh::Full(Depth::Deep)
        );
        assert_eq!(
            refresher.decide(light(3), &frame, &frame),
            Refresh::Full(Depth::Light)
        );
    }

    #[test]
    fn a_new_sequence_number_with_the_same_frame_is_a_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = with_pixel(Point::new(3, 3), Gray4::BLACK);
        refresher.decide(light(1), &frame, &frame);
        assert_eq!(
            refresher.decide(light(2), &frame, &frame.clone()),
            Refresh::Full(Depth::Light)
        );
    }

    #[test]
    fn a_small_change_gives_a_partial_refresh_of_its_area() {
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        refresher.decide(light(1), &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::BLACK);
        assert_eq!(
            refresher.decide(light(1), &shown, &next),
            Refresh::Partial(Rectangle::new(Point::new(10, 20), Size::new(1, 1)))
        );
    }

    #[test]
    fn many_partial_refreshes_stay_partial() {
        // The core decides when to clear ghost images, not the refresher.
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        let next = with_pixel(Point::new(1, 1), Gray4::BLACK);
        refresher.decide(light(1), &shown, &shown);
        for _ in 0..100 {
            assert!(matches!(
                refresher.decide(light(1), &shown, &next),
                Refresh::Partial(_)
            ));
        }
    }

    #[test]
    fn a_grey_pixel_gives_a_light_full_refresh() {
        let mut refresher = Refresher::default();
        let shown = Framebuffer::new();
        // The last full refresh of the core was deep, but the grey pixel
        // needs only a light full refresh.
        refresher.decide(deep(1), &shown, &shown);
        let next = with_pixel(Point::new(10, 20), Gray4::new(7));
        assert_eq!(
            refresher.decide(deep(1), &shown, &next),
            Refresh::Full(Depth::Light)
        );
    }

    /// Returns `frame` with a black square at `area`.
    fn with_black(mut frame: Framebuffer, area: Rectangle) -> Framebuffer {
        let Ok(()) = area
            .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
            .draw(&mut frame);
        frame
    }

    #[test]
    fn the_pattern_to_a_solid_fill_is_a_clean_refresh() {
        let area = Rectangle::new(Point::new(40, 40), Size::new(64, 32));
        let pattern = with_pattern(area);
        let black = with_black(Framebuffer::new(), area);
        let white = Framebuffer::new();
        assert!(!has_grey(&pattern, area));

        let mut refresher = Refresher::default();
        refresher.decide(light(1), &pattern, &pattern);
        assert_eq!(
            refresher.decide(light(1), &pattern, &black),
            Refresh::Clean { area, dots: area }
        );
        assert!(matches!(
            refresher.decide(light(1), &pattern, &white),
            Refresh::Clean { .. }
        ));
    }

    #[test]
    fn the_clean_area_does_not_grow_with_other_changes() {
        let area = Rectangle::new(Point::new(40, 40), Size::new(64, 32));
        let pattern = with_pattern(area);
        // The next frame removes the pattern, and changes a pixel far away.
        let mut next = Framebuffer::new();
        let Ok(()) = Pixel(Point::new(900, 500), Gray4::BLACK).draw(&mut next);
        let mut refresher = Refresher::default();
        refresher.decide(light(1), &pattern, &pattern);
        let Refresh::Clean {
            area: changed,
            dots,
        } = refresher.decide(light(1), &pattern, &next)
        else {
            panic!("not a clean refresh");
        };
        assert_eq!(dots, area);
        assert!(changed.size.width > 800);
    }

    #[test]
    fn a_solid_fill_to_the_pattern_is_a_partial_refresh() {
        let area = Rectangle::new(Point::new(40, 40), Size::new(64, 32));
        let pattern = with_pattern(area);
        let black = with_black(Framebuffer::new(), area);
        let mut refresher = Refresher::default();
        refresher.decide(light(1), &black, &black);
        assert!(matches!(
            refresher.decide(light(1), &black, &pattern),
            Refresh::Partial(_)
        ));
        assert!(matches!(
            refresher.decide(light(1), &Framebuffer::new(), &pattern),
            Refresh::Partial(_)
        ));
    }

    #[test]
    fn a_few_dots_that_go_are_a_partial_refresh() {
        // A dot of text can look like a dot of the pattern.
        let area = Rectangle::new(Point::new(40, 40), Size::new(4, 4));
        let dot = with_pattern(area);
        let mut refresher = Refresher::default();
        refresher.decide(light(1), &dot, &dot);
        assert!(matches!(
            refresher.decide(light(1), &dot, &Framebuffer::new()),
            Refresh::Partial(_)
        ));
    }

    #[test]
    fn reset_gives_a_deep_full_refresh() {
        let mut refresher = Refresher::default();
        let frame = Framebuffer::new();
        refresher.decide(light(1), &frame, &frame);
        refresher.reset();
        assert_eq!(
            refresher.decide(light(1), &frame, &frame),
            Refresh::Full(Depth::Deep)
        );
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

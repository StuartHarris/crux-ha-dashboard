//! What the window shows of the panel: the image, the flash of a full
//! refresh, the area of the last partial refresh and the refresh counters.
//!
//! This module has no egui code, so the tests need no window.

use std::time::{Duration, Instant};

use embedded_graphics::primitives::Rectangle;
use shared::ViewModel;

use ui::Refresh;

use crate::panel::Panel;

/// The time for which a full refresh shows the inverted image.
pub const FLASH: Duration = Duration::from_millis(150);

/// The flash of a full refresh. While the flash is on, the window shows the
/// inverted image, as an e-paper panel does before it shows a new image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flash {
    until: Option<Instant>,
}

impl Flash {
    /// Starts a flash at `now`. A flash that is on starts again.
    pub fn start(&mut self, now: Instant) {
        self.until = Some(now + FLASH);
    }

    /// Returns `true` if the flash is on at `now`. If the flash ended,
    /// turns it off.
    pub fn is_on(&mut self, now: Instant) -> bool {
        match self.until {
            Some(until) if now < until => true,
            Some(_) => {
                self.until = None;
                false
            }
            None => false,
        }
    }

    /// Returns the time from `now` to the end of the flash, or `None` if the
    /// flash is off. The window repaints after this time.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until
            .map(|until| until.saturating_duration_since(now))
            .filter(|time| !time.is_zero())
    }
}

/// The number of refreshes of the panel since the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// The number of full refreshes.
    pub full: u64,
    /// The number of partial refreshes. A draw that changes no pixel is
    /// not a refresh.
    pub partial: u64,
    /// The area of the last partial refresh.
    pub last_partial: Option<Rectangle>,
}

/// The panel as the window shows it.
pub struct Screen {
    panel: Panel,
    version: u64,
    flash: Flash,
    counters: Counters,
    /// The area of the last partial refresh after the last full refresh.
    outline: Option<Rectangle>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    /// Makes a white screen that has not refreshed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panel: Panel::new(),
            version: 0,
            flash: Flash::default(),
            counters: Counters::default(),
            outline: None,
        }
    }

    /// Draws `view` and simulates the refresh of the panel at `now`.
    pub fn show(&mut self, view: &ViewModel, now: Instant) {
        match self.panel.draw(view) {
            Refresh::Full => {
                self.counters.full += 1;
                self.flash.start(now);
                self.outline = None;
            }
            // The simulator shows a clean refresh as a partial refresh. Only
            // the waveform of the panel is different.
            Refresh::Partial(area) | Refresh::Clean { area, .. } => {
                self.counters.partial += 1;
                self.counters.last_partial = Some(area);
                self.outline = Some(area);
            }
            // No pixel changed, so the panel does not refresh.
            Refresh::None => return,
        }
        self.version += 1;
    }

    /// Returns the 8-bit grey level of each pixel for a texture, in rows
    /// from the top. If `invert` is `true`, black becomes white and white
    /// becomes black.
    pub fn levels(&self, invert: bool) -> impl Iterator<Item = u8> + '_ {
        self.panel.lumas().map(move |luma| level(luma, invert))
    }

    /// Returns a number that changes each time that the image changes. The
    /// window uploads the image again only when this number changes.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Returns the flash of the last full refresh.
    pub const fn flash(&mut self) -> &mut Flash {
        &mut self.flash
    }

    /// Returns the refresh counters.
    #[must_use]
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// Returns the area of the last partial refresh, or `None` after a full
    /// refresh.
    #[must_use]
    pub const fn outline(&self) -> Option<Rectangle> {
        self.outline
    }
}

/// Converts a grey level (0 to 15) to an 8-bit level. If `invert` is
/// `true`, black becomes white and white becomes black.
#[must_use]
pub const fn level(luma: u8, invert: bool) -> u8 {
    let luma = if luma > 15 { 15 } else { luma };
    let luma = if invert { 15 - luma } else { luma };
    luma * 17
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use shared::{PowerSource, fixtures::FixedShell};

    use super::{FLASH, Flash, Screen, level};

    #[test]
    fn a_flash_lasts_for_the_flash_time() {
        let start = Instant::now();
        let mut flash = Flash::default();
        assert!(!flash.is_on(start));
        assert_eq!(flash.remaining(start), None);

        flash.start(start);
        assert!(flash.is_on(start));
        assert_eq!(flash.remaining(start), Some(FLASH));
        let later = start + Duration::from_millis(100);
        assert!(flash.is_on(later));
        assert_eq!(flash.remaining(later), Some(Duration::from_millis(50)));

        let end = start + FLASH;
        assert!(!flash.is_on(end));
        assert_eq!(flash.remaining(end), None);
        // The flash stays off.
        assert!(!flash.is_on(start));
    }

    #[test]
    fn a_new_flash_starts_again() {
        let start = Instant::now();
        let mut flash = Flash::default();
        flash.start(start);
        let later = start + Duration::from_millis(100);
        flash.start(later);
        assert!(flash.is_on(start + FLASH));
        assert!(!flash.is_on(later + FLASH));
    }

    #[test]
    fn the_counters_count_full_and_partial_refreshes() {
        let now = Instant::now();
        let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
        let mut screen = Screen::new();

        screen.show(&shell.view(), now);
        let counters = screen.counters();
        assert_eq!((counters.full, counters.partial), (1, 0));
        assert!(screen.flash().is_on(now));
        assert_eq!(screen.version(), 1);

        // The same view changes no pixel, so it is not a refresh and the
        // image does not change.
        screen.show(&shell.view(), now);
        assert_eq!(screen.counters(), counters);
        assert_eq!(screen.version(), 1);

        // A lost connection changes the header only: a partial refresh.
        shell.drop_connection();
        screen.show(&shell.view(), now);
        let counters = screen.counters();
        assert_eq!((counters.full, counters.partial), (1, 1));
        let area = counters.last_partial.unwrap();
        assert_eq!(screen.outline(), Some(area));
        assert_eq!(screen.version(), 2);
    }

    #[test]
    fn a_full_refresh_clears_the_outline() {
        let now = Instant::now();
        let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
        let mut screen = Screen::new();
        screen.show(&shell.view(), now);
        shell.drop_connection();
        screen.show(&shell.view(), now);
        assert!(screen.outline().is_some());

        let mut view = shell.view();
        view.full_refresh_seq += 1;
        screen.show(&view, now);
        assert_eq!(screen.outline(), None);
        // The counter keeps the last partial area.
        assert!(screen.counters().last_partial.is_some());
        assert_eq!(screen.counters().full, 2);
    }

    #[test]
    fn the_texture_levels_and_their_inverse() {
        let levels = |invert| [0, 15, 5].map(|luma| level(luma, invert));
        assert_eq!(levels(false), [0, 255, 85]);
        assert_eq!(levels(true), [255, 0, 170]);

        // A new screen is white, and its flash is black.
        let screen = Screen::new();
        let pixels = (ui::WIDTH * ui::HEIGHT) as usize;
        assert_eq!(
            screen.levels(false).filter(|&level| level == 255).count(),
            pixels
        );
        assert_eq!(
            screen.levels(true).filter(|&level| level == 0).count(),
            pixels
        );
    }
}

//! Converts the samples of the touch controller to taps on the panel.
//!
//! The GT911 reports points in its own coordinates. [`TouchMap`] converts a
//! point to panel coordinates. [`Tapper`] converts a sequence of samples to
//! taps: one tap when a finger touches the panel, and no more taps until a
//! sample shows that the finger lifted.

use embedded_graphics::prelude::Point;
use ui::{HEIGHT, WIDTH};

/// The shortest time between two taps, in milliseconds. A second touch in
/// this time is a bounce, and the tapper ignores it.
pub const MIN_TAP_INTERVAL_MS: u64 = 250;

/// Converts the coordinates of the touch controller to panel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchMap {
    /// The resolution that the controller reports, as (x, y).
    resolution: (u16, u16),
}

impl TouchMap {
    /// Makes a map for a controller with the resolution `(x, y)`, which the
    /// GT911 has in its registers 0x8146 and 0x8148.
    #[must_use]
    pub const fn new(resolution: (u16, u16)) -> Self {
        Self { resolution }
    }

    /// Returns the panel point for the controller point `(x, y)`.
    ///
    /// If the controller is in portrait (540 × 960), the map turns the point
    /// by 90 degrees: the controller y is the panel x, and the controller x
    /// goes from the bottom of the panel to the top. Otherwise the map scales
    /// each axis. This is the map of the `lilygo-t5s3paperpro` driver.
    // TODO(5b): check the orientation on the device with the `bringup` touch step.
    #[must_use]
    pub fn to_panel(&self, (x, y): (u16, u16)) -> Point {
        let (width, height) = self.resolution;
        let max_x = i64::from(WIDTH) - 1;
        let max_y = i64::from(HEIGHT) - 1;
        let scale = |value: u16, range: u16, max: i64| {
            if range > 1 {
                i64::from(value) * max / (i64::from(range) - 1)
            } else {
                i64::from(value)
            }
        };
        let (panel_x, panel_y) = if self.resolution == (540, 960) {
            let flipped = width.saturating_sub(1).saturating_sub(x);
            (scale(y, height, max_x), scale(flipped, width, max_y))
        } else {
            (scale(x, width, max_x), scale(y, height, max_y))
        };
        Point::new(clamp(panel_x, max_x), clamp(panel_y, max_y))
    }
}

fn clamp(value: i64, max: i64) -> i32 {
    i32::try_from(value.clamp(0, max)).unwrap_or(0)
}

/// One read of the touch controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample<P> {
    /// The controller has no new sample. The finger can still touch the
    /// panel.
    Stale,
    /// A new sample with no finger: the finger lifted.
    Released,
    /// A new sample with a finger at this point.
    Pressed(P),
}

impl<P> Sample<P> {
    /// Converts the point of a `Pressed` sample.
    pub fn map<Q>(self, convert: impl FnOnce(P) -> Q) -> Sample<Q> {
        match self {
            Self::Stale => Sample::Stale,
            Self::Released => Sample::Released,
            Self::Pressed(point) => Sample::Pressed(convert(point)),
        }
    }
}

/// Converts touch samples to taps, with a debounce.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tapper {
    pressed: bool,
    last_tap_ms: Option<u64>,
}

impl Tapper {
    /// Gives the tapper a sample at the time `now_ms`, in milliseconds since
    /// the start.
    ///
    /// Returns the point of a new tap. A tap occurs when a finger touches the
    /// panel after a `Released` sample (or at the start), and at least
    /// [`MIN_TAP_INTERVAL_MS`] after the last tap. Only a `Released` sample
    /// ends a touch. A `Stale` sample changes nothing.
    pub fn sample(&mut self, now_ms: u64, sample: Sample<Point>) -> Option<Point> {
        let point = match sample {
            Sample::Stale => return None,
            Sample::Released => {
                self.pressed = false;
                return None;
            }
            Sample::Pressed(point) => point,
        };
        if self.pressed {
            return None;
        }
        self.pressed = true;
        let bounce = self
            .last_tap_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < MIN_TAP_INTERVAL_MS);
        if bounce {
            return None;
        }
        self.last_tap_ms = Some(now_ms);
        Some(point)
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::Point;

    use super::{MIN_TAP_INTERVAL_MS, Sample, Tapper, TouchMap};

    #[test]
    fn a_portrait_controller_is_turned_to_landscape() {
        let map = TouchMap::new((540, 960));
        // The controller origin is at the bottom left of the panel.
        assert_eq!(map.to_panel((0, 0)), Point::new(0, 539));
        assert_eq!(map.to_panel((539, 959)), Point::new(959, 0));
        assert_eq!(map.to_panel((539, 0)), Point::new(0, 0));
        assert_eq!(map.to_panel((0, 959)), Point::new(959, 539));
    }

    #[test]
    fn a_landscape_controller_is_scaled() {
        let map = TouchMap::new((960, 540));
        assert_eq!(map.to_panel((0, 0)), Point::new(0, 0));
        assert_eq!(map.to_panel((959, 539)), Point::new(959, 539));
        let half = TouchMap::new((480, 270));
        assert_eq!(half.to_panel((479, 269)), Point::new(959, 539));
    }

    #[test]
    fn a_point_off_the_range_stays_on_the_panel() {
        let map = TouchMap::new((960, 540));
        assert_eq!(map.to_panel((2000, 2000)), Point::new(959, 539));
        let unknown = TouchMap::new((0, 0));
        assert_eq!(unknown.to_panel((5, 7)), Point::new(5, 7));
    }

    fn pressed(x: i32) -> Sample<Point> {
        Sample::Pressed(Point::new(x, 5))
    }

    #[test]
    fn a_held_finger_gives_one_tap() {
        let mut tapper = Tapper::default();
        assert_eq!(tapper.sample(0, pressed(5)), Some(Point::new(5, 5)));
        assert_eq!(tapper.sample(40, pressed(6)), None);
        assert_eq!(tapper.sample(2000, pressed(6)), None);
        assert_eq!(tapper.sample(2040, Sample::Released), None);
        assert_eq!(tapper.sample(2080, pressed(5)), Some(Point::new(5, 5)));
    }

    #[test]
    fn a_held_finger_with_gaps_between_samples_gives_one_tap() {
        // The controller has no new sample on most reads while the finger
        // stays down.
        let mut tapper = Tapper::default();
        let mut taps = 0;
        for (now, sample) in [
            (0, pressed(5)),
            (30, Sample::Stale),
            (60, Sample::Stale),
            (400, pressed(5)),
            (430, Sample::Stale),
            (900, Sample::Stale),
            (1200, pressed(6)),
        ] {
            taps += usize::from(tapper.sample(now, sample).is_some());
        }
        assert_eq!(taps, 1);
        // The finger lifts. The next touch is a new tap.
        assert_eq!(tapper.sample(1300, Sample::Released), None);
        assert!(tapper.sample(1600, pressed(5)).is_some());
    }

    #[test]
    fn a_bounce_gives_no_tap() {
        let mut tapper = Tapper::default();
        assert!(tapper.sample(1000, pressed(5)).is_some());
        assert_eq!(tapper.sample(1040, Sample::Released), None);
        assert_eq!(tapper.sample(1080, pressed(5)), None);
        assert_eq!(tapper.sample(1120, Sample::Released), None);
        assert!(
            tapper
                .sample(1000 + MIN_TAP_INTERVAL_MS, pressed(5))
                .is_some()
        );
    }

    #[test]
    fn map_converts_only_a_pressed_point() {
        let map = TouchMap::new((960, 540));
        let raw: Sample<(u16, u16)> = Sample::Pressed((10, 20));
        assert_eq!(
            raw.map(|raw| map.to_panel(raw)),
            Sample::Pressed(Point::new(10, 20))
        );
        assert_eq!(
            Sample::<(u16, u16)>::Stale.map(|raw| map.to_panel(raw)),
            Sample::Stale
        );
        assert_eq!(
            Sample::<(u16, u16)>::Released.map(|raw| map.to_panel(raw)),
            Sample::Released
        );
    }
}

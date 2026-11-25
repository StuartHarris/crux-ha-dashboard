//! Converts the samples of the touch controller to taps on the panel and to
//! the edges of the touch key.
//!
//! The GT911 reports points in its own coordinates. [`TouchMap`] converts a
//! point to panel coordinates. [`Tapper`] converts a sequence of samples to
//! taps: one tap when a finger touches the panel, and no more taps until a
//! sample shows that the finger lifted.
//!
//! The touch key on the front of the device is part of the GT911. Each
//! report tells if the key is down (bit 4 of the status register 0x814E).
//! [`KeyEdges`] compares each report with the one before it, and gives the
//! edges of the key. The core decides what a press means.

use embedded_graphics::prelude::Point;
use shared::KeyEdge;
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
    // The bring-up touch step on 2026-11-18 confirmed this orientation.
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
    /// The controller has no new report. The finger and the key can still
    /// be down.
    Stale,
    /// A new report of the controller.
    Report {
        /// The touch key is down.
        key: bool,
        /// The point of the finger, or `None` if no finger touches the
        /// panel: the finger lifted.
        point: Option<P>,
    },
}

impl<P> Sample<P> {
    /// Returns a report with a finger at `point` and the key up.
    #[must_use]
    pub const fn pressed(point: P) -> Self {
        Self::Report {
            key: false,
            point: Some(point),
        }
    }

    /// Returns a report with no finger and the key up.
    #[must_use]
    pub const fn released() -> Self {
        Self::Report {
            key: false,
            point: None,
        }
    }

    /// Converts the point of a report.
    pub fn map<Q>(self, convert: impl FnOnce(P) -> Q) -> Sample<Q> {
        match self {
            Self::Stale => Sample::Stale,
            Self::Report { key, point } => Sample::Report {
                key,
                point: point.map(convert),
            },
        }
    }
}

/// Gives the edges of the touch key from the reports of the controller.
///
/// Each report tells if the key is down now. The edge is the change from the
/// report before. A `Stale` sample changes nothing. Thus a gap with no
/// reads, for example during a refresh of the panel, gives the correct
/// edge at the next report. A press that starts and ends in the gap gives
/// no edge, unless the controller keeps its report until the next read.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyEdges {
    down: bool,
}

impl KeyEdges {
    /// Gives a sample to the edges, and returns the edge of the key, if the
    /// key changed since the last report.
    pub const fn sample<P>(&mut self, sample: &Sample<P>) -> Option<KeyEdge> {
        let Sample::Report { key, .. } = *sample else {
            return None;
        };
        if key == self.down {
            return None;
        }
        self.down = key;
        Some(if key { KeyEdge::Down } else { KeyEdge::Up })
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
    /// panel after a report with no finger (or at the start), and at least
    /// [`MIN_TAP_INTERVAL_MS`] after the last tap. Only a report with no
    /// finger ends a touch. A `Stale` sample changes nothing. The tapper
    /// does not look at the key.
    pub fn sample(&mut self, now_ms: u64, sample: Sample<Point>) -> Option<Point> {
        let point = match sample {
            Sample::Stale => return None,
            Sample::Report { point: None, .. } => {
                self.pressed = false;
                return None;
            }
            Sample::Report {
                point: Some(point), ..
            } => point,
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

    /// Marks the touch now as used at the time `now_ms`, with no tap. The
    /// next sample of the same touch gives no tap. The shell uses this after
    /// a wake that it sent to the core as `Woke`, so that the touch that woke
    /// the device does not also act on a control.
    pub const fn consume(&mut self, now_ms: u64) {
        self.pressed = true;
        self.last_tap_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::Point;

    use alloc::vec::Vec;

    use shared::KeyEdge;

    use super::{KeyEdges, MIN_TAP_INTERVAL_MS, Sample, Tapper, TouchMap};

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
        Sample::pressed(Point::new(x, 5))
    }

    const RELEASED: Sample<Point> = Sample::released();

    #[test]
    fn a_held_finger_gives_one_tap() {
        let mut tapper = Tapper::default();
        assert_eq!(tapper.sample(0, pressed(5)), Some(Point::new(5, 5)));
        assert_eq!(tapper.sample(40, pressed(6)), None);
        assert_eq!(tapper.sample(2000, pressed(6)), None);
        assert_eq!(tapper.sample(2040, RELEASED), None);
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
        assert_eq!(tapper.sample(1300, RELEASED), None);
        assert!(tapper.sample(1600, pressed(5)).is_some());
    }

    #[test]
    fn a_bounce_gives_no_tap() {
        let mut tapper = Tapper::default();
        assert!(tapper.sample(1000, pressed(5)).is_some());
        assert_eq!(tapper.sample(1040, RELEASED), None);
        assert_eq!(tapper.sample(1080, pressed(5)), None);
        assert_eq!(tapper.sample(1120, RELEASED), None);
        assert!(
            tapper
                .sample(1000 + MIN_TAP_INTERVAL_MS, pressed(5))
                .is_some()
        );
    }

    #[test]
    fn a_consumed_touch_gives_no_tap() {
        let mut tapper = Tapper::default();
        tapper.consume(1000);
        // A late sample of the touch that woke the device.
        assert_eq!(tapper.sample(1030, pressed(5)), None);
        assert_eq!(tapper.sample(1060, RELEASED), None);
        // The next touch is a new tap.
        assert!(
            tapper
                .sample(1000 + MIN_TAP_INTERVAL_MS, pressed(5))
                .is_some()
        );
    }

    #[test]
    fn map_converts_only_the_point_of_a_report() {
        let map = TouchMap::new((960, 540));
        let raw: Sample<(u16, u16)> = Sample::Report {
            key: true,
            point: Some((10, 20)),
        };
        assert_eq!(
            raw.map(|raw| map.to_panel(raw)),
            Sample::Report {
                key: true,
                point: Some(Point::new(10, 20))
            }
        );
        assert_eq!(
            Sample::<(u16, u16)>::Stale.map(|raw| map.to_panel(raw)),
            Sample::Stale
        );
        assert_eq!(
            Sample::<(u16, u16)>::released().map(|raw| map.to_panel(raw)),
            RELEASED
        );
    }

    /// A report with the key down and no finger.
    const KEY_ONLY: Sample<Point> = Sample::Report {
        key: true,
        point: None,
    };

    #[test]
    fn the_key_does_not_change_the_taps() {
        let mut tapper = Tapper::default();
        assert_eq!(tapper.sample(0, KEY_ONLY), None);
        let with_key = Sample::Report {
            key: true,
            point: Some(Point::new(5, 5)),
        };
        assert_eq!(tapper.sample(400, with_key), Some(Point::new(5, 5)));
        // A report with the key and no finger ends the touch.
        assert_eq!(tapper.sample(440, KEY_ONLY), None);
        assert!(tapper.sample(800, pressed(5)).is_some());
    }

    #[test]
    fn a_press_of_the_key_gives_one_down_and_one_up() {
        let mut edges = KeyEdges::default();
        let mut seen = Vec::new();
        for sample in [
            RELEASED,
            KEY_ONLY,
            Sample::Stale,
            KEY_ONLY,
            KEY_ONLY,
            Sample::Stale,
            RELEASED,
            RELEASED,
        ] {
            seen.extend(edges.sample(&sample));
        }
        assert_eq!(seen, [KeyEdge::Down, KeyEdge::Up]);
    }

    #[test]
    fn a_gap_in_the_reads_gives_the_correct_edge_after_it() {
        let mut edges = KeyEdges::default();
        assert_eq!(edges.sample(&KEY_ONLY), Some(KeyEdge::Down));
        // A refresh of the panel: no reads for about 1.8 s. The key went up
        // in the gap.
        assert_eq!(edges.sample(&RELEASED), Some(KeyEdge::Up));
        // The key went down in the gap, and stays down after it.
        assert_eq!(edges.sample(&Sample::<Point>::Stale), None);
        assert_eq!(edges.sample(&KEY_ONLY), Some(KeyEdge::Down));
    }

    #[test]
    fn a_finger_does_not_change_the_key() {
        let mut edges = KeyEdges::default();
        assert_eq!(edges.sample(&pressed(5)), None);
        assert_eq!(edges.sample(&RELEASED), None);
        let both = Sample::Report {
            key: true,
            point: Some(Point::new(5, 5)),
        };
        assert_eq!(edges.sample(&both), Some(KeyEdge::Down));
        assert_eq!(edges.sample(&KEY_ONLY), None);
    }
}

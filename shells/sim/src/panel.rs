//! The simulated e-paper panel: the image on it, the refresh rule, and the
//! area that a partial refresh changes.

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::ViewModel;
use ui::Framebuffer;

/// How the panel shows a new image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// All 16 greys. The panel flashes, and the flash clears ghost images.
    Full,
    /// Black and white only. It is fast and does not flash.
    Partial,
}

/// Returns the refresh for a view. The panel does a full refresh if the
/// view asks for a different full refresh than the last one that the panel
/// did, and for the first image.
#[must_use]
pub fn refresh(last_full: Option<u32>, view_seq: u32) -> Refresh {
    if last_full == Some(view_seq) {
        Refresh::Partial
    } else {
        Refresh::Full
    }
}

/// Returns the share of the panel that `area` covers, in percent.
#[must_use]
pub fn share(area: Rectangle) -> f64 {
    let panel = f64::from(ui::WIDTH) * f64::from(ui::HEIGHT);
    f64::from(area.size.width) * f64::from(area.size.height) * 100.0 / panel
}

/// The result of a draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
    /// How the panel shows the new image.
    pub refresh: Refresh,
    /// The smallest rectangle that contains all the pixels that changed, or
    /// `None` if no pixel changed. A partial refresh of the firmware
    /// refreshes only this area.
    pub changed: Option<Rectangle>,
}

/// The panel. It draws into a [`ui::Framebuffer`], the same frame that the
/// firmware uses, and finds the changed area with
/// [`ui::Framebuffer::changed_area`].
pub struct Panel {
    frame: Framebuffer,
    last_full: Option<u32>,
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    /// Makes a white panel that has not refreshed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frame: Framebuffer::new(),
            last_full: None,
        }
    }

    /// Draws `view` and returns how the panel shows it.
    pub fn draw(&mut self, view: &ViewModel) -> Update {
        let mut next = Framebuffer::new();
        let Ok(()) = ui::draw(view, &mut next);
        let refresh = refresh(self.last_full, view.full_refresh_seq);
        if refresh == Refresh::Full {
            self.last_full = Some(view.full_refresh_seq);
        }
        let changed = self.frame.changed_area(&next);
        self.frame = next;
        Update { refresh, changed }
    }

    /// Returns the grey level (0 to 15) of each pixel, in rows from the top.
    pub fn lumas(&self) -> Vec<u8> {
        self.frame.lumas().collect()
    }

    /// Writes the image to a PNG file. Only this function uses the display
    /// of the simulator crate, for its PNG output.
    ///
    /// # Errors
    ///
    /// Returns the error if the file cannot be written.
    pub fn save_png(&self, path: &std::path::Path) -> Result<(), String> {
        let mut display = SimulatorDisplay::<Gray4>::new(Size::new(ui::WIDTH, ui::HEIGHT));
        let area = display.bounding_box();
        let Ok(()) = display.fill_contiguous(&area, self.frame.lumas().map(Gray4::new));
        display
            .to_grayscale_output_image(&OutputSettings::default())
            .save_png(path)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }
}

/// Where the panel is in a window: an integer scale and an offset, so that
/// each panel pixel is a square of window pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    /// The number of window pixels on each side of a panel pixel.
    pub scale: u32,
    /// The left edge of the panel in the window.
    pub left: u32,
    /// The top edge of the panel in the window.
    pub top: u32,
}

impl Viewport {
    /// Returns the largest integer scale at which the panel fits in a window
    /// of `width` × `height` pixels, with the panel in the centre. The scale
    /// is at least 1.
    #[must_use]
    pub fn fit(width: u32, height: u32) -> Self {
        let scale = (width / ui::WIDTH).min(height / ui::HEIGHT).max(1);
        Self {
            scale,
            left: width.saturating_sub(ui::WIDTH * scale) / 2,
            top: height.saturating_sub(ui::HEIGHT * scale) / 2,
        }
    }

    /// Converts a window position to a point on the panel, or `None` if the
    /// position is not on the panel.
    #[must_use]
    pub fn to_panel(self, x: f64, y: f64) -> Option<Point> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let scale = f64::from(self.scale);
        let x = ((x - f64::from(self.left)) / scale).floor();
        let y = ((y - f64::from(self.top)) / scale).floor();
        let inside =
            (0.0..f64::from(ui::WIDTH)).contains(&x) && (0.0..f64::from(ui::HEIGHT)).contains(&y);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the values are on the panel"
        )]
        inside.then(|| Point::new(x as i32, y as i32))
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{prelude::*, primitives::Rectangle};
    use shared::{Connection, PowerSource, ViewModel};

    use super::{Panel, Refresh, Viewport, refresh, share};

    #[test]
    fn the_first_image_and_a_new_sequence_number_are_full_refreshes() {
        assert_eq!(refresh(None, 0), Refresh::Full);
        assert_eq!(refresh(Some(1), 1), Refresh::Partial);
        assert_eq!(refresh(Some(1), 2), Refresh::Full);
        assert_eq!(refresh(Some(u32::MAX), 0), Refresh::Full);
    }

    #[test]
    fn the_share_of_the_panel() {
        let all = Rectangle::new(Point::zero(), Size::new(960, 540));
        assert!((share(all) - 100.0).abs() < f64::EPSILON);
        let quarter = Rectangle::new(Point::zero(), Size::new(480, 270));
        assert!((share(quarter) - 25.0).abs() < f64::EPSILON);
    }

    fn view(status: &str, seq: u32) -> ViewModel {
        ViewModel {
            path: "/".into(),
            title: "Home".into(),
            page_index: 0,
            page_count: 1,
            page_label: "1 / 1".into(),
            tiles: Vec::new(),
            scenes: Vec::new(),
            connection: Connection::Online,
            power: PowerSource::Stand,
            status: status.into(),
            asleep: false,
            last_sync: None,
            updated: "Live".into(),
            full_refresh_seq: seq,
        }
    }

    #[test]
    fn a_partial_refresh_gives_the_area_that_changed() {
        let mut panel = Panel::new();
        let first = panel.draw(&view("Online, on stand", 1));
        assert_eq!(first.refresh, Refresh::Full);

        let same = panel.draw(&view("Online, on stand", 1));
        assert_eq!(same.refresh, Refresh::Partial);
        assert_eq!(same.changed, None);

        let update = panel.draw(&view("Connecting, on stand", 1));
        assert_eq!(update.refresh, Refresh::Partial);
        let area = update.changed.unwrap();
        // Only the status text in the header changed.
        assert!(area.bottom_right().unwrap().y < 60, "{area:?}");
        assert!(area.top_left.x > 480, "{area:?}");
    }

    #[test]
    fn the_viewport_on_a_retina_display() {
        let viewport = Viewport::fit(1920, 1080);
        assert_eq!(
            viewport,
            Viewport {
                scale: 2,
                left: 0,
                top: 0
            }
        );
        assert_eq!(viewport.to_panel(3.0, 5.0), Some(Point::new(1, 2)));
        assert_eq!(
            viewport.to_panel(1919.0, 1079.0),
            Some(Point::new(959, 539))
        );
        assert_eq!(viewport.to_panel(1920.0, 0.0), None);
    }

    #[test]
    fn the_viewport_in_a_larger_window_is_in_the_centre() {
        let viewport = Viewport::fit(1000, 600);
        assert_eq!(
            viewport,
            Viewport {
                scale: 1,
                left: 20,
                top: 30
            }
        );
        assert_eq!(viewport.to_panel(19.0, 30.0), None);
        assert_eq!(viewport.to_panel(20.0, 30.0), Some(Point::new(0, 0)));
        assert_eq!(viewport.to_panel(-1.0, 40.0), None);
    }
}

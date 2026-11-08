//! The simulated e-paper panel: the image on it and the refresh of each new
//! image.
//!
//! The panel decides each refresh with [`ui::Refresher`], as the firmware
//! does, so the simulator shows the refresh pattern of the device.

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::ViewModel;
use ui::{Framebuffer, Refresh, Refresher};

/// Returns the share of the panel that `area` covers, in percent.
#[must_use]
pub fn share(area: Rectangle) -> f64 {
    let panel = f64::from(ui::WIDTH) * f64::from(ui::HEIGHT);
    f64::from(area.size.width) * f64::from(area.size.height) * 100.0 / panel
}

/// The panel. It draws into a [`ui::Framebuffer`], the same frame that the
/// firmware uses.
pub struct Panel {
    frame: Framebuffer,
    refresher: Refresher,
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
            refresher: Refresher::default(),
        }
    }

    /// Draws `view` and returns how the panel shows it. A partial refresh
    /// gives the area that changed.
    pub fn draw(&mut self, view: &ViewModel) -> Refresh {
        let mut next = Framebuffer::new();
        let Ok(()) = ui::draw(view, &mut next);
        let refresh = self
            .refresher
            .decide(view.full_refresh_seq, &self.frame, &next);
        self.frame = next;
        refresh
    }

    /// Returns the grey level (0 to 15) of each pixel, in rows from the top.
    pub fn lumas(&self) -> impl Iterator<Item = u8> + '_ {
        self.frame.lumas()
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

/// The number of screen pixels on each side of a panel pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    /// The largest integer scale at which the panel fits.
    Fit,
    /// This scale, which is at least 1.
    Fixed(u32),
}

impl Scale {
    /// Returns the integer scale for an area of `width` × `height` screen
    /// pixels. The scale is at least 1.
    #[must_use]
    pub fn resolve(self, width: u32, height: u32) -> u32 {
        match self {
            Self::Fit => (width / ui::WIDTH).min(height / ui::HEIGHT).max(1),
            Self::Fixed(scale) => scale.max(1),
        }
    }
}

/// Where the panel is in a window: an integer scale and an offset, so that
/// each panel pixel is a square of window pixels. All the values are in
/// screen pixels, not in egui points.
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
    /// Returns the viewport for `scale` in a window of `width` × `height`
    /// pixels, with the panel in the centre. If the panel is larger than the
    /// window, the panel starts at the top left.
    #[must_use]
    pub fn place(scale: Scale, width: u32, height: u32) -> Self {
        let scale = scale.resolve(width, height);
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
    use shared::{Connection, IndexView, PageView, PowerSource, ViewModel};

    use ui::Refresh;

    use super::{Panel, Scale, Viewport, share};

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
            page: PageView::Index(IndexView {
                weather: None,
                all_lights: None,
                rooms: Vec::new(),
            }),
            connection: Connection::Online,
            power: PowerSource::Stand,
            status: status.into(),
            asleep: false,
            read_only: false,
            last_sync: None,
            updated: "Last changed 09:41".into(),
            full_refresh_seq: seq,
        }
    }

    #[test]
    fn a_partial_refresh_gives_the_area_that_changed() {
        let mut panel = Panel::new();
        let first = panel.draw(&view("Online, on stand", 1));
        assert_eq!(first, Refresh::Full);

        let same = panel.draw(&view("Online, on stand", 1));
        assert_eq!(same, Refresh::None);

        let update = panel.draw(&view("Connecting, on stand", 1));
        let Refresh::Partial(area) = update else {
            panic!("not a partial refresh: {update:?}");
        };
        // Only the status text in the header changed.
        assert!(area.bottom_right().unwrap().y < 60, "{area:?}");
        assert!(area.top_left.x > 480, "{area:?}");
    }

    #[test]
    fn the_viewport_on_a_retina_display() {
        let viewport = Viewport::place(Scale::Fit, 1920, 1080);
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
        let viewport = Viewport::place(Scale::Fit, 1000, 600);
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

    #[test]
    fn a_fixed_scale_maps_each_panel_pixel_to_a_square() {
        // A window of 2100 × 1200 screen pixels.
        let one = Viewport::place(Scale::Fixed(1), 2100, 1200);
        assert_eq!(
            one,
            Viewport {
                scale: 1,
                left: 570,
                top: 330
            }
        );
        assert_eq!(one.to_panel(570.0, 330.0), Some(Point::new(0, 0)));
        assert_eq!(one.to_panel(570.9, 330.9), Some(Point::new(0, 0)));
        assert_eq!(one.to_panel(1529.5, 869.5), Some(Point::new(959, 539)));
        assert_eq!(one.to_panel(1530.0, 869.0), None);
        assert_eq!(one.to_panel(569.9, 400.0), None);

        let two = Viewport::place(Scale::Fixed(2), 2100, 1200);
        assert_eq!(
            two,
            Viewport {
                scale: 2,
                left: 90,
                top: 60
            }
        );
        assert_eq!(two.to_panel(90.0, 60.0), Some(Point::new(0, 0)));
        assert_eq!(two.to_panel(91.9, 61.9), Some(Point::new(0, 0)));
        assert_eq!(two.to_panel(92.0, 62.0), Some(Point::new(1, 1)));
        assert_eq!(two.to_panel(2009.0, 1139.0), Some(Point::new(959, 539)));
        assert_eq!(two.to_panel(2010.0, 1139.0), None);
    }

    #[test]
    fn fit_chooses_the_largest_scale_that_fits() {
        assert_eq!(Scale::Fit.resolve(2100, 1200), 2);
        assert_eq!(Scale::Fit.resolve(2100, 1000), 1);
        assert_eq!(Scale::Fit.resolve(100, 100), 1);
        assert_eq!(Scale::Fit.resolve(2880, 1620), 3);
        assert_eq!(Scale::Fixed(0).resolve(100, 100), 1);
    }

    #[test]
    fn a_panel_larger_than_the_window_starts_at_the_top_left() {
        let viewport = Viewport::place(Scale::Fixed(2), 1000, 600);
        assert_eq!(
            viewport,
            Viewport {
                scale: 2,
                left: 0,
                top: 0
            }
        );
        assert_eq!(viewport.to_panel(999.0, 599.0), Some(Point::new(499, 299)));
    }
}

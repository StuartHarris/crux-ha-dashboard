//! Converts a touch to the control that it is on.

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::{Control, ViewModel};

use crate::layout;

/// Returns the control at `point`, or `None` if the point is not on a
/// control.
///
/// The shell sends the result as `Event::Tapped`, also for `None` and also
/// while the device is asleep. The core decides what the tap does.
///
/// A tile or a scene button is a control only if the view has it. A tap on
/// a sensor tile gives `Control::Tile` too. The core decides that the tap
/// does nothing.
#[must_use]
pub fn hit_test(view: &ViewModel, point: Point) -> Option<Control> {
    let on = |area: Rectangle| area.contains(point);

    if on(layout::previous()) {
        return Some(Control::PreviousPage);
    }
    if on(layout::home()) {
        return Some(Control::Home);
    }
    if on(layout::next()) {
        return Some(Control::NextPage);
    }
    if let Some(index) = (0..view.tiles.len()).find(|index| layout::tile(*index).is_some_and(on)) {
        return Some(Control::Tile(index));
    }
    (0..view.scenes.len())
        .find(|index| layout::scene(*index).is_some_and(on))
        .map(Control::Scene)
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{prelude::*, primitives::Rectangle};
    use shared::{Control, layout::MAX_TILES};

    use super::hit_test;
    use crate::{
        layout::{self, FOOTER_TOP, HEIGHT, WIDTH},
        tests::view,
    };

    fn corners(area: Rectangle) -> [Point; 4] {
        let bottom_right = area.bottom_right().unwrap();
        [
            area.top_left,
            bottom_right,
            Point::new(area.top_left.x, bottom_right.y),
            area.center(),
        ]
    }

    #[test]
    fn each_tile_gives_its_index_at_each_corner() {
        let view = view();
        assert_eq!(view.tiles.len(), MAX_TILES);
        for index in 0..MAX_TILES {
            for point in corners(layout::tile(index).unwrap()) {
                assert_eq!(hit_test(&view, point), Some(Control::Tile(index)));
            }
        }
    }

    #[test]
    fn a_scene_button_gives_its_index() {
        let view = view();
        assert_eq!(view.scenes.len(), 1);
        for point in corners(layout::scene(0).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Control::Scene(0)));
        }
    }

    #[test]
    fn the_navigation_buttons() {
        let view = view();
        for point in corners(layout::previous()) {
            assert_eq!(hit_test(&view, point), Some(Control::PreviousPage));
        }
        for point in corners(layout::home()) {
            assert_eq!(hit_test(&view, point), Some(Control::Home));
        }
        for point in corners(layout::next()) {
            assert_eq!(hit_test(&view, point), Some(Control::NextPage));
        }
    }

    #[test]
    fn dead_space_gives_no_control() {
        let mut view = view();
        view.tiles.truncate(2);
        let dead = [
            // The header.
            Point::new(100, 20),
            Point::new(WIDTH.cast_signed() - 20, 20),
            // The gap between two tiles.
            layout::tile(0).unwrap().top_left - Point::new(1, 0),
            layout::tile(1).unwrap().top_left - Point::new(6, 0),
            // A grid cell with no tile, and a scene cell with no scene.
            layout::tile(2).unwrap().center(),
            layout::tile(5).unwrap().center(),
            layout::scene(1).unwrap().center(),
            // The page number and the time of the last update.
            layout::footer_text().center(),
            // The edges of the panel.
            Point::new(0, FOOTER_TOP.cast_signed() + 10),
            Point::new(5, HEIGHT.cast_signed() - 1),
            // Off the panel.
            Point::new(-1, -1),
            Point::new(WIDTH.cast_signed(), HEIGHT.cast_signed()),
        ];
        for point in dead {
            assert_eq!(hit_test(&view, point), None, "{point:?}");
        }
    }
}

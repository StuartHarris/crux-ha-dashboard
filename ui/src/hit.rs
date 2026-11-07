//! Converts a touch to the event that it means.

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::{Event, Route, ViewModel};

use crate::layout;

/// Returns the event for a touch at `point`, or `None` if the touch is not
/// on a control.
///
/// A tile or a scene button is a control only if the view has it. A tap on
/// a sensor tile gives `TileTapped` too. The core decides that the tap does
/// nothing.
#[must_use]
pub fn hit_test(view: &ViewModel, point: Point) -> Option<Event> {
    let on = |area: Rectangle| area.contains(point);

    if on(layout::previous()) {
        return Some(Event::PreviousPage);
    }
    if on(layout::home()) {
        return Some(Event::Navigate(Route::Home));
    }
    if on(layout::next()) {
        return Some(Event::NextPage);
    }
    if let Some(index) = (0..view.tiles.len()).find(|index| layout::tile(*index).is_some_and(on)) {
        return Some(Event::TileTapped(index));
    }
    (0..view.scenes.len())
        .find(|index| layout::scene(*index).is_some_and(on))
        .map(Event::SceneTapped)
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{prelude::*, primitives::Rectangle};
    use shared::{Event, Route, layout::MAX_TILES};

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
                assert_eq!(hit_test(&view, point), Some(Event::TileTapped(index)));
            }
        }
    }

    #[test]
    fn a_scene_button_gives_its_index() {
        let view = view();
        assert_eq!(view.scenes.len(), 1);
        for point in corners(layout::scene(0).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Event::SceneTapped(0)));
        }
    }

    #[test]
    fn the_navigation_buttons() {
        let view = view();
        for point in corners(layout::previous()) {
            assert_eq!(hit_test(&view, point), Some(Event::PreviousPage));
        }
        for point in corners(layout::home()) {
            assert_eq!(hit_test(&view, point), Some(Event::Navigate(Route::Home)));
        }
        for point in corners(layout::next()) {
            assert_eq!(hit_test(&view, point), Some(Event::NextPage));
        }
    }

    #[test]
    fn dead_space_gives_no_event() {
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

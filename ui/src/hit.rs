//! Converts a touch to the control that it is on.

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::{Control, PageView, ViewModel};

use crate::layout;

/// Returns the control at `point`, or `None` if the point is not on a
/// control.
///
/// The shell sends the result as `Event::Tapped`, also for `None` and also
/// while the device is asleep. The core decides what the tap does.
///
/// Each page has the previous, home and next buttons in the footer. The
/// other controls depend on the page:
///
/// - On the index page, the controls are the All lights toggle (if the view
///   has it) and the room buttons that the view has.
/// - On a room page, the controls are the tiles and the scene buttons that
///   the view has. A tap on a sensor tile gives `Control::Tile` too. The
///   core decides that the tap does nothing.
/// - A tile with steps (`TileView::steps`), which is a dimmable light or a
///   thermostat, also has a − and a + button at its bottom corners. They
///   give `Control::Minus` and `Control::Plus`. A tap on the rest of the
///   tile gives `Control::Tile`.
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
    match &view.page {
        PageView::Index(index) => {
            if index.all_lights.is_some() && on(layout::all_lights()) {
                return Some(Control::AllLights);
            }
            (0..index.rooms.len())
                .find(|room| layout::room(*room).is_some_and(on))
                .map(Control::Room)
        }
        PageView::Room(room) => {
            if let Some(tile) =
                (0..room.tiles.len()).find(|tile| layout::tile(*tile).is_some_and(on))
            {
                if room.tiles[tile].steps {
                    if layout::tile_minus(tile).is_some_and(on) {
                        return Some(Control::Minus(tile));
                    }
                    if layout::tile_plus(tile).is_some_and(on) {
                        return Some(Control::Plus(tile));
                    }
                }
                return Some(Control::Tile(tile));
            }
            (0..room.scenes.len())
                .find(|scene| layout::scene(*scene).is_some_and(on))
                .map(Control::Scene)
        }
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{prelude::*, primitives::Rectangle};
    use shared::{
        Control, PageView, TileKind, ViewModel,
        layout::{MAX_ROOMS, MAX_TILES},
    };

    use super::hit_test;
    use crate::{
        layout::{self, FOOTER_TOP, HEIGHT, WIDTH},
        tests::{index_view, view},
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
        // With no steps, the bottom corners are on the tile.
        let mut view = view();
        if let PageView::Room(room) = &mut view.page {
            for tile in &mut room.tiles {
                tile.steps = false;
            }
        }
        assert_eq!(view.tiles().len(), MAX_TILES);
        for index in 0..MAX_TILES {
            for point in corners(layout::tile(index).unwrap()) {
                assert_eq!(hit_test(&view, point), Some(Control::Tile(index)));
            }
        }
    }

    /// Returns `view` with tile 1 as a light with steps and tile 4 as a light
    /// without steps.
    fn dimmable_view() -> ViewModel {
        let mut view = view();
        if let PageView::Room(room) = &mut view.page {
            room.tiles[1].kind = TileKind::Light;
            room.tiles[1].steps = true;
            room.tiles[4].kind = TileKind::Light;
            room.tiles[4].steps = false;
        }
        view
    }

    #[test]
    fn the_step_buttons_of_a_dimmable_tile_give_minus_and_plus() {
        let view = dimmable_view();
        for point in corners(layout::tile_minus(1).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Control::Minus(1)), "{point:?}");
        }
        for point in corners(layout::tile_plus(1).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Control::Plus(1)), "{point:?}");
        }
    }

    #[test]
    fn the_step_buttons_of_a_climate_tile_give_minus_and_plus() {
        let view = view();
        let tile = &view.tiles()[4];
        assert_eq!(tile.kind, TileKind::Climate);
        assert!(tile.steps);
        for point in corners(layout::tile_minus(4).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Control::Minus(4)), "{point:?}");
        }
        for point in corners(layout::tile_plus(4).unwrap()) {
            assert_eq!(hit_test(&view, point), Some(Control::Plus(4)), "{point:?}");
        }
        // The rest of the tile gives the tile. The core does nothing for it.
        let area = layout::tile(4).unwrap();
        assert_eq!(hit_test(&view, area.top_left), Some(Control::Tile(4)));
        assert_eq!(hit_test(&view, area.center()), Some(Control::Tile(4)));
    }

    #[test]
    fn the_rest_of_a_dimmable_tile_gives_the_toggle() {
        let view = dimmable_view();
        let tile = layout::tile(1).unwrap();
        let minus = layout::tile_minus(1).unwrap();
        let plus = layout::tile_plus(1).unwrap();
        let toggle = [
            // The label row.
            tile.top_left,
            Point::new(tile.center().x, tile.top_left.y + 20),
            tile.top_left + Point::new(tile.size.width.cast_signed() - 1, 0),
            // Just above each button.
            minus.top_left - Point::new(0, 1),
            plus.top_left - Point::new(0, 1),
            // The middle of the strip, between the buttons.
            Point::new(tile.center().x, minus.center().y),
            minus.top_left + Point::new(minus.size.width.cast_signed(), 0),
            plus.top_left - Point::new(1, 0),
            Point::new(tile.center().x, tile.bottom_right().unwrap().y),
        ];
        for point in toggle {
            assert_eq!(hit_test(&view, point), Some(Control::Tile(1)), "{point:?}");
        }
    }

    #[test]
    fn an_onoff_tile_gives_the_toggle_over_its_whole_area() {
        let view = dimmable_view();
        let mut points = corners(layout::tile(4).unwrap()).to_vec();
        points.extend(corners(layout::tile_minus(4).unwrap()));
        points.extend(corners(layout::tile_plus(4).unwrap()));
        for point in points {
            assert_eq!(hit_test(&view, point), Some(Control::Tile(4)), "{point:?}");
        }
    }

    #[test]
    fn a_scene_button_gives_its_index() {
        let view = view();
        assert_eq!(view.scenes().len(), 1);
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
        if let PageView::Room(room) = &mut view.page {
            room.tiles.truncate(2);
        }
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

    #[test]
    fn each_room_button_gives_its_index_at_each_corner() {
        let view = index_view();
        assert_eq!(view.index().unwrap().rooms.len(), MAX_ROOMS);
        for index in 0..MAX_ROOMS {
            for point in corners(layout::room(index).unwrap()) {
                assert_eq!(hit_test(&view, point), Some(Control::Room(index)));
            }
        }
    }

    #[test]
    fn the_all_lights_toggle_on_the_index() {
        let view = index_view();
        for point in corners(layout::all_lights()) {
            assert_eq!(hit_test(&view, point), Some(Control::AllLights));
        }
        // The footer is the same on each page.
        assert_eq!(
            hit_test(&view, layout::next().center()),
            Some(Control::NextPage)
        );
    }

    #[test]
    fn on_the_index_the_tiles_scenes_and_weather_are_not_controls() {
        let mut view = index_view();
        if let PageView::Index(index) = &mut view.page {
            index.rooms.truncate(4);
        }
        let dead = [
            // The title, the status and the weather.
            Point::new(100, 20),
            layout::weather().center(),
            // The gap between the toggle and the first row of rooms.
            layout::room(2).unwrap().top_left - Point::new(-10, 6),
            // A room cell with no room.
            layout::room(4).unwrap().center(),
            layout::room(8).unwrap().center(),
            // The scene row of a room page is in the last row of rooms.
            layout::scene(2).unwrap().center(),
        ];
        for point in dead {
            assert_eq!(hit_test(&view, point), None, "{point:?}");
        }

        // An index with no All lights toggle has no control there.
        if let PageView::Index(index) = &mut view.page {
            index.all_lights = None;
        }
        assert_eq!(hit_test(&view, layout::all_lights().center()), None);
    }
}

//! Finds the area of the panel that changed between two frames.
//!
//! A partial refresh of an e-paper panel is faster when the area is small. The
//! firmware refreshes only the area that [`bounding_box`] gives.

use embedded_graphics::{prelude::*, primitives::Rectangle};

/// Returns the smallest rectangle that contains all of `points`, or `None` if
/// there are no points.
///
/// Give it the points that changed, for example the pixels that are on in a
/// difference mask.
pub fn bounding_box(points: impl IntoIterator<Item = Point>) -> Option<Rectangle> {
    let mut points = points.into_iter();
    let first = points.next()?;
    let (min, max) = points.fold((first, first), |(min, max), point| {
        (min.component_min(point), max.component_max(point))
    });
    Some(Rectangle::with_corners(min, max))
}

#[cfg(test)]
mod tests {
    use embedded_graphics::{prelude::*, primitives::Rectangle};

    use super::bounding_box;

    #[test]
    fn no_points_have_no_box() {
        assert_eq!(bounding_box([]), None);
    }

    #[test]
    fn one_point_is_a_box_of_one_pixel() {
        let point = Point::new(5, 7);
        assert_eq!(
            bounding_box([point]),
            Some(Rectangle::new(point, Size::new(1, 1)))
        );
    }

    #[test]
    fn the_box_contains_all_the_points() {
        let points = [Point::new(30, 2), Point::new(4, 50), Point::new(10, 10)];
        let area = bounding_box(points).unwrap();
        assert_eq!(area, Rectangle::new(Point::new(4, 2), Size::new(27, 49)));
        assert!(points.iter().all(|point| area.contains(*point)));
    }
}

//! The routes of the dashboard: one route for each page.
//!
//! The core owns the current route. A route converts to and from a short path,
//! for example `/room/kitchen`. A shell keeps the path, for example in RTC
//! memory during a deep sleep, and gives it back in
//! [`Start`](crate::Start). No shell parses a path.

use alloc::{
    format,
    string::{String, ToString},
};

/// The id of a room. It is the Home Assistant area id, for example `kitchen`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoomId(pub String);

impl RoomId {
    /// Returns the id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A page of the dashboard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// The home page, which shows the whole house.
    #[default]
    Home,
    /// The page of one room.
    Room(RoomId),
}

impl Route {
    /// Makes the route of the room `id`.
    #[must_use]
    pub fn room(id: &str) -> Self {
        Self::Room(RoomId(id.to_string()))
    }

    /// Returns the path of the route: `/` or `/room/<id>`.
    #[must_use]
    pub fn to_path(&self) -> String {
        match self {
            Self::Home => "/".into(),
            Self::Room(id) => format!("/room/{}", id.as_str()),
        }
    }

    /// Reads a path that [`to_path`](Self::to_path) made. Returns `None` if
    /// the path is not a route.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        if path == "/" {
            return Some(Self::Home);
        }
        let id = path.strip_prefix("/room/")?;
        let valid = !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        valid.then(|| Self::room(id))
    }
}

#[cfg(test)]
mod tests {
    use super::Route;

    #[test]
    fn a_route_goes_to_a_path_and_back() {
        for route in [Route::Home, Route::room("living_room"), Route::room("loft")] {
            assert_eq!(Route::from_path(&route.to_path()), Some(route));
        }
        assert_eq!(Route::room("kitchen").to_path(), "/room/kitchen");
    }

    #[test]
    fn a_path_that_is_not_a_route() {
        for path in [
            "",
            "/room/",
            "/room/Kitchen",
            "/room/a/b",
            "/rooms/kitchen",
            "kitchen",
        ] {
            assert_eq!(Route::from_path(path), None, "{path}");
        }
    }
}

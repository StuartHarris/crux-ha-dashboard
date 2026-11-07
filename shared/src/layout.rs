//! The types that describe the dashboard layout: pages, tiles and scenes.
//!
//! The layout data is in [`crate::pages`]. The layout is `const` data, so it
//! needs no allocator and no parser on the device.

use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::route::Route;

/// The largest number of tiles on one page. The `ui` grid has this many cells.
pub const MAX_TILES: usize = 6;
/// The largest number of scene buttons on one page.
pub const MAX_SCENES: usize = 3;

/// One page of the dashboard: the home page or a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// The Home Assistant area id of the room, or `None` for the home page.
    pub area: Option<&'static str>,
    /// The title at the top of the page.
    pub title: &'static str,
    /// The tiles on the page, in reading order.
    pub tiles: &'static [Tile],
    /// The scene buttons on the page, in reading order.
    pub scenes: &'static [Scene],
}

/// A tile that shows one Home Assistant entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    /// The text on the tile.
    pub label: &'static str,
    /// The Home Assistant entity id, for example `light.kitchen`.
    pub entity_id: &'static str,
    /// How the tile shows the entity and what a tap does.
    pub kind: TileKind,
}

/// A button that turns on a Home Assistant scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scene {
    /// The text on the button.
    pub label: &'static str,
    /// The Home Assistant entity id, for example `scene.evening`.
    pub entity_id: &'static str,
}

/// The kind of entity that a tile shows.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum TileKind {
    /// A light. A tap toggles it. The tile shows the brightness.
    Light,
    /// A switch. A tap toggles it.
    Switch,
    /// A sensor. The tile shows the state and the unit. A tap does nothing.
    Sensor,
    /// A thermostat. The tile shows the current and the target temperature.
    /// A tap does nothing.
    Climate,
    /// A weather entity. The tile shows the condition and the temperature.
    /// A tap does nothing.
    Weather,
}

impl TileKind {
    /// Returns the Home Assistant service that a tap calls, if a tap does something.
    #[must_use]
    pub const fn tap_service(self) -> Option<&'static str> {
        match self {
            Self::Light => Some("light.toggle"),
            Self::Switch => Some("switch.toggle"),
            Self::Sensor | Self::Climate | Self::Weather => None,
        }
    }

    /// Returns the entity attributes that the tile shows.
    ///
    /// The core keeps only these attributes from the state stream.
    #[must_use]
    pub const fn attributes(self) -> &'static [&'static str] {
        match self {
            Self::Light => &["brightness"],
            Self::Switch => &[],
            Self::Sensor => &["unit_of_measurement"],
            Self::Climate => &["current_temperature", "temperature"],
            Self::Weather => &["temperature", "temperature_unit"],
        }
    }
}

impl Page {
    /// Returns the route of the page.
    #[must_use]
    pub fn route(&self) -> Route {
        self.area.map_or(Route::Home, Route::room)
    }

    /// Returns `true` if `route` is the route of the page.
    #[must_use]
    pub fn is_at(&self, route: &Route) -> bool {
        match (self.area, route) {
            (None, Route::Home) => true,
            (Some(area), Route::Room(id)) => area == id.as_str(),
            _ => false,
        }
    }
}

/// Makes the home page.
#[must_use]
pub const fn home(title: &'static str, tiles: &'static [Tile], scenes: &'static [Scene]) -> Page {
    Page {
        area: None,
        title,
        tiles,
        scenes,
    }
}

/// Makes the page of a room. `area` is the Home Assistant area id.
#[must_use]
pub const fn room(
    area: &'static str,
    title: &'static str,
    tiles: &'static [Tile],
    scenes: &'static [Scene],
) -> Page {
    Page {
        area: Some(area),
        title,
        tiles,
        scenes,
    }
}

/// Makes a light tile.
#[must_use]
pub const fn light(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Light,
    }
}

/// Makes a switch tile.
#[must_use]
pub const fn switch(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Switch,
    }
}

/// Makes a sensor tile.
#[must_use]
pub const fn sensor(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Sensor,
    }
}

/// Makes a climate tile.
#[must_use]
pub const fn climate(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Climate,
    }
}

/// Makes a weather tile.
#[must_use]
pub const fn weather(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Weather,
    }
}

/// Makes a scene button.
#[must_use]
pub const fn scene(label: &'static str, entity_id: &'static str) -> Scene {
    Scene { label, entity_id }
}

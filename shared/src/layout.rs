//! The types that describe the dashboard layout: pages, tiles and scenes.
//!
//! The layout data is in [`crate::pages`]. The layout is `const` data, so it
//! needs no allocator and no parser on the device.

use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{entities::EntityState, route::Route, temperature};

/// The largest number of tiles on one page. The `ui` grid has this many cells.
pub const MAX_TILES: usize = 6;
/// The largest number of scene buttons on one page.
pub const MAX_SCENES: usize = 3;
/// The largest number of lights and switches that the summary of one room
/// shows. The `ui` room button has a dot for each. Refer to
/// [`counted`](crate::summary::counted).
pub const MAX_LIGHTS: usize = 4;
/// The largest number of rooms. The index page has a button for each room,
/// and the `ui` grid of room buttons has this many cells.
pub const MAX_ROOMS: usize = 9;
/// The change in brightness, in percent, of one tap on the + or the − button
/// of a dimmable light tile. Refer to [`Tile::dimmable`].
pub const BRIGHTNESS_STEP_PCT: i8 = 25;
// Home Assistant accepts a `brightness_step_pct` from -100 to 100. The −
// button sends the negative value, so the step must be positive.
const _: () = assert!(BRIGHTNESS_STEP_PCT >= 1 && BRIGHTNESS_STEP_PCT <= 100);

/// One page of the dashboard: the home page or a room.
///
/// The home page is the index. It shows a button for each room. The tiles of
/// the home page are not in a grid. The header of the index shows them:
///
/// - The first weather tile gives the weather.
/// - The first light or switch tile is the All lights toggle.
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
    /// The light is a Home Assistant light group, for example all the
    /// lights of a room. The summary of a room on the index does not count
    /// a group if the room has other lights or switches, because the group
    /// and its members are the same lights.
    pub group: bool,
    /// The light can change its brightness. The tile has a − and a + button,
    /// which change the brightness by [`BRIGHTNESS_STEP_PCT`]. A tap on the
    /// other parts of the tile toggles the light. Only a light tile can be
    /// dimmable.
    pub dimmable: bool,
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
    /// If the target temperature is known, the tile has a − and a + button,
    /// which change the target temperature by
    /// [`STEP`](crate::temperature::STEP). A tap on the other parts of the
    /// tile does nothing.
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
            Self::Climate => &["current_temperature", temperature::TARGET],
            Self::Weather => &["temperature", "temperature_unit"],
        }
    }
}

impl Tile {
    /// Returns `true` if the tile shows the − and + buttons for `entity`,
    /// which is the state from the state stream, or `None` if the core has
    /// no state for the entity.
    ///
    /// - A light tile must be dimmable (refer to [`Tile::dimmable`]), and the
    ///   light must be `on` or `off`. A light that is unavailable, or that
    ///   has no state or the state `unknown`, has no buttons. Its tile is a
    ///   toggle, as the tile of an on/off light is.
    /// - A climate tile must have a state that is not `unavailable` or
    ///   `unknown`, and a target temperature. Refer to
    ///   [`temperature::target`]. A thermostat that is off usually has no
    ///   target temperature, so its tile has no buttons.
    /// - Other tiles have no buttons.
    ///
    /// The view and the core both use this function, so the panel shows the
    /// buttons only when the core acts on them.
    #[must_use]
    pub fn has_steps(&self, entity: Option<&EntityState>) -> bool {
        let state = entity.and_then(|entity| entity.state.as_deref());
        match self.kind {
            TileKind::Light => self.dimmable && matches!(state, Some("on" | "off")),
            TileKind::Climate => {
                !matches!(state, None | Some("unavailable" | "unknown"))
                    && entity.and_then(temperature::target).is_some()
            }
            TileKind::Switch | TileKind::Sensor | TileKind::Weather => false,
        }
    }
}

impl Page {
    /// Returns `true` if the page is the home page, which is the index.
    #[must_use]
    pub const fn is_home(&self) -> bool {
        self.area.is_none()
    }

    /// Returns `true` if the page is the page of a room.
    #[must_use]
    pub const fn is_room(&self) -> bool {
        self.area.is_some()
    }

    /// Returns the All lights tile of the home page: its first light or
    /// switch tile. Returns `None` if the page is a room or has no such tile.
    #[must_use]
    pub fn all_lights_tile(&self) -> Option<&'static Tile> {
        let tiles: &'static [Tile] = self.tiles;
        tiles
            .iter()
            .filter(|_| self.is_home())
            .find(|tile| matches!(tile.kind, TileKind::Light | TileKind::Switch))
    }

    /// Returns the weather tile of the home page: its first weather tile.
    /// Returns `None` if the page is a room or has no such tile.
    #[must_use]
    pub fn weather_tile(&self) -> Option<&'static Tile> {
        let tiles: &'static [Tile] = self.tiles;
        tiles
            .iter()
            .filter(|_| self.is_home())
            .find(|tile| tile.kind == TileKind::Weather)
    }

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

/// Makes a light tile for a light that can change its brightness. Refer to
/// [`Tile::dimmable`]. For a light that can only be on or off, use
/// [`onoff_light`].
#[must_use]
pub const fn light(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Light,
        group: false,
        dimmable: true,
    }
}

/// Makes a light tile for a light that can only be on or off. Home Assistant
/// gives `onoff` as its only color mode. The tile has no − and + buttons.
/// Refer to [`Tile::dimmable`].
#[must_use]
pub const fn onoff_light(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Light,
        group: false,
        dimmable: false,
    }
}

/// Makes a light tile for a Home Assistant light group. Refer to
/// [`Tile::group`]. The group can change its brightness. Refer to
/// [`Tile::dimmable`].
#[must_use]
pub const fn light_group(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Light,
        group: true,
        dimmable: true,
    }
}

/// Makes a switch tile.
#[must_use]
pub const fn switch(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Switch,
        group: false,
        dimmable: false,
    }
}

/// Makes a sensor tile.
#[must_use]
pub const fn sensor(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Sensor,
        group: false,
        dimmable: false,
    }
}

/// Makes a climate tile.
#[must_use]
pub const fn climate(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Climate,
        group: false,
        dimmable: false,
    }
}

/// Makes a weather tile.
#[must_use]
pub const fn weather(label: &'static str, entity_id: &'static str) -> Tile {
    Tile {
        label,
        entity_id,
        kind: TileKind::Weather,
        group: false,
        dimmable: false,
    }
}

/// Makes a scene button.
#[must_use]
pub const fn scene(label: &'static str, entity_id: &'static str) -> Scene {
    Scene { label, entity_id }
}

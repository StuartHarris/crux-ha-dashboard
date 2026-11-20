//! The Crux core of the Home Assistant dashboard.
//!
//! The core builds without std when you turn off the `std` feature.
//!
//! - [`app`] is the root: the events, the effects, the model and the dispatch.
//! - [`features`] has the logic for rooms, the connection, Wi-Fi, power,
//!   the front light, the refresh of the panel and the marks of taps that
//!   wait for Home Assistant.
//! - [`capabilities`] has the custom effects: a byte stream, the radio,
//!   power, the front light and the log.
//! - [`mqtt`] is an MQTT 3.1.1 client with no I/O: a codec, a framer and the
//!   state machine of a connection.
//! - [`view`] makes the view model, which has all the text that the `ui`
//!   crate draws.
//! - [`summary`] makes the summary of each room on the index page.
//! - [`pages`] is the layout data. Edit it to change the dashboard.
//! - [`temperature`] reads the target temperature of a thermostat and
//!   gives the steps of the − and + buttons of a climate tile.
//! - [`local_time`] gives the UK time, with summer time, for each time that
//!   the view shows.
//! - `fixtures` (with the `fixtures` feature) is a shell with fixed answers,
//!   for tests and pictures.
//! - `fake` (with the `fixtures` feature) is a fake MQTT broker that reads
//!   and writes bytes, with a fake Home Assistant behind it, and a fake
//!   radio.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod app;
pub mod capabilities;
pub mod entities;
#[cfg(any(test, feature = "fixtures"))]
pub mod fake;
pub mod features;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod layout;
pub mod local_time;
pub mod mqtt;
pub mod pages;
pub mod route;
pub mod summary;
pub mod temperature;
pub mod view;

#[cfg(test)]
mod tests;

pub use app::*;
pub use capabilities::power::PowerSource;
pub use crux_core::Core;
pub use layout::TileKind;
pub use route::Route;
pub use view::{
    Connection, Description, IndexView, LightState, PageView, RoomSummary, RoomView, SceneView,
    TileState, TileView, ViewModel,
};

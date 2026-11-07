//! The Crux core of the Home Assistant dashboard.
//!
//! The core builds without std when you turn off the `std` feature.
//!
//! - [`app`] is the root: the events, the effects, the model and the dispatch.
//! - [`features`] has the logic for rooms, the connection and power.
//! - [`capabilities`] has the custom effects: MQTT, network and power.
//! - [`view`] makes the view model, which has all the text that the `ui`
//!   crate draws.
//! - [`pages`] is the layout data. Edit it to change the dashboard.

#![cfg_attr(not(feature = "std"), no_std)]
// The `Facet` derive generates `unsafe` code, which this lint does not expect.
#![allow(clippy::unsafe_derive_deserialize)]

extern crate alloc;

pub mod app;
pub mod capabilities;
pub mod entities;
pub mod features;
pub mod layout;
pub mod pages;
pub mod route;
pub mod view;

#[cfg(test)]
mod tests;

pub use app::*;
pub use capabilities::power::PowerSource;
pub use crux_core::Core;
pub use layout::TileKind;
pub use route::Route;
pub use view::{Connection, SceneView, TileState, TileView, ViewModel};

//! The Crux core of the Home Assistant dashboard.
//!
//! The core builds without std when you turn off the `std` feature.

#![cfg_attr(not(feature = "std"), no_std)]
// The `Facet` derive generates `unsafe` code, which this lint does not expect.
#![allow(clippy::unsafe_derive_deserialize)]

extern crate alloc;

pub mod app;

pub use app::*;
pub use crux_core::Core;

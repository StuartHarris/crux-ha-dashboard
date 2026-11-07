//! Draws the dashboard's view model on a 960×540 panel with 16 grey levels,
//! and converts a touch to an event.
//!
//! The simulator and the firmware use the same code, so the simulator shows
//! what the panel shows. The view model has all the text, so this crate only
//! draws.
//!
//! - [`draw`] draws a view model on any `DrawTarget<Color = Gray4>`.
//! - [`hit_test`] converts a touch point to the event for the control at
//!   that point.
//! - [`layout`] has the rectangle of each control. `draw` and `hit_test` both
//!   use it.
//! - [`Framebuffer`] is a 4-bit image in memory that `draw` can draw on.
//! - [`bounding_box`] gives the area that changed between two frames, for a
//!   partial refresh.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod changes;
mod draw;
mod framebuffer;
mod hit;
pub mod layout;
mod text;

pub use changes::bounding_box;
pub use draw::draw;
pub use framebuffer::Framebuffer;
pub use hit::hit_test;
pub use layout::{HEIGHT, WIDTH};

#[cfg(test)]
mod tests {
    pub use crate::draw::tests::view;
}

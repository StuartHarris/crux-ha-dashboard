//! Draws the dashboard's view model on a 960×540 panel with 16 grey levels.
//!
//! The simulator and the firmware use the same code, so the simulator shows what
//! the panel shows.

#![no_std]

use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_10X20},
    pixelcolor::Gray4,
    prelude::*,
    text::Text,
};
use shared::ViewModel;

/// The panel width in pixels.
pub const WIDTH: u32 = 960;
/// The panel height in pixels.
pub const HEIGHT: u32 = 540;

/// Draws the view model on the display.
///
/// # Errors
///
/// Returns the error of the display if a draw operation fails.
pub fn draw<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    display.clear(Gray4::WHITE)?;

    let style = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
    Text::new(&view.title, Point::new(24, 40), style).draw(display)?;
    Text::new(&view.status, Point::new(24, 80), style).draw(display)?;

    Ok(())
}

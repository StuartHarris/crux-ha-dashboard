//! Draws a page with fixed entity states to a PNG file, with no window and no
//! broker. CI and the documentation use it.
//!
//! The fixed states and answers come from `shared::fixtures`, the same as
//! the golden-image tests of the `ui` crate.

use std::path::Path;

use shared::{PowerSource, fixtures::FixedShell};

use crate::panel::Panel;

/// The page to draw if the options give none.
const DEFAULT_PAGE: &str = "/room/living_room";

/// Runs the core with fixed answers and writes the page `path` to `out`.
///
/// # Errors
///
/// Returns a message if the file cannot be written.
pub fn run(path: Option<String>, out: &Path) -> Result<(), String> {
    let path = path.unwrap_or_else(|| DEFAULT_PAGE.into());
    let shell = FixedShell::start(PowerSource::Stand, &path);
    let mut panel = Panel::new();
    panel.draw(&shell.view());
    panel.save_png(out)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

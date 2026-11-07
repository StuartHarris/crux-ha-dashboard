//! Runs the core on the desktop and writes the panel image to `panel.png`.

use embedded_graphics::{pixelcolor::Gray4, prelude::*};
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::{Core, Dashboard, Effect, Event};

fn main() {
    let core: Core<Dashboard> = Core::new();
    let mut display = SimulatorDisplay::<Gray4>::new(Size::new(ui::WIDTH, ui::HEIGHT));

    for effect in core.process_event(Event::Start) {
        match effect {
            Effect::Render(_) => {
                let Ok(()) = ui::draw(&core.view(), &mut display);
            }
        }
    }

    let image = display.to_grayscale_output_image(&OutputSettings::default());
    image
        .save_png("panel.png")
        .expect("could not write panel.png");
    println!("Wrote panel.png");
}

//! Golden-image tests: the `ui` crate draws views from the real core, and each
//! image must be the same as its reference PNG in `tests/golden/`.
//!
//! The test drives the core with `shared::fixtures::FixedShell`, which has
//! fixed entity states, no clock and no network, so each run gives the same
//! views.
//!
//! To make the reference images again after a change to the layout, run
//! `just golden`. Then examine the new images before you commit them. A
//! normal test run never writes them.
//!
//! If an image is different, the test writes the image that it drew and a
//! mask of the different pixels to the `target` directory, and gives their
//! paths in the failure message.

use std::path::{Path, PathBuf};

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::{
    Control, Event, PageView, PowerSource, TileState, ViewModel, fixtures::FixedShell, pages::PAGES,
};
use ui::{Framebuffer, Refresh, Refresher};

/// Draws `view` and compares the image with the reference image `name`.
/// Returns a description of the difference, or `None` if the images are the
/// same.
fn check(name: &str, view: &ViewModel) -> Option<String> {
    let mut actual = SimulatorDisplay::<Gray4>::new(Size::new(ui::WIDTH, ui::HEIGHT));
    let Ok(()) = ui::draw(view, &mut actual);
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.png"));
    let settings = OutputSettings::default();

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        actual
            .to_grayscale_output_image(&settings)
            .save_png(&golden)
            .expect("could not write the reference image");
        return None;
    }

    let expected = match SimulatorDisplay::<Gray4>::load_png(&golden) {
        Ok(expected) => expected,
        Err(error) => {
            return Some(format!(
                "{name}: could not read {}: {error}. Run `just golden`.",
                golden.display()
            ));
        }
    };
    if expected.size() != actual.size() {
        return Some(format!("{name}: the reference image has a different size"));
    }
    let mask = expected.diff(&actual)?;

    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden");
    std::fs::create_dir_all(&out).expect("could not make the output directory");
    let actual_path = out.join(format!("{name}-actual.png"));
    let mask_path = out.join(format!("{name}-diff.png"));
    actual
        .to_grayscale_output_image(&settings)
        .save_png(&actual_path)
        .expect("could not write the image");
    mask.to_grayscale_output_image(&settings)
        .save_png(&mask_path)
        .expect("could not write the mask");
    let changed = mask
        .bounding_box()
        .points()
        .filter(|point| mask.get_pixel(*point).is_on())
        .count();
    Some(format!(
        "{name}: {changed} pixels are different. The image is {}, and the mask is {}.",
        actual_path.display(),
        mask_path.display()
    ))
}

fn assert_all_match(failures: &[String]) {
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn each_page_matches_its_reference_image() {
    let failures: Vec<String> = PAGES
        .iter()
        .enumerate()
        .filter_map(|(index, page)| {
            let path = page.route().to_path();
            let shell = FixedShell::start(PowerSource::Stand, &path);
            let view = shell.view();
            assert_eq!(view.path, path);
            assert_eq!(view.updated, "Last changed 10:41");
            let slug = page.area.unwrap_or("home");
            check(&format!("page-{index:02}-{slug}"), &view)
        })
        .collect();
    assert_all_match(&failures);
}

#[test]
fn the_asleep_page_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Battery, "/room/living_room");
    // The idle timer expires, so the core disconnects and sleeps.
    shell.fire_timers();
    let view = shell.view();
    assert!(view.asleep);
    assert_eq!(view.updated, "Last changed 10:41");
    assert_all_match(&check("asleep", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_offline_page_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    shell.drop_connection();
    let view = shell.view();
    assert!(!view.asleep);
    assert_eq!(view.updated, "Last changed 10:41");
    assert_all_match(&check("offline", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_dry_run_page_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
    shell.send(Event::SetReadOnly(true));
    let view = shell.view();
    assert!(view.read_only);
    assert_eq!(view.status, "Dry run · Online");
    assert_all_match(&check("dry-run", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_all_lights_toggle_with_no_state_matches_its_reference_image() {
    let path = PAGES[0].route().to_path();
    let shell = FixedShell::start_without(PowerSource::Stand, &path, &["light.all_lights"]);
    let view = shell.view();
    let index = view.index().expect("the first page is the home page");
    let all_lights = index
        .all_lights
        .as_ref()
        .expect("the home page has the toggle");
    assert_eq!(all_lights.entity_id, "light.all_lights");
    assert_eq!(all_lights.state, TileState::Unknown);
    assert_all_match(
        &check("all-lights-no-state", &view)
            .into_iter()
            .collect::<Vec<_>>(),
    );
}

#[test]
fn dimmable_and_onoff_lights_match_their_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    // The widest brightness on a light that is on, a dimmable light that is
    // off, an on/off light that is on and an on/off light that is off.
    shell.set_state("light.dining_room_lights", "on", &[("brightness", "255")]);
    shell.set_state(
        "light.dining_room_tall_lamp",
        "off",
        &[("brightness", "null")],
    );
    shell.set_state("light.dining_room_candle_light_left", "on", &[]);
    shell.set_state("light.dining_room_candle_light_right", "off", &[]);
    let view = shell.view();
    let tiles = view.tiles();
    let steps: Vec<bool> = tiles.iter().take(4).map(|tile| tile.steps).collect();
    assert_eq!(steps, [true, true, false, false]);
    assert_eq!(tiles[0].detail, "100%");
    assert_all_match(
        &check("dimmable-and-onoff", &view)
            .into_iter()
            .collect::<Vec<_>>(),
    );
}

#[test]
fn a_low_battery_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Battery, "/room/living_room");
    shell.send(Event::BatteryChanged(Some(12)));
    let view = shell.view();
    assert!(!view.asleep);
    // The view rounds the level to the nearest 5%.
    assert_eq!(view.battery, Some(10));
    assert_all_match(&check("battery-low", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_stand_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
    shell.send(Event::BatteryChanged(Some(78)));
    let view = shell.view();
    assert_eq!(view.power, PowerSource::Stand);
    assert_eq!(view.battery, Some(80));
    assert_all_match(
        &check("stand-charging", &view)
            .into_iter()
            .collect::<Vec<_>>(),
    );
}

/// Returns a tap on `control`.
const fn tap(control: Control) -> Event {
    Event::Tapped(Some(control))
}

#[test]
fn pending_tiles_and_a_pending_scene_match_their_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    shell.set_state("light.dining_room_lights", "on", &[("brightness", "255")]);
    shell.set_state("light.dining_room_candle_light_right", "off", &[]);
    shell.set_state(
        "climate.dining_room_thermostat",
        "heat",
        &[("current_temperature", "20.3"), ("temperature", "19.5")],
    );
    // The fixed shell does not answer service calls, so each tap waits.
    shell.send(tap(Control::Minus(0)));
    shell.send(tap(Control::Tile(3)));
    shell.send(tap(Control::Plus(4)));
    shell.send(tap(Control::Scene(0)));
    let view = shell.view();
    let tiles = view.tiles();
    assert_eq!(tiles[0].state, TileState::Pending);
    assert!(tiles[0].steps);
    assert_eq!(tiles[0].detail, "100%");
    assert_eq!(tiles[3].state, TileState::Pending);
    assert_eq!(tiles[3].text, "Off");
    // The thermostat keeps the target of Home Assistant until it confirms
    // the step.
    assert_eq!(tiles[4].state, TileState::Pending);
    assert!(tiles[4].steps);
    assert_eq!(tiles[4].text, "20.3°");
    assert_eq!(tiles[4].detail, "Set 19.5°");
    assert!(view.scenes()[0].pending);
    assert_all_match(&check("pending", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn a_pending_all_lights_toggle_matches_its_reference_image() {
    let path = PAGES[0].route().to_path();
    let mut shell = FixedShell::start(PowerSource::Stand, &path);
    shell.send(tap(Control::AllLights));
    let view = shell.view();
    let index = view.index().expect("the first page is the home page");
    let all_lights = index.all_lights.as_ref().expect("the toggle");
    assert_eq!(all_lights.state, TileState::Pending);
    assert_all_match(
        &check("pending-all-lights", &view)
            .into_iter()
            .collect::<Vec<_>>(),
    );
}

#[test]
fn a_tap_from_off_to_pending_is_partial_and_the_confirm_is_a_clean_refresh() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    shell.set_state("light.dining_room_candle_light_right", "off", &[]);
    let off = shell.view();
    let mut shown = Framebuffer::new();
    let Ok(()) = ui::draw(&off, &mut shown);
    let mut refresher = Refresher::default();
    assert_eq!(
        refresher.decide(off.full_refresh_seq, &shown, &shown),
        Refresh::Full
    );

    // The pattern has black and white pixels only, so the panel does not
    // flash.
    shell.send(tap(Control::Tile(3)));
    let pending = shell.view();
    assert_eq!(pending.tiles()[3].state, TileState::Pending);
    assert_eq!(pending.full_refresh_seq, off.full_refresh_seq);
    let mut next = Framebuffer::new();
    let Ok(()) = ui::draw(&pending, &mut next);
    assert!(matches!(
        refresher.decide(pending.full_refresh_seq, &shown, &next),
        Refresh::Partial(_)
    ));

    // Home Assistant confirms the tap, and the light is on. The change from
    // the pattern removes its dots, so the refresh is clean, with no flash.
    let mut on = pending.clone();
    if let PageView::Room(room) = &mut on.page {
        room.tiles[3].state = TileState::On;
        room.tiles[3].text = "On".into();
    }
    let mut last = Framebuffer::new();
    let Ok(()) = ui::draw(&on, &mut last);
    assert!(matches!(
        refresher.decide(on.full_refresh_seq, &next, &last),
        Refresh::Clean { .. }
    ));
    let panel = Rectangle::new(Point::zero(), Size::new(ui::WIDTH, ui::HEIGHT));
    assert!(!ui::has_grey(&next, panel));
}

#[test]
fn a_scene_button_that_leaves_the_pattern_is_a_clean_refresh_of_the_button() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    shell.send(tap(Control::Scene(0)));
    let pending = shell.view();
    assert!(pending.scenes()[0].pending);
    let mut shown = Framebuffer::new();
    let Ok(()) = ui::draw(&pending, &mut shown);
    let mut refresher = Refresher::default();
    let _ = refresher.decide(pending.full_refresh_seq, &shown, &shown);

    // The button has text and a border on the pattern, so not each cell has
    // a dot. The button still has enough dots for a clean refresh.
    let mut done = pending.clone();
    if let PageView::Room(room) = &mut done.page {
        room.scenes[0].pending = false;
    }
    let mut next = Framebuffer::new();
    let Ok(()) = ui::draw(&done, &mut next);
    let Refresh::Clean { area, dots } = refresher.decide(done.full_refresh_seq, &shown, &next)
    else {
        panic!("not a clean refresh");
    };
    let button = ui::layout::scene(0).expect("the first scene button");
    assert_eq!(area.intersection(&button), area);
    assert_eq!(dots.intersection(&area), dots);
}

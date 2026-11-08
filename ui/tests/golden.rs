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

use embedded_graphics::pixelcolor::Gray4;
use embedded_graphics::prelude::*;
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::{Event, PowerSource, TileState, ViewModel, fixtures::FixedShell, pages::PAGES};

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
            assert_eq!(view.updated, "Live · 10:41");
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
    assert_eq!(view.updated, "Updated 10:41");
    assert_all_match(&check("asleep", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_offline_page_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/dining_room");
    shell.drop_connection();
    let view = shell.view();
    assert!(!view.asleep);
    assert_eq!(view.updated, "Updated 10:41");
    assert_all_match(&check("offline", &view).into_iter().collect::<Vec<_>>());
}

#[test]
fn the_dry_run_page_matches_its_reference_image() {
    let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
    shell.send(Event::SetReadOnly(true));
    let view = shell.view();
    assert!(view.read_only);
    assert_eq!(view.status, "Dry run · Online, on stand");
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

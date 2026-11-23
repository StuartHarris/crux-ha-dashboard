//! Tests of the hourly report on battery, and of the draw rules that the
//! report keeps. The core runs in the test shell against the fake broker.

use alloc::{string::String, vec::Vec};

use super::{flows::house, shell::Shell, start_at};
use crate::PowerSource;

/// Starts the house on the stand on the living room page, and clears the
/// logs of the shell.
fn living_room_on_the_stand() -> Shell {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    shell.take_log();
    shell.take_logs();
    shell
}

/// Returns the number of `Render` requests in `log`.
fn renders(log: &[String]) -> usize {
    log.iter().filter(|entry| *entry == "Render").count()
}

#[test]
fn a_change_on_another_page_draws_nothing_while_awake() {
    let mut shell = living_room_on_the_stand();
    let frames = shell.frames.len();

    // The thermostat is on the dining room page.
    shell
        .broker
        .set_state("climate.dining_room_thermostat", "off", &[]);
    shell.run();
    let log: Vec<String> = shell.take_log();
    assert_eq!(renders(&log), 0, "{log:?}");
    assert_eq!(shell.frames.len(), frames);

    // A change on this page draws.
    shell
        .broker
        .set_state("light.living_room_rocket_lamp", "off", &[]);
    shell.run();
    assert_eq!(renders(&shell.take_log()), 1);
    assert_eq!(shell.frames.len(), frames + 1);
}

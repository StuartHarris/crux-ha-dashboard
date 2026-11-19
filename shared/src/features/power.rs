//! The power feature: the power policy.
//!
//! - On the stand, the device stays awake and connected.
//! - On battery, the device connects when it wakes. After [`IDLE_TIMEOUT`]
//!   without a touch, it sleeps in steps, and each step waits for the
//!   answer of the shell: publish `asleep` and disconnect, then turn Wi-Fi
//!   off, then draw the last image with the time of the last state message
//!   and sleep until a touch.
//! - Each sleep has an id. A tap or a move to the stand wakes the device
//!   during the steps, and the core then ignores the answers of the steps and
//!   of `Sleep`.

use alloc::vec;
use core::time::Duration;

use crux_time::clock::Time;

use crate::{
    Cmd, Event, Model,
    capabilities::{
        mqtt::Mqtt,
        network::Network,
        power::{Power, PowerSource, Wake, WakeSource},
    },
    features::{connection, refresh::redraw, rooms},
    view::rounded_level,
};

/// On battery, the time without a touch after which the device sleeps.
///
/// No other timer of the core waits for this time, so a shell can find the
/// idle timer by its duration, for example to end the idle time early.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The power source and the sleep state.
#[derive(Debug, Default)]
pub struct State {
    /// The power source.
    pub source: PowerSource,
    /// The exact charge of the battery in percent, or `None` if the shell
    /// did not read it yet.
    pub battery: Option<u8>,
    /// The device is asleep, or goes to sleep now.
    pub asleep: bool,
    /// The id of the newest idle timer.
    pub idle: u32,
    /// The core waits one more idle period, because service calls wait in
    /// the outbox.
    pub extended: bool,
    /// The id of the newest sleep. The core ignores the answers of an older
    /// sleep.
    pub sleep: u32,
}

/// The `Sleep` request of the sleep `id` ended with `wake`.
///
/// If the device still sleeps for that request, it wakes and connects again.
/// A touch also acts as a tap on its control. A change of the power source
/// comes first as `PowerSourceChanged`, so `Wake::PowerSource` wakes the
/// device only if the source is now `Stand`. On battery, the core ignores it.
pub fn woke(id: u32, wake: Wake, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    match wake {
        Wake::Touch(control) => rooms::tapped(control, model),
        Wake::PowerSource if model.power.source != PowerSource::Stand => Cmd::done(),
        Wake::Timer | Wake::PowerSource => wake_up(model),
    }
}

/// The MQTT connection of the sleep `id` closed: turn Wi-Fi off.
pub fn offline(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    Network::wifi_off().then_send(move |()| Event::RadioOff(id))
}

/// The radio of the sleep `id` is off: draw the last image and sleep.
pub fn radio_off(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    redraw(model, true).and(
        Power::sleep(vec![WakeSource::Touch])
            .then_send(move |wake| Event::Woke { sleep: id, wake }),
    )
}

/// Wakes the device from a sleep, or from the steps before it, and connects
/// again.
pub fn wake_up(model: &mut Model) -> Cmd {
    if !model.power.asleep {
        return restart_idle_timer(model);
    }
    model.power.asleep = false;
    connection::restart(model);
    redraw(model, true)
        .and(connection::go_online(model))
        .and(restart_idle_timer(model))
}

/// The device moved to the stand or off the stand.
pub fn source_changed(source: PowerSource, model: &mut Model) -> Cmd {
    if source == model.power.source {
        return Cmd::done();
    }
    model.power.source = source;
    match source {
        PowerSource::Stand => {
            let was_asleep = model.power.asleep;
            model.power.asleep = false;
            // The core ignores the idle timer that runs now.
            model.power.idle = model.power.idle.wrapping_add(1);
            connection::restart(model);
            redraw(model, was_asleep).and(connection::go_online(model))
        }
        PowerSource::Battery => redraw(model, false).and(restart_idle_timer(model)),
    }
}

/// The charge of the battery changed. The core keeps the exact level and
/// draws the level of the view.
///
/// The view gives the level rounded to the nearest
/// [`BATTERY_STEP`](crate::view::BATTERY_STEP). If the rounded level does
/// not change, the core does not draw. Thus each change of 1% does not use
/// a partial refresh.
///
/// The level is a fact, not a touch, so it does not wake the device and it
/// does not start the idle timer again. While the device sleeps, the core
/// keeps the level but does not draw, as for a change of the dry run. The
/// wake draws the level.
pub fn battery_changed(level: Option<u8>, model: &mut Model) -> Cmd {
    let shown = model.power.battery.map(rounded_level);
    model.power.battery = level;
    if level.map(rounded_level) == shown {
        return Cmd::done();
    }
    if model.power.asleep {
        return Cmd::done();
    }
    redraw(model, false)
}

/// Starts the idle timer on battery. The core then ignores the timer that
/// ran before.
pub fn restart_idle_timer(model: &mut Model) -> Cmd {
    model.power.idle = model.power.idle.wrapping_add(1);
    model.power.extended = false;
    if model.power.source != PowerSource::Battery || model.power.asleep {
        return Cmd::done();
    }
    let id = model.power.idle;
    let (timer, _handle) = Time::notify_after(IDLE_TIMEOUT);
    timer.then_send(move |_| Event::IdleTimeout(id))
}

/// The idle timer expired: sleep.
///
/// If service calls wait in the outbox, the core first waits one more idle
/// period for the connection.
pub fn idle_timeout(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.idle || model.power.source != PowerSource::Battery || model.power.asleep {
        return Cmd::done();
    }
    if !model.connection.outbox.is_empty() && !model.power.extended {
        let timer = restart_idle_timer(model);
        model.power.extended = true;
        return timer;
    }
    sleep(model)
}

/// Starts the steps of a sleep: publishes `asleep` and disconnects. The
/// answer to `Disconnect` starts the next step ([`offline`]).
///
/// The core drops the service calls that wait in the outbox, and the view
/// shows how many. The last image shows the time of the last state message.
fn sleep(model: &mut Model) -> Cmd {
    model.power.asleep = true;
    model.power.sleep = model.power.sleep.wrapping_add(1);
    let id = model.power.sleep;
    model.unsent += model.connection.outbox.len();
    connection::go_offline(model).then(Mqtt::disconnect().then_send(move |()| Event::Offline(id)))
}

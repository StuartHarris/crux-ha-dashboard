//! The core task: it owns the shell, and so the core. No other task calls
//! the core, because its locks panic if two tasks use it at the same time.

use core::time::Duration;

use crux_time::operation::NotifyAfter;
use logic::{
    shell::{self, Job, Shell, TimerOp},
    sleep::Plan,
    wifi::WifiRequest,
};
use shared::{
    Event, PowerSource,
    capabilities::network::{WifiOff, WifiOn},
};

use crate::{MQTT, RENDER, SIGNALS, SLEEP, TIMERS, WIFI, config::CONFIG, timers::Request};

/// Starts the core with the power source `source`, then gives it each
/// signal from the other tasks.
pub async fn run(source: PowerSource) -> ! {
    let mut shell = Shell::new();
    if CONFIG.read_only {
        log::info!("core: dry run, so a tap publishes no service call");
    }
    // The firmware uses light sleep, which keeps the core in RAM, so a wake
    // does not start the core again.
    // TODO: if the firmware uses deep sleep in the future, give the path and
    // the time of the last update that it kept in RTC memory.
    let start = shell::start(&CONFIG, source, None, None);
    dispatch(shell.send(Event::Start(start)));
    loop {
        let signal = SIGNALS.pop().await;
        let sleeping = shell.is_sleeping();
        dispatch(shell.handle(signal));
        // The shell answered the `Sleep` request, so the display task stops
        // the sleep.
        if sleeping && !shell.is_sleeping() {
            SLEEP.signal(None);
        }
    }
}

/// Gives each job to the task that does it.
///
/// The display task runs the `Sleep` job. `RENDER` gets the last view model
/// before [`SLEEP`] gets the job, and the display task draws a view model that
/// waits before it sleeps.
fn dispatch(jobs: alloc::vec::Vec<Job>) {
    for job in jobs {
        match job {
            Job::Render(view) => RENDER.signal(view),
            Job::Mqtt { connection, op } => MQTT.push((connection, op)),
            Job::WifiOn { token, op: WifiOn } => WIFI.push(WifiRequest::On { token }),
            Job::WifiOff(WifiOff) => WIFI.push(WifiRequest::Off),
            Job::Sleep(sleep) => {
                log::info!("power: sleep until {:?}", sleep.wake_on);
                SLEEP.signal(Some(Plan::new(&sleep)));
            }
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(NotifyAfter { id, duration }),
            } => TIMERS.push(Request::Start {
                id,
                token,
                after: Duration::from(duration),
            }),
            Job::Timer {
                token,
                op: TimerOp::ClearTimer(_),
            } => TIMERS.push(Request::Cancel { token }),
        }
    }
}

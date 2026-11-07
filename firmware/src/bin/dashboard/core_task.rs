//! The core task: it owns the shell, and so the core. No other task calls
//! the core, because its locks panic if two tasks use it at the same time.

use core::time::Duration;

use crux_time::operation::NotifyAfter;
use logic::{
    shell::{self, Job, Shell, TimerOp},
    wifi::WifiRequest,
};
use shared::{
    Event, PowerSource,
    capabilities::network::{WifiOff, WifiOn},
};

use crate::{MQTT, RENDER, SIGNALS, TIMERS, WIFI, clock, config::CONFIG, timers::Request};

/// Starts the core, then gives it each signal from the other tasks.
pub async fn run() -> ! {
    let mut shell = Shell::new(clock::now);
    if CONFIG.read_only {
        log::info!("core: dry run, so a tap publishes no service call");
    }
    // TODO(6): read the VBUS status of the BQ25896 for the power source, and
    // send `PowerSourceChanged` when it changes.
    // TODO(6): after a deep sleep, give the path and the time of the last
    // update that the firmware kept in RTC memory.
    let start = shell::start(&CONFIG, PowerSource::Stand, None, None);
    dispatch(shell.send(Event::Start(start)));
    loop {
        let signal = SIGNALS.pop().await;
        dispatch(shell.handle(signal));
    }
}

/// Gives each job to the task that does it.
fn dispatch(jobs: alloc::vec::Vec<Job>) {
    for job in jobs {
        match job {
            Job::Render(view) => RENDER.signal(view),
            Job::Mqtt { connection, op } => MQTT.push((connection, op)),
            Job::WifiOn { token, op: WifiOn } => WIFI.push(WifiRequest::On { token }),
            Job::WifiOff(WifiOff) => WIFI.push(WifiRequest::Off),
            Job::Sleep(sleep) => {
                // TODO(6): a deep sleep with wake on the GT911 INT (GPIO3)
                // and the RTC (GPIO2), after the panel, the front light and
                // the touch controller are off. In this phase the device
                // stays awake. The next tap goes to the core, which wakes
                // because it sleeps.
                log::info!(
                    "power: sleep until {:?}. Tap the panel to wake.",
                    sleep.wake_on
                );
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

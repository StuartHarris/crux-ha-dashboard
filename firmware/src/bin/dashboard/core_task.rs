//! The core task: it owns the shell, and so the core. No other task calls
//! the core, because its locks panic if two tasks use it at the same time.
//!
//! The core runs the MQTT session, so each chunk of bytes from the broker is
//! an update. The task measures the time of each update. It logs a warning for
//! a slow update (`logic::stream::SLOW_UPDATE`), and at `WifiQuiet` it logs
//! the updates since the core opened the stream.

use core::time::Duration;

use crux_time::operation::NotifyAfter;
use embassy_time::Instant;
use logic::{
    shell::{self, Job, Shell, StreamOp, TimerOp},
    sleep::Plan,
    stream::UpdateStats,
    wifi::WifiRequest,
};
use shared::{
    Event, PowerSource,
    capabilities::network::{WifiOff, WifiOn},
};

use crate::{
    HEALTH, LIGHT, RENDER, SIGNALS, SLEEP, STREAM, TIMERS, WIFI, config::CONFIG, timers::Request,
};

/// Starts the core with the power source `source`, then gives it each
/// signal from the other tasks.
///
/// If `display` is `false`, no display task runs. The core task then
/// answers each `FrontLight` job itself, so the sleep steps continue.
pub async fn run(source: PowerSource, display: bool) -> ! {
    let mut shell = Shell::new();
    if CONFIG.read_only {
        log::info!("core: dry run, so a tap publishes no service call");
    }
    // The firmware uses light sleep, which keeps the core in RAM, so a wake
    // does not start the core again.
    // TODO: if the firmware uses deep sleep in the future, give the path and
    // the time of the last update that it kept in RTC memory.
    let start = shell::start(&CONFIG, source, None, None);
    let mut stats = UpdateStats::default();
    let started = Instant::now();
    let jobs = shell.send(Event::Start(start));
    record(&mut stats, started);
    dispatch(jobs, display, &mut stats);
    loop {
        let signal = SIGNALS.pop().await;
        let sleeping = shell.is_sleeping();
        let started = Instant::now();
        let jobs = shell.handle(signal);
        record(&mut stats, started);
        dispatch(jobs, display, &mut stats);
        // The shell answered the `Sleep` request, so the display task stops
        // the sleep.
        if sleeping && !shell.is_sleeping() {
            SLEEP.signal(None);
        }
    }
}

/// Records the time of an update that started at `started`, and logs a
/// warning if the update was slow.
fn record(stats: &mut UpdateStats, started: Instant) {
    let took = Duration::from_micros(started.elapsed().as_micros());
    if stats.record(took) {
        log::warn!("core: an update took {} ms", took.as_millis());
    }
}

/// Gives each job to the task that does it.
///
/// The display task runs the `Sleep` job. `RENDER` gets the last view model
/// before [`SLEEP`] gets the job, and the display task draws a view model that
/// waits before it sleeps.
///
/// The display task also sets the front light, and answers with
/// `Signal::Light`. If `display` is `false`, this function answers at once.
///
/// A `Close` with `clean: false` makes the link suspect in `HEALTH` before
/// the core can ask for Wi-Fi again. A `Close` with `clean: true` is the
/// first network step of a sleep, so a scan for a better access point stops.
/// An `Open` starts new update statistics, and `WifiQuiet` logs them.
fn dispatch(jobs: alloc::vec::Vec<Job>, display: bool, stats: &mut UpdateStats) {
    for job in jobs {
        match job {
            Job::Render(view) => RENDER.signal(view),
            Job::Stream { connection, op } => {
                match &op {
                    StreamOp::Open(_) => *stats = UpdateStats::default(),
                    StreamOp::Close(close) if close.clean => {
                        log::info!("stream: the core closes {connection}: {}", close.reason);
                        WIFI.push(WifiRequest::Hold);
                    }
                    StreamOp::Close(close) => {
                        log::warn!(
                            "stream: the session on {connection} failed: {}",
                            close.reason
                        );
                        HEALTH.lock(|health| health.borrow_mut().session_failed());
                    }
                    StreamOp::Write(_) => {}
                }
                STREAM.push((connection, op));
            }
            Job::WifiOn { token, op: WifiOn } => WIFI.push(WifiRequest::On { token }),
            Job::WifiOff(WifiOff) => WIFI.push(WifiRequest::Off),
            Job::WifiQuiet(_) => {
                let UpdateStats { count, total, max } = stats.take();
                log::info!(
                    "core: {count} updates since the stream opened, {} ms in total, the longest {} ms",
                    total.as_millis(),
                    max.as_millis()
                );
                WIFI.push(WifiRequest::Roam);
            }
            Job::FrontLight { token, level } if display => LIGHT.push((token, level)),
            Job::FrontLight { token, .. } => SIGNALS.push(shell::Signal::Light { token }),
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

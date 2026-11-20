//! The core task: it owns the shell, and so the core. No other task calls
//! the core, because its locks panic if two tasks use it at the same time.
//!
//! The core runs the MQTT session, so each chunk of bytes from the broker is
//! an update. The task measures the time of each update. It logs a warning for
//! a slow update (`logic::stream::SLOW_UPDATE`). At the `Close` of the
//! stream that the core opened last, it logs the updates since that `Open`.

use core::time::Duration;

use crux_time::operation::NotifyAfter;
use embassy_time::Instant;
use logic::{
    shell::{self, Job, Shell, StreamOp, TimerOp},
    sleep::Plan,
    stream::UpdateStats,
};
use shared::{Event, PowerSource, capabilities::log::Level};

use crate::{LIGHT, RADIO, RENDER, SIGNALS, SLEEP, STREAM, TIMERS, config::CONFIG, radio, timers};

/// The update statistics of the stream that the core opened last.
#[derive(Default)]
struct Stats {
    /// The connection id of the stream, until its first `Close`.
    connection: Option<u32>,
    /// The updates since the `Open` of the stream.
    updates: UpdateStats,
}

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
    let mut stats = Stats::default();
    let started = Instant::now();
    let jobs = shell.send(Event::Start(start));
    record(&mut stats.updates, started);
    dispatch(jobs, display, &mut stats);
    loop {
        let signal = SIGNALS.pop().await;
        let sleeping = shell.is_sleeping();
        let started = Instant::now();
        let jobs = shell.handle(signal);
        record(&mut stats.updates, started);
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
/// The radio task does the radio jobs and the `Restart` job, and answers
/// with `Signal::Radio`.
///
/// This task writes each line of the log of the core as `{area}: {message}`,
/// with no prefix of its own. Thus a `wifi:` line comes between the lines of
/// the radio task.
///
/// An `Open` starts new update statistics. The first `Close` of the same
/// connection logs them. A `Close` of an older connection, or a second
/// `Close`, logs no statistics.
fn dispatch(jobs: alloc::vec::Vec<Job>, display: bool, stats: &mut Stats) {
    for job in jobs {
        match job {
            Job::Render(view) => RENDER.signal(view),
            Job::Stream { connection, op } => {
                match &op {
                    StreamOp::Open(_) => {
                        *stats = Stats {
                            connection: Some(connection),
                            updates: UpdateStats::default(),
                        };
                    }
                    StreamOp::Close(close) => {
                        if close.clean {
                            log::info!("stream: the core closes {connection}: {}", close.reason);
                        } else {
                            log::warn!(
                                "stream: the session on {connection} failed: {}",
                                close.reason
                            );
                        }
                        if stats.connection == Some(connection) {
                            stats.connection = None;
                            let UpdateStats { count, total, max } = stats.updates.take();
                            log::info!(
                                "core: {count} updates since the stream opened, {} ms in total, the longest {} ms",
                                total.as_millis(),
                                max.as_millis()
                            );
                        }
                    }
                    StreamOp::Write(_) => {}
                }
                STREAM.push((connection, op));
            }
            Job::Radio { token, stop, op } => RADIO.push(radio::Request::Job { token, stop, op }),
            Job::Restart(restart) => RADIO.push(radio::Request::Restart(restart)),
            Job::Log(line) => match line.level {
                Level::Info => log::info!("{line}"),
                Level::Warn => log::warn!("{line}"),
            },
            Job::FrontLight { token, level } if display => LIGHT.push((token, level)),
            Job::FrontLight { token, .. } => SIGNALS.push(shell::Signal::Light { token }),
            Job::Sleep(sleep) => {
                log::info!("power: sleep until {:?}", sleep.wake_on);
                SLEEP.signal(Some(Plan::new(&sleep)));
            }
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(NotifyAfter { id, duration }),
            } => TIMERS.push(timers::Request::Start {
                id,
                token,
                after: Duration::from(duration),
            }),
            Job::Timer {
                token,
                op: TimerOp::ClearTimer(_),
            } => TIMERS.push(timers::Request::Cancel { token }),
        }
    }
}

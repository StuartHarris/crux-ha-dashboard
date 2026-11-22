//! The core task: it owns the shell, and so the core. No other task calls
//! the core, because its locks panic if two tasks use it at the same time.
//!
//! The core runs the MQTT session, so each chunk of bytes from the broker is
//! an update. The task measures the time of each update. It logs a warning for
//! a slow update (`logic::stream::SLOW_UPDATE`). At the `Close` of the
//! stream that the core opened last, it logs the updates since that `Open`.
//!
//! The task also takes each warn and error line that the logger keeps
//! (`dashboard_firmware::logger`), and sends it to the core as
//! `Event::ShellLogged`.

use core::time::Duration;

use crux_time::operation::NotifyAfter;
use dashboard_firmware::{
    board::{self, uptime},
    boot, logger,
};
use embassy_futures::select::{Either, select};
use embassy_time::Instant;
use logic::{
    boot::Taken,
    forward::{CORE_TARGET, PENDING},
    shell::{self, Job, Shell, StreamOp, TimerOp},
    sleep::Plan,
    stream::UpdateStats,
};
use shared::{Connection, Event, PowerSource, capabilities::log::Level};

use crate::{
    IN_FLIGHT, LIGHT, RADIO, RENDER, SIGNALS, SLEEP, STREAM, TIMERS, config::CONFIG, radio, timers,
};

/// The update statistics of the stream that the core opened last.
#[derive(Default)]
struct Stats {
    /// The connection id of the stream, until its first `Close`.
    connection: Option<u32>,
    /// The updates since the `Open` of the stream.
    updates: UpdateStats,
}

/// Starts the core with the power source `source`, then gives it each
/// signal from the other tasks, and each line that the logger keeps.
///
/// The lines of the logger do not count in the update statistics. A slow
/// update logs a warning, which the logger keeps, so a slow line would make
/// one more line.
///
/// `taken` has the boot facts and the lines from before the restart, which
/// `main` read from RTC memory ([`boot::read`]). They go in `Start`.
///
/// If `display` is `false`, no display task runs. The core task then
/// answers each `FrontLight` job itself, so the sleep steps continue.
pub async fn run(source: PowerSource, display: bool, taken: &Taken) -> ! {
    let mut shell = Shell::new();
    if CONFIG.read_only {
        log::info!("core: dry run, so a tap publishes no service call");
    }
    // The firmware uses light sleep, which keeps the core in RAM, so a wake
    // does not start the core again.
    // TODO: if the firmware uses deep sleep in the future, give the path and
    // the time of the last update that it kept in RTC memory.
    let start = shell::start(
        &CONFIG,
        board::firmware(),
        taken.boot,
        taken.lines(),
        source,
        None,
        None,
    );
    let mut stats = Stats::default();
    let started = Instant::now();
    let jobs = shell.send(Event::Start(start));
    record(&mut stats.updates, started);
    dispatch(jobs, display, &mut stats);
    loop {
        if let Either::First(signal) = select(SIGNALS.pop(), logger::wait()).await {
            let sleeping = shell.is_sleeping();
            // The answer to a `Log` job is not real work, so it does not count.
            let counts = signal.counts();
            let ack = signal.needs_ack();
            let started = Instant::now();
            let jobs = shell.handle(signal);
            if counts {
                record(&mut stats.updates, started);
            }
            dispatch(jobs, display, &mut stats);
            // The shell answered the `Sleep` request, so the display task stops
            // the sleep.
            if sleeping && !shell.is_sleeping() {
                SLEEP.signal(None);
            }
            // Last, so the display task sees the end of the `Sleep` job
            // before it can sleep (`logic::sleep::InFlight`).
            if ack {
                IN_FLIGHT.handled();
            }
        }
        // At most one full queue. A line that the logger keeps after this
        // wakes the task again. A line of the shell does not wake the core.
        for _ in 0..PENDING {
            let Some(event) = logger::take() else {
                break;
            };
            let jobs = shell.handle(shell::Signal::Event(event));
            dispatch(jobs, display, &mut stats);
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
/// with no prefix of its own, at the level of the line. A `Debug` line
/// prints because `ESP_LOG` has `core=debug`. Thus a `wifi:` line comes
/// between the lines of the radio task. The line has the log target `core`,
/// which the printed line does not show. The logger does not keep a line
/// with this target for the core, because the core keeps it already. Then
/// the task answers at once with `Signal::Logged` and the time of the line
/// ([`uptime`]). A `Clock` job gets the same answer at once as
/// `Signal::Clock`.
///
/// An `Open` starts new update statistics. The first `Close` of the same
/// connection logs them. A `Close` of an older connection, or a second
/// `Close`, logs no statistics.
fn dispatch(jobs: alloc::vec::Vec<Job>, display: bool, stats: &mut Stats) {
    for job in jobs {
        match job {
            Job::Render(view) => {
                // The view shows `Online` after an MQTT connect, so the
                // start works and a later panic restarts the chip again.
                if view.connection == Connection::Online {
                    boot::connected();
                }
                RENDER.signal(view);
            }
            Job::Stream { connection, op } => {
                match &op {
                    StreamOp::Open(_) => {
                        *stats = Stats {
                            connection: Some(connection),
                            updates: UpdateStats::default(),
                        };
                    }
                    StreamOp::Close(close) => {
                        // The core writes one warning with the reason of a
                        // failed session, so this line is for the serial log
                        // only.
                        if close.clean {
                            log::info!("stream: the core closes {connection}: {}", close.reason);
                        } else {
                            log::info!(
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
            Job::Log { token, line } => {
                match line.level {
                    Level::Debug => log::debug!(target: CORE_TARGET, "{line}"),
                    Level::Info => log::info!(target: CORE_TARGET, "{line}"),
                    Level::Warn => log::warn!(target: CORE_TARGET, "{line}"),
                    // The core writes lines at `Info`, `Warn` and `Debug`, never at
                    // `Error`. This arm keeps the match complete.
                    Level::Error => log::error!(target: CORE_TARGET, "{line}"),
                }
                SIGNALS.push(shell::Signal::Logged {
                    token,
                    at: uptime(),
                });
            }
            Job::Clock { token } => SIGNALS.push(shell::Signal::Clock {
                token,
                now: uptime(),
            }),
            Job::FrontLight { token, level } if display => LIGHT.push((token, level)),
            Job::FrontLight { token, .. } => SIGNALS.push(shell::Signal::Light { token }),
            Job::Sleep(sleep) => {
                // A `Sleep` job comes at each check of the power source, so
                // this line is at `Debug`.
                log::debug!("power: sleep until {:?}", sleep.wake_on);
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

//! The shell logic of the firmware: it owns the core, routes each effect to
//! the task that performs it, and gives the answers of the tasks to the core.
//!
//! The core has locks that it only tries to take, so one task owns the
//! [`Shell`]. The other tasks never see the core. They send a [`Signal`] to
//! the task of the shell, and the shell gives the task a [`Job`].
//!
//! The shell keeps the bookkeeping that the simulator shell also has:
//!
//! - Each stream has the connection id of the core. The stream task closes
//!   the open stream when a new `Open` replaces it. The shell ignores the
//!   items of a stream that ended.
//! - A `Write` ends at [`Signal::Written`] for its connection, in the order
//!   of the requests. At the end of the stream, the shell answers each
//!   `Write` of the stream that waits with `Failed`. If the stream ended
//!   before the request, the shell answers `Failed` at once.
//! - A `Close` ends at the end of the stream of its connection, after the
//!   `Closed` item. If the stream ended before the request, the shell answers
//!   at once.
//! - Each timer has a token. The core can use a timer id again after it clears
//!   the timer, so the shell ignores a signal with an old token.
//! - Each radio request has a token. The radio task answers with
//!   [`Signal::Radio`] and the token. The shell drops an answer with a token
//!   that no request waits for.
//! - The shell obeys the stop rule of the radio capability
//!   ([`shared::capabilities::radio`]). A new radio request stops each
//!   older radio request that waits. The shell answers each stopped request
//!   with `Stopped` (or `()`) itself, and the job of the new request tells
//!   the radio task which jobs to stop ([`Job::Radio`]). The shell then
//!   drops the late answers of those jobs.
//! - `Restart` is a notification. The shell gives it to a task as
//!   [`Job::Restart`].
//! - `Log` is a notification. The shell gives it to a task as [`Job::Log`],
//!   in the order of the effects. Thus a line of the log comes before the
//!   job of the next effect, for example before a `Restart`.
//! - `Sleep` ends at [`Signal::Woke`]. While it waits, a tap is the wake
//!   (`Wake::Touch` with the control at the point), and a change of the
//!   power source also ends it (`Wake::PowerSource`). [`Shell::is_sleeping`] tells
//!   the task that runs the sleep when the request ended.
//! - Each `FrontLight` request has a token. The task that drives the light
//!   answers with [`Signal::Light`] and the token when the light has the
//!   level. The answer is also for the older requests that wait.
//! - One batch of effects gives at most one `Render` job, with the view
//!   at the end of the batch. It is at the place of the last `Render` effect,
//!   so it stays before a `Sleep` that follows it.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};

use crux_core::{Request, capability::Operation};
use crux_time::{
    TimerId,
    operation::{ClearTimer, NotifyAfter},
};
use embedded_graphics::prelude::Point;
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, ViewModel,
    capabilities::{
        light::FrontLight,
        log::Log,
        power::{Sleep, Wake},
        radio::{Address, Join, Joined, Lease, Leave, Off, Restart, Scan, Scanned, StopScan},
        stream::{Close, Open, StreamItem, Write, Written},
    },
    mqtt::Credentials,
};

use crate::config::Config;

/// An answer or a fact from a task, for the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// An item from the stream of the connection with this id. `Closed` is
    /// the last item.
    Stream {
        /// The connection id of the core.
        connection: u32,
        /// The item.
        item: StreamItem,
    },
    /// The answer to the oldest `Write` job of the connection with this id.
    Written {
        /// The connection id of the core.
        connection: u32,
        /// The answer.
        outcome: Written,
    },
    /// The timer with this id and token expired.
    Timer {
        /// The id of the timer in the core.
        id: TimerId,
        /// The token of the timer in the shell.
        token: u32,
    },
    /// The answer to the radio job with this token. The shell drops an
    /// answer for a job that a newer job stopped.
    Radio {
        /// The token of the job.
        token: u32,
        /// The answer.
        answer: RadioAnswer,
    },
    /// A tap at this point on the panel. The shell sends it to the core as
    /// `Event::Tapped`, also on no control. While a `Sleep` request waits,
    /// the tap answers it with `Wake::Touch` and the control, in place of a
    /// tap.
    Tap(Point),
    /// The device woke from a real wake. The answer to the `Sleep` job.
    Woke(Wake),
    /// The front light has the level of the `FrontLight` job with this
    /// token. The answer is also for the older jobs that wait.
    Light {
        /// The token of the newest job that the answer is for.
        token: u32,
    },
    /// An event for the core, for example `PowerSourceChanged`.
    Event(Event),
}

/// A stream operation of the core, for the connection with the id in the
/// [`Job`]. Each variant holds the operation of the core, unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamOp {
    /// Open a stream. If a stream is open, close it first, and end it with
    /// `Closed` and the reason `"replaced"`. The stream task answers with the
    /// items of the new stream as [`Signal::Stream`], and the last item is
    /// always `Closed`.
    Open(Open),
    /// Send the bytes. The stream task answers with [`Signal::Written`].
    Write(Write),
    /// Close the stream. Stop a `Write` that waits, send a TCP FIN, send the
    /// bytes that wait for no more than 2 seconds, and abort the socket. Then
    /// end the stream with `Closed`. The shell answers the `Close` request at
    /// the `Closed` item, so the task sends no other answer.
    Close(Close),
}

/// A radio operation of the core. Each variant holds the operation of the
/// core, unchanged. Refer to [`shared::capabilities::radio`] for what the
/// radio task does for each one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadioOp {
    /// Associate with an access point. Answer with [`RadioAnswer::Joined`].
    Join(Join),
    /// Wait for an IP address. Answer with [`RadioAnswer::Lease`].
    Address(Address),
    /// Leave the access point. Answer with [`RadioAnswer::Done`].
    Leave(Leave),
    /// Scan all the channels. Answer with [`RadioAnswer::Scanned`].
    Scan(Scan),
    /// Stop a scan. Answer with [`RadioAnswer::Done`].
    StopScan(StopScan),
    /// Turn the radio off. Answer with [`RadioAnswer::Done`].
    Off(Off),
}

/// The answer of the radio task to a radio job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadioAnswer {
    /// The answer to `Join`.
    Joined(Joined),
    /// The answer to `Address`.
    Lease(Lease),
    /// The answer to `Scan`.
    Scanned(Scanned),
    /// The answer to `Leave`, `StopScan` or `Off`.
    Done,
}

/// A radio request of the core that waits for its answer.
enum RadioRequest {
    Join(Request<Join>),
    Address(Request<Address>),
    Leave(Request<Leave>),
    Scan(Request<Scan>),
    StopScan(Request<StopScan>),
    Off(Request<Off>),
}

/// A timer operation of the core. Each operation holds the id of the timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerOp {
    /// Start the timer. When it expires, send [`Signal::Timer`] with the id
    /// and the token.
    NotifyAfter(NotifyAfter),
    /// Stop the timer with the token of the [`Job`], if it runs. The shell
    /// also makes this operation to replace a timer that has the same id. In
    /// that case no core operation is behind it.
    ClearTimer(ClearTimer),
}

/// Work for a task. A job holds the operations of the core unchanged, and
/// adds only what the shell owns: the radio token, the light token and the
/// timer token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// Draw the view model on the panel. The shell decides this job, so no
    /// core operation is behind it. The view model is in a box, because it
    /// is much larger than the other jobs.
    Render(Box<ViewModel>),
    /// Do an operation on the stream of a connection.
    Stream {
        /// The connection id of the core. It is the same as the id in the
        /// operation.
        connection: u32,
        /// The operation.
        op: StreamOp,
    },
    /// Do a radio operation. Answer with [`Signal::Radio`] and the token.
    ///
    /// Stop the jobs with the tokens in `stop` first, if they run or wait,
    /// and do not answer them. The shell already answered them.
    Radio {
        /// The token of the job.
        token: u32,
        /// The tokens of the older jobs that this job stops.
        stop: Vec<u32>,
        /// The operation.
        op: RadioOp,
    },
    /// Write the reason to the log, wait for the log to go, and restart the
    /// chip. No answer.
    Restart(Restart),
    /// Write the line to the log as `{area}: {message}`, at its level. No
    /// answer.
    Log(Log),
    /// Draw the view model that waits, then put the device to sleep. Answer
    /// with [`Signal::Woke`] at the first real wake. After another wake,
    /// sleep again.
    Sleep(Sleep),
    /// Set the front light to `level` percent. Answer with
    /// [`Signal::Light`] and the token when the light has the level.
    FrontLight {
        /// The token of the request.
        token: u32,
        /// The level in percent, from 0 to 100. The value 0 turns the light
        /// off.
        level: u8,
    },
    /// Do an operation on a timer.
    Timer {
        /// The token of the timer in the shell.
        token: u32,
        /// The operation.
        op: TimerOp,
    },
}

/// A timer that runs.
struct Timer {
    id: TimerId,
    token: u32,
    request: Request<NotifyAfter>,
}

/// The shell logic. One task owns it.
pub struct Shell {
    core: Core<Dashboard>,
    /// The `Open` requests whose streams did not end, from the oldest, with
    /// their connection ids.
    streams: Vec<(u32, Request<Open>)>,
    /// The `Write` requests that wait for their answers, from the oldest,
    /// with their connection ids.
    writes: Vec<(u32, Request<Write>)>,
    /// The `Close` requests that wait for the end of the stream of their
    /// connection.
    closes: Vec<(u32, Request<Close>)>,
    timers: Vec<Timer>,
    last_token: u32,
    /// The radio requests that wait for an answer, with their tokens, from
    /// the oldest.
    radio: Vec<(u32, RadioRequest)>,
    last_radio_token: u32,
    /// The `Sleep` request that waits for a wake.
    sleep: Option<Request<Sleep>>,
    /// The `FrontLight` requests that wait for an answer, with their tokens.
    lights: Vec<(u32, Request<FrontLight>)>,
    last_light_token: u32,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    /// Makes a shell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: Core::new(),
            streams: Vec::new(),
            writes: Vec::new(),
            closes: Vec::new(),
            timers: Vec::new(),
            last_token: 0,
            radio: Vec::new(),
            last_radio_token: 0,
            sleep: None,
            lights: Vec::new(),
            last_light_token: 0,
        }
    }

    /// Returns `true` while a `Sleep` request waits for its answer.
    #[must_use]
    pub const fn is_sleeping(&self) -> bool {
        self.sleep.is_some()
    }

    /// Returns the view of the core now.
    #[must_use]
    pub fn view(&self) -> ViewModel {
        self.core.view()
    }

    /// Sends an event to the core and returns the work for the tasks.
    pub fn send(&mut self, event: Event) -> Vec<Job> {
        let effects = self.core.process_event(event);
        self.process(effects)
    }

    /// Gives a signal from a task to the core and returns the work for the
    /// tasks.
    pub fn handle(&mut self, signal: Signal) -> Vec<Job> {
        match signal {
            Signal::Stream { connection, item } => self.stream(connection, item),
            Signal::Written {
                connection,
                outcome,
            } => {
                let Some(index) = self.writes.iter().position(|(id, _)| *id == connection) else {
                    return Vec::new();
                };
                let (_, mut request) = self.writes.remove(index);
                let effects = resolve(&self.core, &mut request, outcome);
                self.process(effects)
            }
            Signal::Timer { id, token } => {
                let Some(index) = self
                    .timers
                    .iter()
                    .position(|timer| timer.id == id && timer.token == token)
                else {
                    return Vec::new();
                };
                let mut timer = self.timers.remove(index);
                let effects = resolve(&self.core, &mut timer.request, id);
                self.process(effects)
            }
            Signal::Radio { token, answer } => {
                let effects = self.answer_radio(token, answer);
                self.process(effects)
            }
            Signal::Tap(point) => {
                // The last image shows the page, so the point has the same
                // control while the device sleeps.
                let control = ui::hit_test(&self.view(), point);
                if self.is_sleeping() {
                    return self.wake(Wake::Touch(control));
                }
                self.send(Event::Tapped(control))
            }
            Signal::Event(event) => {
                let power = matches!(event, Event::PowerSourceChanged(_));
                let mut jobs = self.send(event);
                if power && self.is_sleeping() {
                    jobs.extend(self.wake(Wake::PowerSource));
                }
                jobs
            }
            Signal::Woke(wake) => self.wake(wake),
            Signal::Light { token } => {
                let (answered, waiting) = core::mem::take(&mut self.lights)
                    .into_iter()
                    .partition(|(request, _)| *request <= token);
                self.lights = waiting;
                let mut effects = Vec::new();
                for (_, mut request) in answered {
                    effects.extend(resolve(&self.core, &mut request, ()));
                }
                self.process(effects)
            }
        }
    }

    /// Gives an item of the stream of `connection` to the core. After the
    /// `Closed` item, answers the `Write` requests of the stream with
    /// `Failed` and its `Close` requests. If the stream ended, returns no
    /// jobs.
    fn stream(&mut self, connection: u32, item: StreamItem) -> Vec<Job> {
        let Some(index) = self.streams.iter().position(|(id, _)| *id == connection) else {
            return Vec::new();
        };
        let reason = match &item {
            StreamItem::Closed { reason, .. } => Some(reason.clone()),
            _ => None,
        };
        let mut effects = resolve(&self.core, &mut self.streams[index].1, item);
        if let Some(reason) = reason {
            self.streams.remove(index);
            let (failed, waiting) = core::mem::take(&mut self.writes)
                .into_iter()
                .partition(|(id, _)| *id == connection);
            self.writes = waiting;
            for (_, mut request) in failed {
                let outcome = Written::Failed(reason.clone());
                effects.extend(resolve(&self.core, &mut request, outcome));
            }
            let (done, waiting) = core::mem::take(&mut self.closes)
                .into_iter()
                .partition(|(id, _)| *id == connection);
            self.closes = waiting;
            for (_, mut request) in done {
                effects.extend(resolve(&self.core, &mut request, ()));
            }
        }
        self.process(effects)
    }

    /// Returns `true` if the stream of `connection` did not end.
    fn is_open(&self, connection: u32) -> bool {
        self.streams.iter().any(|(id, _)| *id == connection)
    }

    /// Answers the `Sleep` request that waits with `wake`. If no request
    /// waits, returns no jobs.
    fn wake(&mut self, wake: Wake) -> Vec<Job> {
        match self.sleep.take() {
            Some(mut request) => {
                let effects = resolve(&self.core, &mut request, wake);
                self.process(effects)
            }
            None => Vec::new(),
        }
    }

    /// Returns the number of timers that run.
    #[must_use]
    pub const fn timer_count(&self) -> usize {
        self.timers.len()
    }

    /// Answers the radio request with `token`, and returns the new effects.
    /// If no request waits for the token, or the answer is not for that kind
    /// of request, the shell drops the answer.
    fn answer_radio(&mut self, token: u32, answer: RadioAnswer) -> Vec<Effect> {
        let Some(index) = self.radio.iter().position(|(radio, _)| *radio == token) else {
            return Vec::new();
        };
        let (_, request) = self.radio.remove(index);
        let core = &self.core;
        match (request, answer) {
            (RadioRequest::Join(mut request), RadioAnswer::Joined(joined)) => {
                resolve(core, &mut request, joined)
            }
            (RadioRequest::Address(mut request), RadioAnswer::Lease(lease)) => {
                resolve(core, &mut request, lease)
            }
            (RadioRequest::Scan(mut request), RadioAnswer::Scanned(scanned)) => {
                resolve(core, &mut request, scanned)
            }
            (RadioRequest::Leave(mut request), RadioAnswer::Done) => {
                resolve(core, &mut request, ())
            }
            (RadioRequest::StopScan(mut request), RadioAnswer::Done) => {
                resolve(core, &mut request, ())
            }
            (RadioRequest::Off(mut request), RadioAnswer::Done) => resolve(core, &mut request, ()),
            (request, _) => {
                // The answer is for another kind of request. The request
                // keeps its place, and the core's time limit ends it.
                self.radio.insert(index, (token, request));
                Vec::new()
            }
        }
    }

    /// Takes a token for the radio request `request` with the operation
    /// `op`, and adds its job to `jobs`. Returns the new effects of the core.
    ///
    /// The core waits for one radio request at a time, so it no longer waits
    /// for an older request. The new request stops each older request, as
    /// the stop rule tells. The shell answers each stopped request with
    /// `Stopped` (or `()`), and the job tells the radio task to stop it.
    fn radio(&mut self, request: RadioRequest, op: RadioOp, jobs: &mut Vec<Job>) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut stop = Vec::new();
        for (token, stopped) in core::mem::take(&mut self.radio) {
            stop.push(token);
            let core = &self.core;
            effects.extend(match stopped {
                RadioRequest::Join(mut request) => resolve(core, &mut request, Joined::Stopped),
                RadioRequest::Address(mut request) => resolve(core, &mut request, Lease::Stopped),
                RadioRequest::Scan(mut request) => resolve(core, &mut request, Scanned::Stopped),
                RadioRequest::Leave(mut request) => resolve(core, &mut request, ()),
                RadioRequest::StopScan(mut request) => resolve(core, &mut request, ()),
                RadioRequest::Off(mut request) => resolve(core, &mut request, ()),
            });
        }
        self.last_radio_token = self.last_radio_token.wrapping_add(1);
        let token = self.last_radio_token;
        self.radio.push((token, request));
        jobs.push(Job::Radio { token, stop, op });
        effects
    }

    fn process(&mut self, effects: Vec<Effect>) -> Vec<Job> {
        let mut queue: alloc::collections::VecDeque<Effect> = effects.into();
        let mut jobs = Vec::new();
        // The place of the last `Render` effect in `jobs`.
        let mut render = None;
        while let Some(effect) = queue.pop_front() {
            if matches!(effect, Effect::Render(_)) {
                render = Some(jobs.len());
            } else {
                queue.extend(self.perform(effect, &mut jobs));
            }
        }
        if let Some(index) = render {
            jobs.insert(index, Job::Render(Box::new(self.view())));
        }
        jobs
    }

    /// Performs one effect. Adds work for the tasks to `jobs`, and
    /// returns the new effects of the core.
    fn perform(&mut self, effect: Effect, jobs: &mut Vec<Job>) -> Vec<Effect> {
        match effect {
            // `process` handles `Render`, because it needs the whole batch.
            Effect::Render(_) => Vec::new(),
            Effect::StreamOpen(request) => {
                let op = request.operation.clone();
                let connection = op.connection;
                jobs.push(stream(connection, StreamOp::Open(op)));
                self.streams.push((connection, request));
                Vec::new()
            }
            Effect::StreamWrite(mut request) => {
                let connection = request.operation.connection;
                if !self.is_open(connection) {
                    let outcome = Written::Failed(NOT_OPEN.to_string());
                    return resolve(&self.core, &mut request, outcome);
                }
                // The job takes the bytes, so the shell does not copy them.
                // The request needs only its answer.
                let op = Write {
                    connection,
                    bytes: core::mem::take(&mut request.operation.bytes),
                };
                jobs.push(stream(connection, StreamOp::Write(op)));
                self.writes.push((connection, request));
                Vec::new()
            }
            Effect::StreamClose(mut request) => {
                let connection = request.operation.connection;
                if !self.is_open(connection) {
                    return resolve(&self.core, &mut request, ());
                }
                let op = request.operation.clone();
                jobs.push(stream(connection, StreamOp::Close(op)));
                self.closes.push((connection, request));
                Vec::new()
            }
            Effect::RadioJoin(request) => {
                let op = RadioOp::Join(request.operation);
                self.radio(RadioRequest::Join(request), op, jobs)
            }
            Effect::RadioAddress(request) => {
                let op = RadioOp::Address(request.operation);
                self.radio(RadioRequest::Address(request), op, jobs)
            }
            Effect::RadioLeave(request) => {
                let op = RadioOp::Leave(request.operation);
                self.radio(RadioRequest::Leave(request), op, jobs)
            }
            Effect::RadioScan(request) => {
                let op = RadioOp::Scan(request.operation);
                self.radio(RadioRequest::Scan(request), op, jobs)
            }
            Effect::RadioStopScan(request) => {
                let op = RadioOp::StopScan(request.operation);
                self.radio(RadioRequest::StopScan(request), op, jobs)
            }
            Effect::RadioOff(request) => {
                let op = RadioOp::Off(request.operation);
                self.radio(RadioRequest::Off(request), op, jobs)
            }
            Effect::RadioRestart(request) => notification(Job::Restart(request.operation), jobs),
            Effect::Log(request) => notification(Job::Log(request.operation), jobs),
            Effect::Sleep(request) => {
                jobs.push(Job::Sleep(request.operation.clone()));
                self.sleep = Some(request);
                Vec::new()
            }
            Effect::FrontLight(request) => {
                self.last_light_token = self.last_light_token.wrapping_add(1);
                let token = self.last_light_token;
                let level = request.operation.level;
                self.lights.push((token, request));
                jobs.push(Job::FrontLight { token, level });
                Vec::new()
            }
            Effect::TimeNotifyAfter(request) => {
                let op = request.operation;
                let id = op.id;
                if let Some(index) = self.timers.iter().position(|timer| timer.id == id) {
                    let old = self.timers.remove(index);
                    jobs.push(clear_timer(old.token, id));
                }
                self.last_token = self.last_token.wrapping_add(1);
                let token = self.last_token;
                jobs.push(Job::Timer {
                    token,
                    op: TimerOp::NotifyAfter(op),
                });
                self.timers.push(Timer { id, token, request });
                Vec::new()
            }
            Effect::TimeClear(mut request) => {
                let id = request.operation.id;
                if let Some(index) = self.timers.iter().position(|timer| timer.id == id) {
                    let old = self.timers.remove(index);
                    jobs.push(clear_timer(old.token, id));
                }
                resolve(&self.core, &mut request, id)
            }
        }
    }
}

/// The reason in the answer to a `Write` for a stream that ended.
const NOT_OPEN: &str = "the stream is not open";

/// Adds the job of a notification to `jobs`. A notification has no answer,
/// so it gives no new effects.
fn notification(job: Job, jobs: &mut Vec<Job>) -> Vec<Effect> {
    jobs.push(job);
    Vec::new()
}

const fn stream(connection: u32, op: StreamOp) -> Job {
    Job::Stream { connection, op }
}

const fn clear_timer(token: u32, id: TimerId) -> Job {
    Job::Timer {
        token,
        op: TimerOp::ClearTimer(ClearTimer { id }),
    }
}

/// Gives the core the answer to a request, and returns the new effects. If
/// the core no longer waits for the answer, returns no effects.
fn resolve<Op: Operation>(
    core: &Core<Dashboard>,
    request: &mut Request<Op>,
    output: Op::Output,
) -> Vec<Effect> {
    core.resolve(request, output).unwrap_or_default()
}

/// Makes the `Start` event from the build-time configuration. The settings
/// have `read_only` from the configuration, so the core does a dry run if
/// it is `true`.
///
/// `path` and `last_sync` are for a start after a deep sleep, with the
/// values that the firmware kept in RTC memory. The firmware uses light
/// sleep, which keeps the core, so it gives `None` for both.
#[must_use]
pub fn start(
    config: &Config<'_>,
    power: PowerSource,
    path: Option<String>,
    last_sync: Option<u64>,
) -> Start {
    let credentials = (!config.mqtt_username.is_empty()).then(|| Credentials {
        username: config.mqtt_username.into(),
        password: config.mqtt_password.into(),
    });
    Start {
        settings: Settings {
            device_id: config.device_id.into(),
            host: config.mqtt_host.into(),
            port: config.mqtt_port,
            credentials,
            read_only: config.read_only,
        },
        power,
        path,
        last_sync,
    }
}

#[cfg(test)]
mod tests;

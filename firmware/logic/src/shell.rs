//! The shell logic of the firmware: it owns the core, routes each effect to
//! the task that performs it, and gives the answers of the tasks to the core.
//!
//! The core has locks that it only tries to take, so one task owns the
//! [`Shell`]. The other tasks never see the core. They send a [`Signal`] to
//! the task of the shell, and the shell gives the task a [`Job`].
//!
//! The shell keeps the bookkeeping that the simulator shell also has:
//!
//! - Each MQTT connection has an id. A new connection disconnects the current
//!   one first. The core ignores the items of an older connection, and the
//!   shell ignores the items of a connection whose stream ended.
//! - Each timer has a token. The core can use a timer id again after it clears
//!   the timer, so the shell ignores a signal with an old token.
//! - Each `WifiOn` request has a token. `WifiOff` answers the requests that
//!   wait with `Failed`, which the core ignores, because it no longer wants
//!   the connection. The shell then drops a late answer of the Wi-Fi task.
//! - `Disconnect` ends when the stream of its connection ends, or at once if
//!   no connection is current. `WifiOff` ends at [`Signal::RadioOff`], in the
//!   order of the requests.
//! - `Sleep` ends at [`Signal::Woke`]. While it waits, a tap is the wake
//!   (`Wake::Touch` with the control at the point), and a change of the
//!   power source also ends it (`Wake::PowerSource`). [`Shell::is_sleeping`] tells
//!   the task that runs the sleep when the request ended.
//! - One batch of effects gives at most one `Render` job, with the view
//!   at the end of the batch. It is at the place of the last `Render` effect,
//!   so it stays before a `Sleep` that follows it.

use alloc::{boxed::Box, collections::VecDeque, string::String, vec::Vec};

use crux_core::{Request, capability::Operation};
use crux_time::{
    TimerId,
    operation::{ClearTimer, NotifyAfter},
};
use embedded_graphics::prelude::Point;
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, ViewModel,
    capabilities::{
        mqtt::{Connect, Credentials, Disconnect, MqttEvent, Publish, Subscribe},
        network::{WifiOff, WifiOn, WifiStatus},
        power::{Sleep, Wake},
    },
};

use crate::config::Config;

/// An answer or a fact from a task, for the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// An item from the stream of the MQTT connection with this id.
    Mqtt {
        /// The id of the connection in the shell.
        connection: u32,
        /// The item.
        event: MqttEvent,
    },
    /// The timer with this id and token expired.
    Timer {
        /// The id of the timer in the core.
        id: TimerId,
        /// The token of the timer in the shell.
        token: u32,
    },
    /// The answer to the `WifiOn` request with this token. The answer is also
    /// for the older requests that wait.
    Wifi {
        /// The token of the newest request that the answer is for.
        token: u32,
        /// The answer.
        status: WifiStatus,
    },
    /// A tap at this point on the panel. The shell sends it to the core as
    /// `Event::Tapped`, also on no control. While a `Sleep` request waits,
    /// the tap answers it with `Wake::Touch` and the control, in place of a
    /// tap.
    Tap(Point),
    /// The radio is off. The answer to the oldest `WifiOff` job that waits.
    RadioOff,
    /// The device woke from a real wake. The answer to the `Sleep` job.
    Woke(Wake),
    /// An event for the core, for example `PowerSourceChanged`.
    Event(Event),
}

/// An MQTT operation of the core, for the connection with an id in the
/// [`Job`]. Each variant holds the operation of the core, unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MqttOp {
    /// Open the connection. The MQTT task answers with the items of its
    /// stream, and the last item is always `Disconnected`.
    Connect(Connect),
    /// Subscribe to the topic filters.
    Subscribe(Subscribe),
    /// Publish a message.
    Publish(Publish),
    /// Send a disconnect packet and close the connection. The core asks for
    /// it, or the shell makes it to close a connection that a new one
    /// replaces. In the second case no core operation is behind it.
    Disconnect(Disconnect),
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
/// adds only what the shell owns: the connection id, the Wi-Fi token and
/// the timer token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    /// Draw the view model on the panel. The shell decides this job, so no
    /// core operation is behind it. The view model is in a box, because it
    /// is much larger than the other jobs.
    Render(Box<ViewModel>),
    /// Do an operation on an MQTT connection.
    Mqtt {
        /// The id of the connection.
        connection: u32,
        /// The operation.
        op: MqttOp,
    },
    /// Turn Wi-Fi on and join the network. Answer with [`Signal::Wifi`] and
    /// the token.
    WifiOn {
        /// The token of the request.
        token: u32,
        /// The operation.
        op: WifiOn,
    },
    /// Turn Wi-Fi off. Stop a join that is in progress, and do not answer it.
    /// Answer with [`Signal::RadioOff`] when the radio is off.
    WifiOff(WifiOff),
    /// Draw the view model that waits, then put the device to sleep. Answer
    /// with [`Signal::Woke`] at the first real wake. After another wake,
    /// sleep again.
    Sleep(Sleep),
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
    /// The connections whose streams did not end, from the oldest.
    connections: Vec<(u32, Request<Connect>)>,
    /// The id of the connection to which subscribe, publish and disconnect
    /// apply.
    current: Option<u32>,
    last_connection: u32,
    timers: Vec<Timer>,
    last_token: u32,
    /// The `WifiOn` requests that wait for an answer, with their tokens.
    wifi: Vec<(u32, Request<WifiOn>)>,
    last_wifi_token: u32,
    /// The `Disconnect` requests that wait for the end of the stream of their
    /// connection.
    disconnects: Vec<(u32, Request<Disconnect>)>,
    /// The `WifiOff` requests that wait for the radio, from the oldest.
    wifi_off: VecDeque<Request<WifiOff>>,
    /// The `Sleep` request that waits for a wake.
    sleep: Option<Request<Sleep>>,
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
            connections: Vec::new(),
            current: None,
            last_connection: 0,
            timers: Vec::new(),
            last_token: 0,
            wifi: Vec::new(),
            last_wifi_token: 0,
            disconnects: Vec::new(),
            wifi_off: VecDeque::new(),
            sleep: None,
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
            Signal::Mqtt { connection, event } => {
                let Some(index) = self
                    .connections
                    .iter()
                    .position(|(id, _)| *id == connection)
                else {
                    return Vec::new();
                };
                let last = matches!(event, MqttEvent::Disconnected { .. });
                let mut effects = resolve(&self.core, &mut self.connections[index].1, event);
                if last {
                    self.connections.remove(index);
                    if self.current == Some(connection) {
                        self.current = None;
                    }
                    // The connection is closed, so its `Disconnect` requests
                    // end.
                    let (done, waiting) = core::mem::take(&mut self.disconnects)
                        .into_iter()
                        .partition(|(id, _)| *id == connection);
                    self.disconnects = waiting;
                    for (_, mut request) in done {
                        effects.extend(resolve(&self.core, &mut request, ()));
                    }
                }
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
            Signal::Wifi { token, status } => {
                let effects = self.answer_wifi(|request| request <= token, status);
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
            Signal::RadioOff => match self.wifi_off.pop_front() {
                Some(mut request) => {
                    let effects = resolve(&self.core, &mut request, ());
                    self.process(effects)
                }
                None => Vec::new(),
            },
            Signal::Woke(wake) => self.wake(wake),
        }
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

    /// Answers the `WifiOn` requests whose tokens match, and returns the new
    /// effects.
    fn answer_wifi(&mut self, matches: impl Fn(u32) -> bool, status: WifiStatus) -> Vec<Effect> {
        let (answered, waiting) = core::mem::take(&mut self.wifi)
            .into_iter()
            .partition(|(token, _)| matches(*token));
        self.wifi = waiting;
        let mut effects = Vec::new();
        for (_, mut request) in answered {
            effects.extend(resolve(&self.core, &mut request, status));
        }
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
            Effect::MqttConnect(request) => {
                if let Some(current) = self.current.take() {
                    jobs.push(mqtt(current, MqttOp::Disconnect(Disconnect)));
                }
                self.last_connection = self.last_connection.wrapping_add(1);
                let id = self.last_connection;
                jobs.push(mqtt(id, MqttOp::Connect(request.operation.clone())));
                self.connections.push((id, request));
                self.current = Some(id);
                Vec::new()
            }
            Effect::MqttSubscribe(request) => {
                if let Some(current) = self.current {
                    let subscribe = request.operation;
                    jobs.push(mqtt(current, MqttOp::Subscribe(subscribe)));
                }
                Vec::new()
            }
            Effect::MqttPublish(request) => {
                if let Some(current) = self.current {
                    jobs.push(mqtt(current, MqttOp::Publish(request.operation)));
                }
                Vec::new()
            }
            Effect::MqttDisconnect(mut request) => match self.current.take() {
                Some(current) => {
                    jobs.push(mqtt(current, MqttOp::Disconnect(request.operation.clone())));
                    self.disconnects.push((current, request));
                    Vec::new()
                }
                None => resolve(&self.core, &mut request, ()),
            },
            Effect::WifiOn(request) => {
                self.last_wifi_token = self.last_wifi_token.wrapping_add(1);
                let token = self.last_wifi_token;
                let op = request.operation.clone();
                self.wifi.push((token, request));
                jobs.push(Job::WifiOn { token, op });
                Vec::new()
            }
            Effect::WifiOff(request) => {
                jobs.push(Job::WifiOff(request.operation.clone()));
                self.wifi_off.push_back(request);
                self.answer_wifi(|_| true, WifiStatus::Failed)
            }
            Effect::Sleep(request) => {
                jobs.push(Job::Sleep(request.operation.clone()));
                self.sleep = Some(request);
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

const fn mqtt(connection: u32, op: MqttOp) -> Job {
    Job::Mqtt { connection, op }
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

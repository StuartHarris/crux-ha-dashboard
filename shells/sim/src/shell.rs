//! The shell: it runs the core on the main thread and performs its effects.
//!
//! The core has locks that it only tries to take, so one thread owns it. The
//! byte streams and the timers run as tasks on a tokio runtime. A task sends
//! a [`Signal`] through a channel when it has an answer. The main thread
//! reads the channel and calls [`Shell::handle`], which gives the answer to
//! the core.
//!
//! The core runs the MQTT session, and the shell gives it only bytes. The
//! shell obeys the rules of the stream capability
//! ([`shared::capabilities::stream`]):
//!
//! - Each stream has a token of the shell and the connection id of the
//!   core. A restart makes a new core, which uses the same connection ids
//!   again, so the signals of a task carry the token. The shell ignores the
//!   signals of a stream that ended.
//! - A new `Open` replaces the stream that is open. The shell stops the old
//!   task, and the old stream ends with `Closed("replaced")`.
//! - A `Write` ends at [`Signal::Written`] for its stream, in the order of
//!   the requests. At the end of the stream, the shell answers each `Write`
//!   of the stream that waits with `Failed`. If no stream has the connection
//!   id of the request, the shell answers `Failed` at once.
//! - A `Close` ends at the end of the stream, after the `Closed` item. If no
//!   stream has the connection id of the request, the shell answers at once.
//! - The core does not clear the timers of the session. Such a timer runs
//!   until it expires, and the core then ignores it.
//!
//! The shell writes each effect to an [`Inspector`], which the window shows.
//! The inspector also finds the MQTT packets in a copy of the bytes of each
//! stream.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, mpsc},
    time::Duration,
};

use embedded_graphics::prelude::Point;

use crux_core::{Request, capability::Operation};
use crux_time::{TimerId, operation::NotifyAfter};
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, ViewModel,
    capabilities::{
        network::WifiStatus,
        power::{Sleep, Wake},
        stream::{Close, Open, StreamItem, Write, Written},
    },
};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    inspector::{Inspector, Wire},
    stream::{FLUSH_LIMIT, Opener, REPLACED, Task},
};

/// The reason in the answer to a `Write` request for a stream that is not
/// open.
const NOT_OPEN: &str = "the stream is not open";

/// An answer from a task on the runtime.
#[derive(Debug)]
pub enum Signal {
    /// An item from the stream with this token. `Closed` is the last item.
    Stream {
        /// The token of the stream in the shell.
        token: u64,
        /// The item.
        item: StreamItem,
    },
    /// The answer to the oldest `Write` request of the stream with this
    /// token.
    Written {
        /// The token of the stream in the shell.
        token: u64,
        /// The answer.
        outcome: Written,
    },
    /// The timer with this id expired.
    Timer {
        /// The id of the timer in the core.
        id: TimerId,
        /// The token of the task in the shell. The core can use an id
        /// again after it clears a timer, so the shell ignores a signal
        /// with an old token.
        token: u64,
    },
}

/// Sends signals to the main thread and wakes it.
#[derive(Clone)]
pub struct Notifier {
    sender: mpsc::Sender<Signal>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Notifier {
    /// Makes a notifier that sends to `sender` and then calls `wake`.
    pub fn new(sender: mpsc::Sender<Signal>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { sender, wake }
    }

    /// Sends a signal. If the main thread stopped, does nothing.
    pub fn send(&self, signal: Signal) {
        if self.sender.send(signal).is_ok() {
            (self.wake)();
        }
    }
}

/// A stream that did not end, and the request of the core for it.
struct Live {
    /// The token of the stream in the shell.
    token: u64,
    /// The connection id of the core.
    connection: u32,
    request: Request<Open>,
    task: Task,
    /// The packets in the bytes of the stream, for the MQTT log.
    wire: Wire,
}

/// A timer that runs.
struct Timer {
    request: Request<NotifyAfter>,
    task: JoinHandle<()>,
    token: u64,
}

/// The shell.
pub struct Shell {
    core: Core<Dashboard>,
    runtime: Handle,
    notifier: Notifier,
    open: Opener,
    /// The streams that did not end, from the oldest. A new `Open` replaces
    /// the stream that is open, so there is at most one.
    streams: Vec<Live>,
    last_stream: u64,
    /// The `Write` requests that wait for their answers, from the oldest,
    /// with the tokens of their streams.
    writes: Vec<(u64, Request<Write>)>,
    /// The `Close` requests that wait for the end of their streams, with the
    /// tokens of the streams.
    closes: Vec<(u64, Request<Close>)>,
    /// The longest time that a close waits for the peer.
    flush_limit: Duration,
    timers: HashMap<TimerId, Timer>,
    last_token: u64,
    render: bool,
    inspector: Inspector,
    /// The settings that [`Shell::start`] gave, for [`Shell::restart`].
    settings: Option<Settings>,
    /// The number of `WifiQuiet` effects.
    quiet: u32,
    /// The `Sleep` request that waits for a wake.
    sleep: Option<Request<Sleep>>,
    /// The level of the front light, in percent. The window shows the
    /// light as a tint over the panel.
    light: u8,
}

impl Drop for Shell {
    /// Stops all the tasks of the shell. A stream stops with no DISCONNECT
    /// packet, so the broker publishes the last will, as for a device that
    /// loses power.
    fn drop(&mut self) {
        for live in &self.streams {
            live.task.abort();
        }
        for timer in self.timers.values() {
            timer.task.abort();
        }
    }
}

impl Shell {
    /// Makes a shell. Tasks run on `runtime` and send their answers with
    /// `notifier`. `open` opens a byte stream.
    pub fn new(runtime: Handle, notifier: Notifier, open: Opener) -> Self {
        Self {
            core: Core::new(),
            runtime,
            notifier,
            open,
            streams: Vec::new(),
            last_stream: 0,
            writes: Vec::new(),
            closes: Vec::new(),
            flush_limit: FLUSH_LIMIT,
            timers: HashMap::new(),
            last_token: 0,
            render: false,
            inspector: Inspector::default(),
            settings: None,
            quiet: 0,
            sleep: None,
            light: 0,
        }
    }

    /// Sends `Start` to the core, as the device does after a cold start.
    /// `settings.read_only` starts the core in a dry run. The shell keeps
    /// the settings for [`Shell::restart`]. A cold start has no
    /// `last_sync`.
    pub fn start(&mut self, settings: Settings, power: PowerSource, path: Option<String>) {
        self.settings = Some(settings.clone());
        self.send(Event::Start(Start {
            settings,
            power,
            path,
            last_sync: None,
        }));
    }

    /// Returns `true` if "Restart" can work: the shell started the core and
    /// the device is asleep.
    pub fn can_restart(&self) -> bool {
        self.settings.is_some() && self.view().asleep
    }

    /// Simulates the wake from a deep sleep. Returns `false` if
    /// [`Shell::can_restart`] is `false`, and then does nothing.
    ///
    /// A deep sleep loses the RAM, so the shell stops all the tasks, makes a
    /// new core and sends `Start`. The `path` and `last_sync` come from the
    /// view model of the old core. These are the two values that the
    /// firmware keeps in RTC memory. The power source is the current one.
    /// After `Start`, the shell sends the current charge of the battery, as
    /// the first read of the fuel gauge does on the device.
    ///
    /// The device reads `read_only` from its configuration after a deep
    /// sleep, but the user can switch the dry run in the simulator. The
    /// restart uses `read_only` from the settings or from the view, so a
    /// restart never switches to live service calls.
    ///
    /// The inspector and the counters of streams and timers stay. A late
    /// signal from a task of the old core finds no stream or timer, so the
    /// shell ignores it.
    pub fn restart(&mut self) -> bool {
        let view = self.view();
        let Some(mut settings) = self.settings.clone().filter(|_| view.asleep) else {
            return false;
        };
        settings.read_only |= view.read_only;
        eprintln!("power: restart after a deep sleep");
        self.inspector.effect(
            "Restart",
            String::new(),
            "deep sleep: new core, the page and the last update kept",
        );
        for live in self.streams.drain(..) {
            live.task.abort();
        }
        for (_, timer) in self.timers.drain() {
            timer.task.abort();
        }
        self.writes.clear();
        self.closes.clear();
        self.sleep = None;
        self.core = Core::new();
        self.send(Event::Start(Start {
            settings,
            power: view.power,
            path: Some(view.path),
            last_sync: view.last_sync,
        }));
        if view.battery.is_some() {
            self.send(Event::BatteryChanged(view.battery));
        }
        true
    }

    /// Sends an event to the core and performs the effects.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.process(effects);
    }

    /// Sends a tap at `point` on the panel to the core, with the control at
    /// that point. While a `Sleep` request waits, the tap is the wake: the
    /// shell answers the request with `Wake::Touch` and the control, and
    /// sends no tap. Else the shell sends each tap, also on no control. The
    /// core decides what the tap does.
    pub fn tap(&mut self, point: Point) {
        let control = ui::hit_test(&self.view(), point);
        if self.answer_sleep(Wake::Touch(control)) {
            return;
        }
        self.send(Event::Tapped(control));
    }

    /// Answers the `Sleep` request that waits with `Wake::Timer`, a wake that
    /// is not a tap, for example from the Wake button. If no request waits,
    /// does nothing.
    ///
    /// The core stays in memory while the simulator sleeps, as in a light
    /// sleep.
    pub fn wake(&mut self) {
        if !self.answer_sleep(Wake::Timer) {
            eprintln!("power: wake, but the device does not sleep");
        }
    }

    /// Sends the change of the power source to the core. If a `Sleep`
    /// request waits, then answers it with `Wake::PowerSource`.
    pub fn set_power_source(&mut self, power: PowerSource) {
        self.send(Event::PowerSourceChanged(power));
        self.answer_sleep(Wake::PowerSource);
    }

    /// Answers the `Sleep` request that waits with `wake`. Returns `false`
    /// if no request waits.
    fn answer_sleep(&mut self, wake: Wake) -> bool {
        let Some(mut request) = self.sleep.take() else {
            return false;
        };
        eprintln!("power: wake ({wake:?})");
        self.inspector
            .effect("Sleep", String::new(), format!("resolved: {wake:?}"));
        let effects = resolve(&self.core, &mut request, wake);
        self.process(effects);
        true
    }

    /// Returns `true` if "Sleep now" can work: the view shows that the
    /// device is on battery and awake. The core ignores `SleepNow` in the
    /// other cases.
    pub fn can_sleep_now(&self) -> bool {
        let view = self.view();
        view.power == PowerSource::Battery && !view.asleep
    }

    /// Sends `Event::SleepNow` to the core, which ends the idle time now.
    /// Returns `false` if [`Shell::can_sleep_now`] is `false`, and then does
    /// nothing.
    ///
    /// The shell does not make the device sleep. The core decides, as after
    /// a real idle timeout. If taps wait to be sent, the core first waits one
    /// more idle period, and a second "Sleep now" ends that period. The
    /// timers continue to run.
    pub fn sleep_now(&mut self) -> bool {
        if !self.can_sleep_now() {
            return false;
        }
        eprintln!("power: sleep now");
        self.inspector.effect(
            "Event",
            "SleepNow".into(),
            "sent by the shell: the core ends the idle time",
        );
        self.send(Event::SleepNow);
        true
    }

    /// Returns the number of timers that run.
    pub fn timer_count(&self) -> usize {
        self.timers.len()
    }

    /// Returns the connection id of the core for the stream that is open, if
    /// there is one.
    pub fn current_connection(&self) -> Option<u32> {
        self.streams.last().map(|live| live.connection)
    }

    /// Returns the number of `WifiQuiet` effects. The core sends `WifiQuiet`
    /// when the broker accepted the subscribe, after the retained states.
    pub const fn quiet_count(&self) -> u32 {
        self.quiet
    }

    /// Returns the logs of the shell.
    pub const fn inspector(&self) -> &Inspector {
        &self.inspector
    }

    /// Returns the logs of the shell, to clear them.
    pub const fn inspector_mut(&mut self) -> &mut Inspector {
        &mut self.inspector
    }

    /// Returns the level of the front light, in percent. The value 0 means
    /// that the light is off.
    pub const fn light(&self) -> u8 {
        self.light
    }

    /// Returns the view of the core now.
    pub fn view(&self) -> ViewModel {
        self.core.view()
    }

    /// Returns `true` if the core asked for a render since the last call.
    pub const fn take_render(&mut self) -> bool {
        std::mem::replace(&mut self.render, false)
    }

    /// Gives an answer from a task to the core.
    pub fn handle(&mut self, signal: Signal) {
        match signal {
            Signal::Stream { token, item } => self.stream(token, item),
            Signal::Written { token, outcome } => {
                let Some(index) = self.writes.iter().position(|(id, _)| *id == token) else {
                    return;
                };
                let (_, mut request) = self.writes.remove(index);
                self.inspector.effect(
                    "StreamWrite",
                    format!("connection {}", request.operation.connection),
                    format!("resolved: {outcome:?}"),
                );
                let effects = resolve(&self.core, &mut request, outcome);
                self.process(effects);
            }
            Signal::Timer { id, token } => {
                if self
                    .timers
                    .get(&id)
                    .is_some_and(|timer| timer.token == token)
                    && let Some(mut timer) = self.timers.remove(&id)
                {
                    self.inspector.effect(
                        "TimeNotifyAfter",
                        format!("timer {}", id.0),
                        "resolved: the timer expired",
                    );
                    let effects = resolve(&self.core, &mut timer.request, id);
                    self.process(effects);
                }
            }
        }
    }

    /// Gives an item of the stream with the token `token` to the core. After
    /// the `Closed` item, answers the `Write` and `Close` requests of the
    /// stream. If the stream ended, does nothing.
    fn stream(&mut self, token: u64, item: StreamItem) {
        let Some(index) = self.streams.iter().position(|live| live.token == token) else {
            return;
        };
        let connection = self.streams[index].connection;
        let request = format!("connection {connection}");
        let mut reason = None;
        match &item {
            StreamItem::Opened => {
                eprintln!("stream: opened (connection {connection})");
                self.inspector.effect("StreamOpen", request, "item: Opened");
            }
            StreamItem::Received(bytes) => {
                self.inspector
                    .received(&mut self.streams[index].wire, bytes);
            }
            StreamItem::Closed(text) => {
                eprintln!("stream: closed (connection {connection}): {text}");
                self.inspector
                    .effect("StreamOpen", request, format!("item: Closed ({text})"));
                reason = Some(text.clone());
            }
        }
        let mut effects = resolve(&self.core, &mut self.streams[index].request, item);
        if let Some(reason) = reason {
            self.streams.remove(index);
            effects.extend(self.end(token, connection, &reason));
        }
        self.process(effects);
    }

    /// Answers the requests of the stream with the token `token`, which
    /// ended for `reason`: `Failed` for each `Write`, and `()` for each
    /// `Close`. Returns the new effects.
    fn end(&mut self, token: u64, connection: u32, reason: &str) -> Vec<Effect> {
        let mut effects = Vec::new();
        let (failed, waiting) = std::mem::take(&mut self.writes)
            .into_iter()
            .partition(|(id, _)| *id == token);
        self.writes = waiting;
        for (_, mut request) in failed {
            self.inspector.effect(
                "StreamWrite",
                format!("connection {connection}"),
                format!("resolved: Failed ({reason})"),
            );
            let outcome = Written::Failed(reason.to_string());
            effects.extend(resolve(&self.core, &mut request, outcome));
        }
        let (done, waiting) = std::mem::take(&mut self.closes)
            .into_iter()
            .partition(|(id, _)| *id == token);
        self.closes = waiting;
        for (_, mut request) in done {
            self.inspector.effect(
                "StreamClose",
                format!("connection {connection}"),
                "resolved: closed",
            );
            effects.extend(resolve(&self.core, &mut request, ()));
        }
        effects
    }

    fn process(&mut self, effects: Vec<Effect>) {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            queue.extend(self.perform(effect));
        }
    }

    /// Performs one effect and returns the new effects of the core.
    fn perform(&mut self, effect: Effect) -> Vec<Effect> {
        match effect {
            Effect::Render(_) => {
                self.render = true;
                self.inspector
                    .effect("Render", String::new(), "the window draws the view");
                Vec::new()
            }
            Effect::StreamOpen(request) => self.open(request),
            Effect::StreamWrite(request) => self.write(request),
            Effect::StreamClose(request) => self.close(request),
            Effect::WifiOn(mut request) => {
                self.inspector
                    .effect("WifiOn", String::new(), "resolved: Connected");
                resolve(&self.core, &mut request, WifiStatus::Connected)
            }
            Effect::WifiOff(mut request) => {
                self.inspector.effect(
                    "WifiOff",
                    String::new(),
                    "resolved: the simulator has no Wi-Fi",
                );
                resolve(&self.core, &mut request, ())
            }
            Effect::WifiQuiet(_) => self.quiet(),
            Effect::Sleep(request) => {
                eprintln!("power: asleep. Click the panel or press W to wake it.");
                self.inspector.effect(
                    "Sleep",
                    String::new(),
                    "asleep: click the panel or press W to wake it",
                );
                self.sleep = Some(request);
                Vec::new()
            }
            Effect::FrontLight(mut request) => {
                let level = request.operation.level;
                self.light = level;
                self.inspector.effect(
                    "FrontLight",
                    format!("{level}%"),
                    "resolved: the window shows the light",
                );
                resolve(&self.core, &mut request, ())
            }
            Effect::TimeNotifyAfter(request) => {
                let id = request.operation.id;
                let duration = Duration::from(request.operation.duration);
                self.inspector.effect(
                    "TimeNotifyAfter",
                    format!("timer {} after {} ms", id.0, duration.as_millis()),
                    "started",
                );
                self.last_token += 1;
                let token = self.last_token;
                let notifier = self.notifier.clone();
                let task = self.runtime.spawn(async move {
                    tokio::time::sleep(duration).await;
                    notifier.send(Signal::Timer { id, token });
                });
                let timer = Timer {
                    request,
                    task,
                    token,
                };
                if let Some(old) = self.timers.insert(id, timer) {
                    old.task.abort();
                }
                Vec::new()
            }
            Effect::TimeClear(mut request) => {
                let id = request.operation.id;
                let outcome = if let Some(timer) = self.timers.remove(&id) {
                    timer.task.abort();
                    "resolved: stopped"
                } else {
                    "resolved: the timer did not run"
                };
                self.inspector
                    .effect("TimeClear", format!("timer {}", id.0), outcome);
                resolve(&self.core, &mut request, id)
            }
        }
    }

    /// Logs `WifiQuiet`. The simulator has no access points, so it does
    /// nothing else.
    fn quiet(&mut self) -> Vec<Effect> {
        self.quiet += 1;
        self.inspector.effect(
            "WifiQuiet",
            String::new(),
            "ignored: the simulator has no access points",
        );
        Vec::new()
    }

    /// Opens a stream for `request`. If a stream is open, the new stream
    /// replaces it, and the old stream ends with `Closed("replaced")`.
    fn open(&mut self, request: Request<Open>) -> Vec<Effect> {
        let mut effects = Vec::new();
        for mut live in std::mem::take(&mut self.streams) {
            live.task.abort();
            let connection = live.connection;
            eprintln!("stream: closed (connection {connection}): {REPLACED}");
            self.inspector.effect(
                "StreamOpen",
                format!("connection {connection}"),
                format!("item: Closed ({REPLACED})"),
            );
            let item = StreamItem::Closed(REPLACED.into());
            effects.extend(resolve(&self.core, &mut live.request, item));
            effects.extend(self.end(live.token, connection, REPLACED));
        }
        let Open {
            connection,
            host,
            port,
        } = &request.operation;
        let (connection, target) = (*connection, format!("{host}:{port}"));
        eprintln!("stream: open {target} (connection {connection})");
        self.inspector.effect(
            "StreamOpen",
            format!("connection {connection}: {target}"),
            "started",
        );
        self.last_stream += 1;
        let token = self.last_stream;
        let connecting = (self.open)(host, *port);
        let task = Task::spawn(
            &self.runtime,
            connecting,
            token,
            self.notifier.clone(),
            self.flush_limit,
        );
        self.streams.push(Live {
            token,
            connection,
            request,
            task,
            wire: Wire::new(target),
        });
        effects
    }

    /// Gives the bytes of `request` to the task of its stream. If no stream
    /// has the connection id of the request, answers `Failed` at once.
    fn write(&mut self, mut request: Request<Write>) -> Vec<Effect> {
        let connection = request.operation.connection;
        let length = request.operation.bytes.len();
        let entry = format!("connection {connection}: {length} bytes");
        let Some(live) = self
            .streams
            .iter_mut()
            .find(|live| live.connection == connection)
        else {
            self.inspector.effect(
                "StreamWrite",
                entry,
                format!("resolved: Failed ({NOT_OPEN})"),
            );
            return resolve(&self.core, &mut request, Written::Failed(NOT_OPEN.into()));
        };
        // The task takes the bytes. The request needs only its answer.
        let bytes = std::mem::take(&mut request.operation.bytes);
        self.inspector.sent(&mut live.wire, &bytes);
        live.task.write(bytes);
        self.writes.push((live.token, request));
        self.inspector
            .effect("StreamWrite", entry, "given to the stream");
        Vec::new()
    }

    /// Closes the stream of `request`, and logs why the core closes it. The
    /// request ends at the end of the stream. If no stream has the
    /// connection id of the request, answers at once.
    fn close(&mut self, mut request: Request<Close>) -> Vec<Effect> {
        let Close {
            connection,
            clean,
            reason,
        } = &request.operation;
        let kind = if *clean { "clean" } else { "failed" };
        eprintln!("stream: close (connection {connection}, {kind}): {reason}");
        let entry = format!("connection {connection}, {kind}: {reason}");
        let connection = *connection;
        let Some(live) = self
            .streams
            .iter_mut()
            .find(|live| live.connection == connection)
        else {
            self.inspector
                .effect("StreamClose", entry, "resolved: no stream");
            return resolve(&self.core, &mut request, ());
        };
        live.task.close();
        self.closes.push((live.token, request));
        self.inspector.effect("StreamClose", entry, "started");
        Vec::new()
    }
}

/// Gives the core the answer to a request, and returns the new effects. If
/// the core no longer waits for the answer, logs it and returns no effects.
fn resolve<Op: Operation>(
    core: &Core<Dashboard>,
    request: &mut Request<Op>,
    output: Op::Output,
) -> Vec<Effect> {
    core.resolve(request, output).unwrap_or_else(|error| {
        eprintln!("core: the answer was not used: {error}");
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{Arc, Mutex, MutexGuard, mpsc},
        time::{Duration, Instant},
    };

    use crux_core::Command;
    use embedded_graphics::prelude::Point;
    use shared::{
        Control, Effect, Event, PowerSource, Settings,
        capabilities::stream::{Stream, StreamItem, Written},
        fake::Broker,
        features::{
            connection::{KEEP_ALIVE_SECS, RECONNECT_MIN},
            power::IDLE_TIMEOUT,
            refresh::{DRAW_DELAY, FULL_REFRESH_INTERVAL},
        },
        mqtt::{Credentials, OPEN_TIMEOUT, codec::server::Packet as Received, ping_interval},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
        runtime::Runtime,
        sync::Notify,
    };

    use super::{NOT_OPEN, Notifier, Shell, Signal};
    use crate::{
        inspector::{Direction, Packet},
        stream::{Io, Opener, PEER_CLOSED, REPLACED},
    };

    /// The longest time that a test waits for a condition.
    const WAIT: Duration = Duration::from_secs(2);
    /// The time with no signal after which the shell has nothing more to do.
    const SETTLE: Duration = Duration::from_millis(100);
    /// The durations of the timers of the features. The timers of the MQTT
    /// session have other durations.
    const FEATURE_TIMERS: [Duration; 3] = [DRAW_DELAY, IDLE_TIMEOUT, FULL_REFRESH_INTERVAL];

    /// What the fake opener gives.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// A stream to the fake broker.
        Broker,
        /// A stream whose peer never reads, so a large write waits.
        Deaf,
        /// A stream that never opens.
        Stall,
    }

    /// The fake network: the broker, and a log of what the shell asks of it.
    struct Net {
        broker: Broker,
        mode: Mode,
        /// The number of streams that the opener made.
        opened: usize,
        /// The number of the stream that the broker serves.
        current: usize,
        /// Wakes the driver of the current stream.
        nudge: Arc<Notify>,
        /// If `true`, the driver of the current stream closes the stream at
        /// the next nudge.
        hang_up: bool,
        /// The streams that the opener opened and the packets that the
        /// broker received, as text, for example `publish <topic> 1`. The
        /// number is the number of the stream. The core subscribes with
        /// several SUBSCRIBE packets in a row, which are one line.
        log: Vec<String>,
        /// The numbers of the streams that the shell closed, in order.
        closed: Vec<usize>,
    }

    type Shared = Arc<Mutex<Net>>;

    impl Net {
        /// Gives bytes from the stream `number` to the broker, and returns
        /// the answer. The broker ignores the bytes of an older stream.
        fn receive(&mut self, number: usize, bytes: Vec<u8>) -> Vec<u8> {
            if number != self.current {
                return Vec::new();
            }
            let before = self.broker.packets.len();
            let answer = self.broker.receive(bytes);
            for packet in &self.broker.packets[before..] {
                let text = match packet {
                    Received::Connect { .. } => "connect".to_string(),
                    Received::Publish { topic, .. } => format!("publish {topic}"),
                    Received::Subscribe { .. } => "subscribe".to_string(),
                    Received::PingReq => "ping".to_string(),
                    Received::Disconnect => "disconnect".to_string(),
                    Received::PubAck(_) => continue,
                };
                let line = format!("{text} {number}");
                if text == "subscribe" && self.log.last() == Some(&line) {
                    continue;
                }
                self.log.push(line);
            }
            answer
        }

        /// Records that the shell closed the stream `number`. If it is the
        /// current stream and the broker did not get a DISCONNECT packet, the
        /// broker publishes the last will.
        fn close(&mut self, number: usize) {
            self.closed.push(number);
            if number == self.current {
                self.broker.drop_connection();
            }
        }
    }

    /// Serves the stream `number` with the broker until the stream closes.
    async fn drive(server: DuplexStream, net: Shared, number: usize, nudge: Arc<Notify>) {
        let (mut reader, mut writer) = tokio::io::split(server);
        let mut buffer = vec![0; 4096];
        loop {
            let answer = tokio::select! {
                read = reader.read(&mut buffer) => match read {
                    Ok(length) if length > 0 => {
                        net.lock().unwrap().receive(number, buffer[..length].to_vec())
                    }
                    _ => {
                        net.lock().unwrap().close(number);
                        return;
                    }
                },
                () = nudge.notified() => {
                    let mut net = net.lock().unwrap();
                    if net.hang_up {
                        net.hang_up = false;
                        net.broker.drop_connection();
                        return;
                    }
                    net.broker.take()
                }
            };
            if !answer.is_empty() && writer.write_all(&answer).await.is_err() {
                return;
            }
        }
    }

    /// Returns an opener that opens a stream of the mode of `net`.
    fn fake_opener(runtime: &Runtime, net: &Shared) -> Opener {
        let net = net.clone();
        let runtime = runtime.handle().clone();
        Box::new(move |_, _| {
            let mut guard = net.lock().unwrap();
            guard.opened += 1;
            let number = guard.opened;
            guard.log.push(format!("open {number}"));
            let client = match guard.mode {
                Mode::Stall => return Box::pin(pending()),
                Mode::Deaf => {
                    let (client, server) = tokio::io::duplex(8);
                    runtime.spawn(async move {
                        let _server = server;
                        pending::<()>().await;
                    });
                    client
                }
                Mode::Broker => {
                    if let Err(reason) = guard.broker.open() {
                        return Box::pin(async move { Err(std::io::Error::other(reason)) });
                    }
                    guard.current = number;
                    let nudge = Arc::new(Notify::new());
                    guard.nudge = nudge.clone();
                    let (client, server) = tokio::io::duplex(64 * 1024);
                    runtime.spawn(drive(server, net.clone(), number, nudge));
                    client
                }
            };
            drop(guard);
            Box::pin(async move { Ok(Box::new(client) as Box<dyn Io>) })
        })
    }

    struct Fixture {
        runtime: Runtime,
        shell: Option<Shell>,
        signals: mpsc::Receiver<Signal>,
        net: Shared,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_mode(Mode::Broker)
        }

        fn with_mode(mode: Mode) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (sender, signals) = mpsc::channel();
            let notifier = Notifier::new(sender, Arc::new(|| {}));
            let net = Arc::new(Mutex::new(Net {
                broker: Broker::default(),
                mode,
                opened: 0,
                current: 0,
                nudge: Arc::new(Notify::new()),
                hang_up: false,
                log: Vec::new(),
                closed: Vec::new(),
            }));
            let open = fake_opener(&runtime, &net);
            let mut shell = Shell::new(runtime.handle().clone(), notifier, open);
            shell.flush_limit = Duration::from_millis(50);
            Self {
                runtime,
                shell: Some(shell),
                signals,
                net,
            }
        }

        fn shell(&mut self) -> &mut Shell {
            self.shell.as_mut().unwrap()
        }

        fn net(&self) -> MutexGuard<'_, Net> {
            self.net.lock().unwrap()
        }

        fn start(&mut self, power: PowerSource) {
            self.start_with(power, None, false);
        }

        fn start_with(
            &mut self,
            power: PowerSource,
            credentials: Option<Credentials>,
            read_only: bool,
        ) {
            let settings = Settings {
                device_id: "test".into(),
                host: "localhost".into(),
                port: 1883,
                credentials,
                read_only,
            };
            self.shell()
                .start(settings, power, Some("/room/living_room".into()));
        }

        /// Gives the signals of the stream tasks to the shell until `done`
        /// is `true`. The timers of the shell do not fire by themselves: a
        /// test sends their signals. Panics after [`WAIT`].
        fn until(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
            let deadline = Instant::now() + WAIT;
            while !done(self) {
                assert!(Instant::now() < deadline, "no {what} after {WAIT:?}");
                self.next(Duration::from_millis(5));
            }
        }

        /// Gives the signals of the stream tasks to the shell until no
        /// signal arrives for [`SETTLE`].
        fn settle(&mut self) {
            while self.next(SETTLE) {}
        }

        /// Gives the next signal of a stream task to the shell. Drops a
        /// timer signal. Returns `false` if no signal arrived in `wait`.
        fn next(&mut self, wait: Duration) -> bool {
            match self.signals.recv_timeout(wait) {
                Ok(Signal::Timer { .. }) => true,
                Ok(signal) => {
                    self.shell().handle(signal);
                    true
                }
                Err(_) => false,
            }
        }

        /// Waits until the core gets the SUBACK packet of the next
        /// connection, after the broker sent the retained messages.
        fn connect(&mut self) {
            let quiet = self.shell().quiet_count();
            self.until("SUBACK", |fixture| fixture.shell().quiet_count() > quiet);
        }

        /// Waits until the core reaches the `Sleep` effect.
        fn sleep(&mut self) {
            self.until("Sleep", |fixture| fixture.shell().sleep.is_some());
        }

        /// Waits until the log has at least `count` lines, then takes it.
        fn wait_log(&mut self, count: usize) -> Vec<String> {
            self.until("log", |fixture| fixture.net().log.len() >= count);
            self.take_log()
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut self.net().log)
        }

        /// Publishes a retained message from Home Assistant, and waits until
        /// the shell received it.
        fn publish(&mut self, topic: &str, payload: &str) {
            let messages = self.shell().inspector().messages;
            {
                let mut net = self.net();
                net.broker.publish(topic, payload.as_bytes(), true);
                net.nudge.notify_one();
            }
            self.until("message", |fixture| {
                fixture.shell().inspector().messages > messages
            });
        }

        /// Starts the core as [`Fixture::start`] does, and returns the id
        /// and the token of each feature timer that the start started, in
        /// the order in which the shell started them.
        fn start_timers(&mut self, power: PowerSource) -> Vec<(crux_time::TimerId, u64)> {
            self.new_timers(|shell| {
                let settings = Settings {
                    device_id: "test".into(),
                    host: "localhost".into(),
                    port: 1883,
                    credentials: None,
                    read_only: false,
                };
                shell.start(settings, power, Some("/room/living_room".into()));
            })
        }

        /// Runs `action` on the shell, and returns the id and the token of
        /// each feature timer that the action started, in the order in
        /// which the shell started them.
        fn new_timers(
            &mut self,
            action: impl FnOnce(&mut Shell),
        ) -> Vec<(crux_time::TimerId, u64)> {
            let before = running(self.shell());
            action(self.shell());
            running(self.shell())
                .into_iter()
                .filter(|timer| !before.contains(timer))
                .collect()
        }

        /// Returns the outcomes in the effect log of `effect`, from the
        /// newest.
        fn outcomes(&mut self, effect: &str) -> Vec<String> {
            self.shell()
                .inspector()
                .effects
                .iter()
                .filter(|entry| entry.effect == effect)
                .map(|entry| format!("{}: {}", entry.request, entry.outcome))
                .collect()
        }

        /// Returns the names in the effect log, from the newest.
        fn effects(&mut self) -> Vec<&'static str> {
            self.shell()
                .inspector()
                .effects
                .iter()
                .map(|entry| entry.effect)
                .collect()
        }
    }

    /// Returns the id and the token of each feature timer that runs. The
    /// shell keeps the timers in a map, so this function sorts them by their
    /// tokens, which increase in the order of the starts.
    fn running(shell: &Shell) -> Vec<(crux_time::TimerId, u64)> {
        let mut timers: Vec<_> = shell
            .timers
            .iter()
            .filter(|(_, timer)| {
                FEATURE_TIMERS.contains(&Duration::from(timer.request.operation.duration))
            })
            .map(|(id, timer)| (*id, timer.token))
            .collect();
        timers.sort_by_key(|(_, token)| *token);
        timers
    }

    /// Returns the durations of the feature timers that run.
    fn durations(shell: &Shell) -> Vec<Duration> {
        let mut durations: Vec<_> = shell
            .timers
            .values()
            .map(|timer| Duration::from(timer.request.operation.duration))
            .filter(|duration| FEATURE_TIMERS.contains(duration))
            .collect();
        durations.sort();
        durations
    }

    /// Returns `true` if a timer with `duration` runs.
    fn runs(shell: &Shell, duration: Duration) -> bool {
        shell
            .timers
            .values()
            .any(|timer| Duration::from(timer.request.operation.duration) == duration)
    }

    /// Gives the effects of `command` to the shell, as if the core asked for
    /// them.
    fn perform(fixture: &mut Fixture, command: &mut Command<Effect, Event>) {
        let effects: Vec<Effect> = command.effects().collect();
        fixture.shell().process(effects);
    }

    #[test]
    fn a_timer_signal_with_an_old_token_is_ignored() {
        let mut fixture = Fixture::new();
        // The first feature timer that the start started.
        let (id, token) = fixture.start_timers(PowerSource::Battery)[0];

        // A signal from an aborted task that had the same id.
        fixture.shell().handle(Signal::Timer {
            id,
            token: token - 1,
        });
        assert!(running(fixture.shell()).contains(&(id, token)));

        fixture.shell().handle(Signal::Timer { id, token });
        assert!(!running(fixture.shell()).iter().any(|timer| timer.0 == id));
    }

    #[test]
    fn the_core_sends_the_status_and_the_subscribe_after_the_connack() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        assert_eq!(
            fixture.take_log(),
            [
                "open 1",
                "connect 1",
                "publish dashboard/test/status 1",
                "subscribe 1"
            ]
        );
        assert!(fixture.shell().current_connection().is_some());
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }

    #[test]
    fn a_wake_during_the_close_opens_a_stream_after_the_answer_to_the_close() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        fixture.take_log();
        let old = fixture.shell().current_connection().unwrap();

        // The idle time ends: the core publishes `asleep` and disconnects.
        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().view().asleep);

        // A tap before the DISCONNECT packet is sent wakes the core. When the
        // packet is sent, the core closes the stream. The core opens a new
        // stream only after the answer to the `Close` request, so no stream
        // is replaced.
        fixture.shell().tap(Point::new(1, 1));
        fixture.connect();
        let replaced = format!("connection {old}: item: Closed ({REPLACED})");
        assert!(!fixture.outcomes("StreamOpen").contains(&replaced));
        assert_eq!(
            fixture.take_log(),
            [
                "publish dashboard/test/status 1",
                "disconnect 1",
                "open 2",
                "connect 2",
                "publish dashboard/test/status 2",
                "subscribe 2"
            ]
        );
        assert_eq!(fixture.net().closed, [1]);
        let new = fixture.shell().current_connection().unwrap();

        // The log of the inspector starts with the newest entry.
        let entries: Vec<(&str, String)> = fixture
            .shell()
            .inspector()
            .effects
            .iter()
            .map(|entry| {
                (
                    entry.effect,
                    format!("{}: {}", entry.request, entry.outcome),
                )
            })
            .collect();
        let place = |effect: &str, text: &str| {
            entries
                .iter()
                .position(|(name, entry)| *name == effect && entry.starts_with(text))
                .unwrap_or_else(|| panic!("{effect} {text} in {entries:?}"))
        };
        let answered = place(
            "StreamClose",
            &format!("connection {old}: resolved: closed"),
        );
        let opened = place("StreamOpen", &format!("connection {new}: localhost"));
        assert!(opened < answered, "{entries:?}");
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }

    #[test]
    fn the_link_is_quiet_after_the_suback() {
        let mut fixture = Fixture::new();
        fixture.net().broker.hold_suback = true;
        fixture.start(PowerSource::Stand);
        fixture.wait_log(4);
        fixture.settle();
        assert_eq!(fixture.shell().quiet_count(), 0);

        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        assert_eq!(fixture.shell().quiet_count(), 1);
    }

    #[test]
    fn a_stream_that_ends_before_the_suback_leaves_the_link_not_quiet() {
        let mut fixture = Fixture::new();
        fixture.net().broker.hold_suback = true;
        fixture.start(PowerSource::Stand);
        fixture.wait_log(4);
        {
            let mut net = fixture.net();
            net.hang_up = true;
            net.nudge.notify_one();
        }
        fixture.until("the end of the stream", |fixture| {
            fixture.shell().current_connection().is_none()
        });
        fixture.settle();
        assert_eq!(fixture.shell().quiet_count(), 0);
    }

    #[test]
    fn a_peer_that_closes_the_stream_makes_the_core_connect_again() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        {
            let mut net = fixture.net();
            net.hang_up = true;
            net.nudge.notify_one();
        }
        fixture.until("the end of the stream", |fixture| {
            fixture.shell().current_connection().is_none()
        });

        assert_ne!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
        assert!(
            fixture.outcomes("StreamOpen")[0].ends_with(&format!("item: Closed ({PEER_CLOSED})"))
        );
        // The stream ended, so the `Close` of the core has no stream.
        assert!(fixture.outcomes("StreamClose")[0].ends_with("resolved: no stream"));
        // The broker lost the client, so it published the last will.
        assert_eq!(
            fixture.net().broker.retained("dashboard/test/status"),
            Some("offline")
        );
        // The core waits, then connects again.
        assert!(runs(fixture.shell(), RECONNECT_MIN));
    }

    #[test]
    fn a_stream_that_does_not_open_in_time_is_closed() {
        let mut fixture = Fixture::with_mode(Mode::Stall);
        fixture.start(PowerSource::Stand);
        assert_eq!(fixture.take_log(), ["open 1"]);
        let connection = fixture.shell().current_connection().unwrap();
        // The core owns the timeout of the open. Its timer expires.
        let (&id, timer) = fixture
            .shell()
            .timers
            .iter()
            .find(|(_, timer)| Duration::from(timer.request.operation.duration) == OPEN_TIMEOUT)
            .expect("the open timer");
        let token = timer.token;
        fixture.shell().handle(Signal::Timer { id, token });

        // The core closes the stream. The task stops the open, the stream
        // ends, and then the shell answers the `Close`.
        fixture.until("the end of the stream", |fixture| {
            fixture.shell().current_connection().is_none()
        });
        assert_eq!(
            fixture.outcomes("StreamClose"),
            [
                format!("connection {connection}: resolved: closed"),
                format!(
                    "connection {connection}, failed: the stream did not open in time: started"
                ),
            ]
        );
        assert!(runs(fixture.shell(), RECONNECT_MIN));
    }

    #[test]
    fn a_newer_open_replaces_the_open_stream() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        fixture.take_log();
        let old = fixture.shell().current_connection().unwrap();

        let mut open = Stream::open(9, "localhost".into(), 1883).then_send(|item| Event::Stream {
            connection: 9,
            item,
        });
        perform(&mut fixture, &mut open);
        // The old stream ends at once, and the core ends its session.
        let closed = format!("connection {old}: item: Closed ({REPLACED})");
        assert!(fixture.outcomes("StreamOpen").contains(&closed));
        assert_ne!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );

        fixture.until("the new stream", |fixture| {
            fixture.outcomes("StreamOpen")[0] == "connection 9: item: Opened"
        });
        assert_eq!(fixture.take_log(), ["open 2"]);
        let events: Vec<Event> = open.events().collect();
        assert!(matches!(
            events.as_slice(),
            [Event::Stream {
                connection: 9,
                item: StreamItem::Opened
            }]
        ));
        assert_eq!(fixture.shell().current_connection(), Some(9));
    }

    #[test]
    fn a_close_cancels_a_write_that_waits() {
        let mut fixture = Fixture::with_mode(Mode::Deaf);
        fixture.start(PowerSource::Stand);
        // The peer does not read, so the CONNECT packet waits.
        fixture.until("the CONNECT packet", |fixture| {
            !fixture.outcomes("StreamWrite").is_empty()
        });
        fixture.settle();
        let connection = fixture.shell().current_connection().unwrap();

        let mut close = Stream::close(connection, false, "a test".into())
            .then_send(move |()| Event::StreamClosed { connection });
        perform(&mut fixture, &mut close);
        fixture.until("the end of the stream", |fixture| {
            fixture.shell().current_connection().is_none()
        });

        assert_eq!(
            fixture.outcomes("StreamWrite")[0],
            format!("connection {connection}: resolved: Failed (closed)")
        );
        let events: Vec<Event> = close.events().collect();
        assert!(matches!(
            events.as_slice(),
            [Event::StreamClosed { connection: closed }] if *closed == connection
        ));
    }

    #[test]
    fn a_request_for_a_stream_that_is_not_open_is_answered_at_once() {
        let mut fixture = Fixture::new();
        let mut write = Stream::write(4, vec![0xC0, 0x00]).then_send(|outcome| Event::Written {
            connection: 4,
            outcome,
        });
        perform(&mut fixture, &mut write);
        let events: Vec<Event> = write.events().collect();
        assert!(matches!(
            events.as_slice(),
            [Event::Written { connection: 4, outcome: Written::Failed(reason) }] if reason == NOT_OPEN
        ));

        let mut close = Stream::close(4, true, "a test".into())
            .then_send(|()| Event::StreamClosed { connection: 4 });
        perform(&mut fixture, &mut close);
        assert_eq!(close.events().count(), 1);
    }

    #[test]
    fn a_tap_on_a_tile_publishes_the_service_call_of_the_core() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        fixture.take_log();
        let tile = ui::layout::tile(0).unwrap().center();
        fixture.shell().tap(tile);
        assert_eq!(fixture.wait_log(1), ["publish dashboard/test/cmd 1"]);
    }

    #[test]
    fn a_dry_run_in_the_settings_reaches_the_core() {
        let mut fixture = Fixture::new();
        fixture.start_with(PowerSource::Stand, None, true);
        fixture.connect();
        fixture.take_log();
        assert!(fixture.shell().view().read_only);

        let tile = ui::layout::tile(0).unwrap().center();
        fixture.shell().tap(tile);
        fixture.settle();
        assert_eq!(fixture.take_log(), Vec::<String>::new());

        fixture.shell().send(Event::SetReadOnly(false));
        fixture.shell().tap(tile);
        assert_eq!(fixture.wait_log(1), ["publish dashboard/test/cmd 1"]);
    }

    #[test]
    fn a_tap_on_no_control_while_asleep_wakes_the_core() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().view().asleep);
        // The header has no control. The shell still sends the tap.
        fixture.shell().tap(Point::new(100, 20));
        assert!(!fixture.shell().view().asleep);
    }

    #[test]
    fn drop_stops_the_streams_and_the_timers() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        // The idle timer and the full-refresh timer.
        assert_eq!(running(fixture.shell()).len(), 2);
        let tasks: Vec<_> = fixture
            .shell()
            .timers
            .values()
            .map(|timer| timer.task.abort_handle())
            .collect();

        drop(fixture.shell.take());
        // The stream stops with no DISCONNECT packet, so the broker
        // publishes the last will, as for a device that loses power.
        let deadline = Instant::now() + WAIT;
        while fixture.net().closed.is_empty()
            || !tasks.iter().all(tokio::task::AbortHandle::is_finished)
        {
            assert!(Instant::now() < deadline, "a task did not stop");
            fixture
                .runtime
                .block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        }
        assert_eq!(
            fixture.net().broker.retained("dashboard/test/status"),
            Some("offline")
        );
    }

    #[test]
    fn the_mqtt_log_has_the_messages_in_and_out() {
        let mut fixture = Fixture::new();
        let topic = "homeassistant/statestream/light/living_room_lights/state";
        fixture.net().broker.retain(topic, b"on");
        fixture.start(PowerSource::Stand);
        fixture.connect();
        fixture.settle();

        let log = &fixture.shell().inspector().mqtt;
        let newest = log
            .iter()
            .find(|entry| entry.direction == Direction::In && entry.packet == Packet::Publish)
            .unwrap();
        assert_eq!(newest.topic, topic);
        assert_eq!(newest.payload, "on");
        assert!(newest.retained);

        let packets: Vec<_> = log.iter().map(|entry| entry.packet).collect();
        assert_eq!(packets.last(), Some(&Packet::Connect));
        assert!(packets.contains(&Packet::ConnAck));
        // One subscribe packet is one entry, with all its topics. The
        // broker answers each one.
        let subscribes: Vec<_> = log
            .iter()
            .filter(|entry| entry.packet == Packet::Subscribe)
            .collect();
        for subscribe in &subscribes {
            assert_eq!(
                subscribe.topic,
                format!("{} topics", subscribe.topics.len())
            );
            assert!(subscribe.topics.len() > 1);
        }
        let subacks = packets.iter().filter(|packet| **packet == Packet::SubAck);
        assert_eq!(subacks.count(), subscribes.len());
        // A message is not an effect, so the effect log does not have it.
        assert!(
            fixture
                .shell()
                .inspector()
                .effects
                .iter()
                .all(|entry| !entry.request.contains(topic))
        );
    }

    #[test]
    fn the_mqtt_log_has_the_pings() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        fixture.take_log();
        // The ping timer of the session expires.
        let interval = ping_interval(KEEP_ALIVE_SECS);
        let (&id, timer) = fixture
            .shell()
            .timers
            .iter()
            .find(|(_, timer)| Some(Duration::from(timer.request.operation.duration)) == interval)
            .expect("the ping timer");
        let token = timer.token;
        fixture.shell().handle(Signal::Timer { id, token });
        assert_eq!(fixture.wait_log(1), ["ping 1"]);
        fixture.until("PINGRESP", |fixture| {
            let mqtt = &fixture.shell().inspector().mqtt;
            mqtt.iter()
                .filter(|entry| entry.packet == Packet::Ping)
                .count()
                == 2
        });
        let pings: Vec<_> = fixture
            .shell()
            .inspector()
            .mqtt
            .iter()
            .filter(|entry| entry.packet == Packet::Ping)
            .map(|entry| entry.direction)
            .collect();
        assert_eq!(pings, [Direction::In, Direction::Out]);
    }

    #[test]
    fn the_logs_do_not_show_the_password() {
        let mut fixture = Fixture::new();
        fixture.start_with(
            PowerSource::Stand,
            Some(Credentials {
                username: "dashboard".into(),
                password: "secret-password".into(),
            }),
            false,
        );
        fixture.connect();
        let inspector = fixture.shell().inspector();
        let effects = inspector
            .effects
            .iter()
            .map(|entry| format!("{} {} {}", entry.effect, entry.request, entry.outcome));
        let packets = inspector
            .mqtt
            .iter()
            .map(|entry| format!("{} {}", entry.topic, entry.payload));
        let text: Vec<String> = effects.chain(packets).collect();
        assert!(text.iter().any(|line| line.contains("localhost:1883")));
        assert!(text.iter().all(|line| !line.contains("secret-password")));
    }

    #[test]
    fn the_effect_log_has_the_effects_and_their_answers() {
        let mut fixture = Fixture::new();
        let timers = fixture.start_timers(PowerSource::Battery);
        fixture.connect();
        // The timers of the start expire, in order. On battery, one of them
        // is the idle timer, so the core sleeps.
        for (id, token) in timers {
            fixture.shell().handle(Signal::Timer { id, token });
        }
        assert!(fixture.shell().view().asleep);
        // The stream closes, so the `Close` request ends, and the core
        // continues to sleep.
        fixture.sleep();
        let effects = fixture.effects();
        for name in [
            "Render",
            "WifiOn",
            "StreamOpen",
            "StreamWrite",
            "StreamClose",
            "TimeNotifyAfter",
            "WifiQuiet",
            "Sleep",
        ] {
            assert!(effects.contains(&name), "{name} is not in {effects:?}");
        }
        let expired = fixture
            .shell()
            .inspector()
            .effects
            .iter()
            .any(|entry| entry.outcome == "resolved: the timer expired");
        assert!(expired);
    }

    #[test]
    fn a_tap_turns_the_front_light_on_and_the_sleep_turns_it_off() {
        use shared::features::light::BATTERY_LEVEL;

        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        assert_eq!(fixture.shell().light(), 0);

        // The shell answers `FrontLight` at once.
        fixture.shell().tap(Point::new(1, 1));
        assert_eq!(fixture.shell().light(), BATTERY_LEVEL);

        // The sleep turns the light off before it closes the stream.
        assert!(fixture.shell().sleep_now());
        assert_eq!(fixture.shell().light(), 0);
        fixture.sleep();
        let effects = fixture.effects();
        // The log has the newest entry first.
        let light = effects.iter().position(|effect| *effect == "FrontLight");
        let close = effects.iter().position(|effect| *effect == "StreamClose");
        assert!(close.is_some() && close < light, "{effects:?}");
    }

    #[test]
    fn a_tap_while_sleep_waits_answers_sleep_with_the_control() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        assert!(fixture.shell().sleep_now());
        fixture.sleep();
        fixture.take_log();

        // A tap on a tile wakes the device and acts on the tile. The
        // service call waits for the new connection.
        let tile = ui::layout::tile(0).unwrap().center();
        fixture.shell().tap(tile);
        assert!(fixture.shell().sleep.is_none());
        assert!(!fixture.shell().view().asleep);
        assert_eq!(fixture.take_log(), ["open 2"]);
        fixture.connect();
        let log = fixture.take_log();
        assert!(
            log.contains(&"publish dashboard/test/cmd 2".into()),
            "{log:?}"
        );

        // With no `Sleep` request, the Wake button does nothing.
        fixture.shell().wake();
        fixture.settle();
        assert_eq!(fixture.take_log(), Vec::<String>::new());
    }

    #[test]
    fn the_stand_while_sleep_waits_answers_sleep() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        assert!(fixture.shell().sleep_now());
        fixture.sleep();

        fixture.shell().set_power_source(PowerSource::Stand);
        assert!(fixture.shell().sleep.is_none());
        assert!(!fixture.shell().view().asleep);
        let answered = fixture
            .shell()
            .inspector()
            .effects
            .iter()
            .any(|entry| entry.outcome == "resolved: PowerSource");
        assert!(answered);
    }

    #[test]
    fn sleep_now_ends_the_idle_time_and_the_core_sleeps() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        fixture.take_log();
        assert!(fixture.shell().can_sleep_now());

        assert!(fixture.shell().sleep_now());
        // The core goes through its normal path to sleep. It waits for the
        // end of the stream before the next step.
        assert!(fixture.shell().view().asleep);
        assert!(fixture.shell().sleep.is_none());
        fixture.sleep();
        assert_eq!(
            fixture.take_log(),
            ["publish dashboard/test/status 1", "disconnect 1"]
        );
        // The broker got the DISCONNECT packet before the stream closed, so
        // it kept `asleep` and did not publish the last will.
        assert_eq!(fixture.net().closed, [1]);
        assert_eq!(
            fixture.net().broker.retained("dashboard/test/status"),
            Some("asleep")
        );
        let effects = fixture.effects();
        for name in ["WifiOff", "Render", "Sleep"] {
            assert!(effects.contains(&name), "{name} is not in {effects:?}");
        }
        assert!(!fixture.shell().can_sleep_now());
    }

    #[test]
    fn sleep_now_does_nothing_on_the_stand() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.connect();
        fixture.take_log();

        assert!(!fixture.shell().can_sleep_now());
        assert!(!fixture.shell().sleep_now());
        assert!(!fixture.shell().view().asleep);
        fixture.settle();
        assert_eq!(fixture.take_log(), Vec::<String>::new());
    }

    #[test]
    fn sleep_now_sends_an_intent_and_does_not_fire_a_timer() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        // A state message for a tile on the page (the living room) starts the
        // draw timer.
        fixture.publish(
            "homeassistant/statestream/light/living_room_lights/state",
            "on",
        );
        assert_eq!(
            durations(fixture.shell()),
            [DRAW_DELAY, IDLE_TIMEOUT, FULL_REFRESH_INTERVAL]
        );

        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().view().asleep);
        fixture.sleep();
        // The core clears the full-refresh timer before it sleeps. The draw
        // timer and the idle timer still run. They are now stale, and they
        // do nothing when they fire.
        assert_eq!(durations(fixture.shell()), [DRAW_DELAY, IDLE_TIMEOUT]);
    }

    #[test]
    fn sleep_now_with_a_tap_that_waits_extends_the_idle_time_once() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        // The connection is not open, so the tap waits in the outbox.
        fixture.shell().send(Event::Tapped(Some(Control::Tile(0))));

        // As after a real idle timeout, the core waits one more idle period.
        assert!(fixture.shell().sleep_now());
        assert!(!fixture.shell().view().asleep);
        assert!(fixture.shell().can_sleep_now());

        assert!(fixture.shell().sleep_now());
        let view = fixture.shell().view();
        assert!(view.asleep);
        assert_eq!(view.status, "Asleep, 1 tap not sent");
    }

    /// A change of a tile on the dining room page: the state, then the time
    /// of the update from Home Assistant. On the dining room page,
    /// `last_sync` is then this time.
    fn state_message(fixture: &mut Fixture) {
        let base = "homeassistant/statestream/light/dining_room_lights";
        for (key, payload) in [
            ("state", "on"),
            ("last_updated", "2026-11-08T14:03:21+00:00"),
        ] {
            fixture.publish(&format!("{base}/{key}"), payload);
        }
    }

    #[test]
    fn restart_keeps_the_page_and_the_last_update_and_connects_again() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        let first = fixture.shell().current_connection();
        fixture.shell().send(Event::NextPage);
        fixture.shell().send(Event::BatteryChanged(Some(64)));
        state_message(&mut fixture);
        let before = fixture.shell().view();
        assert!(before.last_sync.is_some());
        assert_eq!(before.path, "/room/dining_room");
        assert!(fixture.shell().sleep_now());
        fixture.sleep();
        assert!(fixture.shell().can_restart());
        fixture.take_log();

        assert!(fixture.shell().restart());

        // The new core opens a new stream.
        assert_eq!(fixture.take_log(), ["open 2"]);
        let after = fixture.shell().view();
        assert_eq!(after.path, before.path);
        assert_eq!(after.last_sync, before.last_sync);
        // The new core gets the charge again, as the view rounds it.
        assert_eq!(after.battery, Some(65));
        assert!(!after.asleep);
        // The new core counts its connections from the start again.
        assert_eq!(fixture.shell().current_connection(), first);
        fixture.connect();
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }

    #[test]
    fn restart_while_awake_does_nothing() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.connect();
        let first = fixture.shell().current_connection();
        fixture.take_log();
        let before = fixture.shell().view();

        assert!(!fixture.shell().can_restart());
        assert!(!fixture.shell().restart());
        assert_eq!(fixture.shell().view(), before);
        assert_eq!(fixture.shell().current_connection(), first);
        fixture.settle();
        assert_eq!(fixture.take_log(), Vec::<String>::new());
    }

    #[test]
    fn restart_keeps_a_dry_run_that_the_user_switched_on() {
        let mut fixture = Fixture::new();
        fixture.start_with(PowerSource::Battery, None, false);
        fixture.connect();
        fixture.shell().send(Event::SetReadOnly(true));
        assert!(fixture.shell().view().read_only);
        assert!(fixture.shell().sleep_now());

        assert!(fixture.shell().restart());
        assert!(fixture.shell().view().read_only);
    }

    #[test]
    fn restart_keeps_the_dry_run_of_the_settings() {
        let mut fixture = Fixture::new();
        fixture.start_with(PowerSource::Battery, None, true);
        fixture.connect();
        fixture.shell().send(Event::SetReadOnly(false));
        assert!(fixture.shell().sleep_now());

        assert!(fixture.shell().restart());
        assert!(fixture.shell().view().read_only);
    }

    #[test]
    fn restart_ignores_the_signals_of_the_old_core() {
        let mut fixture = Fixture::new();
        // The first feature timer that the start started.
        let (old_id, old_token) = fixture.start_timers(PowerSource::Battery)[0];
        fixture.connect();
        let first = fixture.shell().current_connection();
        assert!(fixture.shell().sleep_now());
        let timers = fixture.new_timers(|shell| assert!(shell.restart()));
        assert_ne!(timers.len(), 0);
        assert!(timers.iter().all(|(_, token)| *token > old_token));

        // The old stream and the new stream have the same connection id. A
        // late signal of the old stream does not reach the new core.
        let old = 1;
        fixture.shell().handle(Signal::Written {
            token: old,
            outcome: Written::Failed("late".into()),
        });
        fixture.shell().handle(Signal::Stream {
            token: old,
            item: StreamItem::Closed("late".into()),
        });
        assert_eq!(fixture.shell().current_connection(), first);
        assert_eq!(fixture.shell().streams[0].token, 2);
        // A late signal of a timer of the old core does not fire a new one.
        fixture.shell().handle(Signal::Timer {
            id: old_id,
            token: old_token,
        });
        let now = running(fixture.shell());
        assert!(timers.iter().all(|timer| now.contains(timer)));
        assert!(!fixture.shell().view().asleep);
        fixture.connect();
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }
}

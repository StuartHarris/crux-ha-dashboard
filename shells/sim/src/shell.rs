//! The shell: it runs the core on the main thread and performs its effects.
//!
//! The core has locks that it only tries to take, so one thread owns it. The
//! MQTT client and the timers run as tasks on a tokio runtime. A task sends a
//! [`Signal`] through a channel when it has an answer. The main thread reads
//! the channel and calls [`Shell::handle`], which gives the answer to the
//! core.
//!
//! A new connection does not stop the old one at once. The shell sends a
//! disconnect packet on the old connection, so the broker does not publish
//! the last will, and the old stream ends with `Disconnected`. The core
//! ignores the items of an older connection.
//!
//! The shell writes each effect and each MQTT packet to an [`Inspector`],
//! which the window shows.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime},
};

use embedded_graphics::prelude::Point;

use crux_core::{Request, capability::Operation};
use crux_time::{Instant, TimerId, operation::NotifyAfter};
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, ViewModel,
    capabilities::{
        mqtt::{Connect, MqttEvent, Publish},
        network::WifiStatus,
    },
    features::power::IDLE_TIMEOUT,
};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    inspector::{Delivery, Direction, Inspector, Packet, truncate},
    mqtt::{Link, Opener},
};

/// An answer from a task on the runtime.
#[derive(Debug)]
pub enum Signal {
    /// An item from the stream of the MQTT connection with this id.
    Mqtt {
        /// The id of the connection in the shell.
        connection: u64,
        /// The item.
        event: MqttEvent,
        /// `true` for a message that the broker retained.
        retained: bool,
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

/// An MQTT connection and the request of its stream.
struct Live {
    id: u64,
    request: Request<Connect>,
    link: Box<dyn Link>,
}

/// A timer that runs.
struct Timer {
    request: Request<NotifyAfter>,
    task: JoinHandle<()>,
    token: u64,
}

impl Timer {
    /// Returns `true` if the timer is an idle timer of the core. No other
    /// timer of the core waits for [`IDLE_TIMEOUT`].
    fn is_idle(&self) -> bool {
        Duration::from(self.request.operation.duration) == IDLE_TIMEOUT
    }
}

/// The shell.
pub struct Shell {
    core: Core<Dashboard>,
    runtime: Handle,
    notifier: Notifier,
    open: Opener,
    /// The connections whose streams did not end, from the oldest.
    connections: Vec<Live>,
    /// The id of the connection to which subscribe, publish and disconnect
    /// apply.
    current: Option<u64>,
    last_connection: u64,
    timers: HashMap<TimerId, Timer>,
    last_token: u64,
    render: bool,
    inspector: Inspector,
    /// The settings that [`Shell::start`] gave, for [`Shell::restart`].
    settings: Option<Settings>,
}

impl Drop for Shell {
    /// Stops all the tasks of the shell. A connection stops with no
    /// disconnect packet, so the broker publishes the last will, as for a
    /// device that loses power.
    fn drop(&mut self) {
        for live in &self.connections {
            live.link.abort();
        }
        for timer in self.timers.values() {
            timer.task.abort();
        }
    }
}

impl Shell {
    /// Makes a shell. Tasks run on `runtime` and send their answers with
    /// `notifier`. `open` opens an MQTT connection.
    pub fn new(runtime: Handle, notifier: Notifier, open: Opener) -> Self {
        Self {
            core: Core::new(),
            runtime,
            notifier,
            open,
            connections: Vec::new(),
            current: None,
            last_connection: 0,
            timers: HashMap::new(),
            last_token: 0,
            render: false,
            inspector: Inspector::default(),
            settings: None,
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
    ///
    /// The device reads `read_only` from its configuration after a deep
    /// sleep, but the user can switch the dry run in the simulator. The
    /// restart uses `read_only` from the settings or from the view, so a
    /// restart never switches to live service calls.
    ///
    /// The inspector and the counters of connections and timers stay. A late
    /// signal from a task of the old core finds no connection or timer, so
    /// the shell ignores it.
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
        for live in self.connections.drain(..) {
            live.link.abort();
        }
        for (_, timer) in self.timers.drain() {
            timer.task.abort();
        }
        self.current = None;
        self.core = Core::new();
        self.send(Event::Start(Start {
            settings,
            power: view.power,
            path: Some(view.path),
            last_sync: view.last_sync,
        }));
        true
    }

    /// Sends an event to the core and performs the effects.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.process(effects);
    }

    /// Sends a tap at `point` on the panel to the core, with the control at
    /// that point. The shell sends each tap, also on no control and while
    /// the device is asleep. The core decides what the tap does.
    pub fn tap(&mut self, point: Point) {
        let control = ui::hit_test(&self.view(), point);
        self.send(Event::Tapped(control));
    }

    /// Sends `Woke`, a wake that is not a tap, for example from the Wake
    /// button.
    ///
    /// The core stays in memory while the simulator sleeps, as in a light
    /// sleep. If the device is awake, the core only starts the idle time
    /// again.
    pub fn wake(&mut self) {
        eprintln!("power: wake");
        self.send(Event::Woke);
    }

    /// Returns `true` if "Sleep now" can work: the device is on battery and
    /// awake, and an idle timer runs.
    pub fn can_sleep_now(&self) -> bool {
        let view = self.view();
        view.power == PowerSource::Battery
            && !view.asleep
            && self.timers.values().any(Timer::is_idle)
    }

    /// Fires the idle timers of the core now, as if the idle time passed.
    /// Returns `false` if [`Shell::can_sleep_now`] is `false`, and then does
    /// nothing.
    ///
    /// The shell does not make the device sleep. The core decides, as after
    /// a real idle timeout. If taps wait to be sent, the core first waits one
    /// more idle period, and a second "Sleep now" ends that period.
    ///
    /// The core starts a new idle timer after each tap and ignores the older
    /// timers, so the shell fires all the idle timers. The other timers
    /// continue to run.
    pub fn sleep_now(&mut self) -> bool {
        if !self.can_sleep_now() {
            return false;
        }
        let ids: Vec<TimerId> = self
            .timers
            .iter()
            .filter(|(_, timer)| timer.is_idle())
            .map(|(id, _)| *id)
            .collect();
        eprintln!("power: sleep now");
        for id in ids {
            if let Some(mut timer) = self.timers.remove(&id) {
                timer.task.abort();
                self.inspector.effect(
                    "TimeNotifyAfter",
                    format!("timer {}", id.0),
                    "resolved early: Sleep now",
                );
                let effects = resolve(&self.core, &mut timer.request, id);
                self.process(effects);
            }
        }
        true
    }

    /// Returns the number of timers that run.
    pub fn timer_count(&self) -> usize {
        self.timers.len()
    }

    /// Returns the id of the current MQTT connection, if there is one.
    pub const fn current_connection(&self) -> Option<u64> {
        self.current
    }

    /// Returns the logs of the shell.
    pub const fn inspector(&self) -> &Inspector {
        &self.inspector
    }

    /// Returns the logs of the shell, to clear them.
    pub const fn inspector_mut(&mut self) -> &mut Inspector {
        &mut self.inspector
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
            Signal::Mqtt {
                connection,
                event,
                retained,
            } => {
                let Some(index) = self
                    .connections
                    .iter()
                    .position(|live| live.id == connection)
                else {
                    return;
                };
                let last = self.record_stream(connection, &event, retained);
                let live = &mut self.connections[index];
                let effects = resolve(&self.core, &mut live.request, event);
                if last {
                    self.connections.remove(index);
                    if self.current == Some(connection) {
                        self.current = None;
                    }
                }
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

    /// Writes an item of the stream of a connection to the logs. Returns
    /// `true` if the item is the last one.
    fn record_stream(&mut self, connection: u64, event: &MqttEvent, retained: bool) -> bool {
        let stream = format!("connection {connection}");
        match event {
            MqttEvent::Connected => {
                eprintln!("mqtt: connected");
                self.inspector
                    .effect("MqttConnect", stream, "resolved: Connected");
                self.inspector.packet(
                    Direction::In,
                    Packet::ConnAck,
                    "",
                    String::new(),
                    false,
                    Delivery::Received,
                );
                false
            }
            MqttEvent::Message(message) => {
                self.inspector.packet(
                    Direction::In,
                    Packet::Publish,
                    message.topic.clone(),
                    truncate(&message.payload),
                    retained,
                    Delivery::Received,
                );
                false
            }
            MqttEvent::Disconnected { reason } => {
                let reason = reason.as_deref().unwrap_or("disconnect");
                eprintln!("mqtt: disconnected ({reason})");
                self.inspector.effect(
                    "MqttConnect",
                    stream,
                    format!("resolved: Disconnected ({reason})"),
                );
                self.inspector.packet(
                    Direction::In,
                    Packet::Disconnect,
                    "",
                    reason.to_string(),
                    false,
                    Delivery::Received,
                );
                true
            }
        }
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
            Effect::MqttConnect(request) => self.connect(request),
            Effect::MqttSubscribe(request) => {
                self.subscribe(&request.operation.topics);
                Vec::new()
            }
            Effect::MqttPublish(request) => {
                self.publish(&request.operation);
                Vec::new()
            }
            Effect::MqttDisconnect(_) => {
                self.disconnect_current();
                Vec::new()
            }
            Effect::WifiOn(mut request) => {
                self.inspector
                    .effect("WifiOn", String::new(), "resolved: Connected");
                resolve(&self.core, &mut request, WifiStatus::Connected)
            }
            Effect::WifiOff(_) => {
                self.inspector.effect(
                    "WifiOff",
                    String::new(),
                    "nothing to do: the simulator has no Wi-Fi",
                );
                Vec::new()
            }
            Effect::Sleep(_) => {
                eprintln!("power: asleep. Click the panel or press W to wake it.");
                self.inspector.effect(
                    "Sleep",
                    String::new(),
                    "asleep: click the panel or press W to wake it",
                );
                Vec::new()
            }
            Effect::TimeNow(mut request) => {
                let now = SystemTime::now();
                let local: chrono::DateTime<chrono::Local> = now.into();
                self.inspector.effect(
                    "TimeNow",
                    String::new(),
                    format!("resolved: {}", local.format("%H:%M:%S%.3f")),
                );
                resolve(&self.core, &mut request, Instant::from(now))
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

    /// Returns the connection to which subscribe, publish and disconnect
    /// apply.
    fn current(&self) -> Option<&Live> {
        let current = self.current?;
        self.connections.iter().find(|live| live.id == current)
    }

    /// Subscribes to `topics` on the current connection.
    fn subscribe(&mut self, topics: &[String]) {
        let delivery = self.current().map_or(Delivery::NoConnection, |live| {
            eprintln!("mqtt: subscribe to {} topics", topics.len());
            delivery("subscribe", live.link.subscribe(topics))
        });
        self.inspector.effect(
            "MqttSubscribe",
            format!("{} topics", topics.len()),
            outcome(&delivery),
        );
        self.inspector.subscribe(topics, delivery);
    }

    /// Publishes a message on the current connection.
    fn publish(&mut self, publish: &Publish) {
        let payload = String::from_utf8_lossy(&publish.payload);
        let delivery = self.current().map_or(Delivery::NoConnection, |live| {
            eprintln!("mqtt: publish {} {payload}", publish.topic);
            delivery("publish", live.link.publish(publish))
        });
        self.inspector
            .effect("MqttPublish", publish.topic.clone(), outcome(&delivery));
        self.inspector.packet(
            Direction::Out,
            Packet::Publish,
            publish.topic.clone(),
            truncate(&publish.payload),
            publish.retain,
            delivery,
        );
    }

    /// Sends a disconnect packet on the current connection. Its stream ends
    /// later with `Disconnected`.
    fn disconnect_current(&mut self) {
        let delivery = self.current().map_or(Delivery::NoConnection, |live| {
            eprintln!("mqtt: disconnect");
            delivery("disconnect", live.link.disconnect())
        });
        if let Some(current) = self.current {
            self.inspector.effect(
                "MqttDisconnect",
                format!("connection {current}"),
                outcome(&delivery),
            );
            self.inspector.packet(
                Direction::Out,
                Packet::Disconnect,
                "",
                String::new(),
                false,
                delivery,
            );
        }
        self.current = None;
    }

    /// Opens a connection and makes it the current connection. If a
    /// connection is current, the shell disconnects it first.
    fn connect(&mut self, request: Request<Connect>) -> Vec<Effect> {
        self.disconnect_current();
        self.last_connection += 1;
        let id = self.last_connection;
        let connect = &request.operation;
        // Do not log the credentials of the request.
        let target = format!("{}:{} as {}", connect.host, connect.port, connect.client_id);
        eprintln!("mqtt: connect to {target}");
        self.inspector.effect(
            "MqttConnect",
            target.clone(),
            format!("opened connection {id}"),
        );
        self.inspector.packet(
            Direction::Out,
            Packet::Connect,
            "",
            target,
            false,
            Delivery::Sent,
        );
        let notifier = self.notifier.clone();
        let link = (self.open)(
            connect,
            Box::new(move |event, retained| {
                notifier.send(Signal::Mqtt {
                    connection: id,
                    event,
                    retained,
                });
            }),
        );
        self.connections.push(Live { id, request, link });
        self.current = Some(id);
        Vec::new()
    }
}

/// Converts the result of a request to the client to a delivery. Logs an
/// error.
fn delivery(request: &str, result: Result<(), String>) -> Delivery {
    match result {
        Ok(()) => Delivery::Sent,
        Err(error) => {
            eprintln!("mqtt: the {request} failed: {error}");
            Delivery::Failed(error)
        }
    }
}

/// Returns the text of the effect log for a delivery.
fn outcome(delivery: &Delivery) -> String {
    match delivery {
        Delivery::Received => "received".into(),
        Delivery::Sent => "sent".into(),
        Delivery::NoConnection => "not sent: no connection".into(),
        Delivery::Failed(error) => format!("failed: {error}"),
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
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use embedded_graphics::prelude::Point;
    use shared::{
        Control, Event, PowerSource, Settings,
        capabilities::mqtt::{Connect, Credentials, Message, MqttEvent, Publish},
        features::{connection::CLOCK_INTERVAL, refresh::FULL_REFRESH_INTERVAL},
    };
    use tokio::runtime::Runtime;

    use super::{IDLE_TIMEOUT, Notifier, Shell, Signal};
    use crate::{
        inspector::{Delivery, Direction, Packet},
        mqtt::{Link, Opener, Sink},
    };

    type Log = Arc<Mutex<Vec<String>>>;

    /// A connection that only records what the shell asks of it.
    struct FakeLink {
        number: usize,
        log: Log,
    }

    impl FakeLink {
        fn record(&self, text: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{text} {}", self.number));
        }
    }

    impl Link for FakeLink {
        fn subscribe(&self, _: &[String]) -> Result<(), String> {
            self.record("subscribe");
            Ok(())
        }

        fn publish(&self, publish: &Publish) -> Result<(), String> {
            self.record(&format!("publish {}", publish.topic));
            Ok(())
        }

        fn disconnect(&self) -> Result<(), String> {
            self.record("disconnect");
            Ok(())
        }

        fn abort(&self) {
            self.record("abort");
        }
    }

    fn fake_opener(log: &Log) -> Opener {
        let log = log.clone();
        let mut count = 0;
        Box::new(move |_: &Connect, _: Sink| {
            count += 1;
            log.lock().unwrap().push(format!("open {count}"));
            Box::new(FakeLink {
                number: count,
                log: log.clone(),
            }) as Box<dyn Link>
        })
    }

    struct Fixture {
        runtime: Runtime,
        shell: Option<Shell>,
        log: Log,
    }

    impl Fixture {
        fn new() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_time()
                .build()
                .unwrap();
            let (sender, _signals) = mpsc::channel();
            let notifier = Notifier::new(sender, Arc::new(|| {}));
            let log = Log::default();
            let shell = Shell::new(runtime.handle().clone(), notifier, fake_opener(&log));
            Self {
                runtime,
                shell: Some(shell),
                log,
            }
        }

        fn shell(&mut self) -> &mut Shell {
            self.shell.as_mut().unwrap()
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

        fn mqtt(&mut self, connection: u64, event: MqttEvent) {
            self.shell().handle(Signal::Mqtt {
                connection,
                event,
                retained: false,
            });
        }

        /// Fires the idle timer, with its token.
        fn fire_timer(&mut self) {
            let (id, token) = self.timer().unwrap();
            self.shell().handle(Signal::Timer { id, token });
        }

        /// Returns the id and the token of the idle timer.
        fn timer(&mut self) -> Option<(crux_time::TimerId, u64)> {
            let mut idle = self
                .shell()
                .timers
                .iter()
                .filter(|(_, timer)| timer.is_idle());
            let found = idle.next().map(|(id, timer)| (*id, timer.token));
            assert!(idle.next().is_none());
            found
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    #[test]
    fn a_timer_signal_with_an_old_token_is_ignored() {
        let mut fixture = Fixture::new();
        // On battery, the core starts the idle timer.
        fixture.start(PowerSource::Battery);
        let (id, token) = fixture.timer().unwrap();

        // A signal from an aborted task that had the same id.
        fixture.shell().handle(Signal::Timer {
            id,
            token: token - 1,
        });
        assert_eq!(fixture.timer(), Some((id, token)));

        fixture.shell().handle(Signal::Timer { id, token });
        assert_eq!(fixture.timer(), None);
    }

    #[test]
    fn a_new_connection_disconnects_the_old_one_and_stays_current() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        assert_eq!(
            fixture.take_log(),
            ["open 1", "subscribe 1", "publish dashboard/test/status 1"]
        );

        // The idle timer expires: the core publishes `asleep`, disconnects
        // and sleeps.
        fixture.fire_timer();
        assert!(fixture.shell().view().asleep);
        assert_eq!(
            fixture.take_log(),
            ["publish dashboard/test/status 1", "disconnect 1"]
        );

        // A tap before the old stream ends wakes the core, which opens a
        // second connection. The old connection is not aborted, so its
        // disconnect completes.
        fixture.shell().tap(Point::new(1, 1));
        assert_eq!(fixture.take_log(), ["open 2"]);

        // The old stream ends. The core ignores it, and the new connection
        // stays current.
        fixture.mqtt(1, MqttEvent::Disconnected { reason: None });
        fixture.mqtt(2, MqttEvent::Connected);
        assert_eq!(
            fixture.take_log(),
            ["subscribe 2", "publish dashboard/test/status 2"]
        );
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }

    #[test]
    fn a_tap_on_a_tile_publishes_the_service_call_of_the_core() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        let tile = ui::layout::tile(0).unwrap().center();
        fixture.shell().tap(tile);
        assert_eq!(fixture.take_log(), ["publish dashboard/test/cmd 1"]);
    }

    #[test]
    fn a_dry_run_in_the_settings_reaches_the_core() {
        let mut fixture = Fixture::new();
        fixture.start_with(PowerSource::Stand, None, true);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        assert!(fixture.shell().view().read_only);

        let tile = ui::layout::tile(0).unwrap().center();
        fixture.shell().tap(tile);
        assert_eq!(fixture.take_log(), Vec::<String>::new());

        fixture.shell().send(Event::SetReadOnly(false));
        fixture.shell().tap(tile);
        assert_eq!(fixture.take_log(), ["publish dashboard/test/cmd 1"]);
    }

    #[test]
    fn a_tap_on_no_control_while_asleep_wakes_the_core() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.fire_timer();
        assert!(fixture.shell().view().asleep);
        // The header has no control. The shell still sends the tap.
        fixture.shell().tap(Point::new(100, 20));
        assert!(!fixture.shell().view().asleep);
    }

    #[test]
    fn drop_stops_the_connections_and_the_timers() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        let tasks: Vec<_> = fixture
            .shell()
            .timers
            .values()
            .map(|timer| timer.task.abort_handle())
            .collect();
        // The idle timer and the full-refresh timer.
        assert_eq!(tasks.len(), 2);

        drop(fixture.shell.take());
        assert_eq!(fixture.take_log(), ["abort 1"]);
        fixture
            .runtime
            .block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
        assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished));
    }

    #[test]
    fn the_mqtt_log_has_the_messages_in_and_out() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.mqtt(1, MqttEvent::Connected);
        let topic = "homeassistant/statestream/light/kitchen/state";
        fixture.shell().handle(Signal::Mqtt {
            connection: 1,
            event: MqttEvent::Message(Message {
                topic: topic.into(),
                payload: b"on".to_vec(),
            }),
            retained: true,
        });

        let log = &fixture.shell().inspector().mqtt;
        let newest = log.iter().next().unwrap();
        assert_eq!(
            (newest.direction, newest.packet, newest.topic.as_str()),
            (Direction::In, Packet::Publish, topic)
        );
        assert_eq!(newest.payload, "on");
        assert!(newest.retained);
        assert_eq!(newest.delivery, Delivery::Received);

        let packets: Vec<_> = log.iter().map(|entry| entry.packet).collect();
        assert_eq!(packets.last(), Some(&Packet::Connect));
        assert!(packets.contains(&Packet::ConnAck));
        // One subscribe request is one entry, with all its topics.
        let subscribes: Vec<_> = log
            .iter()
            .filter(|entry| entry.packet == Packet::Subscribe)
            .collect();
        assert_eq!(subscribes.len(), 1);
        assert_eq!(
            subscribes[0].topic,
            format!("{} topics", subscribes[0].topics.len())
        );
        assert!(subscribes[0].topics.len() > 1);
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
        fixture.mqtt(1, MqttEvent::Connected);
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
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.fire_timer();
        let effects: Vec<_> = fixture
            .shell()
            .inspector()
            .effects
            .iter()
            .map(|entry| entry.effect)
            .collect();
        for name in [
            "Render",
            "WifiOn",
            "MqttConnect",
            "MqttSubscribe",
            "MqttPublish",
            "TimeNotifyAfter",
            "MqttDisconnect",
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

    /// Returns the durations of the timers that run.
    fn durations(shell: &Shell) -> Vec<Duration> {
        shell
            .timers
            .values()
            .map(|timer| Duration::from(timer.request.operation.duration))
            .collect()
    }

    #[test]
    fn sleep_now_fires_the_idle_timer_and_the_core_sleeps() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        assert!(fixture.shell().can_sleep_now());

        assert!(fixture.shell().sleep_now());
        // The core goes through its normal path to sleep.
        assert!(fixture.shell().view().asleep);
        assert_eq!(
            fixture.take_log(),
            ["publish dashboard/test/status 1", "disconnect 1"]
        );
        let effects: Vec<_> = fixture
            .shell()
            .inspector()
            .effects
            .iter()
            .map(|entry| entry.effect)
            .collect();
        for name in ["WifiOff", "Render", "Sleep"] {
            assert!(effects.contains(&name), "{name} is not in {effects:?}");
        }
        assert!(!fixture.shell().can_sleep_now());
    }

    #[test]
    fn sleep_now_does_nothing_on_the_stand() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Stand);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();

        assert!(!fixture.shell().can_sleep_now());
        assert!(!fixture.shell().sleep_now());
        assert!(!fixture.shell().view().asleep);
        assert_eq!(fixture.take_log(), Vec::<String>::new());
    }

    #[test]
    fn sleep_now_does_not_fire_the_other_timers() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        // A state message for a tile on the page (the living room) reads the
        // clock, and the core starts the minute timer.
        fixture.mqtt(
            1,
            MqttEvent::Message(Message {
                topic: "homeassistant/statestream/light/living_room_lights/state".into(),
                payload: b"on".to_vec(),
            }),
        );
        let mut running = durations(fixture.shell());
        running.sort();
        assert_eq!(
            running,
            [IDLE_TIMEOUT, CLOCK_INTERVAL, FULL_REFRESH_INTERVAL]
        );

        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().view().asleep);
        // The core clears the full-refresh timer before it sleeps.
        assert_eq!(durations(fixture.shell()), [CLOCK_INTERVAL]);
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
        assert_eq!(view.status, "Asleep, on battery, 1 tap not sent");
    }

    /// A state message for a tile on the living room page: the core reads the
    /// clock and sets `last_sync`.
    fn state_message(fixture: &mut Fixture, connection: u64) {
        fixture.mqtt(
            connection,
            MqttEvent::Message(Message {
                topic: "homeassistant/statestream/light/living_room_lights/state".into(),
                payload: b"on".to_vec(),
            }),
        );
    }

    #[test]
    fn restart_keeps_the_page_and_the_last_update_and_connects_again() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        state_message(&mut fixture, 1);
        fixture.shell().send(Event::NextPage);
        let before = fixture.shell().view();
        assert!(before.last_sync.is_some());
        assert_ne!(before.path, "/room/living_room");
        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().can_restart());
        fixture.take_log();

        assert!(fixture.shell().restart());

        // The shell aborts the old connection, and the new core opens a new
        // one.
        assert_eq!(fixture.take_log(), ["abort 1", "open 2"]);
        let after = fixture.shell().view();
        assert_eq!(after.path, before.path);
        assert_eq!(after.last_sync, before.last_sync);
        assert!(!after.asleep);
        assert_eq!(fixture.shell().current_connection(), Some(2));
        fixture.mqtt(2, MqttEvent::Connected);
        assert_eq!(
            fixture.shell().view().connection,
            shared::Connection::Online
        );
    }

    #[test]
    fn restart_while_awake_does_nothing() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        let before = fixture.shell().view();

        assert!(!fixture.shell().can_restart());
        assert!(!fixture.shell().restart());
        assert_eq!(fixture.shell().view(), before);
        assert_eq!(fixture.shell().current_connection(), Some(1));
        assert_eq!(fixture.take_log(), Vec::<String>::new());
    }

    #[test]
    fn restart_keeps_a_dry_run_that_the_user_switched_on() {
        let mut fixture = Fixture::new();
        fixture.start_with(PowerSource::Battery, None, false);
        fixture.mqtt(1, MqttEvent::Connected);
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
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.shell().send(Event::SetReadOnly(false));
        assert!(fixture.shell().sleep_now());

        assert!(fixture.shell().restart());
        assert!(fixture.shell().view().read_only);
    }

    #[test]
    fn restart_ignores_the_signals_of_the_old_core() {
        let mut fixture = Fixture::new();
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        let (old_id, old_token) = fixture.timer().unwrap();
        assert!(fixture.shell().sleep_now());
        assert!(fixture.shell().restart());
        fixture.take_log();
        let (id, token) = fixture.timer().unwrap();
        assert!(token > old_token);

        // A late signal of the old connection does not reach the core.
        fixture.mqtt(1, MqttEvent::Disconnected { reason: None });
        assert_eq!(fixture.shell().current_connection(), Some(2));
        // A late signal of the old idle timer does not fire the new one.
        fixture.shell().handle(Signal::Timer {
            id: old_id,
            token: old_token,
        });
        assert_eq!(fixture.timer(), Some((id, token)));
        assert!(!fixture.shell().view().asleep);
    }
}

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

use crux_core::{Request, capability::Operation};
use crux_time::{Instant, TimerId, operation::NotifyAfter};
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, ViewModel,
    capabilities::{
        mqtt::{Connect, MqttEvent, Publish},
        network::WifiStatus,
    },
};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    inspector::{Delivery, Direction, Inspector, Packet, truncate},
    mqtt::{Link, Opener},
};

/// How often the shell checks the local offset from UTC.
const OFFSET_CHECK: Duration = Duration::from_secs(60);

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
    /// Time to check the local offset from UTC.
    CheckOffset,
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
    read_only: bool,
    render: bool,
    /// The offset from UTC that the shell last sent to the core.
    utc_offset_minutes: i16,
    /// The local offset from UTC at the last check. The shell sends a new
    /// offset only when this value changes, so an offset that the user sets
    /// stays until the local offset changes.
    local_offset_minutes: i16,
    offset_task: JoinHandle<()>,
    inspector: Inspector,
}

impl Drop for Shell {
    /// Stops all the tasks of the shell. A connection stops with no
    /// disconnect packet, so the broker publishes the last will, as for a
    /// device that loses power.
    fn drop(&mut self) {
        self.offset_task.abort();
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
    ///
    /// If `read_only` is `true`, the shell does not publish service calls
    /// (the topics that end in `/cmd`), so no light in the house changes.
    pub fn new(runtime: Handle, notifier: Notifier, open: Opener, read_only: bool) -> Self {
        let ticker = notifier.clone();
        let offset_task = runtime.spawn(async move {
            let mut interval = tokio::time::interval(OFFSET_CHECK);
            interval.tick().await;
            loop {
                interval.tick().await;
                ticker.send(Signal::CheckOffset);
            }
        });
        let offset = local_offset_minutes();
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
            read_only,
            render: false,
            utc_offset_minutes: offset,
            local_offset_minutes: offset,
            offset_task,
            inspector: Inspector::default(),
        }
    }

    /// Sends `Start` to the core.
    pub fn start(&mut self, settings: Settings, power: PowerSource, path: Option<String>) {
        self.send(Event::Start(Start {
            settings,
            power,
            path,
            utc_offset_minutes: self.utc_offset_minutes,
            last_sync: None,
        }));
    }

    /// Sends an event to the core and performs the effects.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.process(effects);
    }

    /// Wakes the device if it sleeps.
    ///
    /// The core stays in memory while the simulator sleeps, as in a light
    /// sleep, so the shell sends `Woke`.
    pub fn wake(&mut self) {
        if self.view().asleep {
            eprintln!("power: woke");
            self.send(Event::Woke);
        }
    }

    /// Returns `true` if the shell does not publish service calls.
    pub const fn read_only(&self) -> bool {
        self.read_only
    }

    /// Starts or stops the publication of service calls. If `read_only` is
    /// `true`, a click changes no device in the house.
    pub const fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Returns the offset from UTC, in minutes, that the core uses.
    pub const fn utc_offset_minutes(&self) -> i16 {
        self.utc_offset_minutes
    }

    /// Sends a new offset from UTC to the core, for example to test the
    /// start of summer time. The offset stays until the local offset of the
    /// computer changes.
    pub fn set_utc_offset(&mut self, minutes: i16) {
        if minutes != self.utc_offset_minutes {
            self.utc_offset_minutes = minutes;
            self.send(Event::UtcOffsetChanged(minutes));
        }
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
            Signal::CheckOffset => {
                self.check_offset(local_offset_minutes());
            }
        }
    }

    /// Sends the local offset from UTC to the core if it changed since the
    /// last check.
    fn check_offset(&mut self, minutes: i16) {
        if minutes != self.local_offset_minutes {
            eprintln!("time: the UTC offset is now {minutes} minutes");
            self.local_offset_minutes = minutes;
            self.set_utc_offset(minutes);
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

    /// Publishes a message on the current connection. If the shell is
    /// read-only, it does not publish a service call.
    fn publish(&mut self, publish: &Publish) {
        let payload = String::from_utf8_lossy(&publish.payload);
        let delivery = if self.read_only && publish.topic.ends_with("/cmd") {
            eprintln!(
                "mqtt: read-only, so not published: {} {payload}",
                publish.topic
            );
            Delivery::NotSent
        } else if let Some(live) = self.current() {
            eprintln!("mqtt: publish {} {payload}", publish.topic);
            delivery("publish", live.link.publish(publish))
        } else {
            Delivery::NoConnection
        };
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
        Delivery::NotSent => "not sent: read-only".into(),
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

/// Returns the local offset from UTC now, in minutes.
fn local_offset_minutes() -> i16 {
    let seconds = chrono::Local::now().offset().local_minus_utc();
    i16::try_from(seconds / 60).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use shared::{
        Event, PowerSource, Settings,
        capabilities::mqtt::{Connect, Credentials, Message, MqttEvent, Publish},
    };
    use tokio::runtime::Runtime;

    use super::{Notifier, Shell, Signal};
    use crate::{
        inspector::{Delivery, Direction, MqttEntry, Packet},
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
        fn new(read_only: bool) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_time()
                .build()
                .unwrap();
            let (sender, _signals) = mpsc::channel();
            let notifier = Notifier::new(sender, Arc::new(|| {}));
            let log = Log::default();
            let shell = Shell::new(
                runtime.handle().clone(),
                notifier,
                fake_opener(&log),
                read_only,
            );
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
            self.start_with(power, None);
        }

        fn start_with(&mut self, power: PowerSource, credentials: Option<Credentials>) {
            let settings = Settings {
                device_id: "test".into(),
                host: "localhost".into(),
                port: 1883,
                credentials,
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

        /// Fires the timer that runs, with its token.
        fn fire_timer(&mut self) {
            let (id, token) = self.timer().unwrap();
            self.shell().handle(Signal::Timer { id, token });
        }

        fn timer(&mut self) -> Option<(crux_time::TimerId, u64)> {
            let timers = &self.shell().timers;
            assert!(timers.len() <= 1);
            timers.iter().next().map(|(id, timer)| (*id, timer.token))
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    #[test]
    fn a_timer_signal_with_an_old_token_is_ignored() {
        let mut fixture = Fixture::new(true);
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
        let mut fixture = Fixture::new(true);
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

        // A wake before the old stream ends opens a second connection. The
        // old connection is not aborted, so its disconnect completes.
        fixture.shell().wake();
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
    fn read_only_does_not_publish_service_calls() {
        for (read_only, expected) in [
            (true, vec![]),
            (false, vec!["publish dashboard/test/cmd 1".to_string()]),
        ] {
            let mut fixture = Fixture::new(read_only);
            fixture.start(PowerSource::Stand);
            fixture.mqtt(1, MqttEvent::Connected);
            fixture.take_log();
            fixture.shell().send(Event::TileTapped(0));
            assert_eq!(fixture.take_log(), expected, "read_only = {read_only}");
        }
    }

    #[test]
    fn drop_stops_the_connections_and_the_timers() {
        let mut fixture = Fixture::new(true);
        fixture.start(PowerSource::Battery);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();
        let tasks: Vec<_> = fixture
            .shell()
            .timers
            .values()
            .map(|timer| timer.task.abort_handle())
            .collect();
        assert_eq!(tasks.len(), 1);

        drop(fixture.shell.take());
        assert_eq!(fixture.take_log(), ["abort 1"]);
        fixture
            .runtime
            .block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
        assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished));
    }

    /// Returns the MQTT log entries with a topic that ends in `/cmd`.
    fn commands(shell: &Shell) -> Vec<MqttEntry> {
        shell
            .inspector()
            .mqtt
            .iter()
            .filter(|entry| entry.topic.ends_with("/cmd"))
            .cloned()
            .collect()
    }

    #[test]
    fn read_only_can_change_while_the_shell_runs() {
        let mut fixture = Fixture::new(false);
        fixture.start(PowerSource::Stand);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.take_log();

        fixture.shell().set_read_only(true);
        assert!(fixture.shell().read_only());
        fixture.shell().send(Event::TileTapped(0));
        assert_eq!(fixture.take_log(), Vec::<String>::new());

        fixture.shell().set_read_only(false);
        fixture.shell().send(Event::TileTapped(0));
        assert_eq!(fixture.take_log(), ["publish dashboard/test/cmd 1"]);

        // The newest entry is first.
        let deliveries: Vec<_> = commands(fixture.shell())
            .into_iter()
            .map(|entry| (entry.direction, entry.packet, entry.delivery))
            .collect();
        assert_eq!(
            deliveries,
            [
                (Direction::Out, Packet::Publish, Delivery::Sent),
                (Direction::Out, Packet::Publish, Delivery::NotSent),
            ]
        );
    }

    #[test]
    fn a_service_call_in_read_only_mode_is_in_the_logs_as_not_sent() {
        let mut fixture = Fixture::new(true);
        fixture.start(PowerSource::Stand);
        fixture.mqtt(1, MqttEvent::Connected);
        fixture.shell().send(Event::TileTapped(0));

        let [entry] = commands(fixture.shell()).try_into().unwrap();
        assert_eq!(entry.delivery, Delivery::NotSent);
        assert_ne!(entry.payload, "");
        let effect = fixture.shell().inspector().effects.iter().next().unwrap();
        assert_eq!(effect.effect, "MqttPublish");
        assert_eq!(effect.request, "dashboard/test/cmd");
        assert_eq!(effect.outcome, "not sent: read-only");
    }

    #[test]
    fn the_mqtt_log_has_the_messages_in_and_out() {
        let mut fixture = Fixture::new(true);
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
        let mut fixture = Fixture::new(true);
        fixture.start_with(
            PowerSource::Stand,
            Some(Credentials {
                username: "dashboard".into(),
                password: "secret-password".into(),
            }),
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
        let mut fixture = Fixture::new(true);
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

    #[test]
    fn an_offset_that_the_user_sets_stays_until_the_local_offset_changes() {
        let mut fixture = Fixture::new(true);
        fixture.start(PowerSource::Stand);
        let local = fixture.shell().utc_offset_minutes();

        let summer = local + 60;
        fixture.shell().set_utc_offset(summer);
        assert_eq!(fixture.shell().utc_offset_minutes(), summer);

        // The local offset did not change, so the check keeps the offset.
        fixture.shell().check_offset(local);
        assert_eq!(fixture.shell().utc_offset_minutes(), summer);

        // The local offset changed, so the shell sends it.
        fixture.shell().check_offset(local - 60);
        assert_eq!(fixture.shell().utc_offset_minutes(), local - 60);
    }
}

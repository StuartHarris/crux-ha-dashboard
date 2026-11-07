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
        mqtt::{Connect, MqttEvent},
        network::WifiStatus,
    },
};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::mqtt::{Link, Opener};

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
    utc_offset_minutes: i16,
    offset_task: JoinHandle<()>,
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
            utc_offset_minutes: local_offset_minutes(),
            offset_task,
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
            Signal::Mqtt { connection, event } => {
                let Some(index) = self
                    .connections
                    .iter()
                    .position(|live| live.id == connection)
                else {
                    return;
                };
                let last = match &event {
                    MqttEvent::Connected => {
                        eprintln!("mqtt: connected");
                        false
                    }
                    MqttEvent::Message(_) => false,
                    MqttEvent::Disconnected { reason } => {
                        let reason = reason.as_deref().unwrap_or("disconnect");
                        eprintln!("mqtt: disconnected ({reason})");
                        true
                    }
                };
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
                    let effects = resolve(&self.core, &mut timer.request, id);
                    self.process(effects);
                }
            }
            Signal::CheckOffset => {
                let minutes = local_offset_minutes();
                if minutes != self.utc_offset_minutes {
                    eprintln!("time: the UTC offset is now {minutes} minutes");
                    self.utc_offset_minutes = minutes;
                    self.send(Event::UtcOffsetChanged(minutes));
                }
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
                Vec::new()
            }
            Effect::MqttConnect(request) => self.connect(request),
            Effect::MqttSubscribe(request) => {
                if let Some(live) = self.current() {
                    let topics = &request.operation.topics;
                    eprintln!("mqtt: subscribe to {} topics", topics.len());
                    if let Err(error) = live.link.subscribe(topics) {
                        eprintln!("mqtt: the subscribe failed: {error}");
                    }
                }
                Vec::new()
            }
            Effect::MqttPublish(request) => {
                let publish = &request.operation;
                let payload = String::from_utf8_lossy(&publish.payload);
                if self.read_only && publish.topic.ends_with("/cmd") {
                    eprintln!(
                        "mqtt: read-only, so not published: {} {payload}",
                        publish.topic
                    );
                } else if let Some(live) = self.current() {
                    eprintln!("mqtt: publish {} {payload}", publish.topic);
                    if let Err(error) = live.link.publish(publish) {
                        eprintln!("mqtt: the publish failed: {error}");
                    }
                }
                Vec::new()
            }
            Effect::MqttDisconnect(_) => {
                self.disconnect_current();
                Vec::new()
            }
            Effect::WifiOn(mut request) => resolve(&self.core, &mut request, WifiStatus::Connected),
            Effect::WifiOff(_) => Vec::new(),
            Effect::Sleep(_) => {
                eprintln!("power: asleep. Click the panel or press W to wake it.");
                Vec::new()
            }
            Effect::TimeNow(mut request) => {
                resolve(&self.core, &mut request, Instant::from(SystemTime::now()))
            }
            Effect::TimeNotifyAfter(request) => {
                let id = request.operation.id;
                let duration = Duration::from(request.operation.duration);
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
                if let Some(timer) = self.timers.remove(&id) {
                    timer.task.abort();
                }
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

    /// Sends a disconnect packet on the current connection. Its stream ends
    /// later with `Disconnected`.
    fn disconnect_current(&mut self) {
        if let Some(live) = self.current() {
            eprintln!("mqtt: disconnect");
            if let Err(error) = live.link.disconnect() {
                eprintln!("mqtt: the disconnect failed: {error}");
            }
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
        eprintln!(
            "mqtt: connect to {}:{} as {}",
            connect.host, connect.port, connect.client_id
        );
        let notifier = self.notifier.clone();
        let link = (self.open)(
            connect,
            Box::new(move |event| {
                notifier.send(Signal::Mqtt {
                    connection: id,
                    event,
                });
            }),
        );
        self.connections.push(Live { id, request, link });
        self.current = Some(id);
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
        capabilities::mqtt::{Connect, MqttEvent, Publish},
    };
    use tokio::runtime::Runtime;

    use super::{Notifier, Shell, Signal};
    use crate::mqtt::{Link, Opener, Sink};

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
            let settings = Settings {
                device_id: "test".into(),
                host: "localhost".into(),
                port: 1883,
                credentials: None,
            };
            self.shell()
                .start(settings, power, Some("/room/living_room".into()));
        }

        fn mqtt(&mut self, connection: u64, event: MqttEvent) {
            self.shell().handle(Signal::Mqtt { connection, event });
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
}

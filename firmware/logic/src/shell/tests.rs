use alloc::{format, string::String, vec, vec::Vec};
use core::{
    cell::RefCell,
    ops::{Deref, DerefMut},
    time::Duration,
};

use embedded_graphics::prelude::Point;
use shared::{
    Boot, BootLine, Cmd, Connection, Effect, Event, Firmware, KeyEdge, PowerSource, ResetReason,
    capabilities::{
        log::{self, Area, Level, Log, Uptime},
        power::Wake,
        radio::{Joined, Lease, Radio, Restart, Scanned, SignalRead},
        stream::{StreamItem, Written},
    },
    fake::{Broker, FakeRadio, radio::HALL, session_timers},
    features::{
        connection::{CLOSE_TIMEOUT, RECONNECT_MIN},
        light::{DEFAULT_BATTERY_LEVEL, DEFAULT_STAND_LEVEL, LIGHT_TIMEOUT},
        logs::{self, Kept, Line, Message, Source},
        power::IDLE_TIMEOUT,
        refresh::DRAW_DELAY,
        wifi::JOIN_TIMEOUT,
    },
    mqtt::{
        CONNACK_TIMEOUT, MAX_PACKET,
        codec::server::{self, Packet},
        framer::{Frame, Framer},
    },
};

use crux_time::TimerId;

use super::{Job, RadioAnswer, RadioOp, Shell, Signal, StreamOp, TimerOp, start};
use crate::config::Config;

const CONFIG: Config<'static> = Config {
    wifi_ssid: "network",
    wifi_password: "wifi-secret",
    wifi_auth: crate::config::WifiAuth::Wpa2,
    mqtt_host: "broker",
    mqtt_port: 1883,
    mqtt_username: "dashboard",
    mqtt_password: "mqtt-secret",
    device_id: "test",
    read_only: true,
    force_battery: false,
};

/// The facts about the firmware in the tests.
fn firmware() -> Firmware {
    Firmware {
        version: "0.1.0+test".into(),
        mac: [0x02, 0, 0, 0, 0, 0x01],
    }
}

/// The connection id of the first stream of the core after the start.
const FIRST: u32 = 2;
/// The connection id of the second stream. The core takes a new id when it
/// opens a stream, and again when the session ends.
const SECOND: u32 = FIRST + 2;

/// The shell, the fake broker at the other end of its streams, and the fake
/// radio of the radio task.
struct Device {
    shell: Shell,
    broker: Broker,
    radio: FakeRadio,
    /// The radio jobs that wait for the radio task, from the oldest.
    radio_jobs: Vec<(u32, RadioOp)>,
    /// The token of the last timer of a feature that the core started.
    last_timer: u32,
    /// The time on the clock of the device, with which the device answers
    /// each `Log` job.
    uptime: Uptime,
}

impl Deref for Device {
    type Target = Shell;

    fn deref(&self) -> &Shell {
        &self.shell
    }
}

impl DerefMut for Device {
    fn deref_mut(&mut self) -> &mut Shell {
        &mut self.shell
    }
}

impl Device {
    /// Sends an event to the core, as [`Shell::send`] does, and keeps the
    /// radio jobs for [`Device::link`] and the token of the last timer of a
    /// feature.
    fn send(&mut self, event: Event) -> Vec<Job> {
        let mut jobs = self.shell.send(event);
        self.answer_logs(&mut jobs);
        self.keep_radio_jobs(&jobs);
        self.keep_last_timer(&jobs);
        keep_stream_timers(&jobs);
        jobs
    }

    /// Gives a signal to the shell, as [`Shell::handle`] does, and keeps the
    /// radio jobs for [`Device::link`] and the token of the last timer of a
    /// feature.
    fn handle(&mut self, signal: Signal) -> Vec<Job> {
        let mut jobs = self.shell.handle(signal);
        self.answer_logs(&mut jobs);
        self.keep_radio_jobs(&jobs);
        self.keep_last_timer(&jobs);
        keep_stream_timers(&jobs);
        jobs
    }

    /// Answers each `Log` and `Clock` job in `jobs` at once with
    /// [`Device::uptime`], as the core task does. Adds the jobs of the
    /// answers to `jobs`.
    fn answer_logs(&mut self, jobs: &mut Vec<Job>) {
        let mut index = 0;
        while let Some(job) = jobs.get(index) {
            let signal = match *job {
                Job::Log { token, .. } => Some(Signal::Logged {
                    token,
                    at: self.uptime,
                }),
                Job::Clock { token } => Some(Signal::Clock {
                    token,
                    now: self.uptime,
                }),
                _ => None,
            };
            if let Some(signal) = signal {
                jobs.extend(self.shell.handle(signal));
            }
            index += 1;
        }
    }

    /// Keeps the token of the last timer of a feature that `jobs` starts.
    fn keep_last_timer(&mut self, jobs: &[Job]) {
        if let Some((_, token, _)) = started_timers(jobs).last() {
            self.last_timer = *token;
        }
    }

    fn keep_radio_jobs(&mut self, jobs: &[Job]) {
        for job in jobs {
            if let Job::Radio { token, stop, op } = job {
                self.radio_jobs
                    .retain(|(waiting, _)| !stop.contains(waiting));
                self.radio_jobs.push((*token, op.clone()));
            }
        }
    }

    /// Does the radio jobs that wait, in order, as the radio task does with
    /// the fake radio, until no radio job waits. Returns the jobs of the
    /// last answer.
    fn link(&mut self) -> Vec<Job> {
        let mut last = Vec::new();
        while !self.radio_jobs.is_empty() {
            let (token, op) = self.radio_jobs.remove(0);
            let answer = self.answer(&op);
            last = self.handle(Signal::Radio { token, answer });
        }
        last
    }

    /// Returns the answer of the fake radio to `op`.
    fn answer(&mut self, op: &RadioOp) -> RadioAnswer {
        match op {
            RadioOp::Join(join) => RadioAnswer::Joined(self.radio.join(join)),
            RadioOp::Address(address) => RadioAnswer::Lease(self.radio.address(address)),
            RadioOp::Scan(scan) => RadioAnswer::Scanned(self.radio.scan(scan)),
            RadioOp::Leave(leave) => {
                self.radio.leave(leave);
                RadioAnswer::Done
            }
            RadioOp::StopScan(stop_scan) => {
                self.radio.stop_scan(stop_scan);
                RadioAnswer::Done
            }
            RadioOp::ReadSignal(read) => RadioAnswer::Signal(self.radio.read_signal(read)),
            RadioOp::Off(off) => {
                self.radio.off(off);
                RadioAnswer::Done
            }
        }
    }

    /// Opens the stream of `connection` in the broker, and gives `Opened` to
    /// the shell. Returns the jobs.
    fn opened(&mut self, connection: u32) -> Vec<Job> {
        self.broker.open().expect("the broker is up");
        self.handle(Signal::Stream {
            connection,
            item: StreamItem::Opened,
        })
    }

    /// Does the `Write` jobs in `jobs` as the stream task does. The broker
    /// gets the bytes, the shell gets `Sent`, and then the shell gets the
    /// bytes from the broker. Returns the new jobs.
    fn deliver(&mut self, jobs: &[Job]) -> Vec<Job> {
        let mut next = Vec::new();
        for job in jobs {
            if let Job::Stream {
                connection,
                op: StreamOp::Write(write),
            } = job
            {
                let answer = self.broker.receive(write.bytes.clone());
                next.extend(self.handle(Signal::Written {
                    connection: *connection,
                    outcome: Written::Sent,
                }));
                next.extend(self.received(*connection, answer));
            }
        }
        next
    }

    /// Delivers the writes in `jobs` as [`Device::deliver`] does, and then
    /// each write that follows, until no write waits. Returns the other
    /// jobs, in order.
    ///
    /// After the CONNACK write, the device publishes (the discovery and the
    /// state messages) go in the next write. A dry run publishes neither.
    fn deliver_all(&mut self, jobs: &[Job]) -> Vec<Job> {
        let mut rest = Vec::new();
        let mut next = self.deliver(jobs);
        while next.iter().any(is_write) {
            rest.extend(next.iter().filter(|job| !is_write(job)).cloned());
            next = self.deliver(&next);
        }
        rest.extend(next);
        rest
    }

    /// Gives `bytes` from the broker to the shell, if there are any.
    /// Returns the jobs.
    fn received(&mut self, connection: u32, bytes: Vec<u8>) -> Vec<Job> {
        if bytes.is_empty() {
            return Vec::new();
        }
        self.handle(Signal::Stream {
            connection,
            item: StreamItem::Received(bytes),
        })
    }

    /// Opens the stream of `connection`, and does the writes until the
    /// broker accepted the connection. Returns the jobs after the CONNACK.
    fn connect(&mut self, connection: u32) -> Vec<Job> {
        let jobs = self.opened(connection);
        assert_eq!(names(&jobs), [format!("connect {connection}")]);
        self.deliver(&jobs)
    }

    /// Ends the stream of `connection` with `reason`. Returns the jobs.
    fn closed(&mut self, connection: u32, reason: &str) -> Vec<Job> {
        self.handle(Signal::Stream {
            connection,
            item: StreamItem::closed(reason),
        })
    }
}

/// Returns `true` if `job` is a `Write` job of a stream.
const fn is_write(job: &Job) -> bool {
    matches!(
        job,
        Job::Stream {
            op: StreamOp::Write(_),
            ..
        }
    )
}

/// Returns the packets from the client in `bytes`.
fn packets(bytes: &[u8]) -> Vec<Packet> {
    let mut framer = Framer::new(MAX_PACKET);
    framer.push(bytes.to_vec());
    let mut packets = Vec::new();
    while let Some(frame) = framer.next_packet().expect("valid framing") {
        let Frame::Packet { first, body } = frame else {
            panic!("an oversize packet");
        };
        packets.push(server::decode(first, body).expect("a valid packet"));
    }
    packets
}

std::thread_local! {
    /// The ids of the stream timers that the core started. Refer to
    /// [`is_stream_timer`]. Each timer id is new in the process, so the ids
    /// of two devices do not mix.
    static STREAM_TIMERS: RefCell<Vec<TimerId>> = const { RefCell::new(Vec::new()) };
}

/// Keeps the id of each stream timer that `jobs` starts, so that [`names`]
/// can leave out the jobs that cancel it.
fn keep_stream_timers(jobs: &[Job]) {
    for job in jobs {
        if let Job::Timer {
            op: TimerOp::NotifyAfter(notify),
            ..
        } = job
            && is_stream_timer(notify.duration.into())
        {
            STREAM_TIMERS.with_borrow_mut(|timers| timers.push(notify.id));
        }
    }
}

/// Returns `true` if the timer `id` is a stream timer.
fn is_stream_timer_id(id: TimerId) -> bool {
    STREAM_TIMERS.with_borrow(|timers| timers.contains(&id))
}

/// Returns `true` if `duration` is the duration of a timer of the MQTT
/// session or of the wait for the answer to a `Close` request. No timer of
/// a feature has one of these durations. The tests leave these timers out.
fn is_stream_timer(duration: Duration) -> bool {
    session_timers().contains(&duration) || duration == CLOSE_TIMEOUT
}

/// Returns the first stream job in `jobs`.
fn stream_job(jobs: &[Job]) -> Option<(u32, &StreamOp)> {
    jobs.iter().find_map(|job| match job {
        Job::Stream { connection, op } => Some((*connection, op)),
        _ => None,
    })
}

/// A short name for each job, to compare lists of jobs. A `Write` job has
/// one name for each packet in it, but one name for the SUBSCRIBE packets
/// that follow each other. The names leave out the stream timers (the
/// timers of the MQTT session and of `Close`), the jobs that cancel them,
/// and the lines of the log. Refer to [`logs`].
fn names(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .flat_map(|job| match job {
            Job::Render(_) => vec!["render".into()],
            Job::Stream { connection, op } => match op {
                StreamOp::Open(_) => vec![format!("open {connection}")],
                StreamOp::Write(write) => {
                    let mut names: Vec<String> = packets(&write.bytes)
                        .iter()
                        .map(|packet| match packet {
                            Packet::Connect { .. } => format!("connect {connection}"),
                            Packet::Publish { topic, .. } => {
                                format!("publish {topic} {connection}")
                            }
                            Packet::PubAck(_) => format!("puback {connection}"),
                            Packet::Subscribe { .. } => format!("subscribe {connection}"),
                            Packet::PingReq => format!("ping {connection}"),
                            Packet::Disconnect => format!("disconnect {connection}"),
                        })
                        .collect();
                    // The topics of one subscribe can need more than one
                    // packet. They have one name.
                    names.dedup_by(|a, b| a == b && a.starts_with("subscribe"));
                    names
                }
                StreamOp::Close(_) => vec![format!("close {connection}")],
            },
            Job::Radio { token, op, .. } => vec![match op {
                RadioOp::Join(_) => format!("join {token}"),
                RadioOp::Address(_) => format!("address {token}"),
                RadioOp::Leave(_) => format!("leave {token}"),
                RadioOp::Scan(_) => format!("scan {token}"),
                RadioOp::StopScan(_) => format!("stop scan {token}"),
                RadioOp::ReadSignal(_) => format!("read signal {token}"),
                RadioOp::Off(_) => "wifi off".into(),
            }],
            Job::Restart(_) => vec!["restart".into()],
            Job::Log { .. } | Job::Clock { .. } => Vec::new(),
            Job::Sleep(_) => vec!["sleep".into()],
            Job::FrontLight { token, level } => vec![format!("light {level} {token}")],
            Job::Timer {
                op: TimerOp::NotifyAfter(notify),
                ..
            } if is_stream_timer(notify.duration.into()) => Vec::new(),
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(_),
            } => vec![format!("start timer {token}")],
            Job::Timer {
                op: TimerOp::ClearTimer(clear),
                ..
            } if is_stream_timer_id(clear.id) => Vec::new(),
            Job::Timer {
                token,
                op: TimerOp::ClearTimer(_),
            } => vec![format!("cancel timer {token}")],
        })
        .collect()
}

/// Returns each line of the log in `jobs` as `{level} {area}: {message}`,
/// in order.
fn logs(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Log { line, .. } => Some(format!("{} {line}", line.level)),
            _ => None,
        })
        .collect()
}

fn started(power: PowerSource, read_only: bool) -> (Device, Vec<Job>) {
    let shell = Shell::new();
    // A new device starts with no stream timers.
    STREAM_TIMERS.with_borrow_mut(Vec::clear);
    let config = Config {
        read_only,
        ..CONFIG
    };
    let start = start(
        &config,
        firmware(),
        Boot {
            reset: ResetReason::PowerOn,
            restarts: 0,
        },
        Vec::new(),
        power,
        Some("/room/living_room".into()),
        None,
    );
    let mut device = Device {
        shell,
        broker: Broker::default(),
        radio: FakeRadio::default(),
        radio_jobs: Vec::new(),
        last_timer: 0,
        uptime: Uptime(0),
    };
    let jobs = device.send(Event::Start(start));
    (device, jobs)
}

/// Starts on the stand and connects. Returns the device, with the first
/// connection online and subscribed.
fn online(read_only: bool) -> Device {
    let (mut device, _) = started(PowerSource::Stand, read_only);
    device.link();
    let jobs = device.connect(FIRST);
    device.deliver_all(&jobs);
    device
}

/// Returns the id, the token and the duration of each timer that `jobs`
/// start, in order. The list leaves out the timers that [`is_stream_timer`]
/// finds.
fn started_timers(jobs: &[Job]) -> Vec<(crux_time::TimerId, u32, Duration)> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(notify),
            } => Some((notify.id, *token, Duration::from(notify.duration))),
            _ => None,
        })
        .filter(|(_, _, duration)| !is_stream_timer(*duration))
        .collect()
}

/// The signal of the user intent that ends the idle time now. The tests of
/// the sleep use it, so they do not need the idle timer.
const fn sleep_now() -> Signal {
    Signal::Event(Event::SleepNow)
}

#[test]
fn start_draws_and_joins() {
    let (_, jobs) = started(PowerSource::Stand, true);
    // The core turns the light off, because its state after a reset is not
    // known. Timer 1 is the full-refresh timer of the core.
    assert_eq!(
        names(&jobs),
        [
            "light 0 1",
            "render",
            "start timer 1",
            "join 1",
            "start timer 2"
        ]
    );
}

#[test]
fn wifi_then_the_stream_opens_and_the_core_connects_publishes_the_status_and_subscribes() {
    let (mut device, _) = started(PowerSource::Stand, true);
    let jobs = device.link();
    // The lease clears the time limit of `Address`, and the core opens the
    // stream.
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), format!("open {FIRST}")]
    );
    let Some((_, StreamOp::Open(open))) = stream_job(&jobs) else {
        panic!("not an open");
    };
    assert_eq!((open.host.as_str(), open.port), ("broker", 1883));
    assert_eq!(open.connection, FIRST);

    // The core writes the CONNECT packet when the stream is open.
    let jobs = device.opened(FIRST);
    let Some((_, StreamOp::Write(write))) = stream_job(&jobs) else {
        panic!("not a write");
    };
    let [Packet::Connect { client_id, .. }] = &packets(&write.bytes)[..] else {
        panic!("not a connect");
    };
    assert_eq!(client_id, "dashboard-test");

    // After the CONNACK, the status and the subscribe go in one write.
    let jobs = device.deliver(&jobs);
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {FIRST}"),
            format!("subscribe {FIRST}"),
            "render".into(),
        ]
    );
    let writes = jobs
        .iter()
        .filter(|job| matches!(job, Job::Stream { .. }))
        .count();
    assert_eq!(writes, 1);
    assert_eq!(device.view().connection, Connection::Online);
}

/// Ends the stream of `connection` with a lost link, fires the reconnect
/// timer, and does the radio jobs of the link. Returns the jobs after the
/// broker accepts the new connection `next`.
fn reconnect(device: &mut Device, connection: u32, next: u32) -> Vec<Job> {
    let jobs = device.closed(connection, "connection lost");
    // The stream ended, so the shell answers the `Close` of the core at once.
    assert!(
        !names(&jobs).contains(&format!("close {connection}")),
        "{:?}",
        names(&jobs)
    );
    // The only timer of a feature that the end of the stream starts is the
    // reconnect timer.
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    device.broker.drop_connection();
    device.handle(Signal::Timer { id, token });
    let jobs = device.link();
    // The lease clears the time limit of `Address`.
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), format!("open {next}")]
    );
    device.connect(next)
}

#[test]
fn each_connection_subscribes_again() {
    let mut device = online(true);
    let next = SECOND;
    let jobs = reconnect(&mut device, FIRST, next);
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {next}"),
            format!("subscribe {next}"),
            "render".into(),
        ]
    );
}

/// Returns `true` if `jobs` have a `Scan` job. The core scans for a better
/// access point when the link is quiet and a scan is due.
fn scans(jobs: &[Job]) -> bool {
    names(jobs).iter().any(|name| name.starts_with("scan"))
}

#[test]
fn the_subscribe_answer_makes_the_link_quiet() {
    // The first join goes to the strongest access point, so no scan is due
    // when the link is quiet.
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    let jobs = device.connect(FIRST);
    assert_eq!(names(&device.deliver_all(&jobs)), Vec::<String>::new());

    // The reconnect joins the cached access point. The SUBACK makes the
    // link quiet, and the core scans for a better access point.
    let jobs = reconnect(&mut device, FIRST, SECOND);
    let jobs = device.deliver(&jobs);
    assert!(scans(&jobs), "{:?}", names(&jobs));
}

#[test]
fn the_end_of_the_stream_fails_the_write_that_waits() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    // The write with the status and the subscribe gets no answer.
    device.connect(FIRST);

    // The stream ends first, so the shell answers the write with `Failed`,
    // and the link is not quiet. The new connection subscribes again.
    let next = SECOND;
    let jobs = reconnect(&mut device, FIRST, next);
    assert!(!scans(&jobs), "{:?}", names(&jobs));
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {next}"),
            format!("subscribe {next}"),
            "render".into(),
        ]
    );
    // A late answer for the old stream has no request.
    let late = Signal::Written {
        connection: FIRST,
        outcome: Written::Sent,
    };
    assert_eq!(device.handle(late), Vec::new());
}

#[test]
fn a_failed_write_waits_for_the_closed_item_to_end_the_session() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    device.connect(FIRST);
    // The core ignores `Failed`. The `Closed` item ends the session.
    let jobs = device.handle(Signal::Written {
        connection: FIRST,
        outcome: Written::Failed("write failed".into()),
    });
    assert_eq!(stream_job(&jobs), None, "{:?}", names(&jobs));
    assert!(started_timers(&jobs).is_empty(), "{:?}", names(&jobs));
    device.broker.drop_connection();
    // The stream ended, so the shell answers the `Close` request of the
    // core at once. The core waits for the backoff.
    let jobs = device.closed(FIRST, "connection lost");
    let backoff = started_timers(&jobs)
        .iter()
        .any(|(_, _, duration)| *duration == RECONNECT_MIN);
    assert!(backoff, "{:?}", names(&jobs));
}

#[test]
fn a_new_stream_opens_only_after_the_end_of_the_old_stream() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    // The broker does not answer the CONNECT packet, so the session timers
    // expire. The core closes the stream. The backoff ends before the old
    // stream ends, so the core does not get the link or open a new stream
    // yet.
    let opened = device.opened(FIRST);
    let jobs: Vec<Job> = opened
        .iter()
        .filter_map(|job| match job {
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(notify),
            } if Duration::from(notify.duration) == CONNACK_TIMEOUT => Some((notify.id, *token)),
            _ => None,
        })
        .flat_map(|(id, token)| device.handle(Signal::Timer { id, token }))
        .collect();
    let Some((id, token, _)) = started_timers(&jobs)
        .into_iter()
        .find(|(_, _, duration)| *duration == RECONNECT_MIN)
    else {
        panic!("no backoff timer: {:?}", names(&jobs));
    };
    assert_eq!(device.handle(Signal::Timer { id, token }), Vec::new());

    // The old stream ends, and the shell answers its `Close`. Then the core
    // gets the link and opens the new stream.
    device.broker.drop_connection();
    let next = SECOND;
    device.closed(FIRST, "closed");
    let jobs = device.link();
    // The lease clears the time limit of `Address`.
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), format!("open {next}")]
    );
    let jobs = device.connect(next);
    assert!(names(&jobs).contains(&format!("subscribe {next}")));
}

#[test]
fn a_session_timer_ends_the_session_and_closes_the_stream() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    // The broker gets no bytes and does not answer, so the session timers
    // of the write and of the CONNACK expire. Each has the same duration.
    let jobs = device.opened(FIRST);
    let timers: Vec<_> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(notify),
            } if Duration::from(notify.duration) == CONNACK_TIMEOUT => Some((notify.id, *token)),
            _ => None,
        })
        .collect();
    assert!(!timers.is_empty(), "{jobs:?}");
    let jobs: Vec<Job> = timers
        .into_iter()
        .flat_map(|(id, token)| device.handle(Signal::Timer { id, token }))
        .collect();
    let Some((connection, StreamOp::Close(close))) = stream_job(&jobs) else {
        panic!("not a close: {:?}", names(&jobs));
    };
    assert_eq!(connection, FIRST);
    assert!(!close.clean);
}

#[test]
fn a_refused_subscribe_still_makes_the_link_quiet() {
    // The reconnect joins the cached access point, so a scan is due.
    let mut device = online(true);
    let jobs = reconnect(&mut device, FIRST, SECOND);
    let Some((_, StreamOp::Write(write))) = stream_job(&jobs) else {
        panic!("not a write");
    };
    let filter = packets(&write.bytes)
        .into_iter()
        .find_map(|packet| match packet {
            Packet::Subscribe { mut filters, .. } => Some(filters.remove(0).0),
            _ => None,
        })
        .expect("a subscribe");
    device.broker.refuse = Some(filter);
    let jobs = device.deliver(&jobs);
    assert!(scans(&jobs), "{:?}", names(&jobs));
}

#[test]
fn a_second_radio_answer_is_ignored() {
    let (mut device, _) = started(PowerSource::Stand, true);
    let joined = RadioAnswer::Joined(Joined::Failed("authentication failed".into()));
    let jobs = device.handle(Signal::Radio {
        token: 1,
        answer: joined.clone(),
    });
    // The join failed, so the core waits for the backoff.
    assert_eq!(started_timers(&jobs).len(), 1, "{:?}", names(&jobs));
    let again = Signal::Radio {
        token: 1,
        answer: joined,
    };
    assert_eq!(device.handle(again), Vec::new());
}

#[test]
fn a_message_on_the_stream_redraws_a_visible_tile() {
    let mut device = online(true);
    let tile = device.view().tiles()[0].entity_id.clone();
    let (domain, object) = tile.split_once('.').unwrap();
    let mut message = |key: &str, payload: &[u8]| {
        let topic = format!("homeassistant/statestream/{domain}/{object}/{key}");
        device.broker.publish(&topic, payload, false);
        let bytes = device.broker.take();
        device.received(FIRST, bytes)
    };
    // The state starts the draw timer of the core, and does not draw yet.
    let jobs = message("state", b"on");
    let [(id, token, duration)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    assert_eq!(names(&jobs), [format!("start timer {token}")]);
    assert_eq!(duration, DRAW_DELAY);

    // The time of the update from Home Assistant waits for the same timer.
    // 12:00 UTC is 12:00 in the UK in November.
    let jobs = message("last_updated", b"2026-11-06T12:00:07.250000+00:00");
    assert_eq!(names(&jobs), Vec::<String>::new());

    // The timer draws both once.
    let jobs = device.handle(Signal::Timer { id, token });
    assert_eq!(names(&jobs), ["render"]);
    assert_eq!(device.view().updated, "Last changed 12:00");
}

#[test]
fn the_read_only_setting_of_the_config_makes_a_dry_run_in_the_core() {
    for (read_only, expected) in [
        (true, vec![]),
        (false, vec![format!("publish dashboard/test/cmd {FIRST}")]),
    ] {
        let mut device = online(read_only);
        let jobs = device.handle(Signal::Tap(ui::layout::tile(0).unwrap().center()));
        let publishes: Vec<String> = names(&jobs)
            .into_iter()
            .filter(|name| name.starts_with("publish"))
            .collect();
        assert_eq!(publishes, expected, "read_only = {read_only}");
    }
}

#[test]
fn a_tap_on_dead_space_while_awake_only_turns_the_light_on() {
    // The shell sends the tap. The core turns the light on and starts the
    // light timer, but it does not draw, also on battery. Timer 2 is the
    // time limit of the join. On battery, the idle timer has token 3, so
    // the light timer has 4.
    for (power, level, timer) in [
        (PowerSource::Stand, DEFAULT_STAND_LEVEL.get(), 3),
        (PowerSource::Battery, DEFAULT_BATTERY_LEVEL.get(), 4),
    ] {
        let (mut device, _) = started(power, true);
        let jobs = device.handle(Signal::Tap(Point::new(100, 20)));
        assert_eq!(
            names(&jobs),
            [format!("light {level} 2"), format!("start timer {timer}")],
            "{power:?}"
        );
        let timers = started_timers(&jobs);
        assert_eq!(timers.len(), 1, "{power:?}");
        assert_eq!(timers[0].2, LIGHT_TIMEOUT, "{power:?}");
    }
}

#[test]
fn a_light_answer_is_also_for_the_older_requests() {
    let (mut device, _) = started(PowerSource::Battery, true);
    device.handle(Signal::Tap(Point::new(100, 20)));
    let jobs = device.handle(sleep_now());
    // The core clears the idle timer (3) and the light timer of the tap (4).
    assert_eq!(
        names(&jobs),
        ["cancel timer 3", "cancel timer 4", "light 0 3"]
    );

    // No answer came for jobs 1 to 3. An answer with a newer token answers
    // all of them, so the sleep continues. `Off` stops the join, so the core
    // clears the time limit of the join (2). Timer 5 is the time limit of
    // `Off`.
    let jobs = device.handle(Signal::Light { token: 5 });
    assert_eq!(
        names(&jobs),
        ["cancel timer 2", "wifi off", "start timer 5"]
    );
    // Each request ended, so a second answer does nothing.
    assert_eq!(device.handle(Signal::Light { token: 5 }), Vec::new());
}

#[test]
fn on_battery_the_sleep_waits_for_the_light_before_it_disconnects() {
    let (mut device, _) = started(PowerSource::Battery, true);
    device.handle(Signal::Light { token: 1 });
    device.link();
    let jobs = device.connect(FIRST);
    device.deliver_all(&jobs);
    let jobs = device.handle(Signal::Tap(Point::new(100, 20)));
    assert!(names(&jobs).contains(&"light 25 2".into()));
    device.handle(Signal::Light { token: 2 });

    // The first step of the sleep turns the light off, and the core waits.
    // The core clears the idle timer (3) and the light timer of the tap.
    let light = device.last_timer;
    let jobs = device.handle(sleep_now());
    assert_eq!(
        names(&jobs),
        [
            "cancel timer 3".into(),
            format!("cancel timer {light}"),
            "light 0 3".into()
        ]
    );
    assert!(device.view().asleep);

    // An answer for an older job does not answer job 3.
    assert_eq!(device.handle(Signal::Light { token: 2 }), Vec::new());

    // The light is off. The next step disconnects.
    let jobs = device.handle(Signal::Light { token: 3 });
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {FIRST}"),
            format!("disconnect {FIRST}"),
        ]
    );
}

#[test]
fn a_tap_while_the_light_goes_off_for_the_sleep_wakes_and_the_late_answer_is_ignored() {
    let (mut device, _) = started(PowerSource::Battery, true);
    device.handle(Signal::Tap(Point::new(100, 20)));
    let jobs = device.handle(sleep_now());
    // The core clears the idle timer (3) and the light timer of the tap (4).
    assert_eq!(
        names(&jobs),
        ["cancel timer 3", "cancel timer 4", "light 0 3"]
    );

    // The tap wakes the device and turns the light on again.
    let jobs = device.handle(Signal::Tap(Point::new(100, 20)));
    assert!(
        names(&jobs).contains(&"light 25 4".into()),
        "{:?}",
        names(&jobs)
    );
    assert!(!device.view().asleep);

    // The answer of the sleep step comes late. The core does not disconnect.
    let jobs = device.handle(Signal::Light { token: 3 });
    assert_eq!(jobs, Vec::new());
    assert!(!device.view().asleep);
}

#[test]
fn a_tap_on_the_next_button_turns_the_page() {
    let mut device = online(true);
    let before = device.view().page_index;
    let jobs = device.handle(Signal::Tap(ui::layout::next().center()));
    assert!(names(&jobs).contains(&"render".into()));
    assert_eq!(device.view().page_index, before + 1);
}

#[test]
fn the_items_of_an_ended_stream_are_ignored() {
    let mut device = online(true);
    device.closed(FIRST, "connection lost");
    let late = Signal::Stream {
        connection: FIRST,
        item: StreamItem::Received(vec![0xD0, 0x00]),
    };
    assert_eq!(device.handle(late), Vec::new());
    assert_eq!(device.closed(FIRST, "again"), Vec::new());
}

#[test]
fn the_write_job_has_the_bytes_and_the_connection_of_the_request() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.link();
    let jobs = device.opened(FIRST);
    let Some((connection, StreamOp::Write(write))) = stream_job(&jobs) else {
        panic!("not a write");
    };
    assert_eq!((connection, write.connection), (FIRST, FIRST));
    assert!(
        matches!(packets(&write.bytes)[..], [Packet::Connect { .. }]),
        "{write:?}"
    );
}

#[test]
fn on_battery_the_idle_timer_sleeps_and_a_tap_wakes() {
    let mut device = online(true);
    // Off the stand, the core starts the idle timer, and no other timer.
    let jobs = device.handle(Signal::Event(Event::PowerSourceChanged(
        PowerSource::Battery,
    )));
    let timers = started_timers(&jobs);
    assert_eq!(timers.len(), 1, "{:?}", names(&jobs));
    let (id, token, duration) = timers[0];
    assert_eq!(duration, IDLE_TIMEOUT);
    // Without a dry run, the core publishes the new power source in the
    // state message. Each write goes now.
    device.deliver_all(&jobs);

    let jobs = device.handle(Signal::Timer { id, token });
    // The first step of the sleep. The core waits for the close.
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {FIRST}"),
            format!("disconnect {FIRST}"),
        ]
    );
    assert!(device.view().asleep);
    let jobs = device.deliver(&jobs);
    assert_eq!(names(&jobs), [format!("close {FIRST}")]);

    // Any tap during the steps wakes the device. The old stream has not
    // ended yet, so the core does not connect yet.
    let jobs = device.handle(Signal::Tap(Point::new(1, 1)));
    let names = names(&jobs);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("join") || name.starts_with("open")),
        "{names:?}"
    );
    assert!(!device.view().asleep);

    // The old stream ends. The link is still up, so the new connection
    // opens at once.
    device.broker.drop_connection();
    let jobs = device.closed(FIRST, "closed");
    let next = SECOND;
    assert_eq!(self::names(&jobs), [format!("open {next}")]);
    let jobs = device.connect(next);
    assert!(self::names(&jobs).contains(&format!("subscribe {next}")));
}

#[test]
fn a_close_ends_at_the_end_of_its_stream() {
    let (mut device, jobs) = idle_timeout();
    let jobs = device.deliver(&jobs);
    assert_eq!(names(&jobs), [format!("close {FIRST}")]);
    // The answer to the close starts the next step of the sleep. A signal
    // that is not `Closed` does not answer it.
    let late = Signal::Written {
        connection: FIRST,
        outcome: Written::Sent,
    };
    assert_eq!(device.handle(late), Vec::new());
    let jobs = device.closed(FIRST, "closed");
    assert_eq!(names(&jobs), ["wifi off", "start timer 15"]);
}

#[test]
fn a_timer_signal_with_an_old_token_is_ignored() {
    let (mut device, jobs) = started(PowerSource::Battery, true);
    let (id, token, _) = started_timers(&jobs)[0];
    let stale = Signal::Timer {
        id,
        token: token.wrapping_sub(1),
    };
    assert_eq!(device.handle(stale), Vec::new());
    // The idle timer, the full-refresh timer and the time limit of the
    // join.
    assert_eq!(device.timer_count(), 3);
}

#[test]
fn a_timer_that_the_core_clears_cancels_its_task_and_the_count_does_not_grow() {
    // On battery, a page turn starts the idle timer again. The core clears
    // the idle timer and starts a new one.
    let (mut device, _) = started(PowerSource::Battery, true);
    device.link();
    let jobs = device.connect(FIRST);
    device.deliver_all(&jobs);
    let (_, first, _) = started_timers(&device.send(Event::NextPage))[0];
    let count = device.timer_count();

    let jobs = device.send(Event::NextPage);
    let names = names(&jobs);
    let cancel = names
        .iter()
        .position(|name| *name == format!("cancel timer {first}"));
    let timers = started_timers(&jobs);
    assert_eq!(timers.len(), 1, "{names:?}");
    let (_, second, _) = timers[0];
    let start = names
        .iter()
        .position(|name| *name == format!("start timer {second}"));
    // The old task stops before the new one starts.
    assert!(cancel.is_some() && cancel < start, "{names:?}");
    assert_ne!(first, second);
    assert_eq!(device.timer_count(), count);
}

#[test]
fn start_has_the_settings_of_the_config() {
    let boot = Boot {
        reset: ResetReason::Watchdog,
        restarts: 2,
    };
    let lines = vec![BootLine {
        level: Kept::Warn,
        source: "wifi".into(),
        message: "restart the chip: no address after 3 joins".into(),
    }];
    let start = start(
        &CONFIG,
        firmware(),
        boot,
        lines.clone(),
        PowerSource::Stand,
        None,
        Some(5),
    );
    assert_eq!(start.boot, boot);
    assert_eq!(start.before_restart, lines);
    assert_eq!(start.settings.host, "broker");
    assert_eq!(start.settings.port, 1883);
    assert_eq!(start.settings.device_id, "test");
    let credentials = start.settings.credentials.unwrap();
    assert_eq!(credentials.username, "dashboard");
    assert_eq!(start.last_sync, Some(5));
    assert!(start.settings.read_only);
    assert_eq!(start.firmware, firmware());

    let anonymous = Config {
        mqtt_username: "",
        ..CONFIG
    };
    let start = super::start(
        &anonymous,
        firmware(),
        boot,
        Vec::new(),
        PowerSource::Stand,
        None,
        None,
    );
    assert_eq!(start.settings.credentials, None);
}

#[test]
fn a_late_join_answer_after_off_is_dropped() {
    let (mut device, _) = started(PowerSource::Battery, true);
    // The idle time ends before the join ends: the core turns the radio
    // off. `Off` stops the join, so its job tells the radio task to stop
    // job 1. The shell answered the join with `Stopped`.
    let jobs = device.handle(sleep_now());
    let off = jobs.iter().find_map(|job| match job {
        Job::Radio {
            stop,
            op: RadioOp::Off(_),
            ..
        } => Some(stop.clone()),
        _ => None,
    });
    assert_eq!(off, Some(vec![1]), "{:?}", names(&jobs));

    // The radio task answers the join late. The shell drops the answer.
    let late = Signal::Radio {
        token: 1,
        answer: RadioAnswer::Joined(Joined::Associated {
            access_point: HALL,
            rssi: Some(-55),
            kept: false,
            had_address: false,
        }),
    };
    assert_eq!(device.handle(late), Vec::new());
    assert!(device.view().asleep);
}

#[test]
fn an_answer_with_an_unknown_token_or_of_another_kind_is_dropped() {
    let (mut device, _) = started(PowerSource::Stand, true);
    // No job has token 5.
    let unknown = Signal::Radio {
        token: 5,
        answer: RadioAnswer::Done,
    };
    assert_eq!(device.handle(unknown), Vec::new());
    // Job 1 is a join, so a lease is not its answer. The join still waits.
    let wrong = Signal::Radio {
        token: 1,
        answer: RadioAnswer::Lease(Lease::Leased {
            ip: [192, 168, 1, 50],
        }),
    };
    assert_eq!(device.handle(wrong), Vec::new());
    let jobs = device.link();
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), format!("open {FIRST}")]
    );
}

#[test]
fn a_batch_has_at_most_one_render_with_the_last_view() {
    let mut device = online(true);
    // A page turn and the sleep both render. Each batch has one render.
    let jobs = device.handle(Signal::Tap(ui::layout::next().center()));
    let renders: Vec<_> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Render(view) => Some(view),
            _ => None,
        })
        .collect();
    assert_eq!(renders.len(), 1);
    assert_eq!(**renders[0], device.view());
}

#[test]
fn the_render_before_sleep_stays_before_sleep() {
    let (mut device, _) = started(PowerSource::Battery, true);
    // No stream is open, so the radio turns off at once. The core clears
    // the idle timer (3) and the time limit of the join (2), which `Off`
    // stops. Timer 4 is the time limit of `Off`.
    let jobs = device.handle(sleep_now());
    assert_eq!(
        names(&jobs),
        [
            "cancel timer 3",
            "cancel timer 2",
            "wifi off",
            "start timer 4"
        ]
    );
    let jobs = device.link();
    let names = names(&jobs);
    // The core clears the full-refresh timer before it sleeps.
    assert!(names.contains(&"cancel timer 1".into()), "{names:?}");
    // No timer runs. The core cleared the idle timer at `SleepNow`, and
    // the time limits of `Join` and `Off` when their answers came.
    assert_eq!(device.timer_count(), 0);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert_eq!(names.iter().filter(|name| *name == "render").count(), 1);
    // The names leave out the `Log` jobs, so the test finds the render in
    // the jobs.
    let view = jobs
        .iter()
        .find_map(|job| match job {
            Job::Render(view) => Some(view),
            _ => None,
        })
        .unwrap();
    assert!(view.asleep);
}

/// Starts on battery, connects, and ends the idle time with `SleepNow`.
/// Returns the device and the jobs of the end of the idle time.
fn idle_timeout() -> (Device, Vec<Job>) {
    idle_timeout_with(true)
}

/// As [`idle_timeout`], with the dry run on or off.
fn idle_timeout_with(read_only: bool) -> (Device, Vec<Job>) {
    let (mut device, _) = started(PowerSource::Battery, read_only);
    device.link();
    let jobs = device.connect(FIRST);
    device.deliver_all(&jobs);
    let jobs = device.handle(sleep_now());
    (device, jobs)
}

/// Ends the idle time and answers each step, until the `Sleep` job.
fn asleep() -> Device {
    asleep_with(true)
}

/// As [`asleep`], with the dry run on or off.
fn asleep_with(read_only: bool) -> Device {
    let (mut device, jobs) = idle_timeout_with(read_only);
    device.deliver(&jobs);
    device.broker.drop_connection();
    device.closed(FIRST, "closed");
    device.link();
    assert!(device.is_sleeping());
    device
}

#[test]
fn each_step_of_the_sleep_waits_for_the_answer_of_its_task() {
    let (mut device, jobs) = idle_timeout();
    // Step 1: the core clears the idle timer (3). Then `asleep` and the
    // disconnect go in one write.
    assert_eq!(
        names(&jobs),
        [
            "cancel timer 3".into(),
            format!("publish dashboard/test/status {FIRST}"),
            format!("disconnect {FIRST}"),
        ]
    );
    assert!(device.view().asleep);

    // The broker got the bytes. Step 2: close the stream.
    let jobs = device.deliver(&jobs);
    assert_eq!(names(&jobs), [format!("close {FIRST}")]);
    let Some((_, StreamOp::Close(close))) = stream_job(&jobs) else {
        panic!("not a close");
    };
    assert!(close.clean);

    // The stream ends: the close is done. Step 3: the radio off, with its
    // time limit.
    device.broker.drop_connection();
    let jobs = device.closed(FIRST, "closed");
    assert_eq!(names(&jobs), ["wifi off", "start timer 15"]);
    // The broker keeps `asleep`, because the client sent a DISCONNECT.
    assert_eq!(
        device.broker.retained("dashboard/test/status"),
        Some("asleep")
    );

    // The radio is off. Step 4: the last image, then the sleep.
    let jobs = device.link();
    let names = names(&jobs);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert!(device.is_sleeping());

    // A touch with no point wakes the device. The core connects again.
    let jobs = device.handle(Signal::Woke(Wake::Touch(None)));
    assert!(!device.is_sleeping());
    assert!(!device.view().asleep);
    assert!(
        self::names(&jobs)
            .iter()
            .any(|name| name.starts_with("join")),
        "{:?}",
        self::names(&jobs)
    );
}

#[test]
fn a_sleep_with_no_stream_turns_the_radio_off_at_once() {
    // The join never ends, so no stream opens.
    let (mut device, _) = started(PowerSource::Battery, true);
    let jobs = device.handle(sleep_now());
    // The core sends no close, so `Off` and its time limit follow in the
    // same batch. The core clears the idle timer (3) and the time limit of
    // the join (2), which `Off` stops.
    assert_eq!(
        names(&jobs),
        [
            "cancel timer 3",
            "cancel timer 2",
            "wifi off",
            "start timer 4"
        ]
    );
}

#[test]
fn a_tap_while_sleep_waits_is_the_wake_and_acts_on_its_control() {
    let mut device = asleep();
    // A tap on the next-page button wakes the device and turns the page.
    let path = device.view().path;
    let jobs = device.handle(Signal::Tap(ui::layout::next().center()));
    assert!(!device.is_sleeping());
    assert!(!device.view().asleep);
    assert_ne!(device.view().path, path);
    assert!(
        names(&jobs).iter().any(|name| name.starts_with("join")),
        "{:?}",
        names(&jobs)
    );
}

#[test]
fn a_tap_from_the_wake_answers_sleep_once() {
    let mut device = asleep();
    let _ = device.handle(Signal::Tap(ui::layout::next().center()));
    assert!(!device.is_sleeping());

    // The tap answered the `Sleep` request. A late wake finds no request,
    // so it gives no jobs.
    assert_eq!(device.handle(Signal::Woke(Wake::Touch(None))), Vec::new());
}

#[test]
fn a_key_down_while_sleep_waits_is_the_wake() {
    let mut device = asleep();
    let jobs = device.handle(Signal::Key(KeyEdge::Down));
    assert!(!device.is_sleeping());
    assert!(!device.view().asleep);
    assert!(
        names(&jobs).iter().any(|name| name.starts_with("join")),
        "{:?}",
        names(&jobs)
    );
    // The key answered the `Sleep` request. A late wake gives no jobs.
    assert_eq!(device.handle(Signal::Woke(Wake::Touch(None))), Vec::new());

    // A press that wakes the device is short, so the wake shows the index
    // page. The `Up` edge after the wake does nothing.
    assert_eq!(device.view().path, "/");
    let _ = device.handle(Signal::Key(KeyEdge::Up));
    assert_eq!(device.view().path, "/");
}

#[test]
fn a_key_up_while_sleep_waits_goes_to_the_core_and_does_not_wake() {
    let mut device = asleep();
    let _ = device.handle(Signal::Key(KeyEdge::Up));
    assert!(device.is_sleeping());
    assert!(device.view().asleep);
}

/// Returns the wake sources of the `Sleep` job in `jobs`.
fn sleep_job(jobs: &[Job]) -> Option<&shared::capabilities::power::Sleep> {
    jobs.iter().find_map(|job| match job {
        Job::Sleep(sleep) => Some(sleep),
        _ => None,
    })
}

#[test]
fn a_check_gives_only_a_new_sleep_job() {
    let mut device = asleep();
    let jobs = device.handle(Signal::Woke(Wake::Timer));
    // The `Sleep` job that went out last is the job of the steps of the
    // sleep. The check gives a job with the same wake sources, and nothing
    // more.
    assert_eq!(jobs.len(), 1, "{:?}", names(&jobs));
    let sleep = sleep_job(&jobs).expect("a Sleep job");
    assert_eq!(
        sleep.wake_on,
        [
            shared::capabilities::power::WakeSource::Touch,
            shared::capabilities::power::WakeSource::Timer { seconds: 60 },
        ]
    );
    assert!(device.is_sleeping());
}

#[test]
fn a_stray_wake_gives_a_hold_timer_and_a_line_and_then_a_sleep_job() {
    let mut device = asleep();
    let jobs = device.handle(Signal::Woke(Wake::Other));
    assert_eq!(names(&jobs), [format!("start timer {}", device.last_timer)]);
    assert_eq!(
        logs(&jobs),
        ["info power: sleep 1 woke with no cause, so it sleeps again after 3 s"]
    );
    assert!(sleep_job(&jobs).is_none());
    assert!(!device.is_sleeping());

    // The hold timer expires: one `Sleep` job.
    let (id, token, duration) = started_timers(&jobs)[0];
    assert_eq!(duration, Duration::from_secs(3));
    let jobs = device.handle(Signal::Timer { id, token });
    assert_eq!(names(&jobs), ["sleep"]);
    assert!(device.is_sleeping());
}

#[test]
fn a_tap_on_a_light_while_sleep_waits_is_sent_after_the_reconnect() {
    let mut device = asleep_with(false);
    let jobs = device.handle(Signal::Tap(ui::layout::tile(0).unwrap().center()));
    assert!(!device.is_sleeping());
    // No stream is open, so the toggle waits in the outbox.
    let next = SECOND;
    let command = format!("publish dashboard/test/cmd {next}");
    assert!(!names(&jobs).contains(&command), "{:?}", names(&jobs));

    device.link();
    let names = names(&device.connect(next));
    // The toggle goes first, before the status and the subscribe.
    let toggle = names.iter().position(|name| *name == command);
    let status = format!("publish dashboard/test/status {next}");
    let status = names.iter().position(|name| *name == status);
    assert!(toggle.is_some() && toggle < status, "{names:?}");
}

#[test]
fn a_change_of_the_power_source_while_sleep_waits_ends_it() {
    let mut device = asleep();
    // A change of the charge does not end the sleep.
    device.handle(Signal::Event(Event::BatteryChanged(Some(50))));
    assert!(device.is_sleeping());

    device.handle(Signal::Event(Event::PowerSourceChanged(PowerSource::Stand)));
    assert!(!device.is_sleeping());
    assert!(!device.view().asleep);
}

#[test]
fn a_wake_with_no_sleep_request_is_ignored() {
    let mut device = online(true);
    assert_eq!(device.handle(Signal::Woke(Wake::Timer)), Vec::new());
    let late = Signal::Radio {
        token: 99,
        answer: RadioAnswer::Done,
    };
    assert_eq!(device.handle(late), Vec::new());
}

/// Returns the token, the stop tokens and the operation of each radio job
/// in `jobs`.
fn radio_jobs(jobs: &[Job]) -> Vec<(u32, &[u32], &RadioOp)> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Radio { token, stop, op } => Some((*token, stop.as_slice(), op)),
            _ => None,
        })
        .collect()
}

#[test]
fn each_radio_job_has_a_new_token_and_its_answer_continues_the_link() {
    let (mut device, jobs) = started(PowerSource::Stand, true);
    let [(1, [], RadioOp::Join(join))] = radio_jobs(&jobs)[..] else {
        panic!("not one join: {:?}", names(&jobs));
    };
    let joined = device.radio.join(join);
    let jobs = device.handle(Signal::Radio {
        token: 1,
        answer: RadioAnswer::Joined(joined),
    });
    let [(2, [], RadioOp::Address(address))] = radio_jobs(&jobs)[..] else {
        panic!("not one address: {:?}", names(&jobs));
    };
    let lease = device.radio.address(address);
    let jobs = device.handle(Signal::Radio {
        token: 2,
        answer: RadioAnswer::Lease(lease),
    });
    // The lease clears the time limit of `Address`, which is timer 3.
    assert_eq!(
        names(&jobs),
        ["cancel timer 3".into(), format!("open {FIRST}")]
    );
}

#[test]
fn a_leave_stops_a_scan_and_the_late_answer_of_the_scan_is_dropped() {
    // The reconnect joins the cached access point, so the quiet link scans.
    let mut device = online(true);
    let jobs = reconnect(&mut device, FIRST, SECOND);
    let jobs = device.deliver(&jobs);
    let [(scan, [], RadioOp::Scan(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one scan: {:?}", names(&jobs));
    };
    device.radio_jobs.clear();

    // The connection fails during the scan. The reconnect leaves, so the
    // shell answers the scan with `Stopped`, and the leave job stops it.
    let jobs = device.closed(SECOND, "connection lost");
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    let jobs = device.handle(Signal::Timer { id, token });
    let [(_, stop, RadioOp::Leave(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one leave: {:?}", names(&jobs));
    };
    assert_eq!(stop, [scan]);

    // The radio task answers the scan late. The shell drops the answer.
    let late = Signal::Radio {
        token: scan,
        answer: RadioAnswer::Scanned(Scanned::Stopped),
    };
    assert_eq!(device.handle(late), Vec::new());
}

/// Returns the payload of each state message in the writes of `jobs`.
fn state_payloads(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Stream {
                op: StreamOp::Write(write),
                ..
            } => Some(packets(&write.bytes)),
            _ => None,
        })
        .flatten()
        .filter_map(|packet| match packet {
            Packet::Publish { topic, payload, .. } if topic.ends_with("/state") => {
                Some(String::from_utf8(payload).expect("JSON text"))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_change_that_is_not_urgent_waits_for_a_read_signal_job() {
    let mut device = online(false);
    // The first level of the battery is urgent, so it goes at once.
    let jobs = device.handle(Signal::Event(Event::BatteryChanged(Some(50))));
    assert!(radio_jobs(&jobs).is_empty(), "{:?}", names(&jobs));
    assert_eq!(state_payloads(&jobs).len(), 1);
    device.deliver_all(&jobs);

    // A move of 1% is not urgent. The state message waits for the signal.
    let jobs = device.handle(Signal::Event(Event::BatteryChanged(Some(51))));
    let [(token, [], RadioOp::ReadSignal(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one read signal: {:?}", names(&jobs));
    };
    assert!(state_payloads(&jobs).is_empty(), "{:?}", names(&jobs));
    device.radio_jobs.clear();

    // The answer clears the time limit, and the state message goes with
    // the new signal.
    let jobs = device.handle(Signal::Radio {
        token,
        answer: RadioAnswer::Signal(SignalRead::Read(Some(-61))),
    });
    let [state] = &state_payloads(&jobs)[..] else {
        panic!("not one state message: {:?}", names(&jobs));
    };
    assert!(state.contains(r#""battery":51"#), "{state}");
    assert!(state.contains(r#""rssi":-61"#), "{state}");
}

#[test]
fn a_leave_stops_a_read_signal_job() {
    let mut device = online(false);
    let jobs = device.handle(Signal::Event(Event::BatteryChanged(Some(50))));
    device.deliver_all(&jobs);
    let jobs = device.handle(Signal::Event(Event::BatteryChanged(Some(51))));
    let [(read, [], RadioOp::ReadSignal(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one read signal: {:?}", names(&jobs));
    };
    device.radio_jobs.clear();

    // The connection fails before the answer. The failure makes the link
    // suspect, so the reconnect leaves first. The shell answers the read
    // with `Stopped`, and the leave job stops it.
    let jobs = device.closed(FIRST, "connection lost");
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    let jobs = device.handle(Signal::Timer { id, token });
    let [(_, stop, RadioOp::Leave(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one leave: {:?}", names(&jobs));
    };
    assert_eq!(stop, [read]);

    // The radio task answers the read late. The shell drops the answer.
    let late = Signal::Radio {
        token: read,
        answer: RadioAnswer::Signal(SignalRead::Read(Some(-61))),
    };
    assert_eq!(device.handle(late), Vec::new());
}

#[test]
fn a_stuck_driver_gives_a_restart_job_after_3_joins_with_no_address() {
    let mut device = online(true);
    device.radio.stuck = true;
    device.radio.drop_link();
    let jobs = device.closed(FIRST, "connection lost");
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    device.handle(Signal::Timer { id, token });

    // The cached join and the join to the strongest access point get no
    // address. The core waits for the backoff.
    let jobs = device.link();
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    device.handle(Signal::Timer { id, token });

    // The third join gets no address. The answer clears the time limit of
    // `Address`. The core logs the count, and then asks for a restart.
    let jobs = device.link();
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), "restart".into()]
    );
    assert_eq!(
        logs(&jobs),
        ["warn wifi: 3 of 3 joins got no new address while the link is suspect"]
    );
    assert_eq!(
        jobs[2..],
        [Job::Restart(Restart {
            area: Area::Wifi,
            reason: "no address after 3 joins".into(),
        })]
    );
}

#[test]
fn a_change_of_access_point_leaves_only_after_the_end_of_the_stream() {
    // The reconnect joins the cached access point, so a scan is due.
    let mut device = online(true);
    let jobs = reconnect(&mut device, FIRST, SECOND);
    let jobs = device.deliver(&jobs);
    assert!(scans(&jobs), "{:?}", names(&jobs));
    // The line of the reconnect goes to Home Assistant in a later write.
    assert!(
        names(&jobs).contains(&format!("publish dashboard/test/log {SECOND}")),
        "{:?}",
        names(&jobs)
    );
    device.deliver(&jobs);

    // The scan finds a better access point. The answer clears the time
    // limit of `Scan`. The core logs the reason, and then it sends a
    // DISCONNECT.
    device.radio.move_strongest();
    let jobs = device.link();
    let scan = device.last_timer;
    assert_eq!(
        names(&jobs),
        [
            format!("cancel timer {scan}"),
            format!("disconnect {SECOND}")
        ]
    );
    assert_eq!(
        logs(&jobs),
        [
            "info wifi: change to 02:00:00:00:00:02 on channel 11 (-45 dBm), because the signal \
             of 02:00:00:00:00:01 on channel 6 is -75 dBm"
        ]
    );
    // The clear of the time limit comes first. The line comes before the
    // effects of the decision.
    assert!(matches!(jobs[1], Job::Log { .. }), "{jobs:?}");
    // The session ended, so the view shows `Connecting`.
    let jobs = device.deliver(&jobs);
    assert_eq!(names(&jobs), [format!("close {SECOND}"), "render".into()]);

    // A tap while the stream closes does not leave yet.
    let jobs = device.handle(Signal::Tap(Point::new(100, 20)));
    assert!(
        !names(&jobs).iter().any(|name| name.starts_with("leave")),
        "{:?}",
        names(&jobs)
    );

    // The stream ends. The core leaves, and joins the better access point.
    // The broker got the DISCONNECT, so it keeps `online`.
    device.broker.drop_connection();
    let jobs = device.closed(SECOND, "closed");
    assert!(
        names(&jobs).iter().any(|name| name.starts_with("leave")),
        "{:?}",
        names(&jobs)
    );
    assert_eq!(
        device.broker.retained("dashboard/test/status"),
        Some("online")
    );
}

#[test]
fn a_join_with_no_answer_is_stopped_by_the_next_join() {
    // The join to the strongest access point gets no answer, and its time
    // limit expires. The core waits for the backoff.
    let (mut device, jobs) = started(PowerSource::Stand, true);
    let limit = started_timers(&jobs)
        .into_iter()
        .find(|(_, _, duration)| *duration == JOIN_TIMEOUT)
        .expect("the time limit of the join");
    let jobs = device.handle(Signal::Timer {
        id: limit.0,
        token: limit.1,
    });
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };

    // The next join stops job 1, so the radio task does not wait for it.
    let jobs = device.handle(Signal::Timer { id, token });
    let [(2, stop, RadioOp::Join(_))] = radio_jobs(&jobs)[..] else {
        panic!("not one join: {:?}", names(&jobs));
    };
    assert_eq!(stop, [1]);

    // The late answer of job 1 is dropped, and job 2 continues the link.
    device.radio_jobs.retain(|(token, _)| *token == 2);
    let late = Signal::Radio {
        token: 1,
        answer: RadioAnswer::Joined(Joined::Failed("no access point found".into())),
    };
    assert_eq!(device.handle(late), Vec::new());
    let jobs = device.link();
    let address = device.last_timer;
    assert_eq!(
        names(&jobs),
        [format!("cancel timer {address}"), format!("open {FIRST}")]
    );
}

#[test]
fn a_line_of_the_log_keeps_its_place_before_a_restart() {
    let mut shell = Shell::new();
    let mut cmd: Cmd = logs::warn(Area::Wifi, "3 of 3 joins got no new address")
        .and(Radio::restart(Area::Wifi, "no address after 3 joins".into()).build());
    let effects: Vec<Effect> = cmd.effects().collect();

    let jobs = shell.process(effects);

    assert_eq!(
        jobs,
        [
            Job::Log {
                token: 1,
                line: Log {
                    level: Level::Warn,
                    area: Area::Wifi,
                    message: "3 of 3 joins got no new address".into(),
                },
            },
            Job::Restart(Restart {
                area: Area::Wifi,
                reason: "no address after 3 joins".into(),
            }),
        ]
    );
    assert_eq!(names(&jobs), ["restart"]);
    assert_eq!(logs(&jobs), ["warn wifi: 3 of 3 joins got no new address"]);
}

#[test]
fn a_line_of_the_log_ends_at_its_answer_with_the_time_of_the_device() {
    let mut shell = Shell::new();
    let mut cmd: Cmd = logs::info(Area::Power, "sleep 1 closes the connection");
    let effects: Vec<Effect> = cmd.effects().collect();
    let jobs = shell.process(effects);
    let [Job::Log { token, .. }] = jobs[..] else {
        panic!("not one Log job: {jobs:?}");
    };

    // An answer with a token that no request waits for is dropped.
    let stale = Signal::Logged {
        token: token + 1,
        at: Uptime(1),
    };
    assert_eq!(shell.handle(stale), Vec::new());
    assert_eq!(cmd.events().next(), None);

    // The answer gives the time of the line to the core.
    let answer = Signal::Logged {
        token,
        at: Uptime(42_000),
    };
    assert_eq!(shell.handle(answer), Vec::new());
    assert_eq!(
        cmd.events().collect::<Vec<_>>(),
        [Event::Logs(logs::Event::Stamped(Line {
            at: Uptime(42_000),
            level: Kept::Info,
            source: Source::Core(Area::Power),
            message: Message::new("sleep 1 closes the connection"),
        }))]
    );
    assert!(cmd.is_done());
}

#[test]
fn a_clock_request_ends_at_its_answer_with_the_time_of_the_device() {
    let mut shell = Shell::new();
    let mut cmd: Cmd = log::clock().then_send(|now| {
        Event::Logs(logs::Event::Clock {
            connection: FIRST,
            now,
        })
    });
    let effects: Vec<Effect> = cmd.effects().collect();
    let jobs = shell.process(effects);
    let [Job::Clock { token }] = jobs[..] else {
        panic!("not one Clock job: {jobs:?}");
    };

    // An answer with a token that no request waits for is dropped.
    let stale = Signal::Clock {
        token: token + 1,
        now: Uptime(1),
    };
    assert_eq!(shell.handle(stale), Vec::new());
    assert_eq!(cmd.events().next(), None);

    // The answer gives the time of the device to the core.
    let answer = Signal::Clock {
        token,
        now: Uptime(42_000),
    };
    assert_eq!(shell.handle(answer), Vec::new());
    assert_eq!(
        cmd.events().collect::<Vec<_>>(),
        [Event::Logs(logs::Event::Clock {
            connection: FIRST,
            now: Uptime(42_000),
        })]
    );
    assert!(cmd.is_done());
}

#[test]
fn only_the_answer_to_a_log_job_does_not_count_as_an_update() {
    let logged = Signal::Logged {
        token: 1,
        at: Uptime(5),
    };
    assert!(!logged.counts());
    assert!(Signal::Tap(Point::new(10, 10)).counts());
    assert!(Signal::Woke(Wake::Timer).counts());
    assert!(
        Signal::Stream {
            connection: FIRST,
            item: StreamItem::Opened,
        }
        .counts()
    );
}

#[test]
fn only_an_event_from_a_task_needs_an_ack() {
    assert!(Signal::Event(Event::PowerSourceChanged(PowerSource::Stand)).needs_ack());
    assert!(Signal::Event(Event::BatteryChanged(Some(50))).needs_ack());
    assert!(!Signal::Woke(Wake::Timer).needs_ack());
    assert!(!Signal::Tap(ui::layout::next().center()).needs_ack());
    assert!(!Signal::Light { token: 1 }.needs_ack());
}

#[test]
fn the_connack_gives_a_render_job_with_the_view_online() {
    let (mut device, _) = started(PowerSource::Stand, false);
    device.link();
    let jobs = device.connect(FIRST);
    let online = jobs
        .iter()
        .any(|job| matches!(job, Job::Render(view) if view.connection == Connection::Online));
    assert!(online, "{:?}", names(&jobs));
}

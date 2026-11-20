use alloc::{format, string::String, vec, vec::Vec};
use core::{
    ops::{Deref, DerefMut},
    time::Duration,
};

use embedded_graphics::prelude::Point;
use shared::{
    Connection, Event, PowerSource,
    capabilities::{
        network::WifiStatus,
        power::Wake,
        stream::{StreamItem, Written},
    },
    fake::{Broker, session_timers},
    features::{
        light::{BATTERY_LEVEL, LIGHT_TIMEOUT, STAND_LEVEL},
        power::IDLE_TIMEOUT,
        refresh::DRAW_DELAY,
    },
    mqtt::{
        CONNACK_TIMEOUT, MAX_PACKET,
        codec::server::{self, Packet},
        framer::{Frame, Framer},
    },
};

use super::{Job, Shell, Signal, StreamOp, TimerOp, start};
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

/// The connection id of the first stream of the core after the start.
const FIRST: u32 = 2;
/// The connection id of the second stream. The core takes a new id when it
/// opens a stream, and again when the session ends.
const SECOND: u32 = FIRST + 2;

/// The shell and the fake broker at the other end of its streams.
struct Device {
    shell: Shell,
    broker: Broker,
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
            item: StreamItem::Closed(reason.into()),
        })
    }
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

/// Returns `true` if `duration` is the duration of a timer of the MQTT
/// session. No timer of a feature has one of these durations.
fn is_session_timer(duration: Duration) -> bool {
    session_timers().contains(&duration)
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
/// that follow each other. The names leave out the timers of the MQTT
/// session.
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
            Job::WifiOn { token, .. } => vec![format!("wifi on {token}")],
            Job::WifiOff(_) => vec!["wifi off".into()],
            Job::WifiQuiet(_) => vec!["wifi quiet".into()],
            Job::Sleep(_) => vec!["sleep".into()],
            Job::FrontLight { token, level } => vec![format!("light {level} {token}")],
            Job::Timer {
                op: TimerOp::NotifyAfter(notify),
                ..
            } if is_session_timer(notify.duration.into()) => Vec::new(),
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(_),
            } => vec![format!("start timer {token}")],
            Job::Timer {
                token,
                op: TimerOp::ClearTimer(_),
            } => vec![format!("cancel timer {token}")],
        })
        .collect()
}

fn started(power: PowerSource, read_only: bool) -> (Device, Vec<Job>) {
    let mut shell = Shell::new();
    let config = Config {
        read_only,
        ..CONFIG
    };
    let start = start(&config, power, Some("/room/living_room".into()), None);
    let jobs = shell.send(Event::Start(start));
    let device = Device {
        shell,
        broker: Broker::default(),
    };
    (device, jobs)
}

/// Starts on the stand and connects. Returns the device, with the first
/// connection online and subscribed.
fn online(read_only: bool) -> Device {
    let (mut device, _) = started(PowerSource::Stand, read_only);
    device.handle(joined(1));
    let jobs = device.connect(FIRST);
    device.deliver(&jobs);
    device
}

/// The answer of the Wi-Fi task to the request with `token`.
const fn joined(token: u32) -> Signal {
    Signal::Wifi {
        token,
        status: WifiStatus::Connected,
    }
}

/// Returns the id, the token and the duration of each timer that `jobs`
/// start, in order. The list leaves out the timers of the MQTT session.
fn started_timers(jobs: &[Job]) -> Vec<(crux_time::TimerId, u32, Duration)> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(notify),
            } => Some((notify.id, *token, Duration::from(notify.duration))),
            _ => None,
        })
        .filter(|(_, _, duration)| !is_session_timer(*duration))
        .collect()
}

/// The signal of the user intent that ends the idle time now. The tests of
/// the sleep use it, so they do not need the idle timer.
const fn sleep_now() -> Signal {
    Signal::Event(Event::SleepNow)
}

#[test]
fn start_draws_and_turns_wifi_on() {
    let (_, jobs) = started(PowerSource::Stand, true);
    // The core turns the light off, because its state after a reset is not
    // known. Timer 1 is the full-refresh timer of the core.
    assert_eq!(
        names(&jobs),
        ["light 0 1", "render", "start timer 1", "wifi on 1"]
    );
}

#[test]
fn wifi_then_the_stream_opens_and_the_core_connects_publishes_the_status_and_subscribes() {
    let (mut device, _) = started(PowerSource::Stand, true);
    let jobs = device.handle(joined(1));
    assert_eq!(names(&jobs), [format!("open {FIRST}")]);
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
/// timer, and answers the new `WifiOn` with `wifi_token`. Returns the jobs
/// after the broker accepts the new connection `next`.
fn reconnect(device: &mut Device, connection: u32, wifi_token: u32, next: u32) -> Vec<Job> {
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
    let jobs = device.handle(joined(wifi_token));
    assert_eq!(names(&jobs), [format!("open {next}")]);
    device.connect(next)
}

#[test]
fn each_connection_subscribes_again() {
    let mut device = online(true);
    let next = SECOND;
    let jobs = reconnect(&mut device, FIRST, 2, next);
    assert_eq!(
        names(&jobs),
        [
            format!("publish dashboard/test/status {next}"),
            format!("subscribe {next}"),
            "render".into(),
        ]
    );
}

#[test]
fn the_subscribe_answer_makes_the_link_quiet() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    let jobs = device.connect(FIRST);
    let jobs = device.deliver(&jobs);
    assert_eq!(names(&jobs), ["wifi quiet"]);
}

#[test]
fn the_end_of_the_stream_fails_the_write_that_waits() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    // The write with the status and the subscribe gets no answer.
    device.connect(FIRST);

    // The stream ends first, so the shell answers the write with `Failed`,
    // and the link is not quiet. The new connection subscribes again.
    let next = SECOND;
    let jobs = reconnect(&mut device, FIRST, 2, next);
    assert!(!names(&jobs).contains(&"wifi quiet".into()));
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
fn a_failed_write_ends_the_session_and_closes_the_stream() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    device.connect(FIRST);
    let jobs = device.handle(Signal::Written {
        connection: FIRST,
        outcome: Written::Failed("reset by peer".into()),
    });
    assert!(
        names(&jobs).contains(&format!("close {FIRST}")),
        "{:?}",
        names(&jobs)
    );
    let Some((_, StreamOp::Close(close))) = stream_job(&jobs) else {
        panic!("not a close");
    };
    assert!(!close.clean);
    assert!(close.reason.contains("reset by peer"), "{}", close.reason);
}

#[test]
fn the_end_of_an_old_stream_after_a_new_open_ends_only_the_old_stream() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    device.connect(FIRST);
    // The write fails. The core closes the stream, and opens a new one
    // before the old stream ends.
    let jobs = device.handle(Signal::Written {
        connection: FIRST,
        outcome: Written::Failed("timeout".into()),
    });
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    device.handle(Signal::Timer { id, token });
    let next = SECOND;
    let jobs = device.handle(joined(2));
    assert_eq!(names(&jobs), [format!("open {next}")]);

    // The old stream ends. The shell answers its `Close`, and the core
    // ignores the answer. The new stream continues.
    device.broker.drop_connection();
    assert_eq!(device.closed(FIRST, "closed"), Vec::new());
    let jobs = device.connect(next);
    assert!(names(&jobs).contains(&format!("subscribe {next}")));
}

#[test]
fn a_session_timer_ends_the_session_and_closes_the_stream() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
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
fn a_refused_subscribe_does_not_make_the_link_quiet() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    let jobs = device.connect(FIRST);
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
    assert!(!names(&jobs).contains(&"wifi quiet".into()));
}

#[test]
fn a_second_wifi_answer_is_ignored() {
    let (mut device, _) = started(PowerSource::Stand, true);
    device.handle(joined(1));
    assert_eq!(device.handle(joined(1)), Vec::new());
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
    // light timer, but it does not draw, also on battery.
    // On battery, the idle timer has token 2, so the light timer has 3.
    for (power, level, timer) in [
        (PowerSource::Stand, STAND_LEVEL, 2),
        (PowerSource::Battery, BATTERY_LEVEL, 3),
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
    assert_eq!(names(&jobs), ["light 0 3"]);

    // No answer came for jobs 1 to 3. An answer with a newer token answers
    // all of them, so the sleep continues.
    let jobs = device.handle(Signal::Light { token: 5 });
    assert_eq!(names(&jobs), ["wifi off"]);
    // Each request ended, so a second answer does nothing.
    assert_eq!(device.handle(Signal::Light { token: 5 }), Vec::new());
}

#[test]
fn on_battery_the_sleep_waits_for_the_light_before_it_disconnects() {
    let (mut device, _) = started(PowerSource::Battery, true);
    device.handle(Signal::Light { token: 1 });
    device.handle(joined(1));
    let jobs = device.connect(FIRST);
    device.deliver(&jobs);
    let jobs = device.handle(Signal::Tap(Point::new(100, 20)));
    assert!(names(&jobs).contains(&"light 25 2".into()));
    device.handle(Signal::Light { token: 2 });

    // The first step of the sleep turns the light off, and the core waits.
    let jobs = device.handle(sleep_now());
    assert_eq!(names(&jobs), ["light 0 3"]);
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
    assert_eq!(names(&jobs), ["light 0 3"]);

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
    device.handle(joined(1));
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
    // ended yet.
    let jobs = device.handle(Signal::Tap(Point::new(1, 1)));
    let names = names(&jobs);
    assert!(names.contains(&"wifi on 2".into()), "{names:?}");
    assert!(!device.view().asleep);

    // The old stream ends, and the new connection opens.
    device.broker.drop_connection();
    device.closed(FIRST, "closed");
    let jobs = device.handle(joined(2));
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
    assert_eq!(names(&jobs), ["wifi off"]);
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
    // The idle timer and the full-refresh timer.
    assert_eq!(device.timer_count(), 2);
}

#[test]
fn a_timer_that_the_core_clears_cancels_its_task_and_the_count_does_not_grow() {
    // A page turn draws with a full refresh. The core clears the
    // full-refresh timer and starts a new one.
    let mut device = online(true);
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
    let start = start(&CONFIG, PowerSource::Stand, None, Some(5));
    assert_eq!(start.settings.host, "broker");
    assert_eq!(start.settings.port, 1883);
    assert_eq!(start.settings.device_id, "test");
    let credentials = start.settings.credentials.unwrap();
    assert_eq!(credentials.username, "dashboard");
    assert_eq!(start.last_sync, Some(5));
    assert!(start.settings.read_only);

    let anonymous = Config {
        mqtt_username: "",
        ..CONFIG
    };
    let start = super::start(&anonymous, PowerSource::Stand, None, None);
    assert_eq!(start.settings.credentials, None);
}

#[test]
fn a_late_wifi_answer_after_wifi_off_is_dropped() {
    let (mut device, _) = started(PowerSource::Battery, true);
    // The idle time ends before Wi-Fi joins: the core turns Wi-Fi off.
    let jobs = device.handle(sleep_now());
    assert!(
        names(&jobs).contains(&"wifi off".into()),
        "{:?}",
        names(&jobs)
    );
    // The join of request 1 ends after `WifiOff`. The shell drops the answer.
    assert_eq!(device.handle(joined(1)), Vec::new());
    assert!(device.view().asleep);
}

#[test]
fn an_answer_is_also_for_the_older_requests() {
    let (mut device, _) = started(PowerSource::Stand, true);
    // The answer with a newer token than any request answers request 1.
    let jobs = device.handle(joined(5));
    assert_eq!(names(&jobs), [format!("open {FIRST}")]);
    // An answer with an older token than all requests is dropped.
    assert_eq!(device.handle(joined(0)), Vec::new());
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
    // No stream is open, so the close ends at once.
    let jobs = device.handle(sleep_now());
    assert_eq!(names(&jobs), ["wifi off"]);
    let jobs = device.handle(Signal::RadioOff);
    let names = names(&jobs);
    // The core clears the full-refresh timer before it sleeps.
    assert!(names.contains(&"cancel timer 1".into()), "{names:?}");
    // Only the idle timer of the start runs. It is stale, because
    // `SleepNow` ended the idle time.
    assert_eq!(device.timer_count(), 1);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert_eq!(names.iter().filter(|name| *name == "render").count(), 1);
    let Job::Render(view) = &jobs[render] else {
        unreachable!()
    };
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
    device.handle(joined(1));
    let jobs = device.connect(FIRST);
    device.deliver(&jobs);
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
    device.handle(Signal::RadioOff);
    assert!(device.is_sleeping());
    device
}

#[test]
fn each_step_of_the_sleep_waits_for_the_answer_of_its_task() {
    let (mut device, jobs) = idle_timeout();
    // Step 1: `asleep`, then the disconnect, in one write.
    assert_eq!(
        names(&jobs),
        [
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

    // The stream ends: the close is done. Step 3: Wi-Fi off.
    device.broker.drop_connection();
    let jobs = device.closed(FIRST, "closed");
    assert_eq!(names(&jobs), ["wifi off"]);
    // The broker keeps `asleep`, because the client sent a DISCONNECT.
    assert_eq!(
        device.broker.retained("dashboard/test/status"),
        Some("asleep")
    );

    // The radio is off. Step 4: the last image, then the sleep.
    let jobs = device.handle(Signal::RadioOff);
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
        self::names(&jobs).contains(&"wifi on 2".into()),
        "{:?}",
        self::names(&jobs)
    );
}

#[test]
fn a_close_with_no_stream_ends_at_once() {
    // Wi-Fi never joins, so no stream opens.
    let (mut device, _) = started(PowerSource::Battery, true);
    let jobs = device.handle(sleep_now());
    // The close ends at once, so Wi-Fi off follows in the same batch.
    assert_eq!(names(&jobs), ["wifi off"]);
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
        names(&jobs).contains(&"wifi on 2".into()),
        "{:?}",
        names(&jobs)
    );
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

    device.handle(joined(2));
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
    assert_eq!(device.handle(Signal::RadioOff), Vec::new());
}

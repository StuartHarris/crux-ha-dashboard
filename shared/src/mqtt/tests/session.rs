//! Tests of the session against the byte-level fake broker.

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::time::Duration;

use super::{
    PASSWORD,
    wire::{Chunks, Wire, client_packets, remaining_lengths},
};
use crate::{
    entities::topics,
    mqtt::{
        Action, End, Input, Limits, Message, Publish, Qos, Refused, Session, SubscribeOutcome,
        TimerKind,
        codec::{self, Flags, server::Packet},
    },
    pages::PAGES,
};

/// A publish at `QoS` 1 with no retain flag.
fn command(payload: &str) -> Publish {
    Publish {
        topic: "dashboard/hall/cmd".into(),
        payload: payload.as_bytes().to_vec(),
        qos: Qos::AtLeastOnce,
        retain: false,
    }
}

/// The retained `online` publish on the status topic.
fn online() -> Publish {
    Publish {
        topic: "dashboard/hall/status".into(),
        payload: b"online".to_vec(),
        qos: Qos::AtLeastOnce,
        retain: true,
    }
}

/// Returns the subscribe outcomes in `log`.
fn outcomes(log: &[Action]) -> Vec<SubscribeOutcome> {
    log.iter()
        .filter_map(|action| match action {
            Action::Subscribed { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect()
}

/// Returns the topics of the messages in `log`.
fn message_topics(log: &[Action]) -> Vec<String> {
    log.iter()
        .filter_map(|action| match action {
            Action::Message(message) => Some(message.topic.clone()),
            _ => None,
        })
        .collect()
}

/// Polls the session until it has no action, and returns the bytes of the
/// one `Send` in the actions.
fn take_send(session: &mut Session) -> Vec<u8> {
    let mut sends: Vec<Vec<u8>> = poll_all(session)
        .into_iter()
        .filter_map(|action| match action {
            Action::Send(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(sends.len(), 1);
    sends.remove(0)
}

/// Polls the session until it has no action, and returns the actions.
fn poll_all(session: &mut Session) -> Vec<Action> {
    core::iter::from_fn(|| session.poll()).collect()
}

/// A connected session and the 115 topics of the dashboard, each with a
/// retained message whose payload is the topic. The list of timers is
/// empty.
fn house() -> (Wire, Vec<String>) {
    let topics = topics(PAGES);
    assert_eq!(topics.len(), 115);
    let mut wire = Wire::connected();
    wire.timers.clear();
    for topic in &topics {
        wire.broker.retain(topic, topic.as_bytes());
    }
    (wire, topics)
}

#[test]
fn the_session_connects_with_a_clean_session_and_a_will_at_qos_1() {
    let mut wire = Wire::new();
    wire.open();

    assert_eq!(wire.log, [Action::Connected]);
    let Packet::Connect {
        flags,
        keep_alive,
        client_id,
        will_topic,
        will_payload,
        username,
        password,
    } = &wire.broker.packets[0]
    else {
        panic!("a CONNECT: {:?}", wire.broker.packets);
    };
    assert_eq!(*flags, 0xEE);
    assert_eq!(*keep_alive, 60);
    assert_eq!(client_id, "dashboard-hall");
    assert_eq!(will_topic, "dashboard/hall/status");
    assert_eq!(will_payload, b"offline");
    assert_eq!(username.as_deref(), Some("dashboard"));
    assert_eq!(password.as_deref(), Some(PASSWORD.as_bytes()));
}

#[test]
fn the_session_starts_its_timers_in_order() {
    let mut wire = Wire::new();
    wire.open();
    assert_eq!(
        wire.timer_kinds(),
        [
            (TimerKind::Open, Duration::from_secs(10)),
            (TimerKind::ConnAck, Duration::from_secs(10)),
            (TimerKind::Send, Duration::from_secs(10)),
            (TimerKind::Ping, Duration::from_secs(45)),
            (TimerKind::Silence, Duration::from_secs(120)),
        ]
    );
}

#[test]
fn a_keep_alive_of_0_starts_no_ping_or_silence_timer() {
    let mut connect = super::connect();
    connect.keep_alive_secs = 0;
    let mut session = Session::new(&connect, Limits::default());
    session.handle(Input::Opened);
    let _ = poll_all(&mut session);
    session.handle(Input::Sent);
    let mut connack = Vec::new();
    codec::server::connack(&mut connack, false, 0);
    session.handle(Input::Received(connack));
    assert_eq!(poll_all(&mut session), [Action::Connected]);
}

#[test]
fn a_refused_connack_ends_the_session_with_its_code() {
    for code in 1..=5 {
        let mut wire = Wire::new();
        wire.broker.code = code;
        wire.open();
        assert_eq!(wire.log, [Action::Close, Action::Ended(End::Refused(code))]);
    }
}

#[test]
fn a_session_present_flag_ends_the_clean_session() {
    let mut wire = Wire::new();
    wire.broker.session_present = true;
    wire.open();
    assert_eq!(
        wire.log,
        [Action::Close, Action::Ended(End::SessionPresent)]
    );
}

#[test]
fn requests_before_the_connack_are_refused() {
    let mut session = Session::new(&super::connect(), Limits::default());
    assert_eq!(session.publish(online()), Err(Refused::NotConnected));
    let id = session.subscribe(&["a".to_string()]);
    session.handle(Input::Opened);
    assert_eq!(session.publish(online()), Err(Refused::NotConnected));
    let actions = poll_all(&mut session);
    assert!(actions.contains(&Action::Subscribed {
        id,
        outcome: SubscribeOutcome::Failed
    }));
    // Only the CONNECT packet goes out.
    let sends: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Send(bytes) => Some(client_packets(bytes)),
            _ => None,
        })
        .collect();
    assert_eq!(sends.len(), 1);
    assert!(matches!(sends[0][..], [Packet::Connect { .. }]));
}

#[test]
fn the_burst_after_the_connack_leaves_in_one_send() {
    let topics = topics(PAGES);
    let mut wire = Wire::new();
    wire.session.handle(Input::Opened);
    wire.run_with(|session, action| {
        if *action == Action::Connected {
            session.publish(command("queued")).unwrap();
            session.publish(online()).unwrap();
            let _ = session.subscribe(&topics);
        }
    });

    // The CONNECT, then one send with all the rest.
    assert_eq!(wire.sends.len(), 2);
    let packets = client_packets(&wire.sends[1]);
    let Packet::Publish { payload, .. } = &packets[0] else {
        panic!("the outbox first: {packets:?}");
    };
    assert_eq!(payload, b"queued");
    let Packet::Publish { payload, .. } = &packets[1] else {
        panic!("then online: {packets:?}");
    };
    assert_eq!(payload, b"online");

    let mut filters = Vec::new();
    for packet in &packets[2..] {
        let Packet::Subscribe { filters: more, .. } = packet else {
            panic!("then the subscribes: {packets:?}");
        };
        filters.extend(more.iter().cloned());
    }
    assert!(filters.iter().all(|(_, qos)| *qos == 0));
    let filters: Vec<String> = filters.into_iter().map(|(filter, _)| filter).collect();
    assert_eq!(filters, topics);
    assert_eq!(outcomes(&wire.log), [SubscribeOutcome::Done]);
}

#[test]
fn the_115_topics_pack_into_subscribes_of_at_most_1024_bytes() {
    let (mut wire, topics) = house();
    let _ = wire.session.subscribe(&topics);
    wire.run();

    assert_eq!(wire.sends.len(), 1);
    // The SUBSCRIBE packets use less than half of the outgoing limit, so the
    // outbox and the status fit in the same burst.
    let size = wire.sends[0].len();
    assert!(size * 2 < crate::mqtt::MAX_OUTGOING, "{size} bytes");
    let lengths = remaining_lengths(&wire.sends[0]);
    assert!(lengths.len() > 1, "{lengths:?}");
    assert!(lengths.len() <= 8, "{lengths:?}");
    assert!(lengths.iter().all(|length| *length <= 1024), "{lengths:?}");
    // Each packet but the last is too full for one more filter.
    let longest = topics.iter().map(String::len).max().unwrap();
    for length in &lengths[..lengths.len() - 1] {
        assert!(length + codec::subscribe_entry_length(&"x".repeat(longest)) > 1024);
    }
    assert_eq!(outcomes(&wire.log), [SubscribeOutcome::Done]);
}

#[test]
fn the_retained_flood_gives_each_message_in_order_and_starts_no_timer_for_each_packet() {
    let (mut wire, topics) = house();
    let _ = wire.session.subscribe(&topics);
    wire.run();

    assert_eq!(message_topics(&wire.log), topics);
    for action in &wire.log {
        if let Action::Message(Message { topic, payload }) = action {
            assert_eq!(topic.as_bytes(), payload);
        }
    }
    // Only the timer of the acknowledgements and the timer of the one send.
    assert_eq!(
        wire.timer_kinds(),
        [
            (TimerKind::Send, Duration::from_secs(10)),
            (TimerKind::Ack, Duration::from_secs(10)),
        ]
    );
}

#[test]
fn the_retained_flood_in_random_chunks_gives_the_same_messages() {
    for seed in [1, 7, 42, 1_000] {
        let (mut wire, topics) = house();
        wire.chunks = Chunks::Random(seed, 1460);
        let _ = wire.session.subscribe(&topics);
        wire.run();
        assert_eq!(message_topics(&wire.log), topics, "seed {seed}");
        assert_eq!(outcomes(&wire.log), [SubscribeOutcome::Done]);
        assert_eq!(wire.timers.len(), 2, "seed {seed}");
    }
}

#[test]
fn a_refused_filter_fails_the_subscribe() {
    let (mut wire, topics) = house();
    wire.broker.refuse = Some(topics[50].clone());
    let _ = wire.session.subscribe(&topics);
    wire.run();
    assert_eq!(outcomes(&wire.log), [SubscribeOutcome::Failed]);
}

#[test]
fn an_empty_subscribe_is_done_at_once() {
    let mut wire = Wire::connected();
    let id = wire.session.subscribe(&[]);
    wire.run();
    assert_eq!(wire.sends, [] as [Vec<u8>; 0]);
    assert_eq!(
        wire.log,
        [Action::Subscribed {
            id,
            outcome: SubscribeOutcome::Done
        }]
    );
}

#[test]
fn a_closed_stream_fails_the_jobs_that_wait_and_then_ends() {
    let (mut wire, topics) = house();
    wire.broker.hold_suback = true;
    let first = wire.session.subscribe(&topics[..10]);
    let second = wire.session.subscribe(&topics[10..]);
    wire.run();
    wire.log.clear();

    wire.session
        .handle(Input::Closed("connection reset".into()));
    wire.run();
    assert_eq!(
        wire.log,
        [
            Action::Subscribed {
                id: first,
                outcome: SubscribeOutcome::Failed
            },
            Action::Subscribed {
                id: second,
                outcome: SubscribeOutcome::Failed
            },
            Action::Ended(End::Transport("connection reset".into())),
        ]
    );
}

#[test]
fn the_session_ignores_all_input_after_the_end() {
    let mut wire = Wire::connected();
    wire.session.handle(Input::Closed("reset".into()));
    wire.run();
    wire.log.clear();

    let timer = wire.timer(TimerKind::Ping);
    wire.session.handle(Input::Timer(timer));
    wire.session.handle(Input::Opened);
    wire.session.handle(Input::Received(vec![0xD0, 0x00]));
    wire.session.handle(Input::Closed("again".into()));
    let _ = wire.session.subscribe(&["a".to_string()]);
    wire.session.disconnect();
    assert_eq!(wire.session.publish(online()), Err(Refused::NotConnected));
    assert_eq!(wire.session.poll(), None);
}

#[test]
fn publish_ids_skip_0_and_the_ids_in_flight() {
    let mut wire = Wire::connected();
    // The broker holds the PUBACK of id 1 while the counter wraps.
    wire.broker.hold_puback = true;
    wire.session.publish(command("held")).unwrap();
    wire.run();
    wire.broker.hold_puback = false;

    let mut published = 1;
    while published < usize::from(u16::MAX) {
        // One publish is in flight, so 63 more fit in the window.
        for _ in 0..63.min(usize::from(u16::MAX) - published) {
            wire.session.publish(command("x")).unwrap();
            published += 1;
        }
        wire.run();
        wire.broker.packets.clear();
    }
    wire.session.publish(command("after the wrap")).unwrap();
    wire.run();

    let Packet::Publish { id, payload, .. } = &wire.broker.packets[0] else {
        panic!("a publish");
    };
    assert_eq!(payload, b"after the wrap");
    // 65,535 wraps to 0, which is not a packet id, and 1 is in flight.
    assert_eq!(*id, Some(2));
    assert_eq!(wire.broker.held_pubacks, [1]);
}

#[test]
fn a_puback_frees_its_id_and_an_unknown_puback_is_ignored() {
    let mut wire = Wire::connected();
    wire.broker.hold_puback = true;
    for _ in 0..64 {
        wire.session.publish(command("x")).unwrap();
    }
    assert_eq!(wire.session.publish(command("x")), Err(Refused::Full));
    // A publish at QoS 0 has no id, so the window does not apply.
    let mut status = online();
    status.qos = Qos::AtMostOnce;
    assert_eq!(wire.session.publish(status), Ok(()));
    wire.run();

    let mut acks = Vec::new();
    codec::server::puback(&mut acks, 999);
    wire.deliver(&acks);
    assert_eq!(wire.session.publish(command("x")), Err(Refused::Full));

    let mut acks = Vec::new();
    codec::server::puback(&mut acks, 5);
    wire.deliver(&acks);
    assert_eq!(wire.session.publish(command("x")), Ok(()));
    assert_eq!(wire.session.publish(command("x")), Err(Refused::Full));
}

#[test]
fn subscribe_ids_share_the_counter_with_publishes() {
    let mut wire = Wire::connected();
    wire.broker.hold_suback = true;
    let _ = wire.session.subscribe(&["a".to_string()]);
    wire.session.publish(command("x")).unwrap();
    wire.run();
    let ids: Vec<u16> = wire
        .broker
        .packets
        .iter()
        .filter_map(|packet| match packet {
            Packet::Subscribe { id, .. } => Some(*id),
            Packet::Publish { id, .. } => *id,
            _ => None,
        })
        .collect();
    assert_eq!(ids, [1, 2]);
}

#[test]
fn a_publish_that_is_too_long_is_refused() {
    let mut wire = Wire::connected();
    let mut publish = online();
    publish.topic = "t".repeat(usize::from(u16::MAX) + 1);
    assert_eq!(wire.session.publish(publish), Err(Refused::TooLong));
    wire.run();
    assert_eq!(wire.sends, [] as [Vec<u8>; 0]);
}

#[test]
fn the_session_pings_every_45_seconds() {
    let mut wire = Wire::connected();
    for _ in 0..3 {
        let ping = wire.timer(TimerKind::Ping);
        wire.timers.clear();
        wire.sends.clear();
        wire.fire(ping);
        assert_eq!(wire.sends, [vec![0xC0, 0x00]]);
        assert_eq!(
            wire.timer_kinds(),
            [
                (TimerKind::PingResp, Duration::from_secs(10)),
                (TimerKind::Ping, Duration::from_secs(45)),
                (TimerKind::Send, Duration::from_secs(10)),
            ]
        );
        // The PINGRESP arrived, so its timer does nothing.
        let response = wire.timer(TimerKind::PingResp);
        wire.fire(response);
    }
    assert!(wire.log.is_empty(), "{:?}", wire.log);
}

#[test]
fn no_pingresp_in_time_ends_the_session() {
    let mut wire = Wire::connected();
    wire.broker.hold_pingresp = true;
    wire.fire(wire.timer(TimerKind::Ping));
    wire.fire(wire.timer(TimerKind::PingResp));
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Ended(End::Timeout(TimerKind::PingResp))
        ]
    );
}

#[test]
fn a_silent_broker_ends_the_session() {
    let mut wire = Wire::connected();
    wire.fire(wire.timer(TimerKind::Silence));
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Ended(End::Timeout(TimerKind::Silence))
        ]
    );
}

#[test]
fn bytes_from_the_broker_start_the_silence_timer_again() {
    let mut wire = Wire::connected();
    let first = wire.timer(TimerKind::Silence);
    wire.fire(wire.timer(TimerKind::Ping));
    wire.timers.clear();
    // The PINGRESP arrived in the period, so the period starts again.
    wire.fire(first);
    assert!(wire.log.is_empty(), "{:?}", wire.log);
    assert_eq!(
        wire.timer_kinds(),
        [(TimerKind::Silence, Duration::from_secs(120))]
    );
    // Nothing arrived in the second period.
    wire.fire(wire.timer(TimerKind::Silence));
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Ended(End::Timeout(TimerKind::Silence))
        ]
    );
}

#[test]
fn a_send_that_does_not_finish_in_time_ends_the_session() {
    let mut session = Session::new(&super::connect(), Limits::default());
    session.handle(Input::Opened);
    let actions = poll_all(&mut session);
    let send = actions
        .iter()
        .find_map(|action| match action {
            Action::StartTimer { timer, .. } if timer.kind == TimerKind::Send => Some(*timer),
            _ => None,
        })
        .expect("a send timer");
    session.handle(Input::Timer(send));
    assert_eq!(
        poll_all(&mut session),
        [Action::Close, Action::Ended(End::Timeout(TimerKind::Send))]
    );
}

#[test]
fn a_stream_that_does_not_open_in_time_ends_the_session() {
    let mut session = Session::new(&super::connect(), Limits::default());
    let [Action::StartTimer { timer, after }] = poll_all(&mut session)[..] else {
        panic!("one timer");
    };
    assert_eq!(
        (timer.kind, after),
        (TimerKind::Open, Duration::from_secs(10))
    );
    session.handle(Input::Timer(timer));
    assert_eq!(
        poll_all(&mut session),
        [Action::Close, Action::Ended(End::Timeout(TimerKind::Open))]
    );
}

#[test]
fn no_connack_in_time_ends_the_session() {
    let mut wire = Wire::new();
    wire.broker.code = 0;
    wire.session.handle(Input::Opened);
    // The broker does not answer: the harness does not run.
    let actions = poll_all(&mut wire.session);
    let connack = actions
        .iter()
        .find_map(|action| match action {
            Action::StartTimer { timer, .. } if timer.kind == TimerKind::ConnAck => Some(*timer),
            _ => None,
        })
        .expect("a CONNACK timer");
    wire.session.handle(Input::Sent);
    wire.session.handle(Input::Timer(connack));
    assert_eq!(
        poll_all(&mut wire.session),
        [
            Action::Close,
            Action::Ended(End::Timeout(TimerKind::ConnAck))
        ]
    );
}

#[test]
fn a_timer_with_an_old_generation_does_nothing() {
    let mut wire = Wire::connected();
    let old_ping = wire.timer(TimerKind::Ping);
    let old_silence = wire.timer(TimerKind::Silence);
    wire.fire(old_ping);
    wire.sends.clear();
    wire.timers.clear();
    // The ping timer started again, so the first one is old.
    wire.fire(old_ping);
    assert_eq!(wire.sends, [] as [Vec<u8>; 0]);
    assert_eq!(wire.timers, []);

    // The broker answered, so the silence timer started again.
    wire.fire(old_silence);
    wire.fire(old_silence);
    assert!(wire.log.is_empty(), "{:?}", wire.log);
    // The old timers of the open stream and the CONNACK do nothing too.
    let mut fresh = Wire::new();
    fresh.open();
    let open = fresh.timer(TimerKind::Open);
    let connack = fresh.timer(TimerKind::ConnAck);
    let send = fresh.timer(TimerKind::Send);
    fresh.log.clear();
    for timer in [open, connack, send] {
        fresh.fire(timer);
    }
    assert!(fresh.log.is_empty(), "{:?}", fresh.log);
}

#[test]
fn requests_while_a_send_is_in_flight_coalesce_in_call_order() {
    let mut wire = Wire::connected();
    wire.session.publish(command("first")).unwrap();
    let first = take_send(&mut wire.session);
    assert_eq!(client_packets(&first).len(), 1);

    // The first send is in flight.
    wire.session.publish(command("second")).unwrap();
    let _ = wire.session.subscribe(&["a/#".to_string()]);
    wire.session.publish(command("third")).unwrap();
    assert!(
        !poll_all(&mut wire.session)
            .iter()
            .any(|action| matches!(action, Action::Send(_)))
    );

    wire.session.handle(Input::Sent);
    let second = take_send(&mut wire.session);
    let packets = client_packets(&second);
    let payloads: Vec<&[u8]> = packets
        .iter()
        .map(|packet| match packet {
            Packet::Publish { payload, .. } => payload.as_slice(),
            Packet::Subscribe { .. } => b"subscribe",
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(payloads, [&b"second"[..], b"subscribe", b"third"]);
}

#[test]
fn a_connack_before_the_sent_of_the_connect_still_connects() {
    let mut session = Session::new(&super::connect(), Limits::default());
    session.handle(Input::Opened);
    let _ = poll_all(&mut session);
    let mut connack = Vec::new();
    codec::server::connack(&mut connack, false, 0);
    session.handle(Input::Received(connack));
    session.publish(online()).unwrap();
    let actions = poll_all(&mut session);
    assert!(actions.contains(&Action::Connected));
    // The CONNECT is still in flight, so the publish waits.
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Send(_)))
    );
    session.handle(Input::Sent);
    assert!(matches!(session.poll(), Some(Action::Send(_))));
}

#[test]
fn broken_framing_ends_the_session_after_the_messages_before_it() {
    let (mut wire, topics) = house();
    let _ = wire.session.subscribe(&topics[..1]);
    wire.run();
    wire.log.clear();

    wire.broker.publish(&topics[0], b"on", false);
    let mut bytes = wire.broker.take();
    bytes.extend_from_slice(&[0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    wire.deliver(&bytes);
    wire.run();
    assert_eq!(
        wire.log,
        [
            Action::Message(Message {
                topic: topics[0].clone(),
                payload: b"on".to_vec()
            }),
            Action::Close,
            Action::Ended(End::Malformed),
        ]
    );
}

#[test]
fn a_malformed_packet_ends_the_session() {
    let cases: [&[u8]; 4] = [
        // CONNACK with reserved bits.
        &[0x20, 0x02, 0x02, 0x00],
        // A second CONNACK.
        &[0x20, 0x02, 0x00, 0x00],
        // PUBLISH at QoS 3.
        &[0x36, 0x05, 0x00, 0x01, b'a', 0x00, 0x01],
        // UNSUBACK, which the client never asks for.
        &[0xB0, 0x02, 0x00, 0x01],
    ];
    for bytes in cases {
        let mut wire = Wire::connected();
        wire.deliver(bytes);
        wire.run();
        assert_eq!(
            wire.log,
            [Action::Close, Action::Ended(End::Malformed)],
            "{bytes:x?}"
        );
    }
}

#[test]
fn a_packet_before_the_connack_is_malformed() {
    let mut session = Session::new(&super::connect(), Limits::default());
    session.handle(Input::Opened);
    let _ = poll_all(&mut session);
    session.handle(Input::Received(vec![0xD0, 0x00]));
    assert_eq!(
        poll_all(&mut session),
        [Action::Close, Action::Ended(End::Malformed)]
    );
}

#[test]
fn an_oversize_packet_is_skipped_and_the_session_stays_connected() {
    let limits = Limits {
        max_packet: 64,
        ..Limits::default()
    };
    let mut wire = Wire::with(limits);
    wire.open();
    let _ = wire.session.subscribe(&["#".to_string()]);
    wire.run();
    wire.log.clear();

    wire.broker.publish("big", &[0; 500], false);
    wire.broker.publish("small", b"ok", false);
    let bytes = wire.broker.take();
    wire.chunks = Chunks::Random(3, 100);
    wire.deliver(&bytes);
    wire.run();
    assert_eq!(
        wire.log,
        [
            Action::Skipped { length: 505 },
            Action::Message(Message {
                topic: "small".into(),
                payload: b"ok".to_vec()
            })
        ]
    );
}

#[test]
fn an_oversize_packet_that_is_not_a_publish_is_malformed() {
    let limits = Limits {
        max_packet: 64,
        ..Limits::default()
    };
    let mut wire = Wire::with(limits);
    wire.open();
    wire.log.clear();
    let mut bytes = vec![0x90, 100, 0x00, 0x01];
    bytes.extend([0; 98]);
    wire.deliver(&bytes);
    wire.run();
    assert_eq!(wire.log, [Action::Close, Action::Ended(End::Malformed)]);
}

#[test]
fn an_inbound_publish_at_qos_1_gets_a_puback() {
    let mut wire = Wire::connected();
    let mut bytes = Vec::new();
    let flags = Flags {
        qos: Qos::AtLeastOnce,
        retain: false,
    };
    codec::publish(&mut bytes, "a/b", b"on", flags, Some(300)).unwrap();
    wire.deliver(&bytes);
    wire.run();
    assert_eq!(
        wire.log,
        [Action::Message(Message {
            topic: "a/b".into(),
            payload: b"on".to_vec()
        })]
    );
    assert_eq!(wire.broker.packets, [Packet::PubAck(300)]);
}

#[test]
fn disconnect_sends_its_packet_last_and_closes_after_its_sent() {
    let mut wire = Wire::connected();
    let mut asleep = online();
    asleep.payload = b"asleep".to_vec();
    wire.session.publish(asleep).unwrap();
    let _ = take_send(&mut wire.session);

    // The publish is in flight when the caller asks for the disconnect.
    wire.session.disconnect();
    assert_eq!(wire.session.publish(online()), Err(Refused::NotConnected));
    // A ping timer now adds nothing after the DISCONNECT packet.
    wire.session
        .handle(Input::Timer(wire.timer(TimerKind::Ping)));
    assert_eq!(poll_all(&mut wire.session), []);

    wire.session.handle(Input::Sent);
    assert_eq!(take_send(&mut wire.session), [0xE0, 0x00]);

    wire.session.handle(Input::Sent);
    assert_eq!(
        poll_all(&mut wire.session),
        [Action::Close, Action::Ended(End::Clean)]
    );
}

#[test]
fn disconnect_with_nothing_in_flight_sends_after_the_requests() {
    let mut wire = Wire::connected();
    wire.broker.hold_suback = true;
    let id = wire.session.subscribe(&["a".to_string()]);
    wire.session.publish(command("bye")).unwrap();
    wire.session.disconnect();
    wire.run();
    assert_eq!(wire.sends.len(), 1);
    let packets = client_packets(&wire.sends[0]);
    assert!(matches!(
        packets[..],
        [
            Packet::Subscribe { .. },
            Packet::Publish { .. },
            Packet::Disconnect
        ]
    ));
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Subscribed {
                id,
                outcome: SubscribeOutcome::Failed
            },
            Action::Ended(End::Clean),
        ]
    );
}

#[test]
fn an_inbound_publish_while_closing_gets_no_puback() {
    let mut wire = Wire::connected();
    wire.session.publish(command("x")).unwrap();
    let _ = poll_all(&mut wire.session);
    wire.session.disconnect();
    let mut bytes = Vec::new();
    let flags = Flags {
        qos: Qos::AtLeastOnce,
        retain: false,
    };
    codec::publish(&mut bytes, "a", b"1", flags, Some(9)).unwrap();
    wire.session.handle(Input::Received(bytes));
    wire.session.handle(Input::Sent);
    let actions = poll_all(&mut wire.session);
    let sends: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Send(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sends, [vec![0xE0, 0x00]]);
}

#[test]
fn disconnect_before_the_connack_closes_at_once() {
    let mut session = Session::new(&super::connect(), Limits::default());
    session.handle(Input::Opened);
    let _ = poll_all(&mut session);
    session.disconnect();
    assert_eq!(
        poll_all(&mut session),
        [Action::Close, Action::Ended(End::Clean)]
    );
}

#[test]
fn a_connect_field_that_is_too_long_ends_the_session_when_the_stream_opens() {
    let mut connect = super::connect();
    connect.client_id = "x".repeat(usize::from(u16::MAX) + 1);
    let mut session = Session::new(&connect, Limits::default());
    session.handle(Input::Opened);
    let actions = poll_all(&mut session);
    assert_eq!(actions[1..], [Action::Close, Action::Ended(End::TooLong)]);
}

#[test]
fn debug_output_does_not_show_the_password() {
    let mut session = Session::new(&super::connect(), Limits::default());
    session.handle(Input::Opened);
    let before = format!("{session:?}");
    let send = poll_all(&mut session)
        .into_iter()
        .find(|action| matches!(action, Action::Send(_)))
        .expect("the CONNECT");
    let Action::Send(bytes) = &send else {
        unreachable!();
    };
    let password = PASSWORD.as_bytes();
    assert!(
        bytes
            .windows(password.len())
            .any(|window| window == password)
    );

    let action = format!("{send:?}");
    assert_eq!(action, format!("Send({} bytes)", bytes.len()));
    for text in [before, format!("{session:?}")] {
        assert!(!text.contains(PASSWORD), "{text}");
        assert!(text.contains("bytes"), "{text}");
        // No list of byte values, which would hold the password.
        assert!(!text.contains("[16,"), "{text}");
    }
}

#[test]
fn end_has_a_message_for_each_reason() {
    let cases = [
        (End::Clean, "the client closed the connection"),
        (
            End::Refused(5),
            "the broker refused the connection (5: not authorized)",
        ),
        (End::Malformed, "the broker sent a packet that is not valid"),
        (
            End::Timeout(TimerKind::PingResp),
            "the broker did not answer the ping in time",
        ),
        (
            End::Transport("connection reset".into()),
            "the stream closed: connection reset",
        ),
        (
            End::SessionPresent,
            "the broker kept a session, but the client asked for a clean session",
        ),
        (End::TooLong, "a field of the CONNECT packet is too long"),
    ];
    for (end, text) in cases {
        assert_eq!(end.to_string(), text);
    }
}

#[test]
fn a_ping_timer_while_a_pingreq_waits_keeps_the_pingresp_deadline() {
    let mut wire = Wire::connected();
    wire.broker.hold_pingresp = true;
    wire.fire(wire.timer(TimerKind::Ping));
    let response = wire.timer(TimerKind::PingResp);
    let ping = wire.timer(TimerKind::Ping);
    wire.sends.clear();
    wire.timers.clear();

    // The next ping timer sends no PINGREQ and starts no PINGRESP timer.
    wire.fire(ping);
    assert_eq!(wire.sends, [] as [Vec<u8>; 0]);
    assert_eq!(
        wire.timer_kinds(),
        [(TimerKind::Ping, Duration::from_secs(45))]
    );
    wire.fire(response);
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Ended(End::Timeout(TimerKind::PingResp))
        ]
    );
}

#[test]
fn an_inbound_publish_at_qos_2_is_malformed() {
    let mut wire = Wire::connected();
    wire.deliver(&[0x34, 0x06, 0x00, 0x01, b'a', 0x00, 0x09, b'1']);
    wire.run();
    assert_eq!(wire.log, [Action::Close, Action::Ended(End::Malformed)]);
    assert_eq!(wire.broker.packets, []);
}

#[test]
fn a_disconnect_that_is_not_sent_in_time_ends_with_a_timeout() {
    let mut wire = Wire::connected();
    wire.session.disconnect();
    let actions = poll_all(&mut wire.session);
    assert_eq!(actions[0], Action::Send(vec![0xE0, 0x00]));
    let Action::StartTimer { timer, .. } = actions[1] else {
        panic!("a send timer: {actions:?}");
    };
    wire.session.handle(Input::Timer(timer));
    assert_eq!(
        poll_all(&mut wire.session),
        [Action::Close, Action::Ended(End::Timeout(TimerKind::Send))]
    );
}

#[test]
fn a_lost_puback_ends_the_session() {
    let mut wire = Wire::connected();
    wire.broker.hold_puback = true;
    wire.session.publish(command("one")).unwrap();
    wire.session.publish(command("two")).unwrap();
    wire.run();
    assert_eq!(wire.broker.held_pubacks, [1, 2]);

    // One PUBACK arrives in the first period, so the timer starts again.
    let mut ack = Vec::new();
    codec::server::puback(&mut ack, 1);
    wire.deliver(&ack);
    let first = wire.timer(TimerKind::Ack);
    wire.timers.clear();
    wire.fire(first);
    assert!(wire.log.is_empty(), "{:?}", wire.log);
    assert_eq!(
        wire.timer_kinds(),
        [(TimerKind::Ack, Duration::from_secs(10))]
    );

    // No PUBACK arrives in the second period.
    wire.fire(wire.timer(TimerKind::Ack));
    assert_eq!(
        wire.log,
        [Action::Close, Action::Ended(End::Timeout(TimerKind::Ack))]
    );
}

#[test]
fn a_lost_suback_fails_the_job_and_ends_the_session() {
    let mut wire = Wire::connected();
    wire.broker.hold_suback = true;
    let id = wire.session.subscribe(&["a".to_string()]);
    wire.run();
    wire.fire(wire.timer(TimerKind::Ack));
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Subscribed {
                id,
                outcome: SubscribeOutcome::Failed
            },
            Action::Ended(End::Timeout(TimerKind::Ack)),
        ]
    );
}

#[test]
fn subacks_that_each_arrive_in_time_do_not_end_the_session() {
    let (mut wire, topics) = house();
    wire.broker.hold_suback = true;
    let id = wire.session.subscribe(&topics);
    wire.run();
    let packets: Vec<(u16, usize)> = wire
        .broker
        .packets
        .iter()
        .filter_map(|packet| match packet {
            Packet::Subscribe { id, filters } => Some((*id, filters.len())),
            _ => None,
        })
        .collect();
    assert!(packets.len() > 2, "{packets:?}");

    // One SUBACK in each period of the timer.
    for (packet, filters) in packets {
        let mut suback = Vec::new();
        codec::server::suback(&mut suback, packet, &vec![0; filters]);
        wire.deliver(&suback);
        wire.fire(wire.timer(TimerKind::Ack));
    }
    let outcome = Action::Subscribed {
        id,
        outcome: SubscribeOutcome::Done,
    };
    assert_eq!(wire.log.last(), Some(&outcome));
    // Nothing waits, so the last timer does nothing.
    wire.fire(wire.timer(TimerKind::Ack));
    assert!(
        !wire
            .log
            .iter()
            .any(|action| matches!(action, Action::Ended(_)))
    );
}

#[test]
fn a_puback_of_the_last_publish_stops_the_ack_watch() {
    let mut wire = Wire::connected();
    wire.session.publish(command("one")).unwrap();
    wire.run();
    wire.fire(wire.timer(TimerKind::Ack));
    assert!(wire.log.is_empty(), "{:?}", wire.log);
    // The next publish starts the timer again.
    wire.timers.clear();
    wire.session.publish(command("two")).unwrap();
    wire.run();
    assert_eq!(
        wire.timer_kinds(),
        [
            (TimerKind::Send, Duration::from_secs(10)),
            (TimerKind::Ack, Duration::from_secs(10)),
        ]
    );
}

#[test]
fn a_suback_with_the_wrong_number_of_codes_is_malformed() {
    let mut wire = Wire::connected();
    wire.broker.hold_suback = true;
    let id = wire.session.subscribe(&["a".to_string(), "b".to_string()]);
    wire.run();
    let mut suback = Vec::new();
    codec::server::suback(&mut suback, 1, &[0]);
    wire.deliver(&suback);
    wire.run();
    assert_eq!(
        wire.log,
        [
            Action::Close,
            Action::Subscribed {
                id,
                outcome: SubscribeOutcome::Failed
            },
            Action::Ended(End::Malformed),
        ]
    );
}

#[test]
fn a_close_after_disconnect_is_a_clean_end() {
    let mut wire = Wire::connected();
    wire.session.publish(command("bye")).unwrap();
    let _ = take_send(&mut wire.session);
    wire.session.disconnect();
    wire.session
        .handle(Input::Closed("connection reset".into()));
    assert_eq!(poll_all(&mut wire.session), [Action::Ended(End::Clean)]);
}

#[test]
fn requests_above_the_outgoing_limit_are_refused() {
    let limits = Limits {
        max_outgoing: 100,
        ..Limits::default()
    };
    let mut wire = Wire::with(limits);
    wire.open();
    wire.log.clear();
    let mut publish = command(&"x".repeat(60));
    wire.session.publish(publish.clone()).unwrap();
    assert_eq!(wire.session.publish(publish.clone()), Err(Refused::Full));
    let id = wire.session.subscribe(&["a/b/c".repeat(4)]);
    let actions = poll_all(&mut wire.session);
    assert!(actions.contains(&Action::Subscribed {
        id,
        outcome: SubscribeOutcome::Failed
    }));
    // The refused requests added no bytes.
    let sends: Vec<usize> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Send(bytes) => Some(client_packets(bytes).len()),
            _ => None,
        })
        .collect();
    assert_eq!(sends, [1]);

    // After the send, the bytes no longer wait.
    wire.session.handle(Input::Sent);
    publish.payload = b"again".to_vec();
    assert_eq!(wire.session.publish(publish), Ok(()));
}

/// Returns the `Ack` timers in `actions`.
fn ack_timers(actions: &[Action]) -> Vec<crate::mqtt::Timer> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::StartTimer { timer, .. } if timer.kind == TimerKind::Ack => Some(*timer),
            _ => None,
        })
        .collect()
}

#[test]
fn the_ack_watch_starts_when_the_publish_leaves_not_when_it_is_made() {
    let mut wire = Wire::connected();
    let mut status = online();
    status.qos = Qos::AtMostOnce;
    wire.session.publish(status).unwrap();
    let _ = take_send(&mut wire.session);

    // A slow send is in flight. The publish waits for the next send, so its
    // acknowledgement cannot wait yet.
    wire.session.publish(command("slow")).unwrap();
    assert_eq!(ack_timers(&poll_all(&mut wire.session)), []);

    // The slow send finishes at 9.5 s. The publish leaves now.
    wire.session.handle(Input::Sent);
    let actions = poll_all(&mut wire.session);
    assert_eq!(ack_timers(&actions), []);
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::Send(_)))
    );

    // The watch starts only when the bytes of the publish left.
    wire.session.handle(Input::Sent);
    let timers = ack_timers(&poll_all(&mut wire.session));
    assert_eq!(timers.len(), 1);

    // The PUBACK arrives 1 s later, so the timer finds nothing that waits.
    let mut ack = Vec::new();
    codec::server::puback(&mut ack, 1);
    wire.session.handle(Input::Received(ack));
    wire.session.handle(Input::Timer(timers[0]));
    let actions = poll_all(&mut wire.session);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Ended(_))),
        "{actions:?}"
    );
    assert_eq!(wire.session.publish(command("next")), Ok(()));
}

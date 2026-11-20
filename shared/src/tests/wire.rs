//! A stream to the fake broker for the feature tests, which drive the core
//! one step at a time.
//!
//! A [`Wire`] answers only the requests of the byte stream: each `Write`
//! goes to the fake broker, and the answer of the broker goes back to the
//! core. Home Assistant does not do the service calls, so each change comes
//! from the test. The wire sends the events of the stream to the core at once. It
//! keeps the other effects and events for the test, in
//! [`Wire::effects`] and [`Wire::events`]. It ignores the timers of the MQTT
//! session. It keeps each `Log` notification in [`Wire::logs`].
//!
//! A test checks a `Write` in a command with the [`ExpectWrite`] helpers.
//! They skip the timers of the session, take the `Write`, answer it, and
//! leave the other effects of the command for the next helpers.

use crate::features::connection;
use alloc::{collections::VecDeque, format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};
use crux_time::{TimerId, operation::NotifyAfter};

use super::{broker::client_packets, broker::publish_of, shell::session_timers};
use crate::{
    Cmd, Dashboard, Effect, Event, Model,
    capabilities::stream::{Open, StreamItem, Write, Written},
    fake,
    mqtt::{
        Publish, Qos,
        codec::{self, Flags, server::Packet},
    },
};

/// The stream of one connection to the fake broker.
pub struct Wire {
    /// The fake broker.
    pub broker: fake::Broker,
    /// The `Open` request of the connection.
    open: Request<Open>,
    /// The commands that the wire keeps: the command of the stream first,
    /// then the commands of the updates that the wire did.
    commands: Vec<Cmd>,
    /// The ids of the timers of the session.
    session_timers: Vec<TimerId>,
    /// The bytes from the broker that wait for the core.
    incoming: Vec<u8>,
    /// The effects that are not for the stream, in order.
    pub effects: VecDeque<Effect>,
    /// The events that are not for the stream, in order.
    pub events: VecDeque<Event>,
    /// The packets that the core wrote, in order.
    pub packets: Vec<Packet>,
    /// Each `Log` notification as `{level} {area}: {message}`, in order.
    pub logs: Vec<String>,
}

impl Wire {
    /// Takes the command `cmd` that opens the stream. The command must have
    /// only the `Open` request and timers of the session.
    #[track_caller]
    pub fn new(mut cmd: Cmd) -> Self {
        let mut open = None;
        let mut session_timers = Vec::new();
        for effect in cmd.effects() {
            match effect {
                Effect::StreamOpen(request) => {
                    assert!(open.is_none(), "one Open request");
                    open = Some(request);
                }
                Effect::TimeNotifyAfter(request) if is_session_timer(&request) => {
                    session_timers.push(request.operation.id);
                }
                effect => panic!("an Open request, not {effect:?}"),
            }
        }
        // The test sends each change from Home Assistant itself.
        let mut broker = fake::Broker::default();
        broker.ignore_calls = true;
        broker.time = super::NOW;
        Self {
            broker,
            open: open.expect("an Open request"),
            commands: alloc::vec![cmd],
            session_timers,
            incoming: Vec::new(),
            effects: VecDeque::new(),
            events: VecDeque::new(),
            packets: Vec::new(),
            logs: Vec::new(),
        }
    }

    /// Returns the id of the connection of the stream.
    pub const fn connection(&self) -> u32 {
        self.open.operation.connection
    }

    /// Returns the host and the port of the stream.
    pub fn host(&self) -> (&str, u16) {
        (&self.open.operation.host, self.open.operation.port)
    }

    /// Opens the stream, and runs the core until the broker accepted the
    /// connection. Returns the command of the update for the CONNACK, which
    /// has the `Write` with the outbox, `online` and the subscribe.
    #[track_caller]
    pub fn connect(&mut self, model: &mut Model) -> Cmd {
        self.broker.open().expect("the broker is up");
        self.open.resolve(StreamItem::Opened).unwrap();
        let event = self.commands[0].expect_one_event();
        // The CONNECT packet goes to the broker, and the CONNACK waits.
        let mut cmd = Dashboard.update(event, model);
        for effect in cmd.effects() {
            self.handle(effect);
        }
        for event in cmd.events() {
            self.dispatch(event, model);
        }
        self.commands.push(cmd);
        let bytes = core::mem::take(&mut self.incoming);
        assert!(!bytes.is_empty(), "a CONNACK");
        self.open.resolve(StreamItem::Received(bytes)).unwrap();
        let event = self.commands[0].expect_one_event();
        Dashboard.update(event, model)
    }

    /// Publishes a message from another client, and gives it to the core.
    /// Returns the command of the update.
    #[track_caller]
    pub fn send(&mut self, model: &mut Model, topic: &str, payload: &str) -> Cmd {
        self.broker.publish(topic, payload.as_bytes(), false);
        let bytes = self.broker.take();
        assert!(!bytes.is_empty(), "the core subscribed to {topic}");
        self.open.resolve(StreamItem::Received(bytes)).unwrap();
        let event = self.commands[0].expect_one_event();
        Dashboard.update(event, model)
    }

    /// Gives the core a message on `topic` that no filter of the core
    /// matches, as a broker with a fault sends it. Returns the command of the
    /// update.
    #[track_caller]
    pub fn send_unsubscribed(&mut self, model: &mut Model, topic: &str, payload: &str) -> Cmd {
        let mut bytes = Vec::new();
        let flags = Flags {
            qos: Qos::AtMostOnce,
            retain: false,
        };
        codec::publish(&mut bytes, topic, payload.as_bytes(), flags, None).unwrap();
        self.open.resolve(StreamItem::Received(bytes)).unwrap();
        let event = self.commands[0].expect_one_event();
        Dashboard.update(event, model)
    }

    /// Ends the stream with `reason`, as when the network fails. The broker
    /// publishes the last will. Returns the command of the update.
    #[track_caller]
    pub fn drop_connection(&mut self, model: &mut Model, reason: &str) -> Cmd {
        self.broker.drop_connection();
        self.open.resolve(StreamItem::closed(reason)).unwrap();
        let event = self.commands[0].expect_one_event();
        Dashboard.update(event, model)
    }

    /// Takes the next event that is not for the stream.
    #[track_caller]
    pub fn take_event(&mut self) -> Event {
        self.events.pop_front().expect("an event")
    }

    /// Answers the effects of the stream in `cmd`, and keeps the command.
    /// Then runs the core. Refer to [`Wire::run`].
    pub fn settle(&mut self, cmd: Cmd, model: &mut Model) {
        self.commands.push(cmd);
        self.run(model);
    }

    /// Answers the requests of the stream in the kept commands, gives the
    /// bytes from the broker to the core, and sends the events of the
    /// stream to the core, until nothing changes.
    pub fn run(&mut self, model: &mut Model) {
        loop {
            let mut progress = false;
            let effects: Vec<Effect> = self
                .commands
                .iter_mut()
                .flat_map(|command| command.effects().collect::<Vec<_>>())
                .collect();
            for effect in effects {
                progress = true;
                self.handle(effect);
            }
            if !self.incoming.is_empty() {
                progress = true;
                let bytes = core::mem::take(&mut self.incoming);
                self.open.resolve(StreamItem::Received(bytes)).unwrap();
            }
            let events: Vec<Event> = self
                .commands
                .iter_mut()
                .flat_map(|command| command.events().collect::<Vec<_>>())
                .collect();
            for event in events {
                progress = true;
                self.dispatch(event, model);
            }
            if !progress {
                return;
            }
        }
    }

    /// Sends an event of the stream to the core, and keeps the command.
    /// Keeps another event for the test.
    fn dispatch(&mut self, event: Event, model: &mut Model) {
        match event {
            Event::Connection(
                connection::Event::Stream { .. }
                | connection::Event::Written { .. }
                | connection::Event::StreamClosed { .. }
                | connection::Event::SessionTimer { .. },
            ) => {
                let command = Dashboard.update(event, model);
                self.commands.push(command);
            }
            event => self.events.push_back(event),
        }
    }

    /// Answers a request of the stream, or keeps the effect for the test.
    fn handle(&mut self, effect: Effect) {
        match effect {
            Effect::StreamWrite(request) => self.write(request),
            Effect::StreamClose(mut request) => {
                if request.operation.connection == self.connection() {
                    self.broker.drop_connection();
                }
                request.resolve(()).unwrap();
            }
            Effect::TimeNotifyAfter(request) if is_session_timer(&request) => {
                self.session_timers.push(request.operation.id);
            }
            Effect::Log(request) => {
                let log = request.operation;
                self.logs.push(format!("{} {log}", log.level));
            }
            Effect::TimeClear(mut request)
                if self.session_timers.contains(&request.operation.id) =>
            {
                let id = request.operation.id;
                request.resolve(id).unwrap();
            }
            effect => self.effects.push_back(effect),
        }
    }

    /// Gives the bytes of `request` to the broker and answers `Sent`. The
    /// answer of the broker waits for the next run.
    fn write(&mut self, mut request: Request<Write>) {
        assert_eq!(request.operation.connection, self.connection());
        let bytes = core::mem::take(&mut request.operation.bytes);
        self.packets.extend(client_packets(&bytes));
        let answer = self.broker.receive(bytes);
        self.incoming.extend(answer);
        request.resolve(Written::Sent).unwrap();
    }
}

/// Returns `true` if `request` starts a timer of the MQTT session.
fn is_session_timer(request: &Request<NotifyAfter>) -> bool {
    session_timers().contains(&Duration::from(request.operation.duration))
}

/// The helpers that check a `Write` in a command.
pub trait ExpectWrite {
    /// Skips the timers of the session, takes the next effect, which must be
    /// a `Write`, and answers it through `wire`. Then runs the core. Calls
    /// `check` with the packets of the write.
    fn expect_write_with(
        &mut self,
        wire: &mut Wire,
        model: &mut Model,
        check: impl FnOnce(&[Packet]),
    ) -> &mut Self;

    /// Expects the next effect to clear a timer of the session, and answers
    /// it. The session clears its send timer when the shell sent the bytes
    /// of a `Write`.
    fn expect_session_clear(&mut self, wire: &Wire) -> &mut Self;

    /// Expects the next effect to clear a timer of the session, as
    /// [`ExpectWrite::expect_session_clear`] does, and then no other effect
    /// or event.
    fn expect_only_session_clear(&mut self, wire: &Wire);

    /// Expects a `Write` with one PUBLISH packet, and calls `check` with its
    /// message.
    #[track_caller]
    fn expect_publish_with(
        &mut self,
        wire: &mut Wire,
        model: &mut Model,
        check: impl FnOnce(&Publish),
    ) -> &mut Self {
        self.expect_write_with(wire, model, |packets| {
            let publishes: Vec<Publish> = packets.iter().filter_map(publish_of).collect();
            assert_eq!(publishes.len(), 1, "one publish: {packets:?}");
            check(&publishes[0]);
        })
    }

    /// Expects a `Write` with one PUBLISH packet.
    #[track_caller]
    fn expect_publish(&mut self, wire: &mut Wire, model: &mut Model) -> &mut Self {
        self.expect_publish_with(wire, model, |_| {})
    }

    /// Expects the `Write` of a sleep: the retained `asleep` status and then
    /// a DISCONNECT packet. After the write, the core closes the stream, and
    /// the wire keeps [`power::Event::Offline`] for the test.
    #[track_caller]
    fn expect_disconnect(&mut self, wire: &mut Wire, model: &mut Model) -> &mut Self {
        self.expect_write_with(wire, model, |packets| {
            let [publish, Packet::Disconnect] = packets else {
                panic!("asleep and DISCONNECT: {packets:?}");
            };
            let publish = publish_of(publish).expect("a publish");
            assert_eq!(publish.topic, "dashboard/hall/status");
            assert_eq!(publish.payload, b"asleep");
            assert!(publish.retain);
        })
    }
}

impl ExpectWrite for Cmd {
    #[track_caller]
    fn expect_only_session_clear(&mut self, wire: &Wire) {
        self.expect_session_clear(wire).expect_no_effect_or_events();
    }

    #[track_caller]
    fn expect_session_clear(&mut self, wire: &Wire) -> &mut Self {
        match self.expect_effect() {
            Effect::TimeClear(mut request)
                if wire.session_timers.contains(&request.operation.id) =>
            {
                let id = request.operation.id;
                request.resolve(id).unwrap();
            }
            effect => panic!("the clear of a session timer, not {effect:?}"),
        }
        self
    }

    #[track_caller]
    fn expect_write_with(
        &mut self,
        wire: &mut Wire,
        model: &mut Model,
        check: impl FnOnce(&[Packet]),
    ) -> &mut Self {
        let request = loop {
            match self.expect_effect() {
                Effect::TimeNotifyAfter(request) if is_session_timer(&request) => {
                    wire.session_timers.push(request.operation.id);
                }
                Effect::StreamWrite(request) => break request,
                effect => panic!("a Write, not {effect:?}"),
            }
        };
        let packets = client_packets(&request.operation.bytes);
        check(&packets);
        wire.write(request);
        for event in self.events() {
            wire.dispatch(event, model);
        }
        wire.run(model);
        self
    }
}

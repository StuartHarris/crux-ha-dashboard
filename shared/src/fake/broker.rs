//! The fake broker.
//!
//! The broker has one client and at most one connection. The caller opens the
//! connection with [`Broker::open`], gives the bytes from the client to
//! [`Broker::receive`], and gives the bytes that it returns to the client.
//! A message from another client, for example from Home Assistant, waits in
//! the broker until the next [`Broker::receive`] or [`Broker::take`].
//!
//! The topic filters and the last will belong to the connection, so a new
//! connection has none. The retained messages belong to the broker.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};

use crate::mqtt::{
    LastWill, MAX_PACKET, Publish, Qos,
    codec::{
        self, Flags,
        server::{self, Packet},
    },
    framer::{Frame, Framer},
};

/// Why the broker did not open a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoConnection {
    /// The broker host refused the connection with a TCP reset
    /// ([`Broker::down`]).
    Refused,
    /// The broker host did not answer ([`Broker::unreachable`]).
    Unreachable,
}

impl NoConnection {
    /// Returns the reason for the log.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Refused => "connection refused",
            Self::Unreachable => "no answer from the host",
        }
    }
}

/// The open connection of the client.
struct Connection {
    framer: Framer,
    /// The broker accepted the CONNECT packet.
    accepted: bool,
    /// The topic filters of the client.
    filters: Vec<String>,
    /// The last will from the CONNECT packet.
    last_will: Option<LastWill>,
}

/// The fake broker.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate rule of the fake broker"
)]
#[derive(Default)]
pub struct Broker {
    /// If `true`, the broker is down, and the broker host refuses
    /// connections with a TCP reset. This occurs when the broker process
    /// stops.
    pub down: bool,
    /// If `true`, the broker host does not answer, so no connection opens
    /// and no reset arrives. This occurs when the link moves no data. This
    /// flag wins over [`Broker::down`].
    pub unreachable: bool,
    /// The return code of the CONNACK packet. If it is not 0, the broker
    /// closes the connection after the CONNACK.
    pub code: u8,
    /// The session-present flag of the CONNACK packet.
    pub session_present: bool,
    /// The broker refuses this filter with the code 0x80.
    pub refuse: Option<String>,
    /// If `true`, the broker does not answer a SUBSCRIBE. It keeps the
    /// filters.
    pub hold_suback: bool,
    /// If `true`, the broker keeps the PUBACK of each publish in
    /// [`Broker::held_pubacks`].
    pub hold_puback: bool,
    /// If `true`, the broker does not answer a PINGREQ. It counts the
    /// PINGRESP packets that it held back in [`Broker::held_pingresps`].
    pub hold_pingresp: bool,
    /// The number of PINGRESP packets that the broker held back.
    pub held_pingresps: usize,
    /// The packet ids of the PUBACK packets that the broker held back.
    pub held_pubacks: Vec<u16>,
    /// The packets from the client, in order.
    pub packets: Vec<Packet>,
    /// The messages that the client published, in order.
    pub published: Vec<Publish>,
    /// The service calls that Home Assistant got, in order.
    pub service_calls: Vec<super::ServiceCall>,
    /// If `true`, Home Assistant keeps each service call in
    /// [`Broker::service_calls`], but does not do it.
    pub ignore_calls: bool,
    /// The number of connections that the broker accepted.
    pub connections: usize,
    /// The topics of the messages that the broker sent to the client.
    pub delivered: Vec<String>,
    /// The clock of Home Assistant, in seconds since the Unix epoch. Home
    /// Assistant publishes it as `last_updated` with each change. Each
    /// service call moves the clock one second forward.
    pub time: u64,
    pub(super) retained: BTreeMap<String, Vec<u8>>,
    connection: Option<Connection>,
    /// The bytes for the client that wait.
    outgoing: Vec<u8>,
}

impl Broker {
    /// Opens a connection from the client. A connection that is open closes
    /// with no last will, as when a new client takes over its session.
    ///
    /// # Errors
    ///
    /// Returns [`NoConnection::Unreachable`] if [`Broker::unreachable`] is
    /// `true`, and else [`NoConnection::Refused`] if [`Broker::down`] is
    /// `true`.
    pub fn open(&mut self) -> Result<(), NoConnection> {
        if self.unreachable {
            return Err(NoConnection::Unreachable);
        }
        if self.down {
            return Err(NoConnection::Refused);
        }
        self.connection = Some(Connection {
            framer: Framer::new(MAX_PACKET),
            accepted: false,
            filters: Vec::new(),
            last_will: None,
        });
        self.outgoing.clear();
        self.held_pingresps = 0;
        Ok(())
    }

    /// Returns `true` if a connection is open, also before its CONNECT
    /// packet.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.connection.is_some()
    }

    /// Returns `true` if the broker accepted the CONNECT packet of the open
    /// connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.accepted)
    }

    /// Returns the topic filters of the open connection.
    #[must_use]
    pub fn filters(&self) -> &[String] {
        self.connection
            .as_ref()
            .map_or(&[], |connection| &connection.filters)
    }

    /// Reads bytes from the client, and returns all the bytes for the client
    /// that wait. With no open connection, the broker ignores the bytes.
    ///
    /// # Panics
    ///
    /// Panics if the bytes are not valid packets of a client.
    pub fn receive(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let Some(connection) = &mut self.connection else {
            return Vec::new();
        };
        connection.framer.push(bytes);
        let mut packets = Vec::new();
        while let Some(frame) = connection
            .framer
            .next_packet()
            .expect("valid framing from the client")
        {
            let Frame::Packet { first, body } = frame else {
                panic!("an oversize packet from the client");
            };
            packets.push(server::decode(first, body).expect("a valid packet from the client"));
        }
        for packet in packets {
            self.answer(&packet);
            self.packets.push(packet);
        }
        self.take()
    }

    /// Returns the bytes for the client that wait.
    pub fn take(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outgoing)
    }

    /// Stops the connection without a DISCONNECT packet, as when the network
    /// fails or the client closes the stream. If the broker accepted the
    /// connection, it publishes the last will. The bytes that wait are lost.
    pub fn drop_connection(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        self.outgoing.clear();
        self.held_pingresps = 0;
        if connection.accepted
            && let Some(LastWill {
                topic,
                payload,
                retain,
            }) = connection.last_will
        {
            self.publish(&topic, &payload, retain);
        }
    }

    /// Keeps a retained message, as if another client published it. The
    /// broker sends nothing.
    pub fn retain(&mut self, topic: &str, payload: &[u8]) {
        self.retained.insert(topic.to_string(), payload.to_vec());
    }

    /// Returns the retained payload on `topic` as text.
    #[must_use]
    pub fn retained(&self, topic: &str) -> Option<&str> {
        self.retained
            .get(topic)
            .and_then(|payload| core::str::from_utf8(payload).ok())
    }

    /// Publishes a message from another client. If a filter of the client
    /// matches the topic, the message waits for the client. A retained
    /// message with an empty payload removes the retained message.
    ///
    /// # Panics
    ///
    /// Panics if the topic or the payload is too long for a packet.
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) {
        if retain {
            if payload.is_empty() {
                self.retained.remove(topic);
            } else {
                self.retain(topic, payload);
            }
        }
        if self.is_connected() && self.filters().iter().any(|filter| matches(filter, topic)) {
            let flags = Flags {
                qos: Qos::AtMostOnce,
                retain: false,
            };
            codec::publish(&mut self.outgoing, topic, payload, flags, None)
                .expect("a short publish");
            self.delivered.push(topic.to_string());
        }
    }

    /// Sends the PUBACK packets that the broker held back. They wait for the
    /// client.
    pub fn release_pubacks(&mut self) {
        for id in core::mem::take(&mut self.held_pubacks) {
            server::puback(&mut self.outgoing, id);
        }
    }

    /// Sends the PINGRESP packets that the broker held back. They wait for
    /// the client.
    pub fn release_pingresps(&mut self) {
        for _ in 0..core::mem::take(&mut self.held_pingresps) {
            server::pingresp(&mut self.outgoing);
        }
    }

    /// Takes a message from the client. The broker publishes it, and Home
    /// Assistant does the service call in it, if there is one.
    pub fn publish_from_client(&mut self, publish: Publish) {
        self.publish(&publish.topic, &publish.payload, publish.retain);
        self.service_call(&publish);
        self.published.push(publish);
    }

    fn answer(&mut self, packet: &Packet) {
        match packet {
            Packet::Connect {
                flags,
                will_topic,
                will_payload,
                ..
            } => self.connect(*flags, will_topic, will_payload),
            Packet::Publish {
                flags,
                topic,
                id,
                payload,
            } => {
                let qos = if flags & 0x06 == 0 {
                    Qos::AtMostOnce
                } else {
                    Qos::AtLeastOnce
                };
                self.publish_from_client(Publish {
                    topic: topic.clone(),
                    payload: payload.clone(),
                    qos,
                    retain: flags & 0x01 != 0,
                });
                if let Some(id) = *id {
                    if self.hold_puback {
                        self.held_pubacks.push(id);
                    } else {
                        server::puback(&mut self.outgoing, id);
                    }
                }
            }
            Packet::Subscribe { id, filters } => self.subscribe(*id, filters),
            Packet::PingReq if self.hold_pingresp => self.held_pingresps += 1,
            Packet::PingReq => server::pingresp(&mut self.outgoing),
            Packet::Disconnect => self.connection = None,
            Packet::PubAck(_) => {}
        }
    }

    /// Answers a CONNECT packet with [`Broker::code`]. Accepts the connection
    /// if the code is 0, and closes it if not.
    fn connect(&mut self, flags: u8, will_topic: &str, will_payload: &[u8]) {
        server::connack(&mut self.outgoing, self.session_present, self.code);
        if self.code != 0 {
            self.connection = None;
            return;
        }
        let Some(connection) = &mut self.connection else {
            return;
        };
        connection.accepted = true;
        connection.last_will = (flags & 0x04 != 0).then(|| LastWill {
            topic: will_topic.to_string(),
            payload: will_payload.to_vec(),
            retain: flags & 0x20 != 0,
        });
        self.connections += 1;
    }

    /// Adds the filters, answers with a SUBACK and then sends the retained
    /// messages that match, as a broker does.
    fn subscribe(&mut self, id: u16, filters: &[(String, u8)]) {
        let Some(connection) = &mut self.connection else {
            return;
        };
        let mut codes = Vec::new();
        for (filter, _qos) in filters {
            if self.refuse.as_ref() == Some(filter) {
                codes.push(codec::SUBACK_FAILURE);
            } else {
                codes.push(0);
                connection.filters.push(filter.clone());
            }
        }
        if !self.hold_suback {
            server::suback(&mut self.outgoing, id, &codes);
        }
        let flags = Flags {
            qos: Qos::AtMostOnce,
            retain: true,
        };
        for (filter, _qos) in filters {
            for (topic, payload) in &self.retained {
                if matches(filter, topic) {
                    codec::publish(&mut self.outgoing, topic, payload, flags, None)
                        .expect("a short publish");
                    self.delivered.push(topic.clone());
                }
            }
        }
    }
}

/// Returns `true` if the topic filter `filter` matches `topic`.
#[must_use]
pub fn matches(filter: &str, topic: &str) -> bool {
    let mut topic_levels = topic.split('/');
    for level in filter.split('/') {
        match level {
            "#" => return true,
            "+" => {
                if topic_levels.next().is_none() {
                    return false;
                }
            }
            level => {
                if topic_levels.next() != Some(level) {
                    return false;
                }
            }
        }
    }
    topic_levels.next().is_none()
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{Broker, NoConnection, matches};
    use crate::mqtt::{Connect, LastWill, codec};

    const STATUS: &str = "dashboard/hall/status";

    /// The bytes of a CONNECT packet with a retained last will.
    fn connect() -> Vec<u8> {
        let connect = Connect {
            host: "broker.local".into(),
            port: 1883,
            client_id: "dashboard-hall".into(),
            credentials: None,
            keep_alive_secs: 60,
            last_will: LastWill {
                topic: STATUS.into(),
                payload: b"offline".to_vec(),
                retain: true,
            },
        };
        let mut bytes = Vec::new();
        codec::connect(&mut bytes, &connect).unwrap();
        bytes
    }

    #[test]
    fn a_dropped_connection_publishes_the_last_will() {
        let mut broker = Broker::default();
        broker.open().unwrap();
        let _ = broker.receive(connect());
        assert_eq!(broker.connections, 1);
        broker.drop_connection();
        assert!(!broker.is_connected());
        assert_eq!(broker.retained(STATUS), Some("offline"));
    }

    #[test]
    fn a_disconnect_does_not_publish_the_last_will() {
        let mut broker = Broker::default();
        broker.open().unwrap();
        let _ = broker.receive(connect());
        let mut bytes = Vec::new();
        codec::disconnect(&mut bytes);
        let _ = broker.receive(bytes);
        assert!(!broker.is_connected());
        broker.drop_connection();
        assert_eq!(broker.retained(STATUS), None);
    }

    #[test]
    fn a_takeover_does_not_publish_the_last_will() {
        let mut broker = Broker::default();
        broker.open().unwrap();
        let _ = broker.receive(connect());
        broker.open().unwrap();
        assert!(!broker.is_connected());
        assert_eq!(broker.retained(STATUS), None);
    }

    #[test]
    fn a_refused_connection_closes_with_no_last_will() {
        let mut broker = Broker {
            code: 5,
            ..Broker::default()
        };
        broker.open().unwrap();
        assert_eq!(broker.receive(connect()), [0x20, 0x02, 0x00, 0x05]);
        assert!(!broker.is_connected());
        assert_eq!(broker.connections, 0);
        broker.drop_connection();
        assert_eq!(broker.retained(STATUS), None);
    }

    #[test]
    fn a_broker_that_is_down_refuses_to_open() {
        let mut broker = Broker {
            down: true,
            ..Broker::default()
        };
        assert_eq!(broker.open(), Err(NoConnection::Refused));
        broker.unreachable = true;
        assert_eq!(broker.open(), Err(NoConnection::Unreachable));
    }

    #[test]
    fn a_retained_message_with_no_payload_removes_the_retained_message() {
        let mut broker = Broker::default();
        broker.publish(STATUS, b"online", true);
        broker.publish(STATUS, b"", true);
        assert_eq!(broker.retained(STATUS), None);
    }

    #[test]
    fn topic_filters_match_as_in_mqtt() {
        assert!(matches("a/b/#", "a/b/c/d"));
        assert!(matches("a/b/#", "a/b"));
        assert!(matches("a/+/c", "a/b/c"));
        assert!(!matches("a/+/c", "a/b/d"));
        assert!(!matches("a/b", "a/b/c"));
        assert!(!matches("a/b/c", "a/b"));
    }
}

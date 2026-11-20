//! The fake broker of [`crate::fake`] behind the requests of the MQTT
//! capability.
//!
//! The adapter is the client half of the connection. It encodes each request
//! as packets for the fake broker, and gives the answers to the core through
//! the stream of the `Connect` request and the `Subscribe` requests. The
//! adapter is `Deref` to the fake broker, so a test reads the fake broker
//! and sets its rules through the adapter.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::ops::{Deref, DerefMut};

use crux_core::Request;

pub use crate::fake::iso_time;
use crate::{
    capabilities::mqtt::{MqttEvent, Subscribe},
    fake,
    mqtt::{
        Connect, MAX_PACKET, Message, Publish, SubscribeOutcome,
        codec::{self, Inbound},
        framer::{Frame, Framer},
    },
};

/// The open connection of the dashboard.
struct Connection {
    request: Request<Connect>,
    framer: Framer,
    /// The packet id of the next SUBSCRIBE packet.
    next_id: u16,
    /// The `Subscribe` requests that wait for a SUBACK, with the packet id
    /// of their SUBSCRIBE packet.
    subscribes: Vec<(u16, Request<Subscribe>)>,
}

/// The fake broker, with the requests of the open connection.
#[derive(Default)]
pub struct Broker {
    broker: fake::Broker,
    connection: Option<Connection>,
}

impl Deref for Broker {
    type Target = fake::Broker;

    fn deref(&self) -> &fake::Broker {
        &self.broker
    }
}

impl DerefMut for Broker {
    fn deref_mut(&mut self) -> &mut fake::Broker {
        &mut self.broker
    }
}

impl Broker {
    /// Opens a connection for the `Connect` request and sends the CONNECT
    /// packet. The stream gets `Connected` if the broker accepts the
    /// connection, and `Disconnected` if not.
    pub fn connect(&mut self, mut request: Request<Connect>) {
        if let Err(reason) = self.broker.open() {
            request
                .resolve(MqttEvent::Disconnected {
                    reason: Some(reason),
                })
                .expect("the core listens to the connection");
            return;
        }
        self.close(Some("session taken over".to_string()));
        let mut bytes = Vec::new();
        codec::connect(&mut bytes, &request.operation).expect("a short CONNECT");
        self.connection = Some(Connection {
            request,
            framer: Framer::new(MAX_PACKET),
            next_id: 1,
            subscribes: Vec::new(),
        });
        let answer = self.broker.receive(bytes);
        self.deliver(answer);
    }

    /// Sends a SUBSCRIBE packet for the request. The request ends when the
    /// SUBACK arrives. With no open connection, it ends with `Failed` at once.
    pub fn subscribe(&mut self, mut request: Request<Subscribe>) {
        let Some(connection) = &mut self.connection else {
            request
                .resolve(SubscribeOutcome::Failed)
                .expect("the core waits for the subscribe");
            return;
        };
        let id = connection.next_id;
        connection.next_id += 1;
        let mut bytes = Vec::new();
        codec::subscribe(&mut bytes, id, &request.operation.topics).expect("a short SUBSCRIBE");
        connection.subscribes.push((id, request));
        let answer = self.broker.receive(bytes);
        self.deliver(answer);
    }

    /// Takes a message from the dashboard.
    pub fn publish_from_dashboard(&mut self, publish: Publish) {
        self.broker.publish_from_client(publish);
        self.flush();
    }

    /// Closes the connection after a DISCONNECT packet from the dashboard.
    /// The broker does not publish the last will.
    pub fn disconnect(&mut self) {
        if self.connection.is_some() {
            let mut bytes = Vec::new();
            codec::disconnect(&mut bytes);
            let _ = self.broker.receive(bytes);
        }
        self.close(None);
    }

    /// Stops the connection without a disconnect, as when the network fails.
    /// The broker publishes the last will.
    pub fn drop_connection(&mut self) {
        if self.connection.is_none() {
            return;
        }
        self.broker.drop_connection();
        self.close(Some("connection lost".to_string()));
    }

    /// Publishes a message from another client.
    #[expect(
        dead_code,
        reason = "it hides the `publish` of the fake broker, which does not give the message to the core"
    )]
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) {
        self.broker.publish(topic, payload, retain);
        self.flush();
    }

    /// Publishes an entity state as Home Assistant does. Refer to
    /// [`fake::Broker::set_state`].
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        self.broker.set_state(entity_id, state, attributes);
        self.flush();
    }

    /// Gives the bytes that wait in the broker to the dashboard.
    fn flush(&mut self) {
        let bytes = self.broker.take();
        self.deliver(bytes);
    }

    /// Reads the packets in `bytes` from the broker. Sends each message on
    /// the stream of the connection, and then answers the `Subscribe`
    /// requests that the SUBACK packets acknowledge.
    fn deliver(&mut self, bytes: Vec<u8>) {
        let Some(connection) = &mut self.connection else {
            return;
        };
        connection.framer.push(bytes);
        let mut refused = false;
        let mut outcomes = Vec::new();
        while let Some(frame) = connection
            .framer
            .next_packet()
            .expect("valid framing from the broker")
        {
            let Frame::Packet { first, body } = frame else {
                panic!("an oversize packet from the broker");
            };
            match codec::decode(first, body).expect("a valid packet from the broker") {
                Inbound::ConnAck { code: 0, .. } => {
                    let _ = connection.request.resolve(MqttEvent::Connected);
                }
                Inbound::ConnAck { .. } => refused = true,
                Inbound::Publish { topic, payload, .. } => {
                    let message = Message {
                        topic: topic.to_string(),
                        payload: payload.to_vec(),
                    };
                    let _ = connection.request.resolve(MqttEvent::Message(message));
                }
                Inbound::SubAck { id, codes } => {
                    let outcome = if codes.contains(&codec::SUBACK_FAILURE) {
                        SubscribeOutcome::Failed
                    } else {
                        SubscribeOutcome::Done
                    };
                    outcomes.push((id, outcome));
                }
                Inbound::PubAck { .. } | Inbound::PingResp => {}
            }
        }
        for (id, outcome) in outcomes {
            if let Some(index) = connection
                .subscribes
                .iter()
                .position(|(sent, _)| *sent == id)
            {
                let (_, mut request) = connection.subscribes.remove(index);
                request
                    .resolve(outcome)
                    .expect("the core waits for the subscribe");
            }
        }
        if refused {
            self.close(Some("connection refused".to_string()));
        }
    }

    /// Ends the stream of the open connection, if there is one, with
    /// `reason`. The `Subscribe` requests that wait end with `Failed`.
    fn close(&mut self, reason: Option<String>) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        let _ = connection
            .request
            .resolve(MqttEvent::Disconnected { reason });
        for (_, mut subscribe) in connection.subscribes {
            let _ = subscribe.resolve(SubscribeOutcome::Failed);
        }
    }
}

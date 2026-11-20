//! The fake broker of [`crate::fake`] behind the stream requests of the
//! core.
//!
//! The adapter is the TCP stream between the core and the fake broker. It
//! gives the bytes of each `Write` request to the fake broker, and gives the
//! answers of the broker to the core through the `Open` request. The adapter
//! is `Deref` to the fake broker, so a test reads the fake broker and sets
//! its rules through the adapter.

use alloc::{string::ToString, vec::Vec};
use core::ops::{Deref, DerefMut};

use crux_core::Request;

pub use crate::fake::iso_time;
use crate::{
    capabilities::stream::{Close, Open, StreamItem, Write, Written},
    fake::{self, NoConnection},
    mqtt::{
        MAX_PACKET, Publish, Qos,
        codec::server::{self, Packet},
        framer::{Frame, Framer},
    },
};

/// Returns the packets from the client in `bytes`.
///
/// # Panics
///
/// Panics if the bytes are not whole, valid packets of a client.
pub fn client_packets(bytes: &[u8]) -> Vec<Packet> {
    let mut framer = Framer::new(MAX_PACKET);
    framer.push(bytes.to_vec());
    let mut packets = Vec::new();
    while let Some(frame) = framer.next_packet().expect("valid framing") {
        let Frame::Packet { first, body } = frame else {
            panic!("an oversize packet from the client");
        };
        packets.push(server::decode(first, body).expect("a valid packet"));
    }
    packets
}

/// Returns the message of a PUBLISH packet, or `None` for another packet.
pub fn publish_of(packet: &Packet) -> Option<Publish> {
    let Packet::Publish {
        flags,
        topic,
        payload,
        ..
    } = packet
    else {
        return None;
    };
    Some(Publish {
        topic: topic.clone(),
        payload: payload.clone(),
        qos: if flags & 0x06 == 0 {
            Qos::AtMostOnce
        } else {
            Qos::AtLeastOnce
        },
        retain: flags & 0x01 != 0,
    })
}

/// The open stream of the dashboard.
struct Stream {
    connection: u32,
    request: Request<Open>,
}

/// The fake broker, with the stream of the dashboard.
#[derive(Default)]
pub struct Broker {
    inner: fake::Broker,
    stream: Option<Stream>,
    /// If `true`, the stream does not send the bytes of a `Write`, as a
    /// link that is dead. The `Write` requests wait until the stream ends.
    pub hold_writes: bool,
    held: Vec<Request<Write>>,
    /// If `true`, the shell does not answer a `Close` request until
    /// [`Broker::release_closes`]. The stream closes at once.
    pub hold_closes: bool,
    held_closes: Vec<Request<Close>>,
    /// The answers of the broker to the writes, which wait for
    /// [`Broker::deliver_answers`].
    answers: Vec<u8>,
}

impl Deref for Broker {
    type Target = fake::Broker;

    fn deref(&self) -> &fake::Broker {
        &self.inner
    }
}

impl DerefMut for Broker {
    fn deref_mut(&mut self) -> &mut fake::Broker {
        &mut self.inner
    }
}

impl Broker {
    /// Opens a stream for the `Open` request. A stream that is open ends
    /// with `Closed` and the reason `"replaced"`. If the broker is down or
    /// the broker host is unreachable, the new stream ends at once. Only a broker
    /// that is down ends the stream by the host (a TCP reset).
    pub fn open(&mut self, mut request: Request<Open>) {
        self.end(StreamItem::closed("replaced"));
        if let Err(failure) = self.inner.open() {
            let item = match failure {
                NoConnection::Refused => StreamItem::by_host(failure.reason()),
                NoConnection::Unreachable => StreamItem::closed(failure.reason()),
            };
            request
                .resolve(item)
                .expect("the core listens to the stream");
            return;
        }
        request
            .resolve(StreamItem::Opened)
            .expect("the core listens to the stream");
        self.stream = Some(Stream {
            connection: request.operation.connection,
            request,
        });
    }

    /// Sends the bytes of the `Write` request to the broker, and answers the
    /// request. The answer of the broker waits for
    /// [`Broker::deliver_answers`]. Returns the packets of the request.
    ///
    /// For a request with no open stream, the answer is `Failed`.
    pub fn write(&mut self, mut request: Request<Write>) -> Vec<Packet> {
        let packets = client_packets(&request.operation.bytes);
        if !self.is_stream(request.operation.connection) {
            let failed = Written::Failed("no stream".to_string());
            request
                .resolve(failed)
                .expect("the core waits for the write");
            return packets;
        }
        if self.hold_writes {
            self.held.push(request);
            return packets;
        }
        let bytes = core::mem::take(&mut request.operation.bytes);
        let answer = self.inner.receive(bytes);
        self.answers.extend(answer);
        request
            .resolve(Written::Sent)
            .expect("the core waits for the write");
        packets
    }

    /// Gives the answers of the broker to the writes to the core. If the
    /// broker closed the connection, the stream then ends. Returns `true` if
    /// the core got something.
    pub fn deliver_answers(&mut self) -> bool {
        if self.stream.is_none() {
            self.answers.clear();
            return false;
        }
        let answers = core::mem::take(&mut self.answers);
        let delivered = !answers.is_empty();
        self.deliver(answers);
        if !self.inner.is_open() {
            self.end(StreamItem::by_host("closed by the broker"));
            return true;
        }
        delivered
    }

    /// Closes the stream for the `Close` request, and answers the request.
    /// If the broker did not get a DISCONNECT packet, it publishes the last
    /// will.
    pub fn close(&mut self, mut request: Request<Close>) {
        if self.is_stream(request.operation.connection) {
            self.inner.drop_connection();
            self.end(StreamItem::closed("closed"));
        }
        if self.hold_closes {
            self.held_closes.push(request);
            return;
        }
        request.resolve(()).expect("the core waits for the close");
    }

    /// Answers the `Close` requests that wait. Refer to
    /// [`Broker::hold_closes`].
    pub fn release_closes(&mut self) {
        self.hold_closes = false;
        for mut request in core::mem::take(&mut self.held_closes) {
            request.resolve(()).expect("the core waits for the close");
        }
    }

    /// Stops the stream, as when the network fails. The broker publishes the
    /// last will.
    pub fn drop_connection(&mut self) {
        if self.stream.is_none() {
            return;
        }
        self.inner.drop_connection();
        self.end(StreamItem::closed("connection lost"));
    }

    /// Stops the broker, as when its process stops. The broker host closes
    /// the open stream with a FIN, and then it refuses each connect
    /// ([`fake::Broker::down`]).
    pub fn stop(&mut self) {
        self.inner.down = true;
        if self.stream.is_none() {
            return;
        }
        self.inner.drop_connection();
        self.end(StreamItem::by_host("the broker closed the stream"));
    }

    /// Publishes a message from another client.
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) {
        self.inner.publish(topic, payload, retain);
        self.flush();
    }

    /// Publishes an entity state as Home Assistant does. Refer to
    /// [`fake::Broker::set_state`].
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        self.inner.set_state(entity_id, state, attributes);
        self.flush();
    }

    /// Sends the bytes of the `Write` requests that wait to the broker, and
    /// answers them. Refer to [`Broker::hold_writes`].
    pub fn release_writes(&mut self) {
        self.hold_writes = false;
        for mut request in core::mem::take(&mut self.held) {
            let bytes = core::mem::take(&mut request.operation.bytes);
            let answer = self.inner.receive(bytes);
            self.answers.extend(answer);
            let _ = request.resolve(Written::Sent);
        }
    }

    /// Gives the bytes that wait in the broker to the core.
    pub fn flush(&mut self) {
        let bytes = self.inner.take();
        self.deliver(bytes);
    }

    /// Returns `true` if the open stream has the id `connection`.
    fn is_stream(&self, connection: u32) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.connection == connection)
    }

    /// Gives `bytes` to the core on the open stream.
    fn deliver(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(stream) = &mut self.stream {
            let _ = stream.request.resolve(StreamItem::Received(bytes));
        }
    }

    /// Ends the open stream with `item`, which is a `Closed` item. The
    /// `Write` requests that wait get `Failed` first.
    fn end(&mut self, item: StreamItem) {
        self.answers.clear();
        let StreamItem::Closed { reason, .. } = &item else {
            panic!("a Closed item: {item:?}");
        };
        for mut write in self.held.drain(..) {
            let _ = write.resolve(Written::Failed(reason.clone()));
        }
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.request.resolve(item);
        }
    }
}

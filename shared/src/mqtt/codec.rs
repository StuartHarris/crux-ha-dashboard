//! The encoders and the decoder for the MQTT 3.1.1 packets that the client
//! uses.
//!
//! Each encoder appends one whole packet to a buffer. The decoder reads one
//! packet from the broker, from the first byte of its fixed header and its
//! body. The [`framer`](super::framer) finds the packets in the byte stream.

use alloc::vec::Vec;

use crate::capabilities::mqtt::{Connect, Qos};

/// The protocol level of MQTT 3.1.1.
pub const PROTOCOL_LEVEL: u8 = 4;

/// The largest value of the remaining length that the encoding can hold.
pub const MAX_REMAINING_LENGTH: usize = 268_435_455;

/// The return code in a SUBACK for a topic filter that the broker refused.
pub const SUBACK_FAILURE: u8 = 0x80;

/// The packet type of CONNECT, in the high four bits of the first byte.
pub const CONNECT: u8 = 1;
/// The packet type of CONNACK.
pub const CONNACK: u8 = 2;
/// The packet type of PUBLISH.
pub const PUBLISH: u8 = 3;
/// The packet type of PUBACK.
pub const PUBACK: u8 = 4;
/// The packet type of SUBSCRIBE.
pub const SUBSCRIBE: u8 = 8;
/// The packet type of SUBACK.
pub const SUBACK: u8 = 9;
/// The packet type of PINGREQ.
pub const PINGREQ: u8 = 12;
/// The packet type of PINGRESP.
pub const PINGRESP: u8 = 13;
/// The packet type of DISCONNECT.
pub const DISCONNECT: u8 = 14;

/// The bytes are not a valid MQTT 3.1.1 packet of the subset that the client
/// uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

/// A field is too long for its length prefix, or the packet is too long for
/// the remaining length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong;

/// The flags of a PUBLISH packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags {
    /// The quality of service.
    pub qos: Qos,
    /// If `true`, the broker keeps the message for new subscribers.
    pub retain: bool,
}

/// A packet from the broker. A PUBLISH borrows its topic and payload from the
/// body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inbound<'a> {
    /// The answer to the CONNECT packet.
    ConnAck {
        /// If `true`, the broker kept a session from an earlier connection.
        session_present: bool,
        /// The return code. 0 is success.
        code: u8,
    },
    /// A message on a subscribed topic.
    Publish {
        /// The topic of the message.
        topic: &'a str,
        /// The payload of the message.
        payload: &'a [u8],
        /// The packet id, if the quality of service is above 0.
        id: Option<u16>,
    },
    /// The acknowledgement of a publish at `QoS` 1.
    PubAck {
        /// The packet id of the publish.
        id: u16,
    },
    /// The acknowledgement of a SUBSCRIBE packet.
    SubAck {
        /// The packet id of the SUBSCRIBE packet.
        id: u16,
        /// One return code for each topic filter, in order.
        codes: &'a [u8],
    },
    /// The answer to a PINGREQ packet.
    PingResp,
}

/// Appends the remaining length `length` in its variable-length encoding.
///
/// # Errors
///
/// Returns [`TooLong`] if `length` is more than [`MAX_REMAINING_LENGTH`].
pub fn encode_length(out: &mut Vec<u8>, length: usize) -> Result<(), TooLong> {
    if length > MAX_REMAINING_LENGTH {
        return Err(TooLong);
    }
    let mut rest = length;
    loop {
        let byte = rest.to_le_bytes()[0] & 0x7F;
        rest >>= 7;
        if rest == 0 {
            out.push(byte);
            return Ok(());
        }
        out.push(byte | 0x80);
    }
}

/// Reads a remaining length from the start of `bytes`.
///
/// Returns the length and the number of bytes that hold it, or `None` if
/// `bytes` does not hold all of it yet.
///
/// # Errors
///
/// Returns [`Malformed`] if the encoding uses more than four bytes.
pub fn decode_length(bytes: &[u8]) -> Result<Option<(usize, usize)>, Malformed> {
    let mut length = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if index == 4 {
            return Err(Malformed);
        }
        length |= usize::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(Some((length, index + 1)));
        }
    }
    if bytes.len() >= 4 {
        return Err(Malformed);
    }
    Ok(None)
}

/// Appends a 16-bit length and then `bytes`.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), TooLong> {
    let length = u16::try_from(bytes.len()).map_err(|_| TooLong)?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Appends the fixed header and the body as one packet. The packet is not
/// appended if it is too long.
fn put_packet(out: &mut Vec<u8>, first: u8, body: &[u8]) -> Result<(), TooLong> {
    let mut header = Vec::with_capacity(5);
    header.push(first);
    encode_length(&mut header, body.len())?;
    out.reserve(header.len() + body.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    Ok(())
}

/// Appends a CONNECT packet with a clean session.
///
/// The last will has `QoS` 1. The packet has a user name and a password only
/// if `connect` has credentials.
///
/// # Errors
///
/// Returns [`TooLong`] if a field does not fit in its 16-bit length, and
/// appends nothing.
pub fn connect(out: &mut Vec<u8>, connect: &Connect) -> Result<(), TooLong> {
    // Clean session, a last will, and the will at QoS 1.
    let mut flags = 0x02 | 0x04 | 0x08;
    if connect.last_will.retain {
        flags |= 0x20;
    }
    if connect.credentials.is_some() {
        flags |= 0x80 | 0x40;
    }
    let mut body = Vec::new();
    put_bytes(&mut body, b"MQTT")?;
    body.push(PROTOCOL_LEVEL);
    body.push(flags);
    body.extend_from_slice(&connect.keep_alive_secs.to_be_bytes());
    put_bytes(&mut body, connect.client_id.as_bytes())?;
    put_bytes(&mut body, connect.last_will.topic.as_bytes())?;
    put_bytes(&mut body, &connect.last_will.payload)?;
    if let Some(credentials) = &connect.credentials {
        put_bytes(&mut body, credentials.username.as_bytes())?;
        put_bytes(&mut body, credentials.password.as_bytes())?;
    }
    put_packet(out, CONNECT << 4, &body)
}

/// Appends a PUBLISH packet. `id` is the packet id, which a publish at
/// `QoS` 1 must have and a publish at `QoS` 0 must not have.
///
/// # Errors
///
/// Returns [`TooLong`] if the topic does not fit in its 16-bit length, or if
/// the packet is too long. Appends nothing then.
pub fn publish(
    out: &mut Vec<u8>,
    topic: &str,
    payload: &[u8],
    flags: Flags,
    id: Option<u16>,
) -> Result<(), TooLong> {
    let mut first = PUBLISH << 4;
    if flags.qos == Qos::AtLeastOnce {
        first |= 0x02;
    }
    if flags.retain {
        first |= 0x01;
    }
    let mut body = Vec::with_capacity(2 + topic.len() + 2 + payload.len());
    put_bytes(&mut body, topic.as_bytes())?;
    if let Some(id) = id {
        body.extend_from_slice(&id.to_be_bytes());
    }
    body.extend_from_slice(payload);
    put_packet(out, first, &body)
}

/// Appends a SUBSCRIBE packet that asks for `QoS` 0 on each filter.
///
/// # Errors
///
/// Returns [`TooLong`] if a filter does not fit in its 16-bit length, or if
/// the packet is too long. Appends nothing then.
pub fn subscribe<S: AsRef<str>>(out: &mut Vec<u8>, id: u16, filters: &[S]) -> Result<(), TooLong> {
    let mut body = Vec::new();
    body.extend_from_slice(&id.to_be_bytes());
    for filter in filters {
        put_bytes(&mut body, filter.as_ref().as_bytes())?;
        body.push(0);
    }
    put_packet(out, (SUBSCRIBE << 4) | 0x02, &body)
}

/// Returns the remaining length that a topic filter adds to a SUBSCRIBE
/// packet: the 16-bit length, the filter and the `QoS` byte.
#[must_use]
pub const fn subscribe_entry_length(filter: &str) -> usize {
    2 + filter.len() + 1
}

/// Appends a PUBACK packet for the packet id `id`.
pub fn puback(out: &mut Vec<u8>, id: u16) {
    let [high, low] = id.to_be_bytes();
    out.extend_from_slice(&[PUBACK << 4, 2, high, low]);
}

/// Appends a PINGREQ packet.
pub fn pingreq(out: &mut Vec<u8>) {
    out.extend_from_slice(&[PINGREQ << 4, 0]);
}

/// Appends a DISCONNECT packet.
pub fn disconnect(out: &mut Vec<u8>) {
    out.extend_from_slice(&[DISCONNECT << 4, 0]);
}

/// Reads a 16-bit value from the start of `bytes`, and returns it with the
/// bytes after it.
const fn take_u16(bytes: &[u8]) -> Result<(u16, &[u8]), Malformed> {
    match bytes {
        [high, low, rest @ ..] => Ok((u16::from_be_bytes([*high, *low]), rest)),
        _ => Err(Malformed),
    }
}

/// Reads a string with a 16-bit length from the start of `bytes`, and
/// returns it with the bytes after it.
fn take_str(bytes: &[u8]) -> Result<(&str, &[u8]), Malformed> {
    let (length, rest) = take_u16(bytes)?;
    let length = usize::from(length);
    if rest.len() < length {
        return Err(Malformed);
    }
    let (text, rest) = rest.split_at(length);
    let text = core::str::from_utf8(text).map_err(|_| Malformed)?;
    Ok((text, rest))
}

/// Reads a packet from the broker. `first` is the first byte of the fixed
/// header, and `body` is the variable header and the payload.
///
/// # Errors
///
/// Returns [`Malformed`] if the packet breaks the MQTT 3.1.1 rules, or if
/// the client does not use its type. A PUBLISH at `QoS` 2 or 3, a topic that
/// is not UTF-8 and reserved flags that are not 0 are examples.
pub fn decode(first: u8, body: &[u8]) -> Result<Inbound<'_>, Malformed> {
    let flags = first & 0x0F;
    match first >> 4 {
        CONNACK => match body {
            [acknowledge, code] if flags == 0 && acknowledge & 0xFE == 0 => Ok(Inbound::ConnAck {
                session_present: acknowledge & 0x01 == 1,
                code: *code,
            }),
            _ => Err(Malformed),
        },
        PUBLISH => {
            // The client subscribes at QoS 0, so a broker that follows the
            // rules sends no PUBLISH at QoS 2. The client cannot do the QoS 2
            // flow. QoS 3 does not exist.
            let qos = (flags >> 1) & 0x03;
            if qos >= 2 {
                return Err(Malformed);
            }
            let (topic, rest) = take_str(body)?;
            if topic.is_empty() || topic.contains(['+', '#']) {
                return Err(Malformed);
            }
            let (id, payload) = if qos == 0 {
                (None, rest)
            } else {
                let (id, payload) = take_u16(rest)?;
                if id == 0 {
                    return Err(Malformed);
                }
                (Some(id), payload)
            };
            Ok(Inbound::Publish { topic, payload, id })
        }
        PUBACK if flags == 0 && body.len() == 2 => Ok(Inbound::PubAck {
            id: take_u16(body)?.0,
        }),
        SUBACK if flags == 0 => {
            let (id, codes) = take_u16(body)?;
            let valid = |code: &u8| matches!(*code, 0..=2 | SUBACK_FAILURE);
            if codes.is_empty() || !codes.iter().all(valid) {
                return Err(Malformed);
            }
            Ok(Inbound::SubAck { id, codes })
        }
        PINGRESP if flags == 0 && body.is_empty() => Ok(Inbound::PingResp),
        _ => Err(Malformed),
    }
}

/// The broker side of the codec, for the fake broker in the tests.
#[cfg(test)]
pub mod server {
    use alloc::{string::String, vec::Vec};

    use super::{
        CONNACK, CONNECT, DISCONNECT, Malformed, PINGREQ, PINGRESP, PUBACK, PUBLISH, SUBACK,
        SUBSCRIBE, put_packet, take_str, take_u16,
    };

    /// A packet from the client.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Packet {
        /// A CONNECT packet.
        Connect {
            /// The flags byte.
            flags: u8,
            /// The keep-alive interval in seconds.
            keep_alive: u16,
            /// The client id.
            client_id: String,
            /// The topic of the last will.
            will_topic: String,
            /// The payload of the last will.
            will_payload: Vec<u8>,
            /// The user name, if there is one.
            username: Option<String>,
            /// The password, if there is one.
            password: Option<Vec<u8>>,
        },
        /// A PUBLISH packet.
        Publish {
            /// The low four bits of the first byte.
            flags: u8,
            /// The topic.
            topic: String,
            /// The packet id, if the `QoS` is above 0.
            id: Option<u16>,
            /// The payload.
            payload: Vec<u8>,
        },
        /// A PUBACK packet.
        PubAck(u16),
        /// A SUBSCRIBE packet with its topic filters and their `QoS` values.
        Subscribe {
            /// The packet id.
            id: u16,
            /// The topic filters and the `QoS` that the client asks for.
            filters: Vec<(String, u8)>,
        },
        /// A PINGREQ packet.
        PingReq,
        /// A DISCONNECT packet.
        Disconnect,
    }

    /// Reads a packet from the client.
    ///
    /// # Errors
    ///
    /// Returns [`Malformed`] if the packet is not one that the client sends.
    pub fn decode(first: u8, body: &[u8]) -> Result<Packet, Malformed> {
        let flags = first & 0x0F;
        match first >> 4 {
            CONNECT if flags == 0 => {
                let (name, rest) = take_str(body)?;
                let [level, connect_flags, high, low, rest @ ..] = rest else {
                    return Err(Malformed);
                };
                if name != "MQTT" || *level != super::PROTOCOL_LEVEL {
                    return Err(Malformed);
                }
                let (client_id, rest) = take_str(rest)?;
                let (will_topic, rest) = take_str(rest)?;
                let (length, rest) = take_u16(rest)?;
                let (will_payload, mut rest) = rest
                    .split_at_checked(usize::from(length))
                    .ok_or(Malformed)?;
                let mut username = None;
                if connect_flags & 0x80 != 0 {
                    let (name, after) = take_str(rest)?;
                    username = Some(name.into());
                    rest = after;
                }
                let mut password = None;
                if connect_flags & 0x40 != 0 {
                    let (length, after) = take_u16(rest)?;
                    let (bytes, after) = after
                        .split_at_checked(usize::from(length))
                        .ok_or(Malformed)?;
                    password = Some(bytes.to_vec());
                    rest = after;
                }
                if !rest.is_empty() {
                    return Err(Malformed);
                }
                Ok(Packet::Connect {
                    flags: *connect_flags,
                    keep_alive: u16::from_be_bytes([*high, *low]),
                    client_id: client_id.into(),
                    will_topic: will_topic.into(),
                    will_payload: will_payload.to_vec(),
                    username,
                    password,
                })
            }
            PUBLISH => {
                let (topic, rest) = take_str(body)?;
                let (id, payload) = if flags & 0x06 == 0 {
                    (None, rest)
                } else {
                    let (id, payload) = take_u16(rest)?;
                    (Some(id), payload)
                };
                Ok(Packet::Publish {
                    flags,
                    topic: topic.into(),
                    id,
                    payload: payload.to_vec(),
                })
            }
            PUBACK if flags == 0 && body.len() == 2 => Ok(Packet::PubAck(take_u16(body)?.0)),
            SUBSCRIBE if flags == 2 => {
                let (id, mut rest) = take_u16(body)?;
                let mut filters = Vec::new();
                while !rest.is_empty() {
                    let (filter, after) = take_str(rest)?;
                    let [qos, after @ ..] = after else {
                        return Err(Malformed);
                    };
                    filters.push((filter.into(), *qos));
                    rest = after;
                }
                Ok(Packet::Subscribe { id, filters })
            }
            PINGREQ if flags == 0 && body.is_empty() => Ok(Packet::PingReq),
            DISCONNECT if flags == 0 && body.is_empty() => Ok(Packet::Disconnect),
            _ => Err(Malformed),
        }
    }

    /// Appends a CONNACK packet.
    pub fn connack(out: &mut Vec<u8>, session_present: bool, code: u8) {
        out.extend_from_slice(&[CONNACK << 4, 2, u8::from(session_present), code]);
    }

    /// Appends a PUBACK packet.
    pub fn puback(out: &mut Vec<u8>, id: u16) {
        super::puback(out, id);
    }

    /// Appends a SUBACK packet with one return code for each filter.
    ///
    /// # Panics
    ///
    /// Panics if the packet is longer than the remaining length can hold.
    pub fn suback(out: &mut Vec<u8>, id: u16, codes: &[u8]) {
        let mut body = id.to_be_bytes().to_vec();
        body.extend_from_slice(codes);
        put_packet(out, SUBACK << 4, &body).expect("a short SUBACK");
    }

    /// Appends a PINGRESP packet.
    pub fn pingresp(out: &mut Vec<u8>) {
        out.extend_from_slice(&[PINGRESP << 4, 0]);
    }
}

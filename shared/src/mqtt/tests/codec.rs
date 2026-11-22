//! Tests of the codec against the bytes in the MQTT 3.1.1 specification.

use alloc::{vec, vec::Vec};

use crate::mqtt::{
    Connect, Qos,
    codec::{
        self, Flags, Inbound, MAX_REMAINING_LENGTH, Malformed, TooLong, decode, decode_length,
        encode_length,
        server::{self, Packet},
    },
};

/// The examples of the remaining length in section 2.2.3 of the
/// specification: the smallest and the largest value for each number of
/// bytes.
const LENGTHS: [(usize, &[u8]); 7] = [
    (0, &[0x00]),
    (127, &[0x7F]),
    (128, &[0x80, 0x01]),
    (16_383, &[0xFF, 0x7F]),
    (16_384, &[0x80, 0x80, 0x01]),
    (2_097_152, &[0x80, 0x80, 0x80, 0x01]),
    (268_435_455, &[0xFF, 0xFF, 0xFF, 0x7F]),
];

#[test]
fn remaining_length_encodes_as_in_the_specification() {
    for (length, bytes) in LENGTHS {
        let mut out = Vec::new();
        encode_length(&mut out, length).unwrap();
        assert_eq!(out, bytes, "{length}");
    }
}

#[test]
fn remaining_length_decodes_as_in_the_specification() {
    for (length, bytes) in LENGTHS {
        assert_eq!(
            decode_length(bytes),
            Ok(Some((length, bytes.len()))),
            "{length}"
        );
    }
}

#[test]
fn a_remaining_length_above_the_maximum_does_not_encode() {
    let mut out = Vec::new();
    assert_eq!(
        encode_length(&mut out, MAX_REMAINING_LENGTH + 1),
        Err(TooLong)
    );
    assert_eq!(out, [] as [u8; 0]);
}

#[test]
fn a_fifth_length_byte_is_malformed() {
    assert_eq!(
        decode_length(&[0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
        Err(Malformed)
    );
    // The fourth byte says that a fifth byte follows.
    assert_eq!(decode_length(&[0xFF, 0xFF, 0xFF, 0xFF]), Err(Malformed));
}

#[test]
fn a_remaining_length_that_is_not_complete_waits() {
    assert_eq!(decode_length(&[]), Ok(None));
    assert_eq!(decode_length(&[0x80]), Ok(None));
    assert_eq!(decode_length(&[0xFF, 0xFF, 0xFF]), Ok(None));
}

#[test]
fn pingreq_and_disconnect_are_two_bytes() {
    let mut out = Vec::new();
    codec::pingreq(&mut out);
    codec::disconnect(&mut out);
    assert_eq!(out, [0xC0, 0x00, 0xE0, 0x00]);
}

#[test]
fn puback_has_the_packet_id() {
    let mut out = Vec::new();
    codec::puback(&mut out, 10);
    assert_eq!(out, [0x40, 0x02, 0x00, 0x0A]);
}

/// A `Connect` with one-letter fields, so the golden bytes are short.
fn short_connect(credentials: bool, retain: bool) -> Connect {
    let mut connect = super::connect();
    connect.client_id = "c".into();
    connect.last_will.topic = "w".into();
    connect.last_will.payload = b"o".to_vec();
    connect.last_will.retain = retain;
    connect.credentials = credentials.then(|| crate::mqtt::Credentials {
        username: "u".into(),
        password: "p".into(),
    });
    connect
}

#[test]
fn connect_with_credentials_has_the_flags_0xee() {
    let mut out = Vec::new();
    codec::connect(&mut out, &short_connect(true, true)).unwrap();
    #[rustfmt::skip]
    let expected = [
        0x10, 0x19,
        // The protocol name, the level 4, the flags and the keep-alive 60.
        0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0xEE, 0x00, 0x3C,
        // The client id, the will topic, the will message, the user name and
        // the password, in this order.
        0x00, 0x01, b'c',
        0x00, 0x01, b'w',
        0x00, 0x01, b'o',
        0x00, 0x01, b'u',
        0x00, 0x01, b'p',
    ];
    assert_eq!(out, expected);
}

#[test]
fn connect_without_credentials_has_no_user_name_or_password() {
    let mut out = Vec::new();
    codec::connect(&mut out, &short_connect(false, true)).unwrap();
    #[rustfmt::skip]
    let expected = [
        0x10, 0x13,
        0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x2E, 0x00, 0x3C,
        0x00, 0x01, b'c',
        0x00, 0x01, b'w',
        0x00, 0x01, b'o',
    ];
    assert_eq!(out, expected);
}

#[test]
fn a_will_that_is_not_retained_clears_the_will_retain_flag() {
    let mut out = Vec::new();
    codec::connect(&mut out, &short_connect(false, false)).unwrap();
    assert_eq!(out[9], 0x0E);
}

#[test]
fn a_connect_field_that_is_too_long_does_not_encode() {
    let mut connect = super::connect();
    connect.client_id = "x".repeat(usize::from(u16::MAX) + 1);
    let mut out = Vec::new();
    assert_eq!(codec::connect(&mut out, &connect), Err(TooLong));
    assert_eq!(out, [] as [u8; 0]);
}

#[test]
fn publish_sets_the_qos_and_retain_flags() {
    let cases = [
        (Qos::AtMostOnce, false, None, vec![0x30, 0x07]),
        (Qos::AtMostOnce, true, None, vec![0x31, 0x07]),
        (Qos::AtLeastOnce, false, Some(10), vec![0x32, 0x09]),
        (Qos::AtLeastOnce, true, Some(10), vec![0x33, 0x09]),
    ];
    for (qos, retain, id, header) in cases {
        let mut out = Vec::new();
        let flags = Flags { qos, retain };
        codec::publish(&mut out, "a/b", b"hi", flags, id).unwrap();
        let mut expected = header;
        expected.extend_from_slice(&[0x00, 0x03, b'a', b'/', b'b']);
        if let Some(id) = id {
            expected.extend_from_slice(&id.to_be_bytes());
        }
        expected.extend_from_slice(b"hi");
        assert_eq!(out, expected, "{qos:?} retain {retain}");
    }
}

#[test]
fn a_publish_with_a_long_payload_has_a_two_byte_length() {
    let mut out = Vec::new();
    let flags = Flags {
        qos: Qos::AtMostOnce,
        retain: false,
    };
    codec::publish(&mut out, "t", &[7; 200], flags, None).unwrap();
    // 3 bytes of topic and 200 bytes of payload is 203: 0xCB 0x01.
    assert_eq!(out[..5], [0x30, 0xCB, 0x01, 0x00, 0x01]);
    assert_eq!(out.len(), 3 + 203);
}

#[test]
fn subscribe_asks_for_qos_0_on_each_filter() {
    let mut out = Vec::new();
    codec::subscribe(&mut out, 1, &["a/b", "c"]).unwrap();
    #[rustfmt::skip]
    let expected = [
        0x82, 0x0C,
        0x00, 0x01,
        0x00, 0x03, b'a', b'/', b'b', 0x00,
        0x00, 0x01, b'c', 0x00,
    ];
    assert_eq!(out, expected);
}

#[test]
fn connack_decodes() {
    assert_eq!(
        decode(0x20, &[0x00, 0x00]),
        Ok(Inbound::ConnAck {
            session_present: false,
            code: 0
        })
    );
    assert_eq!(
        decode(0x20, &[0x01, 0x05]),
        Ok(Inbound::ConnAck {
            session_present: true,
            code: 5
        })
    );
}

#[test]
fn connack_with_reserved_bits_or_a_wrong_length_is_malformed() {
    // Reserved bits in the acknowledge flags.
    assert_eq!(decode(0x20, &[0x02, 0x00]), Err(Malformed));
    // Reserved flags in the fixed header.
    assert_eq!(decode(0x21, &[0x00, 0x00]), Err(Malformed));
    // A remaining length that is not 2.
    assert_eq!(decode(0x20, &[0x00]), Err(Malformed));
    assert_eq!(decode(0x20, &[0x00, 0x00, 0x00]), Err(Malformed));
}

#[test]
fn publish_decodes_with_and_without_a_packet_id() {
    let body = [0x00, 0x03, b'a', b'/', b'b', b'h', b'i'];
    assert_eq!(
        decode(0x30, &body),
        Ok(Inbound::Publish {
            topic: "a/b",
            payload: b"hi",
            id: None,
            retain: false,
        })
    );
    // The DUP flag does not change the decode.
    assert_eq!(decode(0x38, &body), decode(0x30, &body));

    let body = [0x00, 0x03, b'a', b'/', b'b', 0x00, 0x0A, b'h', b'i'];
    assert_eq!(
        decode(0x32, &body),
        Ok(Inbound::Publish {
            topic: "a/b",
            payload: b"hi",
            id: Some(10),
            retain: false,
        })
    );
}

#[test]
fn publish_round_trips_through_the_codec() {
    for (qos, id) in [(Qos::AtMostOnce, None), (Qos::AtLeastOnce, Some(65_535))] {
        let mut out = Vec::new();
        let flags = Flags { qos, retain: true };
        codec::publish(&mut out, "home/°C", b"20.5", flags, id).unwrap();
        let body = &out[2..];
        assert_eq!(
            decode(out[0], body),
            Ok(Inbound::Publish {
                topic: "home/°C",
                payload: b"20.5",
                id,
                retain: true,
            })
        );
    }
}

#[test]
fn publish_decodes_the_retain_flag() {
    let qos_0 = [0x00, 0x03, b'a', b'/', b'b', b'h', b'i'];
    let qos_1 = [0x00, 0x03, b'a', b'/', b'b', 0x00, 0x0A, b'h', b'i'];
    let cases: [(u8, &[u8], bool); 6] = [
        (0x30, &qos_0, false),
        (0x31, &qos_0, true),
        (0x32, &qos_1, false),
        (0x33, &qos_1, true),
        // The DUP flag does not change the retain flag.
        (0x38, &qos_0, false),
        (0x39, &qos_0, true),
    ];
    for (first, body, expected) in cases {
        let Ok(Inbound::Publish { retain, .. }) = decode(first, body) else {
            panic!("a publish: {first:#x}");
        };
        assert_eq!(retain, expected, "{first:#x}");
    }
}

#[test]
fn connect_round_trips_through_the_codec() {
    let connect = super::connect();
    let mut out = Vec::new();
    codec::connect(&mut out, &connect).unwrap();
    let packets = super::wire::client_packets(&out);
    let credentials = connect.credentials.unwrap();
    assert_eq!(
        packets,
        [Packet::Connect {
            flags: 0xEE,
            keep_alive: 60,
            client_id: connect.client_id,
            will_topic: connect.last_will.topic,
            will_payload: connect.last_will.payload,
            username: Some(credentials.username),
            password: Some(credentials.password.into_bytes()),
        }]
    );
}

#[test]
fn publish_at_qos_2_or_3_is_malformed() {
    let body = [0x00, 0x01, b'a', 0x00, 0x01];
    assert_eq!(decode(0x34, &body), Err(Malformed));
    assert_eq!(decode(0x36, &body), Err(Malformed));
}

#[test]
fn a_topic_that_is_not_utf8_is_malformed() {
    assert_eq!(decode(0x30, &[0x00, 0x02, 0xFF, 0xFE]), Err(Malformed));
}

#[test]
fn a_topic_that_is_empty_has_a_wildcard_or_is_cut_is_malformed() {
    assert_eq!(decode(0x30, &[0x00, 0x00, b'x']), Err(Malformed));
    assert_eq!(decode(0x30, &[0x00, 0x01, b'#']), Err(Malformed));
    assert_eq!(decode(0x30, &[0x00, 0x01, b'+']), Err(Malformed));
    assert_eq!(decode(0x30, &[0x00, 0x05, b'a']), Err(Malformed));
    assert_eq!(decode(0x30, &[0x00]), Err(Malformed));
}

#[test]
fn a_publish_at_qos_1_with_the_packet_id_0_is_malformed() {
    assert_eq!(
        decode(0x32, &[0x00, 0x01, b'a', 0x00, 0x00]),
        Err(Malformed)
    );
}

#[test]
fn puback_suback_and_pingresp_decode() {
    assert_eq!(decode(0x40, &[0x00, 0x0A]), Ok(Inbound::PubAck { id: 10 }));
    assert_eq!(
        decode(0x90, &[0x00, 0x01, 0x00, 0x80]),
        Ok(Inbound::SubAck {
            id: 1,
            codes: &[0x00, 0x80]
        })
    );
    assert_eq!(decode(0xD0, &[]), Ok(Inbound::PingResp));
}

#[test]
fn acknowledgements_with_a_wrong_form_are_malformed() {
    assert_eq!(decode(0x40, &[0x00]), Err(Malformed));
    assert_eq!(decode(0x41, &[0x00, 0x01]), Err(Malformed));
    // A SUBACK with no return code, or with the code 3.
    assert_eq!(decode(0x90, &[0x00, 0x01]), Err(Malformed));
    assert_eq!(decode(0x90, &[0x00, 0x01, 0x03]), Err(Malformed));
    assert_eq!(decode(0xD0, &[0x00]), Err(Malformed));
}

#[test]
fn packet_types_that_the_client_does_not_use_are_malformed() {
    // CONNECT, PUBREC, PUBREL, PUBCOMP, SUBSCRIBE, UNSUBACK, PINGREQ,
    // DISCONNECT and the reserved types 0 and 15.
    for first in [0x10, 0x50, 0x62, 0x70, 0x82, 0xB0, 0xC0, 0xE0, 0x00, 0xF0] {
        assert_eq!(decode(first, &[0x00, 0x01]), Err(Malformed), "{first:#x}");
    }
}

#[test]
fn the_server_side_encoders_give_the_specification_bytes() {
    let mut out = Vec::new();
    server::connack(&mut out, false, 0);
    server::puback(&mut out, 10);
    server::suback(&mut out, 1, &[0x00, 0x80]);
    server::pingresp(&mut out);
    #[rustfmt::skip]
    let expected = [
        0x20, 0x02, 0x00, 0x00,
        0x40, 0x02, 0x00, 0x0A,
        0x90, 0x04, 0x00, 0x01, 0x00, 0x80,
        0xD0, 0x00,
    ];
    assert_eq!(out, expected);
}

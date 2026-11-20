//! Tests of the framer: the same byte stream in chunks of different sizes
//! gives the same packets.

use alloc::{string::String, vec::Vec};

use crate::{
    entities::topics,
    mqtt::{
        Qos,
        codec::{self, Flags, Malformed, server},
        framer::{Frame, Framer},
    },
    pages::PAGES,
};

/// A packet that the framer gave, with an owned body.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Owned {
    Packet(u8, Vec<u8>),
    Oversize(u8, usize),
}

/// Takes all the packets that `framer` holds.
fn drain(framer: &mut Framer) -> Vec<Owned> {
    let mut packets = Vec::new();
    while let Some(frame) = framer.next_packet().expect("valid framing") {
        packets.push(match frame {
            Frame::Packet { first, body } => Owned::Packet(first, body.to_vec()),
            Frame::Oversize { first, length } => Owned::Oversize(first, length),
        });
    }
    packets
}

/// Gives `chunks` to a new framer one at a time, and returns the packets.
fn frame(chunks: impl IntoIterator<Item = Vec<u8>>, max_packet: usize) -> Vec<Owned> {
    let mut framer = Framer::new(max_packet);
    let mut packets = Vec::new();
    for chunk in chunks {
        framer.push(chunk);
        packets.extend(drain(&mut framer));
    }
    packets
}

/// A publish at `QoS` 0 with no retain flag.
fn publish(out: &mut Vec<u8>, topic: &str, payload: &[u8]) {
    let flags = Flags {
        qos: Qos::AtMostOnce,
        retain: false,
    };
    codec::publish(out, topic, payload, flags, None).unwrap();
}

/// A short stream with packets of each kind, and a publish with a two-byte
/// remaining length.
fn mixed_stream() -> Vec<u8> {
    let mut out = Vec::new();
    server::connack(&mut out, false, 0);
    server::suback(&mut out, 1, &[0, 0, 0]);
    publish(&mut out, "a/b", b"on");
    server::pingresp(&mut out);
    publish(&mut out, "c/d/e", &[b'x'; 200]);
    server::puback(&mut out, 7);
    publish(&mut out, "f", b"");
    out
}

/// The packets in [`mixed_stream`], as the framer gives them whole.
fn mixed_packets() -> Vec<Owned> {
    let packets = frame([mixed_stream()], 1024);
    assert_eq!(packets.len(), 7);
    packets
}

#[test]
fn a_whole_stream_gives_each_packet() {
    let packets = mixed_packets();
    assert_eq!(packets[0], Owned::Packet(0x20, alloc::vec![0, 0]));
    assert_eq!(packets[3], Owned::Packet(0xD0, Vec::new()));
    let Owned::Packet(0x30, body) = &packets[4] else {
        panic!("a publish: {:?}", packets[4]);
    };
    assert_eq!(body.len(), 2 + 5 + 200);
}

#[test]
fn a_stream_byte_by_byte_gives_the_same_packets() {
    let chunks = mixed_stream().into_iter().map(|byte| alloc::vec![byte]);
    assert_eq!(frame(chunks, 1024), mixed_packets());
}

#[test]
fn a_stream_cut_at_each_offset_gives_the_same_packets() {
    let stream = mixed_stream();
    for cut in 0..=stream.len() {
        let (first, second) = stream.split_at(cut);
        let packets = frame([first.to_vec(), second.to_vec()], 1024);
        assert_eq!(packets, mixed_packets(), "cut at {cut}");
    }
}

#[test]
fn a_remaining_length_split_across_chunks_waits_for_its_second_byte() {
    let mut stream = Vec::new();
    publish(&mut stream, "t", &[1; 300]);
    let mut framer = Framer::new(1024);
    // The fixed header and the first byte of the remaining length.
    framer.push(stream[..2].to_vec());
    assert_eq!(framer.next_packet(), Ok(None));
    framer.push(stream[2..3].to_vec());
    assert_eq!(framer.next_packet(), Ok(None));
    framer.push(stream[3..].to_vec());
    assert_eq!(drain(&mut framer).len(), 1);
}

#[test]
fn the_retained_flood_in_tcp_segments_gives_each_message_in_order() {
    let topics = topics(PAGES);
    let mut stream = Vec::new();
    for topic in &topics {
        publish(&mut stream, topic, topic.as_bytes());
    }
    let chunks = stream.chunks(1460).map(<[u8]>::to_vec);
    let packets = frame(chunks, 1024);
    let received: Vec<String> = packets
        .iter()
        .map(|packet| {
            let Owned::Packet(first, body) = packet else {
                panic!("a publish: {packet:?}");
            };
            let codec::Inbound::Publish { topic, payload, .. } =
                codec::decode(*first, body).expect("a valid publish")
            else {
                panic!("a publish");
            };
            assert_eq!(topic.as_bytes(), payload);
            String::from(topic)
        })
        .collect();
    assert_eq!(received, topics);
}

#[test]
fn the_stream_in_random_chunks_gives_the_same_packets() {
    let stream = mixed_stream();
    for seed in 1..50 {
        let chunks = super::random_chunks(&stream, seed, 40);
        assert_eq!(frame(chunks, 1024), mixed_packets(), "seed {seed}");
    }
}

#[test]
fn an_oversize_packet_is_skipped_across_chunks() {
    let mut stream = Vec::new();
    publish(&mut stream, "big", &[9; 100]);
    server::pingresp(&mut stream);
    publish(&mut stream, "small", b"ok");
    // The body of the big packet is 105 bytes, and the limit is 32.
    for cut in [1, 2, 3, 50, 107, 108] {
        let mut chunks: Vec<Vec<u8>> = stream.chunks(cut).map(<[u8]>::to_vec).collect();
        chunks.push(Vec::new());
        let packets = frame(chunks, 32);
        assert_eq!(packets.len(), 3, "chunks of {cut}: {packets:?}");
        assert_eq!(packets[0], Owned::Oversize(0x30, 105), "chunks of {cut}");
        assert_eq!(
            packets[1],
            Owned::Packet(0xD0, Vec::new()),
            "chunks of {cut}"
        );
    }
}

#[test]
fn a_push_into_an_empty_framer_moves_the_chunk() {
    let mut chunk = Vec::with_capacity(64);
    server::pingresp(&mut chunk);
    let (pointer, capacity) = (chunk.as_ptr(), chunk.capacity());
    let mut framer = Framer::new(1024);
    framer.push(chunk);
    assert_eq!(framer.buffer().as_ptr(), pointer);
    assert_eq!(framer.buffer().capacity(), capacity);

    // After the framer gave all its packets, it is empty again.
    assert_eq!(drain(&mut framer).len(), 1);
    let mut chunk = Vec::with_capacity(64);
    server::pingresp(&mut chunk);
    let pointer = chunk.as_ptr();
    framer.push(chunk);
    assert_eq!(framer.buffer().as_ptr(), pointer);
}

#[test]
fn a_fifth_length_byte_is_malformed() {
    let mut framer = Framer::new(1024);
    framer.push(alloc::vec![0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    assert_eq!(framer.next_packet(), Err(Malformed));
}

#[test]
fn an_oversize_packet_that_is_not_a_publish_is_malformed() {
    let mut stream = Vec::new();
    server::suback(&mut stream, 1, &[0; 40]);
    let mut framer = Framer::new(32);
    framer.push(stream);
    assert_eq!(framer.next_packet(), Err(Malformed));
}

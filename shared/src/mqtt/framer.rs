//! The framer finds MQTT packets in the byte stream from the broker.
//!
//! The shell gives the bytes in chunks of any size. A chunk can hold part of
//! a packet, one packet or many packets. The framer keeps the bytes that do
//! not make a whole packet yet, and gives each whole packet as a fixed-header
//! byte and a body.

use alloc::vec::Vec;

use super::codec::{Malformed, PUBLISH, decode_length};

/// A packet that the framer found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A whole packet.
    Packet {
        /// The first byte of the fixed header.
        first: u8,
        /// The variable header and the payload.
        body: &'a [u8],
    },
    /// A PUBLISH packet whose body is longer than the limit of the framer.
    /// The framer skips its body as the body arrives.
    Oversize {
        /// The first byte of the fixed header.
        first: u8,
        /// The length of the body.
        length: usize,
    },
}

/// Finds packets in the byte stream.
#[derive(Debug)]
pub struct Framer {
    /// The bytes that the framer did not give as packets yet start at
    /// `start`.
    buffer: Vec<u8>,
    start: usize,
    /// The number of bytes of an oversize packet that did not arrive yet.
    skip: usize,
    /// The longest body that the framer gives as a packet.
    max_packet: usize,
}

impl Framer {
    /// Makes a framer that gives packets with a body of at most `max_packet`
    /// bytes.
    #[must_use]
    pub const fn new(max_packet: usize) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            skip: 0,
            max_packet,
        }
    }

    /// Adds a chunk of the byte stream.
    ///
    /// If the framer holds no bytes, it keeps `chunk` and does not copy it.
    /// Otherwise, it removes the bytes that it gave as packets, and appends a
    /// copy of `chunk`.
    pub fn push(&mut self, chunk: Vec<u8>) {
        // The bytes of an oversize packet come first.
        let start = self.skip.min(chunk.len());
        self.skip -= start;
        if start == chunk.len() {
            return;
        }
        if self.start == self.buffer.len() {
            self.buffer = chunk;
            self.start = start;
        } else {
            self.buffer.drain(..self.start);
            self.start = 0;
            self.buffer.extend_from_slice(&chunk[start..]);
        }
    }

    /// Returns the next packet, or `None` if the framer does not hold a whole
    /// packet.
    ///
    /// # Errors
    ///
    /// Returns [`Malformed`] if the remaining length uses more than four
    /// bytes, or if a packet that is not a PUBLISH is longer than the limit.
    /// The stream cannot continue after this error.
    pub fn next_packet(&mut self) -> Result<Option<Frame<'_>>, Malformed> {
        if self.skip > 0 {
            return Ok(None);
        }
        let held = &self.buffer[self.start..];
        let Some((&first, rest)) = held.split_first() else {
            return Ok(None);
        };
        let Some((length, used)) = decode_length(rest)? else {
            return Ok(None);
        };
        let header = 1 + used;
        if length > self.max_packet {
            if first >> 4 != PUBLISH {
                return Err(Malformed);
            }
            let arrived = (held.len() - header).min(length);
            self.skip = length - arrived;
            self.start += header + arrived;
            return Ok(Some(Frame::Oversize { first, length }));
        }
        if held.len() < header + length {
            return Ok(None);
        }
        let body_start = self.start + header;
        self.start = body_start + length;
        Ok(Some(Frame::Packet {
            first,
            body: &self.buffer[body_start..self.start],
        }))
    }

    /// Returns the buffer, so a test can check that a push did not copy.
    #[cfg(test)]
    #[must_use]
    pub const fn buffer(&self) -> &Vec<u8> {
        &self.buffer
    }
}

//! Tests of the MQTT client.
//!
//! The codec tests compare the bytes with the examples and the rules of the
//! MQTT 3.1.1 specification. The framer tests give the same byte stream in
//! chunks of different sizes. The session tests run the session against
//! [`wire::WireBroker`], a fake broker that reads and writes bytes.

mod codec;
mod framer;
mod session;
mod wire;

use crate::capabilities::mqtt::{Connect, Credentials, LastWill};

/// The password in [`connect`]. No `Debug` output can show it.
const PASSWORD: &str = "secret-password";

/// A `Connect` with credentials, a keep-alive of 60 seconds and a retained
/// last will.
fn connect() -> Connect {
    Connect {
        host: "broker.local".into(),
        port: 1883,
        client_id: "dashboard-hall".into(),
        credentials: Some(Credentials {
            username: "dashboard".into(),
            password: PASSWORD.into(),
        }),
        keep_alive_secs: 60,
        last_will: LastWill {
            topic: "dashboard/hall/status".into(),
            payload: b"offline".to_vec(),
            retain: true,
        },
    }
}

/// A small random number generator (xorshift), so the tests can cut a
/// stream at random places and give the same cuts each time.
struct Rng(u64);

impl Rng {
    /// Returns a number from 1 to `max`.
    fn below(&mut self, max: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let max = u64::try_from(max).expect("a small number");
        usize::try_from(self.0 % max).expect("a small number") + 1
    }
}

/// Cuts `bytes` into chunks of 1 to `max` bytes at random places.
fn random_chunks(bytes: &[u8], seed: u64, max: usize) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
    let mut rng = Rng(seed);
    let mut chunks = alloc::vec::Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let size = rng.below(max).min(rest.len());
        let (chunk, after) = rest.split_at(size);
        chunks.push(chunk.to_vec());
        rest = after;
    }
    chunks
}

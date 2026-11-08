//! The SNTP task: it sets the clock from the SNTP server of the
//! configuration (`[device] ntp_server`).
//!
//! `logic::clock::SyncSchedule` decides when to send a request: when the
//! device first has a network address, then each 12 hours, with a backoff
//! after a failure. When a request is due, the task waits for the link and
//! a network address. It does not turn Wi-Fi on. If the network goes down
//! during a request, the request is not a failure. The task waits for the
//! network and then sends the request again.
//!
//! While it waits for the reply, the task ignores a datagram that is not
//! from the server address and port 123, and a datagram that is not the
//! answer to its request (`sntp::Rejected::ends_request`).
//!
//! After a good reply, the task sets the clock and asks the display task,
//! which owns the I2C bus, to write the RTC (`RTC_WRITE`).

use alloc::{format, string::String};

use embassy_futures::select::{Either, select};
use embassy_net::{
    IpEndpoint, Stack,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Instant, Timer, with_deadline};
use logic::clock::{Outcome, SyncSchedule, sntp, start_from_sntp};

use crate::{RTC_WRITE, clock, config::CONFIG, mqtt::resolve};

/// The longest time to wait for the reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// The size of the receive buffer. A reply with extensions is longer than
/// 48 bytes.
const RX_BUFFER: usize = 256;

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>) {
    let mut schedule = SyncSchedule::default();
    loop {
        Timer::at(Instant::from_millis(schedule.next_ms())).await;
        stack.wait_link_up().await;
        stack.wait_config_up().await;
        let result = select(sync(stack), network_down(stack)).await;
        // The backoff counts from the end of the request.
        let now_ms = Instant::now().as_millis();
        let network_up = stack.is_link_up() && stack.is_config_up();
        let (outcome, reason) = match result {
            Either::First(Ok(start)) => {
                clock::set(start, clock::Source::Sntp);
                RTC_WRITE.signal(());
                (Outcome::Synced, None)
            }
            Either::First(Err(reason)) => (Outcome::of(false, network_up), Some(reason)),
            Either::Second(()) => (Outcome::NetworkDown, None),
        };
        match (schedule.record(outcome, now_ms), reason) {
            (None, _) => {
                log::info!("sntp: the network is down. The request waits for the network.");
            }
            (Some(wait), None) => log::info!("sntp: next request in {} h", wait / 3_600_000),
            (Some(wait), Some(reason)) => {
                log::warn!("sntp: {reason}. Next try in {} s.", wait / 1_000);
            }
        }
    }
}

/// Waits until the link or the network address goes down.
async fn network_down(stack: Stack<'static>) {
    select(stack.wait_link_down(), stack.wait_config_down()).await;
}

/// Sends one request to the server, and returns the time of the start from
/// the reply.
async fn sync(stack: Stack<'static>) -> Result<u32, String> {
    let address = resolve(stack, CONFIG.ntp_server).await?;
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0; RX_BUFFER];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buffer = [0; sntp::PACKET_LEN];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    // Port 0 lets the stack choose a free local port.
    socket
        .bind(0)
        .map_err(|error| format!("bind failed: {error:?}"))?;

    let nonce = nonce();
    socket
        .send_to(&sntp::request(nonce), (address, sntp::PORT))
        .await
        .map_err(|error| format!("send to {} failed: {error:?}", CONFIG.ntp_server))?;

    let server = IpEndpoint::new(address, sntp::PORT);
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut reply = [0; RX_BUFFER];
    loop {
        let received = with_deadline(deadline, socket.recv_from(&mut reply))
            .await
            .map_err(|_| format!("no reply from {}", CONFIG.ntp_server))?;
        let uptime = clock::uptime_micros();
        let (length, metadata) = match received {
            Ok(received) => received,
            // For example, a datagram that is longer than the buffer.
            Err(error) => {
                log::debug!("sntp: a receive failed: {error:?}");
                continue;
            }
        };
        if metadata.endpoint != server {
            log::debug!("sntp: a datagram from {}", metadata.endpoint);
            continue;
        }
        match sntp::parse(&reply[..length], nonce) {
            Ok(time) => {
                return start_from_sntp(time, uptime)
                    .ok_or_else(|| format!("the server time {} s is not valid", time.seconds));
            }
            Err(rejected) if rejected.ends_request() => {
                return Err(format!("the reply is not valid: {rejected:?}"));
            }
            Err(rejected) => log::debug!("sntp: a datagram that is not the reply: {rejected:?}"),
        }
    }
}

/// Returns a random nonce for the transmit timestamp of the request.
fn nonce() -> u64 {
    let rng = esp_hal::rng::Rng::new();
    (u64::from(rng.random()) << 32) | u64::from(rng.random())
}

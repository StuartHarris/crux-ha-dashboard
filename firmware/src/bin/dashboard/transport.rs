//! The transport of the MQTT client: the TCP socket, with a `flush` that does
//! not wait for the TCP ACK of the broker.
//!
//! `rust-mqtt` flushes the transport after each packet that it sends, also
//! after each automatic PUBACK. The `flush` of the embassy-net `TcpSocket`
//! waits until the broker acknowledged all the sent data. With Wi-Fi power
//! save, each ACK takes about 50 ms, so 115 subscribe packets took about
//! 6.5 s.
//!
//! [`Transport::flush`] returns at once if the socket can send. A `write` of
//! the socket returns when the data is in the send buffer of the socket, and
//! smoltcp sends it from there. So a packet is sent when `flush` returns,
//! but the broker can get it later.
//!
//! A send does not show a dead link now, so the task finds it in these
//! other ways:
//!
//! - A `write` waits while the send buffer is full. The task gives each send
//!   a time limit, so a send that cannot get buffer space ends the
//!   connection.
//! - [`Link::watch`] (`logic::mqtt::AckWatch`) gets the state of the send
//!   buffer before and after each write, and when the task asks. If data
//!   waits and no new data gets an ACK for `ACK_LIMIT`, the task ends the
//!   connection.
//! - The task checks the silence of the broker before each ping. The TCP
//!   timeout of the socket also closes it after a long silence.
//!
//! The socket and the watch are in a `RefCell`, because the client owns the
//! transport, and the task must read the send buffer between the calls to
//! the client. No borrow lasts across an `await`. Each poll borrows the
//! socket again, which is correct because the futures of the socket keep no
//! state between polls.

use core::{cell::RefCell, future::poll_fn, pin::pin, time::Duration};

use embassy_net::tcp::{Error, TcpSocket};
use embassy_time::Instant;
use embedded_io_async::{ErrorType, Read, Write};
use logic::mqtt::AckWatch;

/// The TCP socket of a connection and the watch of its send buffer.
pub struct Link<'a> {
    /// The socket.
    pub socket: TcpSocket<'a>,
    /// The watch of the data that waits for the TCP ACK.
    pub watch: AckWatch,
    /// The number of bytes that the transport wrote to the socket.
    pub written: u64,
}

impl Link<'_> {
    /// Gives the state of the send buffer now to the watch. Returns the
    /// time at which the task must check again, or `None` if no data waits.
    pub fn observe(&mut self) -> Option<Instant> {
        self.watch
            .observe(now(), self.written, self.socket.send_queue())
            .map(|deadline| Instant::from_micros(micros(deadline)))
    }

    /// Returns `true` if sent data waited too long for its ACK.
    pub fn ack_expired(&self) -> bool {
        self.watch.expired(now())
    }
}

/// Returns the time since the start, for the watch.
fn now() -> Duration {
    Duration::from_micros(Instant::now().as_micros())
}

/// Returns `duration` in microseconds, for an `Instant`.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// The transport that the MQTT client owns.
pub struct Transport<'s, 'a> {
    link: &'s RefCell<Link<'a>>,
}

impl<'s, 'a> Transport<'s, 'a> {
    /// Makes the transport of `link`.
    pub const fn new(link: &'s RefCell<Link<'a>>) -> Self {
        Self { link }
    }
}

impl ErrorType for Transport<'_, '_> {
    type Error = Error;
}

impl Read for Transport<'_, '_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        poll_fn(|cx| {
            let mut link = self.link.borrow_mut();
            pin!(link.socket.read(buf)).poll(cx)
        })
        .await
    }
}

impl Write for Transport<'_, '_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        // A report before the write shows the ACKs since the last report.
        self.link.borrow_mut().observe();
        let written = poll_fn(|cx| {
            let mut link = self.link.borrow_mut();
            pin!(link.socket.write(buf)).poll(cx)
        })
        .await?;
        let mut link = self.link.borrow_mut();
        link.written += u64::try_from(written).unwrap_or(u64::MAX);
        link.observe();
        Ok(written)
    }

    /// Returns at once. The data is in the send buffer, and smoltcp sends it
    /// from there. If the socket cannot send, returns an error.
    async fn flush(&mut self) -> Result<(), Self::Error> {
        if self.link.borrow().socket.may_send() {
            Ok(())
        } else {
            Err(Error::ConnectionReset)
        }
    }
}

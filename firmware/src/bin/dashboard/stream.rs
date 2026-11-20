//! The stream task: it opens one TCP stream to the broker at a time, and it
//! moves bytes between the stream and the core. The core runs the MQTT
//! session, so this task knows nothing of MQTT.
//!
//! `logic::stream::Streams` makes sure that each stream ends with exactly one
//! `Closed` item. When the task takes an `Open`, it first takes all the
//! queued jobs, and `logic::stream::supersede` removes the streams that the
//! core no longer wants.
//!
//! The task never waits without a limit. While it opens a stream and while
//! the stream is open, it also listens for its jobs:
//!
//! - The DNS query has a limit of [`DNS_TIMEOUT`], and the TCP connect has a
//!   limit of [`CONNECT_TIMEOUT`]. A newer `Open` and a `Close` stop each of
//!   them.
//! - While the stream is open, one select waits for the next job, the next
//!   bytes from the broker and the write of the oldest `Write` job. Each of
//!   these futures is safe to cancel. A `Write` that gets no space in the
//!   send buffer for [`WRITE_STALL`] ends the stream.
//! - A `Close` sends a TCP FIN, waits for no more than [`CLOSE_FLUSH`] for the
//!   broker to acknowledge the bytes that wait, and then aborts the socket.
//!   The task then waits for no more than [`ABORT_FLUSH`] for the RST. It
//!   takes no jobs during the close. A stream that did not open, failed or
//!   got a FIN from the broker gets only the abort.
//!
//! A `Write` is sent when all its bytes are in the send buffer of the socket.
//! smoltcp sends them from there. The core finds a link that stopped with
//! its own timers, and closes the stream.
//!
//! The core decides the health of the link from the items of the stream, so
//! the task only writes each end to the log. A socket error, a reset from
//! the broker, a write that stalls and a stream that does not open are
//! warnings.
//!
//! The task sets `reset` in the `Closed` item only for a TCP reset from the
//! broker host. embassy-net gives the error `ConnectionReset` for a reset
//! from the host, and also when smoltcp closes the socket because the
//! network stack lost its address (the link dropped). For that reason the
//! task counts the error as a reset only while the link is up and the stack
//! has an address. Refer to [`host_reset`]. The socket has no smoltcp
//! timeout and no keep-alive, so no timer of the socket gives the error.
//!
//! The receive and send buffers of the socket are 8 KiB each. The task takes
//! them from the heap once and keeps them, so the allocator puts them in
//! PSRAM. The SUBSCRIBE packets for all the topics are about 8 KiB, and the
//! retained states arrive in a burst after them.

use alloc::{boxed::Box, collections::VecDeque, format, string::String, vec, vec::Vec};
use core::{net::Ipv4Addr, str::FromStr};

use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::{
    IpAddress, Stack,
    dns::DnsQueryType,
    tcp::{ConnectError, Error, TcpSocket},
};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use logic::{
    shell::StreamOp,
    stream::{CLOSED, REPLACED, Streams, supersede},
};
use shared::capabilities::stream::{Open, Written};

use crate::{SIGNALS, STREAM};

/// The size of each buffer of the socket.
const BUFFER: usize = 8 * 1024;
/// The largest chunk of bytes that one `Received` item holds, and the size of
/// the read buffer of the task.
///
/// Each read makes one `Received` item, and each item is one update of the
/// core. After a connect, the broker sends about 115 retained messages in a
/// burst of more than 10 KiB. A chunk of 4 KiB needs about a quarter of the
/// updates that a chunk of 1 KiB needs. The read buffer is part of the
/// static state of the task, so it takes RAM all the time. For that reason
/// the chunk is half of [`BUFFER`] and not the whole of it. An item copies
/// only the bytes that the read returned, so a larger chunk adds no heap.
const READ_CHUNK: usize = 4 * 1024;
/// The longest time for the DNS query of the host.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest time to open the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest time for which a `Write` can get no space in the send buffer.
const WRITE_STALL: Duration = Duration::from_secs(10);
/// The longest time for which a `Close` waits for the broker to acknowledge
/// the bytes that wait and the FIN.
const CLOSE_FLUSH: Duration = Duration::from_secs(2);
/// The longest time for which the task waits for the RST after an abort.
const ABORT_FLUSH: Duration = Duration::from_millis(500);

/// Jobs that the task took from its queue but did not do yet.
type Pending = VecDeque<(u32, StreamOp)>;

/// Why a stream ended.
struct End {
    /// The reason for the `Closed` item.
    reason: String,
    /// `true` if the link or the broker did not work. The task then writes
    /// the end to the log as a warning.
    failed: bool,
    /// `true` if the broker host answered with a TCP reset. Refer to
    /// [`host_reset`].
    reset: bool,
    /// `true` if the task sends a FIN and waits for the bytes that wait
    /// before it aborts the socket.
    graceful: bool,
}

impl End {
    /// A stream that failed. The task aborts the socket at once.
    fn error(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            failed: true,
            reset: false,
            graceful: false,
        }
    }

    /// A stream that failed with the error `ConnectionReset` from
    /// embassy-net. The end is a reset only if the host sent it. Refer to
    /// [`host_reset`]. The task aborts the socket at once.
    fn reset(stack: Stack<'static>, reason: impl Into<String>) -> Self {
        Self {
            reset: host_reset(stack),
            ..Self::error(reason)
        }
    }

    /// A stream that the task closes for `reason`, with a FIN.
    fn closed(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            failed: false,
            reset: false,
            graceful: true,
        }
    }
}

/// Returns `true` if the error `ConnectionReset` that the task just got came
/// from the broker host: the link is up, and the network stack has an
/// address.
///
/// smoltcp also closes a socket when the stack loses the address of the
/// socket, and embassy-net then gives the same error. When the link drops,
/// embassy-net removes the address before smoltcp closes the socket. Thus
/// such a close always finds no link or no address here. A new address
/// needs a DHCP exchange, which takes much longer than the task needs to
/// see the error.
fn host_reset(stack: Stack<'static>) -> bool {
    stack.is_link_up() && stack.config_v4().is_some()
}

/// Returns the next job: a pending one, or the next one from the queue. The
/// future is safe to cancel.
async fn next_job(pending: &mut Pending) -> (u32, StreamOp) {
    match pending.pop_front() {
        Some(job) => job,
        None => STREAM.pop().await,
    }
}

/// Takes all the queued jobs after the `Open` for `connection`, removes the
/// superseded ones, and returns the stream to open, if any. The other jobs
/// stay in `pending`.
fn plan(connection: u32, open: Open, pending: &mut Pending) -> Option<(u32, Open)> {
    let mut queue: Vec<(u32, StreamOp)> = Vec::new();
    queue.push((connection, StreamOp::Open(open)));
    queue.extend(pending.drain(..));
    queue.extend(STREAM.take_all());
    let (signals, kept) = supersede(queue);
    for signal in signals {
        log::info!("stream: skip a stream that the core no longer wants");
        SIGNALS.push(signal);
    }
    *pending = kept.into();
    while let Some((id, op)) = pending.pop_front() {
        match op {
            StreamOp::Open(open) => return Some((id, open)),
            _ => log::debug!("stream: {id} is not open, so the task skips its job"),
        }
    }
    None
}

/// Takes a buffer of [`BUFFER`] bytes from the heap, and keeps it for the
/// life of the device.
fn buffer() -> &'static mut [u8] {
    Box::leak(vec![0; BUFFER].into_boxed_slice())
}

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>) {
    let rx = buffer();
    let tx = buffer();
    let mut streams = Streams::default();
    let mut pending = Pending::new();
    loop {
        let (connection, open) = match next_job(&mut pending).await {
            (connection, StreamOp::Open(open)) => match plan(connection, open, &mut pending) {
                Some(open) => open,
                None => continue,
            },
            // The shell answered the requests of the stream when it ended.
            (connection, _) => {
                log::debug!("stream: {connection} is not open, so the task skips its job");
                continue;
            }
        };
        if let Some(signal) = streams.begin(connection) {
            SIGNALS.push(signal);
        }
        log::info!("stream: open {connection} to {}:{}", open.host, open.port);
        let end = session(stack, rx, tx, &streams, connection, &open, &mut pending).await;
        let reset = if end.reset {
            " (reset by the host)"
        } else {
            ""
        };
        if end.failed {
            log::warn!("stream: {connection} failed: {}{reset}", end.reason);
        } else {
            log::info!("stream: {connection} closed: {}", end.reason);
        }
        if let Some(signal) = streams.end(connection, end.reason, end.reset) {
            SIGNALS.push(signal);
        }
    }
}

/// Opens the stream of `connection`, and moves bytes until it ends. Returns
/// why it ended.
async fn session(
    stack: Stack<'static>,
    rx: &mut [u8],
    tx: &mut [u8],
    streams: &Streams,
    connection: u32,
    open: &Open,
    pending: &mut Pending,
) -> End {
    let mut socket = TcpSocket::new(stack, rx, tx);
    // The `Write` jobs of the stream, from the oldest.
    let mut writes = VecDeque::new();
    let end = match connect(stack, &mut socket, connection, open, pending, &mut writes).await {
        Ok(()) => {
            log::info!("stream: {connection} is open");
            if let Some(signal) = streams.opened(connection) {
                SIGNALS.push(signal);
            }
            exchange(stack, &mut socket, streams, connection, pending, writes).await
        }
        // The socket sent nothing, so it needs no FIN.
        Err(end) => End {
            graceful: false,
            ..end
        },
    };
    shut(&mut socket, end.graceful).await;
    end
}

/// Finds the address of the host and opens the TCP connection. A newer
/// `Open` and a `Close` for `connection` stop the attempt.
/// A `Write` for `connection` goes to `writes`.
///
/// # Errors
///
/// Returns why the stream ended before it opened.
async fn connect(
    stack: Stack<'static>,
    socket: &mut TcpSocket<'_>,
    connection: u32,
    open: &Open,
    pending: &mut Pending,
    writes: &mut VecDeque<Vec<u8>>,
) -> Result<(), End> {
    let host = open.host.as_str();
    let query = with_timeout(DNS_TIMEOUT, resolve(stack, host));
    let address = match select(query, stop(connection, pending, writes)).await {
        Either::First(Ok(Ok(address))) => address,
        Either::First(Ok(Err(reason))) => return Err(End::error(reason)),
        Either::First(Err(_)) => {
            let secs = DNS_TIMEOUT.as_secs();
            return Err(End::error(format!("no DNS answer for {host} in {secs} s")));
        }
        Either::Second(end) => return Err(end),
    };
    let endpoint = (address, open.port);
    let attempt = with_timeout(CONNECT_TIMEOUT, socket.connect(endpoint));
    match select(attempt, stop(connection, pending, writes)).await {
        Either::First(Ok(Ok(()))) => Ok(()),
        Either::First(Ok(Err(error @ ConnectError::ConnectionReset))) => {
            Err(End::reset(stack, format!("TCP connect failed: {error:?}")))
        }
        Either::First(Ok(Err(error))) => Err(End::error(format!("TCP connect failed: {error:?}"))),
        Either::First(Err(_)) => {
            let secs = CONNECT_TIMEOUT.as_secs();
            Err(End::error(format!("no TCP connection in {secs} s")))
        }
        Either::Second(end) => Err(end),
    }
}

/// Takes the jobs while the stream of `connection` opens. Returns why the
/// attempt must stop: a newer `Open` (which goes back to `pending`) or a
/// `Close` for `connection`. A `Write` for `connection` goes to `writes`.
/// The future is safe to cancel.
async fn stop(connection: u32, pending: &mut Pending, writes: &mut VecDeque<Vec<u8>>) -> End {
    loop {
        match next_job(pending).await {
            (id, StreamOp::Open(open)) => {
                pending.push_front((id, StreamOp::Open(open)));
                return End::closed(REPLACED);
            }
            (id, StreamOp::Close(_)) if id == connection => return End::closed(CLOSED),
            (id, StreamOp::Write(write)) if id == connection => writes.push_back(write.bytes),
            (id, _) => log::debug!("stream: {id} is not open, so the task skips its job"),
        }
    }
}

/// Moves bytes between the open stream of `connection` and the core until
/// the stream ends. Returns why it ended.
///
/// Each pass of the loop waits in one select. Each future in it is safe to
/// cancel. A write that returns puts some bytes in the send buffer, and the
/// next pass writes the rest.
async fn exchange(
    stack: Stack<'static>,
    socket: &mut TcpSocket<'_>,
    streams: &Streams,
    connection: u32,
    pending: &mut Pending,
    mut writes: VecDeque<Vec<u8>>,
) -> End {
    let (mut reader, mut sender) = socket.split();
    let mut chunk = [0; READ_CHUNK];
    // The number of bytes of the oldest write that are in the send buffer.
    let mut sent = 0;
    // The time at which the oldest write stalls, if it gets no space.
    let mut stall = Instant::now() + WRITE_STALL;
    loop {
        let write = async {
            match writes.front() {
                // The socket waits for space to write no bytes, so an empty
                // write is done at once.
                Some(bytes) if sent >= bytes.len() => Some(Ok(0)),
                Some(bytes) => match select(sender.write(&bytes[sent..]), Timer::at(stall)).await {
                    Either::First(result) => Some(result),
                    Either::Second(()) => None,
                },
                None => core::future::pending().await,
            }
        };
        let event = select3(next_job(pending), reader.read(&mut chunk), write).await;
        match event {
            Either3::First((id, StreamOp::Open(open))) => {
                pending.push_front((id, StreamOp::Open(open)));
                return End::closed(REPLACED);
            }
            Either3::First((id, StreamOp::Close(_))) if id == connection => {
                return End::closed(CLOSED);
            }
            Either3::First((id, StreamOp::Write(write))) if id == connection => {
                if writes.is_empty() {
                    stall = Instant::now() + WRITE_STALL;
                }
                writes.push_back(write.bytes);
            }
            Either3::First((id, _)) => {
                log::debug!("stream: {id} is not open, so the task skips its job");
            }
            Either3::Second(Ok(0)) => {
                let mut end = End::closed("the broker closed the stream");
                end.graceful = false;
                return end;
            }
            Either3::Second(Ok(count)) => {
                if let Some(signal) = streams.received(connection, &chunk[..count]) {
                    SIGNALS.push(signal);
                }
            }
            Either3::Second(Err(error @ Error::ConnectionReset)) => {
                return End::reset(stack, format!("read failed: {error:?}"));
            }
            Either3::Third(Some(Ok(count))) => {
                sent += count;
                stall = Instant::now() + WRITE_STALL;
                if writes.front().is_some_and(|bytes| sent >= bytes.len()) {
                    writes.pop_front();
                    sent = 0;
                    if let Some(signal) = streams.written(connection, Written::Sent) {
                        SIGNALS.push(signal);
                    }
                }
            }
            Either3::Third(Some(Err(error @ Error::ConnectionReset))) => {
                return End::reset(stack, format!("write failed: {error:?}"));
            }
            Either3::Third(None) => {
                let secs = WRITE_STALL.as_secs();
                return End::error(format!("no space in the send buffer for {secs} s"));
            }
        }
    }
}

/// Closes the socket. If `graceful` is `true`, sends a FIN and waits for no
/// more than [`CLOSE_FLUSH`] for the broker to acknowledge the bytes that
/// wait. Then aborts the socket and waits for no more than [`ABORT_FLUSH`]
/// for the RST.
async fn shut(socket: &mut TcpSocket<'_>, graceful: bool) {
    if graceful {
        socket.close();
        if with_timeout(CLOSE_FLUSH, socket.flush()).await.is_err() {
            let secs = CLOSE_FLUSH.as_secs();
            log::info!("stream: the broker did not acknowledge the last bytes in {secs} s");
        }
    }
    socket.abort();
    let _ = with_timeout(ABORT_FLUSH, socket.flush()).await;
}

/// Finds the address of the host. The host is an IPv4 address or a name.
///
/// # Errors
///
/// Returns the reason if the host has no address.
async fn resolve(stack: Stack<'static>, host: &str) -> Result<IpAddress, String> {
    if let Ok(address) = Ipv4Addr::from_str(host) {
        return Ok(IpAddress::Ipv4(address));
    }
    let addresses = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|error| format!("DNS query for {host} failed: {error:?}"))?;
    addresses
        .first()
        .copied()
        .ok_or_else(|| format!("DNS has no address for {host}"))
}

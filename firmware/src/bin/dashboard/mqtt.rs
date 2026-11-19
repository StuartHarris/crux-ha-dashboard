//! The MQTT task: it opens one connection at a time with `rust-mqtt` over a
//! TCP socket, and it sends the items of the stream of that connection to
//! the core. `logic::mqtt::Streams` makes sure that each stream ends with
//! exactly one `Disconnected` item.
//!
//! When the task takes a `Connect`, it first takes all the queued jobs,
//! and `logic::mqtt::supersede` removes the connections that the core no
//! longer wants. A publish waits in a `logic::mqtt::Outbox` while the client
//! has no quota. If it cannot be sent, the connection closes, and the core
//! reconnects.
//!
//! While the task connects, it also takes its jobs. A `Disconnect` or a new
//! `Connect` stops the attempt, so the core does not wait for a connection
//! that it no longer wants.
//!
//! `rust-mqtt` flushes the transport after each packet. The flush of the
//! transport (`transport::Transport`) does not wait for the TCP ACK of the
//! broker, so a send returns when the packet is in the send buffer of the
//! socket. The task finds a dead link in these ways:
//!
//! - Each send has a time limit (`WRITE_TIMEOUT`). A send that cannot get
//!   space in the full send buffer ends the connection.
//! - Each read of a packet outside the main loop has a time limit
//!   (`READ_TIMEOUT`): the read of the body of a packet (with the automatic
//!   PUBACK of `rust-mqtt`), and the read while all the subscribe packets are
//!   in flight. These reads stop the checks of the main loop, so they must
//!   end.
//! - If sent data waits for its TCP ACK for `logic::mqtt::ACK_LIMIT`, the task
//!   ends the connection (`logic::mqtt::AckWatch`).
//! - The task ends the connection when the broker is silent for too long.
//!
//! When a session ends with an error, the task tells `HEALTH` before the
//! stream ends. Then the Wi-Fi task leaves and joins again at the next
//! `WifiOn` (`logic::wifi::LinkHealth`). A session that a new connection
//! replaced, a change of access point and a disconnect for the core are not
//! errors.
//!
//! Each connect has a clean start. The task answers each `Subscribe` job when
//! the broker acknowledged all its filters (`logic::mqtt::Subacks`). If the
//! connection closes first, the shell answers the job with `Failed`.
//!
//! After a change of access point (`LINK_CHANGED`), the task closes the
//! connection with a disconnect packet, which can fail if the TCP connection
//! is dead. The stream ends with a reason, and the core connects again.
//!
//! `rust-mqtt` sends no ping by itself, so the task sends one before the
//! keep-alive interval ends. `Client::poll_header` is the only receive call
//! that is safe to cancel, so the task selects on it, and then reads the
//! body of the packet to the end.

use alloc::{
    collections::VecDeque,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{cell::RefCell, net::Ipv4Addr, num::NonZero, pin::pin, str::FromStr};

use embassy_futures::select::{Either, Either3, Either4, select, select3, select4};
use embassy_net::{IpAddress, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer};
use logic::{
    mqtt::{
        ACK_LIMIT, AckWatch, Attempt, Next, Outbox, RETRY_WAIT, Streams, Subacks, broker_silent,
        ping_interval, silence_limit, supersede,
    },
    shell::MqttOp,
};
use rust_mqtt::{
    Bytes,
    buffer::AllocBuffer,
    client::{
        Client, MqttError,
        event::Event,
        options::{
            ConnectOptions, DisconnectOptions, PublicationOptions, SubscriptionOptions,
            TopicReference, WillOptions,
        },
    },
    config::KeepAlive,
    header::FixedHeader,
    types::{MqttBinary, MqttString, QoS, TopicFilter, TopicName},
};
use shared::capabilities::mqtt::{Connect, Publish, Qos, Subscribe, SubscribeOutcome};

use crate::{
    HEALTH, LINK_CHANGED, MQTT, SIGNALS,
    transport::{Link, Transport},
};

/// The size of the receive buffer of the socket. The retained states arrive
/// in a burst after the subscribe.
const RX_BUFFER: usize = 4096;
/// The size of the send buffer of the socket.
const TX_BUFFER: usize = 2048;
/// The longest time to open the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The time limit of one call that sends a packet. A send waits only while
/// the send buffer of the socket is full.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The time limit of a read outside the main loop: the body of a packet
/// after its header, or the next packet while all the subscribe packets are
/// in flight. The broker answers a subscribe packet at once, so a longer
/// silence means that the link is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The time limit of the disconnect packet.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The largest number of subscribe packets in flight. It is the
/// `SUBSCRIBE_MAXIMUM` of [`MqttClient`].
const IN_FLIGHT_SUBSCRIBES: usize = 16;

/// The client: up to 16 subscribe packets and 16 received messages in
/// flight, 4 sent messages in flight, 1 subscription identifier, 2 user
/// properties, and no topic aliases.
type MqttClient<'s, 'a, 'c> =
    Client<'s, 'c, Transport<'s, 'a>, AllocBuffer, IN_FLIGHT_SUBSCRIBES, 16, 4, 1, 2, 0, 0>;

/// Jobs that the task took from its queue but did not do yet.
type Pending = VecDeque<(u32, MqttOp)>;

/// Why a session ended.
struct End {
    /// The reason for the `Disconnected` item, or `None` for a disconnect
    /// that the core asked for.
    reason: Option<String>,
    /// `true` if the session failed: the link or the broker did not work.
    /// The Wi-Fi task then leaves and joins again.
    failed: bool,
}

impl End {
    /// A session that failed.
    fn error(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            failed: true,
        }
    }

    /// A session that the task closed for `reason`, or for the core if
    /// `reason` is `None`.
    const fn closed(reason: Option<String>) -> Self {
        Self {
            reason,
            failed: false,
        }
    }
}

/// Returns the reason for a read that took more than `READ_TIMEOUT`.
fn read_timed_out(what: &str) -> String {
    format!(
        "{what} did not arrive in {} s: the link is down",
        READ_TIMEOUT.as_secs()
    )
}

/// Returns the next job: a pending one, or the next one from the queue.
async fn next_job(pending: &mut Pending) -> (u32, MqttOp) {
    match pending.pop_front() {
        Some(job) => job,
        None => MQTT.pop().await,
    }
}

/// Takes all the queued jobs after the `Connect` for `connection`,
/// removes the superseded ones, and returns the connection to open, if any.
/// The other jobs stay in `pending`.
fn plan(connection: u32, connect: Connect, pending: &mut Pending) -> Option<(u32, Connect)> {
    let mut queue: Vec<(u32, MqttOp)> = Vec::new();
    queue.push((connection, MqttOp::Connect(connect)));
    queue.extend(pending.drain(..));
    queue.extend(MQTT.take_all());
    let (signals, kept) = supersede(queue);
    for signal in signals {
        log::info!("mqtt: skip a connection that the core no longer wants");
        SIGNALS.push(signal);
    }
    *pending = kept.into();
    while let Some((id, op)) = pending.pop_front() {
        match op {
            MqttOp::Connect(connect) => return Some((id, connect)),
            _ => log::debug!("mqtt: connection {id} is closed"),
        }
    }
    None
}

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>) {
    let mut streams = Streams::default();
    let mut pending = Pending::new();
    loop {
        let (connection, connect) = match next_job(&mut pending).await {
            (connection, MqttOp::Connect(connect)) => {
                match plan(connection, connect, &mut pending) {
                    Some(open) => open,
                    None => continue,
                }
            }
            (connection, _) => {
                log::debug!("mqtt: connection {connection} is closed");
                continue;
            }
        };
        if let Some(signal) = streams.begin(connection) {
            SIGNALS.push(signal);
        }
        log::info!(
            "mqtt: connect to {}:{} as {}",
            connect.host,
            connect.port,
            connect.client_id
        );
        let end = session(stack, &streams, connection, &connect, &mut pending).await;
        if let Some(reason) = &end.reason {
            log::warn!("mqtt: connection {connection} closed: {reason}");
        }
        // The Wi-Fi task must know before the core asks for Wi-Fi again.
        if end.failed {
            HEALTH.lock(|health| health.borrow_mut().session_failed());
        }
        if let Some(signal) = streams.end(connection, end.reason) {
            SIGNALS.push(signal);
        }
    }
}

/// Opens a connection, and gives the core its items until it closes.
async fn session(
    stack: Stack<'static>,
    streams: &Streams,
    connection: u32,
    connect: &Connect,
    pending: &mut Pending,
) -> End {
    let mut rx = [0; RX_BUFFER];
    let mut tx = [0; TX_BUFFER];
    let mut buffer = AllocBuffer;
    // A change of access point before this connection does not apply to it.
    // A change during the connect applies, because the TCP connection can
    // use the old access point, so the reset is before the connect.
    LINK_CHANGED.reset();
    let link = RefCell::new(Link {
        socket: TcpSocket::new(stack, &mut rx, &mut tx),
        watch: AckWatch::default(),
        written: 0,
    });
    let mut client =
        match connect_or_stop(stack, streams, connect, pending, &link, &mut buffer).await {
            Ok(client) => client,
            Err(end) => return end,
        };
    opened(streams, connection);
    let mut subacks = Subacks::default();

    let ping_every = ping_period(connect.keep_alive_secs);
    let mut next_ping = ping_every.map(|every| Instant::now() + every);
    // The time of the last packet from the broker.
    let mut last_heard = Instant::now();
    let mut outbox = Outbox::default();
    let mut retry_at: Option<Instant> = None;
    loop {
        let ack_at = match check_acks(&link) {
            Ok(deadline) => deadline,
            Err(end) => return end,
        };
        let ping = Timer::at(next_ping.unwrap_or(Instant::MAX));
        let retry = Timer::at(retry_at.unwrap_or(Instant::MAX));
        let work = select4(next_job(pending), client.poll_header(), ping, retry);
        let ack = Timer::at(ack_at.unwrap_or(Instant::MAX));
        let work = match select3(work, LINK_CHANGED.wait(), ack).await {
            Either3::First(work) => work,
            Either3::Second(()) => {
                disconnect(&mut client, DISCONNECT_TIMEOUT).await;
                return End::closed(Some("the Wi-Fi access point changed".into()));
            }
            // The check at the start of the loop reads the send buffer.
            Either3::Third(()) => continue,
        };
        match work {
            Either4::First((id, op)) => match op {
                MqttOp::Connect(next) => {
                    disconnect(&mut client, WRITE_TIMEOUT).await;
                    // The outer loop opens the new connection.
                    pending.push_front((id, MqttOp::Connect(next)));
                    return End::closed(Some("replaced by a new connection".into()));
                }
                _ if !streams.accepts(id) => {
                    log::debug!("mqtt: connection {id} is closed");
                }
                MqttOp::Subscribe(Subscribe { topics }) => {
                    let sent = subscribe_all(
                        &mut client,
                        streams,
                        connection,
                        &mut subacks,
                        &mut last_heard,
                        &topics,
                    );
                    if let Err(reason) = sent.await {
                        return End::error(reason);
                    }
                }
                MqttOp::Publish(publish) => match push(&mut client, &mut outbox, publish).await {
                    Ok(retry) => retry_at = retry,
                    Err(reason) => return End::error(reason),
                },
                MqttOp::Disconnect(_) => {
                    disconnect(&mut client, DISCONNECT_TIMEOUT).await;
                    return End::closed(None);
                }
            },
            Either4::Second(Ok(header)) => {
                last_heard = Instant::now();
                let read = read_body(&mut client, header, streams, connection, &mut subacks);
                if let Err(reason) = read.await {
                    return End::error(reason);
                }
                // An acknowledgement can give quota to a waiting publish.
                if !outbox.is_empty() {
                    match flush(&mut client, &mut outbox).await {
                        Ok(retry) => retry_at = retry,
                        Err(reason) => return End::error(reason),
                    }
                }
            }
            Either4::Second(Err(error)) => return End::error(format!("receive failed: {error:?}")),
            Either4::Fourth(()) => match flush(&mut client, &mut outbox).await {
                Ok(retry) => retry_at = retry,
                Err(reason) => return End::error(reason),
            },
            Either4::Third(()) => {
                if let Err(reason) =
                    check_and_ping(&mut client, last_heard, connect.keep_alive_secs).await
                {
                    return End::error(reason);
                }
                next_ping = ping_every.map(|every| Instant::now() + every);
            }
        }
    }
}

/// Returns the time between two pings for the keep-alive interval
/// `keep_alive_secs`, or `None` if keep-alive is off.
fn ping_period(keep_alive_secs: u16) -> Option<Duration> {
    ping_interval(keep_alive_secs)
        .and_then(|every| u64::try_from(every.as_millis()).ok())
        .map(Duration::from_millis)
}

/// Records the CONNACK of `connection`, and tells the core.
fn opened(streams: &Streams, connection: u32) {
    log::info!("mqtt: connected");
    // The CONNACK proves that the link moves data both ways.
    HEALTH.lock(|health| health.borrow_mut().traffic());
    if let Some(signal) = streams.connected(connection) {
        SIGNALS.push(signal);
    }
}

/// Reads the body of the packet with `header` and gives it to the core. The
/// read has the limit [`READ_TIMEOUT`], because it also sends the automatic
/// PUBACK, and the checks of the main loop do not run while it waits.
///
/// # Errors
///
/// Returns the reason to close the connection.
async fn read_body(
    client: &mut MqttClient<'_, '_, '_>,
    header: FixedHeader,
    streams: &Streams,
    connection: u32,
    subacks: &mut Subacks,
) -> Result<(), String> {
    match embassy_time::with_timeout(READ_TIMEOUT, client.poll_body(header)).await {
        Ok(Ok(event)) => deliver(streams, connection, subacks, &event),
        Ok(Err(error)) if error.is_recoverable() => log::warn!("mqtt: {error:?}"),
        Ok(Err(error)) => return Err(format!("receive failed: {error:?}")),
        // The client stopped in the middle of a packet, so the session
        // cannot continue.
        Err(_) => return Err(read_timed_out("the body of a packet")),
    }
    Ok(())
}

/// Reads the send buffer of the socket. Returns the time of the next
/// check, or the end of the session if sent data waited too long for its
/// TCP ACK.
fn check_acks(link: &RefCell<Link<'_>>) -> Result<Option<Instant>, End> {
    let mut link = link.borrow_mut();
    let deadline = link.observe();
    if link.ack_expired() {
        return Err(End::error(format!(
            "no TCP ACK for {} s: the link is down",
            ACK_LIMIT.as_secs()
        )));
    }
    Ok(deadline)
}

/// Returns the timeout of the TCP socket. It is the same limit as the silence
/// check, so there is one limit for both.
fn tcp_timeout(keep_alive_secs: u16) -> Option<Duration> {
    silence_limit(keep_alive_secs)
        .and_then(|limit| u64::try_from(limit.as_millis()).ok())
        .map(Duration::from_millis)
}

/// Sends a ping. Before it, checks the silence of the broker. The broker
/// answers each ping, so a long silence means that the link is dead, even if
/// the TCP stack does not see it.
///
/// # Errors
///
/// Returns the reason to close the connection.
async fn check_and_ping(
    client: &mut MqttClient<'_, '_, '_>,
    last_heard: Instant,
    keep_alive_secs: u16,
) -> Result<(), String> {
    if let Some(reason) = silence(last_heard, keep_alive_secs) {
        return Err(reason);
    }
    embassy_time::with_timeout(WRITE_TIMEOUT, client.ping())
        .await
        .map_err(|_| timed_out("ping"))?
        .map_err(|error| format!("ping failed: {error:?}"))
}

/// Returns the reason for a send that took more than `WRITE_TIMEOUT`.
fn timed_out(what: &str) -> String {
    format!(
        "{what} timed out after {} s: the link is down",
        WRITE_TIMEOUT.as_secs()
    )
}

/// Sends a disconnect packet, with the time limit `limit`. A failure or a
/// time-out only gets a log line, because the session ends anyway.
///
/// For the `Disconnect` request of the core, the limit is
/// [`DISCONNECT_TIMEOUT`]. The shell answers the request when the session
/// ends, so a dead link cannot stop the sleep for long.
async fn disconnect(client: &mut MqttClient<'_, '_, '_>, limit: Duration) {
    match embassy_time::with_timeout(limit, client.disconnect(&DisconnectOptions::new())).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::warn!("mqtt: disconnect failed: {error:?}"),
        Err(_) => log::warn!("mqtt: the disconnect timed out after {} s", limit.as_secs()),
    }
}

/// Opens the connection, and takes the jobs while it connects. A
/// `Disconnect` or a new `Connect` stops the attempt, and the function
/// returns the end of the session. The other jobs of this connection go back
/// to `pending`, in order, when the connection is open.
async fn connect_or_stop<'s, 'a, 'c>(
    stack: Stack<'static>,
    streams: &Streams,
    connect: &Connect,
    pending: &mut Pending,
    link: &'s RefCell<Link<'a>>,
    buffer: &'c mut AllocBuffer,
) -> Result<MqttClient<'s, 'a, 'c>, End> {
    let mut early = Vec::new();
    let open = {
        let mut attempt = pin!(attempt(stack, connect, link, buffer));
        loop {
            match select(attempt.as_mut(), next_job(pending)).await {
                Either::First(Ok(open)) => break open,
                Either::First(Err(reason)) => return Err(End::error(reason)),
                Either::Second((id, op)) => match op {
                    MqttOp::Connect(next) => {
                        pending.push_front((id, MqttOp::Connect(next)));
                        let reason = "replaced by a new connection".into();
                        return Err(End::closed(Some(reason)));
                    }
                    _ if !streams.accepts(id) => {
                        log::debug!("mqtt: connection {id} is closed");
                    }
                    MqttOp::Disconnect(_) => {
                        log::info!("mqtt: the connection stopped before it opened");
                        return Err(End::closed(None));
                    }
                    op => early.push((id, op)),
                },
            }
        }
    };
    for job in early.into_iter().rev() {
        pending.push_front(job);
    }
    Ok(open)
}

/// Resolves the host, opens the TCP connection of `link` and sends the MQTT
/// connect. Returns the client of the open connection, or the reason for
/// the failure.
async fn attempt<'s, 'a, 'c>(
    stack: Stack<'static>,
    connect: &Connect,
    link: &'s RefCell<Link<'a>>,
    buffer: &'c mut AllocBuffer,
) -> Result<MqttClient<'s, 'a, 'c>, String> {
    let address = resolve(stack, &connect.host).await?;
    tcp_connect(link, (address, connect.port), connect.keep_alive_secs).await?;
    let mut client = MqttClient::new(buffer);
    open(&mut client, Transport::new(link), connect).await?;
    Ok(client)
}

/// Opens the TCP connection of `link` to `endpoint`.
#[expect(
    clippy::await_holding_refcell_ref,
    reason = "the client does not exist yet, so nothing else borrows the link"
)]
async fn tcp_connect(
    link: &RefCell<Link<'_>>,
    endpoint: (IpAddress, u16),
    keep_alive_secs: u16,
) -> Result<(), String> {
    let mut link = link.borrow_mut();
    // This timeout alone closes a dead connection only after a long silence.
    // The session also limits each send (`WRITE_TIMEOUT`), watches the TCP
    // ACKs (`AckWatch`), and checks the silence of the broker (see
    // `broker_silent`).
    link.socket.set_timeout(tcp_timeout(keep_alive_secs));
    match embassy_time::with_timeout(CONNECT_TIMEOUT, link.socket.connect(endpoint)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("TCP connect failed: {error:?}")),
        Err(_) => Err("TCP connect timed out".into()),
    }
}

/// Returns the reason to close the connection if the broker was silent for too
/// long since `last_heard`.
fn silence(last_heard: Instant, keep_alive_secs: u16) -> Option<String> {
    let silent_for = last_heard.elapsed();
    broker_silent(
        core::time::Duration::from_millis(silent_for.as_millis()),
        keep_alive_secs,
    )
    .then(|| format!("no packet from the broker for {} s", silent_for.as_secs()))
}

/// Finds the address of the broker. The host is an IPv4 address or a name.
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

/// Sends the connect packet with a clean start, the credentials and the last
/// will.
async fn open<'s, 'a>(
    client: &mut MqttClient<'s, 'a, '_>,
    transport: Transport<'s, 'a>,
    connect: &Connect,
) -> Result<(), String> {
    let invalid = |what: &str| format!("the {what} is not valid for MQTT");
    let will_topic = MqttString::try_from(connect.last_will.topic.as_str())
        .ok()
        .and_then(TopicName::new)
        .ok_or_else(|| invalid("last will topic"))?;
    let will_payload = MqttBinary::try_from(connect.last_will.payload.as_slice())
        .map_err(|_| invalid("last will payload"))?;
    let mut will = WillOptions::new(will_topic, will_payload).at_least_once();
    if connect.last_will.retain {
        will = will.retain();
    }
    let keep_alive =
        NonZero::new(connect.keep_alive_secs).map_or(KeepAlive::Infinite, KeepAlive::Seconds);
    let mut options = ConnectOptions::new()
        .clean_start()
        .keep_alive(keep_alive)
        .will(will);
    if let Some(credentials) = &connect.credentials {
        options = options
            .user_name(
                MqttString::try_from(credentials.username.as_str())
                    .map_err(|_| invalid("user name"))?,
            )
            .password(
                MqttBinary::try_from(credentials.password.as_str())
                    .map_err(|_| invalid("password"))?,
            );
    }
    let client_id =
        MqttString::try_from(connect.client_id.as_str()).map_err(|_| invalid("client id"))?;
    // The error does not contain the password.
    embassy_time::with_timeout(
        WRITE_TIMEOUT,
        client.connect(transport, &options, Some(client_id)),
    )
    .await
    .map_err(|_| timed_out("connect"))?
    .map(|_| ())
    .map_err(|error| format!("the broker refused the connection: {error:?}"))
}

/// Sends the subscribe packets of one `Subscribe` job, one for each topic
/// filter. The task answers the job when the broker acknowledged all of
/// them, also if that happens here.
///
/// # Errors
///
/// Returns the reason to close the connection if a packet cannot be sent.
async fn subscribe_all(
    client: &mut MqttClient<'_, '_, '_>,
    streams: &Streams,
    connection: u32,
    subacks: &mut Subacks,
    last_heard: &mut Instant,
    topics: &[String],
) -> Result<(), String> {
    let started = Instant::now();
    let mut timing = SubscribeTiming::default();
    subacks.begin();
    for topic in topics {
        subscribe(
            client,
            streams,
            connection,
            subacks,
            last_heard,
            &mut timing,
            topic,
        )
        .await?;
    }
    log::info!(
        "mqtt: sent the subscribe for {} topics in {} ms ({} ms in sends; \
         {} packets read while {} were in flight, in {} ms)",
        topics.len(),
        started.elapsed().as_millis(),
        timing.sending.as_millis(),
        timing.reads,
        IN_FLIGHT_SUBSCRIBES,
        timing.reading.as_millis()
    );
    answer(streams, connection, subacks.finish());
    Ok(())
}

/// Where the time of one subscribe job goes, for the log.
#[derive(Default)]
struct SubscribeTiming {
    /// The time in the calls that send a subscribe packet.
    sending: Duration,
    /// The number of packets that the task read while all the subscribe
    /// packets were in flight.
    reads: u32,
    /// The time of these reads, which wait for the broker.
    reading: Duration,
}

/// Subscribes to one topic filter. `rust-mqtt` sends one filter in each
/// packet, and it keeps up to [`IN_FLIGHT_SUBSCRIBES`] subscribe packets in
/// flight. If all are in flight, the task reads packets (the retained
/// states) until the broker acknowledges one. Each read has the limit
/// [`READ_TIMEOUT`] and updates `last_heard`. `subacks` records the
/// packet, and `timing` the time of the sends and the reads.
async fn subscribe(
    client: &mut MqttClient<'_, '_, '_>,
    streams: &Streams,
    connection: u32,
    subacks: &mut Subacks,
    last_heard: &mut Instant,
    timing: &mut SubscribeTiming,
    topic: &str,
) -> Result<(), String> {
    let filter = MqttString::try_from(topic)
        .ok()
        .and_then(TopicFilter::new)
        .ok_or_else(|| format!("the topic filter {topic} is not valid"))?;
    let options = SubscriptionOptions::new().at_least_once();
    loop {
        let started = Instant::now();
        let sent = embassy_time::with_timeout(
            WRITE_TIMEOUT,
            client.subscribe(filter.as_borrowed(), &options),
        )
        .await
        .map_err(|_| timed_out("subscribe"))?;
        timing.sending += started.elapsed();
        match sent {
            Ok(packet) => {
                subacks.sent(packet.get().get());
                return Ok(());
            }
            Err(MqttError::SessionBuffer) => {
                let started = Instant::now();
                // This read is outside the main loop, so the checks of the
                // link do not run while it waits. A time limit ends it.
                let read = embassy_time::with_timeout(READ_TIMEOUT, client.poll())
                    .await
                    .map_err(|_| read_timed_out("the next packet of the subscribe"))?;
                timing.reads += 1;
                timing.reading += started.elapsed();
                match read {
                    Ok(event) => {
                        *last_heard = Instant::now();
                        deliver(streams, connection, subacks, &event);
                    }
                    Err(error) => return Err(format!("receive failed: {error:?}")),
                }
            }
            Err(error) => return Err(format!("subscribe to {topic} failed: {error:?}")),
        }
    }
}

/// Adds a message to the outbox and publishes the messages in it. Returns
/// the time of the next try if the client has no quota now.
///
/// # Errors
///
/// Returns the reason to close the connection if the outbox is full or a
/// message cannot be sent.
async fn push(
    client: &mut MqttClient<'_, '_, '_>,
    outbox: &mut Outbox,
    publish: Publish,
) -> Result<Option<Instant>, String> {
    outbox.push(publish)?;
    flush(client, outbox).await
}

/// Publishes the messages in the outbox, in order. Returns the time of the
/// next try if the client has no quota now.
///
/// # Errors
///
/// Returns the reason to close the connection if a message cannot be sent.
async fn flush(
    client: &mut MqttClient<'_, '_, '_>,
    outbox: &mut Outbox,
) -> Result<Option<Instant>, String> {
    loop {
        let attempt = match outbox.front() {
            Some(publish) => send(client, publish).await,
            None => return Ok(None),
        };
        match outbox.record(attempt) {
            Next::Continue => {}
            Next::Wait => {
                let wait = u64::try_from(RETRY_WAIT.as_millis()).unwrap_or(u64::MAX);
                return Ok(Some(Instant::now() + Duration::from_millis(wait)));
            }
            Next::End(reason) => return Err(reason),
        }
    }
}

/// Tries to publish a message once.
async fn send(client: &mut MqttClient<'_, '_, '_>, publish: &Publish) -> Attempt {
    let Some(topic) = MqttString::try_from(publish.topic.as_str())
        .ok()
        .and_then(TopicName::new)
    else {
        return Attempt::Failed(format!("the topic {} is not valid", publish.topic));
    };
    let qos = match publish.qos {
        Qos::AtMostOnce => QoS::AtMostOnce,
        Qos::AtLeastOnce => QoS::AtLeastOnce,
    };
    let mut options = PublicationOptions::new(TopicReference::Name(topic)).qos(qos);
    if publish.retain {
        options = options.retain();
    }
    let Ok(sent) = embassy_time::with_timeout(
        WRITE_TIMEOUT,
        client.publish(&options, Bytes::from(publish.payload.as_slice())),
    )
    .await
    else {
        return Attempt::Failed(timed_out("publish"));
    };
    match sent {
        Ok(_) => {
            log::info!("mqtt: published to {}", publish.topic);
            Attempt::Sent
        }
        Err(
            MqttError::SendQuotaExceeded
            | MqttError::SessionBuffer
            | MqttError::AllPacketIdentifiersUsed,
        ) => Attempt::Busy,
        Err(error) => Attempt::Failed(format!("publish to {} failed: {error:?}", publish.topic)),
    }
}

/// Gives the answers of the `Subscribe` jobs that are complete to the shell.
fn answer(streams: &Streams, connection: u32, outcomes: Vec<SubscribeOutcome>) {
    for signal in streams.subscribed(connection, outcomes) {
        log::info!("mqtt: {signal:?}");
        SIGNALS.push(signal);
    }
}

/// Gives a message from the broker to the core, and records the
/// acknowledgement of a subscribe packet. The task ignores the other events
/// (the other acknowledgements and the answers to pings).
fn deliver(streams: &Streams, connection: u32, subacks: &mut Subacks, event: &Event<'_, 1, 2>) {
    if let Event::Suback(suback) = event {
        let success = suback.reason_code.is_success();
        if !success {
            log::warn!(
                "mqtt: the broker refused a filter: {:?}",
                suback.reason_code
            );
        }
        let packet = suback.packet_identifier.get().get();
        answer(streams, connection, subacks.ack(packet, success));
    } else if let Event::Publish(publish) = event {
        let Some(topic) = publish.topic.name() else {
            log::warn!("mqtt: a message with a topic alias, which the client did not allow");
            return;
        };
        let topic: &MqttString<'_> = topic.as_ref();
        if let Some(signal) =
            streams.message(connection, topic.as_str(), publish.message.as_bytes())
        {
            SIGNALS.push(signal);
        }
    } else {
        log::trace!("mqtt: {}", event_name(event));
    }
}

fn event_name(event: &Event<'_, 1, 2>) -> String {
    let text = format!("{event:?}");
    text.split(['(', ' ', '{']).next().unwrap_or("").to_string()
}

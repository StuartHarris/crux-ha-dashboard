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
//! Each send waits for the ACK of the broker, because `rust-mqtt` flushes the
//! socket after each packet. When the Wi-Fi link is down, no ACK comes, and
//! the TCP timeout does not see the dead link. So each send has a time limit
//! (`WRITE_TIMEOUT`). A send that is too slow ends the connection. The task
//! also ends the connection when the broker is silent for too long.
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
use core::{net::Ipv4Addr, num::NonZero, pin::pin, str::FromStr};

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_net::{IpAddress, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer};
use logic::{
    mqtt::{
        Attempt, Next, Outbox, RETRY_WAIT, Streams, broker_silent, ping_interval, silence_limit,
        supersede,
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
    types::{MqttBinary, MqttString, QoS, TopicFilter, TopicName},
};
use shared::capabilities::mqtt::{Connect, Publish, Qos, Subscribe};

use crate::{MQTT, SIGNALS};

/// The size of the receive buffer of the socket. The retained states arrive
/// in a burst after the subscribe.
const RX_BUFFER: usize = 4096;
/// The size of the send buffer of the socket.
const TX_BUFFER: usize = 2048;
/// The longest time to open the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The time limit of one call that sends a packet. `rust-mqtt` flushes the
/// socket after each packet, and the flush waits for the ACK of the broker.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The time limit of the disconnect packet.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The client: up to 16 subscribe packets and 16 received messages in
/// flight, 4 sent messages in flight, 1 subscription identifier, 2 user
/// properties, and no topic aliases.
type MqttClient<'a, 'c> = Client<'a, 'c, TcpSocket<'a>, AllocBuffer, 16, 16, 4, 1, 2, 0, 0>;

/// Jobs that the task took from its queue but did not do yet.
type Pending = VecDeque<(u32, MqttOp)>;

/// Why a session ended.
struct End {
    /// The reason for the `Disconnected` item, or `None` for a disconnect
    /// that the core asked for.
    reason: Option<String>,
}

impl End {
    fn error(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
        }
    }
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
    let mut client = match connect_or_stop(
        stack,
        streams,
        connect,
        pending,
        &mut rx,
        &mut tx,
        &mut buffer,
    )
    .await
    {
        Ok(client) => client,
        Err(end) => return end,
    };
    log::info!("mqtt: connected");
    if let Some(signal) = streams.connected(connection) {
        SIGNALS.push(signal);
    }

    let ping_every = ping_interval(connect.keep_alive_secs)
        .and_then(|every| u64::try_from(every.as_millis()).ok())
        .map(Duration::from_millis);
    let mut next_ping = ping_every.map(|every| Instant::now() + every);
    // The time of the last packet from the broker.
    let mut last_heard = Instant::now();
    let mut outbox = Outbox::default();
    let mut retry_at: Option<Instant> = None;
    loop {
        let ping = Timer::at(next_ping.unwrap_or(Instant::MAX));
        let retry = Timer::at(retry_at.unwrap_or(Instant::MAX));
        match select4(next_job(pending), client.poll_header(), ping, retry).await {
            Either4::First((id, op)) => match op {
                MqttOp::Connect(next) => {
                    disconnect(&mut client, WRITE_TIMEOUT).await;
                    // The outer loop opens the new connection.
                    pending.push_front((id, MqttOp::Connect(next)));
                    return End::error("replaced by a new connection");
                }
                _ if !streams.accepts(id) => {
                    log::debug!("mqtt: connection {id} is closed");
                }
                MqttOp::Subscribe(Subscribe { topics }) => {
                    for topic in &topics {
                        if let Err(reason) =
                            subscribe(&mut client, streams, connection, topic).await
                        {
                            return End::error(reason);
                        }
                    }
                    log::info!("mqtt: subscribed to {} topics", topics.len());
                }
                MqttOp::Publish(publish) => {
                    if let Err(reason) = outbox.push(publish) {
                        return End::error(reason);
                    }
                    match flush(&mut client, &mut outbox).await {
                        Ok(retry) => retry_at = retry,
                        Err(reason) => return End::error(reason),
                    }
                }
                MqttOp::Disconnect(_) => {
                    disconnect(&mut client, DISCONNECT_TIMEOUT).await;
                    return End { reason: None };
                }
            },
            Either4::Second(Ok(header)) => {
                last_heard = Instant::now();
                match client.poll_body(header).await {
                    Ok(event) => deliver(streams, connection, &event),
                    Err(error) if error.is_recoverable() => log::warn!("mqtt: {error:?}"),
                    Err(error) => return End::error(format!("receive failed: {error:?}")),
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
    client: &mut MqttClient<'_, '_>,
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
async fn disconnect(client: &mut MqttClient<'_, '_>, limit: Duration) {
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
async fn connect_or_stop<'a, 'c>(
    stack: Stack<'static>,
    streams: &Streams,
    connect: &Connect,
    pending: &mut Pending,
    rx: &'a mut [u8],
    tx: &'a mut [u8],
    buffer: &'c mut AllocBuffer,
) -> Result<MqttClient<'a, 'c>, End> {
    let mut early = Vec::new();
    let client = {
        let mut attempt = pin!(attempt(stack, connect, rx, tx, buffer));
        loop {
            match select(attempt.as_mut(), next_job(pending)).await {
                Either::First(Ok(client)) => break client,
                Either::First(Err(reason)) => return Err(End::error(reason)),
                Either::Second((id, op)) => match op {
                    MqttOp::Connect(next) => {
                        pending.push_front((id, MqttOp::Connect(next)));
                        return Err(End::error("replaced by a new connection"));
                    }
                    _ if !streams.accepts(id) => {
                        log::debug!("mqtt: connection {id} is closed");
                    }
                    MqttOp::Disconnect(_) => {
                        log::info!("mqtt: the connection stopped before it opened");
                        return Err(End { reason: None });
                    }
                    op => early.push((id, op)),
                },
            }
        }
    };
    for job in early.into_iter().rev() {
        pending.push_front(job);
    }
    Ok(client)
}

/// Resolves the host, opens the TCP connection and sends the MQTT
/// connect. Returns the client of the open connection, or the reason for
/// the failure.
async fn attempt<'a, 'c>(
    stack: Stack<'static>,
    connect: &Connect,
    rx: &'a mut [u8],
    tx: &'a mut [u8],
    buffer: &'c mut AllocBuffer,
) -> Result<MqttClient<'a, 'c>, String> {
    let address = resolve(stack, &connect.host).await?;
    let mut socket = TcpSocket::new(stack, rx, tx);
    // This timeout alone does not see a dead Wi-Fi link. Each send waits for
    // the ACK of the broker, so each send has its own time limit
    // (`WRITE_TIMEOUT`). The session also checks the silence of the broker
    // (see `broker_silent`).
    socket.set_timeout(tcp_timeout(connect.keep_alive_secs));
    match embassy_time::with_timeout(CONNECT_TIMEOUT, socket.connect((address, connect.port))).await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(format!("TCP connect failed: {error:?}")),
        Err(_) => return Err("TCP connect timed out".into()),
    }
    let mut client = MqttClient::new(buffer);
    open(&mut client, socket, connect).await?;
    Ok(client)
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

/// Sends the connect packet with the credentials and the last will.
async fn open<'a>(
    client: &mut MqttClient<'a, '_>,
    socket: TcpSocket<'a>,
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
        client.connect(socket, &options, Some(client_id)),
    )
    .await
    .map_err(|_| timed_out("connect"))?
    .map(|_| ())
    .map_err(|error| format!("the broker refused the connection: {error:?}"))
}

/// Subscribes to one topic filter. `rust-mqtt` sends one filter in each
/// packet, and it keeps a few subscribe packets in flight. If all are in
/// flight, the task reads packets (the retained states) until the broker
/// acknowledges one.
async fn subscribe(
    client: &mut MqttClient<'_, '_>,
    streams: &Streams,
    connection: u32,
    topic: &str,
) -> Result<(), String> {
    let filter = MqttString::try_from(topic)
        .ok()
        .and_then(TopicFilter::new)
        .ok_or_else(|| format!("the topic filter {topic} is not valid"))?;
    let options = SubscriptionOptions::new().at_least_once();
    loop {
        let sent = embassy_time::with_timeout(
            WRITE_TIMEOUT,
            client.subscribe(filter.as_borrowed(), &options),
        )
        .await
        .map_err(|_| timed_out("subscribe"))?;
        match sent {
            Ok(_) => return Ok(()),
            Err(MqttError::SessionBuffer) => match client.poll().await {
                Ok(event) => deliver(streams, connection, &event),
                Err(error) => return Err(format!("receive failed: {error:?}")),
            },
            Err(error) => return Err(format!("subscribe to {topic} failed: {error:?}")),
        }
    }
}

/// Publishes the messages in the outbox, in order. Returns the time of the
/// next try if the client has no quota now.
///
/// # Errors
///
/// Returns the reason to close the connection if a message cannot be sent.
async fn flush(
    client: &mut MqttClient<'_, '_>,
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
async fn send(client: &mut MqttClient<'_, '_>, publish: &Publish) -> Attempt {
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

/// Gives a message from the broker to the core. The task ignores the other
/// events (acknowledgements and the answers to pings).
fn deliver(streams: &Streams, connection: u32, event: &Event<'_, 1, 2>) {
    if let Event::Publish(publish) = event {
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

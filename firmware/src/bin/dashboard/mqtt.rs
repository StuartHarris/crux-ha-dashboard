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
use core::{net::Ipv4Addr, num::NonZero, str::FromStr};

use embassy_futures::select::{Either4, select4};
use embassy_net::{IpAddress, Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer};
use logic::{
    mqtt::{Attempt, Next, Outbox, RETRY_WAIT, Streams, ping_interval, supersede},
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
    let address = match resolve(stack, &connect.host).await {
        Ok(address) => address,
        Err(reason) => return End::error(reason),
    };
    let mut rx = [0; RX_BUFFER];
    let mut tx = [0; TX_BUFFER];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    // The broker sends a packet at least once in each keep-alive interval
    // (the answer to a ping), so a longer silence means that the link is dead.
    let silence = u64::from(connect.keep_alive_secs.max(30)) * 2;
    socket.set_timeout(Some(Duration::from_secs(silence)));
    match embassy_time::with_timeout(CONNECT_TIMEOUT, socket.connect((address, connect.port))).await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return End::error(format!("TCP connect failed: {error:?}")),
        Err(_) => return End::error("TCP connect timed out"),
    }

    let mut buffer = AllocBuffer;
    let mut client = MqttClient::new(&mut buffer);
    if let Err(reason) = open(&mut client, socket, connect).await {
        return End::error(reason);
    }
    log::info!("mqtt: connected");
    if let Some(signal) = streams.connected(connection) {
        SIGNALS.push(signal);
    }

    let ping_every = ping_interval(connect.keep_alive_secs)
        .and_then(|every| u64::try_from(every.as_millis()).ok())
        .map(Duration::from_millis);
    let mut next_ping = ping_every.map(|every| Instant::now() + every);
    let mut outbox = Outbox::default();
    let mut retry_at: Option<Instant> = None;
    loop {
        let ping = Timer::at(next_ping.unwrap_or(Instant::MAX));
        let retry = Timer::at(retry_at.unwrap_or(Instant::MAX));
        match select4(next_job(pending), client.poll_header(), ping, retry).await {
            Either4::First((id, op)) => match op {
                MqttOp::Connect(next) => {
                    let _ = client.disconnect(&DisconnectOptions::new()).await;
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
                    if let Err(error) = client.disconnect(&DisconnectOptions::new()).await {
                        log::warn!("mqtt: disconnect failed: {error:?}");
                    }
                    return End { reason: None };
                }
            },
            Either4::Second(Ok(header)) => {
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
                if let Err(error) = client.ping().await {
                    return End::error(format!("ping failed: {error:?}"));
                }
                next_ping = ping_every.map(|every| Instant::now() + every);
            }
        }
    }
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
    client
        .connect(socket, &options, Some(client_id))
        .await
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
        match client.subscribe(filter.as_borrowed(), &options).await {
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
    match client
        .publish(&options, Bytes::from(publish.payload.as_slice()))
        .await
    {
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

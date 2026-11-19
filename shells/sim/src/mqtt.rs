//! The MQTT half of the shell: a `rumqttc` client for each `Connect` request.
//!
//! A task on the tokio runtime polls the event loop of the client. The task
//! converts each `rumqttc` event to an [`MqttEvent`] and sends it to the main
//! thread, which gives it to the core. The task stops after the first
//! `Disconnected`, because the core, not the shell, decides when to connect
//! again. (A `rumqttc` event loop connects again by itself if you continue to
//! poll it after an error.)
//!
//! Each connection has a clean session, so the simulator never leaves a
//! session on the broker. The SUBACK packet of a subscribe gives the answer
//! to the `Subscribe` request.

use std::time::Duration;

use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS,
    SubscribeFilter, SubscribeReasonCode,
};
use shared::capabilities::mqtt::{Connect, Message, MqttEvent, Publish, Qos, SubscribeOutcome};
use tokio::{runtime::Handle, task::JoinHandle};

/// The number of requests that can wait for the event loop.
const CAPACITY: usize = 256;
/// The largest MQTT packet, in bytes, in each direction. A retained
/// state-stream message for an entity with many attributes can be larger
/// than the `rumqttc` default of 10 KiB. A packet that is too large stops
/// the connection.
pub const MAX_PACKET_SIZE: usize = 256 * 1024;

/// What the task does with a `rumqttc` event.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Send this item to the core and continue.
    Send(MqttEvent),
    /// Answer the `Subscribe` request with this outcome and continue.
    Subscribed(SubscribeOutcome),
    /// Send this item to the core and stop. It is always `Disconnected`.
    Stop(MqttEvent),
    /// Ignore the event.
    Ignore,
}

/// Converts a `rumqttc` event to what the task does with it.
#[must_use]
pub fn map_event(event: &Event) -> Step {
    match event {
        Event::Incoming(Incoming::ConnAck(ack)) if ack.code == ConnectReturnCode::Success => {
            Step::Send(MqttEvent::Connected)
        }
        Event::Incoming(Incoming::ConnAck(ack)) => Step::Stop(MqttEvent::Disconnected {
            reason: Some(format!("the broker refused the connection: {:?}", ack.code)),
        }),
        Event::Incoming(Incoming::Publish(publish)) => Step::Send(MqttEvent::Message(Message {
            topic: publish.topic.clone(),
            payload: publish.payload.to_vec(),
        })),
        Event::Incoming(Incoming::SubAck(ack)) => {
            let done = ack
                .return_codes
                .iter()
                .all(|code| matches!(code, SubscribeReasonCode::Success(_)));
            Step::Subscribed(if done {
                SubscribeOutcome::Done
            } else {
                SubscribeOutcome::Failed
            })
        }
        Event::Incoming(Incoming::Disconnect) => Step::Stop(MqttEvent::Disconnected {
            reason: Some("the broker closed the connection".into()),
        }),
        Event::Outgoing(Outgoing::Disconnect) => {
            Step::Stop(MqttEvent::Disconnected { reason: None })
        }
        Event::Incoming(_) | Event::Outgoing(_) => Step::Ignore,
    }
}

/// Returns `true` if `event` is an incoming message with the retained flag.
#[must_use]
pub const fn retained(event: &Event) -> bool {
    matches!(event, Event::Incoming(Incoming::Publish(publish)) if publish.retain)
}

/// Converts the error of a poll to the last item of the stream.
#[must_use]
pub fn map_error(error: &impl std::fmt::Display) -> MqttEvent {
    MqttEvent::Disconnected {
        reason: Some(error.to_string()),
    }
}

/// Makes the `rumqttc` options for a `Connect` request.
#[must_use]
pub fn options(connect: &Connect) -> MqttOptions {
    let mut options = MqttOptions::new(&connect.client_id, &connect.host, connect.port);
    options
        .set_keep_alive(Duration::from_secs(connect.keep_alive_secs.into()))
        .set_clean_session(true)
        .set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE)
        .set_last_will(rumqttc::LastWill::new(
            &connect.last_will.topic,
            connect.last_will.payload.clone(),
            QoS::AtLeastOnce,
            connect.last_will.retain,
        ));
    if let Some(credentials) = &connect.credentials {
        options.set_credentials(&credentials.username, &credentials.password);
    }
    options
}

/// One MQTT connection, as the shell sees it. [`Connection`] is the real
/// one. The tests of the shell use a fake one.
pub trait Link {
    /// Subscribes to `topics` with one subscribe packet. The stream answers
    /// with [`Item::Subscribed`] when the SUBACK packet arrives.
    ///
    /// # Errors
    ///
    /// Returns a message if the request cannot go to the client.
    fn subscribe(&self, topics: &[String]) -> Result<(), String>;

    /// Publishes a message.
    ///
    /// # Errors
    ///
    /// Returns a message if the request cannot go to the client.
    fn publish(&self, publish: &Publish) -> Result<(), String>;

    /// Sends a disconnect packet after the requests that wait. The stream
    /// then sends `Disconnected` and stops.
    ///
    /// # Errors
    ///
    /// Returns a message if the request cannot go to the client.
    fn disconnect(&self) -> Result<(), String>;

    /// Stops the connection at once, with no disconnect packet. The broker
    /// then publishes the last will.
    fn abort(&self);
}

/// What a connection gives to the shell.
#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    /// An item of the stream. The flag is `true` for a message that the
    /// broker retained.
    Stream(MqttEvent, bool),
    /// The answer to the oldest `Subscribe` request of the connection.
    Subscribed(SubscribeOutcome),
}

/// The function that gives each item of a connection to the shell.
pub type Sink = Box<dyn Fn(Item) + Send + 'static>;

/// Opens a connection for a `Connect` request, and gives each item of its
/// stream to the sink.
pub type Opener = Box<dyn FnMut(&Connect, Sink) -> Box<dyn Link>>;

/// Returns an opener that makes a real connection with a task on `runtime`.
pub fn opener(runtime: Handle) -> Opener {
    Box::new(move |connect, sink| Box::new(Connection::open(&runtime, connect, sink)))
}

/// An open connection: the client, and the task that polls it.
pub struct Connection {
    client: AsyncClient,
    task: JoinHandle<()>,
}

impl Connection {
    /// Opens a connection for `connect` and starts the task. The task gives
    /// each item of the stream to `sink`.
    pub fn open(runtime: &Handle, connect: &Connect, sink: Sink) -> Self {
        let (client, event_loop) = AsyncClient::new(options(connect), CAPACITY);
        let task = runtime.spawn(poll(event_loop, sink));
        Self { client, task }
    }
}

impl Link for Connection {
    fn subscribe(&self, topics: &[String]) -> Result<(), String> {
        let filters = topics
            .iter()
            .map(|topic| SubscribeFilter::new(topic.clone(), QoS::AtLeastOnce));
        self.client
            .try_subscribe_many(filters)
            .map_err(|error| error.to_string())
    }

    fn publish(&self, publish: &Publish) -> Result<(), String> {
        let qos = match publish.qos {
            Qos::AtMostOnce => QoS::AtMostOnce,
            Qos::AtLeastOnce => QoS::AtLeastOnce,
        };
        self.client
            .try_publish(&publish.topic, qos, publish.retain, publish.payload.clone())
            .map_err(|error| error.to_string())
    }

    fn disconnect(&self) -> Result<(), String> {
        self.client
            .try_disconnect()
            .map_err(|error| error.to_string())
    }

    fn abort(&self) {
        self.task.abort();
    }
}

async fn poll(mut event_loop: EventLoop, send: Sink) {
    loop {
        match event_loop.poll().await {
            Ok(event) => match map_event(&event) {
                Step::Send(item) => send(Item::Stream(item, retained(&event))),
                Step::Subscribed(outcome) => send(Item::Subscribed(outcome)),
                Step::Stop(item) => {
                    send(Item::Stream(item, false));
                    return;
                }
                Step::Ignore => {}
            },
            Err(error) => {
                send(Item::Stream(map_error(&error), false));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rumqttc::{
        ConnAck, ConnectReturnCode, Event, Incoming, Outgoing, Publish, QoS, SubAck,
        SubscribeReasonCode,
    };
    use shared::capabilities::mqtt::{
        Connect, Credentials, LastWill, Message, MqttEvent, SubscribeOutcome,
    };

    use super::{Step, map_error, map_event, options, retained};

    #[test]
    fn a_connack_with_success_is_connected() {
        let event = Event::Incoming(Incoming::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )));
        assert_eq!(map_event(&event), Step::Send(MqttEvent::Connected));
    }

    #[test]
    fn a_refused_connack_stops_the_stream() {
        let event = Event::Incoming(Incoming::ConnAck(ConnAck::new(
            ConnectReturnCode::NotAuthorized,
            false,
        )));
        let Step::Stop(MqttEvent::Disconnected {
            reason: Some(reason),
        }) = map_event(&event)
        else {
            panic!("a refused connection must stop the stream");
        };
        assert!(reason.contains("NotAuthorized"), "{reason}");
    }

    #[test]
    fn a_suback_answers_the_subscribe() {
        let granted = SubscribeReasonCode::Success(QoS::AtLeastOnce);
        let suback = |codes| Event::Incoming(Incoming::SubAck(SubAck::new(1, codes)));
        assert_eq!(
            map_event(&suback(vec![granted, granted])),
            Step::Subscribed(SubscribeOutcome::Done)
        );
        assert_eq!(
            map_event(&suback(vec![granted, SubscribeReasonCode::Failure])),
            Step::Subscribed(SubscribeOutcome::Failed)
        );
    }

    #[test]
    fn a_publish_is_a_message() {
        let topic = "homeassistant/statestream/light/kitchen/state";
        let event = Event::Incoming(Incoming::Publish(Publish::new(
            topic,
            QoS::AtLeastOnce,
            "on",
        )));
        assert_eq!(
            map_event(&event),
            Step::Send(MqttEvent::Message(Message {
                topic: topic.into(),
                payload: b"on".to_vec(),
            }))
        );
    }

    #[test]
    fn the_retained_flag_of_a_message() {
        let mut publish = Publish::new("dashboard/sim/status", QoS::AtLeastOnce, "online");
        assert!(!retained(&Event::Incoming(Incoming::Publish(
            publish.clone()
        ))));
        publish.retain = true;
        assert!(retained(&Event::Incoming(Incoming::Publish(publish))));
        assert!(!retained(&Event::Incoming(Incoming::PingResp)));
    }

    #[test]
    fn a_disconnect_in_either_direction_stops_the_stream() {
        assert_eq!(
            map_event(&Event::Outgoing(Outgoing::Disconnect)),
            Step::Stop(MqttEvent::Disconnected { reason: None })
        );
        assert!(matches!(
            map_event(&Event::Incoming(Incoming::Disconnect)),
            Step::Stop(MqttEvent::Disconnected { reason: Some(_) })
        ));
    }

    #[test]
    fn other_packets_are_ignored() {
        assert_eq!(
            map_event(&Event::Incoming(Incoming::PingResp)),
            Step::Ignore
        );
        assert_eq!(map_event(&Event::Outgoing(Outgoing::PingReq)), Step::Ignore);
        assert_eq!(
            map_event(&Event::Outgoing(Outgoing::Publish(1))),
            Step::Ignore
        );
    }

    #[test]
    fn an_error_is_the_last_item_with_its_text() {
        assert_eq!(
            map_error(&"connection refused"),
            MqttEvent::Disconnected {
                reason: Some("connection refused".into())
            }
        );
    }

    #[test]
    fn the_options_have_the_last_will_and_the_credentials() {
        let connect = Connect {
            host: "broker".into(),
            port: 1884,
            client_id: "dashboard-sim".into(),
            credentials: Some(Credentials {
                username: "dashboard".into(),
                password: "secret".into(),
            }),
            keep_alive_secs: 60,
            last_will: LastWill {
                topic: "dashboard/sim/status".into(),
                payload: b"offline".to_vec(),
                retain: true,
            },
        };
        let options = options(&connect);
        assert_eq!(options.broker_address(), ("broker".into(), 1884));
        assert_eq!(options.client_id(), "dashboard-sim");
        assert_eq!(options.keep_alive().as_secs(), 60);
        assert!(options.clean_session());
        assert_eq!(options.max_packet_size(), 256 * 1024);
        let will = options.last_will().unwrap();
        assert_eq!(will.topic, "dashboard/sim/status");
        assert_eq!(&will.message[..], b"offline");
        assert!(will.retain);
        let login = options.credentials().unwrap();
        assert_eq!(login.username, "dashboard");
        assert_eq!(login.password, "secret");
    }
}

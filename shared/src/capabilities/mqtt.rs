//! The MQTT capability: the operations that the shell performs on an MQTT
//! broker, and the commands that make them.
//!
//! A connection is a stream. The shell answers a [`Connect`] request with a
//! sequence of [`MqttEvent`] values: `Connected` when the broker accepts the
//! connection, a `Message` for each message on a subscribed topic, and one
//! `Disconnected` as the last item. Messages for all subscriptions arrive on
//! the stream of their connection. [`Publish`] is a notification.
//! [`Subscribe`] is a request, and the shell answers it when the broker
//! acknowledged all the topic filters. [`Disconnect`] is a request, and the
//! shell answers it when the connection is closed. These three operations
//! apply to the open connection.

use alloc::{string::String, vec::Vec};
use core::fmt;

use crux_core::{
    Command, Request,
    command::{RequestBuilder, StreamBuilder},
    macros::Operation,
};
use facet::Facet;
use futures::Stream;
use serde::{Deserialize, Serialize};

use super::notify;

/// The user name and password for the broker.
///
/// The `Debug` output does not show the password, so a log of a `Connect`
/// request or of the settings is safe.
#[derive(Facet, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The user name.
    pub username: String,
    /// The password.
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// The quality of service for a publish.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Qos {
    /// The broker gets the message at most once (`QoS` 0).
    AtMostOnce,
    /// The broker gets the message at least once (`QoS` 1).
    AtLeastOnce,
}

/// The message that the broker publishes if the connection stops without a
/// disconnect from the client.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LastWill {
    /// The topic of the message.
    pub topic: String,
    /// The payload of the message.
    pub payload: Vec<u8>,
    /// If `true`, the broker keeps the message for new subscribers.
    pub retain: bool,
}

/// Opens a connection to the broker.
///
/// The shell answers with a stream of [`MqttEvent`] values. The last item is
/// always `Disconnected`, also when the connection fails before it opens. The
/// shell does not reconnect by itself. The core decides when to try again.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(stream, output = MqttEvent)]
pub struct Connect {
    /// The host name or IP address of the broker.
    pub host: String,
    /// The TCP port of the broker.
    pub port: u16,
    /// The MQTT client id.
    pub client_id: String,
    /// The credentials, if the broker needs them.
    pub credentials: Option<Credentials>,
    /// The keep-alive interval in seconds.
    pub keep_alive_secs: u16,
    /// The last-will message.
    pub last_will: LastWill,
}

/// Subscribes the open connection to topic filters.
///
/// Messages on these topics arrive on the stream of the [`Connect`] request.
/// The shell answers with [`SubscribeOutcome::Done`] when the broker
/// acknowledged each filter with success. The shell answers with
/// [`SubscribeOutcome::Failed`] if the broker refuses a filter, or if the
/// connection closes before all the acknowledgements arrive. If no connection
/// is open, the shell answers `Failed` at once.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = SubscribeOutcome)]
pub struct Subscribe {
    /// The topic filters, which can contain the `+` and `#` wildcards.
    pub topics: Vec<String>,
}

/// The answer to a [`Subscribe`] request.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum SubscribeOutcome {
    /// The broker acknowledged each topic filter with success.
    Done,
    /// The broker refused a topic filter, or the connection closed before
    /// all the acknowledgements arrived.
    Failed,
}

/// Publishes a message on the open connection.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct Publish {
    /// The topic of the message.
    pub topic: String,
    /// The payload of the message.
    pub payload: Vec<u8>,
    /// The quality of service.
    pub qos: Qos,
    /// If `true`, the broker keeps the message for new subscribers.
    pub retain: bool,
}

/// Closes the open connection with a disconnect packet, so the broker does
/// not publish the last will.
///
/// The stream of the connection sends `Disconnected` and ends. Then the shell
/// answers this request. If no connection is open, the shell answers at once.
/// The shell can limit the time for the disconnect packet, so that a dead
/// link does not stop the answer. The connection then closes with no packet.
///
/// The shell must do the MQTT operations in the order that it gets them. Before
/// sleep, the core publishes `asleep` on the status topic and then sends
/// `Disconnect`, so the publish must reach the broker first.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Disconnect;

/// A message from the broker on a subscribed topic.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The topic of the message.
    pub topic: String,
    /// The payload of the message.
    pub payload: Vec<u8>,
}

/// An item in the stream of a connection.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum MqttEvent {
    /// The broker accepted the connection.
    Connected,
    /// A message arrived on a subscribed topic.
    Message(Message),
    /// The connection closed or did not open. This is the last item.
    Disconnected {
        /// Why the connection closed, if the shell knows.
        reason: Option<String>,
    },
}

/// Makes the commands for the MQTT operations.
pub struct Mqtt;

impl Mqtt {
    /// Opens a connection. The stream gives the events of the connection.
    #[must_use]
    pub fn connect<Effect, Event>(
        connect: Connect,
    ) -> StreamBuilder<Effect, Event, impl Stream<Item = MqttEvent>>
    where
        Effect: From<Request<Connect>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::stream_from_shell(connect)
    }

    /// Subscribes the open connection to `topics`. The request ends when the
    /// broker acknowledged all the filters, or when the connection closes.
    #[must_use]
    pub fn subscribe<Effect, Event>(
        topics: Vec<String>,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = SubscribeOutcome>>
    where
        Effect: From<Request<Subscribe>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Subscribe { topics })
    }

    /// Publishes a message on the open connection.
    pub fn publish<Effect, Event>(publish: Publish) -> Command<Effect, Event>
    where
        Effect: From<Request<Publish>> + Send + 'static,
        Event: Send + 'static,
    {
        notify(publish)
    }

    /// Closes the open connection. The request ends when the connection is
    /// closed.
    #[must_use]
    pub fn disconnect<Effect, Event>() -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<Disconnect>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Disconnect)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::{Connect, Credentials, LastWill};

    #[test]
    fn debug_does_not_show_the_password() {
        let credentials = Credentials {
            username: "dashboard".into(),
            password: "secret-password".into(),
        };
        let connect = Connect {
            host: "broker".into(),
            port: 1883,
            client_id: "dashboard-hall".into(),
            credentials: Some(credentials.clone()),
            keep_alive_secs: 60,
            last_will: LastWill {
                topic: "dashboard/hall/status".into(),
                payload: b"offline".to_vec(),
                retain: true,
            },
        };
        for text in [format!("{credentials:?}"), format!("{connect:?}")] {
            assert!(!text.contains("secret-password"), "{text}");
            assert!(text.contains("dashboard"), "{text}");
        }
    }
}

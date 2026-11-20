//! The data of an MQTT connection: the options, the messages and the result
//! of a subscribe.

use alloc::{string::String, vec::Vec};
use core::fmt;

use facet::Facet;
use serde::{Deserialize, Serialize};

/// The user name and password for the broker.
///
/// The `Debug` output does not show the password, so a log of a [`Connect`]
/// value or of the settings is safe.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qos {
    /// The broker gets the message at most once (`QoS` 0).
    AtMostOnce,
    /// The broker gets the message at least once (`QoS` 1).
    AtLeastOnce,
}

/// The message that the broker publishes if the connection stops without a
/// disconnect from the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastWill {
    /// The topic of the message.
    pub topic: String,
    /// The payload of the message.
    pub payload: Vec<u8>,
    /// If `true`, the broker keeps the message for new subscribers.
    pub retain: bool,
}

/// The options of a connection to the broker.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// The result of a subscribe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscribeOutcome {
    /// The broker acknowledged each topic filter with success.
    Done,
    /// The broker refused a topic filter, or the connection closed before
    /// all the acknowledgements arrived.
    Failed,
}

/// A message that the client publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// A message from the broker on a subscribed topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The topic of the message.
    pub topic: String,
    /// The payload of the message.
    pub payload: Vec<u8>,
}

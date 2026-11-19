//! A fake MQTT broker with a fake Home Assistant behind it.
//!
//! The broker keeps retained messages, matches topic filters with the `+` and
//! `#` wildcards, and sends messages to the one client through the stream of
//! its `Connect` request. It publishes the last will when a connection stops
//! without a disconnect.
//! The topic filters belong to the connection, so a new connection has
//! none.
//!
//! The fake Home Assistant publishes entity states as the `mqtt_statestream`
//! integration does, and answers service calls on `dashboard/<id>/cmd` as the
//! command automation does.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crux_core::Request;
use serde::Deserialize;

use crate::{
    capabilities::mqtt::{
        Connect, LastWill, Message, MqttEvent, Publish, Subscribe, SubscribeOutcome,
    },
    entities::STATESTREAM,
};

/// The open connection of the dashboard.
struct Connection {
    request: Request<Connect>,
    filters: Vec<String>,
    /// The `Subscribe` requests that wait for the acknowledgements that the
    /// broker holds back. Refer to [`Broker::hold_suback`].
    subscribes: Vec<Request<Subscribe>>,
}

/// A service call that the fake Home Assistant got.
#[derive(Deserialize, Debug, PartialEq, Eq)]
pub struct ServiceCall {
    pub service: String,
    pub entity_id: String,
    /// The change in brightness of a `light.turn_on` call, in percent.
    pub brightness_step_pct: Option<i8>,
}

/// The fake broker.
#[derive(Default)]
pub struct Broker {
    /// If `false`, the broker refuses connections.
    pub down: bool,
    /// If `true`, the broker does not acknowledge a subscribe. It keeps the
    /// filters, but the `Subscribe` request waits, and ends with `Failed`
    /// when the connection closes.
    pub hold_suback: bool,
    retained: BTreeMap<String, Vec<u8>>,
    connection: Option<Connection>,
    /// The messages that the dashboard published, in order.
    pub published: Vec<Publish>,
    /// The service calls that Home Assistant got, in order.
    pub service_calls: Vec<ServiceCall>,
    /// The number of connections that the broker accepted.
    pub connections: usize,
    /// The topics of the messages that the broker sent to the dashboard.
    pub delivered: Vec<String>,
    /// The clock of Home Assistant, in seconds since the Unix epoch. Home
    /// Assistant publishes it as `last_updated` with each change. Each
    /// service call moves the clock one second forward.
    pub time: u64,
}

impl Broker {
    /// Returns `true` if the dashboard has an open connection.
    pub const fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Returns the topic filters of the open connection.
    pub fn filters(&self) -> &[String] {
        self.connection
            .as_ref()
            .map_or(&[], |connection| &connection.filters)
    }

    /// Returns the retained payload on `topic` as text.
    pub fn retained(&self, topic: &str) -> Option<&str> {
        self.retained
            .get(topic)
            .and_then(|payload| core::str::from_utf8(payload).ok())
    }

    /// Accepts or refuses a connection from the dashboard.
    pub fn connect(&mut self, mut request: Request<Connect>) {
        if self.down {
            let reason = Some("connection refused".to_string());
            request
                .resolve(MqttEvent::Disconnected { reason })
                .expect("the core listens to the connection");
            return;
        }
        self.close(Some("session taken over".to_string()));
        request
            .resolve(MqttEvent::Connected)
            .expect("the core listens to the connection");
        self.connections += 1;
        self.connection = Some(Connection {
            request,
            filters: Vec::new(),
            subscribes: Vec::new(),
        });
    }

    /// Adds topic filters to the connection and sends the retained messages
    /// that match them. Then answers the request with `Done`, unless
    /// [`Broker::hold_suback`] is `true`. With no open connection, answers
    /// `Failed`.
    pub fn subscribe(&mut self, mut request: Request<Subscribe>) {
        let Some(connection) = &mut self.connection else {
            request
                .resolve(SubscribeOutcome::Failed)
                .expect("the core waits for the subscribe");
            return;
        };
        for filter in &request.operation.topics {
            for (topic, payload) in &self.retained {
                if matches(filter, topic) {
                    deliver(connection, topic, payload);
                    self.delivered.push(topic.clone());
                }
            }
            connection.filters.push(filter.clone());
        }
        if self.hold_suback {
            connection.subscribes.push(request);
        } else {
            request
                .resolve(SubscribeOutcome::Done)
                .expect("the core waits for the subscribe");
        }
    }

    /// Closes the open connection, if there is one, and ends its stream
    /// with `reason`. The `Subscribe` requests that wait end with `Failed`.
    /// Returns the `Connect` request of the connection.
    fn close(&mut self, reason: Option<String>) -> Option<Request<Connect>> {
        let mut connection = self.connection.take()?;
        let _ = connection
            .request
            .resolve(MqttEvent::Disconnected { reason });
        for mut subscribe in connection.subscribes {
            let _ = subscribe.resolve(SubscribeOutcome::Failed);
        }
        Some(connection.request)
    }

    /// Takes a message from the dashboard.
    pub fn publish_from_dashboard(&mut self, publish: Publish) {
        self.publish(&publish.topic, &publish.payload, publish.retain);
        if publish.topic.ends_with("/cmd")
            && let Ok(call) = serde_json::from_slice::<ServiceCall>(&publish.payload)
        {
            // A service call comes after the state that the dashboard shows,
            // so its change has a later `last_updated` time.
            self.time += 1;
            self.home_assistant(&call);
            self.service_calls.push(call);
        }
        self.published.push(publish);
    }

    /// Closes the connection after a disconnect from the dashboard. The broker
    /// does not publish the last will.
    pub fn disconnect(&mut self) {
        self.close(None);
    }

    /// Stops the connection without a disconnect, as when the network fails.
    /// The broker publishes the last will.
    pub fn drop_connection(&mut self) {
        let Some(request) = self.close(Some("connection lost".to_string())) else {
            return;
        };
        let LastWill {
            topic,
            payload,
            retain,
        } = request.operation.last_will;
        self.publish(&topic, &payload, retain);
    }

    /// Publishes a message from another client. A retained message with an
    /// empty payload removes the retained message.
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) {
        if retain {
            if payload.is_empty() {
                self.retained.remove(topic);
            } else {
                self.retained.insert(topic.to_string(), payload.to_vec());
            }
        }
        if let Some(connection) = &mut self.connection
            && connection
                .filters
                .iter()
                .any(|filter| matches(filter, topic))
        {
            deliver(connection, topic, payload);
            self.delivered.push(topic.to_string());
        }
    }

    /// Publishes an entity state, the time of the update and the attributes,
    /// in this order, as Home Assistant does with `publish_timestamps: true`.
    ///
    /// The time of the update is [`Broker::time`], as ISO 8601 text. Each
    /// attribute value is JSON, for example `128` or `"°C"`.
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
        let base = format!("{STATESTREAM}/{domain}/{object_id}");
        self.publish(&format!("{base}/state"), state.as_bytes(), true);
        let updated = iso_time(self.time);
        self.publish(&format!("{base}/last_updated"), updated.as_bytes(), true);
        for (name, value) in attributes {
            self.publish(&format!("{base}/{name}"), value.as_bytes(), true);
        }
    }

    /// Returns the state of an entity, as Home Assistant published it.
    pub fn state(&self, entity_id: &str) -> Option<&str> {
        let (domain, object_id) = entity_id.split_once('.')?;
        self.retained(&format!("{STATESTREAM}/{domain}/{object_id}/state"))
    }

    /// Does a service call as Home Assistant does.
    fn home_assistant(&mut self, call: &ServiceCall) {
        if let ("light.turn_on", Some(step)) = (call.service.as_str(), call.brightness_step_pct) {
            self.step_brightness(&call.entity_id.clone(), step);
            return;
        }
        let Some((domain, action)) = call.service.split_once('.') else {
            return;
        };
        if action != "toggle" || !call.entity_id.starts_with(domain) {
            return;
        }
        let next = if self.state(&call.entity_id) == Some("on") {
            "off"
        } else {
            "on"
        };
        let entity_id = call.entity_id.clone();
        self.set_state(&entity_id, next, &[]);
    }

    /// Changes the brightness of a light by `step` percent, as
    /// `light.turn_on` with `brightness_step_pct` does. A light that is off
    /// has the brightness 0. If the brightness gets to 0, the light turns
    /// off.
    fn step_brightness(&mut self, entity_id: &str, step: i8) {
        let Some(object_id) = entity_id.strip_prefix("light.") else {
            return;
        };
        let topic = format!("{STATESTREAM}/light/{object_id}/brightness");
        let now = match self.state(entity_id) {
            Some("on") => self
                .retained(&topic)
                .and_then(|value| value.parse::<i32>().ok())
                .unwrap_or(255),
            _ => 0,
        };
        // Home Assistant rounds the step to the nearest brightness.
        let change = i32::from(step) * 255;
        let next = (now + (change + change.signum() * 50) / 100).clamp(0, 255);
        if next == 0 {
            self.set_state(entity_id, "off", &[("brightness", "null")]);
        } else {
            self.set_state(entity_id, "on", &[("brightness", &next.to_string())]);
        }
    }
}

/// Returns the time `seconds` after the Unix epoch as Python `isoformat()`
/// gives it, for example `2026-07-01T08:41:00+00:00`.
pub fn iso_time(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(seconds.cast_signed(), 0)
        .expect("a valid time")
        .to_rfc3339()
}

fn deliver(connection: &mut Connection, topic: &str, payload: &[u8]) {
    let message = Message {
        topic: topic.to_string(),
        payload: payload.to_vec(),
    };
    let _ = connection.request.resolve(MqttEvent::Message(message));
}

/// Returns `true` if the topic filter `filter` matches `topic`.
pub fn matches(filter: &str, topic: &str) -> bool {
    let mut topic_levels = topic.split('/');
    for level in filter.split('/') {
        match level {
            "#" => return true,
            "+" => {
                if topic_levels.next().is_none() {
                    return false;
                }
            }
            level => {
                if topic_levels.next() != Some(level) {
                    return false;
                }
            }
        }
    }
    topic_levels.next().is_none()
}

#[test]
fn topic_filters_match_as_in_mqtt() {
    assert!(matches("a/b/#", "a/b/c/d"));
    assert!(matches("a/b/#", "a/b"));
    assert!(matches("a/+/c", "a/b/c"));
    assert!(!matches("a/+/c", "a/b/d"));
    assert!(!matches("a/b", "a/b/c"));
    assert!(!matches("a/b/c", "a/b"));
}

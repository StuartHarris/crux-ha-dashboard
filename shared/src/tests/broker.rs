//! A fake MQTT broker with a fake Home Assistant behind it.
//!
//! The broker keeps retained messages, matches topic filters with the `+` and
//! `#` wildcards, and sends messages to the one client session through the
//! stream of its `Connect` request. It publishes the last will when a
//! connection stops without a disconnect.
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
    capabilities::mqtt::{Connect, LastWill, Message, MqttEvent, Publish},
    entities::STATESTREAM,
};

/// The connection of the dashboard to the broker.
struct Session {
    request: Request<Connect>,
    filters: Vec<String>,
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
    retained: BTreeMap<String, Vec<u8>>,
    session: Option<Session>,
    /// The messages that the dashboard published, in order.
    pub published: Vec<Publish>,
    /// The service calls that Home Assistant got, in order.
    pub service_calls: Vec<ServiceCall>,
    /// The number of connections that the broker accepted.
    pub connections: usize,
    /// The topics of the messages that the broker sent to the dashboard.
    pub delivered: Vec<String>,
}

impl Broker {
    /// Returns `true` if the dashboard has a session.
    pub const fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    /// Returns the topic filters of the session.
    pub fn filters(&self) -> &[String] {
        self.session
            .as_ref()
            .map_or(&[], |session| &session.filters)
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
        if let Some(mut old) = self.session.take() {
            let reason = Some("session taken over".to_string());
            let _ = old.request.resolve(MqttEvent::Disconnected { reason });
        }
        request
            .resolve(MqttEvent::Connected)
            .expect("the core listens to the connection");
        self.connections += 1;
        self.session = Some(Session {
            request,
            filters: Vec::new(),
        });
    }

    /// Adds topic filters to the session and sends the retained messages
    /// that match them.
    pub fn subscribe(&mut self, topics: &[String]) {
        let Some(session) = &mut self.session else {
            return;
        };
        for filter in topics {
            for (topic, payload) in &self.retained {
                if matches(filter, topic) {
                    deliver(session, topic, payload);
                    self.delivered.push(topic.clone());
                }
            }
            session.filters.push(filter.clone());
        }
    }

    /// Takes a message from the dashboard.
    pub fn publish_from_dashboard(&mut self, publish: Publish) {
        self.publish(&publish.topic, &publish.payload, publish.retain);
        if publish.topic.ends_with("/cmd")
            && let Ok(call) = serde_json::from_slice::<ServiceCall>(&publish.payload)
        {
            self.home_assistant(&call);
            self.service_calls.push(call);
        }
        self.published.push(publish);
    }

    /// Closes the session after a disconnect from the dashboard. The broker
    /// does not publish the last will.
    pub fn disconnect(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session
                .request
                .resolve(MqttEvent::Disconnected { reason: None });
        }
    }

    /// Stops the session without a disconnect, as when the network fails.
    /// The broker publishes the last will.
    pub fn drop_connection(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let LastWill {
            topic,
            payload,
            retain,
        } = session.request.operation.last_will.clone();
        let reason = Some("connection lost".to_string());
        let _ = session.request.resolve(MqttEvent::Disconnected { reason });
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
        if let Some(session) = &mut self.session
            && session.filters.iter().any(|filter| matches(filter, topic))
        {
            deliver(session, topic, payload);
            self.delivered.push(topic.to_string());
        }
    }

    /// Publishes an entity state and attributes as Home Assistant does.
    ///
    /// Each attribute value is JSON, for example `128` or `"°C"`.
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
        let base = format!("{STATESTREAM}/{domain}/{object_id}");
        self.publish(&format!("{base}/state"), state.as_bytes(), true);
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

fn deliver(session: &mut Session, topic: &str, payload: &[u8]) {
    let message = Message {
        topic: topic.to_string(),
        payload: payload.to_vec(),
    };
    let _ = session.request.resolve(MqttEvent::Message(message));
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

//! The fake Home Assistant behind the fake broker.
//!
//! Home Assistant publishes entity states as the `mqtt_statestream`
//! integration does, and answers service calls on `dashboard/<id>/cmd` as
//! the command automation does.

use alloc::{
    format,
    string::{String, ToString},
};

use serde::Deserialize;

use super::Broker;
use crate::{entities::STATESTREAM, mqtt::Publish};

/// A service call that the fake Home Assistant got.
#[derive(Deserialize, Debug, PartialEq)]
pub struct ServiceCall {
    /// The service, for example `light.toggle`.
    pub service: String,
    /// The entity of the call.
    pub entity_id: String,
    /// The change in brightness of a `light.turn_on` call, in percent.
    pub brightness_step_pct: Option<i8>,
    /// The target temperature of a `climate.set_temperature` call, in
    /// degrees.
    pub temperature: Option<f64>,
}

/// The lowest target temperature of a thermostat (`min_temp`).
pub const MIN_TEMP: f64 = 10.0;
/// The highest target temperature of a thermostat (`max_temp`).
pub const MAX_TEMP: f64 = 32.0;

impl Broker {
    /// Publishes an entity state, the time of the update and the attributes,
    /// in this order, as Home Assistant does with `publish_timestamps: true`.
    ///
    /// The time of the update is [`Broker::time`], as ISO 8601 text. Each
    /// attribute value is JSON, for example `128` or `"°C"`.
    ///
    /// # Panics
    ///
    /// Panics if `entity_id` has no domain.
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
    #[must_use]
    pub fn state(&self, entity_id: &str) -> Option<&str> {
        let (domain, object_id) = entity_id.split_once('.')?;
        self.retained(&format!("{STATESTREAM}/{domain}/{object_id}/state"))
    }

    /// If `publish` is a service call on a command topic, moves the clock
    /// one second forward, does the call and keeps it in
    /// [`Broker::service_calls`]. If [`Broker::ignore_calls`] is `true`,
    /// Home Assistant only keeps the call.
    pub(super) fn service_call(&mut self, publish: &Publish) {
        if !publish.topic.ends_with("/cmd") {
            return;
        }
        let Ok(call) = serde_json::from_slice::<ServiceCall>(&publish.payload) else {
            return;
        };
        // A service call comes after the state that the dashboard shows, so
        // its change has a later `last_updated` time.
        self.time += 1;
        if !self.ignore_calls {
            self.home_assistant(&call);
        }
        self.service_calls.push(call);
    }

    /// Does a service call as Home Assistant does.
    fn home_assistant(&mut self, call: &ServiceCall) {
        if let ("light.turn_on", Some(step)) = (call.service.as_str(), call.brightness_step_pct) {
            self.step_brightness(&call.entity_id, step);
            return;
        }
        if let ("climate.set_temperature", Some(target)) = (call.service.as_str(), call.temperature)
        {
            self.set_temperature(&call.entity_id, target);
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
        self.set_state(&call.entity_id, next, &[]);
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

    /// Sets the target temperature of a thermostat, as
    /// `climate.set_temperature` does through the command automation. The
    /// automation keeps the target from [`MIN_TEMP`] to [`MAX_TEMP`]. If the
    /// target does not change, Home Assistant publishes nothing.
    fn set_temperature(&mut self, entity_id: &str, target: f64) {
        let Some(object_id) = entity_id.strip_prefix("climate.") else {
            return;
        };
        let topic = format!("{STATESTREAM}/climate/{object_id}/temperature");
        // Home Assistant publishes a float attribute as JSON, for example
        // `20.0`.
        let value =
            serde_json::to_string(&target.clamp(MIN_TEMP, MAX_TEMP)).expect("a float as JSON");
        if self.retained(&topic) == Some(value.as_str()) {
            return;
        }
        let state = self.state(entity_id).unwrap_or("heat").to_string();
        self.set_state(entity_id, &state, &[("temperature", &value)]);
    }
}

/// Returns the time `seconds` after the Unix epoch as Python `isoformat()`
/// gives it, for example `2026-07-01T08:41:00+00:00`.
///
/// # Panics
///
/// Panics if the time is out of the range of `jiff`.
#[must_use]
pub fn iso_time(seconds: u64) -> String {
    jiff::Timestamp::from_second(seconds.cast_signed())
        .expect("a valid time")
        .display_with_offset(jiff::tz::Offset::UTC)
        .to_string()
}

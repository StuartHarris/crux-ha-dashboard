//! Tests of the app.
//!
//! The feature tests drive one step at a time with the `EffectTestExt`
//! helpers. The flow tests run the core in [`shell::Shell`], which answers the
//! effects with a fake MQTT broker, a fake Home Assistant, Wi-Fi, timers and a
//! clock.

mod broker;
mod connection;
mod flows;
mod power;
mod rooms;

mod shell;

use alloc::string::String;

use crux_time::Instant;

use crate::{PowerSource, Settings, Start, app::Event};

/// The time in the tests: 2026-07-01T08:41:00Z, which is 09:41 in BST.
const NOW: u64 = 1_782_895_260;
/// The UTC offset in the tests: BST.
const BST: i16 = 60;

fn settings() -> Settings {
    Settings {
        device_id: "hall".into(),
        host: "broker.local".into(),
        port: 1883,
        credentials: None,
    }
}

fn start(power: PowerSource) -> Event {
    start_at(power, None)
}

fn start_at(power: PowerSource, path: Option<&str>) -> Event {
    Event::Start(Start {
        settings: settings(),
        power,
        path: path.map(String::from),
        utc_offset_minutes: BST,
        last_sync: None,
    })
}

fn instant(seconds: u64) -> Instant {
    Instant::new(seconds, 0)
}

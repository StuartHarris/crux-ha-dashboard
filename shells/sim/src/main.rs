//! Runs the core on the desktop and writes the panel image to `panel.png`.
//!
//! The simulator has no MQTT client yet. It answers the effects itself: Wi-Fi
//! and the connection always work, and the broker sends a few fixed entity
//! states. It does not run timers.

use std::{collections::VecDeque, time::SystemTime};

use crux_core::Request;
use crux_time::Instant;
use embedded_graphics::{pixelcolor::Gray4, prelude::*};
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use shared::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start,
    capabilities::{
        mqtt::{Connect, Message, MqttEvent},
        network::WifiStatus,
    },
};

/// Retained state-stream messages, as Home Assistant publishes them.
const STATES: &[(&str, &str)] = &[
    ("light/living_room_lights/state", "on"),
    ("light/living_room_lights/brightness", "191"),
    ("light/living_room_rocket_lamp/state", "off"),
    ("light/living_room_standard_lamp/state", "unavailable"),
    ("sensor/living_room_thermometer_temperature/state", "21.4"),
    (
        "sensor/living_room_thermometer_temperature/unit_of_measurement",
        "\"°C\"",
    ),
    ("sensor/living_room_thermometer_humidity/state", "48"),
    (
        "sensor/living_room_thermometer_humidity/unit_of_measurement",
        "\"%\"",
    ),
];

fn main() {
    let core: Core<Dashboard> = Core::new();
    let mut display = SimulatorDisplay::<Gray4>::new(Size::new(ui::WIDTH, ui::HEIGHT));
    let mut connection: Option<Request<Connect>> = None;

    let start = Event::Start(Start {
        settings: Settings {
            device_id: "sim".into(),
            host: "localhost".into(),
            port: 1883,
            credentials: None,
        },
        power: PowerSource::Stand,
        path: Some("/room/living_room".into()),
        utc_offset_minutes: 0,
        last_sync: None,
    });
    let mut effects: VecDeque<Effect> = core.process_event(start).into();

    while let Some(effect) = effects.pop_front() {
        match effect {
            Effect::Render(_) => {
                let Ok(()) = ui::draw(&core.view(), &mut display);
            }
            Effect::WifiOn(mut request) => {
                effects.extend(resolve(&core, &mut request, WifiStatus::Connected));
            }
            Effect::MqttConnect(mut request) => {
                effects.extend(resolve(&core, &mut request, MqttEvent::Connected));
                connection = Some(request);
            }
            Effect::MqttSubscribe(_) => {
                let Some(request) = connection.as_mut() else {
                    continue;
                };
                for (topic, payload) in STATES {
                    let message = MqttEvent::Message(Message {
                        topic: format!("homeassistant/statestream/{topic}"),
                        payload: payload.as_bytes().to_vec(),
                    });
                    effects.extend(resolve(&core, request, message));
                }
            }
            Effect::TimeNow(mut request) => {
                let now = Instant::from(SystemTime::now());
                effects.extend(resolve(&core, &mut request, now));
            }
            Effect::TimeClear(mut request) => {
                // The answer to `ClearTimer` is the id of the timer.
                let id = request.operation.id;
                effects.extend(resolve(&core, &mut request, id));
            }
            Effect::MqttPublish(_)
            | Effect::MqttDisconnect(_)
            | Effect::WifiOff(_)
            | Effect::Sleep(_)
            // The simulator does not run timers yet, so it never answers them.
            | Effect::TimeNotifyAfter(_) => {}
        }
    }

    let image = display.to_grayscale_output_image(&OutputSettings::default());
    image
        .save_png("panel.png")
        .expect("could not write panel.png");
    println!("Wrote panel.png");
}

/// Gives the core the answer to a request, and returns the new effects.
fn resolve<Op>(core: &Core<Dashboard>, request: &mut Request<Op>, output: Op::Output) -> Vec<Effect>
where
    Op: crux_core::capability::Operation,
{
    core.resolve(request, output)
        .expect("the core waits for the answer")
}

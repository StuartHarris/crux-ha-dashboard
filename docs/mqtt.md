# MQTT topic contract

The dashboard and Home Assistant communicate only through the Mosquitto broker on the
Raspberry Pi (`192.168.1.109`, port 1883). Home Assistant publishes entity states. The
dashboard publishes service calls and its own status. The Pi setup is in
`RUNBOOK.md` in the `rpi` notes, in the sections "Mosquitto" and "MQTT for the
dashboard".

## Broker

- The broker is Mosquitto 2 with no TLS. It is on the LAN only.
- Anonymous clients cannot connect. Each client must give a user and a password.
- The dashboard uses the user `dashboard`. Home Assistant uses the user
  `homeassistant`.
- The passwords are only in `/srv/mosquitto/credentials` on the Pi (root, mode 0600).
  Do not put a password in this repo, in a command line or in a log.

## What Home Assistant publishes

The Home Assistant `mqtt_statestream` integration publishes each entity on the
`include` list in `configuration.yaml`. That list has the entities in
`shared/src/pages.rs` (tiles and scenes).

| Topic | Payload |
| --- | --- |
| `homeassistant/statestream/<domain>/<object_id>/state` | The state as plain text, for example `on`, `21.5` or `unavailable` |
| `homeassistant/statestream/<domain>/<object_id>/last_updated` | The time of the last update as plain ISO 8601 text from Python `datetime.isoformat()`, with no quotes, for example `2026-11-08T14:03:21.123456+00:00`. If the microseconds are 0, the text has none, for example `2026-11-08T14:03:21+00:00`. |
| `homeassistant/statestream/<domain>/<object_id>/last_changed` | The time of the last state change, in the same format. The dashboard does not subscribe to it. |
| `homeassistant/statestream/<domain>/<object_id>/<attribute>` | The attribute as JSON, for example `128`, `"°C"` or `null` |

For each change, the integration publishes `state`, then `last_updated`, then
`last_changed`, then each attribute.

- All messages are retained, with QoS 1. When the dashboard subscribes, the broker
  sends the last state of each entity immediately. A wake on battery gets the full
  state in one connection.
- The integration publishes all attributes of each entity. For each entity, the
  dashboard subscribes to `state`, to `last_updated`, and only to the attributes that
  its tile shows:

  | Tile kind | Keys |
  | --- | --- |
  | Light | `state`, `last_updated`, `brightness` |
  | Switch | `state`, `last_updated` |
  | Sensor | `state`, `last_updated`, `unit_of_measurement` |
  | Climate | `state`, `last_updated`, `current_temperature`, `temperature` |
  | Weather | `state`, `last_updated`, `temperature`, `temperature_unit` |

- The integration publishes timestamps (`publish_timestamps: true`). The footer of the
  dashboard shows "Last changed HH:MM": the latest `last_updated` time of the entities
  on the page, in UK time. The dashboard does not read its own clock for this time.
- The dashboard uses `last_updated` and not `last_changed`. Home Assistant moves
  `last_changed` only when the state changes. The current temperature of a thermostat,
  the temperature of the weather and the brightness of a light are attributes. A
  change to one of them moves `last_updated`, but not `last_changed`.
- The dashboard ignores a `last_updated` payload that is not valid ISO 8601 text. It
  keeps the time to the minute, because the footer shows only the minute.
- The messages of one change arrive a few milliseconds apart. The dashboard waits
  200 ms after a change that the page shows, and then draws all the changes once.
- The integration publishes an entity only when its state or attributes change. It
  does not publish the current states when Home Assistant starts. The automation
  `dashboard_statestream_seed` publishes them (refer to "Seed the state stream").
  The broker keeps retained messages in `/srv/mosquitto/data` across restarts.

The Home Assistant documentation does not say that the messages are retained. The
source does: each `mqtt.async_publish` call in
`homeassistant/components/mqtt_statestream/__init__.py` uses QoS 1 and `retain=True`.

If you add an entity to `pages.rs`, add it to the `include` list on the Pi too. Add it
to the list of entities in `dashboard_statestream_seed` too. Then restart Home
Assistant. If you do not, the tile shows `-`.

The `mqtt_statestream` configuration must have `publish_timestamps: true`. Without it,
the footer shows no time, or the time that the dashboard kept through a deep sleep.

Run `just ha-entities --statestream` to get the lines for the `include` list. Run
`just ha-entities` to get a bare list of the entities, to compare with the Pi.

### Seed the state stream

The Home Assistant automation `dashboard_statestream_seed` (Dashboard statestream seed)
publishes the current state of each entity on the `include` list. Without it, an entity
that did not change since Home Assistant started has no retained message, and its tile
shows `-`.

The automation runs at these times:

- When Home Assistant starts.
- When a dashboard publishes `online` to `dashboard/<device_id>/status`. Thus each
  dashboard gets the current states when it connects.

The automation publishes to the same topics, in the same format, as the integration.
The state is plain text. The automation also publishes `last_updated` of each entity
as plain ISO 8601 text, with no quotes, in the same format as the integration. Each
attribute that the dashboard reads is JSON (`brightness`, `unit_of_measurement`,
`current_temperature`, `temperature` and `temperature_unit`). The automation publishes
an attribute only if the entity has it. The messages are retained, with QoS 1. The
core cannot tell which of the two publishers sent a message.

The automation has its own list of entities. That list must be the same as the
`include` list. The `rpi` runbook has a command that compares the two lists.

Run `just ha-entities --seed` to get the `attributes` list and the `entities` list of
the automation. Run `just ha-entities` for a bare list to compare with the Pi.

## What the dashboard publishes

The dashboard uses the client id `dashboard-<device_id>` and a keep-alive of 60
seconds. `<device_id>` comes from `Settings::device_id`, for example `hall`.

### The connect and the subscribe

Each connect has a clean start, so the broker keeps no session for the dashboard.
After each connect, the dashboard publishes the service calls that wait, then
`online`, and then subscribes to all its exact topics. The broker sends the retained
state of each topic with the subscribe.

The `Subscribe` request of the core ends when the broker acknowledged each topic
filter. If the broker refuses a filter, or the connection closes first, the shell
answers `Failed`. After a `Done` answer, the core sends `WifiQuiet`. The firmware can
then scan for a better access point. A change of access point closes the MQTT
connection, and the core connects again.

The firmware does not wait for the TCP ACK of the broker after each packet (refer to
"MQTT transport" in `firmware/README.md`). Thus the 115 subscribe packets and the
acknowledgements of the retained states do not each wait for a round trip.

### Commands

| Topic | QoS | Retained |
| --- | --- | --- |
| `dashboard/<device_id>/cmd` | 1 | No |

The payload is a JSON object with a service and an entity id:

```json
{"service":"light.toggle","entity_id":"light.kitchen"}
```

A tap on the − or the + button of a dimmable light tile sends `light.turn_on` with
a brightness step in percent. The step is `-25` or `25`
(`BRIGHTNESS_STEP_PCT` in `shared/src/layout.rs`):

```json
{"service":"light.turn_on","entity_id":"light.kitchen","brightness_step_pct":25}
```

If a light is off, a + step turns it on at 25%. When the brightness gets to 0, Home
Assistant turns the light off. The dashboard sends no − step for a light that is off,
because Home Assistant does nothing for it. A tile shows the − and + buttons only
while its light is on or off, so the dashboard sends no step for a light that is
unavailable or has no state. A toggle payload and a
scene payload have no `brightness_step_pct` key.

The Home Assistant automation `dashboard_commands` (Dashboard commands) calls the
service. It calls the service only if all of these conditions are true:

- The payload is a JSON object.
- `service` is `light.toggle`, `switch.toggle`, `scene.turn_on` or `light.turn_on`.
- `entity_id` is one entity id, and its domain is the same as the domain of the
  service. For example, `light.toggle` with `switch.kettle` does nothing.
- For `light.turn_on`, the payload has `brightness_step_pct`, and its value is an
  integer from -100 to 100. The automation gives it to the service as `data`.
- The payload has no other keys.

The automation ignores a command that does not agree with these conditions. It sends
no reply. The dashboard sees the result as a new state on the state stream. The
automation runs in `queued` mode, so Home Assistant does each command in sequence.

Until the result comes back, the dashboard shows the tile or the scene button with a
pattern of black and white pixels. The dashboard removes the pattern when the state
stream confirms the call. For a toggle, the state changes, and `last_updated` is later
than the time at the tap. For a step, `last_updated` is later than the time at the
tap. Thus the state stream must publish `last_updated` (`publish_timestamps: true`).
If Home Assistant does not confirm the call in 5 seconds, the tile shows the last
state again. A call that changes nothing shows the pattern for these 5 seconds. A scene
has no state, so its button shows the pattern for 2 seconds. Refer to
`shared/src/features/pending.rs`.

### Status

| Topic | QoS | Retained |
| --- | --- | --- |
| `dashboard/<device_id>/status` | 1 | Yes |

| Payload | When |
| --- | --- |
| `online` | The dashboard publishes it after it connects. |
| `asleep` | The dashboard publishes it before it disconnects to sleep on battery. |
| `offline` | The broker publishes it as the last will if the connection stops without a disconnect. |

## Test from a shell

Run these commands on the Pi. Each command reads the password from the credentials
file into a variable, and the shell in the container expands that variable. The
password does not show in the command line or in your terminal history.

Watch the state stream and the dashboard topics:

```sh
sudo bash -c 'PW=$(awk -F: "\$1==\"dashboard\"{print \$2}" /srv/mosquitto/credentials)
  podman exec -e PW="$PW" mosquitto sh -c \
    "mosquitto_sub -u dashboard -P \"\$PW\" -v -t \"homeassistant/statestream/#\" -t \"dashboard/#\""'
```

Read the retained state of one light, then stop:

```sh
sudo bash -c 'PW=$(awk -F: "\$1==\"dashboard\"{print \$2}" /srv/mosquitto/credentials)
  podman exec -e PW="$PW" mosquitto sh -c \
    "mosquitto_sub -u dashboard -P \"\$PW\" -v -C 1 -W 5 -t homeassistant/statestream/light/hall_lights/state"'
```

Toggle a light as the dashboard `test` would:

```sh
sudo bash -c 'PW=$(awk -F: "\$1==\"dashboard\"{print \$2}" /srv/mosquitto/credentials)
  podman exec -e PW="$PW" mosquitto sh -c \
    "mosquitto_pub -u dashboard -P \"\$PW\" -q 1 -t dashboard/test/cmd \
      -m '\''{\"service\":\"light.toggle\",\"entity_id\":\"light.hall_lights\"}'\''"'
```

From another computer on the LAN, keep the password in a file that only you can read
(mode 0600). Then use the `-P` option with that file. Mosquitto 2 clients have no
option that reads a password from a file:

```sh
mosquitto_sub -h 192.168.1.109 -u dashboard -P "$(cat ~/.config/dashboard-mqtt-password)" \
  -v -t 'homeassistant/statestream/#'
```

With this command, the password is in the process list of that computer while
`mosquitto_sub` runs.

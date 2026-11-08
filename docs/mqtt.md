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
| `homeassistant/statestream/<domain>/<object_id>/<attribute>` | The attribute as JSON, for example `128`, `"°C"` or `null` |

- All messages are retained, with QoS 1. When the dashboard subscribes, the broker
  sends the last state of each entity immediately. A wake on battery gets the full
  state in one connection.
- The integration publishes all attributes of each entity. The dashboard subscribes
  only to the attributes that its tiles show:

  | Tile kind | Attributes |
  | --- | --- |
  | Light | `brightness` |
  | Switch | none |
  | Sensor | `unit_of_measurement` |
  | Climate | `current_temperature`, `temperature` |
  | Weather | `temperature`, `temperature_unit` |

- The integration does not publish timestamps (`publish_timestamps: false`).
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
The state is plain text. Each attribute that the dashboard reads is JSON (`brightness`,
`unit_of_measurement`, `current_temperature`, `temperature` and `temperature_unit`).
The automation publishes an attribute only if the entity has it. The messages are
retained, with QoS 1. The core cannot tell which of the two publishers sent a message.

The automation has its own list of entities. That list must be the same as the
`include` list. The `rpi` runbook has a command that compares the two lists.

Run `just ha-entities --seed` to get the `attributes` list and the `entities` list of
the automation. Run `just ha-entities` for a bare list to compare with the Pi.

## What the dashboard publishes

The dashboard uses the client id `dashboard-<device_id>` and a keep-alive of 60
seconds. `<device_id>` comes from `Settings::device_id`, for example `hall`.

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

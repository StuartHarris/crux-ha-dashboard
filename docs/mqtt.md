# MQTT topic contract

The dashboard and Home Assistant communicate only through the Mosquitto broker on the
Raspberry Pi (`192.168.1.109`, port 1883). Home Assistant publishes entity states. The
dashboard publishes service calls, its own status and its log. The Pi setup is in
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

- All messages are retained, with QoS 1. The dashboard subscribes at QoS 0, so the
  broker delivers each message at QoS 0, and the dashboard sends no PUBACK. When the
  dashboard subscribes, the broker sends the last state of each entity immediately. A
  wake on battery gets the full state in one connection.
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

The core runs MQTT 3.1.1 itself (`shared/src/mqtt/`) over a byte stream
(`shared/src/capabilities/stream.rs`). The shell only opens a TCP stream to the
broker, writes the bytes that the core gives it, and closes the stream. The shell
knows nothing of MQTT.

### The connect and the subscribe

Each connect uses a clean session (the term of MQTT 3.1.1), so the broker keeps no
session for the dashboard. After the CONNACK, the core sends one write that has these
parts, in this order:

1. The service calls that wait (the outbox).
2. The `online` status.
3. The SUBSCRIBE packets for all the topic filters.

The session packs the filters into SUBSCRIBE packets of up to 1024 bytes. The 115
filters need about 7 packets, 7.9 KiB in all. The broker sends the retained state of
each topic after it accepts the filter.

The `Subscribe` job of the session ends when the broker acknowledged each packet.
If the broker refused a filter, the outcome is `Refused`. If the stream closes
first, the outcome is `Failed`. After `Done` or `Refused`, the link is quiet, so the
core starts the roam scan of the Wi-Fi policy (refer to "Change the access
point" below). After `Refused`, the core also writes this line through the log
capability: `mqtt: the broker refused one or more topic filters, so some entities get
no state`. The line does not name the filter.

### Find a dead link

The core finds a link that stopped, and the shell has no part in it. The session has
these timers:

| Timer | Limit | What happens at the limit |
| --- | --- | --- |
| Ping | 45 s (three quarters of the keep-alive) | The session sends a PINGREQ packet. |
| PINGRESP | 10 s after the ping | The session ends. |
| Ack | 10 s | The session ends if a SUBACK or a PUBACK does not arrive. |
| Send | 10 s | The session ends if the shell does not confirm a write. |
| Silence | 120 s | The session ends if no bytes arrive. This timer is a backstop. |

When the session ends with an error, the core closes the stream and connects again
after a backoff and after the close of the stream.

The reason that a session ends also tells the Wi-Fi policy of the core whether the
link works:

| End | Meaning for the link |
| --- | --- |
| `Timeout` or `Transport` | The link failed. The core marks it suspect. |
| `ByHost` | The broker host ended the stream with a TCP reset or a FIN. This proves that traffic crosses the link while the link is up. The core keeps the association. |
| `Clean`, `Refused`, `Malformed` and the other ends | Not a failure of the link. |

A restart of the chip needs 3 joins that get no new address while the link is
suspect. A block by the router (the joins fail) and an outage of the broker never restart the chip. During an outage of the
broker, each connect ends `ByHost`, so each reconnect keeps the association.

### Close the stream

When the dashboard goes to sleep on battery, the core does these steps in order:

1. The core publishes `asleep`.
2. The core sends DISCONNECT.
3. The shell closes the stream and sends a TCP FIN.
4. The core reports that the dashboard is offline.

The core waits for the close of the stream before it reports offline. Thus the
broker has `asleep` and the DISCONNECT before the Wi-Fi turns off, and the last will
does not replace `asleep` with `offline`. If no stream is open, the core reports
offline at once.

After a session ends, the core does not open a new stream until the shell closed the
old stream. A new stream makes the shell abort the old stream, and an abort can make
the broker lose the DISCONNECT. Thus a tap during the close for a sleep wakes the
device, but the core connects only after the close.
If the shell does not answer the `Close` request in 6 seconds (`CLOSE_TIMEOUT`), the
core acts as if the answer arrived. Thus a shell cannot stop a reconnect or a sleep.

### Change the access point

When the link is quiet, the core can scan all the channels once for a better access
point. Each channel has a dwell of 50 to 120 ms, so a scan that finds 3 access points
takes about 1.4 s. If an access point is at least 10 dB stronger than the current
one, the core starts a roam:

1. The core sends DISCONNECT.
2. The core closes the stream, and the shell answers the `Close`.
3. The core joins the stronger access point.
4. The core opens a new stream and connects again.

The DISCONNECT is clean, so the broker does not publish the last will. Home Assistant
sees no `offline` during a roam, and the retained `online` stays. The device does not
publish `asleep`, because it does not sleep.

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

A tap on the − or the + button of a climate tile sends `climate.set_temperature` with
the new target temperature in degrees, as a JSON number:

```json
{"service":"climate.set_temperature","entity_id":"climate.dining_room_thermostat","temperature":20.0}
```

The new target is the next mark of 0.5 °C down or up (`STEP` in
`shared/src/temperature.rs`). For example, + changes 19.5 to 20.0 and 19.3 to 19.5,
and − changes 19.5 to 19.0 and 19.7 to 19.5. If the user taps again before Home
Assistant confirms the step, the next step starts from the target of the earlier step.
The dashboard does not know the limits of the thermostat, so it sends each step. The
automation keeps the target from `min_temp` to `max_temp` of the entity. A climate
tile shows the − and + buttons only while the thermostat has a target temperature and
its state is not `unavailable` or `unknown`.

The Home Assistant automation `dashboard_commands` (Dashboard commands) calls the
service. It calls the service only if all of these conditions are true:

- The payload is a JSON object.
- `service` is `light.toggle`, `switch.toggle`, `scene.turn_on`, `light.turn_on` or
  `climate.set_temperature`.
- `entity_id` is one entity id, and its domain is the same as the domain of the
  service. For example, `light.toggle` with `switch.kettle` does nothing.
- For `light.turn_on`, the payload has `brightness_step_pct`, and its value is an
  integer from -100 to 100. The automation gives it to the service as `data`.
- For `climate.set_temperature`, the payload has `temperature`, and its value is a
  number. The automation sets it to a value from `min_temp` to `max_temp` of the
  entity, and gives it to the service as `data`.
- The payload has no other keys.

The automation ignores a command that does not agree with these conditions. It sends
no reply. The dashboard sees the result as a new state on the state stream. The
automation runs in `queued` mode, so Home Assistant does each command in sequence.

Until the result comes back, the dashboard shows the tile or the scene button with a
pattern of black and white pixels. The dashboard removes the pattern when the state
stream confirms the call. For a toggle, the state changes, and `last_updated` is later
than the time at the tap. For a brightness step, `last_updated` is later than the
time at the tap. For a step of a thermostat, `last_updated` is later than the time at
the tap, and the `temperature` attribute is the target of the step. A change of the
current temperature alone does not confirm the step. Thus the state stream must publish `last_updated` (`publish_timestamps: true`).
If Home Assistant does not confirm the call in 5 seconds (20 seconds for a step of a
thermostat), the tile shows the last state again. A call that changes nothing shows the
pattern for this time. A thermostat gets more time, because a Nest thermostat reports
its new target through the Google cloud, and that can take more than 10 seconds. A scene
has no state, so its button shows the pattern for 2 seconds. Refer to
`shared/src/features/pending/mod.rs`.

### Status

| Topic | QoS | Retained |
| --- | --- | --- |
| `dashboard/<device_id>/status` | 1 | Yes |

| Payload | When |
| --- | --- |
| `online` | The dashboard publishes it after it connects. |
| `asleep` | The dashboard publishes it before it sends DISCONNECT to sleep on battery. |
| `offline` | The broker publishes it as the last will if the connection stops without a DISCONNECT, for example when the link fails or the power fails. A roam does not do this. |

### Log

The core keeps its lines of the log in a ring of 64 lines, with the time of each line
on the monotonic clock of the shell. Refer to `shared/src/features/logs/mod.rs`. The
dashboard publishes each line to Home Assistant.

| Topic | QoS | Retained |
| --- | --- | --- |
| `dashboard/<device_id>/log` | 1 | No |

Each message is one line as JSON:

```json
{"level":"warn","source":"wifi","age":12,"msg":"the scan failed, so stay"}
```

| Key | Value |
| --- | --- |
| `level` | `info` or `warn` |
| `source` | The part of the core that wrote the line: `wifi`, `mqtt` or `power`. The drop message has `log`. |
| `age` | The age of the line in whole seconds when the dashboard publishes it |
| `msg` | The text of the line, with at most 200 bytes. A longer text ends with `…`. |

The dashboard has no wall clock. Home Assistant gets the time of the line as the time
when the message arrives, minus `age`. The clock of the device counts through a light
sleep, with a drift of about 1 to 2 %.

The time in Home Assistant can be a little late:

- The core reads the clock of the shell once after each connect. A line that waits
  behind the limit of publishes (refer to the delivery rules below) gets its age from
  that time, so its age is short by the time that it waited. A line that is newer
  than that time gets the age 0.
- `age` is in whole seconds, so the core rounds it down.
- Home Assistant reads `now()` when a run of the automation starts, not when the
  message arrives. A queued run starts later, usually by some milliseconds.

When the dashboard publishes:

- After each connect, the core asks the shell for the time on its clock. Then it
  publishes the lines that wait, from the oldest. The lines go in a write after the
  CONNACK write, never in it.
- While the connection is up, a new line goes at once, with the age 0.
- From the start of a sleep, while the device sleeps, while it is offline, and while
  the connection closes for a change of access point, the lines wait in the ring. Thus
  the lines of a battery sleep go after the next wake. The connection can still be up
  in the first step of a sleep, but a line from that step waits too. Its PUBACK would
  race the DISCONNECT, so the line would often go two times.

Delivery is at least once:

- A line leaves the ring only when the broker sends its PUBACK.
- If the session ends before the PUBACK, the line goes again on the next connection,
  in the same order. Thus Home Assistant can get a line two times.
- At most 56 messages of the log wait for a PUBACK at a time. The session takes 64, so
  the log always leaves 8 places for the status and the service calls
  (`logs::RESERVED`). If the log is at its limit, or the session takes no more
  publishes, the next line waits for the next PUBACK.

If the ring is full, a new line removes the oldest line, and the core counts the
removed lines. Before the next line, the dashboard then publishes the drop message:

```json
{"level":"warn","source":"log","age":0,"msg":"3 older lines were dropped"}
```

For one line, the message is `1 older line was dropped`.

The core subtracts the count of the drop message when the broker acknowledges it. The
core writes no line about the publish of its own log.

#### The Home Assistant automation

The automation `dashboard_log` writes the lines to the Home Assistant log.

- The trigger is an MQTT message on `dashboard/+/log`.
- The automation has `mode: queued` and `max: 100`. After a connect, the dashboard
  can send up to 56 lines at once. The default queue of 10 runs would drop the other
  lines, and the broker has acknowledged them already, so the dashboard does not send
  them again.
- The message is `[HH:MM:SS] <source>: <msg>`. The time is the time when the message
  arrives, minus `age`, in local time.
- A `warn` line calls `system_log.write` with the logger `dashboard.<device_id>` and
  the level `warning`.
- An `info` line also calls `system_log.write` with the logger
  `dashboard.<device_id>` and the level `warning`, with `[info] ` before the
  message. The Home Assistant log keeps only the level `warning` and higher. A later
  PR moves the `info` lines to `logbook.log` on the status entity of the device.

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

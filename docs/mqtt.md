# MQTT topic contract

The dashboard and Home Assistant communicate only through the Mosquitto broker on the
Raspberry Pi (`192.168.1.109`, port 1883). Home Assistant publishes entity states. The
dashboard publishes service calls, its own status, its log, and its discovery and state
as a device of Home Assistant. The Pi setup is in
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
3. The SUBSCRIBE packets for all the topic filters: the topics of the entities, then
   `homeassistant/status`, and then `dashboard/<device_id>/set/+` (refer to "Home
   Assistant device" below).

The session packs the filters into SUBSCRIBE packets of up to 1024 bytes. The 117
filters need about 7 packets, 7.9 KiB in all. The broker sends the retained state of
each topic after it accepts the filter.

The write has nothing else. The device publishes (the discovery message, the state
message and the lines of the log) go in later writes. If the CONNACK arrives before
the shell confirms the CONNECT write, the CONNACK write waits, and the device
publishes wait behind it.

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
core acts as if the answer arrived. Thus a shell cannot stop a reconnect, a sleep or a
restart.

A restart that Home Assistant asks for (the Restart button) closes the stream cleanly
too. The core does these steps in order:

1. The core publishes `restarting` (retained, QoS 1). If the session refuses the
   publish, the restart goes on.
2. The core sends DISCONNECT.
3. The shell closes the stream.
4. The core asks the shell to restart the chip.

The broker keeps `restarting`, and it does not publish the last will. If the chip does
not start again (for example, a panic at boot or a flat battery), Home Assistant shows
`restarting`, not `online`. The core publishes `restarting` only if the connection is up.
If the session waits for its CONNACK, or the stream already closes for a sleep or a
roam, the core publishes nothing. The retained status then stays as it was. With no
stream, the chip restarts at once.

While the restart waits, it wins over a tap, a wake and a sleep. The core does not
connect again, and a sleep stops before the radio turns off. The sleep keeps the calls
in the outbox and the pending marks, because the restart clears them. The core draws
nothing until the chip restarts, so the reset cannot stop a refresh with the panel half
drawn. Refer to `close_for_restart` in `shared/src/features/connection/mod.rs`.

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
| `restarting` | The dashboard publishes it before it sends DISCONNECT to restart the chip, when Home Assistant asks (the Restart button). The DISCONNECT is clean, so the broker keeps `restarting`. If the chip does not start again, Home Assistant shows `restarting`, not `online`. |
| `offline` | The broker publishes it as the last will if the connection stops without a DISCONNECT, for example when the link fails or the power fails. A roam and a restart from Home Assistant do not do this. |

The automation `dashboard_statestream_seed` starts only on `online` (refer to "Seed the
state stream"). Thus `asleep` and `restarting` do not start it.

### Log

The core keeps its `info` and `warn` lines of the log in a ring of 64 lines, with the
time of each line on the monotonic clock of the shell. The ring also keeps the `warn`
and `error` lines of the shell, which the shell sends as `Event::ShellLogged`. Refer
to `shared/src/features/logs/mod.rs`. The dashboard publishes each line to Home
Assistant.

The core writes the usual steps at the level `debug`, for example each step of a
sleep, and a scan after a wake that finds no better access point. The shell prints a
`debug` line (the firmware on the serial port), but the core does not keep it. So a
`debug` line never goes to Home Assistant, and a usual sleep sends no line. Only
unusual decisions and failures go, for example:

- `power: sleep 3 woke with no cause, so it sleeps again after 3 s` (`info`), for the
  first wake with no cause in a sleep. If more than one came, the next real wake writes
  `power: sleep 3 had 5 wakes with no cause` (`info`).
- `mqtt: the broker host ended the stream: connection reset, so connect again in 4 s`
  (`warn`), one line for each failed session, with the reason and the wait. During
  the steps of a sleep, the line ends with `, during the sleep`, because the sleep
  does not connect again.
- `mqtt: the link is down, so connect again in 2 s` (`info`), after a Wi-Fi warning
  that gives the reason.
- `mqtt: the close of connection 7 got no answer in 6 s` (`warn`).

| Topic | QoS | Retained |
| --- | --- | --- |
| `dashboard/<device_id>/log` | 1 | No |

Each message is one line as JSON:

```json
{"level":"warn","source":"wifi","age":12,"msg":"the scan failed, so stay"}
```

| Key | Value |
| --- | --- |
| `level` | `info` or `warn` for a line of the core. `warn` or `error` for a line of the shell. Never `debug`. |
| `source` | For a line of the core, the part of the core that wrote the line: `wifi`, `mqtt`, `power` or `device`. For a line of the shell, the source that the shell gives, with at most 32 bytes. The drop message has `log`. A line from before a restart has `boot`. |
| `age` | The age of the line in whole seconds when the dashboard publishes it. A line from before a restart has the age 0, because its time is not known. |
| `msg` | The text of the line, with at most 200 bytes. A longer text ends with `…`. |

The firmware sends its own `warn` and `error` lines to the core, also the lines of
esp-radio. It sends them also if `ESP_LOG` does not print them. It does not send a
line that it printed for the core. It starts to send lines just after
`board::init()`, and a line before that only prints. The serial log does not
change. A line of the firmware has the time when the firmware wrote it, so
a line from an outage or a sleep goes after the next connect with its age:

```json
{"level":"warn","source":"power","age":40,"msg":"cannot read the BQ27220 (Nack)"}
```

The firmware writes the failure of a stream at `info`, because the core writes one
`warn` line with the reason. So the stream lines (`stream: 10 failed: ...`) print on
the serial port, but they do not go to Home Assistant.

- For a line of the firmware itself (the crates `dashboard` and
  `dashboard_firmware`), if the line starts with a lowercase word and `": "`, the
  word is the source and the rest is the message. For example,
  `stream: 10 failed: ...` gives the source `stream`.
- Otherwise, and for each line of another crate, the source is the crate of the
  log target, for example `esp_radio`, and the message is the whole line.
- The firmware keeps at most 16 lines for the core task. If more lines come before
  the core task takes them, the firmware drops the new lines, and the drop message
  counts them. Refer to `firmware/logic/src/forward.rs`.

Each start of the core writes one line with the reason of the last reset and the
restart count (refer to "The state message" below). A panic, a watchdog reset or a
brownout gives `warn`. The other resets give `info`:

```json
{"level":"info","source":"power","age":0,"msg":"started after a power-on"}
{"level":"warn","source":"power","age":0,"msg":"started after a watchdog reset, restart 3"}
```

The log of the core is in RAM, so a chip restart loses it. The firmware keeps its last
8 `warn` and `error` lines in RTC memory, which a reset keeps, also the lines of the
core. After a reset that is not a power-on, the firmware gives them back at the start.
The core puts them first in its log, before the start line, with the source `boot` and
the text `before the restart: <source>: <message>`. The time of such a line is not
known on the new clock, so its age is 0, and Home Assistant shows it at the time of
the message. After a power-on, the firmware gives no lines, because the RTC memory
loses its content. Refer to `firmware/README.md`, "The restart facts".

```json
{"level":"warn","source":"boot","age":0,"msg":"before the restart: wifi: restart the chip: no address after 3 joins"}
```

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
  the lines from a battery sleep go after the next wake. The connection can still be up
  in the first step of a sleep, but a line from that step waits too. Its PUBACK would
  race the DISCONNECT, so the line would often go two times.

Delivery is at least once:

- A line leaves the ring only when the broker sends its PUBACK.
- If the session ends before the PUBACK, the line goes again on the next connection,
  in the same order. Thus Home Assistant can get a line two times.
- The log shares one limit with the discovery and the state messages (refer to "Home Assistant
  device" below). At most 56 of these messages wait for a PUBACK at a
  time. The session takes 64, so they always leave 8 places for the status and the
  service calls (`device::RESERVED`). If the queue is at its limit, or the session
  takes no more publishes, the next line waits for the next PUBACK.

If the ring is full, a new line removes the oldest line, and the core counts the
removed lines. Before the next line, the dashboard then publishes the drop message:

```json
{"level":"warn","source":"log","age":0,"msg":"3 older lines were dropped"}
```

For one line, the message is `1 older line was dropped`. The count also includes
the lines that the shell lost before they came to the core.

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
- An `error` line calls `system_log.write` with the logger `dashboard.<device_id>`
  and the level `error`.
- An `info` line calls `logbook.log` with the name `Dashboard <device_id>` and the
  entity `sensor.dashboard_<device_id>_status`. The line shows on the page of the
  device and in the Logbook. The Home Assistant system log keeps only the level
  `warning` and higher, so it is not the place for `info` lines. The Logbook stamps
  each entry with the time when it arrives, so the real time of a line is the time in
  the message.

### Home Assistant device

The dashboard tells Home Assistant about itself with MQTT discovery. Home Assistant
then shows the dashboard as a device with eighteen entities: the status, the charge of
the battery, whether the dashboard is on the stand, five power diagnostics (charging,
the charge state, and the voltage, the current and the temperature of the battery),
seven Wi-Fi and boot diagnostics (the signal, the access point, the IP address,
the reconnects, the restarts, the last reset and the wake reason), and three buttons
(Sleep now, Restart and Full refresh). Refer to
`shared/src/features/device/`. The core gets a new signal only at a join or a scan,
so on the stand the Wi-Fi signal entity can show a value that is hours or days old.

| Topic | Direction | QoS | Retained |
| --- | --- | --- | --- |
| `homeassistant/device/dashboard_<device_id>/config` | Dashboard to Home Assistant | 1 | Yes |
| `dashboard/<device_id>/state` | Dashboard to Home Assistant | 1 | Yes |
| `dashboard/<device_id>/status` | Dashboard to Home Assistant (refer to "Status") | 1 | Yes |
| `homeassistant/status` | Home Assistant to the dashboard | 0 (subscribe) | No |
| `dashboard/<device_id>/set/button` | Home Assistant to the dashboard (refer to "Buttons") | 0 | No |

#### The discovery message

The discovery message is one device-based discovery config of Home Assistant
(2024.11 and later), with abbreviated keys. It has 6,283 bytes for the device id
`hall` (the limit in the tests is 8 KiB, also for a device id of 16 characters). The
discovery and the state message go in one write, which has about 6.5 KB of the 16 KiB
that a write can have (`MAX_OUTGOING`).
For example, for the device id `hall`:

```json
{
  "dev": {
    "ids": "dashboard_hall",
    "name": "Dashboard hall",
    "mf": "LilyGo",
    "mdl": "T5 E-Paper S3 Pro",
    "sw": "0.1.0+1b073946",
    "cns": [["mac", "28:84:85:37:b8:a0"]]
  },
  "o": {"name": "crux-ha-dashboard", "sw": "0.1.0+1b073946"},
  "cmps": {
    "status": {
      "p": "sensor", "name": "Status", "uniq_id": "dashboard_hall_status",
      "dev_cla": "enum", "ic": "mdi:tablet-dashboard",
      "ops": ["online", "asleep", "offline", "restarting"],
      "stat_t": "dashboard/hall/status"
    },
    "battery": {
      "p": "sensor", "name": "Battery", "uniq_id": "dashboard_hall_battery",
      "dev_cla": "battery", "unit_of_meas": "%", "stat_cla": "measurement",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.battery }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}"
    },
    "stand": {
      "p": "binary_sensor", "name": "On stand", "uniq_id": "dashboard_hall_stand",
      "dev_cla": "plug",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.stand }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}"
    },
    "charging": {
      "p": "binary_sensor", "name": "Charging", "uniq_id": "dashboard_hall_charging",
      "dev_cla": "battery_charging",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.charging }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "charge": {
      "p": "sensor", "name": "Charge state", "uniq_id": "dashboard_hall_charge",
      "dev_cla": "enum", "ic": "mdi:battery-charging-outline",
      "ops": ["Not charging", "Pre-charging", "Fast charging", "Done"],
      "stat_t": "dashboard/hall/state",
      "val_tpl": "{{ {'not_charging': 'Not charging', 'pre_charging': 'Pre-charging', 'fast_charging': 'Fast charging', 'done': 'Done'}[value_json.charge] | default(None) }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "voltage": {
      "p": "sensor", "name": "Battery voltage", "uniq_id": "dashboard_hall_voltage",
      "dev_cla": "voltage", "ic": "mdi:flash-triangle-outline",
      "unit_of_meas": "mV", "stat_cla": "measurement",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.voltage }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "current": {
      "p": "sensor", "name": "Battery current", "uniq_id": "dashboard_hall_current",
      "dev_cla": "current", "ic": "mdi:current-dc",
      "unit_of_meas": "mA", "stat_cla": "measurement",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.current }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "temperature": {
      "p": "sensor", "name": "Gauge temperature", "uniq_id": "dashboard_hall_temperature",
      "dev_cla": "temperature", "unit_of_meas": "°C", "stat_cla": "measurement",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.temperature }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "rssi": {
      "p": "sensor", "name": "Wi-Fi signal", "uniq_id": "dashboard_hall_rssi",
      "dev_cla": "signal_strength", "unit_of_meas": "dBm", "stat_cla": "measurement",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.rssi }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "bssid": {
      "p": "sensor", "name": "Access point", "uniq_id": "dashboard_hall_bssid",
      "ic": "mdi:access-point",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.bssid }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "ip": {
      "p": "sensor", "name": "IP address", "uniq_id": "dashboard_hall_ip",
      "ic": "mdi:ip-network",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.ip }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "reconnects": {
      "p": "sensor", "name": "Reconnects", "uniq_id": "dashboard_hall_reconnects",
      "ic": "mdi:lan-connect", "stat_cla": "total_increasing",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.reconnects }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "restarts": {
      "p": "sensor", "name": "Restarts", "uniq_id": "dashboard_hall_restarts",
      "ic": "mdi:restart", "stat_cla": "total_increasing",
      "stat_t": "dashboard/hall/state", "val_tpl": "{{ value_json.restarts }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "reset": {
      "p": "sensor", "name": "Last reset", "uniq_id": "dashboard_hall_reset",
      "dev_cla": "enum", "ic": "mdi:power-cycle",
      "ops": ["Power on", "Software", "Panic", "Watchdog", "Brownout", "Other"],
      "stat_t": "dashboard/hall/state",
      "val_tpl": "{{ {'power_on': 'Power on', 'software': 'Software', 'panic': 'Panic', 'watchdog': 'Watchdog', 'brownout': 'Brownout', 'other': 'Other'}[value_json.reset] | default(None) }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "wake": {
      "p": "sensor", "name": "Wake reason", "uniq_id": "dashboard_hall_wake",
      "dev_cla": "enum", "ic": "mdi:sleep-off", "ops": ["Boot", "Touch", "Stand"],
      "stat_t": "dashboard/hall/state",
      "val_tpl": "{{ {'boot': 'Boot', 'touch': 'Touch', 'stand': 'Stand'}[value_json.wake] | default(None) }}",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'offline' if value == 'offline' else 'online' }}",
      "ent_cat": "diagnostic"
    },
    "sleep": {
      "p": "button", "name": "Sleep now", "uniq_id": "dashboard_hall_sleep",
      "ic": "mdi:sleep",
      "cmd_t": "dashboard/hall/set/button", "pl_prs": "sleep",
      "avty": [
        {"t": "dashboard/hall/status", "val_tpl": "{{ 'online' if value == 'online' else 'offline' }}"},
        {"t": "dashboard/hall/state", "val_tpl": "{{ 'online' if value_json.stand == 'OFF' else 'offline' }}"}
      ],
      "avty_mode": "all"
    },
    "restart": {
      "p": "button", "name": "Restart", "uniq_id": "dashboard_hall_restart",
      "dev_cla": "restart", "ent_cat": "config",
      "cmd_t": "dashboard/hall/set/button", "pl_prs": "restart",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'online' if value == 'online' else 'offline' }}"
    },
    "refresh": {
      "p": "button", "name": "Full refresh", "uniq_id": "dashboard_hall_refresh",
      "ic": "mdi:refresh", "ent_cat": "config",
      "cmd_t": "dashboard/hall/set/button", "pl_prs": "refresh",
      "avty_t": "dashboard/hall/status",
      "avty_tpl": "{{ 'online' if value == 'online' else 'offline' }}"
    }
  }
}
```

The message on the broker has no spaces or new lines.

- `sw` is the version of the firmware: the version of the package and a short commit
  id, for example `0.1.0+1b073946`, or `0.1.0+1b073946.dirty` for a build with edits
  that are not in a commit. Refer to `firmware/README.md`. The simulator gives
  `<version>+sim`.
- `cns` has the MAC address of the Wi-Fi station interface. The simulator gives
  `02:00:00:00:00:01`.
- The config has no shared options at its root. Each component has its own state topic
  and availability, because a shared availability would apply to the status sensor too.
- A button has a command topic (`cmd_t`) and the payload of a press (`pl_prs`), and no
  state. Restart has the device class `restart`. Restart and Full refresh have the
  entity category `config`, so Home Assistant shows them in the "Configuration" part of
  the device page. Sleep now has no category, so it is a control.
- The battery, stand, charging, temperature and signal components keep the default
  icon of their device class. The other components set an icon with `ic`, because
  they have no device class or the icon of the class does not tell what they are.
- The state message keeps the snake_case values of `charge` (`not_charging`,
  `pre_charging`, `fast_charging`, `done`). The `charge` component has the options
  `Not charging`, `Pre-charging`, `Fast charging` and `Done`. Its value template maps
  each wire value to its label. If `charge` is null or unknown, the template gives
  `None`, and Home Assistant shows the state as unknown. The `reset` and `wake`
  components map their values to their labels in the same way.
- `reconnects` and `restarts` have the state class `total_increasing`. Each count goes
  to 0 at a new cycle: `reconnects` at each start of the core, and `restarts` at each
  power-on. Home Assistant then starts a new cycle of the total.
- The power diagnostics and the Wi-Fi and boot diagnostics have the entity category
  `diagnostic`, so Home Assistant shows them in the "Diagnostic" part of the device
  page.
- The voltage is in mV, not in V. The dashboard then sends a whole number, so the
  payload is exact. Home Assistant converts the units of a voltage sensor, so you can
  set the display unit to V in the settings of the entity.

The dashboard publishes the discovery message at these times:

- Once after each `Start` of the core, on the first connection. A wake from a sleep on
  battery is not a `Start`, so it sends no discovery. The message is retained, so the
  broker keeps it.
- Again when Home Assistant publishes its birth message: `online` on
  `homeassistant/status`, not retained. Home Assistant publishes it when it starts. The
  dashboard ignores a retained birth message, because it is from an earlier start.
  If the discovery message waits for its PUBACK, a birth message changes nothing. The
  message in flight is retained, so Home Assistant gets it when it subscribes.
- Again on the next connection, if the session ended before the PUBACK.

During a dry run (`read_only = true` in `cfg.toml`, or the dry run that the user
starts), the dashboard publishes no discovery message and no state message. The
bring-up binary of the firmware uses a dry run, so it does not appear in Home
Assistant as a device. The log still goes. When the dry run stops, the dashboard
publishes the discovery message (if it did not send it yet) and the state.

#### The state message

The state message has the values of all the sensors except the status as one JSON
object:

```json
{"battery":95,"stand":"OFF","charging":"OFF","charge":"not_charging","voltage":3870,"current":-40,"temperature":24.5,"rssi":-62,"bssid":"24:5a:4c:01:02:03","ip":"192.168.1.50","reconnects":0,"restarts":0,"reset":"power_on","wake":"boot"}
```

| Key | Value |
| --- | --- |
| `battery` | The charge of the battery in percent, from 0 to 100. |
| `stand` | `ON` if the dashboard is on the stand, `OFF` if it is on battery. These are the default payloads of a binary sensor. |
| `charging` | `ON` during a pre-charge or a fast charge, else `OFF`. |
| `charge` | The phase of the charge from the charger: `not_charging`, `pre_charging`, `fast_charging` or `done`. |
| `voltage` | The voltage of the battery in mV, rounded to 10 mV. |
| `current` | The average current of the battery in mA, rounded to 10 mA. A positive current charges the battery, and a negative current discharges it. A check on the board is still to do. |
| `temperature` | The temperature of the fuel gauge in °C, rounded to 0.5 °C. |
| `rssi` | The signal of the access point in dBm, from the join or from the last scan that saw the access point. On the stand, the core scans only after a join with the cached access point, so the value can be hours old. |
| `bssid` | The BSSID of the access point, as lowercase hex with colons. |
| `ip` | The IPv4 address from DHCP. |
| `reconnects` | The number of MQTT sessions since the start of the core that failed while the dashboard was awake, so that the core connected again. A link that fails before a session and a sleep do not count. |
| `restarts` | The number of resets that were not a power-on, since the last power-on. A press of RST is a power-on. A flash or a serial monitor is not, so each one adds 1. Refer to "Which resets are a power-on" in `firmware/README.md`. The simulator gives 0. |
| `reset` | The reason of the last reset: `power_on`, `software`, `panic`, `watchdog`, `brownout` or `other`. The simulator gives `power_on`. |
| `wake` | What woke the dashboard last: `boot` (a start, with no sleep since), `touch` or `stand`. The check of the power source every 60 seconds and a wake with no cause keep the dashboard asleep, so they do not change it. |

A value is `null` if the shell did not read it, and `rssi`, `bssid` and `ip` are `null`
with no link. Home Assistant then shows the entity as unknown. The shell keeps the last good value of each diagnostic after a read that fails,
so a value is `null` only until its first good read. The simulator gives fixed values
for each power source.

The message has about 240 bytes. The dashboard publishes it at these times:

- Once on each connection.
- At once after an urgent change, while the connection is up. An urgent change is a
  move on or off the stand, a start or an end of a charge (`charging`), the first level
  of the battery, a level of the battery that moves by more than 1%, and a change of
  `ip`, `bssid`, `restarts`, `reset` or `wake`.
- At most once in 60 seconds for each other change: the voltage, the current, the
  temperature, the charge state while `charging` stays the same, a level of the
  battery that moves by 1%, `rssi` and `reconnects`. The gauge can flap between two levels, so a move of 1% is
  not urgent. The first of these changes goes at once and starts a timer of 60 seconds.
  The changes in that time wait. At the end of the time, the dashboard publishes the
  values of that time, if they changed, and starts a new timer. An urgent change in that
  time goes at once with the values that wait.
- At most one state message waits for a PUBACK. If a value changes while one waits,
  the dashboard publishes the new values after the PUBACK.
- The timer ends with the connection. Each sleep closes the connection first, so the
  timer never waits during a sleep. The next connection publishes the state at once.

#### Availability

| Entity | Available |
| --- | --- |
| Status | Always. It shows `online`, `asleep`, `offline` or `restarting` from the status topic. |
| Battery, On stand, the power diagnostics, and the Wi-Fi and boot diagnostics | Unless the status is `offline`. While the dashboard is asleep or restarts, they keep their last value. |
| Restart, Full refresh | Only while the status is `online`. |
| Sleep now | Only while the status is `online` and the dashboard is on battery (`stand` is `OFF`). On the stand, a sleep does nothing. |

Home Assistant compares the result of an availability template with `online` and
`offline`, and it ignores each other result. So the template of a button must turn
`asleep` and `restarting` into `offline`:
`{{ 'online' if value == 'online' else 'offline' }}`. Each status gives these results:

| Status | Sensor template | Button template |
| --- | --- | --- |
| `online` | `online` | `online` |
| `asleep` | `online` | `offline` |
| `offline` | `offline` | `offline` |
| `restarting` | `online` | `offline` |
 Sleep now
has two availabilities (`avty`) with `avty_mode: all`, so both must give `online`.

#### Buttons

| Topic | Direction | QoS | Retained |
| --- | --- | --- | --- |
| `dashboard/<device_id>/set/button` | Home Assistant to the dashboard | 0 | No |

A press publishes one of these payloads:

| Payload | Button | What the dashboard does |
| --- | --- | --- |
| `sleep` | Sleep now | On battery, the dashboard sleeps at once, as at the end of the idle time. If pending marks are open, the sleep waits for them. If calls wait in the outbox, it first waits one more idle period. If a sleep already waits for the pending marks, it does nothing more. Each of these writes a `debug` line that tells what the dashboard did. On the stand, it stays awake and writes the `info` line `device: Home Assistant asked to sleep, but the device is on the stand, so it stays awake`. While a restart waits, it does nothing and writes a `debug` line. |
| `restart` | Restart | The dashboard publishes `restarting`, closes the connection cleanly, and then restarts the chip (refer to "Close the stream"). The firmware writes the warning `device: restart the chip: Home Assistant asked`. That line goes to Home Assistant after the restart, with the source `boot`, and `restarts` goes up by 1. |
| `refresh` | Full refresh | The dashboard does a full refresh of the panel, which clears ghost images. The panel flashes. While the dashboard sleeps, it does nothing, because the last image before a sleep is a full refresh. While a restart waits, it does nothing and writes a `debug` line. |

- The dashboard subscribes to `dashboard/<device_id>/set/+`. It never publishes on a
  `set` topic, so the filter brings back none of its own messages. It ignores a message
  on another `set` topic.
- The dashboard ignores a retained button message, because a retained `restart` would
  restart the chip at each connect. It writes the `info` line `device: the button message
  on dashboard/<device_id>/set/button is retained, so the core ignored it`. To clear
  such a message, publish an empty retained message on the topic.
- The dashboard ignores a payload that it does not know, and writes a `warn` line with
  at most 32 bytes of the payload. The line escapes each control character, quote and
  backslash of the payload (for example `\n` and `\u{1b}`), so a payload cannot add a
  line to the log or send a terminal code.
- A press is not a touch. It does not start the idle time again, and it does not turn
  the front light on.
- Each client with credentials on the broker can publish to
  `dashboard/<device_id>/set/button`, so each one can press a button. The broker has
  only two users, `dashboard` and `homeassistant` (refer to "Broker").
- A press arrives only while the connection is up. During the close for a sleep or a
  change of access point, the dashboard drops each message, also a button message.

#### When the dashboard publishes

The discovery message, the state message and the lines of the log are the device
publishes. One queue sends them, in this order: the discovery, the state, the log.

- The queue sends only while the connection is up. It sends nothing in the write of
  the CONNACK, nothing from the start of a sleep, and nothing during the close for a
  change of access point. The messages wait for the next connection after the wake.
- At most 56 device publishes wait for a PUBACK at a time, all together. The session
  takes 64, so 8 places stay free for the status (`asleep` or `restarting`) and the
  service calls.
- If the queue is at its limit, or the session takes no more bytes now, the queue
  continues after the next confirmed write or PUBACK.

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

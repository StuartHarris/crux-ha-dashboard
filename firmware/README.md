# Firmware

The firmware runs the dashboard core on the LilyGo T5 E-Paper S3 Pro Lite (board
H752-01: ESP32-S3-WROOM-1-N16R8, ED047TC1 panel, GT911 touch).

The firmware has two parts:

| Part | Path | Builds for | Tests |
| --- | --- | --- | --- |
| The logic of the shell, with no hardware code | `logic/` (crate `firmware-logic`) | the host and the ESP32-S3 | `just test-firmware` in the root directory |
| The hardware binary | `src/` (crate `dashboard-firmware`) | `xtensa-esp32s3-none-elf` | the bring-up checks on a board |

`logic` is a member of the root workspace, so `just dev` and CI run its tests. It is
`no_std`, and the root recipe `just check-nostd` checks that it builds for the
ESP32-S3. The hardware binary is its own Cargo workspace, because feature unification
with the std shells of the root workspace would pull std into it.

## The logic

| Module | What it does |
| --- | --- |
| `shell` | Owns the core. Routes each effect to a task as a `Command`, and gives each `Signal` from a task to the core. Keeps the connection ids of the streams, the timer tokens, the radio tokens and the `FrontLight` tokens, and the `Write`, `Close`, `Radio`, `Sleep` and `FrontLight` requests until their tasks answer. Obeys the stop rule of the radio: a new `Job::Radio` stops each older radio request that still waits, and the shell answers each of them with `Stopped` (or with `()`) itself. The `stop` list of the job gives their tokens to the radio task. Gives at most one `Render` for each batch of effects, with the last view. Sends each tap to the core as `Tapped`, with the control from `ui::hit_test`. While `Sleep` waits, a tap or a change of the power source answers it. The core does not publish service calls in a dry run. |
| `stream` | The bookkeeping of the stream task. The stream of each connection ends with exactly one `Closed` item. `supersede` removes the queued streams that a later job makes useless. `UpdateStats` counts the updates of the core and their time. |
| `backoff` | A wait that doubles from 1 s to 30 s after each failure, for the start of the touch controller. |
| `timers` | The timers of the timer task, in milliseconds since the start. |
| `touch` | Converts a GT911 point to a panel point, and touch samples to taps with a debounce. A sample is new with a finger, new with no finger (the finger lifted), or not new. Only a new sample with no finger ends a touch. |
| `sleep` | Decides when the chip can go into light sleep for a `Sleep` job, which wake is a real wake, and when a blocker gets a warning. |
| `config` | Reads and checks `cfg.toml`. Its `Debug` output does not show the passwords. `build.rs` includes this file, so it uses only `core`. |

## The hardware binary

One task owns the core through `logic::shell::Shell`. The core has locks that it only
tries to take, so no other task calls it. The other tasks send a `Signal` to the core
task through a queue, and the core task gives each task its work through a queue of its
own. The queues (`src/lane.rs`) are unbounded, so two tasks that send to each other
cannot block each other. This is the pattern of the `cpb` example in Crux.

| Task | Work | Answers |
| --- | --- | --- |
| core (`main`) | Owns the shell. Sends `Start` with the power source `Stand`. | |
| `timers` | Starts and cancels the timers of the core. | `Signal::Timer` |
| `display` | Draws the newest view model with `ui::draw`, and refreshes the panel as `ui::Refresher` decides. Reads the touch controller every 30 ms between refreshes. Sets the front light. Runs the `Sleep` job. | `Signal::Tap`, `Signal::Event`, `Signal::Woke`, `Signal::Light` |
| `radio` | Does the radio jobs of the core (`Join`, `Address`, `Leave`, `Scan`, `StopScan` and `Off`) and the `Restart` job. Has no Wi-Fi policy. The core decides which access point to join, when to scan, when to leave and when to restart the chip. | `Signal::Radio` with the token of the job |
| `stream` | Opens one TCP stream at a time. Writes the bytes of the core to it, gives the bytes that arrive to the core, and closes it. Knows nothing of MQTT. | `Signal::Stream`, `Signal::Written` |
| `net` | Runs the network stack. | |

### The byte stream

The core runs MQTT 3.1.1 (`shared/src/mqtt/`). The stream task
(`src/bin/dashboard/stream.rs`) only moves bytes. The task takes the receive buffer
and the send buffer of the socket (8 KiB each) from the heap once, so the allocator
puts them in PSRAM. The 115 topic filters need about 7.9 KiB of SUBSCRIBE packets,
and the core gives them to the socket in one `Write`.

A `Write` ends when all its bytes are in the send buffer, and smoltcp sends them from
there. The task does not wait for the TCP ACK of the broker, so a write of many
packets does not wait for a round trip for each packet.

The task never waits without a limit, and one select waits for all its events. The
core finds a dead link with its own timers (refer to "Find a dead link" in
`docs/mqtt.md`). The task has only these limits:

- The DNS query has a limit of 5 s, and the TCP connect has a limit of 10 s. A newer
  `Open` and a `Close` stop each of them.
- A `Write` that gets no space in the send buffer for 10 s (`WRITE_STALL`) ends the
  stream.
- A `Close` sends a TCP FIN, waits up to 2 s (`CLOSE_FLUSH`) for the broker to
  acknowledge the bytes that wait, and then aborts the socket.
- A change of access point (a roam) is the decision of the core. The core sends
  DISCONNECT and a `Close` first, so the stream task sees an ordinary clean close.
  Home Assistant sees no `offline` (refer to "Change the access point" in
  `docs/mqtt.md`).

The core connects again after a drop, with a backoff.

### The radio and the Wi-Fi policy

The core decides each Wi-Fi step (`shared/src/features/wifi.rs` and
`shared/src/features/wifi/rules.rs`). The radio task has no policy. It does one
job at a time that the core asks for through the `radio` capability
(`shared/src/capabilities/radio.rs`).

Each job is a `Job::Radio` with a token, a `stop` list and an operation. The task
answers with `Signal::Radio` and the same token.

The stop rule: a new radio request stops each older request that still waits. The
shell answers each stopped request itself (`Stopped`, or `()` for a request with no
result), and gives the tokens in the `stop` list. The radio task drops a job that
the new job stops, and sends no answer for it. A `Restart` stops all jobs. The shell
drops a late answer with a token that it does not know.

How the core uses the stream ends:

- Only an end with `Timeout` or `Transport` fails the link. The core then marks the
  link suspect, and the next join leaves the access point and joins again.
- An end by the broker host (`End::ByHost`: a TCP reset or a FIN from the host)
  proves that traffic crosses the link while the link is up. A reconnect during an
  outage of the broker therefore keeps the association.
- After a roam, the core ends the session with a clean DISCONNECT, so the end is not
  a failure.

The driver of `esp-radio` 1.0.0-beta.1 counts the frames that it did not send yet.
If the link goes down after the driver accepts a frame, the frame can stay in the
count. When the count is full, the stack can send and receive nothing. The join
still works, but DHCP gets no address. Only a restart clears the count.

The core restarts the chip (`Job::Restart`) only after 3 joins (`RESTART_AFTER`)
that get no new address while the link is suspect. A new address, or traffic from
the broker, clears the count. A UniFi block (the joins fail) and an outage of the
broker never restart the chip.

The scan for a roam runs when the link is quiet, after the subscribe. The radio
task dwells 50 to 120 ms on each channel. Three access points took 1.4 s on the
board. If an access point is at least 10 dB stronger, the core starts a roam.

The log lines that an operator can search for:

| Log line | Meaning |
| --- | --- |
| `wifi: radio job N: <operation>` | The task starts the radio job N, for example `join ... (keep true)`, `wait for an address` or `scan all the channels`. |
| `wifi: radio job N stops for a newer request` | The stop rule dropped the job N. |
| `wifi: keep <access point> on channel C, signal S dBm` | A join keeps the association that the device has. |
| `wifi: joined <access point> on channel C with <target> in N ms, signal S dBm` | A join succeeded. |
| `wifi: address <address>` | DHCP gave an address. |
| `wifi: the scan found N access points in N ms. Current ..., signal S dBm` | A roam scan ended. |
| `wifi: restart the chip: <reason>` | The core asked for a restart. |
| `stream: N failed: <reason> (by the host)` | The broker host ended the stream. |
| `core: N updates since the stream opened, ...` | The core task writes it at each close of the stream. |

### Light sleep

On battery, the core sleeps after 30 seconds with no touch. The core owns the steps,
and each step waits for the answer of the firmware:

1. This step acts only if the front light is still on. Usually the light timer
   (20 seconds) turned the light off before the idle timer (30 seconds) expired,
   because a tap on a control starts both timers again. A tap on no control turns
   the light on, but it does not start the idle timer again, so it does not delay
   the sleep. If that tap comes less than 20 seconds before the idle timeout, the
   light is still on at the sleep. The core then sends `FrontLight` with the level 0.
   The display task sets the duty to 0 and answers with `Signal::Light`. If the light
   is off, the core starts the next step at once.
2. The core publishes `asleep` and sends DISCONNECT. The core then sends `Close`
   with `clean: true`. The stream task sends the FIN (with a limit of 2 seconds)
   and ends the stream. The shell then answers `Close`, and only then does the core
   report offline, so the last will does not replace `asleep`. If no connection is
   current, the shell answers at once.
3. The core sends an `Off` radio request. The radio task disconnects, waits 100 ms for
   the frames in the driver, and drops the `WifiController`. The drop stops the Wi-Fi
   driver and the radio. `esp-radio` 1.0.0-beta.1 has no other public call to stop it.
   Then the task sends `Signal::Radio` with the answer `Done`. The next `Join` makes a
   new controller.
4. The core draws the "Asleep" image and sends `Sleep`. The display task draws the
   image first, then puts the chip into light sleep. It does not wait for a blocker,
   so the touch poll and the renders continue. If one blocker stops the sleep for
   5 seconds, the log shows a warning. If the blocker is a low INT, the task clears
   the GT911 status register, and starts the touch controller again if INT stays low.
5. The wake sources are a low level on the GT911 INT pin (GPIO3) and, if the core
   asks for it, the RTC timer. The GT911 stays on and in its normal mode. The panel
   is off after each render.
6. A touch wake, or a timer wake at the time that the core asked for, is a real
   wake. After a touch wake, the display task reads the point (the GT911 holds it
   while INT is low) and sends it as a tap. The shell answers `Sleep` with
   `Wake::Touch` and the control at the point, and the core acts on the control. A
   service call waits until MQTT connects again. If the task cannot read the point,
   it answers `Wake::Touch(None)`. The touch then wakes the device and turns the
   front light on, but it does not act on a control. A timer wake
   sends `Signal::Woke`. After another wake
   (for example a sleep that the hardware rejects), the chip sleeps again after
   3 seconds. In that time, a tap or a change of the power source also answers
   `Sleep`.
7. The RTC timer also wakes the chip each 60 seconds for a check of the power source.
   The display task reads only the charger, with no Wi-Fi. If the source is new, the
   task reads it again after 1 second. If the second read agrees, the shell sends
   `PowerSourceChanged` and answers `Sleep` with `Wake::PowerSource`. Otherwise the
   chip sleeps again at once. The check writes only a debug line to the log.

Light sleep keeps the RAM and the PSRAM, so the core and the panel image stay, and a
wake needs no full refresh.

The LEDC of the front light stops in light sleep, and its pin keeps its level. Thus,
before each sleep, the display task first sets the newest `FrontLight` job that waits.
Then it always sets the duty to 0, and waits 2 ms (two PWM periods) for the LEDC to
use the new duty. The core turns the light off
before `Sleep`, so this is only a guard. If the guard turns the light off, the log
shows `light: on at the sleep, so the light goes off`.

`esp-hal` moves the system timer forward by the time asleep, so `Instant` and the
timers of the core stay correct. The alarm of the scheduler (`TIMG0`) does not run in
the sleep, so the display task arms it again after the wake.

The INT pin is in the low-level mode (register 0x804D, bits 1:0 are 10). The touch
poll reads a sample only while INT is low, and at 30 ms it got each tap in the
bring-up. Thus INT stays low longer than a poll interval, and a level wake does not
miss a short touch.

The charger cannot wake the device when it goes on the stand. The pin map of the
vendor (`docs/pinmap.md`, branch `H752-01`) shows no path from the BQ25896 INT pin to
a GPIO. The inputs of the PCA9535 (its INT is on GPIO38) are the button, the power
good and the INT of the TPS65185. Thus the check of the power source each 60 seconds
sees the stand, or a tap wakes the device first.

### Front light

GPIO11 (`PIN_BL_EN` in `docs/pinmap.md`, branch `H752-01`) drives the EN pin of the
PT4103 LED boost driver. The duty of a PWM signal sets the brightness: 0% is off and
100% is full current. The PT4103 needs a PWM frequency of 1 kHz or less.
`src/light.rs` uses the LEDC low-speed timer 0 at 1 kHz, with a duty of 10 bits, on
channel 0.

The core owns the policy (`shared/src/features/light.rs`). A touch turns the light on
at 60% on the stand or 25% on battery. The light goes off 20 seconds after the last
touch. If the light is still on at a sleep, the core turns it off before the other
steps of the sleep. The firmware only sets the level of each `FrontLight` job.

The display task owns the light, because it must turn the light off before each light
sleep. It sets the level at the start of each pass of its loop, before a render. Thus
the light goes on before the refresh after a tap, not after it. If the panel does not
start, no display task runs, and the core task answers each `FrontLight` job itself.

The second binary, `bringup`, runs the hardware checks for a new board.

### Memory

The heap has the 8 MB of PSRAM and a region of 72 KiB in internal RAM. The allocator
uses the first region that has space, so `board::start` adds the PSRAM region first.
The Wi-Fi driver and the scheduler ask for internal RAM explicitly, so the internal
region stays free for them. With Wi-Fi and MQTT connected, about 29 KiB of it is free.

Do not add the internal region first. The other allocations then fill it, and the
Wi-Fi driver cannot get receive buffers. The device sends packets but receives none,
so MQTT stops after the CONNACK.

The release build of `dashboard` on 2026-11-07:

| Item | Bytes |
| --- | --- |
| Flash image (`espflash save-image`) | 876,400 (21% of the 4,128,768-byte app partition) |
| `.text` + `.rodata` (flash) | 808,173 |
| `.data` + `.bss` (internal RAM, with the 72 KiB heap) | 117,852 |
| `.rwtext` (code in internal RAM) | 47,968 |
| Stack (the rest of internal RAM) | 174,904 |

Run `just size` to see the numbers again.

## The panel driver

The driver in `src/panel/` is a copy of part of
[`lilygo-t5s3paperpro`](https://github.com/azw413/lilygo-t5s3paperpro-rs) 1.1.0 (commit
`c9cf0be`, by Andrew Whaley and Frido Koch). Its licence is GPL-3.0, so this crate is
GPL-3.0 too. The licence is in `src/panel/LICENSE`, and `src/panel/mod.rs` lists the
changes from upstream.

The two candidates were:

| | `lilygo-t5s3paperpro` 1.1.0 | `noot/lilygo-epaper` (`t5s3-epaper-core`) |
| --- | --- | --- |
| Licence | GPL-3.0 | GPL-3.0 |
| esp-hal | 1.0 | 1.1.1 |
| Board | tested on the Pro Lite | the Pro (LoRa, GPS), the same panel code |
| Pins | match the H752-01 pin map | the same |
| PMIC and IO expander | TPS65185 and PCA9535 over a blocking I2C bus that the driver owns | the same registers, through an I2C worker on the second CPU core |
| Waveforms | the 16-level LUT for a full refresh, and the DU waveform for a fast black-and-white partial refresh | the same, plus a faster partial with fewer frames |
| Touch | GT911 on the same bus | GT911, BQ25896, BQ27220, PCF8563 |
| Size | about 2,300 lines (without the waveform tables, which no code uses) | about 5,200 lines in the driver crate |

Only the `dashboard-firmware` crate has GPL code. The `logic` crate has none.

The pins in `src/panel/ed047tc1.rs` are the pins of `docs/pinmap.md` on the
`H752-01` branch of the vendor repository. The data bus is D0 to D2 on GPIO 5 to 7,
D3 to D6 on GPIO 15 to 18, and D7 on GPIO8.

Neither crate builds with esp-hal 1.2, which `esp-radio` 1.0.0-beta.1 needs. With
esp-hal 1.2.2, Cargo stops at once:
`lilygo-t5s3paperpro depends on esp-hal with feature psram but esp-hal does not have
that feature`. The crate also needs `esp-alloc` 0.9 and `esp-bootloader-esp-idf` 0.4,
and the firmware uses 0.11 and 0.6. So the firmware has a copy.

I chose `lilygo-t5s3paperpro` because it is the smaller path to a full Gray4 refresh
and a partial black-and-white refresh. It has the same waveforms as the other crate.
The I2C worker of `noot` runs on the second core with its own loop, which does not fit
one Embassy executor with one owner for each peripheral. The copy keeps the bus,
power-on and waveform code of upstream, with the changes that `src/panel/mod.rs`
lists. The touch,
battery and charger code of `noot` is a good reference for phase 6.

The panel driver and the touch controller share the I2C bus, so the display task
owns both. A full refresh blocks the executor for one frame of the waveform at a time
(15 frames after a clear of 32 frames), and the driver yields after each frame, so the
other tasks run between frames. The Wi-Fi driver runs in its own threads of `esp-rtos`.

## Configuration and secrets

The firmware reads its settings at build time from `firmware/cfg.toml`. The file has
the Wi-Fi password and the broker password, so it is in `.gitignore`.

1. Copy the example:

   ```sh
   cp firmware/cfg.toml.example firmware/cfg.toml
   ```

2. Put the Wi-Fi network, the broker and the device id in `firmware/cfg.toml`.
3. Keep `read_only = true` until the dashboard works. The firmware gives the value to the
   core, which then does a dry run: a tap publishes no service call.

`build.rs` checks the Wi-Fi settings: the SSID has 1 to 32 bytes, `auth` is `wpa2`
(the default), `wpa_wpa2` or `open`, and the password is empty for an open network and
8 to 63 bytes for the others. The Wi-Fi driver cannot join a WPA3 network. An unknown
section or key is also an error. An error names the key.

The file has no time zone setting. The dashboard shows UK time, and the core calculates
summer time. If your `cfg.toml` has the old key `utc_offset_minutes` or `ntp_server`,
remove it. The firmware has no clock of its own. The footer text "Last changed HH:MM"
comes from the `last_updated` time that Home Assistant publishes for each entity.

`build.rs` writes the values into `target/.../out/config.rs` as one constant. Each
binary includes it as a private module. The build
does not print a value, and an error in the file gives only the line and the key. To
build with another file, set `DASHBOARD_CONFIG` to its path (relative to `firmware/`).
CI builds with `DASHBOARD_CONFIG=cfg.toml.example`, which has placeholders.

If `cfg.toml` does not exist, the build stops with an error that tells you to copy the
example. Do not commit `cfg.toml`. Do not put a password in a command line or in a log.

## Commands

Install the `esp` toolchain with `espup install` and `espflash` with
`cargo install espflash --locked`. The recipes read `~/export-esp.sh` if it exists.

| Command (in `firmware/`) | What it does |
| --- | --- |
| `just build` | Builds both binaries (release). |
| `just check` | Checks the formatting and the types. |
| `just clippy` | Runs clippy with the pedantic and nursery lints as errors. |
| `just flash` | Flashes `dashboard` and shows the log. |
| `just bringup` | Flashes `bringup` and shows the log. |
| `just monitor` | Shows the log with no flash. |
| `just size` | Shows the size of the image and of each memory section. |

From the root directory, `just firmware <recipe>` runs the same recipes, and
`just test-firmware` runs the host tests of `logic`.

## Bring-up checklist (phase 5b)

Do the steps in this order. Each step needs the steps before it.

1. **Serial log.** Connect the board with USB-C. Run `espflash board-info`. Make sure
   that it shows an ESP32-S3 with 16 MB of flash. If the port does not show, hold
   BOOT (GPIO0), press RST, and release BOOT.
2. **Bring-up binary.** Run `just bringup`. The log shows each step.
3. **PSRAM.** Find `PSRAM size: 8 MB` and the line `step 1: heap`. The heap must have
   an internal region of 72 KiB and an external region of about 8 MB. If the PSRAM
   does not start, set `PsramMode::Auto` in `src/board.rs` and try again.
4. **I2C.** Find `step 2`. It must find 0x20 (PCA9535), 0x51 (PCF8563), 0x55
   (BQ27220), 0x5D (GT911), 0x68 (TPS65185) and 0x6B (BQ25896). The GT911 can be at
   0x14 if INT was high at reset. The firmware does not use the PCF8563.
5. **Panel clear and test pattern.** Find `step 3`. The panel flashes, then shows 16
   bars from black to white. Write down the time of the full refresh.
6. **Partial refresh.** Find `step 4`. A black rectangle shows in the middle with no
   flash of the whole panel.
7. **One golden page.** Find `step 5`. The panel shows the first page with `-` in the
   tiles (no states yet). Compare it with the page from `just sim-png` in the root
   directory. The layout must be the same.
8. **Touch.** Find `step 6`. For 30 seconds, tap the four corners and some tiles. Each
   tap writes its raw point, its panel point and its control. The top-left corner must
   give a point near (0, 0), and the bottom-right corner a point near (959, 539). If
   the axes are turned or flipped, change `TouchMap::to_panel` in
   `logic/src/touch.rs` and its tests.
9. **Wi-Fi.** Find `step 7`. The log shows `joined` and an address.
10. **MQTT and the retained states.** Set `read_only = true` in `cfg.toml`. Run
    `just flash`. The log shows `stream: N is open` and,
    at each close of the stream, the count and the time of the updates of the core. The tiles show the
    states from Home Assistant. The footer shows "Last changed
    HH:MM" in UK time: the latest `last_updated` time of the entities on the page. On the
    Pi, run
    `mosquitto_sub -h localhost -u dashboard -P <password> -t 'dashboard/#' -v`
    (type the password, do not keep it in the shell history). The status topic shows
    `online`.
11. **A tap in a dry run.** Tap a light tile. The log shows `touch: tap`. The
    `mosquitto_sub` window shows no message on `dashboard/<id>/cmd`, and the light does
    not change.
12. **A real tap.** Set `read_only = false`, run `just flash`, and tap a light tile. The
    `cmd` topic shows the service call, and the light changes.
13. **The − and + buttons.** The automation `dashboard_commands` on the Pi must accept
    `light.turn_on` (refer to `docs/mqtt.md`). Keep `read_only = false`. On a few dimmable
    light tiles, tap each corner of the − and the + button with a finger. A tap on a
    button changes the brightness on the tile. A tap that misses a button toggles the
    light. Count the toggles. The log line `touch: tap at (x, y)` gives the panel
    point of each tap. Compare it with `tile_minus` and `tile_plus` in
    `ui/src/layout.rs`. If the misses are too frequent, increase `STEP_WIDTH` or
    `STEP_HEIGHT` in `ui/src/layout.rs`. Then run `just golden` in the root directory
    and examine the new images.

## TODO(5b)

The bring-up on 2026-11-18 gave these results:

| Item | Result |
| --- | --- |
| PSRAM | 8 MB in octal mode. |
| Internal heap | About 29 KiB of the 72 KiB is free with Wi-Fi and MQTT connected. |
| Touch map | The orientation is correct. Each corner gives the expected panel point. |
| Lift | Each lift gives one to three samples with no points, so each touch gives one tap. |
| − and + buttons | 14 taps on the buttons gave 14 steps, and no toggle. |
| Full refresh | 1.1 s to 1.8 s. |
| Partial refresh | 100 ms to 370 ms, by the size of the area. |
| I2C scan | The scan does not find the GT911 at 0x5D, but the touch controller works. The probe of the scan is a read of one byte with no register, and the GT911 does not answer it. |

These items stay open:

- VCOM is 1600 mV, the value of the vendor firmware (`src/panel/ed047tc1.rs`).
- The core asks for a full refresh after 30 partial renders and after 30 minutes
  (`shared/src/features/refresh.rs`). Change the numbers if ghost images show.
- Measure the time from the CONNACK to the end of the subscribe with the 7 SUBSCRIBE
  packets in one write. At each close of the stream, the log shows the count and the
  time of the updates of the core since the stream opened. A warning shows for an update of more than 50 ms.

## Power check (phase 6)

The display task reads the BQ25896 charger (REG0B) and the BQ27220 fuel gauge
(`StateOfCharge()`) each 30 seconds. It only reads these chips. Do these steps with the
board on USB power:

1. Run `just flash`. The log shows `power: source Stand at the start`, then
   `power: battery N%`. The header shows the percent and a plug icon. Keep
   `force_battery = false` for this check.
2. Disconnect USB. In about 30 seconds, the header shows the percent and a battery
   icon. The fill of the icon shows the charge. The serial log is on USB, so you cannot
   see the log line `power: source Battery` now.
3. Connect USB again. In about 30 seconds, the header shows the plug icon. If the serial
   log connects again, it shows `power: source Stand`.

If a read fails, the log shows one warning, and the header keeps the last values.

## Sleep check (phase 6)

On USB the core is on the stand, so it never sleeps. The setting `force_battery` in
`[device]` makes the firmware tell the core that the device is on battery. The power
poll then sends no change of the source, but it still sends the charge.

Light sleep stops the clock of the USB-Serial-JTAG port. The computer can show the
port as disconnected, and the log can stop until you connect the cable again. esp-hal
1.2.2 has no setting that keeps the port on in light sleep, so `force_battery` cannot
keep the log. Thus examine the panel and the status topic for each step, not only the
log.

1. Set `force_battery = true` in `cfg.toml`.
2. Run `just flash`. The log shows `power: source Battery at the start`. The header
   shows a battery icon.
3. On the Pi, run
   `mosquitto_sub -h localhost -u dashboard -P <password> -t 'dashboard/+/status' -v`.
   The status is `online`.
4. Do not touch the panel for 30 seconds. The panel must show "Asleep", and the
   status must show `asleep`. The log can show `wifi: off, and the radio is off`,
   then `power: sleep until [Touch]`. The last log lines before the sleep can be
   lost.
5. Tap a room button on the panel. The panel must show that room, so the tap that
   wakes the device also acts. In a few seconds the status is `online` again. If the
   log continues, it shows
   `power: woke by Touch after N ms, average current M mA (not verified)` and
   `touch: wake tap at (x, y)`.
6. Wait 30 seconds again. The panel must show "Asleep" again.
7. Set `force_battery = false` in `cfg.toml` and run `just flash`.

### Drain while asleep

The wake line gives `AverageCurrent()` of the BQ27220 (command 0x14). The unit (mA) and
the sign (negative for a discharge) are not verified on the board, so the log marks
the value "not verified". The display task reads it directly after the wake, so the value is
mostly the current while the device was asleep. But the serial log needs USB, and on
USB the value is the charge current, not the drain. Thus the wake line cannot give
the drain. To measure the drain:

1. Flash with `force_battery = false`, and disconnect USB.
2. Write down the charge in the header.
3. Let the device sleep for some hours, then tap it. Write down the charge and the
   time.
4. Calculate the drain from the change of the charge, the time and the capacity of
   the battery.

For a direct value, put a meter in series with the battery.

## Front light check

1. On the stand, tap the panel. The panel lights. The log shows `light: 60%`.
2. Tap an empty area of the panel. The light also goes on.
3. Do not touch the panel for 20 seconds. The light goes off, and the log shows
   `light: 0%`.
4. Look for flicker at 1 kHz. Do not increase `FREQUENCY_HZ` in `src/light.rs`
   above 1 kHz, because the PT4103 needs 1 kHz or less.
5. If the light is too bright or too dim, change `STAND_LEVEL` or `BATTERY_LEVEL`
   in `shared/src/features/light.rs`.
6. Disconnect USB and tap the panel. The light is dimmer (25%).
7. Do not touch the panel for 30 seconds. The light goes off at 20 seconds, and the
   panel shows "Asleep" at 30 seconds. In a dark room, make sure that the light stays
   off while the device sleeps.
8. Tap a room button. The light goes on, and the panel shows that room.
9. While the light is on, put the device on the stand. The light changes to the
   level of the stand.

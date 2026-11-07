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
| `shell` | Owns the core. Routes each effect to a task as a `Command`, and gives each `Signal` from a task to the core. Keeps the MQTT connection ids, the timer tokens and the `WifiOn` tokens. Gives at most one `Render` for each batch of effects, with the last view. Converts a tap to an event with `ui::hit_test`, or to `Woke` while the device sleeps. Drops service calls in read-only mode. |
| `mqtt` | The bookkeeping of the MQTT task. The stream of each connection ends with exactly one `Disconnected` item. `supersede` removes the queued connections that a later command makes useless. `Outbox` keeps a publish while the client has no quota, and closes the connection if the publish cannot be sent. |
| `wifi` | A `WifiOff` during a join stops the join. A `WifiOn` during a join continues it with the newer token. |
| `backoff` | A wait that doubles from 1 s to 30 s after each failure, for the start of the touch controller. |
| `timers` | The timers of the timer task, in milliseconds since the start. |
| `refresh` | Decides between a full refresh, a partial refresh of the changed area, and no refresh. |
| `touch` | Converts a GT911 point to a panel point, and touch samples to taps with a debounce. A sample is new with a finger, new with no finger (the finger lifted), or not new. Only a new sample with no finger ends a touch. |
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
| `display` | Draws the newest view model with `ui::draw`, and refreshes the panel. Reads the touch controller every 30 ms between refreshes. | `Signal::Tap` |
| `wifi` | Joins the network and waits for DHCP, or leaves the network. | `Signal::Wifi` |
| `mqtt` | Opens one connection at a time with `rust-mqtt` (MQTT 5) over TCP. Subscribes, publishes, sends pings, and closes. | `Signal::Mqtt` |
| `net` | Runs the network stack. | |

`Sleep` writes a line to the log, and the device stays awake. The view says that the
device sleeps, so the next tap sends `Woke`. Deep sleep comes in phase 6.

`TimeNow` gives the time since the start, from the Unix epoch. The core treats a time
before 2025 as not valid, so the view shows only "Live", and no "Updated HH:MM" text,
until the firmware reads the RTC (refer to the TODO list). The log says at the start that the clock is not
set.

The second binary, `bringup`, runs the hardware checks for a new board.

### Memory

The heap has a region of 72 KiB in internal RAM, for the Wi-Fi driver, and the 8 MB
of PSRAM. The allocator uses the first region that has space. The two framebuffers of
the display task (259,200 bytes each) do not fit in the internal region, so they go to
PSRAM. The waveform table of the driver (65,536 bytes) fits in the internal region, so
the driver puts it in PSRAM explicitly (`esp_alloc::ExternalMemory`).

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
3. Keep `read_only = true` until the dashboard works. In read-only mode, a tap publishes
   no service call.

`build.rs` checks the Wi-Fi settings: the SSID has 1 to 32 bytes, `auth` is `wpa2`
(the default), `wpa_wpa2` or `open`, and the password is empty for an open network and
8 to 63 bytes for the others. The Wi-Fi driver cannot join a WPA3 network. An error
names the key.

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
   0x14 if INT was high at reset.
5. **Panel clear and test pattern.** Find `step 3`. The panel flashes, then shows 16
   bars from black to white. Write down the time of the full refresh.
6. **Partial refresh.** Find `step 4`. A black rectangle shows in the middle with no
   flash of the whole panel.
7. **One golden page.** Find `step 5`. The panel shows the first page with `-` in the
   tiles (no states yet). Compare it with the page from `just sim-png` in the root
   directory. The layout must be the same.
8. **Touch.** Find `step 6`. For 30 seconds, tap the four corners and some tiles. Each
   tap writes its raw point, its panel point and its event. The top-left corner must
   give a point near (0, 0), and the bottom-right corner a point near (959, 539). If
   the axes are turned or flipped, change `TouchMap::to_panel` in
   `logic/src/touch.rs` and its tests.
9. **Wi-Fi.** Find `step 7`. The log shows `joined` and an address.
10. **MQTT and the retained states.** Set `read_only = true` in `cfg.toml`. Run
    `just flash`. The log shows `mqtt: connected` and `mqtt: subscribed to N topics`,
    and the tiles show the states from Home Assistant. On the Pi, run
    `mosquitto_sub -h localhost -u dashboard -P <password> -t 'dashboard/#' -v`
    (type the password, do not keep it in the shell history). The status topic shows
    `online`.
11. **A tap in read-only mode.** Tap a light tile. The log shows `touch: tap`. The
    `mosquitto_sub` window shows no message on `dashboard/<id>/cmd`, and the light does
    not change.
12. **A real tap.** Set `read_only = false`, run `just flash`, and tap a light tile. The
    `cmd` topic shows the service call, and the light changes.

## TODO(5b)

These choices need the board:

- PSRAM: the mode is fixed to octal (`src/board.rs`). Make sure that the log shows
  8 MB.
- The internal heap is 72 KiB. Measure the free internal heap with Wi-Fi and MQTT
  connected, and change `INTERNAL_HEAP` if necessary.
- VCOM is 1600 mV, the value of the vendor firmware (`src/panel/ed047tc1.rs`).
- The GT911 must give a new sample with no points when the finger lifts. The bring-up
  touch step writes `release` for each such sample. If there is none, a held finger
  gives one tap, and the next touch gives no tap.
- The touch map turns a portrait controller (540 × 960) to landscape, as upstream does
  (`logic/src/touch.rs`). Check it with step 6.
- The clock starts at the Unix epoch (`src/bin/dashboard/clock.rs`). Read the PCF8563
  at the start, and set it with SNTP after the first Wi-Fi connection.
- Measure the time of a full and of a partial refresh. The display task does not read
  the touch controller during a refresh. The policy does a full refresh after 30
  partial refreshes (`logic/src/refresh.rs`). Change the number if ghost images show.
- `rust-mqtt` sends one topic filter in each subscribe packet. Measure the time to get
  all retained states, and the size of the receive buffer (4096 bytes).

Phase 6 has the `TODO(6)` items: the power source from the BQ25896, deep sleep with
wake on touch and RTC, and the radio off for `WifiOff`.

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

For the place of the firmware in the system, and the rule "logic in the core, drivers in
the shells", refer to [`docs/architecture.md`](../docs/architecture.md).

## The logic

| Module | What it does |
| --- | --- |
| `shell` | Owns the core. Routes each effect to a task as a `Command`, and gives each `Signal` from a task to the core. Keeps the connection ids of the streams, the timer tokens, the radio tokens and the `FrontLight` tokens, and the `Write`, `Close`, `Radio`, `Sleep` and `FrontLight` requests until their tasks answer. Obeys the stop rule of the radio: a new `Job::Radio` stops each older radio request that still waits, and the shell answers each of them with `Stopped` (or with `()`) itself. The `stop` list of the job gives their tokens to the radio task. Gives at most one `Render` for each batch of effects, with the last view. Sends each tap to the core as `Tapped`, with the control from `ui::hit_test`. While `Sleep` waits, a tap or a change of the power source answers it. The core does not publish service calls in a dry run. |
| `stream` | The bookkeeping of the stream task. The stream of each connection ends with exactly one `Closed` item. `supersede` removes the queued streams that a later job makes useless. `UpdateStats` counts the updates of the core and their time. |
| `backoff` | A wait that doubles from 1 s to 30 s after each failure, for the start of the touch controller. |
| `timers` | The timers of the timer task, in milliseconds since the start. |
| `touch` | Converts a GT911 point to a panel point, and touch samples to taps with a debounce. A sample is new with a finger, new with no finger (the finger lifted), or not new. Only a new sample with no finger ends a touch. |
| `sleep` | Decides when the chip can go into light sleep for a `Sleep` job (the blockers and the hold after a tap), and when a blocker gets a warning. The core decides what each wake means. |
| `forward` | Decides which lines of the log print and which go to the core (`warn` and `error`, not the target `core`, also if `ESP_LOG` does not print them), splits each line into its source and its message, and keeps at most 16 lines for the core task with a count of the lines that it dropped. Reads `ESP_LOG` with the rules of esp-println. |
| `config` | Reads and checks `cfg.toml`. Its `Debug` output does not show the passwords. `build.rs` includes this file, so it uses only `core`. |

## The hardware binary

One task owns the core through `logic::shell::Shell`. The core has locks that it only
tries to take, so no other task calls it. The other tasks send a `Signal` to the core
task through a queue, and the core task gives each task its work through a queue of its
own. The queues (`src/lane.rs`) are unbounded, so two tasks that send to each other
cannot block each other. This is the pattern of the `cpb` example in Crux.

| Task | Work | Answers |
| --- | --- | --- |
| core (`main`) | Owns the shell. Sends `Start` with the power source `Stand`, the firmware version and the MAC address of the Wi-Fi station interface (`board::firmware`). Sends each warn and error line that the logger keeps as `Event::ShellLogged`. | |
| `timers` | Starts and cancels the timers of the core. | `Signal::Timer` |
| `display` | Draws the newest view model with `ui::draw`, and refreshes the panel as `ui::Refresher` decides. Reads the touch controller every 30 ms between refreshes. Sets the front light. Runs the `Sleep` job. | `Signal::Tap`, `Signal::Event`, `Signal::Woke`, `Signal::Light` |
| `radio` | Does the radio jobs of the core (`Join`, `Address`, `Leave`, `Scan`, `StopScan` and `Off`) and the `Restart` job. Has no Wi-Fi policy. The core decides which access point to join, when to scan, when to leave and when to restart the chip. | `Signal::Radio` with the token of the job |
| `stream` | Opens one TCP stream at a time. Writes the bytes of the core to it, gives the bytes that arrive to the core, and closes it. Knows nothing of MQTT. | `Signal::Stream`, `Signal::Written` |
| `net` | Runs the network stack. | |

### The byte stream

The core runs MQTT 3.1.1 (`shared/src/mqtt/`). The stream task
(`src/bin/dashboard/stream.rs`) only moves bytes. The task takes the receive buffer
and the send buffer of the socket (8 KiB each) from the heap once, so the allocator
puts them in PSRAM. The 117 topic filters need about 7.9 KiB of SUBSCRIBE packets,
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

The core decides each Wi-Fi step (`shared/src/features/wifi/mod.rs` and
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

The core writes its decisions through the log capability. The task prints each line
as `wifi: <message>` or `mqtt: <message>`, at the level `info` or `warn`. The core
lines show beside the radio lines `wifi: radio job N: <operation>`. The core line
shows the reason, and the radio line shows the action. One `grep -a 'wifi:'` on the
serial log shows both, in the order that they happened. The core writes the usual
steps at the level `debug`, for example each step of a sleep. `ESP_LOG` is
`info,core=debug` (`.cargo/config.toml`), so these lines print on the serial port,
and the `debug` lines of the other targets do not print. The core does not keep a
`debug` line, so it does not go to Home Assistant.

The core lines have the log target `core`. The logger does not print the target, so
the serial log does not show it. A filter in `ESP_LOG` uses the target: a
`dashboard=...` directive does not match the core lines. The logger also uses the
target: it does not send a core line back to the core.

The logger of the firmware (`src/logger.rs`) prints each line as the logger of
esp-println does, with the same colours and the same `ESP_LOG` rules. It also keeps
each `warn` and `error` line of the firmware, also a line of esp-radio, for the core.
The core task sends each kept line to the core as `Event::ShellLogged`. The core then
publishes the line to Home Assistant with its own lines (refer to the "Log" section of
`docs/mqtt.md`).

- The forward does not depend on `ESP_LOG`. A `warn` or `error` line that `ESP_LOG`
  does not print still goes to the core.
- For a line of the firmware (the crates `dashboard` and `dashboard_firmware`), the
  source is the first word of the line, if the line starts with a lowercase word and
  `": "`, for example `stream` or `power`. Otherwise, and for each line of another
  crate, the source is the crate of the log target, for example `esp_radio`, and the
  message is the whole line. Refer to `logic::forward::split`.
- The logger does not allocate, because a line can come from a thread of esp-rtos or
  before the heap is ready. It keeps at most 16 lines that the core task did not take.
  It drops the lines after that. The event of the last kept line tells the core how
  many it dropped.
- The logger keeps lines from just after `board::init()`, when `main` calls
  `logger::forward()`. A line before that only prints. A line from an outage or a
  sleep goes to Home Assistant after the next connect. The `bringup` binary has no
  core, so its logger only prints.

After the task prints a core line, it answers with the time since the chip started, in
milliseconds. That time counts through a light sleep. The core keeps its last 64 lines
in RAM with these times, also while the board sleeps or is offline. A chip restart
loses that RAM, so the firmware also keeps a few facts in RTC memory. Refer to "The
restart facts" below.

### The restart facts

The firmware keeps these values in RTC fast memory
(`#[ram(unstable(rtc_fast, persistent))]`, about 2 KB). esp-hal fills that memory with
zeros at a power-on, and keeps it through each other reset (software, panic,
watchdog, brownout, a reset from the USB port). `src/boot.rs` holds the statics, and
`logic::boot` has the rules and their host tests.

- **The reset reason.** `main` reads `esp_hal::system::reset_reason()` once, just
  after `board::init()`, and maps it to the `ResetReason` of the core
  (`logic::boot::reset_reason`): a power-on (also when the chip cannot tell), a
  software reset, a watchdog (each of the 8 watchdog codes), a brownout of the digital
  core, or another reset (the USB port, a glitch of the clock or of the power). On the
  ESP32-S3, a brownout of the whole chip can also give the code of a power-on. The
  core gets the reason in `Start`, writes `power: started after <reset>`, and publishes
  it as the `reset` sensor.
- **The restart count.** Each reset that is not a power-on adds 1. A power-on sets the
  count to 0. The core publishes it as the `restarts` sensor, and adds `, restart N` to
  the start line.
- **The last warn and error lines.** The logger keeps each warn and error line, also
  a line of the core (target `core`) and a line that `ESP_LOG` does not print, in a
  ring of 8 lines. The
  newest line replaces the oldest line. Each line has the caps of a forwarded line, in
  fixed buffers with no allocation. A line of the core has its area as the source, for
  example `wifi`. At the next start after a reset that is not a power-on, `main` takes
  the lines and clears the ring before the first warn line of the new start. The core
  then puts them first in its log, as `before the restart: <source>: <message>` with
  the source `boot`. After a power-on, the ring is empty.
- **The panic flag and the panic count.** A panic restarts the chip (refer to the next
  part). The next start reports `Panic` in place of a software reset.

The memory starts with a magic word with the version of the layout. A reset after a
new firmware is flashed is usually not a power-on (refer to the next part), so the new
firmware can read the memory of the old one at other addresses. If the magic word is
not there, the firmware clears the memory. The level byte of each line goes last, so a reset during a write leaves an
empty place. The read drops a line with a level, a length or UTF-8 that is not valid.

### Which resets are a power-on

The chip, not the firmware, decides the reset reason. Three cases are easy to confuse:

| What you do | The reset reason | The restart facts |
| --- | --- | --- |
| Press RST, or remove the power | A power-on, because RST pulls the EN pin of the chip low | Cleared |
| Run `just flash` while the board is awake | Another reset (`Other`), because espflash resets the chip through the USB port | Kept. The restart count goes up by 1. |
| Run `just monitor`, or another serial monitor | Another reset (`Other`), because the monitor resets the chip when it opens the port | Kept. The restart count goes up by 1. |

If the board is asleep, espflash cannot reset it. Hold BOOT, press RST and release
BOOT, then flash. The first start after that flash is a power-on, because of the RST
press. Thus after a usual flash, the `reset` sensor shows `Other` and the `restarts`
sensor goes up. A flash does not clear the panic count.

### A panic restarts the chip

esp-backtrace prints the panic and its backtrace. Then, with its `custom-halt`
feature, it calls `boot::custom_halt`. That function sets the panic flag, waits 200 ms
for the serial port, and restarts the chip with `esp_hal::system::software_reset()`.
`esp_hal::init` turns off each watchdog. Without this restart, a panic stops the
board until a power-on.

If a panic occurs at each start, each restart causes the next panic, and the restarts
empty the battery. So the firmware counts the panic restarts in a row. The count goes
back to 0 only at these times:

- A power-on (a press of RST is one), because esp-hal fills the memory with zeros.
- The uptime of one start reaches 10 minutes (`logic::boot::STABLE_AFTER` and
  `logic::boot::Uptime`). The uptime counts through light sleep, so the rule works on
  battery too, where the board is online for only about 30 s at each wake. The core
  task checks the uptime after each signal: the MQTT pings while online, and the wake
  for the check of the power source every 60 s while asleep. An MQTT connect alone
  does not set the count to 0, because a panic can follow each connect.

Each other reset keeps the count: a software reset, a watchdog reset, a brownout and
a reset from the USB port (a flash or a serial monitor). The third panic in a row
(`logic::boot::PANIC_LIMIT`) stops the chip, and prints
`panic: 3 panics in a row, so the chip stops`. The `restarts` sensor shows
the restarts before that.

The stop masks and disables each interrupt, and then waits in `waiti`, so the CPU
clock stops (`boot::halt`). No watchdog resets the stopped chip, because
`esp_hal::init` disables each one. The stop does not stop the Wi-Fi radio, because a
call to the driver is not safe in a panic. A panic that stops the chip does not set
the panic flag, so the next start does not report `Panic`.

To start a stopped chip, press RST. That press is a power-on, so it clears the count.
If a flash restarts the stopped chip, the flash keeps the count at the limit. The next
panic then stops the chip at once. Only a power-on, or 10 minutes of uptime with no
panic, clears the count.

The panic message is not in the ring of lines. The hooks of esp-backtrace 0.20
(`custom-pre-backtrace`, `custom-halt`) get no `PanicInfo`, and a panic handler of our
own would replace the backtrace of esp-backtrace. The serial log keeps the message and
the backtrace.

The `bringup` binary does not read the RTC memory, so the lines from before a restart
stay for the dashboard. Its core gets the facts of a power-on, and a panic in it stops
the chip as before.

The log lines that an operator can search for:

| Log line | Meaning |
| --- | --- |
| `wifi: radio job N: <operation>` | The task starts the radio job N, for example `join ... (keep true)`, `wait for an address` or `scan all the channels`. |
| `wifi: radio job N stops for a newer request` | The stop rule dropped the job N. |
| `wifi: keep <access point> on channel C, signal S dBm` | A join keeps the association that the device has. |
| `wifi: joined <access point> on channel C with <target> in N ms, signal S dBm` | A join succeeded. |
| `wifi: address <address>` | DHCP gave an address. The core gets it in the lease and publishes it as the `ip` sensor. |
| `wifi: the network stack is up with no IPv4 address` | Not expected: the address request answers `NoLease`. |
| `wifi: the scan found N access points in N ms. Current ..., signal S dBm` | A roam scan ended. |
| `<area>: restart the chip: <reason>` | The core asked for a restart. The area is `wifi` for a restart after 3 joins with no address, and `device` for a restart that Home Assistant asked for (`device: restart the chip: Home Assistant asked`). |
| `wifi: the link is suspect, because the MQTT session failed: <end>` | The core thinks the link is bad, so the next rejoin leaves first. |
| `wifi: the link is suspect, so leave <access point> and join again` | The core leaves before the join, because the link is suspect. |
| `wifi: N of 3 joins got no new address while the link is suspect` | The count toward a restart of the chip. |
| `wifi: the join to the cached access point <...> failed, so join the strongest access point` | The core changes the target after a failed join. |
| `wifi: the scan found no access point that is 10 dB stronger than ..., so stay` | A roam scan ended with no roam (`debug`). |
| `mqtt: <end>, so connect again in N s` | The session failed for the reason `<end>`. The core waits N seconds before the next connect. |
| `mqtt: <end>, during the sleep` | The session failed in the steps of a sleep. The sleep continues. |
| `mqtt: the link is down, so connect again in N s` | A join failed (the `wifi:` warning before it gives the reason). The core waits N seconds. |
| `mqtt: the close of connection N got no answer in 6 s` | The `Close` request got no answer in its time limit. |
| `power: started after <reset>[, restart N]` | The start of the core, with the reset reason and the restart count. A panic, a watchdog reset or a brownout gives `warn`. |
| `power: sleep N <step>` | The start of each step of the sleep N (`debug`). |
| `panic: restart the chip` | A panic restarts the chip (printed by `boot::custom_halt`). |
| `power: sleep N woke with no cause, so it sleeps again after 3 s` | The first wake with no cause in the sleep N. |
| `power: sleep N had K wakes with no cause` | The wake or the report after more than one wake with no cause in the sleep N. |
| `power: sleep N reports to Home Assistant` | The hourly report of the sleep N starts (`debug`). |
| `power: the report of sleep N stopped after 30 s: ...` | The hourly report of the sleep N did not end in its limit. The line tells what was not done, or the count of failed tries and the last reason. |
| `power: sleep N got a power source wake on battery, so it sleeps again` | Not expected: the shell answered `Wake::PowerSource`, but the core is still on battery. |
| `mqtt: the broker refused one or more topic filters, so some entities get no state` | The broker refused a filter of the subscribe. |
| `mqtt: a message of N bytes is too long, so the core skipped it` | The core skipped a message that is too long. |
| `mqtt: the service call for <entity> is too long for a packet, so the core dropped it` | The core dropped a service call. |
| `stream: N failed: <reason> (by the host)` | The broker host ended the stream (`info`, serial only). |
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
5. The wake sources are the sources of the `Sleep` request: a low level on the GT911
   INT pin (GPIO3), and the RTC timer of 60 seconds for the check of the power
   source. The GT911 stays on and in its normal mode. The panel is off after each
   render.
6. The display task answers each wake at once, and the core decides what to do. The
   firmware keeps no schedule.
   - After a touch wake, the display task reads the point (the GT911 holds it while
     INT is low) and sends it as a tap. The shell answers `Sleep` with `Wake::Touch`
     and the control at the point, and the core acts on the control. A service call
     waits until MQTT connects again. If the task cannot read the point, it answers
     `Wake::Touch(None)`. The touch then wakes the device and turns the front light
     on, but it does not act on a control.
   - After a timer wake, the display task reads the charger, the charge and the
     diagnostics, with no Wi-Fi, and answers `Wake::Timer`. The core usually sends a
     new `Sleep` at once, with no image and no line in Home Assistant. Each 60th check
     of a sleep starts the hourly report instead (refer to "The hourly report" in
     `docs/mqtt.md`): no `Sleep` job waits, so the display task stays awake as after a
     wake, and the network task joins and connects. The report then ends with the
     steps 2 to 4 above. Step 4 draws only if the image changed, with a partial
     refresh, and otherwise sends `Sleep` with no `Render`. If the source is new, the task reads it again after
     1 second, and the chip does not sleep before that read
     (`Blocker::PowerConfirm`). If the second read agrees, the shell sends
     `PowerSourceChanged`, and the core wakes on the stand.
   - After another wake (for example a sleep that the hardware rejects), the task
     answers `Wake::Other`. The core stays awake for 3 seconds, then sends a new
     `Sleep`. In that time, a tap or a change of the power source wakes the device.
     The first such wake of a sleep writes an `info` line to Home Assistant.
7. The chip does not sleep while an event that the display task sent waits for the
   core task (`Blocker::CoreBusy`). The display task sends a change of the power
   source or the charge, and then tries to sleep in the same pass of its loop. An
   exact handshake closes that gap (`logic::sleep::InFlight`):
   - The display task adds 1 to the count before it sends the event.
   - The core task subtracts 1 after the shell handled the event, and after it
     signalled the end of the `Sleep` job.
   - The display task takes a new value of the `Sleep` job before it tries to sleep.
   Thus a confirmed stand never waits in the queue while the chip sleeps.
8. After a tap, the chip does not sleep for 2 seconds (`TAP_HOLD_MS`). The display
   task sends the tap and then tries to sleep in the same pass, before the core task
   handles the tap. The hold closes that gap.

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

The core owns the policy (`shared/src/features/light/`). A touch turns the light on
at the tap level of the power source: by default 60% on the stand or 25% on battery.
Home Assistant can set each tap level, and it can turn the light on and off (refer to
"Front light" in `docs/mqtt.md`). The light of a touch goes off 20 seconds after the
last touch. If the light is still on at a sleep, the core turns it off before the
other steps of the sleep. The firmware only sets the level of each `FrontLight` job.

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

`build.rs` also gives the firmware its version (`board::VERSION`): the version of the
package, `+`, and a short commit id, for example `0.1.0+1b073946`. The commit id is
build metadata of SemVer. It is the jj commit id (8 hex digits) of the nearest change
that has code: the working copy, or its nearest ancestor that is not empty
(`latest(heads(::@ & ~empty()))`). If the working copy has edits, the version ends with
`.dirty`, for example `0.1.0+1b073946.dirty`. If jj is not there, the commit id is the
git commit of `HEAD`. If neither works, it is `unknown`. The core sends the version
to Home Assistant in the discovery message (refer to `docs/mqtt.md`).

The script runs jj with `--at-op=@`, so it takes no snapshot of the working copy, and
it changes nothing in the repository. Thus `.dirty` tells the state of the working
copy at the last jj command. An edit after the last jj command does not count. Run a
jj command (for example `jj st`) before the build if the version must show it.

Cargo runs the script again after each jj operation, after a move of a detached git
`HEAD`, and after each git commit on a branch (`logs/HEAD` in the git directory).

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

The display task reads the BQ25896 charger and the BQ27220 fuel gauge each 5 seconds
while the chip is awake. While the chip sleeps, the check of the power source each 60
seconds reads the same registers as the poll: the charger (REG0B), the charge and the
diagnostics. It sends each change to the core before `Wake::Timer`. The core only keeps
the values while the device sleeps and publishes nothing, so the state message of the
next connection (for example an hourly report) carries a fresh charge. The five reads
take about 1 to 3 ms (6 bytes each on the I2C bus at 400 to 100 kHz, not measured). The
display task only reads these chips.

| Chip | Register or command | Value | Unit |
| --- | --- | --- | --- |
| BQ25896 | REG0B, bits 7:5 and 2 (`VBUS_STAT`, `PG_STAT`) | the power source | |
| BQ25896 | REG0B, bits 4:3 (`CHRG_STAT`) | the charge state: 00 not charging, 01 pre-charge, 10 fast charge, 11 done | |
| BQ27220 | `StateOfCharge()`, 0x2C | the charge | % |
| BQ27220 | `Voltage()`, 0x08 | the voltage of the cell | mV, `u16` |
| BQ27220 | `AverageCurrent()`, 0x14 | the average current | mA, `i16` |
| BQ27220 | `Temperature()`, 0x06 | the temperature of the gauge | 0.1 K, `u16` |

The registers come from the BQ25896 data sheet (SLUSC76C, table 17) and the BQ27220
technical reference manual (SLUUBD4A, table 2-1 and chapter 2). The firmware rounds the
voltage to 10 mV, the current to 10 mA and the temperature to 0.5 °C
(`logic::power::PowerMonitor`). A new charge state counts only after two equal reads in
a row, as the power source does, because `CHRG_STAT` can toggle at the end of a charge.
A voltage below 2000 mV is not valid, and the monitor keeps the last good value. It
sends `PowerRead` to the core only when a rounded value changes, and before a change of
the power source in the same pass. The core publishes the values to Home Assistant (`docs/mqtt.md`, "Home
Assistant device").

The sign of `AverageCurrent()` is still to check on the board. The manual tells that
the gauge is in its CHARGE mode when the current is positive, and in its DISCHARGE mode
when it is negative. Thus the current must be positive on the stand while the battery
charges, and negative on battery. Do the check in step 4.

Do these steps with the board on USB power:

1. Run `just flash`. The log shows `power: source Stand at the start`, then
   `power: battery N%`. The header shows the percent and a plug icon. Keep
   `force_battery = false` for this check.
2. Disconnect USB. In about 6 seconds, the header shows the percent and a battery
   icon. The fill of the icon shows the charge. The serial log is on USB, so you cannot
   see the log line `power: source Battery` now.
3. Connect USB again. In about 6 seconds, the header shows the plug icon. If the serial
   log connects again, it shows `power: source Stand`.

4. In Home Assistant, open the device of the dashboard and its "Diagnostic" entities.
   On USB power, "Charging" is on while the battery is not full, and "Battery current"
   is positive. Disconnect USB. In about 6 seconds, "Charging" is off. Tap the panel
   about each 20 seconds for 1 minute, so that the device stays awake. The gauge
   averages the current over about 15 seconds, and the dashboard sends a change of
   only the current at most once each 60 seconds. "Battery current" must then be
   negative. "Battery voltage" is about 3500 to 4200 mV, and "Gauge temperature" is
   near the room temperature.

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
   then stops. The line `power: sleep until [Touch, Timer { seconds: 60 }]` is a
   `DEBUG` line with the log target `dashboard::core_task`. It does not print with
   the default `ESP_LOG` (`info,core=debug`). To print it, add `dashboard=debug` to
   `ESP_LOG`. The last log lines before the sleep can be lost.
5. Tap a room button on the panel. The panel must show that room, so the tap that
   wakes the device also acts. In a few seconds the status is `online` again. If the
   log continues, it shows
   `power: woke by a touch after N ms, average current M mA` and
   `touch: wake tap at (x, y)`.
6. Wait 30 seconds again. The panel must show "Asleep" again.
7. Set `force_battery = false` in `cfg.toml` and run `just flash`.

### Drain while asleep

The wake line gives `AverageCurrent()` of the BQ27220 (command 0x14). The technical
reference manual gives the unit (mA) and the sign (negative for a discharge). The board
confirmed the unit and the sign of a discharge on 2026-11-22: off the stand, Home
Assistant showed −20 mA. The sign while the battery charges is not seen on the board yet
(refer to "Power check", step 4). The display task reads it directly after the wake, so the value is
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
5. If the light is too bright or too dim, set the tap levels in Home Assistant (the
   "Tap light on the stand" and "Tap light on battery" numbers of the device).
6. Disconnect USB and tap the panel. The light is dimmer (25%).
7. Do not touch the panel for 30 seconds. The light goes off at 20 seconds, and the
   panel shows "Asleep" at 30 seconds. In a dark room, make sure that the light stays
   off while the device sleeps.
8. Tap a room button. The light goes on, and the panel shows that room.
9. While the light is on, put the device on the stand. The light changes to the
   level of the stand.

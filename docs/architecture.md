# Architecture

This page tells how the dashboard is built: the parts of the system, the Crux core,
the two shells and how we test them. For the MQTT topics and payloads, refer to
[`mqtt.md`](mqtt.md). For the hardware, refer to [`firmware/README.md`](../firmware/README.md).

## The system

The dashboard is a Home Assistant panel on a LilyGo T5 E-Paper S3 Pro Lite (board
H752-01). The board has an ESP32-S3, a 960×540 e-paper panel with 16 grey levels, a
GT911 touch controller, a front light and a battery. It sits on a charging stand, or
it runs on its battery.

The dashboard and Home Assistant communicate only through MQTT. The Raspberry Pi runs
the Mosquitto broker and Home Assistant. On the Home Assistant side, these parts do
the work:

- The `mqtt_statestream` integration publishes the state of each entity on the
  dashboard.
- The automation `dashboard_statestream_seed` publishes the current states when a
  dashboard connects.
- The automation `dashboard_commands` does the service calls that the dashboard
  publishes.
- The automation `dashboard_log` writes the lines of the dashboard log to the Home
  Assistant log.
- MQTT discovery shows the dashboard as a device of Home Assistant, with sensors,
  buttons and a light.

The software has one core and two shells. The core (`shared`) holds all the behaviour.
The firmware shell runs the core on the board. The simulator shell runs the same core
in a window on a desktop. Both shells draw the view model with the same `ui` crate.

```text
 ┌──────────────────────────┐
 │ LilyGo board             │
 │   firmware shell         │──┐
 │   core (shared), ui      │  │ Wi-Fi, TCP          ┌──────────────────────────────┐
 └──────────────────────────┘  │                     │ Raspberry Pi                 │
                               ├────── MQTT ────────►│   Mosquitto broker           │
 ┌──────────────────────────┐  │                     │   Home Assistant:            │
 │ Desktop                  │  │                     │     mqtt_statestream         │
 │   simulator shell (egui) │──┘                     │     MQTT discovery           │
 │   core (shared), ui      │                        │     dashboard_* automations  │
 └──────────────────────────┘                        └──────────────────────────────┘
```

[`mqtt.md`](mqtt.md) gives each topic between the dashboard and Home Assistant, with its
payloads. It is the contract between the two.

## The repo

| Path | What it holds |
| --- | --- |
| `shared/` | The Crux core: the events, the effects, the model, the features, the view model and the sans-io MQTT client. It builds with or without `std`. |
| `ui/` | The drawing of the view model on the panel, the hit test of a touch, and the refresh decision of the panel (`Refresher`). It has the golden-image tests. |
| `shells/sim/` | The desktop simulator: an egui window, a PNG mode and a smoke mode. |
| `firmware/logic/` | The logic of the firmware shell that does not touch the hardware. It is `no_std` and has host tests. |
| `firmware/src/` | The hardware binary for the ESP32-S3 (crate `dashboard-firmware`): the tasks, the drivers and the panel driver. `firmware/` is its own Cargo workspace. |
| `docs/` | This page and the MQTT topic contract. |
| `scripts/ha_entities.rs` | Prints the entities of the dashboard for the Pi setup (`just ha-entities`). |
| `Justfile` | The recipes of the host workspace. `firmware/Justfile` has the firmware recipes. |

The core, `ui` and `firmware/logic` are members of the root workspace. The firmware
binary is not a member, because feature unification with the `std` shells would pull
`std` into it.

## The core

The core is the `Dashboard` app in `shared/src/app.rs`. It has no I/O. It takes events,
changes its model, and returns commands that ask the shell for effects.

Both shells are Rust and link the core directly. There is no FFI boundary and no type
generation. Thus `Event`, `Effect` and `ViewModel` have no serde or `Facet` derives.

### Events

The public variants of `Event` are facts from the shell and intents of the user:

| Event | Meaning |
| --- | --- |
| `Start` | The shell started. It gives the settings, the firmware facts, the reset reason and the power source. |
| `PowerSourceChanged` | The device moved to the stand or off the stand. |
| `BatteryChanged` | The charge of the battery changed. |
| `PowerRead` | The shell read new power diagnostics. |
| `Tapped` | The user tapped the panel, on a `Control` or on no control. |
| `NextPage`, `PreviousPage`, `Navigate` | The user asked for a page. |
| `SetReadOnly` | The user started or stopped a dry run. |
| `SleepNow` | The user asked the device to sleep now. |
| `ShellLogged` | The shell wrote a warn or error line of its own log. |

The other variants (`Wifi`, `Connection`, `Power`, `Refresh`, `Pending`, `Light`,
`Logs` and `Device`) wrap the internal events of the features. The core makes them
from the answers to its effects. The shell never sends them.

### Effects and capabilities

Each `Effect` variant wraps one operation. The custom capabilities are in
`shared/src/capabilities/`. Each shell implements the other half.

| Capability | Effects | What the shell does |
| --- | --- | --- |
| `Render` (Crux) | `Render` | Draws the view model, with a partial or a full refresh. |
| `stream` | `StreamOpen`, `StreamWrite`, `StreamClose` | Opens one TCP stream to the broker, writes the bytes of the core, gives back the bytes that arrive, and closes it. It knows nothing of MQTT. |
| `radio` | `RadioJoin`, `RadioAddress`, `RadioLeave`, `RadioScan`, `RadioStopScan`, `RadioReadSignal`, `RadioOff`, `RadioRestart` | Does the Wi-Fi radio work. The core decides when to join, which access point to join, when to scan, when to read the signal and when to restart the chip. |
| `power` | `Sleep` | Puts the device to sleep until a touch or a timer, and answers with the `Wake`. A touch wake can be a tap on the panel or a press of the touch key. |
| `light` | `FrontLight` | Sets the brightness of the front light. |
| `log` | `Log`, `LogClock` | Writes a line of the core to the shell log and answers with its monotonic time. |
| `crux_time` | `TimeNotifyAfter`, `TimeClear` | Starts and clears the timers of the core. |

### The view model

`view()` makes the `ViewModel` from the model (`shared/src/view.rs`). The view model
has all the text that the panel shows: the page, the tiles, the status text, the
power icon and the charge. It also has `full_refresh`, which tells the shell to do a
full refresh of the panel, and its depth: light (1 flash) or deep (4 flashes). The `ui`
crate only draws it.

### Features over one model

The core uses features over one model, not nested state machines. Each feature is a
module in `shared/src/features/` that guards one part of `Model`:

- Each feature, except `rooms`, has its own `State` in the model, its own internal
  `Event`, and one `pub fn update(event, model: &mut Model) -> Cmd`.
- The feature functions take the whole `&mut Model` and return the root `Cmd`.
- `From<feature::Event> for crate::Event` lifts the events, so a feature uses
  `.into()`, not `.map_event(..)`.
- `Model::update` in `app.rs` is a dispatcher. It sends each event to a feature and
  has no logic of its own.

The features act together, and this is the reason for the pattern. A sleep in `power`
turns off the front light, closes the connection, turns off the radio and draws the
last image. With `.map_event(..)`, a `power` function could not return a `connection`
command. Each step that crosses features would go up to the parent as an outcome, and
the parent `update` would become a large orchestrator.

| Feature | What it owns | What it drives |
| --- | --- | --- |
| `power` | The power source, the idle timer, the steps of a sleep, the wakes and the hourly report | `light` (off), `connection` (go offline, go online), `wifi` (stop the scan, radio off), `refresh` (the last image), `device` |
| `connection` | The MQTT `Session`, the outbox of service calls, the reconnects and the messages from the state stream | `wifi` (ask for the link, report a quiet link or a failure), `pending` (queue and confirm the marks), `device` (the device publish queue, the commands from Home Assistant), `power` (a failed report) |
| `wifi` | The Wi-Fi policy: the access point cache, the health of the link, the roam scan and the restart of the chip | `connection` (link up, link failed, close for a roam), `power` (the next step of a sleep), `device` |
| `rooms` | The taps on the controls and the page changes. It has no state or events of its own. The route is in `Model`. | `connection` (send a service call), `pending` (mark the tap), `light` (touch), `power` (wake, restart the idle timer), `device` |
| `key` | The touch key on the front of the device: the press in progress and its long-press timer. A short press shows the index page. A long press asks for a deep full refresh. | `power` (wake, restart the idle timer), `light` (touch), `rooms` (the index page), `device` (the full refresh, with the rules of the Full refresh button) |
| `pending` | The marks of taps that Home Assistant did not confirm yet | `power` (sleep when the marks are settled) |
| `light` | The front light policy: the tap level, the light timer, the light that Home Assistant holds | `device` (publish the light state) |
| `refresh` | The choice of a partial or a full refresh, and when to draw | `power` (sleep when the draw is done) |
| `logs` | The ring of log lines of the core and the shell, and their ages | `connection` (start `connection::pump`) |
| `device` | The dashboard as a device of Home Assistant: discovery, the state message, the buttons and the device publish queue (`device::drain`) | `logs` and `light` (as publishers in the queue), `power` (Sleep now), `connection` (restart, pump), `wifi` (read the signal before a state message) |

`shared/src/features/timer.rs` is a helper, not a feature. It gives a timer that the
core can clear, held in one `Slot`.

Two things take the place of typed boundaries between features:

- Each feature has a small `pub fn` surface, and other features call only that surface.
- Each feature keeps its state in enums with no invalid states. For example, the
  connection is in one phase at a time (`Offline`, `Starting`, `Up`, `Closing` or
  `Draining`), and each phase holds only its own data.

Nested state machines, as in the Crux [weather example][weather] (with its `Started`,
`Outcome` and `Status` types), fit parts that exclude each other. An example is a
lifecycle level (setup, running) above the features. If the core gets such a level, put
it above the features, and do not mix the two patterns at one level. `.map_effect(..)`
is for a child module with its own smaller `Effect` type. This core has one `Effect`
enum, so it needs no `.map_effect(..)`.

## Crux and Clean Architecture

Crux follows the dependency rule of Clean Architecture strictly. The core never
depends on a shell.

| Clean Architecture ring | In this app |
| --- | --- |
| Entities and use cases (policy) | The core: `Model`, the features and their decisions |
| Input port (the use-case boundary) | The public variants of `Event` (`Start`, `Tapped`, `BatteryChanged` and the others) |
| Output ports (gateway interfaces) | The capabilities and their operations (`Open`, `Write`, `Join`, `Sleep`, `FrontLight`, `Log`, the timers, `Render`) |
| Presenter output | `view()` gives the `ViewModel` |
| Adapters, frameworks and drivers | The shells: the firmware (esp-hal, the TCP stack, the panel driver) and the egui simulator |

Crux goes one step further than the usual recipe. A use case does not call a gateway
interface. It returns a `Command`, which describes the port calls, and the shell does
them. Thus the inner rings are pure, and the tests need no mocks. This is a functional
core and an imperative shell, on top of ports and adapters.

The project rule "logic in the core, drivers in the shells" is the dependency rule:
policy goes inward, and drivers go outward. Two examples:

- The firmware `Sleeper` decided the sleep schedule, which is a business rule in the
  driver ring. PR #51 moved the schedule into the core. `Sleeper`
  (`firmware/logic/src/sleep.rs`) now keeps only driver guards, for example "do not
  sleep while the GT911 holds INT low".
- Three firmware tasks shared the Wi-Fi decisions, and no host test covered them
  together. PR #34 moved the Wi-Fi policy into the `wifi` feature. The radio task now
  does one job at a time and decides nothing.

The core also holds some interface-adapter code: the sans-io MQTT client
(`shared/src/mqtt/`) and the JSON payloads for Home Assistant. In strict terms, this
code belongs to the adapter ring. But the rings are about the direction of the
dependencies, not about the crate that holds the code. A sans-io adapter is pure and
depends on nothing outward, so it can live in the core, and we can test it there. The
shell keeps only the true driver: the TCP socket.

### The two structures in the inner rings

Both structures keep the dependency rule. They differ in how they split the use cases
and how they bound one use case from another.

- **Nested state machines are separate use-case components with typed boundaries.** Each
  stage has its own state, and it talks to its parent only through a typed result. It
  does not know its siblings. Dependencies go one way, from parent to child, and the
  compiler enforces the boundaries. This structure fits a domain of independent use
  cases.
- **Features over one model are domain services over one aggregate.** `Model` acts as
  the aggregate root, and each feature guards its own part. Features depend on each
  other inside one ring (`power` calls `connection`, `wifi` and `light`). Clean
  Architecture permits this inside a layer, but the acyclic dependencies principle still
  applies. Rust modules permit cycles, so only discipline prevents them.

There is one cycle to watch. `connection::pump` calls `device::drain`, which asks
`logs` for its next line. When a line arrives, `logs` calls `connection::pump`. The
cycle is harmless now, but it is the kind of dependency that needs attention.

In summary, nested state machines give boundaries that the compiler checks, which is
correct when the use cases are independent. Features over one model give one
consistent aggregate with conventional boundaries, which is correct when the use cases
must act together, as they do on this device.

## The shells

Each shell performs the effects and sends the facts of its platform as events. The rule
is: logic in the core, drivers in the shells. If a decision can be pure, it goes in the
core, where a test can reach it without hardware.

### The firmware

The firmware runs the core on the ESP32-S3 with esp-hal and Embassy tasks. One
task owns the core through `logic::shell::Shell`. The other tasks never see the core.
Each task sends a `Signal` to the core task through a queue, and the core task gives
each task its work as a `Job`.

| Task | Work |
| --- | --- |
| core | Owns the shell, routes each effect to a task, and sends the warn and error lines of the firmware to the core |
| `timers` | Starts and cancels the timers of the core |
| `display` | Draws the view model with `ui::draw`, refreshes the panel as `ui::Refresher` decides, reads the touch controller, sets the front light and runs the `Sleep` job |
| `radio` | Does the radio jobs, with no Wi-Fi policy |
| `stream` | Opens one TCP stream at a time and moves bytes |
| `net` | Runs the network stack |

Other facts about the firmware:

- On battery, the chip uses light sleep. Light sleep keeps the RAM, so the core and its
  model stay. The core decides what each wake means.
- The panel driver in `firmware/src/panel/` is a copy of part of the
  `lilygo-t5s3paperpro` crate, under GPL-3.0.
- The firmware keeps some facts in RTC memory, which a reset does not clear: the reset
  reason, the restart count, the last warn and error lines, and the panic flag. The
  core gets them in `Start`.
- `firmware/logic` holds the parts of the shell that do not touch the hardware, for
  example the effect routing, the touch debounce and the sleep guards. They have host
  tests.

[`firmware/README.md`](../firmware/README.md) has the detail.

### The simulator

The simulator (`shells/sim/`) runs the same core in an egui window, connected to the
live broker. A click is a tap. The window also shows the view model, the effect log and
the MQTT log. The simulator has these parts in place of hardware:

- A `FakeRadio` answers each radio request at once.
- A check timer answers a `Sleep` request with `Wake::Timer`, as the firmware does.
- Keys change the fake hardware. For example, `S` moves the device on or off the stand,
  `L` drops the Wi-Fi link and `Z` ends the idle time so that the core sleeps.
- `--png` draws a page with fixed entity states to a PNG file, with no window and no
  broker. `--smoke` connects to the broker, draws the page to a PNG file and stops.

Run `cargo run -p sim -- --help` for all the keys and options. Run `just sim` to start
the window.

## Testing

The core has most of the tests, because it has most of the logic.

- **Feature tests** drive the core one step at a time with the `EffectTestExt` helpers
  that Crux generates (`expect_*`, `resolve_*`). The feature tests are in
  `shared/src/features/<name>/tests.rs`. `Wire` (`shared/src/tests/wire.rs`) connects
  the byte stream of a test to the fake broker, and its `ExpectWrite` helpers check the
  MQTT packets in a `Write`.
- **Flow tests** (`shared/src/tests/flows.rs` and the other files in
  `shared/src/tests/`) run the core in a test shell (`shared/src/tests/shell.rs`). The
  test shell answers each effect as a real shell does, with a fake clock, the fake
  broker, a fake Home Assistant and the fake radio. The fakes are in `shared/src/fake/`.
- **Test-shell asserts** check rules that span features. For example, the test shell
  panics if a timer of the core waits at a `Sleep` request.
- **Capability tests** (`shared/src/capabilities/log.rs`, `radio.rs` and `stream.rs`)
  test each command builder with no app. A test makes the `Command` with a small
  `Effect` type of its own, takes the request from `command.effects()` and resolves it.
  Then it checks the events from `command.events()` and that `command.is_done()`.
- **MQTT client tests** (`shared/src/mqtt/tests/`) run the `Session` against the fake
  broker.
- **Golden-image tests** (`ui/tests/golden.rs`) draw views from the real core with
  `FixedShell` (`shared/src/fixtures.rs`) and compare each image with its reference PNG
  in `ui/tests/golden/`. To make the references again, run `just golden`, then examine
  the new images before you commit them.
- **Firmware logic tests** run on the host (`just test-firmware`).

Run `just dev` before each commit. It formats the code, checks the types and the lints,
checks the `no_std` build of the core, `ui` and `firmware/logic`, builds the workspace
and runs all the tests.

## Complexity

Features over one model trade structural complexity for coupling complexity. Nested
state machines make the opposite trade.

| | Features over one model (this app) | Nested state machines |
| --- | --- | --- |
| Simpler | No `Outcome` or transition types, and no code that puts a state back. A step that crosses features is a direct call and `.and(..)`. | Local reasoning: the signature of a machine shows its state and its events, and the compiler stops it from touching anything else. |
| | A flow reads in one place. "What happens at a sleep" reads from top to bottom in `power`. | Small tests of one machine, with small fixtures. |
| | Shared facts (the settings, the power source, the connection state) are in the model, with no code to pass them down. | No cycles: dependencies go only from parent to child. |
| Harder | The coupling is implicit. Any feature can call any other feature, so to find the effect of a change, you search the callers. | Boilerplate at each layer: outcome and transition types, and the lift of events and transitions. |
| | A rule that spans features is not in the types. "No core timer waits at a `Sleep`" needs a test-shell assert. "No device publish while asleep" depends on `device::drain` checking the model. | A behaviour that spans parts is scattered. The story of one sleep is in several files and types. |
| | Hub functions grow. `connection::pump` drains the outbox and then the device publish queue. | Shared data must be passed down or copied, so a change to a rule that spans parts touches several layers. |
| | Most tests are whole flows. They are thorough, but they need a large test shell and a fake broker, and a failure takes longer to trace. | |

The complexity of this core is in the coupling. These things keep it under control:

- state enums with no invalid states
- a small `pub fn` surface for each feature
- flow tests, and test-shell asserts such as the timer check at a `Sleep`
- a log line for each decision

The thing to watch is a hub function such as `connection::pump`. A new kind of device
publish goes in the device publish queue, not in `pump`. Nested state machines would
replace this risk with ceremony, and they would make the flows of the whole device
(sleep, wake, reconnect) harder to follow. These flows are the most important code to
read in this app.

## Where to read next

- [`mqtt.md`](mqtt.md): the MQTT topic contract, the Home Assistant automations and the
  device entities.
- [`firmware/README.md`](../firmware/README.md): the tasks, the light sleep, the restart
  facts, the panel driver and the bring-up checks.
- The module docs of `shared/src/features/`: each feature tells its rules, and some have
  a state table.
- The [Crux book](https://redbadger.github.io/crux/latest_master/) and the
  [weather example][weather].

[weather]: https://github.com/redbadger/crux/tree/master/examples/weather

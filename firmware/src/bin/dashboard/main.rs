//! The dashboard firmware.
//!
//! One task owns the core (through `logic::shell::Shell`). The other tasks
//! never see the core. They send a `Signal` to the core task through
//! [`SIGNALS`], and the core task gives them work through their own lanes:
//!
//! | Task | Work | Answers |
//! | --- | --- | --- |
//! | `core` | owns the shell, routes each job, and gives the warn and error lines of the logger to the core | |
//! | `timers` | `StartTimer`, `CancelTimer` | `Signal::Timer` |
//! | `display` | draws the latest view model, reads the touch controller, the charger and the fuel gauge, sets the front light, and runs the `Sleep` job | `Signal::Tap`, `Signal::Event`, `Signal::Woke`, `Signal::Light` |
//! | `radio` | the radio jobs (`Join`, `Address`, `Leave`, `Scan`, `StopScan`, `Off`) and `Restart`, with no Wi-Fi policy | `Signal::Radio` |
//! | `stream` | the TCP stream to the broker, one at a time: `Open`, `Write`, `Close` | `Signal::Stream`, `Signal::Written` |
//! | `net` | runs the network stack | |
//!
//! The core runs the MQTT session, and the stream task moves only bytes. The
//! core also decides each Wi-Fi step (the access point, the scan, the change
//! of access point and the restart of the chip), and the radio task only does
//! the work of the Wi-Fi driver.

#![no_std]
#![no_main]
#![allow(clippy::future_not_send)]

extern crate alloc;

mod core_task;
mod display;
mod radio;
mod stream;
mod timers;

use dashboard_firmware::{board, lane::Lane, light::FrontLight, logger, panel::Panel, panel_pins};
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal as Latest};
use esp_backtrace as _;
use logic::{
    shell::{Signal, StreamOp},
    sleep::{InFlight, Plan},
};
use shared::{PowerSource, ViewModel};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// The build-time configuration, which `build.rs` reads from `cfg.toml`.
mod config {
    include!(concat!(env!("OUT_DIR"), "/config.rs"));
}

/// The memory of the network stack.
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

/// The signals for the core task.
pub static SIGNALS: Lane<Signal> = Lane::new();
/// The events that the display task sent in [`SIGNALS`], and that the core
/// task did not handle yet. The chip does not sleep while one waits.
pub static IN_FLIGHT: InFlight = InFlight::new();
/// The newest view model to draw.
///
/// A newer view model replaces one that the display task did not draw yet.
/// The view model stays in its box from the
/// core task to the display task, so it is not copied on the stack.
pub static RENDER: Latest<CriticalSectionRawMutex, alloc::boxed::Box<ViewModel>> = Latest::new();
/// The work of the timer task.
pub static TIMERS: Lane<timers::Request> = Lane::new();
/// The work of the stream task, with the connection id of the core.
pub static STREAM: Lane<(u32, StreamOp)> = Lane::new();
/// The work of the radio task: the radio jobs, with their tokens, and
/// `Restart`.
pub static RADIO: Lane<radio::Request> = Lane::new();
/// The `FrontLight` jobs for the display task, with their tokens.
pub static LIGHT: Lane<(u32, u8)> = Lane::new();
/// The `Sleep` job for the display task, or `None` when the `Sleep`
/// request ended. A newer value replaces one that the task did not read yet.
pub static SLEEP: Latest<CriticalSectionRawMutex, Option<Plan>> = Latest::new();

/// The number of sockets. `embassy_net::new` takes one socket for DNS, and
/// one for DHCP while DHCP runs. The stream task has one TCP socket at a
/// time. That is 3 at most, and there is 1 spare.
const SOCKETS: usize = 4;

#[esp_hal::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = board::init();
    // From now, the warn and error lines wait for the core, also the lines
    // before the core starts.
    logger::forward();
    board::start(
        peripherals.PSRAM,
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
    );
    log::info!("dashboard {} starts", env!("CARGO_PKG_VERSION"));

    let pins = panel_pins!(peripherals);
    let mut panel = match Panel::new(
        pins,
        peripherals.I2C0,
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
    ) {
        Ok(panel) => Some(panel),
        Err(error) => {
            log::error!("panel: cannot start: {error:?}");
            None
        }
    };

    // The display task reads the charger after this. The core starts with
    // this source, so the display task sends only a change.
    let source = if config::CONFIG.force_battery {
        log::warn!("power: force_battery is true, so the core is on battery also on USB");
        PowerSource::Battery
    } else {
        match &mut panel {
            Some(panel) => first_source(panel).await,
            None => None,
        }
        .unwrap_or_else(|| {
            log::warn!("power: the source is not known, so the core starts on the stand");
            PowerSource::Stand
        })
    };
    log::info!("power: source {source:?} at the start");

    let interface = esp_radio::wifi::Interface::station();

    let rng = esp_hal::rng::Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        interface,
        embassy_net::Config::dhcpv4(embassy_net::DhcpConfig::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner).expect("spawn the network task"));
    spawner.spawn(radio::task(peripherals.WIFI, stack).expect("spawn the radio task"));
    spawner.spawn(stream::task(stack).expect("spawn the stream task"));
    spawner.spawn(timers::task().expect("spawn the timer task"));
    // The display task drives the front light, because it must turn the
    // light off before each light sleep.
    let has_display = panel.is_some();
    if let Some(panel) = panel {
        let sleep = display::Sleep::new(peripherals.LPWR, config::CONFIG.force_battery);
        let light = match FrontLight::new(peripherals.LEDC, peripherals.GPIO11) {
            Ok(light) => Some(light),
            Err(error) => {
                log::error!("light: cannot start: {error:?}");
                None
            }
        };
        spawner.spawn(display::task(panel, source, sleep, light).expect("spawn the display task"));
    }
    core_task::run(source, has_display).await
}

/// The number of tries of the first read of the charger.
const SOURCE_TRIES: u32 = 3;

/// Reads the power source from the charger. If a read fails, waits a short
/// time and tries again, [`SOURCE_TRIES`] times in total. Logs only the last
/// failure.
async fn first_source(panel: &mut Panel<'static>) -> Option<PowerSource> {
    for attempt in 1..=SOURCE_TRIES {
        if let Some(source) = display::read_source(panel, attempt == SOURCE_TRIES) {
            return Some(source);
        }
        if attempt < SOURCE_TRIES {
            embassy_time::Timer::after(embassy_time::Duration::from_millis(20)).await;
        }
    }
    None
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await;
}

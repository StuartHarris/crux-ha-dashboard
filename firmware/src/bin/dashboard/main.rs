//! The dashboard firmware.
//!
//! One task owns the core (through `logic::shell::Shell`). The other tasks
//! never see the core. They send a `Signal` to the core task through
//! [`SIGNALS`], and the core task gives them work through their own lanes:
//!
//! | Task | Work | Answers |
//! | --- | --- | --- |
//! | `core` | owns the shell and routes each job | |
//! | `timers` | `StartTimer`, `CancelTimer` | `Signal::Timer` |
//! | `display` | draws the latest view model, reads the touch controller, writes the RTC | `Signal::Tap` |
//! | `wifi` | `WifiOn`, `WifiOff` | `Signal::Wifi` |
//! | `mqtt` | the MQTT operations, one connection at a time | `Signal::Mqtt` |
//! | `sntp` | sets the clock from an SNTP server | |
//! | `net` | runs the network stack | |

#![no_std]
#![no_main]
#![allow(clippy::future_not_send)]

extern crate alloc;

mod clock;
mod core_task;
mod display;
mod mqtt;
mod sntp;
mod timers;
mod wifi;

use dashboard_firmware::{board, lane::Lane, network, panel::Panel, panel_pins, rtc};
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal as Latest};
use esp_backtrace as _;
use esp_radio::wifi::WifiController;
use logic::{
    shell::{MqttOp, Signal},
    wifi::WifiRequest,
};
use shared::ViewModel;
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
/// The newest view model to draw.
///
/// A newer view model replaces one that the display task did not draw yet.
/// The view model stays in its box from the
/// core task to the display task, so it is not copied on the stack.
pub static RENDER: Latest<CriticalSectionRawMutex, alloc::boxed::Box<ViewModel>> = Latest::new();
/// The work of the timer task.
pub static TIMERS: Lane<timers::Request> = Lane::new();
/// The work of the MQTT task, with the id of the connection.
pub static MQTT: Lane<(u32, MqttOp)> = Lane::new();
/// The work of the Wi-Fi task.
pub static WIFI: Lane<WifiRequest> = Lane::new();
/// A request to the display task, which owns the I2C bus, to compare the
/// RTC with the clock and to write the RTC if necessary. The SNTP task
/// sends it after it sets the clock.
///
/// If the panel does not start, the display task does not run, and no task
/// takes the request. Then the RTC is not written, and SNTP sets the clock
/// after each start. This is acceptable.
pub static RTC_WRITE: Latest<CriticalSectionRawMutex, ()> = Latest::new();

/// The number of sockets. `embassy_net::new` takes one socket for DNS, and
/// one for DHCP while DHCP runs. The MQTT task has one TCP socket at a
/// time, and the SNTP task has one UDP socket during a request. That is 4
/// at most, and there is 1 spare.
const SOCKETS: usize = 5;

#[esp_hal::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = board::init();
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
    // The panel owns the I2C bus, so the RTC read uses it. The display task
    // takes the panel later.
    if let Some(panel) = &mut panel {
        read_rtc(panel);
    }
    clock::log_not_set();

    let controller = WifiController::new(
        peripherals.WIFI,
        network::controller_config(&config::CONFIG),
    )
    .expect("start the Wi-Fi driver");
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
    spawner.spawn(wifi::task(controller, stack).expect("spawn the Wi-Fi task"));
    spawner.spawn(mqtt::task(stack).expect("spawn the MQTT task"));
    spawner.spawn(sntp::task(stack).expect("spawn the SNTP task"));
    spawner.spawn(timers::task().expect("spawn the timer task"));
    if let Some(panel) = panel {
        spawner.spawn(display::task(panel).expect("spawn the display task"));
    }
    core_task::run().await
}

/// Sets the clock from the RTC, if the RTC has a valid time.
fn read_rtc(panel: &mut Panel<'_>) {
    match rtc::read(panel) {
        Ok(block) => match logic::clock::start_from_rtc(
            logic::clock::pcf8563::decode_block(&block),
            clock::uptime_micros(),
        ) {
            Some(start) => clock::set(start, clock::Source::Rtc),
            None => log::warn!("rtc: no valid time"),
        },
        Err(error) => log::warn!("rtc: read failed: {error:?}"),
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await;
}

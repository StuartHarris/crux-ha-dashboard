//! Starts the chip: the CPU clock, the heap in internal RAM and in PSRAM, the
//! log and the scheduler.

use alloc::string::String;

use esp_hal::{
    clock::CpuClock,
    efuse::{self, InterfaceMacAddress},
    peripherals::{FROM_CPU_INTR0, PSRAM, Peripherals, TIMG0},
    psram::{PsramConfig, PsramMode},
    timer::timg::TimerGroup,
};
use shared::{Firmware, capabilities::log::Uptime};

/// The size of the heap in internal RAM. The Wi-Fi driver needs internal RAM
/// for its buffers. With Wi-Fi and MQTT connected, about 29 KiB of it is free
/// (measured on 2026-11-18).
pub const INTERNAL_HEAP: usize = 72 * 1024;

/// Starts the log and the clocks, and returns the peripherals.
///
/// The log only prints. The dashboard then calls [`crate::logger::forward`]
/// to send its warn and error lines to the core.
///
/// Give `PSRAM`, `TIMG0` and `FROM_CPU_INTR0` to [`start`] next.
#[must_use]
pub fn init() -> Peripherals {
    crate::logger::init();
    esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()))
}

/// Returns the time since the chip started, in milliseconds.
///
/// The time comes from the system timer of esp-hal. At a wake from light
/// sleep, esp-hal moves the system timer forward by the time asleep, so the
/// time counts through a sleep. Before `esp_hal::init` sets the scale of
/// the timer, the time is not correct.
#[must_use]
pub fn uptime() -> Uptime {
    Uptime(
        esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis(),
    )
}

/// The version of the firmware: the version of the package and a short
/// commit id, for example `0.1.0+1b073946` or `0.1.0+1b073946.dirty`.
/// `build.rs` makes it.
pub const VERSION: &str = env!("FIRMWARE_VERSION");

/// Returns the facts about the firmware for `Start`: [`VERSION`] and the MAC
/// address of the Wi-Fi station interface.
#[must_use]
pub fn firmware() -> Firmware {
    let station = efuse::interface_mac_address(InterfaceMacAddress::Station);
    let mut mac = [0; 6];
    mac.copy_from_slice(station.as_bytes());
    Firmware {
        version: String::from(VERSION),
        mac,
    }
}

/// Starts the heap and the scheduler.
///
/// The heap has two regions. The allocator uses the first region that has
/// space, so the PSRAM region (8 MB, octal) is first. Thus the internal region
/// stays free for the Wi-Fi driver and the scheduler, which ask for internal
/// RAM explicitly.
///
/// Do not add the internal region first. The core, the stream buffers and
/// the strings then fill it, and the Wi-Fi driver cannot get receive
/// buffers. The device can then send packets but cannot receive them.
pub fn start(psram: PSRAM<'static>, timg0: TIMG0<'static>, interrupt: FROM_CPU_INTR0<'static>) {
    // The board has an ESP32-S3-WROOM-1-N16R8, which has octal PSRAM. The
    // automatic detection is not reliable, so the mode is fixed.
    let config = PsramConfig {
        mode: PsramMode::OctalSpi,
        ..PsramConfig::default()
    };
    esp_alloc::psram_allocator!(psram, esp_hal::psram, config);
    esp_alloc::heap_allocator!(size: INTERNAL_HEAP);
    log::info!("heap: {}", esp_alloc::HEAP.stats());

    let timers = TimerGroup::new(timg0);
    esp_rtos::start(timers.timer0, interrupt);
}

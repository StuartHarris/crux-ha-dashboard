//! Starts the chip: the CPU clock, the heap in internal RAM and in PSRAM, the
//! log and the scheduler.

use esp_hal::{
    clock::CpuClock,
    peripherals::{FROM_CPU_INTR0, PSRAM, Peripherals, TIMG0},
    psram::{PsramConfig, PsramMode},
    timer::timg::TimerGroup,
};

/// The size of the heap in internal RAM. The Wi-Fi driver needs internal RAM
/// for its buffers. With Wi-Fi and MQTT connected, about 29 KiB of it is free
/// (measured on 2026-11-18).
pub const INTERNAL_HEAP: usize = 72 * 1024;

/// Starts the log and the clocks, and returns the peripherals.
///
/// Give `PSRAM`, `TIMG0` and `FROM_CPU_INTR0` to [`start`] next.
#[must_use]
pub fn init() -> Peripherals {
    esp_println::logger::init_logger_from_env();
    esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()))
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

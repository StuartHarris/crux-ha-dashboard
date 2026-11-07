//! Starts the chip: the CPU clock, the heap in internal RAM and in PSRAM, the
//! log and the scheduler.

use esp_hal::{
    clock::CpuClock,
    peripherals::{FROM_CPU_INTR0, PSRAM, Peripherals, TIMG0},
    psram::{PsramConfig, PsramMode},
    timer::timg::TimerGroup,
};

/// The size of the heap in internal RAM. The Wi-Fi driver needs internal RAM
/// for its buffers.
// TODO(5b): measure the free internal heap with Wi-Fi and MQTT connected.
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
/// The heap has two regions. The allocator uses the internal region first.
/// A large allocation that does not fit there goes to the 8 MB of octal
/// PSRAM. The framebuffers (259,200 bytes each) go there. The panel driver
/// puts its waveform table (65,536 bytes) in PSRAM explicitly, because it
/// fits in the internal region.
pub fn start(psram: PSRAM<'static>, timg0: TIMG0<'static>, interrupt: FROM_CPU_INTR0<'static>) {
    esp_alloc::heap_allocator!(size: INTERNAL_HEAP);
    // The board has an ESP32-S3-WROOM-1-N16R8, which has octal PSRAM. The
    // automatic detection is not reliable, so the mode is fixed.
    // TODO(5b): check that the log shows "PSRAM size: 8 MB".
    let config = PsramConfig {
        mode: PsramMode::OctalSpi,
        ..PsramConfig::default()
    };
    esp_alloc::psram_allocator!(psram, esp_hal::psram, config);
    log::info!("heap: {}", esp_alloc::HEAP.stats());

    let timers = TimerGroup::new(timg0);
    esp_rtos::start(timers.timer0, interrupt);
}

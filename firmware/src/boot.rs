//! The memory that keeps its values through a reset, in RTC fast memory.
//!
//! The rules are in [`logic::boot`], with host tests. This module only
//! holds the statics and connects them to esp-hal:
//!
//! - [`read`] reads the reset reason and the memory once at the start. It
//!   gives the facts for `Start`, and starts the record of lines.
//! - [`record`] keeps a warn or error line of the logger.
//! - [`stable`] tells the guard against a loop of panics that the uptime
//!   reached [`STABLE_AFTER`](logic::boot::STABLE_AFTER). The core task
//!   decides when, with [`logic::boot::Uptime`].
//! - `custom_halt` runs at the end of each panic. It restarts the chip, or
//!   stops it after [`PANIC_LIMIT`](logic::boot::PANIC_LIMIT) panics in a
//!   row.
//!
//! The statics are atomics in `.rtc_fast.persistent`. esp-hal fills them
//! with zeros at a power-on, and keeps them through each other reset. They
//! need no `unsafe` block.

use core::sync::atomic::{AtomicBool, Ordering};

use esp_hal::{delay::Delay, ram};
use logic::{
    boot::{AfterPanic, LINES, PANIC_LIMIT, Rtc, SLOT, Taken},
    forward::Line,
};
use portable_atomic::{AtomicU8, AtomicU32};

#[ram(unstable(rtc_fast, persistent))]
static MAGIC: AtomicU32 = AtomicU32::new(0);

#[ram(unstable(rtc_fast, persistent))]
static RESTARTS: AtomicU32 = AtomicU32::new(0);

#[ram(unstable(rtc_fast, persistent))]
static PANICKED: AtomicU32 = AtomicU32::new(0);

#[ram(unstable(rtc_fast, persistent))]
static PANICS: AtomicU32 = AtomicU32::new(0);

#[ram(unstable(rtc_fast, persistent))]
static HEAD: AtomicU32 = AtomicU32::new(0);

#[ram(unstable(rtc_fast, persistent))]
static SLOTS: [[AtomicU8; SLOT]; LINES] = [const { [const { AtomicU8::new(0) }; SLOT] }; LINES];

/// `true` after [`read`]. Before, [`record`] keeps no line, so a line of
/// this start does not mix with the lines from before the restart.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// The time for the serial port to print the panic before the restart.
const PANIC_PRINT_MS: u32 = 200;

/// Returns the memory as the rules of [`logic::boot`] use it.
const fn rtc() -> Rtc<'static> {
    Rtc {
        magic: &MAGIC,
        restarts: &RESTARTS,
        panicked: &PANICKED,
        panics: &PANICS,
        head: &HEAD,
        slots: &SLOTS,
    }
}

/// Reads the reset reason and the memory, and starts the record of lines.
/// Refer to [`Rtc::boot`].
///
/// Call it once, after `esp_hal::init` and before the first warn line of
/// this start. It does not allocate, so the heap does not need to be ready.
#[must_use]
pub fn read() -> Taken {
    let raw = esp_hal::system::reset_reason().map(|reason| reason as u32);
    let taken = rtc().boot(raw);
    RECORDING.store(true, Ordering::Release);
    taken
}

/// Returns `true` after [`read`], when the ring keeps the warn and error
/// lines.
pub fn recording() -> bool {
    RECORDING.load(Ordering::Acquire)
}

/// Keeps a warn or error line in the ring, after [`read`]. The logger calls
/// it in a critical section, so two lines do not write the ring at the same
/// time.
pub fn record(line: &Line) {
    if recording() {
        rtc().record(line);
    }
}

/// The start is stable, so the count of panics in a row starts again at 0.
/// Refer to [`Rtc::stable`].
pub fn stable() {
    rtc().stable();
}

/// Ends each panic, after esp-backtrace printed it and its backtrace.
///
/// esp-backtrace calls this function with its `custom-halt` feature. It
/// finds the function by its symbol name, so the function must not be
/// mangled. `#[unsafe(no_mangle)]` is an unsafe attribute, because two
/// items with one symbol name are undefined behaviour. No other item in the
/// firmware has the name `custom_halt`.
///
/// The function counts the panic and sets the panic flag in the memory
/// ([`Rtc::panicked`]). Then it waits for the serial port, and restarts the
/// chip, so the next start reports `Panic`. After [`PANIC_LIMIT`] panics in
/// a row, it stops the chip and does not set the flag, so a panic at each
/// start cannot empty the battery. Before [`read`] (in the bring-up), it
/// stops the chip too. Refer to [`halt`].
#[unsafe(no_mangle)]
#[expect(
    clippy::no_mangle_with_rust_abi,
    reason = "esp-backtrace declares `custom_halt` in an `extern \"Rust\"` block"
)]
pub fn custom_halt() -> ! {
    // The bring-up does not call `read`, so a panic there stops the chip as
    // before, and the memory does not change.
    let after = if recording() {
        rtc().panicked()
    } else {
        AfterPanic::Halt
    };
    match after {
        AfterPanic::Restart => {
            esp_println::println!("panic: restart the chip");
            Delay::new().delay_millis(PANIC_PRINT_MS);
            esp_hal::system::software_reset()
        }
        AfterPanic::Halt => {
            if recording() {
                esp_println::println!("panic: {PANIC_LIMIT} panics in a row, so the chip stops");
            }
            halt()
        }
    }
}

/// Stops the CPU until a reset, with as little power as is safe in a panic.
///
/// The function masks each interrupt (`PS.INTLEVEL` in the critical
/// section) and also disables each one (`INTENABLE` = 0). Then it waits in
/// `waiti`, which stops the clock of the CPU until an interrupt. No
/// interrupt is enabled, so the CPU stays stopped. `waiti 0` lowers
/// `PS.INTLEVEL` to 0, so `INTENABLE` = 0 is necessary: without it, the
/// tasks of the scheduler and of the Wi-Fi driver would run again after the
/// panic. The loop is for an interrupt that cannot be disabled (the debug
/// interrupt of a debugger).
///
/// The function does not stop the Wi-Fi radio. The radio task owns the
/// controller, and the panic can come from inside the driver, so a call to
/// the driver here is not safe. With no interrupts, the driver does no more
/// work, and the access point drops the device after its time limit.
fn halt() -> ! {
    // No watchdog resets the stopped chip: `esp_hal::init` disables the RTC
    // watchdog, the super watchdog and the watchdogs of timer groups 0 and 1
    // (esp-hal 1.2.2, `src/lib.rs`, "Disable watchdog timers"), and the
    // firmware enables none.
    critical_section::with(|_| {
        xtensa_lx::interrupt::disable_mask(u32::MAX);
        loop {
            esp_hal::interrupt::wait_for_interrupt();
        }
    })
}

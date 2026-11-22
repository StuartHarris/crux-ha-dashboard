//! Tests of the boot facts: the reset reason, the count of restarts, the
//! ring of lines that a restart keeps, and the guard against a loop of
//! panics.

use alloc::{format, string::String, vec::Vec};
use core::{array, time::Duration};

use portable_atomic::{AtomicU8, AtomicU32, Ordering};
use shared::{
    Boot, BootLine, ResetReason,
    features::logs::{Kept, MAX_MESSAGE},
};

use super::{
    AfterPanic, LINES, MAGIC, MESSAGE, PANIC_LIMIT, Rtc, SLOT, STABLE_AFTER, Uptime, kept,
    reset_reason,
};
use crate::forward::Line;

/// The memory of the tests, as the firmware has it in RTC fast memory.
struct Memory {
    magic: AtomicU32,
    restarts: AtomicU32,
    panicked: AtomicU32,
    panics: AtomicU32,
    head: AtomicU32,
    slots: [[AtomicU8; SLOT]; LINES],
}

impl Memory {
    /// Memory with all bytes 0, as esp-hal leaves it after a power-on.
    fn zeroed() -> Self {
        Self::filled(0)
    }

    /// Memory with each byte `byte`, as the memory of another firmware.
    fn filled(byte: u8) -> Self {
        let word = u32::from_ne_bytes([byte; 4]);
        Self {
            magic: AtomicU32::new(word),
            restarts: AtomicU32::new(word),
            panicked: AtomicU32::new(word),
            panics: AtomicU32::new(word),
            head: AtomicU32::new(word),
            slots: array::from_fn(|_| array::from_fn(|_| AtomicU8::new(byte))),
        }
    }

    const fn rtc(&self) -> Rtc<'_> {
        Rtc {
            magic: &self.magic,
            restarts: &self.restarts,
            panicked: &self.panicked,
            panics: &self.panics,
            head: &self.head,
            slots: &self.slots,
        }
    }

    /// A power-on: esp-hal fills the memory with zeros, then the firmware
    /// reads it.
    fn power_on(&self) -> (Boot, Vec<BootLine>) {
        for word in [
            &self.magic,
            &self.restarts,
            &self.panicked,
            &self.panics,
            &self.head,
        ] {
            word.store(0, Ordering::Relaxed);
        }
        for byte in self.slots.iter().flatten() {
            byte.store(0, Ordering::Relaxed);
        }
        self.boot(Some(0x01))
    }

    /// A start after a reset with the raw reason `raw`.
    fn boot(&self, raw: Option<u32>) -> (Boot, Vec<BootLine>) {
        let taken = self.rtc().boot(raw);
        (taken.boot, taken.lines())
    }

    /// Keeps a line as the logger does.
    fn record(&self, level: Kept, target: &str, text: &str) {
        self.rtc()
            .record(&Line::kept(level, target, format_args!("{text}")));
    }
}

/// The raw reason of a software reset of the CPU.
const CPU_SW: Option<u32> = Some(0x0C);

/// The raw reason of a reset by the watchdog of timer group 0.
const CORE_MWDT0: Option<u32> = Some(0x07);

/// The raw reason of a reset from the USB port (`CoreUsbUart`), for example
/// a reset by hand with the tools.
const CORE_USB_UART: Option<u32> = Some(0x15);

fn boot_line(level: Kept, source: &str, message: &str) -> BootLine {
    BootLine {
        level,
        source: source.into(),
        message: message.into(),
    }
}

// The reset reason.

#[test]
fn each_raw_reason_maps_to_its_kind() {
    let cases = [
        (None, ResetReason::PowerOn),
        (Some(0x01), ResetReason::PowerOn),
        (Some(0x03), ResetReason::Software),
        (Some(0x0C), ResetReason::Software),
        (Some(0x07), ResetReason::Watchdog),
        (Some(0x08), ResetReason::Watchdog),
        (Some(0x09), ResetReason::Watchdog),
        (Some(0x0B), ResetReason::Watchdog),
        (Some(0x0D), ResetReason::Watchdog),
        (Some(0x10), ResetReason::Watchdog),
        (Some(0x11), ResetReason::Watchdog),
        (Some(0x12), ResetReason::Watchdog),
        (Some(0x0F), ResetReason::Brownout),
        (Some(0x05), ResetReason::Other),
        (Some(0x13), ResetReason::Other),
        (Some(0x15), ResetReason::Other),
        (Some(0x16), ResetReason::Other),
        (Some(0x17), ResetReason::Other),
        (Some(0xFF), ResetReason::Other),
    ];
    for (raw, reason) in cases {
        assert_eq!(reset_reason(raw, false), reason, "{raw:?}");
    }
}

#[test]
fn a_panic_flag_gives_a_panic_except_after_a_power_on() {
    assert_eq!(reset_reason(CPU_SW, true), ResetReason::Panic);
    assert_eq!(reset_reason(CORE_MWDT0, true), ResetReason::Panic);
    assert_eq!(reset_reason(Some(0x01), true), ResetReason::PowerOn);
    assert_eq!(reset_reason(None, true), ResetReason::PowerOn);
}

// The count of restarts.

#[test]
fn each_reset_that_is_not_a_power_on_counts_and_a_power_on_starts_at_0() {
    let memory = Memory::zeroed();
    let boot = |reset, restarts| Boot { reset, restarts };
    assert_eq!(memory.power_on().0, boot(ResetReason::PowerOn, 0));
    assert_eq!(memory.boot(CPU_SW).0, boot(ResetReason::Software, 1));
    assert_eq!(memory.boot(CORE_MWDT0).0, boot(ResetReason::Watchdog, 2));
    assert_eq!(memory.boot(Some(0x16)).0, boot(ResetReason::Other, 3));
    assert_eq!(memory.power_on().0, boot(ResetReason::PowerOn, 0));
    assert_eq!(memory.boot(CPU_SW).0, boot(ResetReason::Software, 1));
}

#[test]
fn memory_of_another_firmware_is_cleared() {
    // A reset from the USB port after a flash keeps the memory of the old
    // firmware. Its bytes are not valid here.
    let memory = Memory::filled(0xA5);
    let (boot, lines) = memory.boot(Some(0x16));
    assert_eq!(
        boot,
        Boot {
            reset: ResetReason::Other,
            restarts: 1
        }
    );
    assert_eq!(lines, Vec::new());
    assert_eq!(memory.magic.load(Ordering::Relaxed), MAGIC);
    assert_eq!(memory.panics.load(Ordering::Relaxed), 0);
}

// The ring of lines.

#[test]
fn the_lines_before_a_restart_come_back_once_from_the_oldest() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    memory.record(Kept::Warn, "dashboard::radio", "wifi: join failed: timeout");
    memory.record(
        Kept::Warn,
        "core",
        "wifi: restart the chip: no address after 3 joins",
    );
    memory.record(Kept::Error, "esp_radio::wifi", "driver error 12");

    let (boot, lines) = memory.boot(CPU_SW);
    assert_eq!(boot.reset, ResetReason::Software);
    assert_eq!(
        lines,
        [
            boot_line(Kept::Warn, "wifi", "join failed: timeout"),
            // A line of the core has its area as the source.
            boot_line(
                Kept::Warn,
                "wifi",
                "restart the chip: no address after 3 joins"
            ),
            // A line of another crate has the crate as the source.
            boot_line(Kept::Error, "esp_radio", "driver error 12"),
        ]
    );

    // The start took the lines, so the next start has none.
    assert_eq!(memory.boot(CPU_SW).1, Vec::new());
}

#[test]
fn the_ring_keeps_the_last_8_lines() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for index in 0..LINES + 3 {
        memory.record(Kept::Warn, "dashboard", &format!("line {index}"));
    }
    let lines: Vec<String> = memory
        .boot(CORE_MWDT0)
        .1
        .into_iter()
        .map(|line| line.message)
        .collect();
    let expected: Vec<String> = (3..LINES + 3)
        .map(|index| format!("line {index}"))
        .collect();
    assert_eq!(LINES, 8);
    assert_eq!(lines, expected);
}

#[test]
fn a_power_on_gives_no_lines() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    memory.record(Kept::Warn, "dashboard", "power: a line");
    // The power came off: the memory lost its content.
    assert_eq!(memory.power_on().1, Vec::new());

    // A power-on that the chip reports while the memory kept its content
    // (a brownout of the whole chip can do that) still gives no lines.
    memory.record(Kept::Warn, "dashboard", "power: a line");
    assert_eq!(memory.boot(Some(0x01)).1, Vec::new());
    assert_eq!(memory.boot(CPU_SW).1, Vec::new());
}

#[test]
fn only_warn_and_error_lines_go_into_the_ring() {
    assert_eq!(kept(log::Level::Error), Some(Kept::Error));
    assert_eq!(kept(log::Level::Warn), Some(Kept::Warn));
    for level in [log::Level::Info, log::Level::Debug, log::Level::Trace] {
        assert_eq!(kept(level), None, "{level}");
    }
}

#[test]
fn an_info_line_is_not_kept() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    memory.record(Kept::Info, "core", "power: started after a power-on");
    assert_eq!(memory.boot(CPU_SW).1, Vec::new());
}

#[test]
fn a_place_that_is_not_valid_is_dropped() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for index in 0..4 {
        memory.record(Kept::Warn, "dashboard", &format!("power: line {index}"));
    }
    // A reset during a write: the level byte is still 0.
    memory.slots[0][0].store(0, Ordering::Relaxed);
    // A level that is not valid.
    memory.slots[1][0].store(7, Ordering::Relaxed);
    // A length that is too long.
    memory.slots[2][2].store(u8::MAX, Ordering::Relaxed);
    let lines = memory.boot(CPU_SW).1;
    assert_eq!(lines, [boot_line(Kept::Warn, "power", "line 3")]);

    // Bytes that are not UTF-8.
    memory.record(Kept::Warn, "dashboard", "power: line 4");
    memory.slots[0][3].store(0xFF, Ordering::Relaxed);
    assert_eq!(memory.boot(CPU_SW).1, Vec::new());
}

#[test]
fn a_long_line_keeps_the_bytes_of_the_forward_so_the_core_cuts_it() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    let long = "é".repeat(300);
    memory.record(Kept::Error, "dashboard", &long);
    let lines = memory.boot(CPU_SW).1;
    assert_eq!(lines.len(), 1);
    let message = &lines[0].message;
    // The ring keeps more than the core keeps, on a character boundary.
    assert!(message.len() > MAX_MESSAGE);
    assert!(message.len() <= MESSAGE);
    assert!(long.starts_with(message.as_str()));
}

// The guard against a loop of panics.

#[test]
fn a_panic_restarts_and_the_next_start_reports_it() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
    // The panic restarts the chip with a software reset.
    assert_eq!(
        memory.boot(CPU_SW).0,
        Boot {
            reset: ResetReason::Panic,
            restarts: 1
        }
    );
    // The start cleared the flag, so the next software reset is not a
    // panic.
    assert_eq!(memory.boot(CPU_SW).0.reset, ResetReason::Software);
}

#[test]
fn the_third_panic_in_a_row_halts() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 1..PANIC_LIMIT {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        assert_eq!(memory.boot(CPU_SW).0.reset, ResetReason::Panic);
    }
    assert_eq!(PANIC_LIMIT, 3);
    assert_eq!(memory.rtc().panicked(), AfterPanic::Halt);
}

#[test]
fn a_stable_start_between_panics_starts_the_count_again() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 0..5 {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        let _ = memory.boot(CPU_SW);
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        let _ = memory.boot(CPU_SW);
        memory.rtc().stable();
    }
    assert_eq!(memory.panics.load(Ordering::Relaxed), 0);
}

/// Returns `secs` seconds as a duration.
const fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

#[test]
fn a_panic_5_s_after_each_connect_reaches_the_limit_and_halts() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    let mut after = AfterPanic::Restart;
    for start in 1..=PANIC_LIMIT {
        // Each start connects after 20 s, and panics 5 s later. The firmware
        // tells the memory when the start is stable, as `core_task` does.
        let mut uptime = Uptime::default();
        for now in [secs(0), secs(20), secs(25)] {
            if uptime.tick(now) {
                memory.rtc().stable();
            }
        }
        after = memory.rtc().panicked();
        if start < PANIC_LIMIT {
            assert_eq!(after, AfterPanic::Restart);
            assert_eq!(memory.boot(CPU_SW).0.reset, ResetReason::Panic);
        }
    }
    assert_eq!(after, AfterPanic::Halt);
}

#[test]
fn ten_minutes_of_uptime_resets_the_count_once() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 1..PANIC_LIMIT {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        let _ = memory.boot(CPU_SW);
    }
    // The next panic would halt. The uptime counts also while the board
    // sleeps, so the board needs no link for the rule.
    let mut uptime = Uptime::default();
    assert!(!uptime.tick(secs(60)));
    assert!(!uptime.tick(secs(599)));
    assert!(uptime.tick(STABLE_AFTER));
    memory.rtc().stable();
    assert_eq!(STABLE_AFTER, secs(600));
    // The start is stable once. A later tick does not reset the count
    // again.
    assert!(!uptime.tick(secs(660)));
    assert!(!uptime.tick(secs(2000)));

    assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
    let _ = memory.boot(CPU_SW);
    assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
}

#[test]
fn a_watchdog_reset_between_panics_keeps_the_count() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 1..PANIC_LIMIT {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        assert_eq!(memory.boot(CPU_SW).0.reset, ResetReason::Panic);
        // A watchdog reset comes before the next panic.
        assert_eq!(memory.boot(CORE_MWDT0).0.reset, ResetReason::Watchdog);
    }
    assert_eq!(memory.rtc().panicked(), AfterPanic::Halt);
}

#[test]
fn a_reset_by_hand_after_a_halt_keeps_the_count_and_is_not_a_panic() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 1..PANIC_LIMIT {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        let _ = memory.boot(CPU_SW);
    }
    assert_eq!(memory.rtc().panicked(), AfterPanic::Halt);
    // The chip stops. A reset by hand starts it again, and the start does
    // not report a panic.
    assert_eq!(
        memory.boot(CORE_USB_UART).0,
        Boot {
            reset: ResetReason::Other,
            restarts: 3
        }
    );
    // The count stays at the limit, so the next panic stops the chip at
    // once.
    assert_eq!(memory.panics.load(Ordering::Relaxed), PANIC_LIMIT);
    assert_eq!(memory.rtc().panicked(), AfterPanic::Halt);
}

#[test]
fn a_power_on_code_clears_the_count_also_if_the_memory_stays() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    for _ in 1..PANIC_LIMIT {
        assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
        let _ = memory.boot(CPU_SW);
    }
    // A brownout of the whole chip can give the code of a power-on while
    // the memory keeps its values.
    assert_eq!(memory.boot(Some(0x01)).0.reset, ResetReason::PowerOn);
    assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
}

#[test]
fn a_power_on_clears_the_count_of_panics() {
    let memory = Memory::zeroed();
    let _ = memory.power_on();
    let _ = memory.rtc().panicked();
    let _ = memory.rtc().panicked();
    let _ = memory.power_on();
    assert_eq!(memory.rtc().panicked(), AfterPanic::Restart);
}

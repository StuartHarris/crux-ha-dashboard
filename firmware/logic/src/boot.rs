//! The facts about the start of the chip, and the memory that keeps them
//! through a reset.
//!
//! The firmware keeps a few values in RTC fast memory that a reset does not
//! clear (`#[ram(unstable(rtc_fast, persistent))]` in esp-hal 1.2). esp-hal
//! fills that memory with zeros at a power-on, and also when it cannot read
//! the reset reason. This module has the rules for that memory. The
//! firmware gives it the statics, so the rules have host tests.
//!
//! - [`reset_reason`] maps the reset reason of the chip to a
//!   [`ResetReason`] of the core.
//! - [`Rtc::boot`] reads the memory once at the start: it counts the
//!   restarts, takes the lines that a restart kept, and clears them.
//! - [`Rtc::record`] keeps a warn or error line in a ring of [`LINES`]
//!   lines. The newest line replaces the oldest line.
//! - [`Rtc::panicked`] and [`Rtc::connected`] are the guard against a loop
//!   of panics. Refer to [`PANIC_LIMIT`].
//!
//! # The memory is not trusted
//!
//! A reset after a new firmware is flashed is not a power-on (for example
//! a reset from the USB port), so the new firmware can read the memory of
//! the old one at other addresses. The memory starts with [`MAGIC`]. If the
//! magic is not there, [`Rtc::boot`] clears all the values. Each line also
//! has its level byte last: [`Rtc::record`] sets it to 0 first, writes the
//! line, and then sets the level. A reset during the write thus leaves an
//! empty place, not half a line. [`Rtc::boot`] checks the level, the
//! lengths and the UTF-8 of each line, and drops a line that is not valid.
//!
//! All the values are atomics, because esp-hal accepts only integers,
//! atomics and arrays of them in that memory (its `Persistable` trait). The
//! firmware then needs no `unsafe` block for them.

use alloc::{string::String, vec::Vec};

use portable_atomic::{AtomicU8, AtomicU32, Ordering};
use shared::{Boot, BootLine, ResetReason, features::logs::Kept};

use crate::forward::{Line, MESSAGE, SOURCE, Text};

/// The number of lines that the ring keeps through a restart.
pub const LINES: usize = 8;

/// The bytes of one place of the ring: the level, the length of the source,
/// the length of the message, the source and the message.
pub const SLOT: usize = 3 + SOURCE + MESSAGE;

/// The value at the start of the memory. It has the version of the layout
/// in its low byte, so a firmware with another layout clears the memory.
pub const MAGIC: u32 = 0x4442_5401;

/// The number of panics in a row, with no MQTT connect between them, after
/// which a panic stops the chip and does not restart it.
///
/// A panic restarts the chip, so the board recovers by itself on battery.
/// If the panic occurs before the first connect at each start, the
/// restarts empty the battery. So the third panic in a row stops the chip, as
/// a panic did before this guard. The first MQTT connect after a start sets
/// the count to 0 ([`Rtc::connected`]).
pub const PANIC_LIMIT: u32 = 3;

/// The raw reset reasons of the ESP32-S3 (`SocResetReason` in esp-hal).
mod code {
    pub const POWER_ON: u32 = 0x01;
    pub const CORE_SW: u32 = 0x03;
    pub const CORE_MWDT0: u32 = 0x07;
    pub const CORE_MWDT1: u32 = 0x08;
    pub const CORE_RTC_WDT: u32 = 0x09;
    pub const CPU_MWDT0: u32 = 0x0B;
    pub const CPU_SW: u32 = 0x0C;
    pub const CPU_RTC_WDT: u32 = 0x0D;
    pub const SYS_BROWN_OUT: u32 = 0x0F;
    pub const SYS_RTC_WDT: u32 = 0x10;
    pub const CPU_MWDT1: u32 = 0x11;
    pub const SYS_SUPER_WDT: u32 = 0x12;
}

/// Returns the reset reason for the core from the raw reason `raw` of the
/// chip (`esp_hal::system::reset_reason()` as a number), or `None` if the
/// chip cannot tell.
///
/// - `None` and a power-on give `PowerOn`, because esp-hal then fills the
///   memory with zeros. On the ESP32-S3, a brownout of the whole chip can
///   also give the code of a power-on.
/// - If `panicked` is `true`, the last start ended with a panic, and the
///   panic restarted the chip. That restart is a software reset, so a
///   panic gives `Panic` for each reason except a power-on.
/// - The software resets give `Software`, and each watchdog gives
///   `Watchdog`. A brownout of the digital core gives `Brownout`.
/// - Each other reason gives `Other`, for example a reset from the USB
///   port, a glitch of the clock or of the power, or a deep sleep.
#[must_use]
pub const fn reset_reason(raw: Option<u32>, panicked: bool) -> ResetReason {
    let Some(raw) = raw else {
        return ResetReason::PowerOn;
    };
    if raw == code::POWER_ON {
        return ResetReason::PowerOn;
    }
    if panicked {
        return ResetReason::Panic;
    }
    match raw {
        code::CORE_SW | code::CPU_SW => ResetReason::Software,
        code::CORE_MWDT0
        | code::CORE_MWDT1
        | code::CORE_RTC_WDT
        | code::CPU_MWDT0
        | code::CPU_RTC_WDT
        | code::SYS_RTC_WDT
        | code::CPU_MWDT1
        | code::SYS_SUPER_WDT => ResetReason::Watchdog,
        code::SYS_BROWN_OUT => ResetReason::Brownout,
        _ => ResetReason::Other,
    }
}

/// Returns the level in the ring of a line with the level `level`, or
/// `None` if the line does not go into the ring.
///
/// The ring keeps each warn and error line, of any target. A line of the
/// core (target `core`) goes into the ring too, although it does not go to
/// the core as `ShellLogged`.
#[must_use]
pub const fn kept(level: log::Level) -> Option<Kept> {
    match level {
        log::Level::Error => Some(Kept::Error),
        log::Level::Warn => Some(Kept::Warn),
        log::Level::Info | log::Level::Debug | log::Level::Trace => None,
    }
}

/// What a panic does. Refer to [`Rtc::panicked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterPanic {
    /// Restart the chip. The next start reports `Panic`.
    Restart,
    /// Stop the chip and do not restart it, because [`PANIC_LIMIT`] panics
    /// came in a row with no MQTT connect.
    Halt,
}

/// The memory that keeps its values through a reset. The firmware puts
/// each field in RTC fast memory.
pub struct Rtc<'a> {
    /// [`MAGIC`] when the values are valid.
    pub magic: &'a AtomicU32,
    /// The number of resets that were not a power-on, since the last
    /// power-on.
    pub restarts: &'a AtomicU32,
    /// 1 if the last start ended with a panic that restarts the chip.
    pub panicked: &'a AtomicU32,
    /// The number of panics in a row with no MQTT connect.
    pub panics: &'a AtomicU32,
    /// The place of the ring for the next line.
    pub head: &'a AtomicU32,
    /// The places of the ring. Byte 0 is the level (0 for no line, 1 for
    /// `Warn`, 2 for `Error`), byte 1 the length of the source, byte 2 the
    /// length of the message, then the bytes of the source and of the
    /// message.
    pub slots: &'a [[AtomicU8; SLOT]; LINES],
}

/// What [`Rtc::boot`] read: the boot facts and the lines from before the
/// restart, in fixed buffers. The heap can be not ready when the firmware
/// reads the memory, so this type does not allocate.
#[derive(Clone, Copy, Debug)]
pub struct Taken {
    /// The facts for `Start`.
    pub boot: Boot,
    /// The lines from before the restart, from the oldest.
    lines: [Line; LINES],
    /// The number of lines in `lines`.
    len: usize,
}

impl Taken {
    /// Returns the lines from before the restart, from the oldest, for
    /// [`shared::Start::before_restart`].
    #[must_use]
    pub fn lines(&self) -> Vec<BootLine> {
        self.lines[..self.len]
            .iter()
            .map(|line| BootLine {
                level: line.level,
                source: String::from(line.source.as_str()),
                message: String::from(line.message.as_str()),
            })
            .collect()
    }
}

/// Returns the byte of the level of a line. The ring keeps no `Info`
/// line, so `Info` gives 0, the byte of no line.
const fn level_byte(level: Kept) -> u8 {
    match level {
        Kept::Info => 0,
        Kept::Warn => 1,
        Kept::Error => 2,
    }
}

/// Returns the level of the byte `byte`, or `None` for no line or a byte
/// that is not valid. The ring keeps only warn and error lines.
const fn byte_level(byte: u8) -> Option<Kept> {
    match byte {
        1 => Some(Kept::Warn),
        2 => Some(Kept::Error),
        _ => None,
    }
}

impl Rtc<'_> {
    /// Reads the memory at the start of the chip, after the reset with the
    /// raw reason `raw` (refer to [`reset_reason`]).
    ///
    /// 1. If the memory does not start with [`MAGIC`], clears all the
    ///    values and writes the magic.
    /// 2. Maps the reason, with the panic flag of the last start, and
    ///    clears the flag.
    /// 3. Counts the restart: 0 after a power-on, else 1 more.
    /// 4. Takes the lines of the ring, from the oldest, and clears the ring.
    ///    After a power-on, no line is taken.
    ///
    /// Call it once, before the logger keeps the first line of this start.
    #[must_use]
    pub fn boot(&self, raw: Option<u32>) -> Taken {
        if self.magic.load(Ordering::Relaxed) != MAGIC {
            self.restarts.store(0, Ordering::Relaxed);
            self.panicked.store(0, Ordering::Relaxed);
            self.panics.store(0, Ordering::Relaxed);
            self.clear_ring();
            self.magic.store(MAGIC, Ordering::Relaxed);
        }
        let panicked = self.panicked.swap(0, Ordering::Relaxed) != 0;
        let reset = reset_reason(raw, panicked);
        let restarts = if reset == ResetReason::PowerOn {
            0
        } else {
            self.restarts.load(Ordering::Relaxed).saturating_add(1)
        };
        self.restarts.store(restarts, Ordering::Relaxed);
        let mut taken = Taken {
            boot: Boot { reset, restarts },
            lines: [Line::EMPTY; LINES],
            len: 0,
        };
        if reset != ResetReason::PowerOn {
            let head = self.head.load(Ordering::Relaxed) as usize % LINES;
            for offset in 0..LINES {
                if let Some(line) = Self::read(&self.slots[(head + offset) % LINES]) {
                    taken.lines[taken.len] = line;
                    taken.len += 1;
                }
            }
        }
        self.clear_ring();
        taken
    }

    /// Keeps `line` in the ring, in the place of the oldest line. The
    /// caller must not call it from two contexts at the same time (the
    /// logger holds a critical section).
    ///
    /// The level byte goes last, so a reset during the write leaves an
    /// empty place. A line that is not `Warn` or `Error` is not kept.
    pub fn record(&self, line: &Line) {
        if !matches!(line.level, Kept::Warn | Kept::Error) {
            return;
        }
        let head = self.head.load(Ordering::Relaxed) as usize % LINES;
        let slot = &self.slots[head];
        slot[0].store(0, Ordering::Relaxed);
        let source = line.source.as_str().as_bytes();
        let message = line.message.as_str().as_bytes();
        // `Line` keeps at most `SOURCE` and `MESSAGE` bytes, which fit a
        // byte each.
        slot[1].store(u8::try_from(source.len()).unwrap_or(0), Ordering::Relaxed);
        slot[2].store(u8::try_from(message.len()).unwrap_or(0), Ordering::Relaxed);
        for (place, byte) in slot[3..].iter().zip(source) {
            place.store(*byte, Ordering::Relaxed);
        }
        for (place, byte) in slot[3 + SOURCE..].iter().zip(message) {
            place.store(*byte, Ordering::Relaxed);
        }
        slot[0].store(level_byte(line.level), Ordering::Release);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the place is less than LINES"
        )]
        self.head
            .store(((head + 1) % LINES) as u32, Ordering::Relaxed);
    }

    /// Counts a panic, sets the panic flag, and returns what the panic
    /// does: a restart, or a stop after [`PANIC_LIMIT`] panics in a row.
    #[must_use]
    pub fn panicked(&self) -> AfterPanic {
        let panics = self.panics.load(Ordering::Relaxed).saturating_add(1);
        self.panics.store(panics, Ordering::Relaxed);
        self.panicked.store(1, Ordering::Relaxed);
        if panics >= PANIC_LIMIT {
            AfterPanic::Halt
        } else {
            AfterPanic::Restart
        }
    }

    /// The core made an MQTT connect, so the start works. The count of
    /// panics in a row starts again at 0.
    pub fn connected(&self) {
        self.panics.store(0, Ordering::Relaxed);
    }

    /// Returns the line in `slot`, or `None` if the place has no valid
    /// line.
    fn read(slot: &[AtomicU8; SLOT]) -> Option<Line> {
        let level = byte_level(slot[0].load(Ordering::Acquire))?;
        let source_len = usize::from(slot[1].load(Ordering::Relaxed));
        let message_len = usize::from(slot[2].load(Ordering::Relaxed));
        if source_len > SOURCE || message_len > MESSAGE {
            return None;
        }
        let mut source = [0; SOURCE];
        for (byte, place) in source.iter_mut().zip(&slot[3..3 + source_len]) {
            *byte = place.load(Ordering::Relaxed);
        }
        let mut message = [0; MESSAGE];
        for (byte, place) in message
            .iter_mut()
            .zip(&slot[3 + SOURCE..3 + SOURCE + message_len])
        {
            *byte = place.load(Ordering::Relaxed);
        }
        let source = core::str::from_utf8(&source[..source_len]).ok()?;
        let message = core::str::from_utf8(&message[..message_len]).ok()?;
        Some(Line {
            level,
            source: Text::of(source),
            message: Text::of(message),
            ..Line::EMPTY
        })
    }

    /// Clears each place of the ring and the head.
    fn clear_ring(&self) {
        for slot in self.slots {
            slot[0].store(0, Ordering::Relaxed);
        }
        self.head.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;

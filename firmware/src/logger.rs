//! The logger of the firmware: it prints each line to the serial port, and
//! keeps each warn and error line for the core.
//!
//! The printed line is the same as the line of the logger of esp-println
//! 0.18 with its `colors` feature: `<colour><LEVEL> - <message><reset>`.
//! The logger reads the levels from `ESP_LOG` at build time, with the same
//! rules (refer to [`logic::forward::Filter`]), with one change: a target
//! matches only if it is equal, or if it ends at `::`. Thus the serial log
//! does not change, and `grep -a 'stream:'` still finds the same lines.
//!
//! The filter reads `ESP_LOG` at compile time. The `core=debug` directive
//! makes the `log` facade pass each `Debug` line, also of other crates, to
//! the logger. The logger drops such a line at once, with no string
//! parsing. The `log` macros call only [`log::Log::log`], so `log` makes
//! the same check. [`log::Log::enabled`] makes it for code that asks.
//!
//! The dashboard calls [`forward`] just after `board::init`. From then, the
//! logger also keeps each line that [`logic::forward::forwarded`] accepts,
//! in a queue of [`logic::forward::PENDING`] lines. A line before
//! [`forward`] only prints. The core task waits for a line with [`wait`],
//! and takes each line as an event for the core with [`take`].
//!
//! The forward does not depend on `ESP_LOG`. A warn or error line that
//! `ESP_LOG` does not print still goes to the core. Refer to
//! [`logic::forward::route`].
//!
//! # The ring that a restart keeps
//!
//! The logger also keeps each warn and error line, also a line of the core
//! and a line that `ESP_LOG` does not print, in a ring of 8 lines in RTC
//! memory ([`crate::boot::record`]). [`forward`] sets the maximum level of
//! the `log` facade to at least `Warn`, so each such line comes to the
//! logger. The ring keeps its lines through a reset that
//! is not a power-on, so the next start gives them to the core in `Start`.
//! The ring starts to keep lines at [`crate::boot::read`].
//!
//! # Where a line can come from
//!
//! A line can come from any task of Embassy, from a thread of esp-rtos (for
//! example the Wi-Fi driver), and before the heap is ready. The heap of
//! esp-alloc has a lock that is not reentrant: it turns off the interrupts,
//! and it panics if the same context takes it again. A line from inside an
//! allocation would thus panic if the logger allocated.
//!
//! So the logger does not allocate. It writes the line into fixed buffers
//! ([`logic::forward::Line`]), and keeps it in a fixed queue behind a
//! critical section. The core task makes the event, which allocates, in its
//! own task.
//!
//! A line that comes while the logger keeps another line prints, but the
//! logger does not keep it. This includes a line from inside the work of the
//! logger itself, and a line from a thread that runs in that short time. The
//! logger counts such a line as lost. The event of the last line that waits
//! then tells the core.

use core::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use critical_section::Mutex;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use logic::forward::{self, Filter, Line, Pending, Route};
use shared::Event;

use crate::board::uptime;

/// The levels from `ESP_LOG`, read once at build time.
static FILTER: Filter<'static> = Filter::new(option_env!("ESP_LOG"));

/// The lines that wait for the core task.
static PENDING: Mutex<RefCell<Pending>> = Mutex::new(RefCell::new(Pending::new()));

/// Wakes the core task when the logger keeps a line.
static READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// `true` after [`forward`]. Before, the logger only prints.
static FORWARD: AtomicBool = AtomicBool::new(false);

/// `true` while the logger keeps a line.
static BUSY: AtomicBool = AtomicBool::new(false);

/// The lines that the logger did not keep because it was busy.
static LOST: AtomicU32 = AtomicU32::new(0);

/// The logger.
static LOGGER: Logger = Logger;

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let Route { print, forward } = route(metadata.level(), metadata.target());
        print || forward.is_some() || ring(metadata.level()).is_some()
    }

    fn log(&self, record: &log::Record<'_>) {
        // The macros of `log` do not call `enabled`, so this drops a line
        // that neither prints nor goes to the core.
        if !self.enabled(record.metadata()) {
            return;
        }
        let Route {
            print: prints,
            forward,
        } = route(record.level(), record.target());
        if prints {
            print(record);
        }
        if let Some(level) = forward {
            keep(level, record);
        }
        if let Some(level) = ring(record.level()) {
            persist(level, record);
        }
    }

    fn flush(&self) {}
}

/// Returns what the logger does with a line with `level` and `target`.
fn route(level: log::Level, target: &str) -> Route {
    forward::route(&FILTER, FORWARD.load(Ordering::Relaxed), level, target)
}

/// Installs the logger, and sets the maximum level from `ESP_LOG`. The
/// logger only prints until [`forward`].
///
/// If a logger is installed already, the function does nothing.
pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(forward::max_level(&FILTER, false));
    }
}

/// Starts to keep the warn and error lines for the core. Also sets the
/// maximum level of the `log` facade to at least `Warn`, so each warn line
/// comes to the logger, also if `ESP_LOG` does not print it.
///
/// Call it after `esp_hal::init`, because the time of a line
/// ([`uptime`]) is not correct before. Only the dashboard calls it, because
/// the bring-up has no core.
pub fn forward() {
    FORWARD.store(true, Ordering::Relaxed);
    log::set_max_level(forward::max_level(&FILTER, true));
}

/// Waits until the logger keeps a line. The wait can end with no line, if
/// the core task took the line already.
pub async fn wait() {
    READY.wait().await;
}

/// Takes the oldest line that waits, as an event for the core, or `None`.
///
/// The event of the last line that waits has the number of lines that the
/// logger lost. These lines came after the lines that wait, so the core
/// adds the count after that line. Each other event has the count 0.
#[must_use]
pub fn take() -> Option<Event> {
    let lost = LOST.swap(0, Ordering::Relaxed);
    let (line, dropped) = critical_section::with(|cs| {
        let mut pending = PENDING.borrow_ref_mut(cs);
        pending.lose(lost);
        pending.pop()
    })?;
    Some(line.event(dropped))
}

/// Returns the level in the ring that a restart keeps, or `None` if a line
/// with `level` does not go into it. The ring keeps each warn and error
/// line of any target, also if it does not print, from
/// [`crate::boot::read`]. Refer to [`logic::boot::kept`].
fn ring(level: log::Level) -> Option<shared::features::logs::Kept> {
    if crate::boot::recording() {
        logic::boot::kept(level)
    } else {
        None
    }
}

/// Keeps `record` in the ring that a restart keeps. The write is in a
/// critical section, so a line from another context does not write the
/// ring at the same time. The ring does not allocate.
fn persist(level: shared::features::logs::Kept, record: &log::Record<'_>) {
    let line = Line::kept(level, record.target(), *record.args());
    critical_section::with(|_| crate::boot::record(&line));
}

/// Prints `record` as the logger of esp-println does.
fn print(record: &log::Record<'_>) {
    const RESET: &str = "\u{001B}[0m";
    let colour = match record.level() {
        log::Level::Error => "\u{001B}[31m",
        log::Level::Warn => "\u{001B}[33m",
        log::Level::Info => "\u{001B}[32m",
        log::Level::Debug => "\u{001B}[34m",
        log::Level::Trace => "\u{001B}[35m",
    };
    esp_println::println!("{}{} - {}{}", colour, record.level(), record.args(), RESET);
}

/// Keeps `record` for the core at `level`, and wakes the core task.
///
/// If the logger keeps another line now, it only counts the line as lost.
fn keep(level: shared::features::logs::Kept, record: &log::Record<'_>) {
    if BUSY.swap(true, Ordering::Acquire) {
        LOST.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let line = Line::new(uptime(), level, record.target(), *record.args());
    critical_section::with(|cs| PENDING.borrow_ref_mut(cs).push(line));
    READY.signal(());
    BUSY.store(false, Ordering::Release);
}

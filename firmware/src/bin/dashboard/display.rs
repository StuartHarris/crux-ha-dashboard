//! The display task: it draws the newest view model on the panel, and it
//! reads the touch controller between refreshes.
//!
//! The panel and the touch controller share the I2C bus, so one task owns
//! both of them. A refresh takes up to about two seconds. The task does not
//! use the bus for touch during a refresh.
//!
//! If the touch controller does not start, or a read fails, the task tries
//! to start it again after a wait that doubles after each failure
//! (`logic::backoff`), and it writes one log line for each try.
//!
//! The BQ25896 charger and the BQ27220 fuel gauge are on the same bus. Each
//! [`POWER_POLL`], after a touch read or a render, the task reads the power
//! source and the charge, and
//! sends each change to the core (`logic::power::PowerMonitor`). The task
//! only reads these chips. It never writes to them.

use core::mem;

use dashboard_firmware::panel::{Area, Panel};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::primitives::Rectangle;
use logic::{
    backoff::Backoff,
    power::{
        CHARGER_ADDRESS, CHARGER_STATUS, GAUGE_ADDRESS, GAUGE_STATE_OF_CHARGE, PowerMonitor,
        level_from_bq27220, source_from_bq25896,
    },
    shell::Signal,
    touch::{Tapper, TouchMap},
};
use shared::{Event, PowerSource, ViewModel};
use ui::{Framebuffer, Refresh, Refresher};

use crate::{RENDER, SIGNALS};

/// The time between two reads of the touch controller.
const TOUCH_POLL: Duration = Duration::from_millis(30);
/// The time between two reads of the charger and the fuel gauge.
const POWER_POLL: Duration = Duration::from_secs(30);
/// The time to the next read of the charger when a new power source waits
/// for its second read.
const POWER_CONFIRM: Duration = Duration::from_secs(1);

/// The frames of the display task.
struct Frames {
    /// The frame that the panel shows.
    shown: Framebuffer,
    /// The frame to draw next.
    next: Framebuffer,
    refresher: Refresher,
}

/// `source` is the power source that the core got in `Start`.
#[embassy_executor::task]
pub async fn task(mut panel: Panel<'static>, source: PowerSource) {
    let mut touch = Touch::default();
    let mut power = Power::new(source);
    let mut frames = Frames {
        shown: Framebuffer::new(),
        next: Framebuffer::new(),
        refresher: Refresher::default(),
    };
    loop {
        match select(RENDER.wait(), Timer::after(TOUCH_POLL)).await {
            Either::First(view) => render(&mut panel, &mut frames, &view).await,
            Either::Second(()) => touch.poll(&mut panel).await,
        }
        // After each branch, so that many renders in a row do not stop the
        // power poll.
        power.poll(&mut panel);
    }
}

/// Reads the power source from the BQ25896 charger. Returns `None` if the
/// read fails. If `warn` is `true`, logs a failure.
pub fn read_source(panel: &mut Panel<'static>, warn: bool) -> Option<PowerSource> {
    let mut status = [0];
    match panel.read_registers(CHARGER_ADDRESS, CHARGER_STATUS, &mut status) {
        Ok(()) => Some(source_from_bq25896(status[0])),
        Err(error) => {
            if warn {
                log::warn!("power: cannot read the BQ25896 ({error:?})");
            }
            None
        }
    }
}

/// Reads the charge in percent from the BQ27220 fuel gauge. Returns `None`
/// if the read fails or the value is not valid. If `warn` is `true`, logs a
/// failure.
fn read_level(panel: &mut Panel<'static>, warn: bool) -> Option<u8> {
    let mut bytes = [0; 2];
    match panel.read_registers(GAUGE_ADDRESS, GAUGE_STATE_OF_CHARGE, &mut bytes) {
        Ok(()) => {
            let level = level_from_bq27220(bytes);
            if level.is_none() && warn {
                log::warn!("power: the BQ27220 gives a charge that is not valid ({bytes:?})");
            }
            level
        }
        Err(error) => {
            if warn {
                log::warn!("power: cannot read the BQ27220 ({error:?})");
            }
            None
        }
    }
}

/// The state of the power poll.
struct Power {
    monitor: PowerMonitor,
    /// The time of the next read. The first read is at once.
    next: Instant,
    /// The last read of the charger failed, so the task does not log the
    /// next failure.
    charger_failed: bool,
    /// The last read of the fuel gauge failed, so the task does not log the
    /// next failure.
    gauge_failed: bool,
}

impl Power {
    fn new(source: PowerSource) -> Self {
        Self {
            monitor: PowerMonitor::new(source),
            next: Instant::now(),
            charger_failed: false,
            gauge_failed: false,
        }
    }

    /// If the time of the next read came, reads the charger and the fuel
    /// gauge, and sends each change to the core.
    fn poll(&mut self, panel: &mut Panel<'static>) {
        let now = Instant::now();
        if now < self.next {
            return;
        }
        // The task logs only the first failure in a row of each chip.
        let source = read_source(panel, !self.charger_failed);
        let level = read_level(panel, !self.gauge_failed);
        self.charger_failed = source.is_none();
        self.gauge_failed = level.is_none();
        for event in self.monitor.update(source, level) {
            match &event {
                Event::PowerSourceChanged(source) => log::info!("power: source {source:?}"),
                Event::BatteryChanged(Some(level)) => log::info!("power: battery {level}%"),
                _ => {}
            }
            SIGNALS.push(Signal::Event(event));
        }
        let wait = if self.monitor.is_pending() {
            POWER_CONFIRM
        } else {
            POWER_POLL
        };
        self.next = now + wait;
    }
}

/// The state of the touch controller.
#[derive(Default)]
struct Touch {
    /// The map to the panel, or `None` until the controller starts.
    map: Option<TouchMap>,
    tapper: Tapper,
    backoff: Backoff,
}

impl Touch {
    /// Reads one sample, and sends a tap to the core. Starts the controller
    /// first, if it did not start and the wait after the last failure ended.
    async fn poll(&mut self, panel: &mut Panel<'static>) {
        let now = Instant::now().as_millis();
        let Some(map) = self.map else {
            if self.backoff.ready(now) {
                match panel.init_touch().await {
                    Ok(resolution) => {
                        log::info!("touch: ready, resolution {resolution:?}");
                        self.map = Some(TouchMap::new(resolution));
                        self.backoff.succeeded();
                    }
                    Err(error) => {
                        let wait = self.backoff.failed(now);
                        log::warn!(
                            "touch: the GT911 does not start ({error:?}). Next try in {wait} ms."
                        );
                    }
                }
            }
            return;
        };
        match panel.touch_sample() {
            Ok(sample) => {
                if let Some(tap) = self.tapper.sample(now, sample.map(|raw| map.to_panel(raw))) {
                    log::info!("touch: tap at ({}, {})", tap.x, tap.y);
                    SIGNALS.push(Signal::Tap(tap));
                }
            }
            Err(error) => {
                self.map = None;
                let wait = self.backoff.failed(now);
                log::warn!("touch: read failed ({error:?}). Next start in {wait} ms.");
            }
        }
    }
}

/// Draws `view` and refreshes the panel.
async fn render(panel: &mut Panel<'static>, frames: &mut Frames, view: &ViewModel) {
    let Ok(()) = ui::draw(view, &mut frames.next);
    let refresh = frames
        .refresher
        .decide(view.full_refresh_seq, &frames.shown, &frames.next);
    if refresh == Refresh::None {
        return;
    }
    let started = Instant::now();
    // A failed power-on turns the high voltages off again.
    if let Err(error) = panel.power_on() {
        log::error!("panel: power on failed: {error:?}");
        frames.refresher.reset();
        return;
    }
    let result = match refresh {
        Refresh::Full => panel.full(&frames.next).await,
        Refresh::Partial(area) => {
            panel
                .partial(&frames.shown, &frames.next, to_area(area))
                .await
        }
        Refresh::None => Ok(()),
    };
    if let Err(error) = panel.power_off() {
        log::error!("panel: power off failed: {error:?}");
    }
    match result {
        Ok(()) => {
            mem::swap(&mut frames.shown, &mut frames.next);
            log::info!(
                "panel: {refresh:?} in {} ms",
                Instant::now().duration_since(started).as_millis()
            );
        }
        Err(error) => {
            log::error!("panel: refresh failed: {error:?}");
            frames.refresher.reset();
        }
    }
}

/// Converts a rectangle on the panel to the area type of the driver.
fn to_area(rectangle: Rectangle) -> Area {
    let clamp = |value: i32| u16::try_from(value.max(0)).unwrap_or(u16::MAX);
    let size = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
    Area {
        x: clamp(rectangle.top_left.x),
        y: clamp(rectangle.top_left.y),
        width: size(rectangle.size.width),
        height: size(rectangle.size.height),
    }
}

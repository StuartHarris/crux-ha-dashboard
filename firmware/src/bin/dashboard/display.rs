//! The display task: it draws the newest view model on the panel, and it
//! reads the touch controller between refreshes. When the SNTP task sets
//! the clock, the display task also writes the RTC if its time is wrong.
//!
//! The panel, the touch controller and the RTC share the I2C bus, so one
//! task owns all of them. A refresh takes up to about two seconds. The task
//! does not use the bus for touch or the RTC during a refresh.
//!
//! If the touch controller does not start, or a read fails, the task tries
//! to start it again after a wait that doubles after each failure
//! (`logic::backoff`), and it writes one log line for each try.

use core::mem;

use dashboard_firmware::{
    panel::{Area, Panel},
    rtc,
};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::primitives::Rectangle;
use logic::{
    backoff::Backoff,
    clock::{RtcWrite, civil::DateTime, pcf8563},
    shell::Signal,
    touch::{Tapper, TouchMap},
};
use shared::ViewModel;
use ui::{Framebuffer, Refresh, Refresher};

use crate::{RENDER, RTC_WRITE, SIGNALS, clock};

/// The time between two reads of the touch controller.
const TOUCH_POLL: Duration = Duration::from_millis(30);

/// The frames of the display task.
struct Frames {
    /// The frame that the panel shows.
    shown: Framebuffer,
    /// The frame to draw next.
    next: Framebuffer,
    refresher: Refresher,
}

#[embassy_executor::task]
pub async fn task(mut panel: Panel<'static>) {
    let mut touch = Touch::default();
    let mut frames = Frames {
        shown: Framebuffer::new(),
        next: Framebuffer::new(),
        refresher: Refresher::default(),
    };
    let mut rtc_write = RtcWrite::default();
    loop {
        let now_ms = Instant::now().as_millis();
        if RTC_WRITE.try_take().is_some() {
            rtc_write.request(now_ms);
        }
        if rtc_write.due(now_ms) {
            match write_rtc(&mut panel).await {
                Ok(()) => rtc_write.succeeded(),
                Err(error) if rtc_write.failed(Instant::now().as_millis()) => {
                    log::warn!("rtc: {error:?}. No more tries until the next SNTP reply.");
                }
                Err(error) => log::warn!(
                    "rtc: {error:?}. Next try in {} ms.",
                    logic::clock::RTC_RETRY_MS
                ),
            }
        }
        match select(RENDER.wait(), Timer::after(TOUCH_POLL)).await {
            Either::First(view) => render(&mut panel, &mut frames, &view).await,
            Either::Second(()) => touch.poll(&mut panel).await,
        }
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

/// Writes the clock to the RTC, if the RTC has no valid time or a time that
/// is more than `logic::clock::RTC_TOLERANCE_SECONDS` from the clock.
///
/// The RTC starts to count a second when it is written, so the function
/// waits for the start of the next second of the clock (at most 1 s), and
/// then writes that second. During the wait, the task does not read the
/// touch controller.
///
/// Returns `Ok` if the RTC has the time now, or if the clock has no valid
/// time to write.
async fn write_rtc(panel: &mut Panel<'static>) -> Result<(), RtcError> {
    let block = rtc::read(panel).map_err(RtcError::Read)?;
    let now = clock::now();
    if !logic::clock::is_valid(now.seconds()) {
        return Ok(());
    }
    if !logic::clock::rtc_needs_write(pcf8563::decode_block(&block), now.seconds()) {
        log::debug!("rtc: the time is correct");
        return Ok(());
    }
    let (second, wait_micros) = logic::clock::next_second(now.seconds(), now.nanos());
    let Some(payload) = pcf8563::write_payload(second, block[1]) else {
        log::warn!("rtc: cannot keep {}", DateTime::from_unix(second));
        return Ok(());
    };
    Timer::after(Duration::from_micros(wait_micros)).await;
    rtc::write(panel, &payload).map_err(RtcError::Write)?;
    log::info!("rtc: set to {}", DateTime::from_unix(second));
    Ok(())
}

/// Why a write of the RTC failed.
#[derive(Debug)]
enum RtcError {
    /// The read before the write failed.
    #[allow(dead_code, reason = "the log shows the field")]
    Read(dashboard_firmware::panel::Error),
    /// The write failed.
    #[allow(dead_code, reason = "the log shows the field")]
    Write(dashboard_firmware::panel::Error),
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

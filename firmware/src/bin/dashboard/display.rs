//! The display task: it draws the newest view model on the panel, and it
//! reads the touch controller between refreshes.
//!
//! The panel and the touch controller share the I2C bus, so one task owns
//! both. A refresh takes up to about two seconds. The task does not read
//! the touch controller during a refresh.
//!
//! If the touch controller does not start, or a read fails, the task tries
//! to start it again after a wait that doubles after each failure
//! (`logic::backoff`), and it writes one log line for each try.

use core::mem;

use dashboard_firmware::panel::{Area, Panel};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::primitives::Rectangle;
use logic::{
    backoff::Backoff,
    shell::Signal,
    touch::{Tapper, TouchMap},
};
use shared::ViewModel;
use ui::{Framebuffer, Refresh, Refresher};

use crate::{RENDER, SIGNALS};

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
    loop {
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

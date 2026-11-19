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
//!
//! The task also sets the front light ([`Light`]). The core task writes each
//! `FrontLight` job in [`LIGHT`]. The task sets the level at the start of
//! the next pass of its loop, before a render, and answers with
//! `Signal::Light`.
//!
//! The task also runs the `Sleep` job, because it owns the INT pin of the
//! touch controller and the I2C bus. The core sends `Sleep` only after the
//! MQTT connection closed and the radio is off. The core task writes the job
//! in [`SLEEP`], and `None` when the shell answered the request. Refer to
//! [`Sleep`].

use core::mem;

use dashboard_firmware::{
    light::FrontLight,
    panel::{Area, Panel},
};
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::{prelude::Point, primitives::Rectangle};
use esp_hal::{
    peripherals::LPWR,
    rtc_cntl::{
        WakeupSource,
        sleep::{LowPower, RtcSleepConfig},
        wakeup_cause,
    },
};
use logic::{
    backoff::Backoff,
    power::{
        CHARGER_ADDRESS, CHARGER_STATUS, GAUGE_ADDRESS, GAUGE_AVERAGE_CURRENT,
        GAUGE_STATE_OF_CHARGE, PowerMonitor, current_from_bq27220, level_from_bq27220,
        source_from_bq25896,
    },
    shell::Signal,
    sleep::{After, Blocker, Cause, Plan, Readiness, Sleeper},
    touch::{Sample, Tapper, TouchMap},
};
use shared::{Event, PowerSource, ViewModel, capabilities::power::Wake};
use ui::{Framebuffer, Refresh, Refresher};

use crate::{LIGHT, RENDER, SIGNALS, SLEEP};

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

/// `source` is the power source that the core got in `Start`. If `light` is
/// `None`, the front light did not start. The task then answers each
/// `FrontLight` job, but it does not set a level.
#[embassy_executor::task]
pub async fn task(
    mut panel: Panel<'static>,
    source: PowerSource,
    mut sleep: Sleep,
    light: Option<FrontLight>,
) {
    let mut light = Light(light);
    let mut touch = Touch::default();
    let mut power = Power::new(source, sleep.fixed_source);
    let mut frames = Frames {
        shown: Framebuffer::new(),
        next: Framebuffer::new(),
        refresher: Refresher::default(),
    };
    loop {
        let next = select3(RENDER.wait(), SLEEP.wait(), Timer::after(TOUCH_POLL)).await;
        // A tap gives a `FrontLight` job and a render together. The light
        // goes on first, because a refresh takes up to about two seconds.
        light.poll();
        match next {
            Either3::First(view) => render(&mut panel, &mut frames, &view).await,
            Either3::Second(job) => sleep.set_job(job),
            Either3::Third(()) => {
                if touch.poll(&mut panel).await {
                    sleep.sleeper.tapped(now_ms());
                }
            }
        }
        // After each branch, so that many renders in a row do not stop the
        // power poll.
        power.poll(&mut panel);
        // A view model that waits blocks the sleep, so the last image
        // before `Sleep` is on the panel first.
        sleep.poll(&mut panel, &mut touch, &mut power, &mut light);
    }
}

/// The front light, or `None` if it did not start.
struct Light(Option<FrontLight>);

impl Light {
    /// Sets the level of the newest `FrontLight` job that waits, and answers
    /// the jobs that wait. The answer for the newest job is also for the
    /// older jobs.
    ///
    /// The task answers also if it cannot set the level, because the core
    /// waits for the answer before the steps of a sleep.
    fn poll(&mut self) {
        let Some(&(token, level)) = LIGHT.take_all().last() else {
            return;
        };
        match &mut self.0 {
            Some(light) => match light.set(level) {
                Ok(()) => log::info!("light: {level}%"),
                Err(error) => log::warn!("light: cannot set the level {level}% ({error:?})"),
            },
            None => log::warn!("light: the light did not start, so the level {level}% is not set"),
        }
        SIGNALS.push(Signal::Light { token });
    }

    /// Turns the light off before a light sleep. Refer to
    /// [`FrontLight::off_for_sleep`].
    fn off_for_sleep(&mut self) {
        if let Some(light) = &mut self.0 {
            light.off_for_sleep();
        }
    }
}

/// The light sleep of the chip.
///
/// Light sleep keeps the RAM and the PSRAM, so the program continues after
/// the wake, with no full refresh. The wake sources are the GT911 INT pin
/// (GPIO3, a low level) and the RTC timer. `esp-hal` moves the system timer
/// forward by the time asleep, so `Instant` and the running timers stay
/// correct. The alarm of the scheduler (`TIMG0`) stops in the sleep, so the
/// task arms it again after the wake.
///
/// The LEDC of the front light also stops in the sleep, and the pin keeps
/// its level. Thus, before each sleep, the task sets the newest
/// `FrontLight` job that waits, then sets the duty to 0. The core turns the
/// light off before `Sleep`, so this is a guard.
///
/// A wake by the INT pin, or by the RTC timer at the deadline of the core, is
/// a real wake. After a touch wake, the task reads the point and sends it as
/// a tap, which answers the job and acts on its control. Else it answers
/// the job with `Signal::Woke`. The RTC timer also wakes the chip each
/// `logic::sleep::CHECK_SECONDS` for a read of the charger. If the source
/// changes, the shell answers the job with `Wake::PowerSource`. Otherwise
/// the chip sleeps again at once. After another wake, the chip sleeps again
/// after `logic::sleep::WAKE_HOLD_MS`. In that time, a tap or a change of
/// the power source can also end the `Sleep` request (`logic::shell`).
pub struct Sleep {
    low_power: LowPower<'static>,
    /// The `Sleep` job that waits for a real wake.
    job: Option<Plan>,
    sleeper: Sleeper,
    /// The power source does not follow the charger (`force_battery`).
    fixed_source: bool,
}

impl Sleep {
    /// Makes the sleep. If `force_battery` is `true`, the power poll keeps
    /// the source `Battery`.
    pub fn new(lpwr: LPWR<'static>, force_battery: bool) -> Self {
        Self {
            low_power: LowPower::new(lpwr),
            job: None,
            sleeper: Sleeper::default(),
            fixed_source: force_battery,
        }
    }

    /// Takes a new `Sleep` job, or `None` when the shell answered the
    /// request.
    fn set_job(&mut self, job: Option<Plan>) {
        if let Some(plan) = job {
            self.sleeper.start(now_ms(), plan);
        }
        self.job = job;
    }

    /// Sleeps if a `Sleep` job waits, the hold after the last wake or tap
    /// ended, and nothing blocks the sleep (`logic::sleep::Readiness`). It
    /// does not wait for a blocker, so the touch poll and the renders
    /// continue, and the task tries again on its next pass.
    fn poll(
        &mut self,
        panel: &mut Panel<'static>,
        touch: &mut Touch,
        power: &mut Power,
        light: &mut Light,
    ) {
        let now = now_ms();
        let Some(requested) = self.job.filter(|_| self.sleeper.can_sleep(now)) else {
            self.sleeper.unblocked();
            return;
        };
        let Some(plan) = requested.armed(touch.is_ready()) else {
            if self.sleeper.not_ready(now) {
                log::warn!("power: the touch controller is not ready, so the device stays awake");
            }
            return;
        };
        let readiness = Readiness {
            render_pending: RENDER.signaled(),
            touch_pending: panel.touch_pending(),
        };
        if let Some(blocker) = readiness.blocker(plan) {
            if self.sleeper.blocked(now, blocker) {
                warn_blocked(panel, touch, blocker);
            }
            return;
        }
        self.sleeper.unblocked();
        let seconds = self.sleeper.timer_seconds(now);
        log::debug!("power: light sleep, wake on {plan:?}, timer {seconds} s");
        // The core turned the light off before `Sleep`. This is a guard. A
        // `FrontLight` job can arrive after the poll at the start of this
        // pass, so the task sets that level first.
        light.poll();
        light.off_for_sleep();
        let started = Instant::now();
        let cause = self.sleep(panel, plan, seconds);
        let slept = Instant::now().duration_since(started).as_millis();
        // First, while the device draws little current.
        let current = read_average_current(panel);
        let after = self.sleeper.after(now_ms(), cause);
        if after == After::CheckPower {
            log::debug!("power: check of the power source after {slept} ms");
            if power.check(panel) {
                // The new source needs its second read. The power poll does
                // it, and the shell then answers the job.
                self.sleeper.woke(now_ms());
            }
            return;
        }
        log::info!(
            "power: woke by {cause:?} after {slept} ms, average current {} (not verified)",
            Milliamps(current)
        );
        match after {
            After::Touch => {
                self.job = None;
                // The GT911 holds the point while INT is low. The tap at the
                // point answers the job (`logic::shell`), and it acts on its
                // control.
                if let Some(point) = touch.wake_point(panel) {
                    log::info!("touch: wake tap at ({}, {})", point.x, point.y);
                    SIGNALS.push(Signal::Tap(point));
                } else {
                    log::info!("touch: no point after the wake, so the touch acts on no control");
                    SIGNALS.push(Signal::Woke(Wake::Touch(None)));
                }
            }
            After::Answer(wake) => {
                self.job = None;
                SIGNALS.push(Signal::Woke(wake));
            }
            After::SleepAgain | After::CheckPower => self.sleeper.woke(now_ms()),
        }
    }

    /// Enters light sleep with the touch wake of `plan` and a timer wake
    /// after `seconds`, and returns the cause of the wake.
    fn sleep(&mut self, panel: &mut Panel<'static>, plan: Plan, seconds: u32) -> Cause {
        // The sleep and the reads of the wake cause are in one critical
        // section, as in the automatic light sleep of `esp-rtos`. Thus no
        // interrupt handler clears the GPIO interrupt before the read.
        let wake = critical_section::with(|_| {
            if plan.touch {
                panel.touch_wake_arm();
            }
            self.low_power.set_wakeup_deadline(
                esp_hal::time::Instant::now()
                    + esp_hal::time::Duration::from_secs(u64::from(seconds)),
            );
            self.low_power.sleep_light(RtcSleepConfig::default());
            let touch = plan.touch && panel.touch_wake_disarm();
            if touch {
                Cause::Touch
            } else if wakeup_cause().contains(WakeupSource::Timer) {
                Cause::Timer
            } else {
                Cause::Other
            }
        });
        // The deadline is a standing request, so remove it.
        self.low_power.clear_wakeup_deadline();
        // The alarm of the scheduler did not run in the sleep.
        esp_rtos::rearm_alarm();
        wake
    }
}

/// Writes a warning about a blocker that stopped the sleeps for
/// `logic::sleep::BLOCK_WARNING_MS`. If the GT911 holds INT low, clears its
/// status register. If INT is still low, starts the touch controller again.
fn warn_blocked(panel: &mut Panel<'static>, touch: &mut Touch, blocker: Blocker) {
    log::warn!("power: {blocker:?} stops the sleep");
    if blocker != Blocker::Touch {
        return;
    }
    if let Err(error) = panel.touch_clear_status() {
        log::warn!("touch: cannot clear the status ({error:?})");
    }
    if panel.touch_pending() {
        log::warn!("touch: INT is still low, so the touch controller starts again");
        touch.restart();
    }
}

/// Returns the time since the start, in milliseconds.
fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// A current in mA for the log, or "not known".
struct Milliamps(Option<i16>);

impl core::fmt::Display for Milliamps {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(current) => write!(f, "{current} mA"),
            None => write!(f, "not known"),
        }
    }
}

/// Reads the average current in mA from the BQ27220 fuel gauge. A negative
/// value is a discharge. Returns `None` if the read fails.
fn read_average_current(panel: &mut Panel<'static>) -> Option<i16> {
    let mut bytes = [0; 2];
    panel
        .read_registers(GAUGE_ADDRESS, GAUGE_AVERAGE_CURRENT, &mut bytes)
        .ok()
        .map(|()| current_from_bq27220(bytes))
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
    /// If `fixed` is `true`, the monitor keeps `source` and sends no change
    /// of the source.
    fn new(source: PowerSource, fixed: bool) -> Self {
        Self {
            monitor: if fixed {
                PowerMonitor::fixed(source)
            } else {
                PowerMonitor::new(source)
            },
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
        self.send(source, level, now);
    }

    /// Reads only the charger now, as the check of the power source during
    /// a sleep does. Sends a change to the core. Returns `true` if a new
    /// source waits for its second read.
    fn check(&mut self, panel: &mut Panel<'static>) -> bool {
        let source = read_source(panel, !self.charger_failed);
        self.charger_failed = source.is_none();
        self.send(source, None, Instant::now());
        self.monitor.is_pending()
    }

    /// Gives the reads to the monitor, sends each change to the core, and
    /// sets the time of the next read.
    fn send(&mut self, source: Option<PowerSource>, level: Option<u8>, now: Instant) {
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
    /// Returns `true` if the touch controller started.
    const fn is_ready(&self) -> bool {
        self.map.is_some()
    }

    /// Reads the point of the touch that woke the chip, or `None` if the
    /// read fails or the sample has no point. Marks the touch as used, so
    /// that a later sample of the same touch is not a second tap.
    fn wake_point(&mut self, panel: &mut Panel<'static>) -> Option<Point> {
        let map = self.map?;
        let point = match panel.touch_sample() {
            Ok(Sample::Pressed(raw)) => Some(map.to_panel(raw)),
            Ok(_) => None,
            Err(error) => {
                log::warn!("touch: read failed after the wake ({error:?})");
                None
            }
        };
        self.tapper.consume(Instant::now().as_millis());
        point
    }

    /// Makes the next poll start the touch controller again.
    const fn restart(&mut self) {
        self.map = None;
    }

    /// Reads one sample, and sends a tap to the core. Starts the controller
    /// first, if it did not start and the wait after the last failure ended.
    /// Returns `true` if the task sent a tap.
    async fn poll(&mut self, panel: &mut Panel<'static>) -> bool {
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
            return false;
        };
        match panel.touch_sample() {
            Ok(sample) => {
                if let Some(tap) = self.tapper.sample(now, sample.map(|raw| map.to_panel(raw))) {
                    log::info!("touch: tap at ({}, {})", tap.x, tap.y);
                    SIGNALS.push(Signal::Tap(tap));
                    return true;
                }
                false
            }
            Err(error) => {
                self.map = None;
                let wait = self.backoff.failed(now);
                log::warn!("touch: read failed ({error:?}). Next start in {wait} ms.");
                false
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
        Refresh::Clean { area, dots } => {
            panel
                .clean(&frames.shown, &frames.next, to_area(area), to_area(dots))
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

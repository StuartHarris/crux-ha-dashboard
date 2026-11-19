//! The front light: a PWM signal on GPIO11 (`PIN_BL_EN` in the vendor pin
//! map).
//!
//! GPIO11 drives the EN pin of the PT4103 LED boost driver. The duty of the
//! PWM signal sets the brightness: 0% is off and 100% is full current. The
//! PT4103 needs a PWM frequency of 1 kHz or less, so the LEDC low-speed timer
//! runs at [`FREQUENCY_HZ`].
//!
//! The core decides the level and when the light goes on and off. This
//! module only sets the duty.

use esp_hal::{
    delay::Delay,
    gpio::DriveMode,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, Channel, ChannelIFace},
        timer::{self, Timer, TimerIFace},
    },
    peripherals::{GPIO11, LEDC},
    time::Rate,
};
use static_cell::StaticCell;

/// The frequency of the PWM signal. The PT4103 needs 1 kHz or less.
pub const FREQUENCY_HZ: u32 = 1_000;

/// The time to wait after the duty changes to 0 before the chip sleeps, in
/// milliseconds. The LEDC uses a new duty at the start of the next PWM
/// period (1 ms), so the wait is two periods.
const LATCH_MS: u32 = 2;

/// The LEDC timer. The channel keeps a reference to it, so it is static.
static TIMER: StaticCell<Timer<'static, LowSpeed>> = StaticCell::new();

/// An error from the LEDC driver when the light starts.
#[derive(Debug)]
pub enum Error {
    /// The timer did not accept its configuration.
    Timer(timer::Error),
    /// The channel did not accept its configuration.
    Channel(channel::Error),
}

/// The front light.
pub struct FrontLight {
    channel: Channel<'static, LowSpeed>,
    /// The level now, in percent.
    level: u8,
}

impl FrontLight {
    /// Starts the LEDC on GPIO11 with the light off.
    ///
    /// Call this function only one time, because the timer is static.
    ///
    /// # Errors
    ///
    /// Returns an error if the LEDC does not accept the configuration.
    ///
    /// # Panics
    ///
    /// Panics if you call it a second time.
    pub fn new(ledc: LEDC<'static>, pin: GPIO11<'static>) -> Result<Self, Error> {
        let mut ledc = Ledc::new(ledc);
        ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
        let timer = TIMER.init(ledc.timer::<LowSpeed>(timer::Number::Timer0));
        timer
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty10Bit,
                clock_source: timer::LSClockSource::APBClk,
                frequency: Rate::from_hz(FREQUENCY_HZ),
            })
            .map_err(Error::Timer)?;
        let mut channel = ledc.channel(channel::Number::Channel0, pin);
        channel
            .configure(channel::config::Config {
                timer,
                duty_pct: 0,
                drive_mode: DriveMode::PushPull,
            })
            .map_err(Error::Channel)?;
        Ok(Self { channel, level: 0 })
    }

    /// Sets the light to `level` percent. A level above 100 is 100.
    ///
    /// # Errors
    ///
    /// Returns an error if the LEDC does not accept the duty. The light then
    /// keeps its last level.
    pub fn set(&mut self, level: u8) -> Result<(), channel::Error> {
        let level = level.min(100);
        self.channel.set_duty(level)?;
        self.level = level;
        Ok(())
    }

    /// Turns the light off before the chip goes into light sleep.
    ///
    /// The LEDC stops in light sleep, so the pin keeps its level at that
    /// time. This function always sets the duty to 0, also if the last
    /// known level is 0, because a failed `set` can leave the light on. Then
    /// it waits until the LEDC uses the new duty. The core turns the light
    /// off before it sends `Sleep`, so this function is only a guard.
    pub fn off_for_sleep(&mut self) {
        if self.level > 0 {
            log::warn!("light: on at the sleep, so the light goes off");
        }
        if let Err(error) = self.set(0) {
            log::warn!("light: cannot turn the light off before the sleep ({error:?})");
        }
        Delay::new().delay_millis(LATCH_MS);
    }
}

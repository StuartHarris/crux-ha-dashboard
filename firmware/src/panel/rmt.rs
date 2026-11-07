//! The RMT channel that drives CKV, the vertical clock of the panel.
//!
//! Adapted from `src/rmt.rs` of `lilygo-t5s3paperpro` for the RMT API of
//! esp-hal 1.2. The upstream driver stole the RMT peripheral when it first
//! sent a pulse. This version configures the channel once, in `new`.

use esp_hal::{
    Blocking,
    gpio::Level,
    peripherals,
    rmt::{self, Channel, PulseCode, Tx, TxChannelConfig, TxChannelCreator, TxTransaction},
    time::Rate,
};

use super::{Error, Result};

pub struct Rmt<'a> {
    channel: Option<Channel<'a, Blocking, Tx>>,
}

/// A pulse that the channel sends now. Give it to [`Rmt::finish`].
pub type Transfer<'a, 'b> = TxTransaction<'a, 'b>;

impl<'a> Rmt<'a> {
    pub fn new(rmt: peripherals::RMT<'a>, pin: peripherals::GPIO48<'a>) -> Result<Self> {
        let rmt = rmt::Rmt::new(rmt, Rate::from_mhz(80)).map_err(Error::RmtConfig)?;
        let config = TxChannelConfig::default()
            .with_clk_divider(8)
            .with_idle_output_level(Level::Low)
            .with_idle_output(true)
            .with_carrier_modulation(false)
            .with_carrier_level(Level::Low);
        let channel = rmt
            .channel1
            .configure_tx(&config)
            .map_err(Error::RmtConfig)?
            .with_pin(pin);
        Ok(Self {
            channel: Some(channel),
        })
    }

    /// Sends a pulse and waits until it ends.
    pub fn pulse(&mut self, data: &[PulseCode]) -> Result<()> {
        let transfer = self.start(data)?;
        self.finish(transfer)
    }

    /// Starts to send a pulse.
    pub fn start<'b>(&mut self, data: &'b [PulseCode]) -> Result<Transfer<'a, 'b>> {
        let channel = self.channel.take().ok_or(Error::Missing)?;
        channel.transmit(data).map_err(|(error, channel)| {
            self.channel = Some(channel);
            Error::Rmt(error)
        })
    }

    /// Waits until a pulse ends, and takes the channel back.
    pub fn finish(&mut self, transfer: Transfer<'a, '_>) -> Result<()> {
        match transfer.wait() {
            Ok(channel) => {
                self.channel = Some(channel);
                Ok(())
            }
            Err((error, channel)) => {
                self.channel = Some(channel);
                Err(Error::Rmt(error))
            }
        }
    }
}

//! The ED047TC1 bus, the PCA9535 IO expander, the TPS65185 PMIC and the
//! GT911 touch controller.
//!
//! Adapted from `src/ed047tc1.rs` of `lilygo-t5s3paperpro` (refer to
//! `mod.rs` for the source and the licence). The changes are for esp-hal 1.2:
//! the RMT and DMA calls, and `Delay` in place of the cycle counter. The touch
//! read gives the raw controller point, and `logic::touch` maps it to the
//! panel. The battery, charger and button reads are removed.

use embassy_time::Timer;
use esp_hal::{
    Blocking,
    delay::Delay,
    dma::DmaTxBuf,
    dma_tx_buffer,
    gpio::{Flex, InputConfig, Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    lcd_cam::{
        LcdCam,
        lcd::{i8080, i8080::Command},
    },
    peripherals,
    rmt::PulseCode,
    time::Rate,
};
use log::debug;
use logic::touch::Sample;

use super::{Error, Result, rmt};

macro_rules! pulse {
    ($high:expr, $low:expr) => {
        if $high > 0 {
            [
                PulseCode::new(Level::High, $high, Level::Low, $low),
                PulseCode::end_marker(),
            ]
        } else {
            [
                PulseCode::new(Level::High, $low, Level::Low, 0),
                PulseCode::end_marker(),
            ]
        }
    };
}

const DMA_BUFFER_SIZE: usize = 248;
const I2C_FREQUENCY_KHZ: u32 = 100;
pub const PCA9535_ADDR: u8 = 0x20;
pub const TPS65185_ADDR: u8 = 0x68;
const PCA9535_REG_INPUT_PORT1: u8 = 1;
const PCA9535_REG_OUTPUT_PORT0: u8 = 2;
const PCA9535_REG_OUTPUT_PORT1: u8 = 3;
const PCA9535_REG_INVERT_PORT0: u8 = 4;
const PCA9535_REG_INVERT_PORT1: u8 = 5;
const PCA9535_REG_CONFIG_PORT0: u8 = 6;
const PCA9535_REG_CONFIG_PORT1: u8 = 7;
const TPS_REG_ENABLE: u8 = 0x01;
const TPS_REG_VCOM1: u8 = 0x03;
const TPS_REG_VCOM2: u8 = 0x04;
const TPS_REG_PG: u8 = 0x0F;
pub const GT911_ADDR_LOW: u8 = 0x5D;
const GT911_ADDR_HIGH: u8 = 0x14;
const GT911_PRODUCT_ID: u16 = 0x8140;
const GT911_CONFIG_VERSION: u16 = 0x8047;
const GT911_MODULE_SWITCH_1: u16 = 0x804D;
const GT911_CONFIG_CHKSUM: u16 = 0x80FF;
const GT911_CONFIG_FRESH: u16 = 0x8100;
const GT911_CONFIG_LENGTH: usize = 186;
const GT911_POINT_INFO: u16 = 0x814E;
const GT911_POINT_1: u16 = 0x814F;
const GT911_X_RESOLUTION: u16 = 0x8146;
const GT911_Y_RESOLUTION: u16 = 0x8148;
const GT911_DEV_ID: u32 = 911;
// TODO(5b): the vendor firmware uses 1600 mV. The panel label can give
// another VCOM value.
const VCOM_MV: u16 = 1600;
const PCA_BIT_OE: u8 = 1 << 0;
const PCA_BIT_MODE: u8 = 1 << 1;
const PCA_BIT_BUTTON: u8 = 1 << 2;
const PCA_BIT_PWRUP: u8 = 1 << 3;
const PCA_BIT_VCOM_CTRL: u8 = 1 << 4;
const PCA_BIT_WAKEUP: u8 = 1 << 5;
const PCA_BIT_PWRGOOD: u8 = 1 << 6;
const PCA_BIT_INT: u8 = 1 << 7;
/// The number of times that power-on reads the power-good bits.
const POWER_GOOD_TRIES: u32 = 500;

#[derive(Default)]
struct ConfigRegister {
    mode: bool,
    output_enable: bool,
    pwrup: bool,
    vcom_ctrl: bool,
    wakeup: bool,
}

/// The pins of the panel and of the touch controller. The names are the nets
/// of the H752-01 schematic.
pub struct PinConfig<'a> {
    pub d0: peripherals::GPIO5<'a>,
    pub d1: peripherals::GPIO6<'a>,
    pub d2: peripherals::GPIO7<'a>,
    pub d3: peripherals::GPIO15<'a>,
    pub d4: peripherals::GPIO16<'a>,
    pub d5: peripherals::GPIO17<'a>,
    pub d6: peripherals::GPIO18<'a>,
    pub d7: peripherals::GPIO8<'a>,
    pub i2c_sda: peripherals::GPIO39<'a>,
    pub i2c_scl: peripherals::GPIO40<'a>,
    pub le: peripherals::GPIO42<'a>,
    pub sth: peripherals::GPIO41<'a>,
    pub ckh: peripherals::GPIO4<'a>,
    pub ckv: peripherals::GPIO48<'a>,
    pub stv: peripherals::GPIO45<'a>,
    pub touch_int: peripherals::GPIO3<'a>,
    pub touch_rst: peripherals::GPIO9<'a>,
}

/// The I2C devices of the panel: the IO expander, the PMIC and the touch
/// controller.
struct ConfigWriter<'a> {
    i2c: I2c<'a, Blocking>,
    le: Output<'a>,
    stv: Output<'a>,
    touch_rst: Output<'a>,
    touch_int: Flex<'a>,
    touch_initialized: bool,
    touch_addr: u8,
    touch_resolution: (u16, u16),
    config: ConfigRegister,
    delay: Delay,
}

impl<'a> ConfigWriter<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        i2c: peripherals::I2C0<'a>,
        sda: peripherals::GPIO39<'a>,
        scl: peripherals::GPIO40<'a>,
        le: peripherals::GPIO42<'a>,
        stv: peripherals::GPIO45<'a>,
        touch_rst: peripherals::GPIO9<'a>,
        touch_int: peripherals::GPIO3<'a>,
    ) -> Result<Self> {
        let i2c = I2c::new(
            i2c,
            I2cConfig::default().with_frequency(Rate::from_khz(I2C_FREQUENCY_KHZ)),
        )
        .map_err(Error::I2cConfig)?
        .with_sda(sda)
        .with_scl(scl);

        let mut writer = ConfigWriter {
            i2c,
            le: Output::new(le, Level::Low, OutputConfig::default()),
            stv: Output::new(stv, Level::High, OutputConfig::default()),
            touch_rst: {
                // A deep sleep can leave the pad held.
                let mut pin = Output::new(touch_rst, Level::High, OutputConfig::default());
                pin.set_pad_hold(false);
                pin
            },
            touch_int: {
                let mut pin = Flex::new(touch_int);
                pin.set_pad_hold(false);
                pin.set_output_enable(false);
                pin.set_input_enable(true);
                pin.apply_input_config(&InputConfig::default());
                pin
            },
            touch_initialized: false,
            touch_addr: GT911_ADDR_LOW,
            touch_resolution: (0, 0),
            config: ConfigRegister::default(),
            delay: Delay::new(),
        };

        writer.write_register(
            PCA9535_ADDR,
            &[
                PCA9535_REG_CONFIG_PORT1,
                PCA_BIT_BUTTON | PCA_BIT_PWRGOOD | PCA_BIT_INT,
            ],
        )?;
        writer.write_register(PCA9535_ADDR, &[PCA9535_REG_INVERT_PORT0, 0x00])?;
        writer.write_register(PCA9535_ADDR, &[PCA9535_REG_INVERT_PORT1, 0x00])?;
        writer.write_register(PCA9535_ADDR, &[PCA9535_REG_CONFIG_PORT0, 0x00])?;
        writer.write_register(PCA9535_ADDR, &[PCA9535_REG_OUTPUT_PORT0, 0xFF])?;
        writer.write()?;

        Ok(writer)
    }

    fn write(&mut self) -> Result<()> {
        let mut value = 0;
        if self.config.output_enable {
            value |= PCA_BIT_OE;
        }
        if self.config.mode {
            value |= PCA_BIT_MODE;
        }
        if self.config.pwrup {
            value |= PCA_BIT_PWRUP;
        }
        if self.config.vcom_ctrl {
            value |= PCA_BIT_VCOM_CTRL;
        }
        if self.config.wakeup {
            value |= PCA_BIT_WAKEUP;
        }
        self.write_register(PCA9535_ADDR, &[PCA9535_REG_OUTPUT_PORT1, value])
    }

    fn set_stv(&mut self, level: bool) {
        self.stv
            .set_level(if level { Level::High } else { Level::Low });
    }

    fn pulse_le(&mut self) {
        // About 64 CPU cycles at 240 MHz.
        self.le.set_high();
        self.delay.delay_nanos(270);
        self.le.set_low();
        self.delay.delay_nanos(270);
    }

    fn pwrgood(&mut self) -> Result<bool> {
        Ok(self.read_register(PCA9535_ADDR, PCA9535_REG_INPUT_PORT1)? & PCA_BIT_PWRGOOD != 0)
    }

    fn enable_tps(&mut self) -> Result<()> {
        self.write_register(TPS65185_ADDR, &[TPS_REG_ENABLE, 0x3F])?;
        self.set_vcom(VCOM_MV)
    }

    fn set_vcom(&mut self, mv: u16) -> Result<()> {
        let value = mv / 10;
        self.write_register(
            TPS65185_ADDR,
            &[TPS_REG_VCOM2, ((value & 0x100) >> 8) as u8],
        )?;
        self.write_register(TPS65185_ADDR, &[TPS_REG_VCOM1, (value & 0xFF) as u8])
    }

    fn tps_power_good(&mut self) -> Result<bool> {
        Ok(self.read_register(TPS65185_ADDR, TPS_REG_PG)? & 0xFA == 0xFA)
    }

    fn read_register(&mut self, device: u8, reg: u8) -> Result<u8> {
        let mut value = [0u8; 1];
        self.i2c
            .write_read(device, &[reg], &mut value)
            .map_err(Error::I2c)?;
        Ok(value[0])
    }

    fn write_register(&mut self, device: u8, payload: &[u8]) -> Result<()> {
        self.i2c.write(device, payload).map_err(Error::I2c)
    }

    fn write_register16(&mut self, device: u8, reg: u16, payload: &[u8]) -> Result<()> {
        let mut buffer = [0u8; 41];
        let len = payload.len() + 2;
        buffer[0] = (reg >> 8) as u8;
        buffer[1] = reg as u8;
        buffer[2..len].copy_from_slice(payload);
        self.i2c.write(device, &buffer[..len]).map_err(Error::I2c)
    }

    fn read_register16(&mut self, device: u8, reg: u16, payload: &mut [u8]) -> Result<()> {
        let reg = [(reg >> 8) as u8, reg as u8];
        self.i2c
            .write_read(device, &reg, payload)
            .map_err(Error::I2c)
    }

    /// Resets the GT911, finds its address, and reads its resolution. The
    /// waits of the reset are `async`, so other tasks run during them.
    async fn init_touch(&mut self) -> Result<()> {
        debug!("touch init: probing GT911");
        self.touch_initialized = false;
        self.touch_reset_for_address(GT911_ADDR_LOW).await;

        let mut product_id = [0u8; 4];
        if self
            .read_register16(GT911_ADDR_LOW, GT911_PRODUCT_ID, &mut product_id)
            .is_ok()
            && parse_gt911_chip_id(product_id) == GT911_DEV_ID
        {
            self.touch_addr = GT911_ADDR_LOW;
        } else {
            self.touch_reset_for_address(GT911_ADDR_HIGH).await;
            self.read_register16(GT911_ADDR_HIGH, GT911_PRODUCT_ID, &mut product_id)?;
            self.touch_addr = GT911_ADDR_HIGH;
        }

        let chip_id = parse_gt911_chip_id(product_id);
        if chip_id != GT911_DEV_ID {
            debug!("touch init: unexpected chip id {chip_id}");
            return Err(Error::TouchInitFailed);
        }

        self.touch_resolution = (
            self.touch_read_u16(GT911_X_RESOLUTION)?,
            self.touch_read_u16(GT911_Y_RESOLUTION)?,
        );
        self.touch_set_interrupt_mode_low_level_query()?;
        debug!(
            "touch init: address 0x{:02X}, resolution {}x{}",
            self.touch_addr, self.touch_resolution.0, self.touch_resolution.1
        );
        self.touch_initialized = true;
        Ok(())
    }

    /// Resets the GT911. The level of INT during the reset selects its I2C
    /// address.
    async fn touch_reset_for_address(&mut self, address: u8) {
        self.touch_rst.set_low();
        self.delay.delay_micros(125);
        if address == GT911_ADDR_HIGH {
            self.touch_int.set_high();
        } else {
            self.touch_int.set_low();
        }
        self.touch_int.set_output_enable(true);
        self.touch_int.set_input_enable(false);
        self.delay.delay_micros(125);
        self.touch_rst.set_high();
        Timer::after_millis(19).await;
        self.touch_int.set_output_enable(false);
        self.touch_int.set_input_enable(true);
        self.touch_int.apply_input_config(&InputConfig::default());
        Timer::after_millis(21).await;
    }

    fn touch_pressed(&self) -> bool {
        self.touch_int.is_low()
    }

    fn touch_read_u16(&mut self, reg: u16) -> Result<u16> {
        let mut value = [0u8; 2];
        self.read_register16(self.touch_addr, reg, &mut value)?;
        Ok(u16::from_le_bytes(value))
    }

    fn touch_set_interrupt_mode_low_level_query(&mut self) -> Result<()> {
        let mut value = [0u8; 1];
        self.read_register16(self.touch_addr, GT911_MODULE_SWITCH_1, &mut value)?;
        value[0] = (value[0] & 0xFC) | 0x02;
        self.write_register16(self.touch_addr, GT911_MODULE_SWITCH_1, &value)?;
        self.touch_reload_config()
    }

    fn touch_reload_config(&mut self) -> Result<()> {
        let mut config = [0u8; GT911_CONFIG_LENGTH - 2];
        self.read_register16(self.touch_addr, GT911_CONFIG_VERSION, &mut config)?;
        let checksum = (!config
            .iter()
            .fold(0u8, |sum, value| sum.wrapping_add(*value)))
        .wrapping_add(1);
        self.write_register16(self.touch_addr, GT911_CONFIG_CHKSUM, &[checksum])?;
        self.write_register16(self.touch_addr, GT911_CONFIG_FRESH, &[0x01])?;
        Ok(())
    }

    /// Reads one sample. INT is high, or the buffer flag is clear, when the
    /// GT911 has no new sample. A new sample with no points means that the
    /// finger lifted.
    // TODO(5b): check on the device that a lift gives a sample with no points.
    fn touch_sample(&mut self) -> Result<Sample<(u16, u16)>> {
        if !self.touch_initialized {
            return Err(Error::TouchNotReady);
        }
        if !self.touch_pressed() {
            return Ok(Sample::Stale);
        }
        let mut point_info = [0u8; 1];
        self.read_register16(self.touch_addr, GT911_POINT_INFO, &mut point_info)?;
        let status = point_info[0];
        let count = status & 0x0F;
        let buffer_ready = status & 0x80 != 0;
        if !buffer_ready {
            return Ok(Sample::Stale);
        }
        self.write_register16(self.touch_addr, GT911_POINT_INFO, &[0x00])?;
        if count == 0 {
            return Ok(Sample::Released);
        }
        let mut buffer = [0u8; 8];
        self.read_register16(self.touch_addr, GT911_POINT_1, &mut buffer)?;
        let x = u16::from_le_bytes([buffer[1], buffer[2]]);
        let y = u16::from_le_bytes([buffer[3], buffer[4]]);
        Ok(Sample::Pressed((x, y)))
    }

    /// Returns the addresses on the bus that answer. The probe is a read of
    /// one byte, because esp-hal does not send a write of zero bytes.
    fn scan(&mut self, found: &mut [u8; 16]) -> usize {
        let mut count = 0;
        let mut byte = [0u8; 1];
        for address in 0x08..0x78u8 {
            if count < found.len() && self.i2c.read(address, &mut byte).is_ok() {
                found[count] = address;
                count += 1;
            }
        }
        count
    }
}

/// The bus of the panel.
pub struct Ed047tc1<'a> {
    i8080: Option<i8080::I8080<'a, Blocking>>,
    cfg_writer: ConfigWriter<'a>,
    rmt: rmt::Rmt<'a>,
    dma_buf: Option<DmaTxBuf>,
    delay: Delay,
}

impl<'a> Ed047tc1<'a> {
    pub fn new(
        pins: PinConfig<'a>,
        i2c: peripherals::I2C0<'a>,
        dma: peripherals::DMA_CH0<'a>,
        lcd_cam: peripherals::LCD_CAM<'a>,
        rmt: peripherals::RMT<'a>,
    ) -> Result<Self> {
        let lcd_cam = LcdCam::new(lcd_cam);

        let mut cfg_writer = ConfigWriter::new(
            i2c,
            pins.i2c_sda,
            pins.i2c_scl,
            pins.le,
            pins.stv,
            pins.touch_rst,
            pins.touch_int,
        )?;
        cfg_writer.write()?;

        let dma_buf: DmaTxBuf = dma_tx_buffer!(DMA_BUFFER_SIZE).map_err(Error::DmaBuffer)?;
        let dma_buf = Some(dma_buf);

        let config = i8080::Config::default()
            .with_frequency(Rate::from_mhz(20))
            .with_cd_idle_edge(false)
            .with_cd_cmd_edge(true)
            .with_cd_dummy_edge(false)
            .with_cd_data_edge(false);
        // The order of the data pins is the order of the upstream driver.
        // The line buffers depend on it.
        let i8080 = i8080::I8080::new(lcd_cam.lcd, dma, config)
            .map_err(Error::I8080Config)?
            .with_dc(pins.sth)
            .with_wrx(pins.ckh)
            .with_data0(pins.d6)
            .with_data1(pins.d7)
            .with_data2(pins.d4)
            .with_data3(pins.d5)
            .with_data4(pins.d2)
            .with_data5(pins.d3)
            .with_data6(pins.d0)
            .with_data7(pins.d1);
        Ok(Self {
            i8080: Some(i8080),
            cfg_writer,
            rmt: rmt::Rmt::new(rmt, pins.ckv)?,
            dma_buf,
            delay: Delay::new(),
        })
    }

    /// Turns on the high voltages. If a step fails, the function turns them
    /// off again before it returns the error.
    pub fn power_on(&mut self) -> Result<()> {
        let result = self.power_on_steps();
        if result.is_err() {
            // The error of power-on is the one to report.
            let _ = self.power_off();
        }
        result
    }

    fn power_on_steps(&mut self) -> Result<()> {
        self.cfg_writer.set_stv(true);
        self.cfg_writer.config.output_enable = true;
        self.cfg_writer.config.mode = false;
        self.cfg_writer.config.wakeup = true;
        self.cfg_writer.write()?;
        self.cfg_writer.config.pwrup = true;
        self.cfg_writer.write()?;
        self.cfg_writer.config.vcom_ctrl = true;
        self.cfg_writer.write()?;
        self.delay.delay_millis(1);
        let mut tries = 0;
        while !self.cfg_writer.pwrgood()? {
            tries += 1;
            if tries >= POWER_GOOD_TRIES {
                return Err(Error::PowerTimeout);
            }
            self.delay.delay_millis(1);
        }
        self.cfg_writer.enable_tps()?;
        let mut tries = 0;
        while !self.cfg_writer.tps_power_good()? {
            tries += 1;
            if tries >= POWER_GOOD_TRIES {
                return Err(Error::PowerTimeout);
            }
            self.delay.delay_millis(1);
        }
        Ok(())
    }

    /// Turns off the high voltages: VCOM, then the PMIC, then its wake line,
    /// as upstream does. Each call writes the same register values, so a
    /// second call does no harm.
    pub fn power_off(&mut self) -> Result<()> {
        self.cfg_writer.config.vcom_ctrl = false;
        self.cfg_writer.config.pwrup = false;
        self.cfg_writer.config.output_enable = false;
        self.cfg_writer.config.mode = false;
        self.cfg_writer.write()?;
        self.delay.delay_millis(1);
        self.cfg_writer.config.wakeup = false;
        self.cfg_writer.write()?;
        self.cfg_writer.set_stv(false);
        Ok(())
    }

    pub fn touch_sample(&mut self) -> Result<Sample<(u16, u16)>> {
        self.cfg_writer.touch_sample()
    }

    pub async fn init_touch(&mut self) -> Result<(u16, u16)> {
        self.cfg_writer.init_touch().await?;
        Ok(self.cfg_writer.touch_resolution)
    }

    pub fn scan(&mut self, found: &mut [u8; 16]) -> usize {
        self.cfg_writer.scan(found)
    }

    /// Reads `buffer.len()` registers of `device` from `register` on. The
    /// other devices on the bus (for example the charger) use this function.
    pub fn read_registers(&mut self, device: u8, register: u8, buffer: &mut [u8]) -> Result<()> {
        self.cfg_writer
            .i2c
            .write_read(device, &[register], buffer)
            .map_err(Error::I2c)
    }

    /// Writes `payload` (the first register, then the values) to `device`.
    /// The other devices on the bus (for example the charger) use this function.
    pub fn write_registers(&mut self, device: u8, payload: &[u8]) -> Result<()> {
        self.cfg_writer.write_register(device, payload)
    }

    pub fn frame_start(&mut self) -> Result<()> {
        self.cfg_writer.config.mode = true;
        self.cfg_writer.write()?;

        self.rmt.pulse(&pulse!(10, 10))?;
        self.cfg_writer.set_stv(false);
        self.delay.delay_micros(1);
        let data = pulse!(100, 100);
        let transfer = self.rmt.start(&data)?;
        self.cfg_writer.set_stv(true);
        self.rmt.finish(transfer)?;

        self.rmt.pulse(&pulse!(0, 100))?;

        self.cfg_writer.config.output_enable = true;
        self.cfg_writer.write()?;

        self.rmt.pulse(&pulse!(10, 10))?;
        Ok(())
    }

    pub fn latch_row(&mut self) {
        self.cfg_writer.pulse_le();
    }

    pub fn skip(&mut self) -> Result<()> {
        self.rmt.pulse(&pulse!(45, 5))
    }

    pub fn output_row(&mut self, output_time: u16) -> Result<()> {
        self.latch_row();

        let data = pulse!(output_time, 50);
        let transfer = self.rmt.start(&data)?;
        let i8080 = self.i8080.take().ok_or(Error::Missing)?;
        let dma_buf = self.dma_buf.take().ok_or(Error::Missing)?;
        let tx = i8080
            .send(Command::<u8>::One(0), 0, dma_buf)
            .map_err(|(err, i8080, buf)| {
                self.dma_buf = Some(buf);
                self.i8080 = Some(i8080);
                Error::Dma(err)
            })?;
        let (result, i8080, dma_buf) = tx.wait();
        self.i8080 = Some(i8080);
        self.dma_buf = Some(dma_buf);
        self.rmt.finish(transfer)?;
        result.map_err(Error::Dma)
    }

    pub fn frame_end(&mut self) -> Result<()> {
        self.cfg_writer.config.output_enable = false;
        self.cfg_writer.write()?;
        self.cfg_writer.config.mode = false;
        self.cfg_writer.write()?;
        let data = pulse!(10, 10);
        self.rmt.pulse(&data)?;
        self.rmt.pulse(&data)?;
        Ok(())
    }

    pub fn set_buffer(&mut self, data: &[u8]) -> Result<()> {
        let dma_buf = self.dma_buf.as_mut().ok_or(Error::Missing)?;
        let slice = dma_buf.as_mut_slice();
        slice.fill(0);
        slice[..data.len()].copy_from_slice(data);
        Ok(())
    }
}

fn parse_gt911_chip_id(product_id: [u8; 4]) -> u32 {
    let mut value = 0u32;
    for digit in product_id {
        if digit == 0 {
            break;
        }
        if !digit.is_ascii_digit() {
            return 0;
        }
        value = value * 10 + u32::from(digit - b'0');
    }
    value
}

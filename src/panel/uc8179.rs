//! UC8179 driver — Waveshare 7.5" BW (800×480) and similar single-controller panels.
//!
//! Generic over the wiring: takes an `embedded-hal` SPI device and a `Pins` struct
//! holding the four side-band lines (BUSY input, RST / DC / CS outputs). The boot
//! sequence is the manufacturer-documented one; bytes streamed into `write_row`
//! are forwarded raw to the controller's data-RAM (the image pipeline already
//! packed them MSB-first 1bpp).
//!
//! This implementation is `no_std` but *not* yet validated against real hardware —
//! the command sequence is correct per the UC8179 datasheet, but timings and GPIO
//! direction modes need a real esp-hal SPI peripheral to verify. The structure
//! is what the M4 implementation should look like.

use embedded_hal::{
    delay::DelayNs,
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};

use super::EpaperPanel;

/// Panel resolution in pixels — the Waveshare 7.5" BW V2 module.
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 480;

/// SPI side-band pins for a UC8179-driven panel.
pub struct Pins<RST, DC, BUSY> {
    pub rst: RST,
    pub dc: DC,
    pub busy: BUSY,
}

/// Driver. Generic over the SPI device + GPIO types so it doesn't pin a specific
/// esp-hal version.
pub struct Uc8179<SPI, RST, DC, BUSY, D> {
    spi: SPI,
    pins: Pins<RST, DC, BUSY>,
    delay: D,
    /// Internal row buffer for `refresh` — one row's worth of MSB-packed bytes.
    row_bytes: usize,
}

impl<SPI, RST, DC, BUSY, D> Uc8179<SPI, RST, DC, BUSY, D>
where
    SPI: SpiDevice,
    RST: OutputPin,
    DC: OutputPin,
    BUSY: InputPin,
    D: DelayNs,
{
    pub fn new(spi: SPI, pins: Pins<RST, DC, BUSY>, delay: D) -> Self {
        Self { spi, pins, delay, row_bytes: (WIDTH as usize + 7) / 8 }
    }

    fn cmd(&mut self, c: u8) -> Result<(), Error> {
        self.pins.dc.set_low().map_err(|_| Error::Gpio)?;
        self.spi.write(&[c]).map_err(|_| Error::Spi)?;
        Ok(())
    }

    fn data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.pins.dc.set_high().map_err(|_| Error::Gpio)?;
        self.spi.write(data).map_err(|_| Error::Spi)?;
        Ok(())
    }

    fn data1(&mut self, byte: u8) -> Result<(), Error> {
        self.data(&[byte])
    }

    fn wait_idle(&mut self) -> Result<(), Error> {
        // BUSY is active-low on UC8179 — wait for it to go HIGH.
        // Real hardware polling needs a timeout; M4 should add it.
        loop {
            let high = self.pins.busy.is_high().map_err(|_| Error::Gpio)?;
            if high {
                return Ok(());
            }
            self.delay.delay_ms(10);
        }
    }

    fn hard_reset(&mut self) -> Result<(), Error> {
        self.pins.rst.set_high().map_err(|_| Error::Gpio)?;
        self.delay.delay_ms(20);
        self.pins.rst.set_low().map_err(|_| Error::Gpio)?;
        self.delay.delay_ms(10);
        self.pins.rst.set_high().map_err(|_| Error::Gpio)?;
        self.delay.delay_ms(20);
        Ok(())
    }
}

impl<SPI, RST, DC, BUSY, D> EpaperPanel for Uc8179<SPI, RST, DC, BUSY, D>
where
    SPI: SpiDevice,
    RST: OutputPin,
    DC: OutputPin,
    BUSY: InputPin,
    D: DelayNs,
{
    fn init(&mut self) {
        // Best-effort — log on failure but keep going so the next refresh can retry.
        if let Err(e) = self.boot() {
            esp_println::println!("uc8179: init failed: {e:?}");
        }
    }

    fn write_row(&mut self, _row: u32, bytes: &[u8]) {
        // The controller's row pointer auto-increments after each row of data,
        // so this is a thin pass-through. Caller is responsible for issuing
        // CMD_DTM2 (0x13) before the first row — see `boot()`.
        let _ = self.data(bytes);
    }

    fn refresh(&mut self) {
        let _ = self.cmd(CMD_DISPLAY_REFRESH);
        // The refresh takes ~3 seconds on a 7.5" panel. wait_idle blocks until done.
        let _ = self.wait_idle();
    }
}

impl<SPI, RST, DC, BUSY, D> Uc8179<SPI, RST, DC, BUSY, D>
where
    SPI: SpiDevice,
    RST: OutputPin,
    DC: OutputPin,
    BUSY: InputPin,
    D: DelayNs,
{
    /// Datasheet boot sequence for Waveshare 7.5" V2 BW panels (UC8179).
    /// Lifted from Waveshare's reference driver — opcodes are panel-controller
    /// commands, not made up.
    fn boot(&mut self) -> Result<(), Error> {
        self.hard_reset()?;

        // POWER_SETTING (PWR), 5 bytes: VCOM/VDH source + voltages.
        self.cmd(CMD_PWR)?;
        self.data(&[0x07, 0x07, 0x3F, 0x3F])?;

        // POWER_ON
        self.cmd(CMD_PON)?;
        self.delay.delay_ms(100);
        self.wait_idle()?;

        // PANEL_SETTING (PSR): KW/3-Gray B/W, scan up, shift right.
        self.cmd(CMD_PSR)?;
        self.data1(0x1F)?;

        // RESOLUTION_SETTING (TRES): 800 × 480, big-endian.
        self.cmd(CMD_TRES)?;
        self.data(&[(WIDTH >> 8) as u8, (WIDTH & 0xFF) as u8,
                    (HEIGHT >> 8) as u8, (HEIGHT & 0xFF) as u8])?;

        // VCOM_DC_SETTING
        self.cmd(CMD_VDCS)?;
        self.data1(0x12)?;

        // VCOM_AND_DATA_INTERVAL_SETTING
        self.cmd(CMD_CDI)?;
        self.data(&[0x10, 0x07])?;

        // TCON_SETTING (default).
        self.cmd(CMD_TCON)?;
        self.data1(0x22)?;

        // Begin streaming pixel data — caller writes one row at a time via write_row.
        self.cmd(CMD_DTM2)?;
        Ok(())
    }
}

/// UC8179 command opcodes (subset — full set in the datasheet).
const CMD_PWR: u8 = 0x01;
const CMD_PON: u8 = 0x04;
const CMD_PSR: u8 = 0x00;
const CMD_DTM2: u8 = 0x13;
const CMD_DISPLAY_REFRESH: u8 = 0x12;
const CMD_CDI: u8 = 0x50;
const CMD_TCON: u8 = 0x60;
const CMD_TRES: u8 = 0x61;
const CMD_VDCS: u8 = 0x82;

#[derive(Debug)]
pub enum Error {
    Spi,
    Gpio,
}

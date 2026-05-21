//! UC8179 e-paper controller driver.
//!
//! The UC8179 is the controller IC behind Waveshare's 7.5" BW V2 panel and
//! the panel inside Seeed's reTerminal E1001. It accepts a SPI command +
//! data stream, with a side-band DC pin selecting which is which, plus a
//! RST pin for hard reset and a BUSY input for refresh-in-progress.
//!
//! This driver is generic over `embedded-hal` 1.0 traits — that lets the
//! firmware bind it to real `esp-hal` peripherals on the device while the
//! desktop simulator binds the same driver to in-memory fakes that record
//! SPI bytes into the virtual framebuffer. Both paths execute the byte-
//! exact command sequence; protocol-layer bugs surface in both.
//!
//! ## Wire-up
//!
//! On real hardware:
//! - `SPI`: SCK + MOSI + CS as one `SpiDevice` (e.g. `esp_hal::spi::master::Spi` + `ExclusiveDevice`)
//! - `RST`: any `OutputPin` (panel reset, active-low)
//! - `DC`: any `OutputPin` (data/command select; low = command, high = data)
//! - `BUSY`: any `InputPin` (active-low; high when controller idle)
//! - `D`: a `DelayNs` source for reset timing + busy polling
//!
//! ## What's modelled
//!
//! The boot sequence (power-on, panel config, resolution, VCOM, etc.) is the
//! manufacturer-documented one. `write_chunk` forwards bytes to the
//! controller's frame-RAM via CMD_DTM2 (0x13). `refresh` issues CMD_DISPLAY_
//! REFRESH (0x12) and blocks on BUSY going high (~3 s on a 7.5").
//!
//! ## What's not modelled
//!
//! Partial refresh, three-color LUTs, deep-sleep mode. None affect the
//! current data path; the polling protocol always sends a full frame.

#![no_std]

use embedded_hal::{
    delay::DelayNs,
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};
use log::warn;
use paperanywhere_ports::EpaperPanel;

/// Panel resolution in pixels — the 7.5" 800×480 BW V2 module.
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 480;

/// Side-band GPIO bundle for the UC8179.
pub struct Pins<RST, DC, BUSY> {
    pub rst: RST,
    pub dc: DC,
    pub busy: BUSY,
}

/// Driver. Generic over the SPI device + pin types so it doesn't pin a
/// specific esp-hal version (or even a specific platform — the sim binds
/// fake types here).
pub struct Uc8179<SPI, RST, DC, BUSY, D> {
    spi: SPI,
    pins: Pins<RST, DC, BUSY>,
    delay: D,
    /// Whether the panel's DTM2 data plane reads `0 = white, 1 = black`
    /// natively. Many Good Display / Waveshare 7.5" V2 BW modules do
    /// (their reference drivers `~byte` every pixel before sending) but
    /// the same UC8179 controller can ship with the opposite polarity on
    /// some BWR / smaller-grayscale variants. The boards/<name>.rs module
    /// declares this per-panel; the driver applies it transparently inside
    /// `write_chunk`.
    invert_data_plane: bool,
}

impl<SPI, RST, DC, BUSY, D> Uc8179<SPI, RST, DC, BUSY, D>
where
    SPI: SpiDevice,
    RST: OutputPin,
    DC: OutputPin,
    BUSY: InputPin,
    D: DelayNs,
{
    /// Construct the driver.
    ///
    /// `invert_data_plane` controls whether `write_chunk` XORs each byte
    /// with `0xFF` before forwarding to the panel — set per the specific
    /// e-paper module's polarity. Renderers (boot screen, image pipeline,
    /// sim unpack) all speak the same friendly "bit set = white" convention;
    /// the driver translates at the wire boundary when needed.
    pub fn new(
        spi: SPI,
        pins: Pins<RST, DC, BUSY>,
        delay: D,
        invert_data_plane: bool,
    ) -> Self {
        Self { spi, pins, delay, invert_data_plane }
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
        // Real hardware polling needs a timeout; that's a hardening pass.
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

    /// Datasheet boot sequence for the 7.5" V2 BW panels. Opcodes are
    /// controller commands per the UC8179 datasheet — cross-checked against
    /// Waveshare's `EPD_7IN5_V2_Init` reference and Zephyr's
    /// `drivers/display/uc8179.c`. Note the explicit CMD 0x15 (Dual SPI)
    /// disable — UC8179 powers up in dual-SPI mode, which causes each byte
    /// to be interpreted as two 4-bit pixels and visually inverts B/W output
    /// until disabled.
    fn boot(&mut self) -> Result<(), Error> {
        self.hard_reset()?;

        // BTST_P (Booster Soft Start). Skipping this works on bench but
        // leaves the booster ramping sub-optimally — adds visible flash on
        // first refresh. Values are the manufacturer defaults.
        self.cmd(CMD_BTST_P)?;
        self.data(&[0x17, 0x17, 0x28, 0x17])?;

        // POWER_SETTING (PWR): VCOM/VDH source + voltages.
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
        self.data(&[
            (WIDTH >> 8) as u8,
            (WIDTH & 0xFF) as u8,
            (HEIGHT >> 8) as u8,
            (HEIGHT & 0xFF) as u8,
        ])?;

        // DUAL_SPI: disable. **Critical** — UC8179's power-on default has
        // dual-SPI enabled, which packs two 4-bit pixels per byte and makes
        // every B/W image render inverted. Setting this register to 0x00
        // forces single-SPI byte-per-pixel-octet, which is what every BW
        // driver assumes.
        self.cmd(CMD_DUSPI)?;
        self.data1(0x00)?;

        // VCOM_AND_DATA_INTERVAL_SETTING
        self.cmd(CMD_CDI)?;
        self.data(&[0x10, 0x07])?;

        // TCON_SETTING (default).
        self.cmd(CMD_TCON)?;
        self.data1(0x22)?;

        // Begin streaming pixel data — caller pumps bytes via write_chunk.
        self.cmd(CMD_DTM2)?;
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
        // Best-effort — log on failure but keep going so the next refresh
        // gets another chance.
        if let Err(e) = self.boot() {
            warn!("uc8179: init failed: {:?}", e);
        }
    }

    fn write_chunk(&mut self, bytes: &[u8]) {
        if self.invert_data_plane {
            // Renderer-friendly bytes use `1 = white`; this panel reads
            // them with the opposite polarity, so flip at the wire.
            let mut scratch = [0u8; 256];
            for chunk in bytes.chunks(scratch.len()) {
                for (dst, &src) in scratch.iter_mut().zip(chunk.iter()) {
                    *dst = !src;
                }
                let _ = self.data(&scratch[..chunk.len()]);
            }
        } else {
            let _ = self.data(bytes);
        }
    }

    fn refresh(&mut self) {
        let _ = self.cmd(CMD_DISPLAY_REFRESH);
        // The refresh takes ~3 seconds on a 7.5" panel. wait_idle blocks
        // until BUSY goes high; the sim's fake BUSY pin always reports
        // high so this returns immediately there.
        let _ = self.wait_idle();
    }

    fn refresh_fast(&mut self) {
        // Partial-update sequence: PIN → PTL(whole panel) → DRF → POUT.
        // The panel's internal partial-LUT runs a shorter waveform
        // than the OTP full LUT, completing in ~750 ms instead of ~3 s.
        //
        // PTL coordinates: x must be aligned to 8 px (low 3 bits = 0)
        // because the panel addresses sources in 8-pixel groups. We
        // refresh the entire panel here (0 to WIDTH-1, 0 to HEIGHT-1),
        // so alignment is naturally satisfied.
        let _ = self.cmd(CMD_PIN);
        let _ = self.cmd(CMD_PTL);
        let x_end = (WIDTH - 1) as u16;
        let y_end = (HEIGHT - 1) as u16;
        let _ = self.data(&[
            0x00,
            0x00, // x_start = 0 (high byte, low byte)
            (x_end >> 8) as u8,
            (x_end & 0xFF) as u8, // x_end
            0x00,
            0x00, // y_start = 0
            (y_end >> 8) as u8,
            (y_end & 0xFF) as u8, // y_end
            0x01, // PT_SCAN = 1 (keep scan direction as configured by PSR)
        ]);
        let _ = self.cmd(CMD_DISPLAY_REFRESH);
        let _ = self.wait_idle();
        let _ = self.cmd(CMD_POUT);
    }
}

// ── UC8179 command opcodes (subset; full set in the datasheet) ──

/// Power setting.
pub const CMD_PWR: u8 = 0x01;
/// Power on.
pub const CMD_PON: u8 = 0x04;
/// Panel setting.
pub const CMD_PSR: u8 = 0x00;
/// Booster soft start.
pub const CMD_BTST_P: u8 = 0x06;
/// Display start transmission 2 — what we use for the pixel data plane.
pub const CMD_DTM2: u8 = 0x13;
/// Display refresh — kicks the active frame onto the panel.
pub const CMD_DISPLAY_REFRESH: u8 = 0x12;
/// Dual-SPI mode. Power-on default is enabled; we force-disable it during
/// boot so single-byte writes map to 8 pixels (not 2 nibbles).
pub const CMD_DUSPI: u8 = 0x15;
/// VCOM and data interval setting.
pub const CMD_CDI: u8 = 0x50;
/// Source/gate timing — TCON.
pub const CMD_TCON: u8 = 0x60;
/// Resolution setting.
pub const CMD_TRES: u8 = 0x61;
/// Partial window — followed by 7 data bytes specifying x_start
/// (2 bytes), x_end (2 bytes), y_start (2 bytes), y_end (2 bytes),
/// then a single byte for the scan direction (0x00 = same as
/// global). Note: x_start/x_end's low bit must be 0, low nibble 0;
/// the panel addresses pixels in 8-pixel groups.
pub const CMD_PTL: u8 = 0x90;
/// Partial in — switches the controller to partial-update mode so
/// subsequent CMD_DTM2 + CMD_DISPLAY_REFRESH use the partial LUT
/// (faster than the full waveform, with the trade-off of slight
/// ghosting accumulating over many partial refreshes).
pub const CMD_PIN: u8 = 0x91;
/// Partial out — returns the controller to full-refresh mode.
pub const CMD_POUT: u8 = 0x92;
/// VCOM DC setting (unused now — Waveshare's reference for the 7.5" V2
/// panel omits this; the panel's OTP default is fine).
#[allow(dead_code)]
pub const CMD_VDCS: u8 = 0x82;

#[derive(Debug)]
pub enum Error {
    Spi,
    Gpio,
}

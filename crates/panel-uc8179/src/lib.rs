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
};
// Async SPI device. Each `self.spi.write(...).await` yields the embassy
// executor while the SPI peripheral's DMA / FIFO drains, so embassy-net
// can poll incoming WiFi frames during the framebuffer flush. The sync
// variant used to busy-poll the FIFO refill register, holding the CPU
// for the full ~38 ms of a 48 KB framebuffer write — long enough for
// the gateway to ARP-evict our DHCP lease and the device to go offline
// after every panel refresh. Task #90.
use embedded_hal_async::spi::SpiDevice;
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

    async fn cmd(&mut self, c: u8) -> Result<(), Error> {
        self.pins.dc.set_low().map_err(|_| Error::Gpio)?;
        self.spi.write(&[c]).await.map_err(|_| Error::Spi)?;
        Ok(())
    }

    async fn data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.pins.dc.set_high().map_err(|_| Error::Gpio)?;
        self.spi.write(data).await.map_err(|_| Error::Spi)?;
        Ok(())
    }

    async fn data1(&mut self, byte: u8) -> Result<(), Error> {
        self.data(&[byte]).await
    }

    /// Sync busy-poll. Used during boot() where we're not yet inside
    /// an embassy task context. Bounded so a disconnected BUSY pin
    /// doesn't deadlock startup.
    fn wait_idle(&mut self) -> Result<(), Error> {
        const POLL_INTERVAL_MS: u32 = 10;
        const MAX_POLLS: u32 = 1000; // = 10 s at 10 ms per poll
        for _ in 0..MAX_POLLS {
            let high = self.pins.busy.is_high().map_err(|_| Error::Gpio)?;
            if high {
                return Ok(());
            }
            self.delay.delay_ms(POLL_INTERVAL_MS);
        }
        Err(Error::Timeout)
    }

    /// Async busy-poll. Used from inside embassy tasks (refresh /
    /// refresh_fast). Yields to the executor every 10 ms so other
    /// tasks — embassy-net's poll loop, runtime's wake cycle — keep
    /// getting scheduled while the panel is committing its
    /// multi-second refresh. A sync busy-poll here used to be THE
    /// root cause of ICMP timeout clusters during refreshes and
    /// OTA download stalls.
    async fn wait_idle_async(&mut self) -> Result<(), Error> {
        use embassy_time::{Duration, Timer};
        const POLL_INTERVAL_MS: u64 = 10;
        const MAX_POLLS: u32 = 1000; // = 10 s at 10 ms per poll
        for _ in 0..MAX_POLLS {
            let high = self.pins.busy.is_high().map_err(|_| Error::Gpio)?;
            if high {
                return Ok(());
            }
            Timer::after(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
        Err(Error::Timeout)
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
    async fn boot(&mut self) -> Result<(), Error> {
        self.hard_reset()?;

        // BTST_P (Booster Soft Start). Skipping this works on bench but
        // leaves the booster ramping sub-optimally — adds visible flash on
        // first refresh. Values are the manufacturer defaults.
        self.cmd(CMD_BTST_P).await?;
        self.data(&[0x17, 0x17, 0x28, 0x17]).await?;

        // POWER_SETTING (PWR): VCOM/VDH source + voltages.
        self.cmd(CMD_PWR).await?;
        self.data(&[0x07, 0x07, 0x3F, 0x3F]).await?;

        // POWER_ON
        self.cmd(CMD_PON).await?;
        self.delay.delay_ms(100);
        self.wait_idle_async().await?;

        // PANEL_SETTING (PSR): KW/3-Gray B/W, scan up, shift right.
        self.cmd(CMD_PSR).await?;
        self.data1(0x1F).await?;

        // RESOLUTION_SETTING (TRES): 800 × 480, big-endian.
        self.cmd(CMD_TRES).await?;
        self.data(&[
            (WIDTH >> 8) as u8,
            (WIDTH & 0xFF) as u8,
            (HEIGHT >> 8) as u8,
            (HEIGHT & 0xFF) as u8,
        ]).await?;

        // DUAL_SPI: disable. **Critical** — UC8179's power-on default has
        // dual-SPI enabled, which packs two 4-bit pixels per byte and makes
        // every B/W image render inverted. Setting this register to 0x00
        // forces single-SPI byte-per-pixel-octet, which is what every BW
        // driver assumes.
        self.cmd(CMD_DUSPI).await?;
        self.data1(0x00).await?;

        // VCOM_AND_DATA_INTERVAL_SETTING
        self.cmd(CMD_CDI).await?;
        self.data(&[0x10, 0x07]).await?;

        // TCON_SETTING (default).
        self.cmd(CMD_TCON).await?;
        self.data1(0x22).await?;

        // Begin streaming pixel data — caller pumps bytes via write_chunk.
        self.cmd(CMD_DTM2).await?;
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
    async fn init(&mut self) {
        // Best-effort — log on failure but keep going so the next refresh
        // gets another chance.
        if let Err(e) = self.boot().await {
            warn!("uc8179: init failed: {:?}", e);
        }
    }

    async fn begin_frame(&mut self) {
        // Re-enter Display-Transmission-2 mode so the next sequence of
        // write_chunk calls overwrites the panel's frame RAM from the
        // top, instead of appending past wherever the previous refresh
        // left the internal write pointer. Cheap (single 1-byte cmd).
        let _ = self.cmd(CMD_DTM2).await;
    }

    async fn write_chunk(&mut self, bytes: &[u8]) {
        // Each `self.data(...).await` yields during the SPI transfer
        // wait, AND we then explicitly `yield_now().await` between
        // bursts. The explicit yield is the actual fix for the
        // "panel render → device falls off network" symptom:
        //
        //   async `SpiDevice::write` does yield (at FIFO-refill
        //   boundaries), but esp-hal's thread-mode embassy executor
        //   may keep polling this task as soon as the FIFO interrupt
        //   fires — that's the cooperative-scheduler "death by a
        //   thousand papercuts" pattern documented in
        //   esp-rs/esp-wifi-sys#222 + #226. esp-radio's task gets
        //   the same scheduling crumbs the SPI await released, which
        //   on a 48 KB framebuffer means esp-radio can't service
        //   incoming WiFi frames fast enough to ACK beacons / answer
        //   the gateway's ARP probes. The AP eventually times out the
        //   STA (default ~6 s ESP_WIFI_AP_STA_INACTIVITY_TIMER) and
        //   removes us from its association table, which manifests
        //   on the host as "Destination host unreachable" forever.
        //
        // `yield_now` is zero-cost: returns Pending on first poll,
        // executor cleanly checks every task's readiness, polls
        // Ready on second poll. Inserts a hard scheduling boundary
        // between bursts that the executor cannot skip past.
        //
        // Burst size: 256 B for both paths now. 192 yields per
        // 48 KB framebuffer × 64 µs of SPI per burst = 12 ms of
        // executor-yielded time spread across the flush, which is
        // ~6× headroom over esp-radio's minimum service interval.
        // Larger bursts (4 KB) were also tried and gave the same
        // ARP-eviction symptom — what matters is the yield density,
        // not the per-burst size.
        use embassy_futures::yield_now;
        const BURST: usize = 256;
        if self.invert_data_plane {
            // Renderer-friendly bytes use `1 = white`; this panel reads
            // them with the opposite polarity, so flip at the wire.
            let mut scratch = [0u8; BURST];
            for chunk in bytes.chunks(BURST) {
                for (dst, &src) in scratch.iter_mut().zip(chunk.iter()) {
                    *dst = !src;
                }
                let _ = self.data(&scratch[..chunk.len()]).await;
                yield_now().await;
            }
        } else {
            for chunk in bytes.chunks(BURST) {
                let _ = self.data(chunk).await;
                yield_now().await;
            }
        }
    }

    async fn refresh(&mut self) {
        let _ = self.cmd(CMD_DISPLAY_REFRESH).await;
        // The refresh takes ~3 seconds on a 7.5" panel. The async
        // wait yields to the embassy executor every 10 ms so other
        // tasks (embassy-net, ICMP) keep getting scheduled while the
        // panel is committing. The sim's fake BUSY pin always reports
        // high so this returns immediately there.
        let _ = self.wait_idle_async().await;
    }

    async fn refresh_fast(&mut self) {
        // No-flash partial refresh. The PIN+PTL+DRF+POUT sequence on
        // its own does NOT skip the clearing flash — the controller
        // keeps running its OTP LUT, which includes the
        // black-flash → white-flash → final-transition phases that
        // ship from the factory. Restricting the active window via
        // PTL doesn't change which LUT runs.
        //
        // To eliminate the flash we have to push a custom single-
        // phase LUT into the controller's LUT registers (0x20…0x24)
        // and flip PSR bit 5 (LUT_EN) so the panel reads LUTs from
        // those registers instead of the OTP block.
        //
        // Trade-offs vs. the OTP LUT:
        //   - No clearing phase → some ghosting accumulates over
        //     many partial refreshes. The compositor's
        //     `FULL_REFRESH_EVERY` schedule (every 8th refresh) does
        //     a full LUT pass to wipe the ghost.
        //   - LUT values are tuned for the GDEW075T7 panel (the one
        //     in Waveshare 7.5" BW V2 + reTerminal E1001). Other
        //     UC8179 panels may need different waveforms.

        let _ = self.write_partial_luts().await;

        // Tighten the data interval for partial mode. The init value
        // (0x10, 0x07) is optimised for the full LUT's three-phase
        // sweep; in partial mode the panel runs hotter and a wider
        // border (0xA9 = "wide white border, no inversion") gives
        // cleaner edges around updated regions.
        let _ = self.cmd(CMD_CDI).await;
        let _ = self.data(&[0xA9, 0x07]).await;

        // PSR with LUT_EN = 1 (bit 5). 0x3F = KW-3Gray + scan up +
        // shift right + LUT from registers. The init path leaves
        // this at 0x1F (LUT from OTP) for full refreshes.
        let _ = self.cmd(CMD_PSR).await;
        let _ = self.data1(0x3F).await;

        let _ = self.cmd(CMD_PIN).await;
        let _ = self.cmd(CMD_PTL).await;
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
        ]).await;
        let _ = self.cmd(CMD_DISPLAY_REFRESH).await;
        let _ = self.wait_idle_async().await;
        let _ = self.cmd(CMD_POUT).await;

        // Restore PSR + CDI so the next full refresh uses the OTP
        // LUT (needed periodically to clear accumulated ghosting).
        let _ = self.cmd(CMD_PSR).await;
        let _ = self.data1(0x1F).await;
        let _ = self.cmd(CMD_CDI).await;
        let _ = self.data(&[0x10, 0x07]).await;
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
    /// Push the partial-refresh LUT set into controller registers
    /// 0x20…0x24. Five LUTs:
    ///   - 0x20 LUTC  (VCOM)
    ///   - 0x21 LUTWW (white → white, no transition)
    ///   - 0x22 LUTBW (black → white, drive VSH)
    ///   - 0x23 LUTWB (white → black, drive VSL)
    ///   - 0x24 LUTBB (black → black, no transition)
    ///
    /// Each LUT is 42 bytes = 7 phases × 6 bytes/phase. Per phase:
    /// `[VS, FN1, FN2, FN3, FN4, RPT]` where VS packs four 2-bit
    /// sub-stage voltage selectors (00 = GND, 01 = VSH, 10 = VSL,
    /// 11 = VSL alt) and FN1-4 are the per-sub-stage frame counts.
    ///
    /// We use a single-phase waveform: ~15 frames driving the target
    /// voltage, then six unused phases of zeros. This skips the
    /// OTP LUT's clearing phases — the visible flash — at the cost
    /// of a small amount of ghosting per refresh that the compositor
    /// periodically clears with a full refresh.
    async fn write_partial_luts(&mut self) -> Result<(), Error> {
        self.cmd(CMD_LUTC).await?;
        self.data(&LUT_VCOM_PARTIAL).await?;
        self.cmd(CMD_LUTWW).await?;
        self.data(&LUT_WW_PARTIAL).await?;
        self.cmd(CMD_LUTBW).await?;
        self.data(&LUT_BW_PARTIAL).await?;
        self.cmd(CMD_LUTWB).await?;
        self.data(&LUT_WB_PARTIAL).await?;
        self.cmd(CMD_LUTBB).await?;
        self.data(&LUT_BB_PARTIAL).await?;
        Ok(())
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

/// LUT for VCOM during a refresh — 42 bytes follow. Only consulted
/// when PSR bit 5 (LUT_EN) is set; the OTP LUT is used otherwise.
pub const CMD_LUTC: u8 = 0x20;
/// LUT for white→white pixel transitions (no change). 42 bytes.
pub const CMD_LUTWW: u8 = 0x21;
/// LUT for black→white pixel transitions (drive the pixel to the
/// white voltage). 42 bytes.
pub const CMD_LUTBW: u8 = 0x22;
/// LUT for white→black pixel transitions (drive to black). 42 bytes.
pub const CMD_LUTWB: u8 = 0x23;
/// LUT for black→black pixel transitions (no change). 42 bytes.
pub const CMD_LUTBB: u8 = 0x24;

// ── Partial-refresh waveform LUTs ───────────────────────────────────
//
// Each LUT is 42 bytes (7 phases × 6 bytes). We use a single phase:
// drive the target voltage for ~15 frames, then six empty phases.
// Tuned for the GDEW075T7 (Waveshare 7.5" BW V2 + reTerminal E1001).
//
// Phase byte layout: `[VS, FN1, FN2, FN3, FN4, RPT]`
//   - VS: four 2-bit sub-stage voltage selectors packed MSB-first.
//         00 = GND, 01 = VSH (+), 10 = VSL (-), 11 = VSL alt.
//   - FN1..FN4: frame counts for sub-stages 0..3.
//   - RPT: phase repeat count.

/// 42-byte LUT where phase 0 sits at GND for 15 frames, repeat 1 ×
/// — i.e. "do nothing". Used for VCOM and the WW/BB no-change LUTs.
const LUT_NO_TRANSITION: [u8; 42] = {
    let mut lut = [0u8; 42];
    lut[0] = 0x00; // VS: all four sub-stages at GND
    lut[1] = 0x0F; // FN1 = 15 frames
    lut[5] = 0x01; // RPT = 1
    lut
};

const LUT_VCOM_PARTIAL: [u8; 42] = LUT_NO_TRANSITION;
const LUT_WW_PARTIAL: [u8; 42] = LUT_NO_TRANSITION;
const LUT_BB_PARTIAL: [u8; 42] = LUT_NO_TRANSITION;

/// LUT for black→white pixels: drive VSH (positive) for 15 frames.
/// 0x40 = 01_00_00_00 — sub-stage 0 selects VSH, the rest GND.
const LUT_BW_PARTIAL: [u8; 42] = {
    let mut lut = [0u8; 42];
    lut[0] = 0x40;
    lut[1] = 0x0F;
    lut[5] = 0x01;
    lut
};

/// LUT for white→black pixels: drive VSL (negative) for 15 frames.
/// 0x80 = 10_00_00_00 — sub-stage 0 selects VSL, the rest GND.
const LUT_WB_PARTIAL: [u8; 42] = {
    let mut lut = [0u8; 42];
    lut[0] = 0x80;
    lut[1] = 0x0F;
    lut[5] = 0x01;
    lut
};

#[derive(Debug)]
pub enum Error {
    Spi,
    Gpio,
    /// BUSY pin didn't go high within `wait_idle`'s deadline. Typically
    /// indicates the panel cable is loose, the panel browned out during
    /// the refresh, or the BUSY GPIO isn't actually wired up. Callers
    /// should log and continue; the next refresh attempt will retry.
    Timeout,
}

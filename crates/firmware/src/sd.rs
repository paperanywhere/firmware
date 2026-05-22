//! SD-card driver: SPI-mode, mounts FAT16/FAT32 via embedded-sdmmc,
//! shares SPI2 with the e-paper panel.
//!
//! ## Hardware (reTerminal E1001, schematic V1.0)
//!
//! | Net    | GPIO | Notes                                             |
//! |--------|------|---------------------------------------------------|
//! | SCK    |   7  | shared with panel SCK                             |
//! | MOSI   |   9  | shared with panel MOSI                            |
//! | MISO   |   8  | SD-only (UC8179 is write-only)                    |
//! | SD_CS  |  14  | chip select, active-low                           |
//! | SD_DET |  15  | card-detect input (active-low when card present)  |
//! | SD_EN  |  16  | TPS22916 load-switch enable (active-high)         |
//!
//! ## Bus sharing
//!
//! The panel uses `embedded_hal_async::spi::SpiDevice`. The SD
//! driver (`embedded-sdmmc`) uses the *blocking* trait. Both want
//! the same physical SPI2 bus.
//!
//! We park `Spi<'static, Async>` in an `embassy_sync::mutex::Mutex`
//! (`boards::SharedSpiBus`). The panel wraps it with
//! `embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice`
//! (implements async `SpiDevice`). The SD wraps the same Mutex
//! with [`SharedBusSdSpi`] below — a small blocking adapter that
//! uses `embassy_futures::block_on` around the async lock + bus
//! operations to satisfy embedded-sdmmc's blocking trait.
//!
//! Why this works: SD transactions are short (microseconds-to-
//! milliseconds). `block_on` parks the current task until the bus
//! ops finish, but doesn't stall the executor — other tasks
//! continue to run while the SD adapter is waiting on its own
//! futures. Mutex contention is naturally bounded by the SD's
//! transaction length.

use core::cell::RefCell;

use embassy_futures::block_on;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{ErrorType as BlockingErrorType, Operation, SpiDevice};
// Bring both async SpiBus methods (read/write/transfer/etc.) and
// embedded-sdmmc's BlockDevice trait into method-resolution scope.
// Aliases prevent collision: `embedded_sdmmc::BlockDevice` would
// otherwise shadow `crate::swap::BlockDevice` further down the file.
use embedded_hal_async::spi::SpiBus as _;
use embedded_sdmmc::BlockDevice as _;
use esp_hal::Async;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::Spi;

use crate::boards::SdBoard;

/// Hardware bundle the SD driver owns. The shared SPI bus + the
/// SD-only GPIOs. CS lives on `SharedBusSdSpi`, not here — `cs`
/// is the underlying line that the adapter toggles per
/// transaction.
pub struct SdHardware {
    pub cs: Output<'static>,
    pub detect: Input<'static>,
    pub power_enable: Output<'static>,
}

/// Blocking SpiDevice wrapper over an async-mode shared SPI bus.
/// Used by `embedded_sdmmc::SdCard`, which requires the blocking
/// `embedded_hal::spi::SpiDevice` trait. Internally drives CS
/// itself and uses `embassy_futures::block_on` around the async
/// `Mutex::lock` + `SpiBus` calls.
///
/// `'a` is the lifetime of the shared bus reference. Constructed
/// from a `&'a Mutex<CSRawMutex, Spi<Async>>` + an `Output` pin
/// for chip-select.
pub struct SharedBusSdSpi<'a> {
    bus: &'a Mutex<CriticalSectionRawMutex, Spi<'static, Async>>,
    cs: RefCell<Output<'static>>,
}

impl<'a> SharedBusSdSpi<'a> {
    pub fn new(
        bus: &'a Mutex<CriticalSectionRawMutex, Spi<'static, Async>>,
        cs: Output<'static>,
    ) -> Self {
        Self {
            bus,
            cs: RefCell::new(cs),
        }
    }
}

/// SPI error surfaced through the blocking SpiDevice trait. The
/// concrete underlying error is `esp_hal::spi::Error` but we type-
/// erase it at the trait boundary so we don't expose esp-hal in the
/// `BlockDevice` Error position.
#[derive(Debug)]
pub struct SharedBusSpiError;

impl embedded_hal::spi::Error for SharedBusSpiError {
    fn kind(&self) -> embedded_hal::spi::ErrorKind {
        embedded_hal::spi::ErrorKind::Other
    }
}

impl<'a> BlockingErrorType for SharedBusSdSpi<'a> {
    type Error = SharedBusSpiError;
}

impl<'a> SpiDevice for SharedBusSdSpi<'a> {
    fn transaction(
        &mut self,
        operations: &mut [Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        block_on(async {
            let mut bus_guard = self.bus.lock().await;
            let bus: &mut Spi<'static, Async> = &mut *bus_guard;
            let mut cs = self.cs.borrow_mut();
            // esp-hal's `Output` has an inherent `set_low()` that
            // returns `()`, shadowing the OutputPin trait method.
            // Call through the trait to get the Result-returning
            // form.
            <Output<'_> as OutputPin>::set_low(&mut *cs)
                .map_err(|_| SharedBusSpiError)?;
            let mut result: Result<(), SharedBusSpiError> = Ok(());
            for op in operations.iter_mut() {
                let r: Result<(), SharedBusSpiError> = match op {
                    Operation::Read(buf) => {
                        <Spi<'_, Async> as embedded_hal_async::spi::SpiBus<u8>>::read(bus, buf)
                            .await
                            .map_err(|_| SharedBusSpiError)
                    }
                    Operation::Write(buf) => {
                        <Spi<'_, Async> as embedded_hal_async::spi::SpiBus<u8>>::write(bus, buf)
                            .await
                            .map_err(|_| SharedBusSpiError)
                    }
                    Operation::Transfer(rx, tx) => {
                        <Spi<'_, Async> as embedded_hal_async::spi::SpiBus<u8>>::transfer(
                            bus, rx, tx,
                        )
                        .await
                        .map_err(|_| SharedBusSpiError)
                    }
                    Operation::TransferInPlace(buf) => {
                        <Spi<'_, Async> as embedded_hal_async::spi::SpiBus<u8>>::transfer_in_place(
                            bus, buf,
                        )
                        .await
                        .map_err(|_| SharedBusSpiError)
                    }
                    Operation::DelayNs(ns) => {
                        let ms = (*ns).div_ceil(1_000_000);
                        embassy_time::Timer::after(
                            embassy_time::Duration::from_millis(ms as u64),
                        )
                        .await;
                        Ok(())
                    }
                };
                if r.is_err() {
                    result = r;
                    break;
                }
            }
            // CS goes back high regardless of the result so the
            // bus is left in a clean idle state.
            let _ = <Output<'_> as OutputPin>::set_high(&mut *cs);
            result
        })
    }
}

// ── Time source for embedded-sdmmc ──
//
// embedded-sdmmc's `VolumeManager` requires a `TimeSource` trait so
// it can stamp newly-created files. We don't have a real wall
// clock until task #78 (NTP) lands; until then, return a fixed
// pre-2026 timestamp. Files end up dated 2026-01-01 — fine.

struct FixedTimeSource;

impl embedded_sdmmc::TimeSource for FixedTimeSource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        embedded_sdmmc::Timestamp {
            year_since_1970: 56, // 2026
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

// ── FwSd: the live driver ──

/// Live SD-card handle: owns the bus adapter, the card-detect /
/// power-enable GPIOs, and the embedded-sdmmc `SdCard` instance.
/// Once mounted, `FwSd` is the entry point for filesystem access
/// + the [`crate::swap::BlockDevice`] backing for SwapAlloc.
pub struct FwSd {
    inner: embedded_sdmmc::SdCard<SharedBusSdSpi<'static>, Delay>,
    /// Card-detect input — re-polled by callers that want to
    /// catch a hot-swap mid-session.
    pub detect: Input<'static>,
    /// Power-enable output, retained so we can drop power before
    /// deep sleep.
    pub power_enable: Output<'static>,
    /// Per-board quirks (timings, polarities, retry budgets).
    /// Driver code reads these knobs rather than embedding
    /// board-specific constants.
    quirks: SdBoard,
}

/// What `mount` returns on the happy path. `FwSd` itself doesn't
/// impl Debug (the inner SdCard wraps a non-Debug SPI adapter),
/// so the Mounted variant is opaque — callers either match on
/// the variant or use `mount().into_driver()`.
pub enum MountState {
    /// Card detected + initialised. Carries the live driver
    /// handle so caller can immediately use it as a swap backing
    /// store.
    Mounted(FwSd),
    /// No card in the slot — `detect` reads as not-present.
    NoCard,
    /// Card present but init / mount failed at the driver layer.
    Failed(SdError),
}

impl MountState {
    /// One-line summary suitable for logging. Avoids forcing
    /// `Debug` on the inner driver.
    pub fn describe(&self) -> &'static str {
        match self {
            MountState::Mounted(_) => "Mounted",
            MountState::NoCard => "NoCard",
            MountState::Failed(SdError::PowerUp) => "Failed(PowerUp)",
            MountState::Failed(SdError::InitProtocol) => "Failed(InitProtocol)",
        }
    }
    /// Consume into the driver handle on success.
    pub fn into_driver(self) -> Option<FwSd> {
        match self {
            MountState::Mounted(sd) => Some(sd),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum SdError {
    /// Card power-up sequence (drive power_enable, wait, sample
    /// CMD0/ACMD41) failed.
    PowerUp,
    /// SPI transaction error at the card-init protocol level.
    InitProtocol,
}

impl FwSd {
    /// Initialise the SD: drive power_enable per quirks, settle,
    /// build the SPI adapter, hand to `embedded_sdmmc::SdCard`,
    /// confirm the card responds. Returns the live driver on
    /// success.
    ///
    /// FAT32 mount + format-on-first-boot is a separate step
    /// (`open_volume` + `fatfs::format_volume` for reformatting)
    /// — that lands in a follow-up once the format-helper path is
    /// validated. Today's contract: "card is alive, addressable
    /// blocks reachable" — which is exactly what `BlockDevice` +
    /// SwapAlloc need.
    pub fn mount(
        bus: &'static crate::boards::SharedSpiBus,
        mut hw: SdHardware,
        quirks: &SdBoard,
    ) -> MountState {
        let card_present = if quirks.detect_active_low {
            hw.detect.is_low()
        } else {
            hw.detect.is_high()
        };
        if !card_present {
            log::info!("sd: slot empty (detect polarity-checked) — skipping mount");
            return MountState::NoCard;
        }

        // Power-cycle into a known state. Drive enable to its
        // active level, then hold for the board's settle window.
        if quirks.power_enable_active_high {
            hw.power_enable.set_high();
        } else {
            hw.power_enable.set_low();
        }
        Delay::new().delay_millis(quirks.power_up_delay_ms);

        // Hand the CS line + shared bus to the blocking adapter.
        let spi = SharedBusSdSpi::new(bus, hw.cs);
        let sdcard = embedded_sdmmc::SdCard::new(spi, Delay::new());

        // Card probe: num_bytes() forces the driver to walk the
        // SPI-mode init sequence (CMD0 -> CMD8 -> ACMD41 ->
        // CMD58). Failure here means the card didn't respond
        // within the init budget.
        match sdcard.num_bytes() {
            Ok(bytes) => {
                log::info!(
                    "sd: mounted (size = {} MB; quirks: retries={}, settle={}ms)",
                    bytes / 1_048_576,
                    quirks.init_retry_count,
                    quirks.power_up_delay_ms
                );
            }
            Err(e) => {
                log::warn!("sd: card init failed: {:?}", e);
                return MountState::Failed(SdError::InitProtocol);
            }
        }

        MountState::Mounted(FwSd {
            inner: sdcard,
            detect: hw.detect,
            power_enable: hw.power_enable,
            quirks: *quirks,
        })
    }

    /// Capacity in 512-byte blocks. Exposed so `SwapAlloc` (and
    /// any future consumer) can compute reservable address space.
    pub fn num_blocks(&self) -> Result<u32, SdError> {
        self.inner
            .num_blocks()
            .map(|b| b.0)
            .map_err(|_| SdError::InitProtocol)
    }

    /// Quirks the driver was built with (read-only). Useful for
    /// logging + cross-checking against the catalog.
    pub fn quirks(&self) -> &SdBoard {
        &self.quirks
    }
}

// ── BlockDevice integration with the swap allocator ──

impl crate::swap::BlockDevice for FwSd {
    type Error = SdError;

    fn read_blocks(&mut self, block_idx: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let block_count = buf.len() / 512;
        // embedded_sdmmc's read works one block at a time via
        // `read` with a Block buffer. We iterate so SwapAlloc's
        // multi-block page reads stay efficient (~4 KB / 8 blocks
        // per page).
        for i in 0..block_count {
            let mut block = [embedded_sdmmc::Block::new()];
            self.inner
                .read(&mut block, embedded_sdmmc::BlockIdx(block_idx + i as u32), "swap")
                .map_err(|_| SdError::InitProtocol)?;
            let off = i * 512;
            buf[off..off + 512].copy_from_slice(&block[0].contents);
        }
        Ok(())
    }

    fn write_blocks(&mut self, block_idx: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let block_count = buf.len() / 512;
        for i in 0..block_count {
            let off = i * 512;
            let mut block = embedded_sdmmc::Block::new();
            block.contents.copy_from_slice(&buf[off..off + 512]);
            self.inner
                .write(&[block], embedded_sdmmc::BlockIdx(block_idx + i as u32))
                .map_err(|_| SdError::InitProtocol)?;
        }
        Ok(())
    }
}

/// Constructor helper for boards that declare an `SdBoard`. Wraps
/// the per-board pin numbers in the GPIO drivers `FwSd::mount`
/// expects + captures the per-board quirks. main.rs supplies the
/// raw `peripherals.GPIOxx` handles since they're feature-cfg'd
/// already.
pub fn sd_pins_for(
    board: &SdBoard,
    cs: Output<'static>,
    detect: Input<'static>,
    power_enable: Output<'static>,
) -> SdHardware {
    let _ = (
        board.miso,
        board.cs,
        board.detect,
        board.power_enable,
        board.detect_active_low,
        board.power_enable_active_high,
        board.power_up_delay_ms,
        board.init_retry_count,
    );
    SdHardware {
        cs,
        detect,
        power_enable,
    }
}

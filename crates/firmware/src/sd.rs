//! SD-card driver bindings + FAT32 mount / format-on-first-boot.
//!
//! ## Hardware
//!
//! reTerminal E1001 wires the SD slot in **SPI mode** on the same
//! SPI2 peripheral the e-paper panel uses. Pin map confirmed against
//! the V1.0 schematic
//! (`202004307_reTerminal_E1001_V1.0_SCH_250805.pdf`):
//!
//! | Net          | GPIO | Notes                                             |
//! |--------------|------|---------------------------------------------------|
//! | SCK          |   7  | shared with panel SCK                             |
//! | MOSI         |   9  | shared with panel MOSI                            |
//! | MISO         |   8  | SD-only (UC8179 is write-only)                    |
//! | SD_CS        |  14  | active-low chip select                            |
//! | SD_DET       |  15  | card-detect input (active low when card inserted) |
//! | SD_EN        |  16  | TPS22916 load-switch enable (drives card power)   |
//!
//! ## Why we use SPI mode, not native SDMMC
//!
//! esp-hal 1.1 exposes no SDMMC peripheral driver — only SPI, I2C,
//! UART, etc. SD cards have a fallback SPI interface mandated by the
//! spec, and the reTerminal hardware is already wired that way, so
//! `embedded-sdmmc` (SPI-only) is the natural fit. Theoretical
//! topology for a future native SDMMC port would give us ~4× the
//! throughput, but writing an esp-hal SDMMC driver is multi-week
//! work that's not justified for the file sizes we're shifting
//! (single graph rasters ≤ 1 MB, never approaching the SPI ceiling).
//!
//! ## Bus sharing
//!
//! The panel currently owns SPI2 exclusively via
//! `ExclusiveDevice<Spi<Async>, ...>`. SD also needs SPI2, so the
//! bus has to be wrapped in a shared device (e.g.
//! `embedded_hal_bus::spi::CriticalSectionDevice`) with separate
//! CS-aware wrappers for panel + SD. This refactor sits behind the
//! [`mount`] stub below and is the next concrete step for landing
//! end-to-end SD support on hardware.

use esp_hal::gpio::{Input, Output};

use crate::boards::SdPinMap;

/// Hardware bundle the SD driver owns. The shared SPI bus + the
/// SD-only GPIOs. CS lives in the bus device wrapper, not here —
/// `cs` is the underlying line, handed to the shared device factory
/// at construction.
pub struct SdHardware {
    /// Active-low chip-select line for the SD card. The shared bus
    /// device flips this for the duration of each SD transfer.
    pub cs: Output<'static>,
    /// Card-detect input — pulled low when a card is inserted.
    /// Polled at boot (and on demand) so the firmware can skip SD
    /// init entirely if the slot is empty.
    pub detect: Input<'static>,
    /// Power-enable output for the SD load switch (TPS22916).
    /// Active high. Held low during deep sleep so the SD doesn't
    /// drain the battery; pulled high before any access.
    pub power_enable: Output<'static>,
}

/// What `mount` returns on the happy path. The actual filesystem
/// handle (`embedded_sdmmc::VolumeManager`) lives behind this
/// opaque struct so callers don't have to depend on the crate
/// directly.
pub struct FwSd {
    _hw: SdHardware,
    // Filesystem handle lands here once the SPI-share refactor
    // makes it possible to construct an embedded_sdmmc::BlockSpi
    // against a shared bus device. Field is private + unused
    // today; placeholder so call sites can type against `FwSd`
    // immediately.
}

/// Outcomes from the boot-time SD setup.
#[derive(Debug)]
pub enum MountState {
    /// Card detected, mounted FAT32 cleanly.
    Mounted,
    /// Card detected but filesystem was unrecognised; reformatted
    /// to FAT32 in place. Bytes on the card before mount are gone.
    ReformattedFat32,
    /// No card in the slot — `detect` reads high.
    NoCard,
    /// Card present but init / format failed at the driver layer.
    /// Carry the error so the boot path can surface a halt-screen
    /// code rather than silently continuing without storage.
    Failed(SdError),
}

/// SD-layer errors. Sits above the underlying embedded-sdmmc /
/// SPI error types so callers can match against a stable set of
/// variants regardless of which sub-crate produced the failure.
#[derive(Debug)]
pub enum SdError {
    /// Card power-up sequence (drive `power_enable` high, wait
    /// for stabilization, sample CMD0 ACMD41 init) failed.
    PowerUp,
    /// SPI transaction error at the card-init protocol level.
    InitProtocol,
    /// Filesystem mount failed AND the in-place reformat also
    /// failed — usually means a dead / write-protected card.
    MountAndFormat,
    /// SPI bus sharing isn't wired in this build yet. Returned by
    /// the placeholder `mount` while [`crate::sd`]'s real binding
    /// lands. Once SPI sharing is in place this variant goes away.
    SpiShareNotWired,
}

impl FwSd {
    /// Initialise the SD card: power up, probe, mount FAT32,
    /// reformat in-place if the existing filesystem isn't FAT32.
    ///
    /// Today this is a stub that returns
    /// `MountState::Failed(SpiShareNotWired)`. The remaining work
    /// to make it real:
    ///
    /// 1. Refactor `boot.rs` so the panel's SPI2 bus is wrapped in
    ///    a `CriticalSectionDevice` factory shared across two
    ///    consumers (panel + SD).
    /// 2. Construct an `embedded_sdmmc::SdCard<SharedSpiDevice, _>`
    ///    against the shared device.
    /// 3. Probe via `SdCard::num_bytes()`; on success, `VolumeManager`
    ///    + `open_volume(VolumeIdx(0))` and verify the FAT type
    ///    is FAT32.
    /// 4. On mount-failure or wrong-FAT, use the `fatfs` crate's
    ///    `format_volume` to reformat (caveat: this needs a blocking
    ///    `Read + Write + Seek` over the card; embedded-sdmmc's
    ///    `BlockDevice` doesn't satisfy that directly, so we'd ship
    ///    a tiny adapter).
    /// 5. Drive `power_enable` low when mount completes failure-OK
    ///    so the card doesn't keep drawing current on devices that
    ///    are about to deep-sleep.
    ///
    /// The signature is final — only the body is a placeholder.
    pub fn mount(_hw: SdHardware) -> MountState {
        // Card-detect check is the one piece that's truthful at
        // this stage: if no card is in the slot, no amount of SPI
        // wiring will help.
        let inserted = _hw.detect.is_low();
        if !inserted {
            log::info!("sd: slot empty (card-detect high) — skipping mount");
            return MountState::NoCard;
        }
        log::info!(
            "sd: card detected but mount path is stubbed pending SPI-bus sharing (task #116)"
        );
        MountState::Failed(SdError::SpiShareNotWired)
    }
}

/// Constructor helper for boards that declare an `SdPinMap`. Wraps
/// the per-board pin numbers in the GPIO drivers `FwSd::mount`
/// expects. main.rs supplies the raw `peripherals.GPIOxx` handles
/// since they're board-feature-cfg'd already.
pub fn sd_pins_for(
    pin_map: &SdPinMap,
    cs: Output<'static>,
    detect: Input<'static>,
    power_enable: Output<'static>,
) -> SdHardware {
    // The pin-number fields exist for documentation + future
    // sanity-checks; the actual GPIO drivers already encode which
    // pin they are. Suppress unused-warning by referring to them.
    let _ = (pin_map.cs, pin_map.detect, pin_map.power_enable, pin_map.miso);
    SdHardware {
        cs,
        detect,
        power_enable,
    }
}

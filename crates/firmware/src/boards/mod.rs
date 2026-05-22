//! Board configuration trait + per-board modules.
//!
//! Each `BoardConfig` declares the panel driver, pin map, integrated peripherals,
//! and default power profile. The boot path is identical across boards — only
//! this struct varies. New products plug in here.
//!
//! Also exported: `Panel` (the concrete `EpaperPanel`-implementing type for
//! the active board) and `build_panel`, which assembles the driver from the
//! GPIO/SPI handles main.rs gathered into `FirmwareResources`.

use core::cell::RefCell;

use critical_section::Mutex;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::Async;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::Spi;
use paperanywhere_panel_uc8179::{Pins, Uc8179};

use crate::resources::PanelHardware;

/// Future home of the panel+SD shared SPI2 bus. Today the panel
/// owns the bus exclusively via `ExclusiveDevice`; sharing requires
/// a blocking-over-async adapter (the panel uses async SpiDevice,
/// embedded-sdmmc uses blocking) which is its own focused chunk of
/// work — see task #116 + `crate::sd` for the design path.
///
/// Type alias kept as a documentation handle so the SD module can
/// reference the eventual shape without us having to plumb the
/// final wrapper through every call site preemptively.
pub type SharedSpiBus = Mutex<RefCell<Spi<'static, Async>>>;

/// Bare panel driver, before the compositor wraps it. The SPI bus
/// runs in **async** mode (`Spi<'static, Async>`) so each transfer
/// yields to the embassy executor while the FIFO drains, instead of
/// busy-polling. Without this, the 48 KB framebuffer flush blocked
/// embassy-net's WiFi-RX poll for ~38 ms straight — long enough for
/// the gateway to ARP-evict the device's DHCP lease. Task #90.
///
/// `embassy_time::Delay` (rather than `esp_hal::delay::Delay`) satisfies
/// `ExclusiveDevice`'s `D: AsyncDelayNs` bound that comes with the
/// `embedded-hal-bus` "async" feature. The UC8179 driver doesn't
/// invoke its DelayNs slot, so this is effectively unused — but the
/// trait bound has to be satisfied for `AsyncSpiDevice` to be in
/// scope on the ExclusiveDevice wrapper.
pub type BarePanel = Uc8179<
    ExclusiveDevice<Spi<'static, Async>, Output<'static>, embassy_time::Delay>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Delay,
>;

/// Concrete `EpaperPanel` type the rest of the firmware sees. The
/// compositor wraps the bare panel driver and reserves the top
/// [`paperanywhere_compositor::DEFAULT_STATUS_BAR_HEIGHT`] rows of the
/// panel for the status bar (battery / wifi widgets). Runtime renders
/// land in the main region only.
pub type Panel = paperanywhere_compositor::Compositor<BarePanel>;

/// Assemble the SPI device (bus + CS), instantiate the UC8179
/// driver against the runtime-typed pins main.rs gathered, and
/// wrap it in the compositor so the runtime sees a layered
/// framebuffer. `invert_data_plane` is sourced from the board's
/// `BoardConfig`.
pub fn build_panel(hw: PanelHardware, board: BoardConfig) -> Panel {
    let delay = Delay::new();
    let spi_device = ExclusiveDevice::new(hw.spi_bus, hw.cs, embassy_time::Delay)
        .expect("ExclusiveDevice::new (embassy_time::Delay is infallible)");
    let bare = Uc8179::new(
        spi_device,
        Pins { rst: hw.rst, dc: hw.dc, busy: hw.busy },
        delay,
        board.panel_data_inverted,
    );
    paperanywhere_compositor::Compositor::new(
        bare,
        board.panel_width_px,
        board.panel_height_px,
        board.default_color_mode_ports(),
        paperanywhere_compositor::DEFAULT_STATUS_BAR_HEIGHT,
    )
}

// Local enum stubs replacing paperanywhere-proto imports while proto is
// detached from this crate (see Cargo.toml note). When proto comes back these
// re-export from paperanywhere_proto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode { Mono1bpp, MonoRed1bpp, MonoYellow1bpp, Gray4, Gray16, Color7 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackingKind {
    RowMajorMsbFirst1bpp, RowMajorLsbFirst1bpp,
    RowMajorBe2bpp, RowMajorBe4bpp, AcepIndexed4bpp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerPolicy { ScheduledWake, AlwaysOn }

/// Board-specific SD-card binding — pins + behavioural quirks.
/// The core SD driver (`crate::sd`) is generic over this struct so
/// the in-firmware logic stays board-agnostic while every per-board
/// detail (which GPIO holds the card-detect, whether power-enable
/// is active-high, how many milliseconds the load switch needs to
/// settle) lives in the board file. The pattern matches the rest
/// of the per-board catalog and survives the future migration to
/// the `paperanywhere-devices` submodule (task #69) without
/// changing the driver.
///
/// SCK + MOSI are intentionally not listed here — they're shared
/// with the panel's SPI bus and live on the panel pin map. The
/// `FwSd` driver borrows the panel's bus through a shared device
/// wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdBoard {
    // ── Pins ──
    /// SD's data-out line (MCU input). reTerminal E1001 = GPIO8.
    pub miso: u8,
    /// SD chip select. reTerminal E1001 = GPIO14.
    pub cs: u8,
    /// Card-detect input. reTerminal E1001 = GPIO15.
    pub detect: u8,
    /// SD power-enable output. reTerminal E1001 = GPIO16, drives
    /// a TPS22916 load switch.
    pub power_enable: u8,

    // ── Quirks ──
    /// `true` if the card-detect line reads LOW when a card is
    /// inserted (most common; reTerminal = true). `false` for
    /// boards that wire detect inverted.
    pub detect_active_low: bool,
    /// `true` if the power-enable line must go HIGH to supply the
    /// card (TPS22916-style; reTerminal = true). `false` for
    /// active-low PMOS load switches.
    pub power_enable_active_high: bool,
    /// Settle time after asserting power-enable before the first
    /// SPI command. TPS22916 rise time on the E-series is ~1 ms;
    /// 10 ms gives a comfortable margin across power-rail
    /// tolerances and avoids racy "card not responding" failures.
    pub power_up_delay_ms: u32,
    /// Max number of CMD0/ACMD41 init retries before declaring the
    /// card dead. embedded-sdmmc's default is conservative; some
    /// E-series carriers tolerate fewer because the power supply
    /// is tighter.
    pub init_retry_count: u8,
}

/// Capabilities + pin map for a specific physical device. Constructed once at
/// boot by `current()` based on the active Cargo feature.
#[derive(Debug, Clone, Copy)]
pub struct BoardConfig {
    /// Combined "Manufacturer Model" string. Kept for legacy code
    /// paths (logs, future telemetry payloads) that surface a single
    /// display name. New code should prefer `manufacturer` + `model`
    /// separately so the boot screen can render them in distinct
    /// columns.
    pub name: &'static str,
    /// Hardware manufacturer (e.g. "Seeed Studio", "M5Stack").
    pub manufacturer: &'static str,
    /// Specific model identifier (e.g. "reTerminal E1001",
    /// "Inkplate 6"). Together with `manufacturer` reproduces `name`.
    pub model: &'static str,
    pub panel_model_id: i32,
    pub panel_width_px: u32,
    pub panel_height_px: u32,
    pub default_color_mode: ColorMode,
    pub default_packing: PackingKind,
    pub default_power_policy: PowerPolicy,
    pub default_sleep_interval_sec: u32,
    pub has_battery: bool,
    pub has_buttons: bool,
    pub has_sensors: bool,
    pub has_buzzer: bool,
    pub has_sd_card: bool,
    pub panel_busy: u8,
    pub panel_rst: u8,
    pub panel_dc: u8,
    pub panel_cs: u8,
    pub panel_sclk: u8,
    pub panel_mosi: u8,
    pub battery_adc: Option<u8>,
    /// SD card binding (pins + per-board behavioural quirks).
    /// `None` on boards without an SD slot. On the reTerminal
    /// E-series the SD shares SCK + MOSI with the panel.
    pub sd: Option<SdBoard>,
    /// Whether the panel's data plane is natively `0 = white, 1 = black`
    /// (opposite of the renderer-friendly convention). True for most Good
    /// Display 7.5" V2 BW modules including the one in reTerminal E1001;
    /// some BWR variants flip this.
    pub panel_data_inverted: bool,
}

impl BoardConfig {
    /// Bridge the firmware-local [`ColorMode`] enum to the
    /// [`paperanywhere_ports::ColorMode`] the runtime + compositor
    /// speak. The two enums are structurally identical — this exists
    /// because the boards module hasn't been wired through proto yet
    /// (see the inline note above the enum definitions).
    pub fn default_color_mode_ports(&self) -> paperanywhere_ports::ColorMode {
        match self.default_color_mode {
            ColorMode::Mono1bpp => paperanywhere_ports::ColorMode::Mono1bpp,
            ColorMode::MonoRed1bpp => paperanywhere_ports::ColorMode::MonoRed1bpp,
            ColorMode::MonoYellow1bpp => paperanywhere_ports::ColorMode::MonoYellow1bpp,
            ColorMode::Gray4 => paperanywhere_ports::ColorMode::Gray4,
            ColorMode::Gray16 => paperanywhere_ports::ColorMode::Gray16,
            ColorMode::Color7 => paperanywhere_ports::ColorMode::Color7,
        }
    }
}

#[cfg(feature = "board-reterminal-e1001")]
mod reterminal_e1001;
#[cfg(feature = "board-reterminal-e1002")]
mod reterminal_e1002;
#[cfg(feature = "board-reterminal-e1003")]
mod reterminal_e1003;
#[cfg(feature = "board-reterminal-e1004")]
mod reterminal_e1004;
#[cfg(feature = "board-inkplate-6")]
mod inkplate_6;
#[cfg(feature = "board-inkplate-10")]
mod inkplate_10;
#[cfg(feature = "board-generic-esp32s3-waveshare-75")]
mod generic_esp32s3_waveshare_75;

/// Return the `BoardConfig` for whichever board feature is active.
pub fn current() -> BoardConfig {
    #[cfg(feature = "board-reterminal-e1001")]
    return reterminal_e1001::CONFIG;
    #[cfg(feature = "board-reterminal-e1002")]
    return reterminal_e1002::CONFIG;
    #[cfg(feature = "board-reterminal-e1003")]
    return reterminal_e1003::CONFIG;
    #[cfg(feature = "board-reterminal-e1004")]
    return reterminal_e1004::CONFIG;
    #[cfg(feature = "board-inkplate-6")]
    return inkplate_6::CONFIG;
    #[cfg(feature = "board-inkplate-10")]
    return inkplate_10::CONFIG;
    #[cfg(feature = "board-generic-esp32s3-waveshare-75")]
    return generic_esp32s3_waveshare_75::CONFIG;
}

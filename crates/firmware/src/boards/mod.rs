//! Board configuration trait + per-board modules.
//!
//! Each `BoardConfig` declares the panel driver, pin map, integrated peripherals,
//! and default power profile. The boot path is identical across boards — only
//! this struct varies. New products plug in here.
//!
//! Also exported: `Panel` (the concrete `EpaperPanel`-implementing type for
//! the active board) and `build_panel`, which assembles the driver from the
//! GPIO/SPI handles main.rs gathered into `FirmwareResources`.

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::Spi;
use paperanywhere_panel_uc8179::{Pins, Uc8179};

use crate::resources::PanelHardware;

/// Bare panel driver, before the compositor wraps it. Kept as a type
/// alias so changes to the panel-driver type signature don't require
/// chasing through both `Panel` (the public-facing type) and the
/// concrete `Uc8179<...>` everywhere.
pub type BarePanel = Uc8179<
    ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>,
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

/// Assemble the SPI device (bus + CS), instantiate the UC8179 driver
/// against the runtime-typed pins main.rs gathered, and wrap it in the
/// compositor so the runtime sees a layered framebuffer.
/// `invert_data_plane` is sourced from the board's `BoardConfig`.
pub fn build_panel(hw: PanelHardware, board: BoardConfig) -> Panel {
    let delay = Delay::new();
    let spi_device = ExclusiveDevice::new(hw.spi_bus, hw.cs, delay)
        .expect("ExclusiveDevice::new (Delay is infallible)");
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

/// Capabilities + pin map for a specific physical device. Constructed once at
/// boot by `current()` based on the active Cargo feature.
#[derive(Debug, Clone, Copy)]
pub struct BoardConfig {
    pub name: &'static str,
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

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

/// Concrete `EpaperPanel` type the rest of the firmware sees. All current
/// boards use UC8179; when a board needs a different controller (e.g.
/// UC8159 for Color7 panels, IT8951 for the 10.3" Gray16 panel) this becomes
/// an enum and `build_panel` switches on the active board feature.
pub type Panel = Uc8179<
    ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Delay,
>;

/// Assemble the SPI device (bus + CS) and instantiate the UC8179 driver
/// against the runtime-typed pins main.rs gathered for the active board.
/// `invert_data_plane` is sourced from the board's `BoardConfig` so each
/// product's panel polarity is declared with its other metadata.
pub fn build_panel(hw: PanelHardware, invert_data_plane: bool) -> Panel {
    let delay = Delay::new();
    let spi_device = ExclusiveDevice::new(hw.spi_bus, hw.cs, delay)
        .expect("ExclusiveDevice::new (Delay is infallible)");
    Uc8179::new(
        spi_device,
        Pins { rst: hw.rst, dc: hw.dc, busy: hw.busy },
        delay,
        invert_data_plane,
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

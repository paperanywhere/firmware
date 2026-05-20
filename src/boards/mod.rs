//! Board configuration trait + per-board modules.
//!
//! Each `BoardConfig` declares the panel driver, pin map, integrated peripherals,
//! and default power profile. The boot path is identical across boards — only
//! this struct varies. New products plug in here.

use paperanywhere_proto::{ColorMode, PackingKind, PowerPolicy};

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
    // Pin map (BUSY / RST / DC / CS / SCLK / MOSI). The exact GPIO numbers come
    // from the open-source schematic per board. Values below are *placeholders*
    // until the schematic is pulled in M4 kickoff.
    pub panel_busy: u8,
    pub panel_rst: u8,
    pub panel_dc: u8,
    pub panel_cs: u8,
    pub panel_sclk: u8,
    pub panel_mosi: u8,
    pub battery_adc: Option<u8>,
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

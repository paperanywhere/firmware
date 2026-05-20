//! Soldered Inkplate 6 — ESP32, 800×600 mono e-paper with Gray4 support.

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Soldered Inkplate 6",
    panel_model_id: 9,
    panel_width_px: 800,
    panel_height_px: 600,
    default_color_mode: ColorMode::Mono1bpp,
    default_packing: PackingKind::RowMajorBe2bpp,
    default_power_policy: PowerPolicy::ScheduledWake,
    default_sleep_interval_sec: 3600,
    has_battery: true,
    has_buttons: true,
    has_sensors: false,
    has_buzzer: false,
    has_sd_card: true,
    panel_busy: 0, panel_rst: 0, panel_dc: 0, panel_cs: 0, panel_sclk: 0, panel_mosi: 0,
    battery_adc: Some(35),
};

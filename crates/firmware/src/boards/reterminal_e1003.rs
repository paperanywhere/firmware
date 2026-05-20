//! Seeed Studio reTerminal E1003 — ESP32-S3, 10.3" 1404×1872 monochrome e-paper,
//! 16-level grayscale, ~6-month battery life.

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Seeed Studio reTerminal E1003",
    panel_model_id: 3,
    panel_width_px: 1404,
    panel_height_px: 1872,
    default_color_mode: ColorMode::Mono1bpp,
    default_packing: PackingKind::RowMajorMsbFirst1bpp,
    default_power_policy: PowerPolicy::ScheduledWake,
    default_sleep_interval_sec: 21_600,
    has_battery: true,
    has_buttons: true,
    has_sensors: true,
    has_buzzer: true,
    has_sd_card: true,
    panel_busy: 4,
    panel_rst: 5,
    panel_dc: 6,
    panel_cs: 7,
    panel_sclk: 8,
    panel_mosi: 9,
    battery_adc: Some(1),
    // Gray16 10.3" panel — uses IT8951, not UC8179. Flag inapplicable.
    panel_data_inverted: false,
};

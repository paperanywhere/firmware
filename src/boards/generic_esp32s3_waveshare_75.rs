//! Generic ESP32-S3 carrier driving a Waveshare 7.5" BW e-paper panel.
//! Reference build for users wiring their own hardware (matches `panel_model_id = 7`).

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Generic ESP32-S3 + Waveshare 7.5\" BW",
    panel_model_id: 7,
    panel_width_px: 800,
    panel_height_px: 480,
    default_color_mode: ColorMode::Mono1bpp,
    default_packing: PackingKind::RowMajorMsbFirst1bpp,
    default_power_policy: PowerPolicy::ScheduledWake,
    default_sleep_interval_sec: 21_600,
    has_battery: false,
    has_buttons: false,
    has_sensors: false,
    has_buzzer: false,
    has_sd_card: false,
    // Reference wiring per Waveshare's standard pinout for ESP32-S3 dev boards.
    panel_busy: 4,
    panel_rst: 16,
    panel_dc: 17,
    panel_cs: 5,
    panel_sclk: 18,
    panel_mosi: 23,
    battery_adc: None,
};

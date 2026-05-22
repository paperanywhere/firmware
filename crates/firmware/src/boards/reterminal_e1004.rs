//! Seeed Studio reTerminal E1004 — ESP32-S3, 13.3" full-color ACeP e-paper,
//! ~6-month battery life.

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Seeed Studio reTerminal E1004",
    panel_model_id: 4,
    panel_width_px: 1200,
    panel_height_px: 1600,
    default_color_mode: ColorMode::Color7,
    default_packing: PackingKind::AcepIndexed4bpp,
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
    // ACeP Color7 13.3" panel — UC8159, not UC8179. Flag inapplicable.
    sd: None,
    panel_data_inverted: false,
};

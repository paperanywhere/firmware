//! Seeed Studio reTerminal E1002 — ESP32-S3, 7.3" 800×480 full-color ACeP e-paper.

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Seeed Studio reTerminal E1002",
    panel_model_id: 2,
    panel_width_px: 800,
    panel_height_px: 480,
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
    // ACeP Color7 panel — uses a different controller (UC8159), so the
    // UC8179-specific polarity flag is moot for this board. Placeholder.
    panel_data_inverted: false,
};

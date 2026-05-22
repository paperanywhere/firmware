//! Seeed Studio reTerminal E1001 — ESP32-S3, 7.5" 800×480 monochrome e-paper,
//! 2000 mAh, ~3-month battery life at 6h refresh.
//!
//! Pin mapping placeholder — confirm against the open-source Seeed schematic before
//! a real build. (See <https://wiki.seeedstudio.com/getting_started_with_reterminal_e1001/>.)

use super::{ColorMode, PackingKind, PowerPolicy};

use super::BoardConfig;

pub const CONFIG: BoardConfig = BoardConfig {
    name: "Seeed Studio reTerminal E1001",
    manufacturer: "Seeed Studio",
    model: "reTerminal E1001",
    panel_model_id: 1,
    panel_width_px: 800,
    panel_height_px: 480,
    default_color_mode: ColorMode::Mono1bpp,
    default_packing: PackingKind::RowMajorMsbFirst1bpp,
    default_power_policy: PowerPolicy::ScheduledWake,
    default_sleep_interval_sec: 21_600, // 6 hours — matches Seeed's published spec
    has_battery: true,
    has_buttons: true,
    has_sensors: true,
    has_buzzer: true,
    has_sd_card: true,
    // TODO(M4): pull real GPIO numbers from the schematic. Placeholders below.
    panel_busy: 4,
    panel_rst: 5,
    panel_dc: 6,
    panel_cs: 7,
    panel_sclk: 8,
    panel_mosi: 9,
    battery_adc: Some(1),
    // Pin map confirmed against the V1.0 schematic
    // (202004307_reTerminal_E1001_V1.0_SCH_250805.pdf). The SD slot
    // sits on SPI2 alongside the panel; SCK + MOSI are shared, and
    // CS / DET / EN / MISO are SD-only.
    sd: Some(super::SdPinMap {
        miso: 8,
        cs: 14,
        detect: 15,
        power_enable: 16,
    }),
    // Good Display GDEW075T7 (the panel module Seeed integrates here)
    // wants `0 = white, 1 = black` on its DTM2 plane. Verified by flashing
    // the boot screen and observing inverted output without this flag.
    panel_data_inverted: true,
};

//! `FirmwareResources` — the single bundle of platform handles main.rs hands
//! to `boot::run`. As the firmware adds peripherals (SPI for the panel,
//! ADC for battery sensing, CPU_CTRL for the second-core embassy executor,
//! etc.) they land here rather than as new positional parameters threaded
//! through every layer.
//!
//! The struct deliberately owns everything by value with `'static` lifetimes
//! — these are one-shot peripheral handles consumed by their subsystems and
//! never returned. The board config travels alongside them because every
//! subsystem needs at least one field from it (pin numbers, capability
//! flags, default policy).

use esp_hal::Async;
use esp_hal::gpio::{Input, Output};
use esp_hal::interrupt::software::SoftwareInterrupt;
use esp_hal::peripherals::{ADC1, CPU_CTRL, FLASH, GPIO1, GPIO21, LPWR, RNG, TIMG0, WIFI};
use esp_hal::spi::master::Spi;

use crate::boards::BoardConfig;

/// Hardware bundle for the battery gauge. Construction is board-
/// specific (different boards drive different GPIOs / use different
/// fuel-gauge chips), so this carries only the peripheral handles —
/// the per-board module turns them into a real `BatteryGauge`.
pub struct BatteryHardware {
    /// SAR-ADC1 peripheral. On the reTerminal E1001 family the
    /// battery-sense divider is wired to a GPIO that lives on ADC1.
    pub adc1: ADC1<'static>,
    /// GPIO1: voltage-divider output (battery_voltage / 2 in steady
    /// state). Source the schematic net name from Seeed's ESPHome
    /// board YAML — confirmed `gpio1` with `multiply: 2.0`.
    pub batt_sense: GPIO1<'static>,
    /// GPIO21: drives the high-side enable for the divider so we
    /// only burn the divider current during a sample. Seeed's
    /// `bsp_battery_enable` net. Active-high.
    pub batt_enable: GPIO21<'static>,
}

/// Peripheral handles + GPIO line drivers the panel needs. `main.rs`
/// constructs this per-board (cfg-gated pin selection); from here on
/// downstream code is board-agnostic — the SPI bus + four GPIO lines are
/// the only shape the UC8179 driver cares about.
///
/// Boards that don't use UC8179 (e.g. ACeP color panels with UC8159, IT8951
/// gray panels) would expand this into an enum or pick a different bundle
/// type per-board. For now there's only one bundle shape.
/// Panel-side pins + the once-only SPI bus. main.rs constructs the
/// bus, stores it in the shared `'static` mutex (see
/// `boards::SharedSpiBus`), and hands the pins + a `&'static` ref
/// to the bus down to `boards::build_panel`. SD reuses the same
/// `&'static` ref via its own CS-aware device wrapper.
pub struct PanelHardware {
    pub spi_bus: Spi<'static, Async>,
    pub cs: Output<'static>,
    pub dc: Output<'static>,
    pub rst: Output<'static>,
    pub busy: Input<'static>,
}

pub struct FirmwareResources {
    pub board: BoardConfig,

    // ── WiFi / radio ──
    /// Timer group that drives `esp-rtos`'s scheduler tick.
    pub timg0: TIMG0<'static>,
    /// Software interrupt esp-rtos uses for its context-switch trampoline.
    pub sw_int0: SoftwareInterrupt<'static, 0>,
    pub rng: RNG<'static>,
    pub wifi: WIFI<'static>,

    // ── Power ──
    pub lpwr: LPWR<'static>,

    // ── Persistent storage ──
    pub flash: FLASH<'static>,

    // ── Panel transport (board-specific pin map, built in main.rs) ──
    pub panel: PanelHardware,

    // ── Battery readout (board-specific; None on USB-only carriers) ──
    pub battery: Option<BatteryHardware>,

    // ── SD card pins (board-specific; None on cards without a slot) ──
    //
    // SCK + MOSI are NOT listed here — those are part of the panel's
    // SPI2 bus, which boot.rs parks in a shared `'static` mutex so
    // the SD's `CriticalSectionDevice` can draw against the same bus.
    pub sd: Option<crate::sd::SdHardware>,

    // ── Second-core embassy executor (reserved) ──
    //
    // Captured but not yet consumed. The plan was to spawn the embassy-net
    // runner on core 1; that's blocked by `embassy_net::Runner: !Send`, so
    // we'll re-target these for a panel-refresh worker (or whatever post-
    // async-refactor concurrency we want) instead. Keeping the handles
    // claimed here means main.rs doesn't have to remember to re-extract
    // them once we have a consumer.
    /// The second core's control peripheral.
    #[allow(dead_code)]
    pub cpu_ctrl: CPU_CTRL<'static>,
    /// Software interrupt 1 — mirrors `sw_int0` on core 0.
    #[allow(dead_code)]
    pub sw_int1: SoftwareInterrupt<'static, 1>,
    // Future: SPI2 / DMA channels for panel, ADC1 for battery, etc.
}

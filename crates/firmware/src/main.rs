//! paperanywhere-firmware — ESP32-S3 entrypoint.
//!
//! Architecture: **HTTPS polling** (no WebSockets). On every wake the device:
//!   1. Associates with WiFi using credentials from NVS
//!   2. GETs `/api/device/state` to fetch the next thing to render + sleep duration
//!   3. If a new image is offered, GETs `/api/device/blob/:id` and streams it to the panel
//!   4. POSTs `/api/device/ack` to confirm
//!   5. Deep-sleeps until `next_check_at`
//!
//! This file owns the peripheral handoff. `boot::run` orchestrates the rest.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_println::println;

// Emits the ESP-IDF app descriptor that the second-stage bootloader checks
// before handing control to our entry point. Without this espflash + the
// bootloader refuse to load the image. The macro is a one-liner at crate
// scope.
esp_bootloader_esp_idf::esp_app_desc!();

mod boards;
mod boot;
mod http;
mod network;
mod nvs;
mod ota;
mod panel;
mod power;
mod provisioning;
mod resources;
mod sd_config;
mod wifi;

/// Firmware version stamp produced by `build.rs` — e.g. `0.1.0+a1b2c3d4`.
/// Reported to the backend in /claim and heartbeats so the server can
/// decide when to offer an OTA update.
pub const FW_VERSION: &str = env!("PAPERANYWHERE_FW_VERSION");

/// Wall-clock build time stamp, also from `build.rs`. Format:
/// `2026-05-20 22:13 UTC`. Surfaced on the boot screen so a deployed
/// device can be cross-checked against the release that built it.
pub const BUILD_TIME: &str = env!("PAPERANYWHERE_BUILD_TIME");

/// `BuildInfo` the compositor uses to draw the boot-screen overlay.
/// Cheap to construct from the two `&'static str` consts above.
pub const BUILD_INFO: paperanywhere_compositor::BuildInfo =
    paperanywhere_compositor::BuildInfo { fw_version: FW_VERSION, build_time: BUILD_TIME };

#[cfg(not(any(
    feature = "board-reterminal-e1001",
    feature = "board-reterminal-e1002",
    feature = "board-reterminal-e1003",
    feature = "board-reterminal-e1004",
    feature = "board-inkplate-6",
    feature = "board-inkplate-10",
    feature = "board-generic-esp32s3-waveshare-75",
)))]
compile_error!("paperanywhere-firmware requires exactly one `board-*` feature to be enabled");

const HEAP_SIZE: usize = 96 * 1024;

fn init_heap() {
    esp_alloc::heap_allocator!(size: HEAP_SIZE);
}

#[esp_hal::main]
fn main() -> ! {
    init_heap();
    // Register esp-println as the `log` crate backend so the runtime's
    // `info!`/`warn!`/`error!` calls reach the UART. Without this all
    // `log::*` macros are no-ops and wake-cycle errors disappear silently.
    // We pin at `Info` rather than `init_logger_from_env` because the
    // env-var variant is read at *compile* time and an unset `ESP_LOG`
    // defaults to silent.
    esp_println::logger::init_logger(log::LevelFilter::Info);
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(CpuClock::max()),
    );

    let board = boards::current();
    println!("paperanywhere-firmware booting on {}", board.name);

    // One bundle, threaded through to `boot::run`. As new subsystems claim
    // peripherals (panel SPI, battery ADC, second-core executor for embassy-
    // net) they grow as additional fields here rather than additional
    // positional arguments down the call chain.
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    // Board-specific GPIO/SPI bundle for the e-paper panel. Each cfg branch
    // consumes the peripheral fields it needs; remaining fields flow into
    // `FirmwareResources` below. This is the only place pin numbers
    // actually appear in the firmware — moving a panel to different GPIOs
    // is a one-block edit per board.
    //
    // Pin map for reTerminal E1001 cross-checked against Zephyr's device
    // tree (`boards/seeed/reterminal_e1001/reterminal_e1001_procpu.dts`):
    //   SCK=7, MOSI=9, CS=10, DC=11 (active-high), RST=12 (active-low),
    //   BUSY=13 (high=idle, low=busy).
    // Max bus rate per the same DT is 4 MHz.
    // (Earlier guesses landed CS on GPIO45 — which is the *buzzer* — turning
    // SPI traffic into audible chirps. Useful diagnostic, embarrassing bug.)
    #[cfg(feature = "board-reterminal-e1001")]
    let panel = {
        let bus = Spi::new(
            peripherals.SPI2,
            SpiConfig::default()
                .with_frequency(Rate::from_mhz(4))
                .with_mode(Mode::_0),
        )
        .expect("SPI2 config")
        .with_sck(peripherals.GPIO7)
        .with_mosi(peripherals.GPIO9);
        resources::PanelHardware {
            spi_bus: bus,
            cs: Output::new(peripherals.GPIO10, Level::High, OutputConfig::default()),
            dc: Output::new(peripherals.GPIO11, Level::Low, OutputConfig::default()),
            // RST is active-low: idle state is high, drop low to assert
            // reset. UC8179 driver pulses this internally during `boot()`.
            rst: Output::new(peripherals.GPIO12, Level::High, OutputConfig::default()),
            busy: Input::new(
                peripherals.GPIO13,
                InputConfig::default().with_pull(Pull::Up),
            ),
        }
    };
    #[cfg(not(feature = "board-reterminal-e1001"))]
    compile_error!(
        "no panel-hardware wiring for the active board feature yet — add a cfg block above"
    );

    let resources = resources::FirmwareResources {
        board,
        timg0: peripherals.TIMG0,
        sw_int0: sw_ints.software_interrupt0,
        rng: peripherals.RNG,
        wifi: peripherals.WIFI,
        lpwr: peripherals.LPWR,
        flash: peripherals.FLASH,
        cpu_ctrl: peripherals.CPU_CTRL,
        sw_int1: sw_ints.software_interrupt1,
        panel,
    };

    boot::run(resources)
}

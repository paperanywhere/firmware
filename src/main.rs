//! paperanywhere-firmware — ESP32-S3 entrypoint.
//!
//! Boards are selected via mutually-exclusive Cargo features. Each board module
//! provides a `BoardConfig` struct with the panel driver choice, pin map, and
//! capability flags. The boot path is identical across boards.
//!
//! NOTE: the firmware is in a transitional state — the M4 modules (wifi, ws_client,
//! https, panel drivers, nvs, etc.) are not compiled in right now because they
//! depend on either paperanywhere-proto (temporarily dropped — see Cargo.toml)
//! or esp-hal-embassy / esp-wifi crates that don't yet have a release matching
//! esp-hal 1.1 on crates.io. They live on disk and will come back module by
//! module as their deps catch up. Today the firmware just proves the
//! toolchain + board catalog compile.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_println::println;

mod boards;

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

const HEAP_SIZE: usize = 64 * 1024;

fn init_heap() {
    esp_alloc::heap_allocator!(size: HEAP_SIZE);
}

#[esp_hal::main]
fn main() -> ! {
    init_heap();
    let _peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(CpuClock::max()),
    );

    let board = boards::current();
    println!("paperanywhere-firmware booting on {}", board.name);
    println!(
        "panel: {}x{} model_id={} (M4 will wire WiFi + WS + panel driver)",
        board.panel_width_px, board.panel_height_px, board.panel_model_id,
    );

    loop {
        core::hint::spin_loop();
    }
}

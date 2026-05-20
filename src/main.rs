//! paperanywhere-firmware — ESP32-S3 entrypoint.
//!
//! Architecture: **HTTPS polling** (no WebSockets). On every wake the device:
//!   1. Associates with WiFi using credentials from NVS
//!   2. GETs `/api/device/state` to fetch the next thing to render + sleep duration
//!   3. If a new image is offered, GETs `/api/device/blob/:id` and streams it to the panel
//!   4. POSTs `/api/device/ack` to confirm
//!   5. Deep-sleeps until `next_check_at`
//!
//! This file owns the boot orchestration. Each step is its own module under `src/`.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_println::println;

mod boards;
mod boot;
mod http;
mod nvs;
mod panel;
mod power;
mod provisioning;
mod sd_config;
mod wifi;
mod wire;

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

    boot::run(board)
}

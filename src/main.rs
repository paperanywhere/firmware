//! paperanywhere-firmware — ESP32-S3 entrypoint.
//!
//! Boards are selected via mutually-exclusive Cargo features. Each board module
//! provides a `BoardConfig` struct with the panel driver choice, pin map, and
//! capability flags. The boot path is identical across boards.

#![no_std]
#![no_main]

extern crate alloc;

use core::mem::MaybeUninit;

use esp_backtrace as _;
use esp_hal::{
    clock::ClockControl,
    peripherals::Peripherals,
    prelude::*,
    system::SystemControl,
    timer::timg::TimerGroup,
};
use esp_println::println;

mod boards;
mod boot;
mod https;
mod nvs;
mod panel;
mod peripherals;
mod power;
mod provisioning;
mod sd_config;
mod wifi;
mod ws_client;

/// Compile-time sanity check: exactly one board feature must be enabled.
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

/// Initialize a small heap so `alloc`-using crates work in no_std. The heap is
/// claim-flow scratch + serde decode buffers + framebuffer streaming — not for
/// long-lived allocations.
#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

const HEAP_SIZE: usize = 64 * 1024;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

fn init_heap() {
    unsafe {
        ALLOCATOR.init(HEAP_MEM.as_mut_ptr() as *mut u8, HEAP_SIZE);
    }
}

#[entry]
fn main() -> ! {
    init_heap();
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::max(system.clock_control).freeze();
    let _timg0 = TimerGroup::new(peripherals.TIMG0, &clocks);

    let board = boards::current();
    println!("paperanywhere-firmware booting on {}", board.name);

    // boot.rs decides: if device_token in NVS → main_loop; else → claim flow.
    boot::run(board);
}

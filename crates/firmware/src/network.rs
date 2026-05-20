//! IP stack glue. Wraps esp-radio's WiFi station `Interface` in an
//! `embassy_net::Stack` and exposes the embassy task that polls the runner.
//!
//! Both `net_task` and the polling runtime task run on the same embassy
//! executor (core 0) — embassy-net's `Stack` is `!Send` and shouldn't move
//! between cores, so cohabiting one executor is the correct architecture.
//!
//! No TLS yet — set `backend_url` to `http://...` in the device config.
//! Wiring `embedded-tls` is a follow-up that keeps the trait surface intact.

use embassy_net::{Config, Runner, Stack, StackResources};
use esp_radio::wifi::Interface as RadioInterface;
use static_cell::StaticCell;

/// How many simultaneously-open sockets the stack supports. DHCP + DNS each
/// claim one of these, so the budget for user TCP connections is `N - 2`.
/// 4 leaves us 2 TCP slots — enough for one /state + one /blob streaming
/// at a time without contention.
pub const SOCKETS: usize = 4;

static STACK_RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

/// Build the network stack. Returns the `Stack` for sharing with the HTTP
/// client and the `Runner` for the embassy task that drives it.
pub fn build(
    interface: RadioInterface<'static>,
) -> (Stack<'static>, Runner<'static, RadioInterface<'static>>) {
    let config = Config::dhcpv4(Default::default());
    // 64-bit seed for embassy-net's TCP ISN. A real implementation would
    // read this from `esp_hal::rng::Rng` so each boot gets a fresh seed;
    // for now a fixed value is fine — backends don't care about ISN
    // uniqueness across boots, only across active connections.
    let seed = 0xA1B2_C3D4_E5F6_0708u64;
    embassy_net::new(
        interface,
        config,
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    )
}

/// Embassy task that drives the network stack. Spawned on the same executor
/// as the polling runtime — embassy-net's Stack is `!Send`, so the two must
/// share an executor.
#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, RadioInterface<'static>>) -> ! {
    runner.run().await
}

//! On-device diagnostics surface (task #102).
//!
//! Periodic heartbeat printer that the user can capture from the
//! serial monitor when something's gone weird. Two outputs:
//!
//! 1. **Heap stats** — `esp_alloc::HEAP.free()` / `.used()`.
//!    Catches the slow-leak pattern: if heap is monotonically
//!    shrinking across wakes the next "memory allocation of N
//!    bytes failed" panic isn't a surprise anymore.
//!
//! 2. **Chrome snapshot** — the network + identity state the runtime
//!    is operating against. Includes WiFi link state, RSSI, IP /
//!    gateway, device status, claim code presence. If a wake is
//!    stalled this tells us what the system *thinks* its own state
//!    is at that moment.
//!
//! Frequency: 5 seconds. Cheap (one chrome lock + one heap query +
//! a single printf), runs on core 0's executor alongside net_task
//! and the HTTP/WiFi proxies so it never blocks on application
//! work. If core 0 itself goes quiet, the heartbeat stops appearing
//! — that's its own diagnostic signal.

use embassy_time::{Duration, Timer};
use paperanywhere_ports::chrome;

#[embassy_executor::task]
pub async fn heartbeat_task() -> ! {
    log::info!("diag: heartbeat task starting on core 0");
    let mut tick: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(5)).await;
        tick = tick.wrapping_add(1);
        let s = chrome::snapshot();
        let heap_free = esp_alloc::HEAP.free();
        let heap_used = esp_alloc::HEAP.used();
        // Net-runner poll counter: if this stops advancing during a
        // /state hang, net_task is genuinely stuck on core 0. If it
        // keeps climbing, the hang is in the TCP state machine or
        // the SocketDevice driver and net_task itself is fine.
        let net_polls = embassy_net::diag::NET_RUNNER_POLLS
            .load(core::sync::atomic::Ordering::Relaxed);
        log::info!(
            "diag #{}: heap free={} used={} | net_polls={} | wifi={:?} rssi={:?} ip={:?} gw={:?} | batt {}mv ({}%) | status={:?} uuid={} code={}",
            tick,
            heap_free,
            heap_used,
            net_polls,
            s.wifi_link_state,
            s.rssi_dbm,
            s.ip.as_deref().unwrap_or("--"),
            s.gateway_v4.as_deref().unwrap_or("--"),
            s.battery_mv.map(|m| m as i32).unwrap_or(-1),
            s.battery_percent.map(|p| p as i32).unwrap_or(-1),
            s.device_status,
            // Trim the UUID to its tail 12 chars so the line stays
            // readable on a terminal — the full UUID is dumped on
            // register; the short tail is just for cross-reference.
            s.device_uuid
                .as_deref()
                .map(|u| if u.len() > 12 { &u[u.len() - 12..] } else { u })
                .unwrap_or("--"),
            // claim_code is the most boot-stage-sensitive marker —
            // "(none)" while we're still calling register, becomes
            // the real code on success, and going from real → none
            // would indicate a corrupted NVS read.
            // We don't have it in chrome (NVS-only), so render a
            // present/absent marker derived from device_uuid's
            // presence as a proxy for "register succeeded".
            if s.device_uuid.is_some() { "registered" } else { "(not yet)" }
        );
    }
}

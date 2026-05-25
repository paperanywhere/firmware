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

fn dump_rx_slots() {
    let slots = embassy_net::diag::snapshot_rx();
    for (i, s) in slots.iter().enumerate() {
        if s.head_len == 0 {
            continue;
        }
        let head_len = s.head_len as usize;
        // Hex-dump the captured header bytes. 14 bytes Ethernet then
        // EtherType — we just print the whole 54 raw and let the
        // reader decode by eye for now (ARP is 42 B, IPv4+ICMP is
        // 14+20+8=42, IPv4+TCP min is 54). Format: "rx[idx] len=N
        // total=M aabbcc...".
        let mut hex: heapless::String<128> = heapless::String::new();
        for b in s.head[..head_len].iter() {
            let _ = core::fmt::Write::write_fmt(
                &mut hex,
                format_args!("{:02x}", b),
            );
        }
        log::info!(
            "rx[{}] len={} total={} {}",
            i, head_len, s.total_len, hex.as_str()
        );
    }
}

#[embassy_executor::task]
pub async fn heartbeat_task() -> ! {
    log::info!("diag: heartbeat task starting on core 0");
    let mut tick: u32 = 0;
    let mut last_rx_dumped: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(5)).await;
        tick = tick.wrapping_add(1);
        // Feed the per-core heartbeat counter the hardware watchdog
        // gates on. As long as this task gets scheduled, core 0 is
        // alive enough to keep the chip from resetting.
        crate::watchdog::touch_core0();
        let s = chrome::snapshot();
        let heap_free = esp_alloc::HEAP.free();
        let heap_used = esp_alloc::HEAP.used();
        // Net-runner poll counter: if this stops advancing during a
        // /state hang, net_task is genuinely stuck on core 0. If it
        // keeps climbing, the hang is in the TCP state machine or
        // the SocketDevice driver and net_task itself is fine.
        let net_polls = embassy_net::diag::NET_RUNNER_POLLS
            .load(core::sync::atomic::Ordering::Relaxed);
        // Radio-edge counters: what's actually crossing the
        // smoltcp ↔ esp-radio driver boundary. rx_pkts that
        // doesn't grow while the AP is forwarding broadcasts /
        // unicast to us means the radio isn't delivering frames
        // upward (filter, power-save, BSSID mismatch, etc.).
        // rx_none growing proportional to net_polls is the idle
        // "polled, nothing there" baseline.
        let rx_pkts = embassy_net::diag::NET_RX_PKTS
            .load(core::sync::atomic::Ordering::Relaxed);
        let rx_none = embassy_net::diag::NET_RX_NONE
            .load(core::sync::atomic::Ordering::Relaxed);
        let tx_pkts = embassy_net::diag::NET_TX_PKTS
            .load(core::sync::atomic::Ordering::Relaxed);
        let tx_none = embassy_net::diag::NET_TX_NONE
            .load(core::sync::atomic::Ordering::Relaxed);
        let garps = embassy_net::diag::GRATUITOUS_ARPS_SENT
            .load(core::sync::atomic::Ordering::Relaxed);
        log::info!(
            "diag #{}: heap free={} used={} | net_polls={} rx_pkts={} rx_none={} tx_pkts={} tx_none={} garps={} | wifi={:?} rssi={:?} ip={:?} gw={:?} | batt {}mv ({}%) | status={:?} uuid={} code={}",
            tick,
            heap_free,
            heap_used,
            net_polls,
            rx_pkts,
            rx_none,
            tx_pkts,
            tx_none,
            garps,
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

        // Header dump fires once, on the first heartbeat where any
        // RX has happened. Confirms what the very first frames we
        // received were (DHCP / gateway ARP reply / etc.) — useful
        // when bringing up a new network or chasing an early-boot
        // RX issue. Once dumped, stays quiet for the rest of the
        // session.
        if last_rx_dumped == 0 && rx_pkts > 0 {
            dump_rx_slots();
            last_rx_dumped = rx_pkts;
        }
    }
}

/// Core-1 liveness heartbeat. Independent of the core-0 task: a hang
/// in one core's executor is still observable via the OTHER core's
/// counter advancing without the wedged one's. The hardware watchdog
/// (see `crate::watchdog`) only feeds when both counters advance, so
/// a stall on either core triggers a chip-level reset.
///
/// Prints heap once a minute so the trace stays readable. The actual
/// liveness signal is the `touch_core1()` call.
#[embassy_executor::task]
pub async fn core1_heartbeat_task() -> ! {
    log::info!("diag: heartbeat task starting on core 1");
    let mut tick: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(5)).await;
        tick = tick.wrapping_add(1);
        crate::watchdog::touch_core1();
        // Stamp a line every 60 s (every 12th tick) so the serial
        // trace shows core 1 is alive without dominating the log.
        if tick % 12 == 0 {
            log::info!("diag c1 #{}: core1 alive", tick);
        }
    }
}

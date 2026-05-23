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
/// claim one of these (verified via embassy-net 0.7.1 source — `embassy_net::new`
/// adds a DNS socket via `i.sockets.add(...)` when the `dns` feature is on
/// + a DHCPv4 socket when `Config::dhcpv4` is used). So the budget for
/// user TCP connections is `N - 2`. With the runtime's HTTP client
/// + image-blob streaming + a future OTA download possibly overlapping,
/// 8 leaves comfortable headroom (smoltcp's `SocketSet::add` panics on
/// full — not what we want as a debug-friendly failure mode).
pub const SOCKETS: usize = 8;

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

/// Periodic gratuitous-ARP announcer. Re-broadcasts our MAC↔IP
/// binding every 20 s so APs that auto-age bridge-table entries
/// (UniFi observed) don't forget where to forward unicast addressed
/// to us. Wired hosts and the gateway latch onto the announce too.
///
/// Smoltcp does a single announce on DHCP bound; this keeps that
/// behavior fresh. Cost: one broadcast Ethernet frame every 20 s
/// (~60 B). Effectively free.
#[embassy_executor::task]
pub async fn garp_refresh_task(stack: &'static embassy_net::Stack<'static>) -> ! {
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(20)).await;
        stack.announce_self_v4();
    }
}

/// Periodic UNICAST keepalive to the gateway. Some APs (UniFi
/// observed) only refresh their per-STA bridge entry on unicast
/// traffic *from* the client — a broadcast gratuitous ARP doesn't
/// count, so a device that's only ever doing gARP every 20 s
/// eventually drops out of the AP's forwarding table and inbound
/// unicast (ping replies, HTTP SYN-ACKs, anything the runtime is
/// waiting for) stops being delivered. Once the device transmits a
/// unicast frame, the AP "rediscovers" us and delivery resumes for
/// some window — pings start succeeding for 30-60 s and then fail
/// again. That intermittent-pings pattern is the symptom.
///
/// The fix is a periodic short-lived TCP connect to the gateway.
/// The TCP target (gateway:80) is irrelevant — what matters is that
/// we emit a unicast IPv4 frame addressed to the gateway's MAC,
/// which the AP sees on the radio and uses to refresh its STA
/// table. The connect almost always RST's (gateway typically
/// doesn't run HTTP) but that's fine; the frame went out either
/// way. Smoltcp's neighbor cache also benefits — the gateway entry
/// gets refreshed on every iteration.
///
/// Period: 10 s. Short enough to beat any reasonable AP aging
/// timer (UniFi defaults are 30-60 s but some configs are tighter),
/// long enough that the cost is negligible (one SYN + one RST
/// per iteration = ~120 B of radio time every 10 s).
///
/// Bail out (do nothing this tick) when no IPv4 gateway is
/// configured — covers the brief window between association and
/// DHCP completion. Once `config_v4()` returns Some, the keepalive
/// starts running.
#[embassy_executor::task]
pub async fn gateway_keepalive_task(stack: &'static embassy_net::Stack<'static>) -> ! {
    use embassy_net::tcp::TcpSocket;
    use core::sync::atomic::{AtomicU32, Ordering};
    // 5 s — tighter than UniFi's typical 30-60 s STA aging window
    // and tighter than the gARP cadence so we reliably keep the
    // AP's per-STA bridge entry warm. Cost: one SYN + one RST per
    // tick (~120 B of radio time every 5 s).
    const KEEPALIVE_INTERVAL: embassy_time::Duration =
        embassy_time::Duration::from_secs(5);
    const PROBE_TIMEOUT: embassy_time::Duration =
        embassy_time::Duration::from_secs(2);
    // Recovery threshold: after this many consecutive TIMEOUTs we
    // assume the L2 path is wedged (AP forgot us, esp-radio's
    // association state is out of sync with the AP, etc.) and
    // trigger an explicit disconnect → re-associate. 6 × 5 s = 30 s
    // of dead path before we intervene — long enough to ride out a
    // transient blip, short enough that the user only sees one
    // wake cycle worth of staleness on the display.
    const RECOVERY_THRESHOLD: u32 = 6;
    // Hold-off after a forced reconnect attempt so we don't fight
    // with the runtime's own associate path (it may be in the
    // middle of its own re-associate when we fire) and don't
    // hammer the radio if the AP is actually down for a while.
    const RECOVERY_COOLDOWN: embassy_time::Duration =
        embassy_time::Duration::from_secs(60);
    static KEEPALIVE_COUNT: AtomicU32 = AtomicU32::new(0);
    let mut consecutive_timeouts: u32 = 0;
    // Initial delay so we don't race the gateway-ARP warmup in
    // wifi.rs::wait_for_ip_or_fallback.
    embassy_time::Timer::after(KEEPALIVE_INTERVAL).await;
    let mut rx_buf = [0u8; 128];
    let mut tx_buf = [0u8; 128];
    loop {
        if let Some(cfg) = stack.config_v4() {
            if let Some(gw) = cfg.gateway {
                let mut sock = TcpSocket::new(*stack, &mut rx_buf, &mut tx_buf);
                sock.set_timeout(Some(PROBE_TIMEOUT));
                let connect_fut = sock.connect((
                    embassy_net::IpAddress::Ipv4(gw),
                    80u16,
                ));
                let outcome = embassy_time::with_timeout(PROBE_TIMEOUT, connect_fut).await;
                sock.close();
                let n = KEEPALIVE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                // Log every probe at INFO so the trace shows
                // exactly when the AP path is healthy and when it
                // wedges. Recovery decisions below feed off the
                // same outcome.
                match outcome {
                    Ok(Ok(())) => {
                        log::info!("keepalive #{}: gateway:80 OK", n);
                        consecutive_timeouts = 0;
                    }
                    Ok(Err(e)) => {
                        log::info!("keepalive #{}: gateway:80 err {:?} (reply received — AP forwarding)", n, e);
                        consecutive_timeouts = 0;
                    }
                    Err(_) => {
                        consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                        log::warn!(
                            "keepalive #{}: gateway:80 TIMEOUT ({} consecutive) — inbound unicast may be blocked",
                            n,
                            consecutive_timeouts,
                        );
                        if consecutive_timeouts >= RECOVERY_THRESHOLD {
                            log::warn!(
                                "keepalive: {} consecutive TIMEOUTs ≥ threshold {} — forcing wifi reassociate",
                                consecutive_timeouts,
                                RECOVERY_THRESHOLD,
                            );
                            // Reset the counter BEFORE the reconnect
                            // attempt so a long-running disconnect
                            // doesn't accumulate more "timeouts"
                            // against us. If the reconnect itself
                            // fails, the next keepalive iteration
                            // restarts the threshold count.
                            consecutive_timeouts = 0;
                            match crate::wifi::force_reconnect_via_handoff().await {
                                Ok(()) => log::warn!("keepalive: force reassociate OK — cooling down 60 s"),
                                Err(e) => log::error!(
                                    "keepalive: force reassociate FAILED: {:?} — cooling down 60 s anyway",
                                    e,
                                ),
                            }
                            // Also raise the watchdog flag so the
                            // next runtime wake double-checks the
                            // association state (defence in depth
                            // against partial reconnect success).
                            FORCE_REASSOCIATE.store(true, core::sync::atomic::Ordering::Release);
                            embassy_time::Timer::after(RECOVERY_COOLDOWN).await;
                            continue;
                        }
                    }
                }
            }
        }
        embassy_time::Timer::after(KEEPALIVE_INTERVAL).await;
    }
}

/// Set when the connectivity watchdog observes a silent failure (link
/// reports connected but no packets have been delivered for the
/// silence window). The runtime polls this at the top of each wake
/// cycle and, when set, forces a wifi re-associate before continuing.
/// Cleared by the runtime after the recovery action is initiated.
pub static FORCE_REASSOCIATE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Connectivity watchdog. Observes RX packet flow + DHCP lease state
/// and flags the runtime when the radio looks alive at the L2 layer
/// but L3 has gone dark. Three failure modes this catches:
///
/// 1. **AP bridge-table blackhole** — we're still associated, gateway
///    ARPs us occasionally, but unicast frames addressed to us are
///    silently dropped. Symptom: RX counter stops climbing while link
///    stays up.
/// 2. **Driver queue stall** — esp-radio's TX queue is full and the
///    drain thread is starved. Symptom: same as above — radio thinks
///    it's fine, embassy-net sees nothing land.
/// 3. **DHCP lease expired without renewal** — smoltcp's DHCP socket
///    didn't get polled enough to refresh. Symptom: `config_v4()`
///    returns None even though the radio is still associated.
///
/// All three were independently observed during the multi-week
/// hardening pass that landed gratuitous ARP (#125), static ARP
/// preflight (#123), and the 2 ms net_task ticker (#104). The
/// watchdog is the catch-all that fires regardless of cause and
/// kicks the runtime into a recovery cycle.
///
/// Tuning: 30 s silence window is short enough that the user notices
/// at most one missed paint cycle, long enough that brief radio-
/// silent periods (e.g. while we're between /state polls) don't
/// trip it. RX counter is compared against the value snapshotted at
/// the start of each window — a single packet during the window is
/// enough to reset.
#[embassy_executor::task]
pub async fn connectivity_watchdog_task(stack: &'static embassy_net::Stack<'static>) -> ! {
    use core::sync::atomic::Ordering;
    const CHECK_INTERVAL: embassy_time::Duration = embassy_time::Duration::from_secs(5);
    const SILENCE_WINDOW_SECS: u32 = 30;
    const CHECKS_PER_WINDOW: u32 = SILENCE_WINDOW_SECS / 5;

    let mut last_rx = embassy_net::diag::NET_RX_PKTS.load(Ordering::Relaxed);
    let mut consecutive_silent_checks: u32 = 0;
    // Don't trip during the first window after boot — DHCP hasn't
    // finished and there's legitimately no traffic yet.
    embassy_time::Timer::after(embassy_time::Duration::from_secs(SILENCE_WINDOW_SECS as u64))
        .await;
    last_rx = embassy_net::diag::NET_RX_PKTS.load(Ordering::Relaxed);

    loop {
        embassy_time::Timer::after(CHECK_INTERVAL).await;

        let now_rx = embassy_net::diag::NET_RX_PKTS.load(Ordering::Relaxed);
        let link_up = stack.is_link_up();
        let has_cfg = stack.config_v4().is_some();

        if now_rx == last_rx && link_up && has_cfg {
            consecutive_silent_checks = consecutive_silent_checks.saturating_add(1);
            if consecutive_silent_checks >= CHECKS_PER_WINDOW {
                log::warn!(
                    "watchdog: {}s RX-silent with link_up={} cfg_v4=Some — flagging \
                     re-associate (rx={} polls={})",
                    SILENCE_WINDOW_SECS,
                    link_up,
                    now_rx,
                    embassy_net::diag::NET_RUNNER_POLLS.load(Ordering::Relaxed),
                );
                FORCE_REASSOCIATE.store(true, Ordering::Release);
                // Snooze a full window before re-arming so the runtime
                // has time to act before we fire again.
                consecutive_silent_checks = 0;
                last_rx = now_rx;
                embassy_time::Timer::after(embassy_time::Duration::from_secs(
                    SILENCE_WINDOW_SECS as u64,
                ))
                .await;
                last_rx = embassy_net::diag::NET_RX_PKTS.load(Ordering::Relaxed);
                continue;
            }
        } else if !link_up || !has_cfg {
            // Link itself has dropped — wake cycle's own associate
            // path handles this. Reset the silence counter so we
            // don't double-fire after re-association.
            consecutive_silent_checks = 0;
            last_rx = now_rx;
        } else {
            // Packets are flowing; reset.
            consecutive_silent_checks = 0;
            last_rx = now_rx;
        }
    }
}

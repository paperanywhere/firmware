//! The polling state machine shared between the device firmware and the
//! desktop simulator.
//!
//! The runtime is dumb. It owns no peripherals, no HTTP client, no panel
//! driver — those are passed in as `&mut`'s implementing the traits in
//! [`paperanywhere_ports`]. Each wake cycle does:
//!
//! 1. Associate WiFi using credentials from [`NvsStore`].
//! 2. GET `/api/device/state` to find out what to render and when to wake.
//! 3. If a fresh image is offered (not equal to `last_applied_image_id`):
//!    download it streamingly into the panel, refresh, persist
//!    `last_applied_image_id`, POST an ack.
//! 4. Disconnect WiFi and sleep for `next_check_at - now`.
//!
//! That's the whole protocol. Provisioning, claim-code flows, captive-portal
//! credential capture, and factory-reset detection are firmware-only concerns
//! that run *before* [`run`] is called — once you're here, the device is
//! considered provisioned and just polls forever.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::ToString;

use log::{debug, error, info, warn};
use paperanywhere_ports::{
    AckPhase, DeviceAck, DeviceStatus, EpaperPanel, FirmwareUpdater, HttpTransport, NvsStore,
    PowerPolicy, Sleeper, WifiLink,
};

/// Reasons a single wake cycle can fail. All non-fatal — the loop logs and
/// falls back to a short retry sleep so a flaky network doesn't permanently
/// brick the device.
#[derive(Debug)]
pub enum WakeError {
    NoWifiCreds,
    NoDeviceToken,
    WifiAssociate,
    StateFetch,
    BlobFetch,
    PanelWrite,
}

/// Minimum time between polls when a wake fails before its `next_check_at`
/// arrives. Keeps a misbehaving server from causing a device to spin.
const FAILURE_RETRY_SEC: u32 = 60;

/// Drive the polling loop forever. The firmware enters this from `boot::run`
/// (as an embassy task) after provisioning resolves; the sim enters it from
/// a tokio runtime task after wiring up reqwest + the virtual panel.
///
/// `default_policy` is used when a `/state` call fails before the device has
/// learned the server's preferred policy; subsequent wakes honour whatever
/// the server most recently returned.
pub async fn run<W, H, N, P, S, F>(
    wifi: &mut W,
    http: &mut H,
    nvs: &mut N,
    panel: &mut P,
    sleeper: &mut S,
    fw_updater: &mut F,
    default_policy: PowerPolicy,
    // Pre-baked boot screen, panel-native packed bytes. Rendered exactly
    // once before the first wake cycle, so users see something the moment
    // the device boots even before WiFi associates. Pass an empty slice to
    // suppress it.
    boot_screen: &[u8],
    // Status screen rendered immediately before an OTA install kicks off.
    // The flash + download window is ~30–60s on a typical firmware blob;
    // without this the panel would display the previous content while the
    // device looks frozen. Empty slice = skip (e.g. for the sim, which
    // can't OTA itself anyway).
    ota_screen: &[u8],
) -> !
where
    W: WifiLink,
    H: HttpTransport,
    N: NvsStore,
    // `Send` lifts the `+ Send` constraint on `stream_blob`'s closure up
    // through the call chain so the future stays `Send` and tokio can spawn
    // the runtime on a multi-threaded runtime. Embassy's single-threaded
    // executor accepts both `Send` and `!Send` futures, so the firmware
    // doesn't notice the constraint.
    P: EpaperPanel + Send,
    S: Sleeper,
    F: FirmwareUpdater,
{
    panel.init();
    // Seed the status bar's left-side info from the device's stored
    // token (only the last 4 hex chars are exposed, which is more
    // than enough to disambiguate a shelf of devices visually).
    if let Some(token) = nvs.load_device_token() {
        let id_view = if token.len() > 4 {
            &token[token.len() - 4..]
        } else {
            token.as_str()
        };
        let mut full: alloc::string::String = alloc::string::String::with_capacity(8);
        full.push_str("D-");
        full.push_str(id_view);
        panel.set_device_id(&full);
    }
    if !boot_screen.is_empty() {
        // No WiFi yet, no battery reading yet; the compositor renders
        // the bar with `None` inputs (disconnected wifi icon, empty
        // battery outline). Subsequent wakes refresh with live state.
        panel.set_chrome(sleeper.battery_mv(), wifi.rssi_dbm());
        panel.write_chunk(boot_screen);
        panel.refresh();
    }
    let mut active_policy = default_policy;

    let mut wake_counter: u32 = 0;
    let mut consecutive_failures: u32 = 0;
    // Boot screen is a one-time render after cold boot + first DHCP.
    // boot.rs paints the splash with IP="connecting..."; the runtime
    // does ONE redraw with the real IP overlay and then leaves the
    // panel alone until an image or the adoption screen replaces it.
    // Subsequent wakes don't touch the boot screen.
    let mut boot_screen_finalized: bool = false;
    loop {
        wake_counter = wake_counter.wrapping_add(1);
        info!(
            "=== wake #{} start (consecutive failures: {}, boot finalized: {}) ===",
            wake_counter, consecutive_failures, boot_screen_finalized
        );
        let (sleep_seconds, policy) = match single_wake_cycle(
            wifi,
            http,
            nvs,
            panel,
            sleeper,
            fw_updater,
            ota_screen,
            &mut boot_screen_finalized,
        )
        .await
        {
            Ok((secs, p)) => {
                if consecutive_failures > 0 {
                    info!(
                        "wake #{}: recovered after {} consecutive failures",
                        wake_counter, consecutive_failures
                    );
                }
                consecutive_failures = 0;
                info!("wake #{}: cycle ok, sleeping {}s", wake_counter, secs);
                panel.set_status(DeviceStatus::Ready);
                (secs, p)
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    "wake #{}: cycle failed ({:?}) — consecutive failure #{} of {}",
                    wake_counter, e, consecutive_failures, FAILURE_LIMIT_BEFORE_HALT
                );
                panel.set_status(DeviceStatus::Stalled);
                if consecutive_failures >= FAILURE_LIMIT_BEFORE_HALT {
                    error!(
                        "wake #{}: hit failure limit ({} consecutive) — halting device",
                        wake_counter, consecutive_failures
                    );
                    halt_with_screen(
                        panel,
                        "Your device ran into a problem.",
                        "Too many consecutive failures reaching the backend.",
                        "PA-NET-001",
                    );
                }
                // Exponential backoff: 2^N seconds, capped at 30 s.
                // Sequence: 1, 2, 4, 8, 16, 30, 30, ... so the device
                // retries fast initially and settles at 30 s while
                // approaching the halt threshold.
                let exp = consecutive_failures.saturating_sub(1).min(10);
                let backoff = (1u32 << exp).min(MAX_BACKOFF_SEC);
                info!(
                    "wake #{}: backing off {}s before next attempt",
                    wake_counter, backoff
                );
                (backoff, active_policy)
            }
        };
        active_policy = policy;
        sleeper.sleep_for(sleep_seconds, active_policy);
    }
}

/// Halt threshold — number of consecutive `single_wake_cycle` failures
/// before the runtime paints the BSOD-style halt screen and stops
/// trying. Beyond this point only a power-cycle or re-provision
/// recovers the device.
const FAILURE_LIMIT_BEFORE_HALT: u32 = 30;

/// Cap on the exponential-backoff sleep between failed wake cycles.
/// Pattern: 1, 2, 4, 8, 16, then 30 s until the halt threshold.
const MAX_BACKOFF_SEC: u32 = 30;

/// Single wake: associate, fetch state, maybe render, ack, disconnect.
/// Returns `(seconds_to_sleep, policy_to_use)` so the outer loop knows when
/// and how to sleep next.
async fn single_wake_cycle<W, H, N, P, S, F>(
    wifi: &mut W,
    http: &mut H,
    nvs: &mut N,
    panel: &mut P,
    sleeper: &mut S,
    fw_updater: &mut F,
    ota_screen: &[u8],
    boot_screen_finalized: &mut bool,
) -> Result<(u32, PowerPolicy), WakeError>
where
    W: WifiLink,
    H: HttpTransport,
    N: NvsStore,
    P: EpaperPanel + Send,
    S: Sleeper,
    F: FirmwareUpdater,
{
    let creds = nvs.load_wifi_creds().ok_or(WakeError::NoWifiCreds)?;
    panel.set_status(DeviceStatus::Connecting);
    info!("wake: associating to SSID \"{}\"", creds.ssid.as_str());
    wifi.associate(&creds).await.map_err(|e| {
        error!("wake: wifi.associate FAILED: {:?}", e);
        // Tell the compositor we're disconnected before bailing so any
        // subsequent forced refresh (e.g. boot screen on a retry) shows
        // the slashed wifi icon.
        panel.on_wifi_state_changed(None);
        WakeError::WifiAssociate
    })?;
    info!("wake: wifi associated ok");
    panel.on_wifi_state_changed(wifi.rssi_dbm());
    panel.on_battery_sample(sleeper.battery_mv());

    // Poll the wifi stack briefly for the DHCP-assigned IP. We push
    // it into the compositor's status state regardless of which main-
    // region view we end up painting, since the IP is used by both
    // the boot screen overlay AND the adoption screen.
    let ip_bytes = wait_for_local_ip(wifi).await;
    let ip_string: Option<alloc::string::String> = ip_bytes.map(|ip| {
        let mut buf: alloc::string::String = alloc::string::String::with_capacity(16);
        let _ = core::fmt::write(
            &mut buf,
            format_args!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]),
        );
        buf
    });
    if let Some(buf) = ip_string.as_ref() {
        info!("wake: local IP = {}", buf);
        panel.set_ip(buf);
    } else {
        warn!("wake: DHCP didn't complete within wait window");
        panel.set_ip("no DHCP");
    }

    // Branch on token presence BEFORE touching the main region so we
    // never ping-pong between boot screen and adoption screen on
    // unclaimed devices (each would have a different hash, so dedup
    // would flip on every wake).
    let token_opt = nvs.load_device_token();
    info!(
        "wake: token in NVS? {}",
        if token_opt.is_some() { "yes — render boot screen + /state" } else { "no — render adoption screen" }
    );
    let Some(token) = token_opt else {
        warn!(
            "wake: no device token (unclaimed) — adoption screen on main region, skipping /state"
        );
        panel.set_status(DeviceStatus::WaitingForAdoption);
        paint_adoption_screen(panel, nvs, wifi);
        // Keep wifi up so the dev_server stays reachable for `pa-dev push`.
        return Ok((FAILURE_RETRY_SEC, PowerPolicy::AlwaysOn));
    };

    // Claimed device: paint the boot screen with the live IP — but
    // ONLY ONCE per cold boot. Subsequent wakes leave the panel
    // alone; /state's image render is what next touches the main
    // region. The boot screen isn't re-rendered on every poll
    // because that would burn an e-ink refresh for no visible change.
    panel.set_status(DeviceStatus::Ready);
    if !*boot_screen_finalized && ip_string.is_some() {
        info!("boot-screen: one-time post-DHCP redraw to overlay IP");
        panel.redraw_boot_screen();
        panel.compose();
        let pending = panel.pending_hash();
        let cached = nvs.load_last_render_hash();
        info!(
            "boot-screen: pending_hash={:?} cached_hash={:?}",
            pending.map(|h| h & 0xFFFF_FFFF),
            cached.map(|h| h & 0xFFFF_FFFF)
        );
        if pending.is_none() || pending != cached {
            info!("boot-screen: refreshing panel");
            panel.refresh();
            if let Some(h) = pending {
                nvs.save_last_render_hash(h);
            }
        }
        *boot_screen_finalized = true;
    } else {
        info!(
            "boot-screen: already finalized this boot — leaving panel alone (image render path handles updates)"
        );
    }

    panel.set_status(DeviceStatus::DownloadingConfig);
    let state = match http.get_state(&token).await {
        Ok(s) => s,
        Err(e) => {
            error!("wake: get_state failed: {:?}", e);
            panel.set_status(DeviceStatus::Stalled);
            return Err(WakeError::StateFetch);
        }
    };

    // Firmware update offered? Apply it BEFORE rendering anything else —
    // if the install succeeds the device reboots and we never reach the
    // image-render branch this cycle. If it fails (HTTP, hash mismatch,
    // flash error) the updater logs and we continue with the rest of the
    // wake so the panel still gets refreshed.
    if let Some(update) = state.firmware_update.as_ref() {
        // No device today consumes the backend-served firmware_update
        // field. Production devices will pull releases from GitHub
        // directly (task #74). Dev devices receive updates via a
        // direct HTTP PUT to the device itself (task #79). The /state
        // field is reserved for future use; for now we just log + skip.
        info!(
            "wake: /state firmware_update {} offered but no consumer wired for this channel — skipping",
            update.version
        );
        let _ = update.revoke;
        let _ = update.byte_len;
        let _ = ota_screen;
        let _ = fw_updater;
    }

    if let Some(image) = state.image.as_ref() {
        info!("wake: image {} offered, streaming to panel", image.image_id);
        // Push current chrome state into the compositor BEFORE the
        // image stream so the status bar reflects "just associated,
        // currently rendering image N" rather than stale values.
        panel.set_chrome(sleeper.battery_mv(), wifi.rssi_dbm());
        let render_result = stream_image_to_panel(http, panel, &token, image).await;
        let phase = match &render_result {
            Ok(()) => {
                // Driver-level dedup: compose, hash, compare. The
                // dedup question is "do the image bytes + the OTHER
                // chrome (battery, wifi, usb, device id) differ from
                // last refresh?" The last-update time is excluded
                // from this compose so a clock-tick alone doesn't
                // trigger a wasted e-ink refresh.
                panel.compose();
                let pending = panel.pending_hash();
                let cached = nvs.load_last_render_hash();
                if pending.is_none() || pending != cached {
                    // We ARE going to refresh — stamp last_update
                    // with the wall-clock now (so the user sees when
                    // the panel was last touched), then recompose so
                    // the new text lands in the bar before flushing.
                    if let Some(stamp) = format_local_now(sleeper) {
                        panel.set_last_update(&stamp);
                        panel.compose();
                    }
                    panel.refresh();
                    // Save the hash of what we actually flushed, not
                    // the pre-stamp hash — otherwise the next wake
                    // would think the panel needs a refresh again
                    // just to show the same time.
                    if let Some(h) = panel.pending_hash() {
                        nvs.save_last_render_hash(h);
                    }
                } else {
                    debug!("wake: composed surface matches NVS hash — skipping refresh");
                }
                AckPhase::Applied
            }
            Err(_) => AckPhase::Failed,
        };
        let ack = DeviceAck {
            image_id: image.image_id.clone(),
            phase,
            error: render_result.as_ref().err().map(|e| format!("{:?}", e)),
            battery_mv: sleeper.battery_mv(),
            rssi_dbm: wifi.rssi_dbm(),
        };
        if let Err(e) = http.post_ack(&token, &ack).await {
            warn!("wake: post_ack: {:?}", e);
        }
    }

    // Keep wifi associated between wakes. On a ScheduledWake device the
    // chip will deep-sleep and lose state anyway; on AlwaysOn (dev),
    // dropping the link only to re-WPA-handshake next wake is what was
    // making associate intermittent.

    let now = sleeper.unix_now();
    let sleep_for = state
        .next_check_at
        .saturating_sub(now)
        .min(u32::MAX as u64) as u32;
    let sleep_for = sleep_for.max(FAILURE_RETRY_SEC);
    Ok((sleep_for, state.config.power_policy))
}

/// Pull bytes from the blob endpoint and feed them straight into the panel.
/// The panel impl tracks its own write cursor — we just keep pushing.
async fn stream_image_to_panel<H, P>(
    http: &mut H,
    panel: &mut P,
    token: &str,
    image: &paperanywhere_ports::ImageRef,
) -> Result<(), WakeError>
where
    H: HttpTransport,
    // `Send` because the closure handed to `stream_blob` captures `&mut P`,
    // and the closure must be `Send` for the returned future to be `Send`
    // (which tokio's multi-threaded runtime requires of spawned futures).
    P: EpaperPanel + Send,
{
    let result = http
        .stream_blob(token, &image.blob_url, &mut |chunk| {
            panel.write_chunk(chunk);
            Ok(())
        })
        .await;
    if let Err(e) = result {
        warn!("blob stream failed: {:?}", e);
        return Err(WakeError::BlobFetch);
    }
    // Note: refresh is the caller's responsibility — the caller runs
    // compose() + the driver-level hash dedup before deciding to flush.
    let _ = image.byte_len.to_string(); // silence unused-import lint paths
    Ok(())
}

/// Paint the halt screen (BSOD-style) onto the panel, refresh once
/// with the full LUT for clarity, then busy-loop forever. Never
/// returns — power-cycle / re-provision is the only recovery.
///
/// Marked `-> !`. Callers use `halt_with_screen(...)` like a panic in
/// terms of control flow: anything after the call is unreachable.
fn halt_with_screen<P: EpaperPanel>(
    panel: &mut P,
    headline: &'static str,
    detail: &'static str,
    code: &'static str,
) -> ! {
    error!(
        "halt: {} — {} (code {}). device will not auto-recover; reset to retry.",
        headline, detail, code
    );
    panel.set_status(DeviceStatus::Halted);
    panel.render_halt_screen(headline, detail, code);
    panel.compose();
    // Full-LUT refresh so the BSOD is crisp without ghosting.
    panel.refresh();
    // Halt: tight loop, no async progression, no deep sleep. The
    // dev_server task (if any) keeps running on the other embassy
    // task — useful for `pa-dev push` to recover a misbehaving device
    // without a serial cable.
    loop {
        core::hint::spin_loop();
    }
}

/// Paint the adoption screen onto the panel. Called when the device
/// is associated (has an IP) but has no `device_token` yet. Reads
/// the cached claim code from NVS — empty placeholder until task #84
/// wires the backend's /api/device/claim-code/request endpoint.
fn paint_adoption_screen<P, N, W>(panel: &mut P, nvs: &mut N, wifi: &W)
where
    P: EpaperPanel,
    N: NvsStore,
    W: WifiLink,
{
    let claim_code = nvs
        .load_claim_code()
        .unwrap_or_else(|| alloc::string::String::from("(requesting…)"));
    let device_id = nvs
        .load_device_token()
        .as_deref()
        .map(|t| {
            if t.len() > 4 {
                alloc::format!("D-{}", &t[t.len() - 4..])
            } else {
                alloc::format!("D-{}", t)
            }
        })
        .unwrap_or_else(|| alloc::string::String::from("(unassigned)"));
    let ip = wifi
        .local_ip()
        .map(|ip| alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]))
        .unwrap_or_else(|| alloc::string::String::from("--"));
    // Backend URL from prov / NVS — see comment in single_wake_cycle.
    let base_url = nvs
        .load_backend_url()
        .unwrap_or_else(|| alloc::string::String::from("https://paperanywhere.io"));
    let mut adopt_url = alloc::string::String::new();
    adopt_url.push_str(base_url.trim_end_matches('/'));
    adopt_url.push_str("/adopt");

    info!(
        "adoption: claim_code={} device_id={} ip={} url={}",
        claim_code, device_id, ip, adopt_url
    );
    panel.render_adoption_screen(&claim_code, &device_id, &ip, &adopt_url);
    panel.compose();
    let pending = panel.pending_hash();
    let cached = nvs.load_last_render_hash();
    info!(
        "adoption: pending_hash={:?} cached_hash={:?}",
        pending.map(|h| h & 0xFFFF_FFFF),
        cached.map(|h| h & 0xFFFF_FFFF)
    );
    if pending.is_none() || pending != cached {
        info!("adoption: hash differs — refreshing panel");
        panel.refresh();
        if let Some(h) = pending {
            nvs.save_last_render_hash(h);
        }
    } else {
        info!("adoption: hash matches — skipping refresh");
    }
}

/// Poll `wifi.local_ip()` with a small back-off until it returns Some
/// or the cumulative wait exceeds the timeout. embassy_time::Timer is
/// safe here because we're inside an embassy_executor task —
/// `embassy-time/generic-queue-8` means it doesn't even need the
/// executor's waker.
async fn wait_for_local_ip<W: WifiLink>(wifi: &W) -> Option<[u8; 4]> {
    // 150 × 100 ms = 15 s. DHCP usually completes inside ~3 s, but a
    // first-time association after boot can be slower — esp-radio + the
    // embassy-net DHCP client need to settle.
    for _ in 0..150 {
        if let Some(ip) = wifi.local_ip() {
            return Some(ip);
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
    }
    wifi.local_ip()
}

/// HH:MM stamp of the current wall-clock, or `None` when the device
/// clock hasn't been NTP-synced yet (sleeper returns 0 in that case).
/// Used by the runtime to set the status bar's "last update" field at
/// the moment we commit to a panel refresh.
fn format_local_now<S: Sleeper>(sleeper: &S) -> Option<alloc::string::String> {
    let now = sleeper.unix_now();
    if now == 0 {
        return None;
    }
    let hh = (now / 3600) % 24;
    let mm = (now / 60) % 60;
    let mut buf: alloc::string::String = alloc::string::String::with_capacity(8);
    let _ = core::fmt::write(&mut buf, format_args!("{:02}:{:02}", hh, mm));
    Some(buf)
}

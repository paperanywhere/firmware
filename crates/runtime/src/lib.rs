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
    AckPhase, DeviceAck, EpaperPanel, FirmwareUpdater, HttpTransport, NvsStore, PowerPolicy,
    Sleeper, WifiLink,
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

    loop {
        let (sleep_seconds, policy) = match single_wake_cycle(
            wifi, http, nvs, panel, sleeper, fw_updater, ota_screen,
        )
        .await
        {
            Ok((secs, p)) => (secs, p),
            Err(e) => {
                warn!("wake: cycle failed: {:?}", e);
                (FAILURE_RETRY_SEC, active_policy)
            }
        };
        active_policy = policy;
        sleeper.sleep_for(sleep_seconds, active_policy);
    }
}

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

    // Poll the wifi stack briefly for the DHCP-assigned IP, then push
    // it into the compositor + redraw the boot screen. We do this
    // BEFORE the token check so an unclaimed dev device still shows
    // its IP on the panel (so the user can `pa-dev push` to it).
    let ip = wait_for_local_ip(wifi).await;
    if let Some(ip) = ip {
        let mut buf: alloc::string::String = alloc::string::String::with_capacity(16);
        let _ = core::fmt::write(
            &mut buf,
            format_args!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]),
        );
        info!("wake: local IP = {}", buf);
        panel.set_ip(&buf);
        panel.redraw_boot_screen();
        panel.compose();
        let pending = panel.pending_hash();
        let cached = nvs.load_last_render_hash();
        if pending.is_none() || pending != cached {
            info!("wake: boot-screen template differs (likely IP changed) — refreshing");
            panel.refresh();
            if let Some(h) = pending {
                nvs.save_last_render_hash(h);
            }
        }
    } else {
        warn!("wake: DHCP didn't complete within wait window — boot screen won't show IP");
    }

    // Now check for a device token. Without one we can't hit /state, but
    // the panel is up + the dev_server is reachable, which is enough
    // for a dev rig waiting to be claimed (or for `pa-dev push` to
    // flash a new build). Return Ok so the caller doesn't trigger the
    // failure-retry path and we sleep our normal interval.
    let token = match nvs.load_device_token() {
        Some(t) => t,
        None => {
            warn!(
                "wake: no device token in NVS (not yet claimed) — skipping /state, panel + dev_server stay up"
            );
            // DO NOT disconnect — the dev_server task is still bound to
            // the same embassy-net stack and the user might `pa-dev push`
            // at any moment. We want the IP to stick around.
            return Ok((FAILURE_RETRY_SEC, PowerPolicy::AlwaysOn));
        }
    };

    let state = http.get_state(&token).await.map_err(|e| {
        error!("wake: get_state: {:?}", e);
        WakeError::StateFetch
    })?;

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

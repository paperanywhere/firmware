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
    wifi.associate(&creds).map_err(|e| {
        error!("wake: wifi.associate: {:?}", e);
        // Tell the compositor we're disconnected before bailing so any
        // subsequent forced refresh (e.g. boot screen on a retry) shows
        // the slashed wifi icon.
        panel.on_wifi_state_changed(None);
        WakeError::WifiAssociate
    })?;
    // Association succeeded — push the new RSSI into the status bar.
    panel.on_wifi_state_changed(wifi.rssi_dbm());
    panel.on_battery_sample(sleeper.battery_mv());

    let token = nvs.load_device_token().ok_or(WakeError::NoDeviceToken)?;

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
        info!(
            "wake: backend offered firmware update {} (revoke={}, {} bytes)",
            update.version, update.revoke, update.byte_len
        );
        if !ota_screen.is_empty() {
            // Render the "Updating firmware..." status frame before the
            // long flash-write window so the user sees something other
            // than the previous image while the panel is otherwise idle.
            // The render-hash is updated so the next post-reset boot's
            // dedup check correctly decides whether to refresh again.
            let ota_screen_hash = paperanywhere_ports::hash_bytes(ota_screen);
            if Some(ota_screen_hash) != nvs.load_last_render_hash() {
                panel.set_chrome(sleeper.battery_mv(), wifi.rssi_dbm());
                panel.write_chunk(ota_screen);
                panel.refresh();
                nvs.save_last_render_hash(ota_screen_hash);
            }
        }
        if let Err(e) = fw_updater.apply(http, &token, update).await {
            warn!("wake: firmware update failed: {:?}", e);
            // fall through — non-fatal; resume the normal render path.
        }
    }

    if let Some(image) = state.image.as_ref() {
        // Content hash from the backend's `sha256_hex` — same hash function
        // boot.rs uses for the splash. Comparing against `last_render_hash`
        // catches "same content under a different image_id" and "fresh-
        // device-but-server-thinks-it's-already-applied" simultaneously.
        let new_hash = paperanywhere_ports::hash_bytes(image.sha256_hex.as_bytes());
        if Some(new_hash) != nvs.load_last_render_hash() {
            info!("wake: new image {} (hash {:#x}), streaming to panel", image.image_id, new_hash);
            // Push current chrome state into the compositor BEFORE the
            // image stream so the status bar reflects "just associated,
            // currently rendering image N" rather than stale values.
            panel.set_chrome(sleeper.battery_mv(), wifi.rssi_dbm());
            let render_result = stream_image_to_panel(http, panel, &token, image).await;
            let phase = match &render_result {
                Ok(()) => {
                    nvs.save_last_render_hash(new_hash);
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
        } else {
            debug!("wake: image {} already on panel (hash match), skipping", image.image_id);
        }
    }

    let _ = wifi.disconnect();
    panel.on_wifi_state_changed(None);

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
    panel.refresh();
    let _ = image.byte_len.to_string(); // silence unused-import lint paths
    Ok(())
}

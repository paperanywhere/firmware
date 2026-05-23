//! The polling state machine. Drives the device's wake cycle (associate,
//! fetch state, render, ack, sleep) and emits paint commands to the
//! panel-actor task instead of touching the panel directly.
//!
//! After the actor-pattern migration the runtime owns no panel. Every
//! panel-touching operation is a [`PaintCmd`] pushed down
//! [`PaintChannel`]; the actor task (in the firmware crate) consumes
//! and renders. This means:
//!
//!   * the runtime never blocks for an e-paper refresh,
//!   * the panel actor can live on a second CPU core (it does, on
//!     ESP32-S3),
//!   * the OTA install path's progress events flow into the actor
//!     through the same channel, so a firmware update preempts
//!     whatever non-urgent paint the runtime had queued.
//!
//! Each wake cycle does:
//!
//! 1. Associate WiFi using credentials from [`NvsStore`].
//! 2. GET `/api/device/state` for the next image + sleep window.
//! 3. If a fresh image is offered, stream it into a buffer and emit
//!    [`PaintCmd::ShowImage`].
//! 4. Disconnect WiFi (modem-sleep) and sleep for `next_check_at - now`.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::String as HString;
use log::{error, info, warn};
use paperanywhere_ports::{
    AckPhase, BatteryGauge, DeviceAck, DeviceIdentity, DeviceStatus, FirmwareUpdater, HttpTransport,
    NvsStore, OtaPhase, PowerPolicy, Sleeper, WifiLink,
};
use paperanywhere_ports::chrome::{self, Persist};

pub mod paint;
pub use paint::{
    AdoptUrlStr, ClaimCodeStr, DeviceIdStr, IpStr, LastUpdateStr, OwnerEmailStr, PaintChannel,
    PaintCmd, PaintHandle, ProjectNameStr, SeqCmd, PAINT_CHANNEL_DEPTH, mark_processed, submit,
    submit_silent,
};

/// Cross-task channel for OTA progress events. The OTA install path
/// calls `signal()` with each phase change; the panel-actor task
/// wakes on each signal and renders the live progress view. The
/// runtime does NOT observe this signal Ã¢â‚¬â€ the actor drops
/// view-change commands (adoption / boot / image) during OTA, so the
/// runtime can keep its wake cycle running without clobbering the UI.
///
/// The mutex flavour (`CriticalSectionRawMutex`) is the smallest one
/// that works in both the firmware's interrupt context and a hosted
/// test context. Only the latest phase is stored Ã¢â‚¬â€ older unconsumed
/// events are dropped, which is exactly what we want (the actor only
/// ever needs to render the *current* state).
pub type OtaProgressChannel = Signal<CriticalSectionRawMutex, OtaPhase>;

/// Reasons a single wake cycle can fail. All non-fatal Ã¢â‚¬â€ the loop logs
/// and falls back to a short retry sleep so a flaky network doesn't
/// permanently brick the device.
#[derive(Debug)]
pub enum WakeError {
    NoWifiCreds,
    NoDeviceToken,
    WifiAssociate,
    /// DHCP didn't assign an IP within `wait_for_local_ip`'s 15 s
    /// window. Distinct from `WifiAssociate` (which means we never
    /// joined the AP at all) Ã¢â‚¬â€ here we joined fine but the router's
    /// DHCP didn't reply in time. Common on cold boots if the WPA
    /// handshake takes long enough to push DHCP past the deadline.
    DhcpTimeout,
    StateFetch,
    BlobFetch,
    PanelWrite,
}

/// Minimum time between polls when a wake fails before its
/// `next_check_at` arrives. Keeps a misbehaving server from causing a
/// device to spin.
///
/// Note: a faster cadence here for the pre-adoption state was tried
/// and reverted — under the UniFi-blackhole conditions we've seen,
/// rapid wake cycles caused the radio to refuse far more TX
/// (NET_TX_NONE went from ~50 to ~10500 within a 3-min window)
/// because of WPA handshake churn. 60 s gives the AP / radio time to
/// settle between attempts.
const FAILURE_RETRY_SEC: u32 = 60;

/// Halt threshold Ã¢â‚¬â€ number of consecutive `single_wake_cycle` failures
/// before the runtime paints the BSOD-style halt screen and stops
/// trying. Beyond this point only a power-cycle or re-provision
/// recovers the device.
const FAILURE_LIMIT_BEFORE_HALT: u32 = 30;

/// Cap on the exponential-backoff sleep between failed wake cycles.
/// Pattern: 1, 2, 4, 8, 16, then 30 s until the halt threshold.
const MAX_BACKOFF_SEC: u32 = 30;

/// Drive the polling loop forever. The firmware enters this from
/// `boot::run` (as an embassy task) after provisioning resolves.
///
/// `default_policy` is used when a `/state` call fails before the
/// device has learned the server's preferred policy; subsequent wakes
/// honour whatever the server most recently returned.
pub async fn run<W, H, N, S, F, B>(
    wifi: &mut W,
    http: &mut H,
    nvs: &mut N,
    sleeper: &mut S,
    fw_updater: &mut F,
    battery: &mut B,
    default_policy: PowerPolicy,
    // Hardware identity used by `POST /api/device/register` on the first
    // unclaimed boot. The MAC + panel_model_id let the backend create an
    // anonymous device row + mint a claim_code that the adoption screen
    // can then display to the user.
    identity: DeviceIdentity,
    // Shared signal carrying live OTA phase updates. The OTA install
    // path writes to this; the panel actor task (NOT the runtime)
    // consumes it and renders the progress view.
    // Accepted here so the runtime can pass it through to anywhere
    // that still wants to react to OTA state (e.g. surface "updating"
    // status text). Today no runtime branch reads from it; kept on
    // the signature for forward compatibility.
    ota_progress: &'static OtaProgressChannel,
    // Cross-task paint channel. Every panel-touching operation flows
    // through here to the actor task.
    paint: &'static PaintChannel,
) -> !
where
    W: WifiLink,
    H: HttpTransport,
    N: NvsStore,
    S: Sleeper,
    F: FirmwareUpdater,
    B: BatteryGauge,
{
    // Accepted for forward compatibility; the actor consumes the OTA
    // signal directly. Keeping it on the signature so call sites in
    // boot.rs don't change when a future hook needs it.
    let _ = ota_progress;

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
        paint::submit_silent(paint, PaintCmd::UpdateDeviceId(to_hstring(&full))).await;
    }

    let mut active_policy = default_policy;

    let mut wake_counter: u32 = 0;
    let mut consecutive_failures: u32 = 0;
    // Boot screen is a one-time render after cold boot + first DHCP.
    // boot.rs paints the splash with IP="connecting..." pre-executor;
    // the runtime sends ONE RedrawBootScreen command with the real IP
    // and then leaves the actor alone until an image or the adoption
    // screen replaces it.
    let mut boot_screen_finalized: bool = false;
    // Main-view placeholder is rendered once on the first wake where
    // the device is adopted. Subsequent wakes skip the re-render via
    // the actor's framebuffer-hash dedup, but we also gate at the
    // runtime level so we don't queue a paint command that's about
    // to be a no-op. Reset to false on unpair so the next adoption
    // cycle does the visible boot -> main transition again.
    let mut main_view_finalized: bool = false;

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
            sleeper,
            fw_updater,
            battery,
            &identity,
            paint,
            &mut boot_screen_finalized,
            &mut main_view_finalized,
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
                paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Ready)).await;
                (secs, p)
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    "wake #{}: cycle failed ({:?}) Ã¢â‚¬â€ consecutive failure #{} of {}",
                    wake_counter, e, consecutive_failures, FAILURE_LIMIT_BEFORE_HALT
                );
                paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Stalled)).await;
                if consecutive_failures >= FAILURE_LIMIT_BEFORE_HALT {
                    error!(
                        "wake #{}: hit failure limit ({} consecutive) Ã¢â‚¬â€ halting device",
                        wake_counter, consecutive_failures
                    );
                    halt_with_screen(
                        paint,
                        "Your device ran into a problem.",
                        "Too many consecutive failures reaching the backend.",
                        "PA-NET-001",
                    )
                    .await;
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
        // Bringup gate: don't enter deep sleep until both the boot
        // screen has been finalized (we've rendered the splash with
        // the real DHCP-assigned IP on top) AND adoption has
        // completed (NVS holds a device token). Until both are true
        // the user is still in an "is this device working?" loop Ã¢â‚¬â€
        // pa-dev iteration, claim-code typing, watching the panel
        // come up Ã¢â‚¬â€ and a 6-hour deep sleep would strand them.
        // Production devices flip to whatever the backend asked for
        // (scheduled_wake / always_on) as soon as bringup is done.
        let bringup_done = boot_screen_finalized && nvs.load_device_token().is_some();
        // Dev builds also force AlwaysOn permanently so `pa-dev push`
        // stays reachable indefinitely. Production builds honour
        // bringup-gated backend policy.
        active_policy = if !bringup_done || nvs.load_is_dev_build() {
            PowerPolicy::AlwaysOn
        } else {
            policy
        };

        sleeper.sleep_for(sleep_seconds, active_policy).await;
    }
}

/// Single wake: associate, fetch state, maybe render, ack, disconnect.
/// Returns `(seconds_to_sleep, policy_to_use)` so the outer loop knows
/// when and how to sleep next.
async fn single_wake_cycle<W, H, N, S, F, B>(
    wifi: &mut W,
    http: &mut H,
    nvs: &mut N,
    sleeper: &mut S,
    fw_updater: &mut F,
    battery: &mut B,
    identity: &DeviceIdentity,
    paint: &'static PaintChannel,
    boot_screen_finalized: &mut bool,
    main_view_finalized: &mut bool,
) -> Result<(u32, PowerPolicy), WakeError>
where
    W: WifiLink,
    H: HttpTransport,
    N: NvsStore,
    S: Sleeper,
    F: FirmwareUpdater,
    B: BatteryGauge,
{
    let creds = nvs.load_wifi_creds().ok_or(WakeError::NoWifiCreds)?;
    paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Connecting))
        .await;
    // Push the SSID + 3-state link signal so the boot screen's
    // Network column reflects the in-flight association attempt
    // (rather than the stale "Disconnected" default).
    let ssid_buf: heapless::String<32> = to_hstring(creds.ssid.as_str());
    paint::submit_silent(paint, PaintCmd::UpdateSsid(Some(ssid_buf))).await;
    paint::submit_silent(paint, PaintCmd::UpdateWifiLinkState(
            paperanywhere_ports::WifiLinkState::Connecting,
        ))
        .await;
    info!("wake: associating to SSID \"{}\"", creds.ssid.as_str());
    let assoc_result = wifi.associate(&creds).await;
    if let Err(e) = assoc_result {
        error!("wake: wifi.associate FAILED: {:?}", e);
        paint::submit_silent(paint, PaintCmd::WifiDisconnected).await;
        // Auth failures are PERMANENT until creds change — looping is
        // futile and just hammers the AP. Halt with a BSOD that
        // names the SSID so the user knows exactly what to fix.
        if W::is_auth_error(&e) {
            halt_with_screen(
                paint,
                "WiFi authentication failed.",
                "The AP rejected our credentials or refused the handshake. \
                 Re-provision with `pa-dev provision` to change SSID/password.",
                "PA-WIFI-AUTH",
            )
            .await;
        }
        return Err(WakeError::WifiAssociate);
    }
    info!("wake: wifi associated ok");
    // One battery sample per wake. Published straight into chrome so
    // the status bar's battery widget refreshes alongside the rssi
    // sample the actor publishes via UpdateChrome below. Doing it
    // once at the top of the wake cycle (rather than on every paint)
    // keeps the divider's bleed current bounded.
    let battery_sample = battery.sample().await;
    if let Some(ref s) = battery_sample {
        info!("wake: battery {}mv ({}%)", s.mv, s.percent);
    }
    chrome::set_battery(battery_sample);
    paint::submit_silent(paint, PaintCmd::UpdateChrome {
            battery_mv: battery_sample.map(|s| s.mv),
            rssi_dbm: wifi.rssi_dbm(),
        })
        .await;

    // Poll the wifi stack briefly for the DHCP-assigned IP. We push
    // it into the actor's status state regardless of which main-region
    // view we end up painting, since the IP is used by both the boot
    // screen overlay AND the adoption screen.
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
        paint::submit_silent(paint, PaintCmd::UpdateIp(to_hstring(buf))).await;
        // Push the gateway alongside the IP. embassy-net populates
        // `cfg.gateway` from the DHCP offer the moment the lease is
        // ready, so by the time `local_ip` is Some so is `gateway_v4`
        // (when the network's DHCP server announces one). Surfaced on
        // the boot screen's Network column.
        let gw: Option<IpStr> = wifi.gateway_v4().map(|g| {
            let mut buf: alloc::string::String = alloc::string::String::with_capacity(16);
            let _ = core::fmt::write(
                &mut buf,
                format_args!("{}.{}.{}.{}", g[0], g[1], g[2], g[3]),
            );
            to_hstring(&buf)
        });
        paint::submit_silent(paint, PaintCmd::UpdateGateway(gw)).await;
        paint::submit_silent(paint, PaintCmd::UpdateWifiLinkState(
                paperanywhere_ports::WifiLinkState::Connected,
            ))
            .await;
    } else {
        // No DHCP this cycle. Painting the adoption screen would lie
        // to the user (claim code can't be entered if the dashboard
        // isn't reachable, /state can't poll either), so DON'T fall
        // into the adoption branch. Instead return a wake error and
        // let the outer loop retry; the consecutive-failure halt
        // (FAILURE_LIMIT_BEFORE_HALT) takes over only if DHCP keeps
        // failing across many wakes Ã¢â‚¬â€ which is the right behaviour
        // on a freshly-reset device where WPA + DHCP can plausibly
        // take longer than the 15 s wait window on the first attempt.
        warn!("wake: DHCP didn't complete within wait window Ã¢â‚¬â€ retrying");
        // IP field stays empty (None Ã¢â€ â€™ "--" in the boot-screen render).
        // The runtime can't currently send Option<IpStr>::None via
        // UpdateIp (which takes a concrete IpStr), so we send a "--"
        // string as the IP placeholder. TODO: switch UpdateIp to
        // Option<IpStr> for clean signalling.
        paint::submit_silent(paint, PaintCmd::UpdateIp(to_hstring("--"))).await;
        paint::submit_silent(paint, PaintCmd::UpdateGateway(None)).await;
        paint::submit_silent(paint, PaintCmd::WifiDisconnected).await;
        return Err(WakeError::DhcpTimeout);
    }

    // First-DHCP hand-off: redraw the boot screen with the real IP
    // overlay, then run a visible 5-second countdown at the bottom of
    // the build-info block before transitioning to the next view
    // (adoption screen for unclaimed, image render for claimed). Runs
    // for BOTH paths on the first wake Ã¢â‚¬â€ the user explicitly wanted
    // the boot template to populate with concrete info (IP now,
    // wall-clock time once NTP lands per task #78) and tick down
    // visibly so they know the splash is about to disappear. Only
    // fires once per cold boot.
    //
    // NTP integration is pending (#78). Once it lands, this block
    // should also wait for the clock to be synced before starting the
    // countdown Ã¢â‚¬â€ i.e. "we have network address AND wall-clock time"
    // gates the countdown. For now we proceed once we have IP only;
    // the boot-screen's "Last Update" / time field stays at its
    // pre-NTP default until #78.
    if !*boot_screen_finalized && ip_string.is_some() {
        info!(
            "boot-screen: post-DHCP redraw + 10s countdown before view transition"
        );
        // First, refresh the boot screen with the real IP overlay. No
        // countdown yet Ã¢â‚¬â€ give the user a moment to actually see the
        // populated splash (IP, Gateway, WiFi=Connected) before the
        // countdown line appears at the bottom.
        paint::submit_silent(paint, PaintCmd::RedrawBootScreen).await;
        embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;

        // Countdown 10 Ã¢â€ â€™ 1, one second per tick.
        const BOOT_HOLD_SECS: u8 = 10;
        for n in (1u8..=BOOT_HOLD_SECS).rev() {
            paint::submit_silent(paint, PaintCmd::UpdateBootCountdown(Some(n))).await;
            embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
        }
        // Clear the countdown line before the view transition so any
        // future boot-screen re-render (post-OTA reboot, etc.) doesn't
        // start with a stale "Transitioning in 1 second..." message.
        paint::submit_silent(paint, PaintCmd::UpdateBootCountdown(None)).await;
        *boot_screen_finalized = true;
    }

    // Branch on token presence so we never ping-pong between boot
    // screen and adoption screen on unclaimed devices.
    let token_opt = nvs.load_device_token();
    info!(
        "wake: token in NVS? {}",
        if token_opt.is_some() {
            "yes Ã¢â‚¬â€ render /state image flow"
        } else {
            "no Ã¢â‚¬â€ render adoption screen"
        }
    );
    // If we have a token AND a cached claim_code, we registered with
    // the backend but the user hasn't completed dashboard adoption
    // yet. Keep showing the adoption screen + poll /state to detect
    // when the user pairs (backend will clear claim_code in response,
    // we clear it locally on next register-skip wake).
    let still_unadopted = token_opt.is_some() && nvs.load_claim_code().is_some();
    if still_unadopted {
        info!("wake: registered but claim_code still cached — keep adoption screen + poll /state");
    }
    let Some(token) = token_opt.filter(|_| !still_unadopted) else {
        warn!(
            "wake: no device token (unclaimed) -- adoption screen on main region, skipping /state"
        );
        // Re-arm the main-view gate. If a prior wake painted main and
        // we lost the token (factory reset / unpair), the next adopted
        // /state cycle should redo the visible boot -> main transition
        // instead of silently skipping it.
        *main_view_finalized = false;
        paint::submit_silent(
            paint,
            PaintCmd::UpdateStatus(DeviceStatus::WaitingForAdoption),
        )
        .await;
        // UX ordering: paint adoption screen FIRST so the user sees
        // the view transition out of the boot splash immediately.
        // Await the returned PaintHandle so we don't start the HTTP
        // register call until the e-paper's full-LUT refresh
        // actually finishes â€” otherwise the panel sits on the boot
        // screen during the entire TLS+request round-trip and the
        // user has no signal that anything is happening.
        //
        // The handle resolves exactly when the actor publishes our
        // seq to PROCESSED_SEQ_WATCH â€” no magic timers, no estimated
        // panel-cycle durations. If a future panel is slower or
        // faster, this just adjusts automatically.
        let retry_notice = adoption_retry_notice(ip_string.as_deref(), nvs);
        let adoption_handle =
            paint_adoption_screen(paint, nvs, wifi, retry_notice).await;
        info!("wake: adoption screen paint queued (seq={}) â€” awaiting panel refresh before register", adoption_handle.seq());
        adoption_handle.await_processed().await;
        info!("wake: adoption screen refresh complete â€” proceeding to register");

        // DEBUG: skip /register entirely so the operator can verify
        // basic network connectivity (ping from another host on the
        // LAN) before involving the HTTP path. Set DEBUG_SKIP_REGISTER
        // to false to restore the normal flow. Validated 2026-05-22:
        // pings to the device work cleanly while in skip mode, so
        // network is healthy — turning off to test /register again.
        const DEBUG_SKIP_REGISTER: bool = false;
        if DEBUG_SKIP_REGISTER {
            info!(
                "wake: DEBUG_SKIP_REGISTER — device IP {} is up, sitting at adoption screen. \
                 Try `ping {}` from another LAN host to verify basic connectivity.",
                ip_string.as_deref().unwrap_or("--"),
                ip_string.as_deref().unwrap_or("<no ip>")
            );
            return Ok((FAILURE_RETRY_SEC, PowerPolicy::AlwaysOn));
        }

        // Now do the register call. Idempotent on MAC, so a re-wake
        // re-issues a fresh claim code without creating a duplicate
        // device row. Skip when we already have a cached code (the
        // adoption screen we just painted will already show it).
        if nvs.load_claim_code().is_none() {
            info!(
                "wake: no cached claim code Ã¢â‚¬â€ calling /api/device/register (mac={}, panel_model_id={})",
                identity.mac, identity.panel_model_id
            );
            match http.register(identity).await {
                Ok(reg) => {
                    info!(
                        "wake: register ok Ã¢â‚¬â€ uuid={}, claim_code={}",
                        reg.device_uuid, reg.claim_code
                    );
                    // token + claim_code aren't in chrome (they're
                    // pure NVS-domain Ã¢â‚¬â€ never displayed, never
                    // mutated mid-session). Save those directly. The
                    // token-before-code ordering still matters: a
                    // power-loss between the two writes leaves us in
                    // the "have token, no code" state, which the
                    // outer loop reads as already-claimed Ã¢â€ â€™ proceeds
                    // to /state, which is recoverable. The reverse
                    // (code-without-token) would leave us with a code
                    // but no auth Ã¢â‚¬â€ stuck.
                    nvs.save_device_token(&reg.device_token);
                    nvs.save_claim_code(&reg.claim_code);
                    // UUID is a chrome value Ã¢â‚¬â€ single call now writes
                    // to NVS via the persistence hook AND fires the
                    // dirty signal so the panel actor re-renders the
                    // boot screen / adoption screen with the new UUID.
                    // What used to be six lines (nvs save + heapless
                    // string + paint.send) is now one.
                    chrome::set_device_uuid_with(
                        Some(&reg.device_uuid),
                        Persist::Flash,
                    );
                    // Re-paint adoption now that we have the real
                    // code. Compositor's within-view fast-LUT path
                    // updates just the code field on the existing
                    // adoption layout Ã¢â‚¬â€ no 3-s flash, no ghosting.
                    let retry_notice = adoption_retry_notice(ip_string.as_deref(), nvs);
                    // Fire-and-forget â€” the user doesn't need to wait
                    // for the post-register repaint before we continue
                    // the wake loop. The actor will refresh asynchronously.
                    let _ = paint_adoption_screen(paint, nvs, wifi, retry_notice).await;
                }
                Err(e) => {
                    warn!(
                        "wake: /api/device/register failed: {:?} Ã¢â‚¬â€ adoption screen stays on '(requestingÃ¢â‚¬Â¦)' placeholder",
                        e
                    );
                    // The adoption screen we already painted at the
                    // top of this branch shows the placeholder. Next
                    // wake will retry register; on success the fast-
                    // LUT path swaps the placeholder for the code.
                }
            }
        } else {
            info!("wake: claim code already cached in NVS - probing /state for adoption");
            // Poll /state with our token. If 200 -> backend has accepted
            // us as adopted (the dashboard consumed the claim_code);
            // clear it locally so the next wake takes the /state-only
            // path. If 401 -> token stale, clear both + re-register.
            if let Some(t) = nvs.load_device_token() {
                match http.get_state(&t).await {
                    Ok(_) => {
                        info!("wake: /state ok with token - device is adopted, clearing claim_code");
                        nvs.clear_claim_code();
                    }
                    Err(e) if H::is_unauthorized_error(&e) => {
                        warn!("wake: /state returned 401 - token stale, re-registering on next wake");
                        nvs.clear_device_token();
                        nvs.clear_claim_code();
                    }
                    Err(e) => {
                        info!("wake: /state probe transient err {:?} - adoption screen stays", e);
                    }
                }
            }
        }

        // Stay AlwaysOn so the backend can reach us to push the
        // claim Ã¢â€ â€™ adoption transition and any subsequent firmware
        // update offer without waiting on the next scheduled wake.
        return Ok((FAILURE_RETRY_SEC, PowerPolicy::AlwaysOn));
    };

    // Claimed device: transition out of the boot screen into the main
    // view placeholder so the user sees a clean boot -> adopted hand-
    // off BEFORE /state is in flight. Subsequent wakes (where the
    // boot screen was already torn down) skip this re-render because
    // the actor's framebuffer-hash dedup catches the identical
    // placeholder bytes -- no wasted refresh.
    if !*main_view_finalized {
        paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Ready)).await;
        let main_handle = paint::submit(
            paint,
            build_show_main_cmd(ip_string.as_deref(), sleeper),
        )
        .await;
        info!(
            "wake: main placeholder paint queued (seq={}) -- awaiting refresh before /state",
            main_handle.seq()
        );
        main_handle.await_processed().await;
        info!("wake: main placeholder refresh complete -- proceeding to /state");
        *main_view_finalized = true;
    } else {
        paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Ready)).await;
    }

    paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::DownloadingConfig))
        .await;
    let state = match http.get_state(&token).await {
        Ok(s) => s,
        Err(e) => {
            // Backend rejected our token -- device was unpaired from
            // the dashboard. Wipe NVS + drop back to adoption so the
            // next wake re-registers and the user gets a fresh code.
            if H::is_unauthorized_error(&e) {
                warn!(
                    "wake: /state returned Unauthorized -- device unpaired server-side, resetting to adoption"
                );
                nvs.clear_device_token();
                nvs.clear_claim_code();
                chrome::set_device_uuid_with(None, Persist::Flash);
                chrome::set_device_name_with(None, Persist::Flash);
                paint::submit_silent(
                    paint,
                    PaintCmd::UpdateStatus(DeviceStatus::WaitingForAdoption),
                )
                .await;
                let retry_notice = adoption_retry_notice(ip_string.as_deref(), nvs);
                let adoption_handle =
                    paint_adoption_screen(paint, nvs, wifi, retry_notice).await;
                adoption_handle.await_processed().await;
                // Re-arm the main-view gate so the NEXT successful
                // adoption -> /state cycle does the visible boot ->
                // main transition again instead of silently dedup'ing.
                *main_view_finalized = false;
                return Ok((FAILURE_RETRY_SEC, PowerPolicy::AlwaysOn));
            }
            error!("wake: get_state failed: {:?}", e);
            paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Stalled)).await;
            return Err(WakeError::StateFetch);
        }
    };

    // /state carries the user-supplied friendly name + the backend-
    // assigned UUID. With chrome's persistent setters, each is a
    // single call that writes the in-memory KV, fires the dirty
    // signal (so the actor re-renders the boot screen on its next
    // wake), AND mirrors the value to NVS via the registered
    // persistence hook. No paint.send, no manual NVS save Ã¢â‚¬â€ that
    // collapses what used to be ~6 lines per field into one.
    //
    // The save_* mutators inside the hook early-return when the value
    // is unchanged, so re-issuing on every wake is cheap.
    if let Some(uuid) = state.device_uuid.as_deref() {
        chrome::set_device_uuid_with(Some(uuid), Persist::Flash);
    }
    if let Some(name) = state.name.as_deref() {
        chrome::set_device_name_with(Some(name), Persist::Flash);
    }
    // Owner identity Ã¢â‚¬â€ surfaced on the main-view placeholder. Not
    // persisted to NVS (would require a migration + extra storage)
    // since /state delivers fresh values on every poll. Idle screen
    // shows "--" between cold boot and the first /state success.
    chrome::set_owner_email(state.owner_email.as_deref());
    chrome::set_project_name(state.project_name.as_deref());

    // Choose the "ready" view for this wake:
    //   * Image pending (from the device queue)  -> handled below
    //   * Non-empty playlist                     -> ShowPlaylistPage
    //   * Otherwise                              -> ShowMain fallback
    // The user authored the playlist; we paint exactly one page per
    // wake here. Page selection cycles across wakes (see
    // PLAYLIST_PAGE_INDEX). When the AdvancePolicy on the painted
    // page is `After { seconds }` the outer loop overrides the
    // backend-supplied next_check_at so the next wake lands at the
    // dwell deadline.
    let mut playlist_advance_override: Option<u32> = None;
    if state.image.is_none() {
        match state.playlist.as_ref() {
            Some(playlist) if !playlist.pages.is_empty() => {
                let total = playlist.pages.len();
                let idx = next_playlist_page_index(total);
                let page = playlist.pages[idx].clone();
                if let cardstock::AdvancePolicy::After { seconds } = page.advance {
                    playlist_advance_override = Some(seconds.max(1));
                }
                info!(
                    "wake: playlist page {}/{} (id={}, advance={:?})",
                    idx + 1,
                    total,
                    page.id,
                    page.advance
                );
                paint::submit_silent(paint, PaintCmd::ShowPlaylistPage {
                    page,
                    index: idx as u16,
                    total: total as u16,
                })
                .await;
            }
            _ => {
                // Empty / absent playlist -> fall back to the main
                // placeholder so the panel still reflects identity.
                paint::submit_silent(
                    paint,
                    build_show_main_cmd(ip_string.as_deref(), sleeper),
                )
                .await;
            }
        }
    }

    // Firmware update offered? Today no device consumes the backend-
    // served firmware_update field. Production devices will pull
    // releases from GitHub directly (task #74). Dev devices receive
    // updates via a direct HTTP PUT to the device (task #79). The
    // /state field is reserved for future use; for now we just log.
    if let Some(update) = state.firmware_update.as_ref() {
        info!(
            "wake: /state firmware_update {} offered but no consumer wired for this channel Ã¢â‚¬â€ skipping",
            update.version
        );
        let _ = update.revoke;
        let _ = update.byte_len;
        let _ = fw_updater;
    }

    if let Some(image) = state.image.as_ref() {
        info!("wake: image {} offered, streaming to panel", image.image_id);
        // Push current chrome state into the actor BEFORE the image
        // stream so the status bar reflects "just associated, currently
        // rendering image N" rather than stale values.
        paint::submit_silent(paint, PaintCmd::UpdateChrome {
                battery_mv: battery_sample.map(|s| s.mv),
                rssi_dbm: wifi.rssi_dbm(),
            })
            .await;
        let render_result = fetch_image_bytes(http, &token, image).await;
        let phase = match &render_result {
            Ok(bytes) => {
                let last_update = format_local_now(sleeper);
                paint::submit_silent(paint, PaintCmd::ShowImage {
                        bytes: bytes.clone(),
                        last_update,
                    })
                    .await;
                AckPhase::Applied
            }
            Err(_) => AckPhase::Failed,
        };
        let ack = DeviceAck {
            image_id: image.image_id.clone(),
            phase,
            error: render_result.as_ref().err().map(|e| format!("{:?}", e)),
            battery_mv: battery_sample.map(|s| s.mv),
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
    // Playlist override: if the current page uses
    // `AdvancePolicy::After { seconds }`, wake again at the dwell
    // deadline (clamped against the backend's next_check_at so we
    // never wait LONGER than the server expected). Sticky / Manual
    // pages don't override -- they re-poll on the normal cadence.
    let sleep_for = match playlist_advance_override {
        Some(dwell) => dwell.min(sleep_for),
        None => sleep_for,
    };

    // Return the backend's policy unmodified Ã¢â‚¬â€ the outer loop applies
    // the bringup-gate ("don't sleep until both boot screen + adoption
    // are finalized") and the dev-build override. Doing both checks
    // in one place keeps the rule legible.
    Ok((sleep_for, state.config.power_policy))
}

/// Pull bytes from the blob endpoint into a buffer. The buffer is then
/// shipped to the panel actor as a single `ShowImage` command. Buffering
/// (rather than chunked-write straight to the panel) is what lets the
/// runtime decouple from the panel Ã¢â‚¬â€ without a channel-friendly streaming
/// primitive, we trade one heap allocation per image for the actor
/// pattern's separation of concerns. 1bpp 800Ãƒâ€”480 is 48 KB; even the
/// 7-color 13.3" panel tops out around 200 KB, well within the 8 MB
/// PSRAM budget.
async fn fetch_image_bytes<H>(
    http: &mut H,
    token: &str,
    image: &paperanywhere_ports::ImageRef,
) -> Result<Vec<u8>, WakeError>
where
    H: HttpTransport,
{
    let mut buf: Vec<u8> = Vec::with_capacity(image.byte_len as usize);
    let result = http
        .stream_blob(token, &image.blob_url, &mut |chunk| {
            buf.extend_from_slice(chunk);
            Ok(())
        })
        .await;
    if let Err(e) = result {
        warn!("blob stream failed: {:?}", e);
        return Err(WakeError::BlobFetch);
    }
    let _ = image.byte_len.to_string(); // silence unused-import lint paths
    Ok(buf)
}

/// Emit the halt screen (BSOD-style) to the panel actor, then spin
/// forever. Marked `-> !`: anything after the call is unreachable.
/// Power-cycle / re-provision is the only recovery.
async fn halt_with_screen(
    paint: &'static PaintChannel,
    headline: &'static str,
    detail: &'static str,
    code: &'static str,
) -> ! {
    error!(
        "halt: {} Ã¢â‚¬â€ {} (code {}). device will not auto-recover; reset to retry.",
        headline, detail, code
    );
    paint::submit_silent(paint, PaintCmd::UpdateStatus(DeviceStatus::Halted)).await;
    paint::submit_silent(paint, PaintCmd::ShowHalt {
            headline,
            detail,
            code,
        })
        .await;
    // Slow heartbeat instead of spin_loop. spin_loop on this task
    // doesn't actively block the executor (other tasks are still
    // interrupt-scheduled), but it pegs the runtime task at 100% CPU
    // and prevents serial / dev-tools from seeing periodic life-signs.
    // A 30 s heartbeat keeps the device discoverable via the monitor
    // and ensures espflash reset / flash work cleanly without racing
    // CPU spin. Other tasks (embassy-net, panel actor) continue as
    // normal because they run on their own executors.
    let mut tick: u32 = 0;
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(30)).await;
        tick = tick.saturating_add(1);
        log::warn!(
            "halt tick #{}: {} ({}) — power-cycle or re-provision to recover",
            tick, headline, code
        );
    }
}

/// Decide which retry notice (if any) the adoption screen should show
/// this wake. Pure function over locally-observable state so the call
/// stays cheap; the message is plumbed straight into the rasterised
/// screen by [`paint_adoption_screen`].
///
/// Today's rules Ã¢â‚¬â€ refined as task #86 wires the actual register call:
///   * No IP yet         Ã¢â€ â€™ "Waiting for networkÃ¢â‚¬Â¦"
///   * IP but no cached
///     claim code        Ã¢â€ â€™ "Backend unreachable Ã¢â‚¬â€ retrying"
///   * Otherwise         Ã¢â€ â€™ `None` (clean state, fresh code on screen)
fn adoption_retry_notice<N>(ip: Option<&str>, nvs: &mut N) -> Option<&'static str>
where
    N: NvsStore,
{
    if ip.is_none() {
        return Some("Waiting for network -- check WiFi credentials");
    }
    if nvs.load_claim_code().is_none() {
        return Some("Asking the backend for a claim code...");
    }
    None
}

/// Emit a ShowAdoption command to the panel actor. Called when the
/// device is associated (has an IP) but has no `device_token` yet.
/// Reads the cached claim code from NVS Ã¢â‚¬â€ empty placeholder until task
/// #84 wires the backend's /api/device/claim-code/request endpoint.
async fn paint_adoption_screen<N, W>(
    paint: &'static PaintChannel,
    nvs: &mut N,
    wifi: &W,
    retry_notice: Option<&'static str>,
) -> paint::PaintHandle
where
    N: NvsStore,
    W: WifiLink,
{
    let claim_code = nvs
        .load_claim_code()
        .unwrap_or_else(|| alloc::string::String::from("(requestingÃ¢â‚¬Â¦)"));
    // Prefer the backend-assigned UUID for the device identifier slot:
    // it's the same value the dashboard's device row shows, so the
    // user can cross-reference at a glance. Fall back to `(unassigned)`
    // pre-register (the post-register re-paint will fill it in). We
    // explicitly do NOT fall back to a MAC- or token-derived `D-XXXX`
    // Ã¢â‚¬â€ that was misleading the user into thinking that string was
    // the device's canonical identity.
    let device_id = nvs
        .load_device_uuid()
        .unwrap_or_else(|| alloc::string::String::from("(unassigned)"));
    let ip = wifi
        .local_ip()
        .map(|ip| alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]))
        .unwrap_or_else(|| alloc::string::String::from("--"));
    // Backend URL from prov / NVS Ã¢â‚¬â€ see comment in single_wake_cycle.
    let base_url = nvs
        .load_backend_url()
        .unwrap_or_else(|| alloc::string::String::from("https://paperanywhere.io"));
    let mut adopt_url = alloc::string::String::new();
    adopt_url.push_str(base_url.trim_end_matches('/'));
    adopt_url.push_str("/adopt");

    info!(
        "adoption: claim_code={} device_id={} ip={} url={} notice={:?}",
        claim_code, device_id, ip, adopt_url, retry_notice
    );
    // Return the handle so callers can await the actual panel
    // refresh (the multi-second full-LUT cycle on UC8179) before
    // doing anything else. The adoption-before-register flow uses
    // this; other callers can drop the handle to fire-and-forget.
    paint::submit(
        paint,
        PaintCmd::ShowAdoption {
            claim_code: to_hstring(&claim_code),
            device_id: to_hstring(&device_id),
            ip: to_hstring(&ip),
            adopt_url: to_hstring(&adopt_url),
            retry_notice,
        },
    )
    .await
}

/// Poll `wifi.local_ip()` with a small back-off until it returns Some
/// or the cumulative wait exceeds the timeout. embassy_time::Timer is
/// safe here because we're inside an embassy_executor task Ã¢â‚¬â€
/// `embassy-time/generic-queue-8` means it doesn't even need the
/// executor's waker.
async fn wait_for_local_ip<W: WifiLink>(wifi: &W) -> Option<[u8; 4]> {
    // 150 Ãƒâ€” 100 ms = 15 s. DHCP usually completes inside ~3 s, but a
    // first-time association after boot can be slower Ã¢â‚¬â€ esp-radio + the
    // embassy-net DHCP client need to settle.
    for _ in 0..150 {
        if let Some(ip) = wifi.local_ip() {
            return Some(ip);
        }
        embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
    }
    wifi.local_ip()
}

/// HH:MM stamp of the current wall-clock, as a bounded heapless
/// String for transit through the paint channel. Returns `None` when
/// the device clock hasn't been NTP-synced yet (sleeper returns 0).
fn format_local_now<S: Sleeper>(sleeper: &S) -> Option<LastUpdateStr> {
    let now = sleeper.unix_now();
    if now == 0 {
        return None;
    }
    let hh = (now / 3600) % 24;
    let mm = (now / 60) % 60;
    let mut buf: LastUpdateStr = HString::new();
    // 5 chars ("HH:MM") fits in LastUpdateStr (HString<24>), so the
    // write cannot fail. Use core::fmt::Write::write_fmt rather than
    // format! so we don't pull in alloc for this hot path.
    use core::fmt::Write;
    let _ = write!(&mut buf, "{:02}:{:02}", hh, mm);
    Some(buf)
}

/// Copy an `alloc::String`-shaped input into a fixed-size
/// `heapless::String<N>` for paint-channel transit. Truncates if the
/// input overflows the target Ã¢â‚¬â€ every call site uses inputs whose
/// bounded length comes from a known schema (IPv4 quad, claim code,
/// adopt URL), so silent truncation is safer than panicking on
/// device.
fn to_hstring<const N: usize>(s: &str) -> HString<N> {
    let mut h: HString<N> = HString::new();
    // Walk char boundaries instead of raw bytes — a byte slice
    // `&s[..N]` panics if N lands inside a multi-byte UTF-8 char
    // (e.g. the `…` in our "(requesting…)" placeholder, the `—`
    // in adoption_retry_notice strings). That was the recurring
    // panic in single_wake_cycle the chrome-as-KV refactor surfaced.
    let cap = s.len().min(N);
    let safe_end = s
        .char_indices()
        .take_while(|(i, ch)| i + ch.len_utf8() <= cap)
        .last()
        .map(|(i, ch)| i + ch.len_utf8())
        .unwrap_or(0);
    let _ = h.push_str(&s[..safe_end]);
    h
}

/// Monotonic playlist page cursor. The runtime advances this on
/// every wake that paints a playlist page. Index wraps modulo the
/// playlist length at read time, so a playlist that shrinks (e.g.
/// user deleted a page) doesn't strand us pointing past the end.
static PLAYLIST_PAGE_INDEX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn next_playlist_page_index(total: usize) -> usize {
    let raw = PLAYLIST_PAGE_INDEX
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    raw % total.max(1)
}

/// Snapshot the values [`PaintCmd::ShowMain`] needs into a fresh
/// command. Reads owner + project from chrome (populated by /state).
/// Used both for the pre-/state initial paint (where owner/project
/// land as None/None and render as "--") and the post-/state re-paint
/// (where they're populated). Letting both call sites share one
/// builder keeps the placeholder's view contract in one place.
fn build_show_main_cmd<S: Sleeper>(ip: Option<&str>, sleeper: &S) -> PaintCmd {
    let ip_str: IpStr = ip.map(to_hstring).unwrap_or_else(IpStr::new);
    let last_update = format_local_now(sleeper);
    let snap = chrome::snapshot();
    let owner = snap.owner_email.as_deref().map(to_hstring);
    let project = snap.project_name.as_deref().map(to_hstring);
    PaintCmd::ShowMain {
        ip: ip_str,
        last_update,
        owner_email: owner,
        project_name: project,
    }
}

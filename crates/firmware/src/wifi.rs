//! WiFi station-mode association via `esp-radio` — concrete [`WifiLink`] impl
//! for the device.
//!
//! `esp-radio` 0.18 only exposes async APIs for connect/disconnect. We drive
//! them with `embassy_futures::block_on` so the polling main loop in
//! `paperanywhere-runtime` can stay blocking — the futures progress because
//! esp-radio fires its internal wakers from the WiFi interrupt context, which
//! `esp-rtos` keeps live via the timg0 tick driver.
//!
//! **Safety:** configures the WiFi peripheral only. No eFuse writes, no flash-
//! encryption setup, no secure-boot key handling. The factory MAC read for AP
//! naming is a *read* — fuses are never written from this firmware.

use alloc::string::String;

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use esp_hal::interrupt::software::SoftwareInterrupt;
use esp_hal::peripherals::{RNG, TIMG0, WIFI};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{
    Config, ControllerConfig, Interface as RadioInterface, WifiController, sta::StationConfig,
};
use paperanywhere_ports::{NvsStore, WifiCreds, WifiLink};

// The variants are read by the runtime via the derived `Debug` impl when
// logging WakeError contexts; `dead_code` doesn't see through `{:?}`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum WifiError {
    BadCreds,
    SetConfig,
    ConnectFailed,
    /// esp-radio's connect future didn't resolve within the bounded
    /// wait window. Either the AP is unreachable, creds are wrong,
    /// or the WPA handshake stalled. Caller logs + retries on the
    /// next wake instead of blocking forever.
    Timeout,
    /// WPA / WPA2 authentication was rejected by the AP. Either the
    /// password is wrong, MAC filtering blocked us, or the AP's
    /// 4-way handshake didn't complete (FourWayHandshakeTimeout).
    /// The runtime turns this into a BSOD-style halt — there's no
    /// point retrying when the AP won't accept us at the WPA layer.
    AuthFailed,
}

/// Owns the active controller after a successful `init`. Implements
/// [`WifiLink`] so the generic runtime can drive it without knowing it's
/// `esp-radio` underneath.
pub struct FwWifi {
    controller: WifiController<'static>,
    /// Embassy-net stack handle, set post-construction by
    /// [`FwWifi::attach_stack`] once the IP layer has been wired up.
    /// Used by [`WifiLink::local_ip`] to surface the DHCP-assigned
    /// address into the status bar + the dev HTTP server's /info JSON.
    /// `None` until attached.
    stack: Option<&'static embassy_net::Stack<'static>>,
    /// Local "we already associated successfully" cache. Lets
    /// subsequent `associate` calls short-circuit so we don't tear
    /// down a working WPA session by calling `set_config` again.
    /// `is_connected` would be the canonical signal but it's behind
    /// esp-radio's `unstable` feature; tracking it ourselves keeps
    /// the dep surface minimal.
    associated: bool,
    /// Latches the first time DHCP gives us an IP. We only treat a
    /// subsequent "stack has no IP" observation as a real drop (and
    /// force a re-associate) AFTER this flag is set. Without it, the
    /// first few wake cycles — when DHCP hasn't yet completed —
    /// would look like a "broken" association and we'd tear down the
    /// freshly-established WPA session before DHCP got a chance to
    /// finish, leaving the device stuck in a connect / disconnect /
    /// reconnect loop that never makes progress past the boot screen.
    had_ip_in_session: bool,
    /// SSID we most recently called `set_config` for. Keys into the
    /// per-SSID DHCP-lease cache in NVS so we can fall back to a
    /// static config on DHCP timeout (UniFi / AP bridge-table glitches
    /// have been observed where a device that already has a valid
    /// lease can't re-DHCP after a soft reset).
    current_ssid: alloc::string::String,
}

impl FwWifi {
    /// Latch onto an embassy-net stack so [`WifiLink::local_ip`] can
    /// query it. Called once from boot.rs after `network::build`
    /// produces the stack.
    pub fn attach_stack(&mut self, stack: &'static embassy_net::Stack<'static>) {
        self.stack = Some(stack);
    }

    /// Wait for embassy-net to acquire an IPv4 config (DHCP). Returns
    /// `true` if DHCP succeeded.
    ///
    /// Saves a fresh DHCP lease to NVS on success (for future
    /// observability / out-of-tree static-fallback consumers).
    ///
    /// Note: the static-lease *consumption* path was disabled
    /// 2026-05-22 — empirically the fallback claimed stale leases on
    /// networks with the same SSID but different subnets, leaving the
    /// device with an IP no one on the LAN routes to. The save side
    /// stays because it's harmless and useful for diagnostics. If
    /// you genuinely need the fallback for an air-gapped same-network
    /// reboot scenario, gate it on a separate opt-in flag, not on
    /// every DHCP timeout.
    ///
    /// `unsafe`-via-AtomicPtr access to FwNvs follows the same handoff
    /// pattern as `boot::flash_persist_hook` — see NVS_HANDOFF
    /// in boot.rs for the SAFETY contract.
    pub async fn wait_for_ip_or_fallback(
        &mut self,
        dhcp_timeout: Duration,
    ) -> bool {
        let stack = match self.stack {
            Some(s) => s,
            None => return false,
        };
        // Fast path: DHCP came back already.
        if stack.config_v4().is_some() {
            if let Some(cfg) = stack.config_v4() {
                self.cache_lease(&cfg);
            }
            return true;
        }
        // Race DHCP against the timeout.
        let dhcp_result = embassy_futures::select::select(
            stack.wait_config_up(),
            Timer::after(dhcp_timeout),
        )
        .await;
        match dhcp_result {
            embassy_futures::select::Either::First(()) => {
                if let Some(cfg) = stack.config_v4() {
                    self.cache_lease(&cfg);
                }
                true
            }
            embassy_futures::select::Either::Second(()) => {
                // Static-lease fallback intentionally disabled. The cache
                // was claiming stale IPs on networks with the same SSID
                // but different subnets, which left the device with an
                // IP no one on the LAN routes to. The save side stays
                // (cache_lease above) so out-of-tree code can opt in.
                log::warn!(
                    "wifi: DHCP timed out on \"{}\" — wake will retry on next cycle",
                    self.current_ssid
                );
                false
            }
        }
    }

    /// Override the station MAC with a randomized one (Espressif OUI
    /// preserved, lower 3 bytes from the HW TRNG). Logs the new MAC
    /// + readback for verification. Call between `esp_radio::wifi::new`
    /// and the first `set_config` — that's the window ESP-IDF allows
    /// `esp_wifi_set_mac` in. Returns silently on any failure (we
    /// fall back to the eFuse MAC).
    ///
    /// SAFETY: calls into the esp-wifi-sys FFI which is unsafe by
    /// nature. The MAC buffer is on the stack and outlives the call.
    /// The set_mode → set_mac → get_mac sequence matches the ESP-IDF
    /// API contract.
    fn rotate_station_mac() {
        let mut new_mac = [0u8; 6];
        new_mac[0] = 0x44;
        new_mac[1] = 0x1b;
        new_mac[2] = 0xf6;
        let mut rng = esp_hal::rng::Rng::new();
        let r = rand_core::RngCore::next_u32(&mut rng);
        new_mac[3] = (r >> 16) as u8;
        new_mac[4] = (r >> 8) as u8;
        new_mac[5] = r as u8;
        log::info!(
            "wifi: rotating station MAC to {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (was eFuse default)",
            new_mac[0], new_mac[1], new_mac[2], new_mac[3], new_mac[4], new_mac[5]
        );
        unsafe {
            let _ = esp_wifi_sys_esp32s3::include::esp_wifi_set_mode(
                esp_wifi_sys_esp32s3::include::wifi_mode_t_WIFI_MODE_STA,
            );
            let rc = esp_wifi_sys_esp32s3::include::esp_wifi_set_mac(
                esp_wifi_sys_esp32s3::include::wifi_interface_t_WIFI_IF_STA,
                new_mac.as_ptr(),
            );
            if rc != esp_wifi_sys_esp32s3::include::ESP_OK as i32 {
                log::warn!(
                    "wifi: esp_wifi_set_mac returned err {} — using eFuse MAC",
                    rc
                );
            } else {
                let mut readback = [0u8; 6];
                let rc2 = esp_wifi_sys_esp32s3::include::esp_wifi_get_mac(
                    esp_wifi_sys_esp32s3::include::wifi_interface_t_WIFI_IF_STA,
                    readback.as_mut_ptr(),
                );
                log::info!(
                    "wifi: esp_wifi_get_mac readback rc={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    rc2, readback[0], readback[1], readback[2],
                    readback[3], readback[4], readback[5],
                );
            }
        }
    }

    /// Forcibly tear down the current association and re-join the same
    /// SSID. Used by the HTTP retry-on-blackhole path: when the AP is
    /// silently dropping our unicast frames post-DHCP (UniFi bridge-
    /// table glitch — see [`project_register_unifi_blackhole`] memory),
    /// a deauth-then-reassociate kicks the AP into re-learning our MAC
    /// + repopulating its forwarding table.
    ///
    /// No-op if we haven't successfully associated yet (no SSID to
    /// reuse). Returns the result of the re-`associate` call.
    pub async fn force_reconnect(&mut self) -> Result<(), WifiError> {
        if self.current_ssid.is_empty() {
            log::warn!("wifi: force_reconnect called before first associate — skipping");
            return Err(WifiError::ConnectFailed);
        }
        log::warn!(
            "wifi: force_reconnect — disconnecting + re-associating to {:?}",
            self.current_ssid
        );
        // disconnect_async, then clear the sticky associated flag so
        // associate() doesn't short-circuit.
        let _ = self.controller.disconnect_async().await;
        self.associated = false;
        // Re-read creds from NVS so we don't have to cache a password
        // copy on the heap inside FwWifi.
        let nvs_raw = crate::boot::NVS_HANDOFF
            .load(core::sync::atomic::Ordering::Acquire);
        if nvs_raw.is_null() {
            return Err(WifiError::BadCreds);
        }
        let nvs: &crate::nvs::FwNvs = unsafe { &*nvs_raw };
        let creds = match nvs.load_wifi_creds() {
            Some(c) => c,
            None => return Err(WifiError::BadCreds),
        };
        self.associate(&creds).await
    }

    fn cache_lease(&self, cfg: &embassy_net::StaticConfigV4) {
        if self.current_ssid.is_empty() {
            return;
        }
        let ip_octets = cfg.address.address().octets();
        let gateway = cfg.gateway.map(|g| g.octets());
        let dns = cfg.dns_servers.first().map(|d| d.octets());
        let lease = crate::nvs::CachedLease {
            ssid: self.current_ssid.clone(),
            ipv4: ip_octets,
            prefix: cfg.address.prefix_len(),
            gateway,
            dns,
        };
        crate::wifi::save_cached_lease(&lease);
    }
}

impl FwWifi {
    /// Boot the radio. Hands esp-rtos a timer + sw-interrupt for its
    /// scheduler, then hands esp-radio the WIFI peripheral. Radio init is
    /// implicit inside `wifi::new` (it grabs a `RadioRefGuard` internally).
    ///
    /// Takes the four peripherals it owns directly — they come from
    /// [`crate::resources::FirmwareResources`] which `boot::run` destructures.
    pub fn init(
        timg0: TIMG0<'static>,
        sw_int0: SoftwareInterrupt<'static, 0>,
        rng: RNG<'static>,
        wifi: WIFI<'static>,
    ) -> Result<(Self, RadioInterface<'static>), WifiError> {
        let timg0 = TimerGroup::new(timg0);
        // esp-rtos 0.3 needs both a tick timer and software interrupt 0 for
        // its context-switch trampoline. timer1 stays free for other code.
        esp_rtos::start(timg0.timer0, sw_int0);

        // The RNG handle is parameterless on esp-hal 1.x. The peripheral
        // handle is consumed so it can't be re-grabbed elsewhere; esp-radio
        // reads from the hardware RNG through its own driver path.
        let _keep_rng = rng;

        let (controller, interfaces) =
            esp_radio::wifi::new(wifi, ControllerConfig::default())
                .map_err(|_| WifiError::ConnectFailed)?;

        // ── MAC rotation (task #127, OFF by default) ──────────────
        // The plumbing is in place: rotate_station_mac() calls
        // esp_wifi_set_mac via the FFI and verifies via get_mac.
        // BUT — empirically validated on the Foxpaws Network UniFi:
        // rotated MACs cannot get a DHCP lease (UniFi has per-MAC
        // allow-listing / DHCP fingerprinting that only honours
        // specific known MACs). The original eFuse MAC was on the
        // allow-list; randomized ones are silently dropped at DHCP
        // DISCOVER. Useful elsewhere; not on this network.
        //
        // Toggle this at the call site (e.g. via a future provtool
        // flag or NVS bit) when the deployment is known to live on
        // a network that auto-trusts any MAC.
        const ROTATE_MAC: bool = false;
        if ROTATE_MAC {
            Self::rotate_station_mac();
        }

        // NOTE on WiFi power-save: esp-radio 0.18 defaults to
        // PowerSaveMode::None (no modem sleep), which is what we want
        // here. We'd pin it explicitly but `Controller::set_power_saving`
        // is gated behind esp-radio's `unstable` feature; turning that
        // on pulls in the wider unstable surface so we leave it on
        // the documented default instead. If a future version flips
        // the default, the symptom (router timing us out during long
        // refreshes despite yield_now in the panel driver) would
        // recur — re-pin via the unstable API at that point.

        // Returned: the controller (used for associate/disconnect calls from
        // the runtime) plus the station interface, which the network module
        // hands to embassy-net to build its TCP/IP stack.
        Ok((
            Self {
                controller,
                stack: None,
                associated: false,
                had_ip_in_session: false,
                current_ssid: alloc::string::String::new(),
            },
            interfaces.station,
        ))
    }
}

impl WifiLink for FwWifi {
    type Error = WifiError;

    async fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error> {
        // Already-connected short-circuit BUT only when we can prove
        // we're still actually on the network. The bug we fixed here:
        // a one-way `associated = true` flag is sticky across radio
        // dropouts — if the AP momentarily disappears, or a brown-out
        // during an OTA push trips the WiFi stack, this function used
        // to short-circuit forever thinking we were still connected
        // and the device went permanently dead to the network until a
        // physical reset. Cross-checking against `embassy-net`'s
        // config_v4 (which tracks the live DHCP lease) means a stale
        // `associated` flag triggers a real re-associate — but ONLY
        // once we've actually had an IP this session, so we don't
        // tear down a freshly-established WPA session while DHCP is
        // still running in the background.
        let stack_has_ip = self.stack.and_then(|s| s.config_v4()).is_some();
        if stack_has_ip {
            self.had_ip_in_session = true;
        }
        if self.associated && stack_has_ip {
            return Ok(());
        }
        if self.associated && !stack_has_ip && self.had_ip_in_session {
            log::warn!(
                "wifi: lost DHCP lease after previously having one — forcing re-associate"
            );
            self.associated = false;
            // esp-radio gets confused if we set_config without an
            // explicit disconnect first; this is idempotent.
            let _ = self.controller.disconnect_async().await;
        }
        if self.associated {
            // associated && !stack_has_ip && !had_ip_in_session:
            // we joined the AP successfully but DHCP hasn't completed
            // yet on this session. Don't disconnect — let DHCP keep
            // trying. The wake-cycle's wait_for_local_ip will keep
            // polling for the IP; if it never arrives, DhcpTimeout
            // flows through to the outer-loop retry counter.
            return Ok(());
        }

        let mut pw = String::new();
        pw.push_str(creds.password.as_str());

        let cfg = Config::Station(
            StationConfig::default()
                .with_ssid(creds.ssid.as_str())
                .with_password(pw),
        );
        // `set_config` performs an implicit `esp_wifi_start` when mode goes
        // NULL → STA, so there's no separate `start` to call.
        self.controller.set_config(&cfg).map_err(|_| WifiError::SetConfig)?;
        // Race the connect future against a 25 s timeout. Now that
        // associate is async (no `block_on`), the embassy executor
        // can schedule the time driver between connect polls, so the
        // Timer actually fires.
        let result = select(
            self.controller.connect_async(),
            Timer::after(Duration::from_secs(25)),
        )
        .await;
        match result {
            Either::First(Ok(_info)) => {
                self.associated = true;
                self.current_ssid.clear();
                self.current_ssid.push_str(creds.ssid.as_str());
                // Best-effort wait for DHCP (15 s). On timeout, fall
                // back to a cached static lease if available. Ignore
                // the boolean — the runtime's wait_for_local_ip will
                // surface the no-IP failure via its own retry path,
                // and we don't want a missing cached-lease to look
                // like a fresh association failure.
                let _ = self
                    .wait_for_ip_or_fallback(Duration::from_secs(30))
                    .await;
                Ok(())
            }
            Either::First(Err(e)) => {
                log::error!("wifi: esp-radio connect_async err: {:?}", e);
                // Map WPA / authentication failures to AuthFailed so
                // the runtime knows to halt with BSOD instead of
                // retrying forever. esp-radio surfaces these as a
                // Disconnected event with a reason code:
                //   - AuthenticationFailed: AP outright rejected creds
                //   - FourWayHandshakeTimeout: AP didn't finish the WPA
                //     handshake (bad password, MAC ACL, or rate limit)
                //   - HandshakeTimeout: same family
                //   - AssocFail / AssocExpire: AP refused association
                let is_auth = matches!(
                    &e,
                    esp_radio::wifi::WifiError::Disconnected(info)
                    if matches!(
                        info.reason,
                        esp_radio::wifi::DisconnectReason::AuthenticationFailed
                            | esp_radio::wifi::DisconnectReason::AuthenticationExpired
                            | esp_radio::wifi::DisconnectReason::FourWayHandshakeTimeout
                            | esp_radio::wifi::DisconnectReason::HandshakeTimeout
                            | esp_radio::wifi::DisconnectReason::AssociationFailed
                            | esp_radio::wifi::DisconnectReason::_802_1xAuthenticationFailed
                            | esp_radio::wifi::DisconnectReason::MicFailure
                    )
                );
                if is_auth {
                    Err(WifiError::AuthFailed)
                } else {
                    Err(WifiError::ConnectFailed)
                }
            }
            Either::Second(_) => Err(WifiError::Timeout),
        }
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        // Idempotent — esp-radio returns NotConnected if we're already down.
        let _ = embassy_futures::block_on(self.controller.disconnect_async());
        self.associated = false;
        Ok(())
    }

    fn rssi_dbm(&self) -> Option<i16> {
        // Public RSSI access lives behind esp-radio's `unstable` feature
        // (the dashboard heartbeat would want a real value when we
        // wire that up). For the status-bar wifi icon's connected /
        // disconnected indicator the boolean is what matters, so we
        // synthesise a placeholder dBm when we know we associated.
        // `None` ⇒ slashed icon; `Some(_)` ⇒ signal-bars icon.
        if self.associated {
            Some(-50)
        } else {
            None
        }
    }

    fn local_ip(&self) -> Option<[u8; 4]> {
        let stack = self.stack?;
        let cfg = stack.config_v4()?;
        Some(cfg.address.address().octets())
    }

    fn is_auth_error(err: &Self::Error) -> bool {
        matches!(err, WifiError::AuthFailed | WifiError::BadCreds)
    }

    fn gateway_v4(&self) -> Option<[u8; 4]> {
        let stack = self.stack?;
        let cfg = stack.config_v4()?;
        // embassy-net 0.7 returns the gateway as Option<Ipv4Address>
        // — None when DHCP didn't include one. Forward as-is.
        cfg.gateway.map(|g| g.octets())
    }
}

/// Look up a cached DHCP lease for `ssid`. Returns `None` if NVS hasn't
/// been wired in yet, or no lease exists for this SSID.
///
/// SAFETY: dereferences the global NVS pointer published by
/// [`crate::boot::NVS_HANDOFF`]. The pointer is set once at boot before
/// any task starts; reads are Acquire-ordered. After publication the
/// pointed-to FwNvs lives for the entire program lifetime.
pub(crate) fn load_cached_lease(ssid: &str) -> Option<crate::nvs::CachedLease> {
    let raw = crate::boot::NVS_HANDOFF
        .load(core::sync::atomic::Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // Read-only access here — safe to take a shared reference even if
    // another task holds a separate &mut, since save_* methods that
    // mutate are called from the same single-threaded async runtime.
    let nvs: &crate::nvs::FwNvs = unsafe { &*raw };
    nvs.load_wifi_lease(ssid)
}

/// Persist a successful DHCP lease for future fallback. Same SAFETY
/// contract as [`load_cached_lease`].
pub(crate) fn save_cached_lease(lease: &crate::nvs::CachedLease) {
    let raw = crate::boot::NVS_HANDOFF
        .load(core::sync::atomic::Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    let nvs: &mut crate::nvs::FwNvs = unsafe { &mut *raw };
    nvs.save_wifi_lease(lease);
}

/// Force a WiFi reassociate via the module-level FwWifi handoff.
/// Called from the HTTP retry-on-blackhole path. SAFETY: same as
/// [`save_cached_lease`] — relies on the boot-time-published pointer
/// in [`crate::boot::WIFI_HANDOFF_GLOBAL`].
pub(crate) async fn force_reconnect_via_handoff() -> Result<(), WifiError> {
    let raw = crate::boot::WIFI_HANDOFF_GLOBAL
        .load(core::sync::atomic::Ordering::Acquire);
    if raw.is_null() {
        log::warn!("wifi: force_reconnect: WIFI handoff not yet published");
        return Err(WifiError::ConnectFailed);
    }
    let wifi: &mut FwWifi = unsafe { &mut *raw };
    wifi.force_reconnect().await
}

/// Captive-portal AP for first-time provisioning. Hosts an HTTP server on
/// 192.168.4.1, captures `ssid` + `password` from a form, writes them to NVS.
///
/// M4 stub — needs an external embassy-net stack to handle the HTTP serving
/// (esp-radio gives us L2; we need an IP stack on top).
pub fn captive_portal() -> Result<(), WifiError> {
    esp_println::println!("wifi: captive portal stub (needs IP stack)");
    Err(WifiError::ConnectFailed)
}

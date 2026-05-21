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
use paperanywhere_ports::{WifiCreds, WifiLink};

// The variants are read by the runtime via the derived `Debug` impl when
// logging WakeError contexts; `dead_code` doesn't see through `{:?}`.
#[allow(dead_code)]
#[derive(Debug)]
pub enum WifiError {
    BadCreds,
    SetConfig,
    ConnectFailed,
    /// esp-radio's connect future didn't resolve within the bounded
    /// wait window. Either the AP is unreachable, creds are wrong,
    /// or the WPA handshake stalled. Caller logs + retries on the
    /// next wake instead of blocking forever.
    Timeout,
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
}

impl FwWifi {
    /// Latch onto an embassy-net stack so [`WifiLink::local_ip`] can
    /// query it. Called once from boot.rs after `network::build`
    /// produces the stack.
    pub fn attach_stack(&mut self, stack: &'static embassy_net::Stack<'static>) {
        self.stack = Some(stack);
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

        let (controller, interfaces) = esp_radio::wifi::new(wifi, ControllerConfig::default())
            .map_err(|_| WifiError::ConnectFailed)?;

        // Returned: the controller (used for associate/disconnect calls from
        // the runtime) plus the station interface, which the network module
        // hands to embassy-net to build its TCP/IP stack.
        Ok((Self { controller, stack: None, associated: false }, interfaces.station))
    }
}

impl WifiLink for FwWifi {
    type Error = WifiError;

    async fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error> {
        // Already-connected short-circuit. Avoids the `set_config +
        // connect_async` dance on every wake cycle — that was what
        // turned the first cycle's working association into Timeout
        // errors on cycles 2+ (the second set_config tore down the
        // existing STA state).
        if self.associated {
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
                Ok(())
            }
            Either::First(Err(_)) => Err(WifiError::ConnectFailed),
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

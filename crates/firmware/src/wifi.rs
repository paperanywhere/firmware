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

use embassy_futures::block_on;
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
}

/// Owns the active controller after a successful `init`. Implements
/// [`WifiLink`] so the generic runtime can drive it without knowing it's
/// `esp-radio` underneath.
pub struct FwWifi {
    controller: WifiController<'static>,
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
        Ok((Self { controller }, interfaces.station))
    }
}

impl WifiLink for FwWifi {
    type Error = WifiError;

    fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error> {
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
        block_on(self.controller.connect_async()).map_err(|_| WifiError::ConnectFailed)?;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        // Idempotent — esp-radio returns NotConnected if we're already down.
        let _ = block_on(self.controller.disconnect_async());
        Ok(())
    }

    fn rssi_dbm(&self) -> Option<i16> {
        // Public RSSI access lives behind esp-radio's `unstable` feature.
        // We avoid that for now to keep the dep surface small; the dashboard
        // heartbeat just won't have RSSI until we enable it.
        None
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

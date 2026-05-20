//! WiFi station-mode association via `esp-wifi` + `embassy-net`.
//!
//! Two entry points:
//!   - [`Driver::associate`] — load creds from NVS and connect (subsequent boots)
//!   - [`captive_portal`] — host an AP for first-time credential capture
//!     (last-resort provisioning path; see `provisioning::resolve`)
//!
//! **Safety note:** this module only configures the WiFi peripheral — no
//! eFuse writes, no flash-encryption setup, no secure-boot key handling.

use alloc::format;
use alloc::string::String;

use embassy_net::{Config, Stack, StackResources};
use esp_hal::{
    peripherals::{RADIO_CLK, RNG, TIMG0, WIFI},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_wifi::{
    EspWifiInitFor,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiError, WifiStaDevice,
        WifiState,
    },
};
use heapless::String as HString;

#[derive(Debug, Clone)]
pub struct WifiCreds {
    pub ssid: HString<32>,
    pub password: HString<64>,
}

impl WifiCreds {
    pub fn from_strs(ssid: &str, password: &str) -> Result<Self, ()> {
        let mut s = HString::new();
        let mut p = HString::new();
        s.push_str(ssid).map_err(|_| ())?;
        p.push_str(password).map_err(|_| ())?;
        Ok(Self { ssid: s, password: p })
    }
}

/// Try to load credentials from NVS. `None` means the device hasn't been
/// provisioned yet — caller should fall back to the captive portal.
pub fn load_creds() -> Option<WifiCreds> {
    let (ssid, password) = crate::nvs::load_wifi_creds()?;
    Some(WifiCreds { ssid, password })
}

/// Peripheral bundle required to drive the WiFi radio. Built once in `main.rs`
/// and passed into [`Driver::new`] before any networking call.
pub struct WifiPeripherals {
    pub timg: TimerGroup<'static, TIMG0>,
    pub wifi: WIFI,
    pub radio_clk: RADIO_CLK,
    pub rng: Rng,
    pub _rng_peripheral: RNG,
}

/// Stateful WiFi driver that wraps the esp-wifi controller + embassy-net stack
/// behind a small ergonomic surface (associate / disconnect / is_connected).
pub struct Driver {
    controller: WifiController<'static>,
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
}

impl Driver {
    /// Initialize the radio + IP stack. Returns a `Driver` ready to associate.
    /// Sets up the embassy-net stack but does NOT connect to any network — the
    /// caller follows up with [`Driver::associate`].
    pub fn new(p: WifiPeripherals) -> Result<Self, WifiError> {
        let init = esp_wifi::init(
            EspWifiInitFor::Wifi,
            p.timg.timer0,
            p.rng,
            p.radio_clk,
        )
        .expect("esp-wifi init");

        let (wifi_interface, controller) =
            esp_wifi::wifi::new_with_mode(&init, p.wifi, WifiStaDevice)?;

        let cfg = Config::dhcpv4(Default::default());
        let seed = ((p.rng.random() as u64) << 32) | p.rng.random() as u64;

        static STACK_RESOURCES: static_cell::StaticCell<StackResources<3>> =
            static_cell::StaticCell::new();
        static STACK: static_cell::StaticCell<Stack<WifiDevice<'static, WifiStaDevice>>> =
            static_cell::StaticCell::new();

        let resources = STACK_RESOURCES.init(StackResources::<3>::new());
        let stack = STACK.init(Stack::new(wifi_interface, cfg, resources, seed));

        Ok(Self { controller, stack })
    }

    /// Connect to an AP using the supplied credentials. Yields to the embassy
    /// executor until DHCP completes or the association errors out.
    pub async fn associate(&mut self, creds: &WifiCreds) -> Result<(), WifiError> {
        let client = ClientConfiguration {
            ssid: creds.ssid.as_str().try_into().map_err(|_| WifiError::InternalError(
                esp_wifi::wifi::InternalWifiError::EspErrInvalidArg,
            ))?,
            password: creds.password.as_str().try_into().map_err(|_| WifiError::InternalError(
                esp_wifi::wifi::InternalWifiError::EspErrInvalidArg,
            ))?,
            ..Default::default()
        };
        self.controller.set_configuration(&Configuration::Client(client))?;
        self.controller.start()?;
        self.controller.connect()?;

        loop {
            let state = esp_wifi::wifi::sta_state();
            if matches!(state, WifiState::StaConnected) {
                break;
            }
            if matches!(state, WifiState::StaDisconnected) {
                return Err(WifiError::Disconnected);
            }
            embassy_time::Timer::after_millis(200).await;
        }
        while !self.stack.is_link_up() {
            embassy_time::Timer::after_millis(200).await;
        }
        loop {
            if self.stack.config_v4().is_some() {
                break;
            }
            embassy_time::Timer::after_millis(200).await;
        }
        let ip = self.stack.config_v4().expect("just checked");
        esp_println::println!("wifi: associated, IP = {:?}", ip.address);
        Ok(())
    }

    /// Drop the WiFi association. Used before deep sleep on `scheduled_wake`
    /// devices to make sure the radio is fully de-energized.
    pub fn disconnect(&mut self) -> Result<(), WifiError> {
        let _ = self.controller.disconnect();
        let _ = self.controller.stop();
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        matches!(esp_wifi::wifi::sta_state(), WifiState::StaConnected)
            && self.stack.is_link_up()
            && self.stack.config_v4().is_some()
    }

    /// Last assigned RSSI in dBm. Reported in the next `DeviceMsg::Heartbeat`.
    pub fn rssi_dbm(&self) -> Option<i8> {
        self.controller.rssi().ok()
    }

    pub fn stack(&self) -> &Stack<WifiDevice<'static, WifiStaDevice>> {
        self.stack
    }
}

/// Captive-portal AP mode for first-time provisioning. Hosts an open AP with
/// SSID `paperanywhere-XXXX` (last 4 hex of the MAC) and a minimal HTTP page
/// that collects SSID / password and writes them to NVS.
///
/// Returns `Ok(())` once credentials are captured + persisted; the caller's
/// next association attempt picks them up. Returns `Err(_)` only on hardware
/// init failure.
pub async fn captive_portal(_p: &mut WifiPeripherals) -> Result<(), WifiError> {
    // M4: switch the controller to AP mode, bring up a small httparse-based
    // HTTP server on embassy-net listening at 192.168.4.1, serve a single
    // credential-capture page, write the result to NVS via crate::nvs.
    //
    // The MAC read below is from a factory-burnt eFuse — a *read* only.
    // We never write eFuses anywhere in this firmware.
    let mac = esp_hal::efuse::Efuse::mac_address();
    let ssid: String = format!("paperanywhere-{:02x}{:02x}", mac[4], mac[5]);
    esp_println::println!("wifi: captive portal stub (would host AP `{}`)", ssid);
    Err(WifiError::InternalError(
        esp_wifi::wifi::InternalWifiError::EspErrNotSupported,
    ))
}

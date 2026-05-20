//! WiFi station-mode association — blocking surface (no async).
//!
//! When the esp-wifi 0.15+ vs esp-hal 1.1 private-feature mismatch resolves
//! upstream, this module gets a real impl that:
//!   1. Initialises esp-wifi with the radio peripherals.
//!   2. Calls `controller.set_configuration(ClientConfiguration{ssid,password,...})`.
//!   3. Calls `controller.start()` then `controller.connect()` (blocking).
//!   4. Spins on `controller.is_connected()` + DHCP via smoltcp until ready.
//!
//! For now the type surface matches what `boot.rs` calls into.
//!
//! **Safety note:** this module only configures the WiFi peripheral. It does
//! NOT touch eFuses, flash encryption, or any one-time-programmable hardware
//! state. Reading the factory-burnt MAC for the captive-portal AP name is a
//! read, not a write — that's the only fuse access anywhere.

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

pub fn load_creds() -> Option<WifiCreds> {
    let (ssid, password) = crate::nvs::load_wifi_creds()?;
    Some(WifiCreds { ssid, password })
}

#[derive(Debug)]
pub enum WifiError {
    NotImplemented,
    HardwareInit,
    BadCreds,
    AssociateTimeout,
    DhcpTimeout,
}

pub struct Driver {
    _marker: core::marker::PhantomData<()>,
}

impl Driver {
    pub fn new() -> Result<Self, WifiError> {
        // M4: esp_wifi::init(timer, rng, radio_clk) + esp_wifi::wifi::new_with_mode(WifiStaDevice)
        Err(WifiError::NotImplemented)
    }

    pub fn associate(&mut self, _creds: &WifiCreds) -> Result<(), WifiError> {
        // M4: blocking associate + DHCP via smoltcp.
        Err(WifiError::NotImplemented)
    }

    pub fn disconnect(&mut self) -> Result<(), WifiError> {
        Ok(())
    }

    pub fn rssi_dbm(&self) -> Option<i8> {
        None
    }
}

/// Captive-portal AP for first-time provisioning. Hosts an HTTP server on
/// 192.168.4.1, captures `ssid` + `password` from a form, writes them to NVS.
///
/// M4 stub.
pub fn captive_portal() -> Result<(), WifiError> {
    esp_println::println!("wifi: captive portal stub");
    Err(WifiError::NotImplemented)
}

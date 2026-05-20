//! WiFi association + captive-portal AP for first-time provisioning.

pub struct WifiCreds {
    pub ssid: heapless::String<32>,
    pub password: heapless::String<64>,
}

pub fn load_creds() -> Option<WifiCreds> {
    // M4: read from NVS.
    None
}

pub fn associate(_creds: &WifiCreds) -> Result<(), ()> {
    // M4: esp-wifi station mode + embassy-net.
    Err(())
}

pub fn captive_portal() {
    // M4: AP mode, HTTP server serving a credential form.
}

//! Local wire-type stubs to replace paperanywhere-proto temporarily.
//!
//! paperanywhere-proto pulls serde, and a target-vs-host feature-unification
//! trap with esp-hal-procmacros' serde_yaml dep was blocking firmware CI.
//! These stubs cover just enough of the protocol surface that the rest of the
//! firmware compiles; the real types come back from paperanywhere-proto once
//! the upstream fix lands.

use heapless::String;

#[derive(Debug, Clone)]
pub struct ProvBlob {
    pub ssid: alloc::string::String,
    pub password: alloc::string::String,
    pub backend_url: Option<alloc::string::String>,
    pub claim_code: Option<alloc::string::String>,
}

#[allow(dead_code)]
pub type WifiSsid = String<32>;
#[allow(dead_code)]
pub type WifiPassword = String<64>;

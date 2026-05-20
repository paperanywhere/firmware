//! Persisted device state, written to the `nvs` partition declared in
//! `flash/partition-table.csv` (24 KB starting at 0x9000).
//!
//! Fields stored:
//!
//!   - device_id (UUID, 16 bytes) — issued by /api/device/claim
//!   - device_token_hex (≤64 chars) — bearer token for /state, /ack, /blob
//!   - wifi.ssid + wifi.password — credentials for STA mode
//!   - backend_url (≤128 chars) — optional override from the prov bundle
//!   - claim_code_pending (≤16 chars) — only set if the prov bundle carries one
//!   - last_applied_image_id — to skip re-rendering on duplicate /state responses
//!
//! Layout is a single fixed-record 512-byte struct at the start of the partition,
//! CRC32-validated. M4 hardening pass should rotate between two sectors with a
//! generation counter for crash-safety; today's failure mode on power-loss
//! mid-write is "fall back to captive portal on next boot" — annoying but not
//! permanent damage.

use heapless::String as HString;

use crate::boards::BoardConfig;

// ── M4 stubs — bodies plug into esp-storage once its matrix lines up ──

pub fn load_wifi_creds() -> Option<(HString<32>, HString<64>)> {
    None
}

pub fn save_wifi_creds(_ssid: &str, _password: &str) {}

pub fn load_backend_url() -> Option<HString<128>> {
    None
}

pub fn save_backend_url(_url: &str) {}

pub fn load_device_token() -> Option<HString<64>> {
    None
}

pub fn save_device_token(_token_hex: &str) {}

pub fn load_pending_claim_code() -> Option<HString<16>> {
    None
}

pub fn save_pending_claim_code(_code: &str) {}

pub fn clear_pending_claim_code() {}

pub fn load_last_applied_image_id() -> Option<HString<32>> {
    None
}

pub fn save_last_applied_image_id(_image_id: &str) {}

/// Wipe NVS — called on long-press factory reset. Drops device_id + token + WiFi.
/// Next cold boot enters the provisioning resolver from scratch.
pub fn factory_reset() {
    esp_println::println!("nvs: factory reset (would wipe NVS partition)");
}

/// Stub claim flow. Real impl renders a 6-char Crockford base32 code on the panel,
/// hosts a WiFi captive portal, then POSTs to /api/device/claim once credentials
/// arrive. Blocks until the dashboard consumes the code.
pub fn claim_flow_stub(_board: BoardConfig) {
    esp_println::println!("nvs: claim_flow_stub — render code, capture WiFi");
}

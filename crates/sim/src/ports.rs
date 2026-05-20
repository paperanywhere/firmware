//! Concrete port implementations for the simulator.
//!
//! Each port writes into the shared [`SimState`] so the egui UI can show
//! what's happening — status text, recent activity feed, mock battery/RSSI.

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use log::{debug, info, warn};
use paperanywhere_ports::{
    DeviceAck, DeviceState, FirmwareUpdate, FirmwareUpdater, HttpTransport, NvsStore, PowerPolicy,
    Sleeper, WifiCreds, WifiLink, parse_device_state,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::SimConfig;
use crate::state::SimState;

// ── WiFi (mock) ───────────────────────────────────────────────────────────────

/// No actual WiFi association on the host — the network stack is already up.
/// We just narrate the lifecycle into the UI and decrement the mock RSSI a
/// touch to simulate signal jitter, so the side panel doesn't look frozen.
pub struct SimWifi {
    state: Arc<SimState>,
}

impl SimWifi {
    pub fn new(state: Arc<SimState>) -> Self {
        Self { state }
    }
}

#[derive(Debug)]
pub enum SimWifiError {}

impl WifiLink for SimWifi {
    type Error = SimWifiError;

    fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error> {
        self.state.set_status(format!("wifi: associate SSID=\"{}\"", creds.ssid.as_str()));
        thread::sleep(Duration::from_millis(120)); // mimic real associate latency
        let mut rssi = self.state.mock_rssi_dbm.lock().unwrap();
        *rssi = (*rssi).saturating_add(if (*rssi).rem_euclid(2) == 0 { 1 } else { -1 });
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        self.state.set_status("wifi: disconnected");
        Ok(())
    }

    fn rssi_dbm(&self) -> Option<i16> {
        Some(*self.state.mock_rssi_dbm.lock().unwrap())
    }
}

// ── HTTP (reqwest blocking) ───────────────────────────────────────────────────

pub struct SimHttp {
    state: Arc<SimState>,
    base_url: String,
    client: Client,
}

impl SimHttp {
    pub fn new(state: Arc<SimState>, base_url: String) -> Self {
        let client = Client::builder()
            .user_agent(concat!("paperanywhere-sim/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Self { state, base_url, client }
    }

    fn resolve(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_string()
        } else if path_or_url.starts_with('/') {
            format!("{}{}", self.base_url.trim_end_matches('/'), path_or_url)
        } else {
            format!("{}/{}", self.base_url.trim_end_matches('/'), path_or_url)
        }
    }
}

// Variant payloads are read via the derived `Debug` impl when the runtime
// formats errors into log lines and ack bodies; the dead-code lint can't see
// through `{:?}`.
#[allow(dead_code)]
#[derive(Debug)]
pub enum SimHttpError {
    Request(String),
    BadStatus(u16),
    Decode(String),
}

impl HttpTransport for SimHttp {
    type Error = SimHttpError;

    async fn get_state(&mut self, token: &str) -> Result<DeviceState, Self::Error> {
        let url = self.resolve("/api/device/state");
        self.state.set_status("http: GET /api/device/state");
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| SimHttpError::Request(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(SimHttpError::BadStatus(status));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| SimHttpError::Decode(e.to_string()))?;
        debug!("/state body: {}", body);
        let state = parse_device_state(&body)
            .ok_or_else(|| SimHttpError::Decode("parse_device_state returned None".into()))?;
        let image_label = state
            .image
            .as_ref()
            .map(|i| i.image_id.as_str())
            .unwrap_or("(none)");
        self.state.push_activity(format!(
            "/state ok — next={}s policy={:?} image={}",
            state.config.sleep_interval_sec, state.config.power_policy, image_label
        ));
        Ok(state)
    }

    async fn stream_blob(
        &mut self,
        token: &str,
        blob_url: &str,
        on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
    ) -> Result<(), Self::Error> {
        let url = self.resolve(blob_url);
        self.state.set_status(format!("http: GET {}", url));
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| SimHttpError::Request(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SimHttpError::BadStatus(resp.status().as_u16()));
        }
        let mut total = 0usize;
        let mut stream = resp.bytes_stream();
        while let Some(item) = stream.next().await {
            let bytes = item.map_err(|e| SimHttpError::Decode(e.to_string()))?;
            total += bytes.len();
            if on_chunk(&bytes).is_err() {
                return Err(SimHttpError::Decode("on_chunk asked to stop".into()));
            }
        }
        info!("blob stream: {} bytes total", total);
        Ok(())
    }

    async fn post_ack(&mut self, token: &str, ack: &DeviceAck) -> Result<(), Self::Error> {
        let url = self.resolve("/api/device/ack");
        let body = ack.to_json();
        self.state.set_status(format!(
            "http: POST /api/device/ack (phase={})",
            ack.phase.as_str()
        ));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| SimHttpError::Request(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SimHttpError::BadStatus(resp.status().as_u16()));
        }
        self.state.push_activity(format!("/ack {} ok", ack.phase.as_str()));
        Ok(())
    }
}

// ── NVS (JSON file) ───────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct NvsFile {
    /// 64-bit hash of the bytes currently on the virtual panel. Replaces
    /// the older `last_applied_image_id` field — change in schema is OK
    /// because this file is dev-only state in the user's data dir.
    #[serde(default, alias = "last_applied_image_id")]
    last_render_hash: Option<u64>,
}

pub struct SimNvs {
    state: Arc<SimState>,
    config: SimConfig,
    cache: NvsFile,
}

impl SimNvs {
    pub fn new(state: Arc<SimState>, config: SimConfig) -> Self {
        let cache = fs::read_to_string(&config.nvs_path)
            .ok()
            .and_then(|s| serde_json::from_str::<NvsFile>(&s).ok())
            .unwrap_or_default();
        if let Some(h) = cache.last_render_hash {
            *state.last_image_id.lock().unwrap() = Some(format!("hash:{:#018x}", h));
        }
        Self { state, config, cache }
    }

    fn persist(&self) {
        if let Some(parent) = self.config.nvs_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.cache) {
            Ok(body) => {
                if let Err(e) = fs::write(&self.config.nvs_path, body) {
                    warn!("nvs persist: {}", e);
                }
            }
            Err(e) => warn!("nvs serialize: {}", e),
        }
    }
}

impl NvsStore for SimNvs {
    fn load_wifi_creds(&self) -> Option<WifiCreds> {
        // Sim doesn't need real creds; runtime just checks for presence.
        // Hand back a placeholder so the polling loop proceeds.
        WifiCreds::from_strs("sim-network", "sim-password").ok()
    }

    fn load_device_token(&self) -> Option<String> {
        Some(self.config.device_token.clone())
    }

    fn load_last_render_hash(&self) -> Option<u64> {
        self.cache.last_render_hash
    }

    fn save_last_render_hash(&mut self, hash: u64) {
        self.cache.last_render_hash = Some(hash);
        *self.state.last_image_id.lock().unwrap() = Some(format!("hash:{:#018x}", hash));
        self.persist();
    }
}

// ── Firmware updater (mock) ───────────────────────────────────────────────────

/// The sim can't replace its own binary while running — that's a `cargo run`
/// concern, not a runtime one. The mock impl just logs the offer into the
/// activity feed so the user can see the wiring works, then returns Ok(()).
/// Returning Ok rather than Err keeps the wake cycle quiet (the runtime only
/// logs failures).
pub struct SimFirmwareUpdater {
    state: Arc<SimState>,
}

impl SimFirmwareUpdater {
    pub fn new(state: Arc<SimState>) -> Self {
        Self { state }
    }
}

#[derive(Debug)]
pub enum SimFirmwareUpdaterError {}

impl FirmwareUpdater for SimFirmwareUpdater {
    type Error = SimFirmwareUpdaterError;

    async fn apply<H: HttpTransport>(
        &mut self,
        _http: &mut H,
        _token: &str,
        update: &FirmwareUpdate,
    ) -> Result<(), Self::Error> {
        self.state.push_activity(format!(
            "fw update offered: {} ({} bytes){}",
            update.version,
            update.byte_len,
            if update.revoke { " [REVOKE]" } else { "" }
        ));
        info!(
            "sim: firmware update offered but ignored ({}); restart with the new sim binary instead",
            update.version
        );
        Ok(())
    }
}

// ── Sleep / clock / mock battery ──────────────────────────────────────────────

pub struct SimSleeper {
    state: Arc<SimState>,
}

impl SimSleeper {
    pub fn new(state: Arc<SimState>) -> Self {
        Self { state }
    }
}

impl Sleeper for SimSleeper {
    fn sleep_for(&mut self, seconds: u32, policy: PowerPolicy) {
        // Cap a single sleep at 30s so the user can iterate quickly without
        // waiting out a real 6h scheduled-wake interval. Real firmware would
        // honour `seconds` verbatim.
        let effective = seconds.min(30);
        self.state.set_status(format!(
            "sleep: {}s (clamped from {}s) policy={:?}",
            effective, seconds, policy
        ));
        // Trickle mock battery down so the UI doesn't look frozen.
        {
            let mut mv = self.state.mock_battery_mv.lock().unwrap();
            *mv = mv.saturating_sub(if effective > 10 { 5 } else { 1 });
        }
        thread::sleep(Duration::from_secs(effective.max(1) as u64));
    }

    fn unix_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn battery_mv(&self) -> Option<u16> {
        Some(*self.state.mock_battery_mv.lock().unwrap())
    }
}

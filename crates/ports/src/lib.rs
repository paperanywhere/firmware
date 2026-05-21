//! Port traits for the paperanywhere device runtime.
//!
//! The firmware binary and the desktop simulator both drive the same polling
//! state machine in [`paperanywhere-runtime`][runtime]. The state machine doesn't
//! know how WiFi, HTTP, NVS, the e-paper panel, or the clock are implemented —
//! it only knows the traits in this crate. Two concrete implementations exist:
//!
//!   * **firmware** wires the traits to `esp-radio`, `esp-storage`, real SPI
//!     panel drivers, and `esp-hal`'s RTC deep-sleep
//!   * **sim** wires them to `reqwest`, a JSON file on disk, an in-memory
//!     framebuffer rendered by `egui`, and `std::thread::sleep`
//!
//! Anything that can't be virtualized cleanly (e.g. captive-portal provisioning,
//! eFuse-backed factory MAC reads) stays in the firmware binary outside the
//! runtime.
//!
//! **Safety note:** none of these traits are permitted to write ESP32 eFuses.
//! Reading the factory MAC for AP-naming purposes is fine. Permanent fuse
//! writes are an explicit non-goal of the project.
//!
//! [runtime]: ../paperanywhere_runtime/index.html

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use heapless::String as HString;

pub mod chrome;

// ── Wire types (formerly firmware/src/wire.rs) ────────────────────────────────
//
// Sharing these here means the sim talks to the backend with exactly the same
// JSON shape the firmware does — no drift between the two transports.

/// Response from `GET /api/device/state`. Tells the device what to do next.
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// Image to render. `None` means "nothing new, just check again later".
    pub image: Option<ImageRef>,
    pub config: DeviceConfig,
    /// Unix seconds when the device should wake to poll again.
    pub next_check_at: u64,
    /// New firmware available for this device. The backend attaches this
    /// field when the device's reported `fw_version` is older than the
    /// latest release for its board model + channel. `None` means no
    /// update — most polls.
    pub firmware_update: Option<FirmwareUpdate>,
    /// Friendly name set by the user during adoption (or later via the
    /// dashboard). `None` for unclaimed devices — backend still returns
    /// the auto-generated `device-XXXX` name in that case so the
    /// device has *something* to show. The runtime persists this to
    /// NVS for the next boot screen.
    pub name: Option<String>,
    /// Backend-assigned UUID echoed back on every state poll. Same
    /// value the device originally received from register; included
    /// here so firmware that lost / never persisted the UUID can
    /// recover it via /state alone. The runtime saves this to NVS on
    /// every successful poll.
    pub device_uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub image_id: String,
    pub blob_url: String,
    pub sha256_hex: String,
    pub byte_len: u64,
}

#[derive(Debug, Clone)]
pub struct FirmwareUpdate {
    /// Human-readable version stamp the new image will report after boot
    /// (e.g. `0.2.0+f1e2d3c4`). Devices store this once they've finished
    /// flashing so they don't loop on the same offer.
    pub version: String,
    /// Path the device GETs to stream the image bytes. Bearer-authenticated
    /// with the device token, same as `image.blob_url`.
    pub blob_url: String,
    /// SHA-256 of the binary as a hex string. Verified chunked during
    /// download — mismatch aborts the install without touching otadata.
    pub sha256_hex: String,
    pub byte_len: u64,
    /// Set by the server-side kill switch: forces this update even if the
    /// device's current version was higher (rollback to a known-good
    /// release after a bad rollout still booted).
    pub revoke: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DeviceConfig {
    pub sleep_interval_sec: u32,
    pub power_policy: PowerPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerPolicy {
    ScheduledWake,
    AlwaysOn,
}

/// Request body for `POST /api/device/ack`. Reports the outcome of an image
/// render attempt plus telemetry.
#[derive(Debug, Clone)]
pub struct DeviceAck {
    pub image_id: String,
    pub phase: AckPhase,
    pub error: Option<String>,
    pub battery_mv: Option<u16>,
    pub rssi_dbm: Option<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckPhase {
    Received,
    Applied,
    Failed,
}

impl AckPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            AckPhase::Received => "received",
            AckPhase::Applied => "applied",
            AckPhase::Failed => "failed",
        }
    }
}

impl DeviceAck {
    /// Render to JSON without pulling serde. Matches what the backend route in
    /// `paperanywhere/crates/backend/src/routes/device_api.rs` accepts.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        push_kv_str(&mut out, "image_id", &self.image_id);
        out.push(',');
        push_kv_str(&mut out, "phase", self.phase.as_str());
        if let Some(err) = &self.error {
            out.push(',');
            push_kv_str(&mut out, "error", err);
        }
        if let Some(mv) = self.battery_mv {
            out.push(',');
            push_kv_num(&mut out, "battery_mv", mv as i64);
        }
        if let Some(rssi) = self.rssi_dbm {
            out.push(',');
            push_kv_num(&mut out, "rssi_dbm", rssi as i64);
        }
        out.push('}');
        out
    }
}

fn push_kv_str(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => push_unicode_escape(out, c),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_kv_num(out: &mut String, key: &str, value: i64) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    push_i64(out, value);
}

fn push_i64(out: &mut String, mut value: i64) {
    if value < 0 {
        out.push('-');
        value = -value;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if value == 0 {
        out.push('0');
        return;
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    out.push_str(core::str::from_utf8(&buf[i..]).expect("ascii digits"));
}

fn push_unicode_escape(out: &mut String, c: char) {
    out.push_str("\\u");
    let code = c as u32;
    for shift in [12, 8, 4, 0] {
        let nibble = ((code >> shift) & 0xF) as u8;
        out.push(if nibble < 10 { (b'0' + nibble) as char } else { (b'a' + nibble - 10) as char });
    }
}

/// Tiny ad-hoc JSON parser for the small `/state` response. Only handles the
/// shapes the backend produces — not a full parser, no nested objects beyond
/// what we need. Returns `None` on any structural surprise; caller treats it
/// as "skip this wake, retry next".
pub fn parse_device_state(body: &str) -> Option<DeviceState> {
    let image_id = extract_str(body, "image_id");
    let blob_url = extract_str(body, "blob_url");
    let sha256 = extract_str(body, "sha256");
    let byte_len = extract_u64(body, "byte_len");
    let sleep_interval_sec = extract_u64(body, "sleep_interval_sec").unwrap_or(21_600) as u32;
    let next_check_at = extract_u64(body, "next_check_at")?;
    let policy_str = extract_str(body, "power_policy").unwrap_or_else(|| String::from("scheduled_wake"));
    let power_policy = match policy_str.as_str() {
        "always_on" => PowerPolicy::AlwaysOn,
        _ => PowerPolicy::ScheduledWake,
    };

    let image = match (image_id, blob_url, sha256, byte_len) {
        (Some(id), Some(url), Some(sha), Some(len)) => Some(ImageRef {
            image_id: id,
            blob_url: url,
            sha256_hex: sha,
            byte_len: len,
        }),
        _ => None,
    };

    // Optional firmware-update block. Backend serializes a JSON object as
    // `firmware_update`; we parse it field-by-field via the existing
    // extractors. If any required field is missing we treat the whole
    // block as absent so a malformed response can't accidentally trigger
    // an OTA install.
    let firmware_update = parse_firmware_update(body);

    let name = extract_str(body, "name");
    let device_uuid = extract_str(body, "device_uuid");

    Some(DeviceState {
        image,
        config: DeviceConfig { sleep_interval_sec, power_policy },
        next_check_at,
        firmware_update,
        name,
        device_uuid,
    })
}

fn parse_firmware_update(body: &str) -> Option<FirmwareUpdate> {
    let version = extract_str(body, "firmware_version")?;
    let blob_url = extract_str(body, "firmware_blob_url")?;
    let sha256_hex = extract_str(body, "firmware_sha256")?;
    let byte_len = extract_u64(body, "firmware_byte_len")?;
    let revoke = extract_u64(body, "firmware_revoke").unwrap_or(0) != 0;
    Some(FirmwareUpdate { version, blob_url, sha256_hex, byte_len, revoke })
}

fn extract_str(body: &str, key: &str) -> Option<String> {
    let needle = needle_for(key, true);
    let start = body.find(needle.as_str())? + needle.len();
    let rest = &body[start..];
    let mut end = 0;
    let mut chars = rest.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch == '\\' {
            let _ = chars.next();
            continue;
        }
        if ch == '"' {
            end = i;
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let raw = &rest[..end];
    let mut out = String::with_capacity(raw.len());
    let mut iter = raw.chars();
    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '/' => out.push('/'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn extract_u64(body: &str, key: &str) -> Option<u64> {
    let needle = needle_for(key, false);
    let start = body.find(needle.as_str())? + needle.len();
    let rest = body[start..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn needle_for(key: &str, quoted_value: bool) -> String {
    let mut s = String::with_capacity(key.len() + 5);
    s.push('"');
    s.push_str(key);
    s.push_str("\":");
    if quoted_value {
        s.push('"');
    }
    s
}

// ── Panel geometry ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Mono1bpp,
    MonoRed1bpp,
    MonoYellow1bpp,
    Gray4,
    Gray16,
    Color7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackingKind {
    RowMajorMsbFirst1bpp,
    RowMajorLsbFirst1bpp,
    RowMajorBe2bpp,
    RowMajorBe4bpp,
    AcepIndexed4bpp,
}

/// What the runtime needs to know about the device. Concrete board configs
/// (with pin maps, peripheral capability flags, etc.) live in the firmware
/// binary; the runtime gets a slimmer view.
#[derive(Debug, Clone, Copy)]
pub struct PanelGeometry {
    pub width_px: u32,
    pub height_px: u32,
    pub color_mode: ColorMode,
    pub packing: PackingKind,
}

// ── Credentials ───────────────────────────────────────────────────────────────

/// WiFi STA credentials. Fixed-size buffers so the firmware doesn't need a
/// heap allocation per associate call; the sim ignores the size limits since
/// nothing in `std::net` cares about SSID length.
#[derive(Debug, Clone)]
pub struct WifiCreds {
    pub ssid: HString<32>,
    pub password: HString<64>,
}

impl WifiCreds {
    pub fn from_strs(ssid: &str, password: &str) -> Result<Self, ()> {
        let mut s: HString<32> = HString::new();
        let mut p: HString<64> = HString::new();
        s.push_str(ssid).map_err(|_| ())?;
        p.push_str(password).map_err(|_| ())?;
        Ok(Self { ssid: s, password: p })
    }
}

// ── Ports ─────────────────────────────────────────────────────────────────────

/// High-level device lifecycle state surfaced in the status bar's
/// top-left block. The runtime calls
/// [`EpaperPanel::set_status`] at each transition so the user can
/// see at a glance what the device is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Cold boot, before the runtime starts polling.
    Booting,
    /// WiFi associate / DHCP in progress.
    Connecting,
    /// Hitting `/state` to fetch the next image + config.
    DownloadingConfig,
    /// OTA install running.
    Updating,
    /// Wake cycle bounced through an error path — sleeping for the
    /// failure-retry interval before trying again.
    Stalled,
    /// Device is associated + has an IP but no `device_token` in NVS
    /// — waiting to be adopted to a user account. The adoption screen
    /// is on the main region.
    WaitingForAdoption,
    /// Idle, between wakes.
    Ready,
    /// Terminal error — the halt screen is on the main region and the
    /// runtime is busy-looping. Power-cycle or re-provision to clear.
    Halted,
}

impl Default for DeviceStatus {
    fn default() -> Self {
        Self::Booting
    }
}

impl DeviceStatus {
    /// Lower-case label for the status bar's "Status:" prefix.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Booting => "booting",
            Self::Connecting => "connecting",
            Self::DownloadingConfig => "downloading configuration",
            Self::Updating => "updating",
            Self::Stalled => "stalled",
            Self::WaitingForAdoption => "waiting for adoption",
            Self::Ready => "ready",
            Self::Halted => "halted",
        }
    }
}

/// L2 WiFi association. The runtime asks for an association each wake and a
/// disconnect before sleep — IP-stack setup is the [`HttpTransport`]'s problem.
pub trait WifiLink {
    type Error: core::fmt::Debug;
    #[allow(async_fn_in_trait)]
    async fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error>;
    fn disconnect(&mut self) -> Result<(), Self::Error>;
    /// Best-effort signal-strength readout for the `/ack` heartbeat payload.
    /// Returns `None` if unsupported on this implementation.
    fn rssi_dbm(&self) -> Option<i16>;
    /// Current IPv4 address from DHCP, or `None` when not yet
    /// associated / address pending. Default `None` for impls that
    /// don't track this (the sim's mock impl). Used by the status
    /// bar to show the local IP and by the dev HTTP server's /info
    /// response so the developer doesn't need to ARP-scan their LAN.
    fn local_ip(&self) -> Option<[u8; 4]> {
        None
    }
    /// Current IPv4 gateway address from the DHCP lease, or `None`
    /// when the lease hasn't yet established / no gateway is in the
    /// network's offer. Surfaced on the boot screen's Network column.
    /// Default `None` for impls that don't track it.
    fn gateway_v4(&self) -> Option<[u8; 4]> {
        None
    }
}

/// Identity an unclaimed device sends to `POST /api/device/register` so the
/// backend can create an anonymous device row keyed by MAC and surface the
/// device's hardware shape (panel model) to the dashboard before the user
/// adopts it. After the user enters the returned `claim_code` in the
/// dashboard, the backend already knows the panel model + firmware version
/// — the adoption form is just `claim_code + optional name`.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// `aa:bb:cc:dd:ee:ff` — backend normalises so any common formatting works.
    pub mac: alloc::string::String,
    /// Catalog row in the backend's `panel_models` table. Comes from the
    /// firmware's `BoardConfig::panel_model_id` constant — every board crate
    /// declares its own.
    pub panel_model_id: i32,
    /// Firmware build stamp (e.g. `0.1.0+a1b2c3d4`). Sent so the dashboard
    /// can decide whether to offer an OTA update on the adopt-success page.
    pub fw_version: alloc::string::String,
}

/// What the backend hands back after a successful registration. The
/// firmware persists all three to NVS:
///   * `device_token` becomes the bearer for `/api/device/state` + `/ack`,
///   * `claim_code` is what the adoption screen renders for the user,
///   * `device_uuid` is the identifier the backend uses internally — useful
///     for any future device-initiated lookup but not user-visible.
#[derive(Debug, Clone)]
pub struct DeviceRegistration {
    pub device_uuid: alloc::string::String,
    pub device_token: alloc::string::String,
    pub claim_code: alloc::string::String,
}

/// HTTPS calls against the backend. Most are bearer-authenticated with the
/// device token from [`NvsStore::load_device_token`]; [`register`] is the
/// exception — it's how a fresh device acquires that token in the first
/// place, so it sends no `Authorization` header.
///
/// Methods are `async` because the firmware impl drives an `embassy-net`
/// stack on top of `esp-radio`'s WiFi station — the same executor that runs
/// the polling state machine also runs the network task, so any blocking
/// `block_on` inside an impl would starve packet processing. The sim impl
/// runs in a tokio runtime; same shape, different executor.
///
/// The `async_fn_in_trait` lint warns that auto-traits (`Send`/`Sync`) on the
/// returned futures can't be expressed in the trait. We use this trait only
/// through generic monomorphization — never `dyn HttpTransport` — so callers
/// see the concrete return type and the auto-traits flow naturally from
/// captures. The `Send` bound on `stream_blob`'s closure is the only one
/// that needs to be explicit because it's the only opaque-typed argument.
#[allow(async_fn_in_trait)]
pub trait HttpTransport {
    type Error: core::fmt::Debug;
    /// Identify this device to the backend so the dashboard's adoption
    /// flow can match a claim code back to the right hardware. Called
    /// once per unclaimed device on first cold boot after WiFi + DHCP
    /// come up. The backend returns a fresh `device_token`, a 6-char
    /// `claim_code`, and the device's allocated UUID. Subsequent
    /// re-registers (same MAC) return the same UUID — the backend's
    /// `register_anonymous_device` is idempotent on MAC.
    async fn register(
        &mut self,
        identity: &DeviceIdentity,
    ) -> Result<DeviceRegistration, Self::Error>;
    async fn get_state(&mut self, token: &str) -> Result<DeviceState, Self::Error>;
    /// Streams the processed-blob bytes through `on_chunk`. Lets the firmware
    /// pipe chunks straight into the panel SPI without ever holding the full
    /// image in RAM.
    async fn stream_blob(
        &mut self,
        token: &str,
        blob_url: &str,
        // `+ Send` so the returned future is `Send`, which tokio's
        // multi-threaded runtime requires for `spawn`. The firmware's
        // single-threaded embassy executor doesn't care either way.
        on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
    ) -> Result<(), Self::Error>;
    async fn post_ack(&mut self, token: &str, ack: &DeviceAck) -> Result<(), Self::Error>;
}

/// Persisted device state.
///
/// The firmware impl talks to the NVS partition via `esp-storage`; the sim
/// keeps a JSON file in the user's data dir.
pub trait NvsStore {
    fn load_wifi_creds(&self) -> Option<WifiCreds>;
    fn load_device_token(&self) -> Option<String>;
    /// 64-bit hash identifying the content currently rendered on the panel.
    /// `None` means nothing has been rendered yet (fresh device or post-
    /// factory-reset). Callers compute the hash of what they're about to
    /// render and skip the refresh entirely if it matches — content-
    /// addressed dedup that catches "same image, different image_id" as
    /// well as the boot-screen-on-every-wake case.
    fn load_last_render_hash(&self) -> Option<u64>;
    fn save_last_render_hash(&mut self, hash: u64);
    /// Optional override of the server root URL — set via the prov partition
    /// or the SD-card config file. `None` means "use the firmware default",
    /// which is baked in at build time.
    fn load_backend_url(&self) -> Option<String> {
        None
    }
    /// Was this device flashed as a dev build? Provtool sets this byte to
    /// `true` when generating a prov.bin with `--dev`; production flashes
    /// leave it unset. When `true`, the firmware skips the GitHub-release
    /// OTA check entirely — useful so a dev rig doesn't get its
    /// hand-built binary clobbered the moment a real release ships.
    /// Default impl returns `false` (i.e. "this is a production build,
    /// OTA is allowed") so existing impls don't have to opt-in.
    fn load_is_dev_build(&self) -> bool {
        false
    }
    /// Device-side claim code (e.g. `1234-5678`) that the user types
    /// into the frontend to adopt this device. Issued by the backend
    /// on first registration and persisted to NVS so it survives reboots.
    /// `None` on impls (sim) that don't support a claim flow.
    fn load_claim_code(&self) -> Option<alloc::string::String> {
        None
    }
    /// Backend-assigned UUID. Returned by `POST /api/device/register`
    /// and persisted to NVS; remains the device's identity across
    /// reboots (and matches the row in the backend's `devices`
    /// table). Surfaced on the boot screen.
    fn load_device_uuid(&self) -> Option<alloc::string::String> {
        None
    }
    /// User-supplied friendly name. Returned on `GET /api/device/state`
    /// once the device has been adopted; persisted to NVS so reboots
    /// show the name before /state can be reached. Surfaced on the
    /// boot screen alongside the UUID.
    fn load_device_name(&self) -> Option<alloc::string::String> {
        None
    }
    /// Persist the bearer token returned by the backend's
    /// `POST /api/device/register`. Default no-op so impls that don't
    /// participate in the adoption flow (e.g. sim with a hardcoded
    /// token) don't have to override.
    fn save_device_token(&mut self, _token: &str) {}
    /// Persist the claim code returned by the backend's
    /// `POST /api/device/register`. Default no-op for impls without
    /// a claim flow.
    fn save_claim_code(&mut self, _code: &str) {}
    /// Persist the device UUID returned by register. Default no-op.
    fn save_device_uuid(&mut self, _uuid: &str) {}
    /// Persist the friendly name returned by /state. Default no-op.
    fn save_device_name(&mut self, _name: &str) {}
}

/// Three-state WiFi link status as surfaced on the boot screen's
/// Network column.
///
/// Distinct from `WifiLink::rssi_dbm`'s Option signal — the boot
/// screen wants to distinguish "we're in the middle of associating"
/// from "we're truly disconnected", whereas the status-bar widgets
/// only care about the binary "have signal / no signal" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WifiLinkState {
    /// No association attempt active (cold boot, factory reset, or
    /// post-disconnect before retry kicks in).
    #[default]
    Disconnected,
    /// `WifiLink::associate` is in flight — WPA handshake + DHCP not
    /// yet complete.
    Connecting,
    /// Associated with a live DHCP lease.
    Connected,
}

impl WifiLinkState {
    /// Display label for the boot-screen Network column.
    pub fn label(self) -> &'static str {
        match self {
            WifiLinkState::Disconnected => "Disconnected",
            WifiLinkState::Connecting => "Connecting",
            WifiLinkState::Connected => "Connected",
        }
    }
}

/// E-paper panel surface. HTTPS chunks arrive at arbitrary byte boundaries —
/// the runtime doesn't try to slice them into rows for us — so the trait is a
/// raw byte sink. The implementation tracks its write cursor across calls and
/// resets it on `init` / `refresh`.
pub trait EpaperPanel {
    /// Reset + init the controller and reset the write cursor to 0. Called
    /// once before the first wake cycle. Async because the underlying SPI
    /// device is async — the boot sequence sends a series of init commands
    /// that each yield to the executor while the SPI FIFO drains, instead
    /// of busy-polling like the previous sync-SPI driver did.
    fn init(&mut self) -> impl core::future::Future<Output = ()>;
    /// Push current chrome state (battery, wifi association) into whatever
    /// status-bar layer is sitting between the runtime and the bare panel.
    /// Default is a no-op; the compositor overrides it. Called by the
    /// runtime before any [`refresh`] so the bar reflects current state.
    fn set_chrome(&mut self, _battery_mv: Option<u16>, _wifi_rssi_dbm: Option<i16>) {}
    /// Fine-grained hook: WiFi association state changed. `None` means
    /// disconnected; `Some(rssi)` means associated at that signal level.
    /// Default no-op; compositors override to mark the WiFi icon dirty.
    /// Doesn't itself trigger a panel refresh — the next [`refresh`] call
    /// picks it up. Once partial-refresh LUTs land we can flush only the
    /// status-bar region here without disturbing the main image.
    fn on_wifi_state_changed(&mut self, _rssi_dbm: Option<i16>) {}
    /// Fine-grained hook: new battery sample available. Same semantics as
    /// [`on_wifi_state_changed`]: updates the cached value but doesn't
    /// trigger a panel refresh until the runtime asks.
    fn on_battery_sample(&mut self, _mv: Option<u16>) {}
    /// USB-CDC enumeration state. `Some(true)` means a serial host is
    /// attached; `Some(false)` explicitly disconnected; `None` means
    /// the board has no USB-CDC support / never sets the field.
    fn on_usb_state_changed(&mut self, _connected: Option<bool>) {}
    /// Identifier shown on the left side of the status bar — typically
    /// a short fingerprint derived from the device token (e.g. last
    /// 4 hex chars). Idempotent; the runtime calls this once at boot.
    fn set_device_id(&mut self, _id: &str) {}
    /// Local-time stamp of the most recent successful image render
    /// (e.g. `10:23`). Runtime updates this after each completed
    /// render; the compositor shows it in the left-side info text.
    fn set_last_update(&mut self, _local_time: &str) {}
    /// IPv4 dotted-quad address (e.g. `10.0.1.42`) for the status
    /// bar's left-side info text. Runtime sets this once DHCP
    /// resolves, after `WifiLink::associate` succeeds.
    fn set_ip(&mut self, _ip: &str) {}
    /// Re-paint the boot-screen template with current chrome state
    /// (notably the IP, which lands under the version line). Default
    /// no-op for bare drivers; compositors that cache the boot screen
    /// at startup use this to refresh the splash once DHCP completes,
    /// so the IP shows up on the panel without a separate render
    /// path in the firmware. Runtime calls this once per wake after
    /// successfully fetching /state.
    fn redraw_boot_screen(&mut self) {}
    /// Push the high-level device status into the status bar's top-
    /// left block. Default no-op. Runtime updates this at each
    /// transition: Booting → Connecting → DownloadingConfig → Ready
    /// (or → Updating / Stalled depending on what happens). The
    /// compositor renders the human-readable label.
    fn set_status(&mut self, _status: DeviceStatus) {}
    /// Set the boot-screen hold countdown. `Some(n)` displays
    /// "Transitioning in N..." at the bottom of the build-info block
    /// on the next refresh; `None` blanks the countdown line. Doesn't
    /// itself trigger a refresh — caller follows up with
    /// `redraw_boot_screen` to apply. Default no-op for bare panels
    /// that don't render a boot template.
    fn set_boot_countdown(&mut self, _seconds: Option<u8>) {}
    /// IPv4 gateway address from the current DHCP lease (e.g.
    /// `10.0.1.1`). Surfaced on the boot-screen's Network column.
    /// Doesn't trigger a refresh on its own; takes effect on the
    /// next view-render. Default no-op for bare panels.
    fn set_gateway(&mut self, _ip: Option<&str>) {}
    /// Backend URL the device polls/posts to (e.g.
    /// `https://api.paperanywhere.io`). Surfaced on the boot-screen's
    /// Firmware column so the user can verify the device is pointed
    /// at the right environment. Default no-op for bare panels.
    fn set_backend_url(&mut self, _url: Option<&str>) {}
    /// WiFi link-state for the Network column on the boot screen.
    /// Distinct from `on_wifi_state_changed`'s RSSI signal because
    /// the boot screen wants a 3-state label (Disconnected /
    /// Connecting / Connected) rather than a binary associated /
    /// not-associated flag. Default no-op.
    fn set_wifi_link_state(&mut self, _state: WifiLinkState) {}
    /// SSID currently being attempted / associated with. `None`
    /// clears the field (e.g. on factory reset / no creds yet).
    /// Default no-op.
    fn set_ssid(&mut self, _ssid: Option<&str>) {}
    /// Backend-assigned device UUID. Surfaced on the boot screen as
    /// a full-width line below the column block — the full 36-char
    /// UUID4 doesn't fit in the column's per-row value budget.
    /// Default no-op.
    fn set_device_uuid(&mut self, _uuid: Option<&str>) {}
    /// User-supplied friendly name (e.g. "Kitchen calendar"). Shown
    /// on the boot screen's DEVICE column once `/state` has reported
    /// it. Default no-op.
    fn set_device_name(&mut self, _name: Option<&str>) {}
    /// Mark the next [`refresh`] / [`refresh_fast`] as a full-LUT
    /// pass even if the compositor would otherwise pick fast. Used
    /// at view boundaries where the user expects a clear, unmistakable
    /// visual transition (e.g. "we just connected to WiFi — show
    /// the populated boot screen") that the partial waveform might
    /// be too subtle to convey on small text changes. Default no-op
    /// for bare panels.
    fn force_full_next_refresh(&mut self) {}
    /// Paint the adoption screen into the main region (QR code + big
    /// claim code + device id + IP + adopt URL). Called by the
    /// runtime when the device has no `device_token` in NVS, i.e.
    /// the device hasn't been claimed to a user account yet.
    ///
    /// `retry_notice` is `Some(msg)` when the runtime wants to surface
    /// a transient warning at the bottom of the screen — typically
    /// "Backend unreachable — retrying…" or "Waiting for network…".
    /// `None` when the registration round-trip completed cleanly and
    /// the displayed claim code is fresh.
    ///
    /// Default no-op for bare panels; compositors that know their
    /// framebuffer layout override and rasterise the screen.
    fn render_adoption_screen(
        &mut self,
        _claim_code: &str,
        _device_id: &str,
        _ip: &str,
        _adopt_url: &str,
        _retry_notice: Option<&str>,
    ) {
    }
    /// Paint the halt screen ("blue screen of death") into the main
    /// region. Caller (runtime) renders this then halts the device
    /// — no further wake cycles, no deep sleep. The screen explains
    /// why; `code` is a stable short identifier the user can look up
    /// at `paperanywhere.io/errors/<code>`.
    fn render_halt_screen(&mut self, _headline: &str, _detail: &str, _code: &str) {}

    /// Paint the OTA-progress view (live "Updating firmware..." screen
    /// with a progress bar + status line). Called by the runtime
    /// whenever the shared OTA progress signal changes. Implementors
    /// should mark the next refresh as a full-LUT pass on the first
    /// non-Idle phase so the view replaces the prior content cleanly,
    /// and use fast-refresh on subsequent updates (progress-bar ticks)
    /// to keep iteration cost low. Default no-op for bare panels.
    fn render_ota_progress(&mut self, _phase: OtaPhase) {}
    /// Reset the panel's frame-write cursor back to position 0 so the
    /// next [`write_chunk`] sequence overwrites the previous frame
    /// instead of appending past it. Default no-op for panels that
    /// don't have a separate write cursor; UC8179 implementations
    /// re-issue CMD_DTM2 here so the next write starts at the top of
    /// the panel's frame RAM.
    ///
    /// MUST be called before every full frame write (typically once
    /// per refresh). The compositor calls it from its own refresh path
    /// so callers using a bare panel don't need to remember.
    ///
    /// Async — the UC8179 impl issues a 1-byte SPI command (CMD_DTM2)
    /// whose write yields to the executor. Default returns a ready
    /// future so bare panels that don't need a cursor reset cost
    /// nothing.
    fn begin_frame(&mut self) -> impl core::future::Future<Output = ()> {
        core::future::ready(())
    }
    /// Append `bytes` to the panel's frame RAM at the current cursor.
    /// Chunk boundaries don't need to align to any pixel structure.
    ///
    /// Async — yields to the embassy executor during each SPI burst so
    /// embassy-net's WiFi-RX poll keeps running. The previous sync
    /// signature meant a 48 KB framebuffer flush held the CPU for
    /// ~38 ms with no yields, long enough for the gateway to ARP-evict
    /// our DHCP lease (the "panel render → 'Destination host
    /// unreachable' stream" symptom). Task #90.
    fn write_chunk(&mut self, bytes: &[u8]) -> impl core::future::Future<Output = ()>;
    /// Stage the final composed surface ahead of [`refresh`]. Lets
    /// layered drivers (the compositor) paint chrome on top of the
    /// runtime-streamed main-region bytes — so [`pending_hash`] and
    /// [`refresh`] see the actual bytes that will hit the panel,
    /// status bar included. Bare drivers are no-ops; their frame RAM
    /// is whatever `write_chunk` already staged. Idempotent.
    fn compose(&mut self) {}
    /// Hash of the bytes that would be flushed if [`refresh`] ran
    /// right now. Computed AFTER [`compose`] so it includes whatever
    /// chrome the compositor adds. Returns `None` when the driver
    /// has no content-hash dedup (bare UC8179, sim virtual panel);
    /// the runtime then refreshes unconditionally. Done at the driver
    /// level rather than the runtime so the dedup naturally tracks
    /// status-bar changes (battery %, wifi state, last-update time)
    /// the same way it tracks image changes.
    fn pending_hash(&self) -> Option<u64> {
        None
    }
    /// Commit the buffer to the panel surface and reset the cursor to 0 so
    /// the next image starts from the top. On a real panel this is the slow
    /// e-ink refresh; on the sim it triggers an egui repaint.
    ///
    /// MUST be async — the UC8179 driver polls a BUSY pin for ~3 s during
    /// the full-LUT refresh, and a sync implementation that hard-spins on
    /// that pin starves the embassy executor (no embassy-net polling, no
    /// ICMP — everything else on the chip freezes for the full refresh
    /// window). The impl yields between polls so other tasks keep
    /// getting scheduled.
    ///
    /// Future does NOT need to be `Send` — embassy's single-threaded
    /// executor on the chip never moves a future between threads, and
    /// the sim is also single-task. Adding `+ Send` would force every
    /// compositor field to be `Send` for no real benefit.
    fn refresh(&mut self) -> impl core::future::Future<Output = ()>;
    /// Fast refresh using the panel's partial-LUT mode where available.
    /// Typically ~750 ms on UC8179 boards versus ~3 s for a full refresh,
    /// with the trade-off of slight ghosting that accumulates over
    /// successive partial refreshes (callers usually intersperse a full
    /// refresh every N updates to clear the residual). Default falls
    /// back to [`refresh`] for panels without a partial-LUT path.
    ///
    /// Same async contract as [`refresh`] — the BUSY-pin wait must yield
    /// to the executor between polls.
    fn refresh_fast(&mut self) -> impl core::future::Future<Output = ()> {
        self.refresh()
    }
}

/// Apply a firmware update offered by the backend in the /state response.
///
/// On real hardware this means: download the new image into the inactive
/// app slot, verify its SHA-256, write the otadata partition to swap the
/// boot pointer, and reset the chip. The function only returns on
/// **failure** — on success the device is mid-reboot and execution never
/// reaches the caller.
///
/// The sim's impl is a no-op (the sim can't update its own binary while
/// running; the user re-runs `cargo run` for that).
#[allow(async_fn_in_trait)]
pub trait FirmwareUpdater {
    type Error: core::fmt::Debug;
    async fn apply<H: HttpTransport>(
        &mut self,
        http: &mut H,
        token: &str,
        update: &FirmwareUpdate,
    ) -> Result<(), Self::Error>;
}

/// Clock + sleep. The firmware impl invokes `esp-hal`'s deep-sleep (which
/// resets the chip — `sleep_for` "returns" because the function body type-
/// checks via the `-> !` `deep_sleep_for` call, but in practice control never
/// reaches the caller). The sim impl just `thread::sleep`s.
/// Status of an in-progress OTA firmware update. Surfaced from the
/// OTA install path (backend-driven update or future GitHub-releases
/// fetcher) to the runtime, which renders the matching panel view.
///
/// Variants are ordered chronologically — `Idle → Receiving →
/// Verifying → Applying → (chip resets)`. `Failed` is terminal for
/// the current attempt; the device stays on the failed-view until the
/// next reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaPhase {
    /// No OTA in progress. Runtime renders normal views (boot / image / adoption).
    Idle,
    /// Bytes streaming in. Bar fills proportionally.
    Receiving { bytes_received: u32, total_bytes: u32 },
    /// All bytes received; verifying the sha256 hash.
    Verifying,
    /// Hash matched; swapping OTA slots + marking the new image active.
    Applying,
    /// Terminal error during the attempt. `code` is the short PA-OTA-NNN
    /// identifier that maps to a user-readable description on the docs site;
    /// `message` is a short one-liner shown directly on the panel.
    Failed { code: &'static str, message: &'static str },
}

impl OtaPhase {
    /// Convenience for the compositor: returns a 0..=100 progress percent.
    /// Receiving uses real progress; Verifying = 95, Applying = 99,
    /// other terminal states = 100. Avoids the panel rendering a
    /// 100%-full bar while the chip is still verifying.
    pub fn percent(&self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Receiving { bytes_received, total_bytes } => {
                if *total_bytes == 0 {
                    0
                } else {
                    let scaled =
                        (u64::from(*bytes_received) * 90 / u64::from(*total_bytes)) as u8;
                    scaled.min(90)
                }
            }
            Self::Verifying => 95,
            Self::Applying => 99,
            Self::Failed { .. } => 100,
        }
    }

    /// Short status line shown under the progress bar. Caller-supplied
    /// buffer because we're no_std + want stack-only formatting; pass a
    /// 64-byte `heapless::String<64>` and the function fills it.
    ///
    /// Numeric values are zero-padded to a fixed width so the panel
    /// doesn't reflow text between updates — e.g. "Receiving   42
    /// /  938 KB" → "Receiving  421 /  938 KB" → "Receiving  938 /
    /// 938 KB". Without this, every tick visibly shifts the line
    /// (centered alignment + changing string length).
    pub fn write_status(self, dst: &mut HString<64>) {
        dst.clear();
        match self {
            Self::Idle => {
                let _ = dst.push_str("Idle");
            }
            Self::Receiving { bytes_received, total_bytes } => {
                let kb_recv = bytes_received / 1024;
                let kb_total = total_bytes / 1024;
                let _ = core::fmt::write(
                    dst,
                    format_args!("Receiving {:>5} / {:>5} KB", kb_recv, kb_total),
                );
            }
            Self::Verifying => {
                let _ = dst.push_str("Verifying sha256...");
            }
            Self::Applying => {
                let _ = dst.push_str("Applying — swapping slots...");
            }
            Self::Failed { code, message } => {
                let _ = core::fmt::write(dst, format_args!("Failed [{}]: {}", code, message));
            }
        }
    }
}

pub trait Sleeper {
    /// Sleep `seconds` honoring `policy`. For [`PowerPolicy::ScheduledWake`]
    /// this is the chip's deep-sleep + RTC wake on real hardware; for
    /// [`PowerPolicy::AlwaysOn`] it's an `embassy_time::Timer` that yields
    /// to the executor (so other tasks like the network stack and panel
    /// actor keep getting polled while the wake loop is between
    /// iterations). The sim awaits a `tokio::time::sleep`.
    ///
    /// MUST be async — a busy-spin sync implementation would starve every
    /// other embassy task on the chip (embassy-net polling, panel actor,
    /// etc.) for the entire sleep window.
    fn sleep_for(
        &mut self,
        seconds: u32,
        policy: PowerPolicy,
    ) -> impl core::future::Future<Output = ()>;
    fn unix_now(&self) -> u64;
    fn battery_mv(&self) -> Option<u16>;
}

// ── Content-hash helper ──
//
// FNV-1a 64 — small, fast, no_std-friendly. Used for the panel content-dedup
// path: callers hash the bytes they're about to render (or the backend's
// `sha256_hex` string) and compare against `NvsStore::load_last_render_hash`
// before issuing a refresh. Collision risk at 64 bits is negligible for the
// small number of distinct screens any one device sees over its lifetime.

const FNV_1A_64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_1A_64_PRIME: u64 = 0x0000_0100_0000_01B3;

/// FNV-1a 64-bit hash. Stable across builds + across consumers.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = FNV_1A_64_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_1A_64_PRIME);
    }
    h
}

#[allow(dead_code)]
fn _force_use_vec(_v: Vec<u8>) {}

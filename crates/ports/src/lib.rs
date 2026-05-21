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

    Some(DeviceState {
        image,
        config: DeviceConfig { sleep_interval_sec, power_policy },
        next_check_at,
        firmware_update,
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
    /// Idle, between wakes / serving the dev_server.
    Ready,
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
}

/// HTTPS calls against the backend. All three are bearer-authenticated with
/// the device token from [`NvsStore::load_device_token`].
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
    /// into the frontend to adopt this device. Generated once from
    /// the chip MAC at boot time and persisted by the firmware so
    /// it's stable across reboots. `None` on impls (sim) that don't
    /// support a claim flow.
    fn load_claim_code(&self) -> Option<alloc::string::String> {
        None
    }
}

/// E-paper panel surface. HTTPS chunks arrive at arbitrary byte boundaries —
/// the runtime doesn't try to slice them into rows for us — so the trait is a
/// raw byte sink. The implementation tracks its write cursor across calls and
/// resets it on `init` / `refresh`.
pub trait EpaperPanel {
    /// Reset + init the controller and reset the write cursor to 0. Called
    /// once before the first wake cycle.
    fn init(&mut self);
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
    /// Paint the adoption screen into the main region (QR code + big
    /// claim code + device id + IP + adopt URL). Called by the
    /// runtime when the device has no `device_token` in NVS, i.e.
    /// the device hasn't been claimed to a user account yet.
    /// Default no-op for bare panels; compositors that know their
    /// framebuffer layout override and rasterise the screen.
    fn render_adoption_screen(
        &mut self,
        _claim_code: &str,
        _device_id: &str,
        _ip: &str,
        _adopt_url: &str,
    ) {
    }
    /// Append `bytes` to the panel's frame RAM at the current cursor. Chunk
    /// boundaries don't need to align to any pixel structure.
    fn write_chunk(&mut self, bytes: &[u8]);
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
    fn refresh(&mut self);
    /// Fast refresh using the panel's partial-LUT mode where available.
    /// Typically ~750 ms on UC8179 boards versus ~3 s for a full refresh,
    /// with the trade-off of slight ghosting that accumulates over
    /// successive partial refreshes (callers usually intersperse a full
    /// refresh every N updates to clear the residual). Default falls
    /// back to [`refresh`] for panels without a partial-LUT path.
    fn refresh_fast(&mut self) {
        self.refresh();
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
pub trait Sleeper {
    /// Sleep `seconds` honoring `policy`. For [`PowerPolicy::ScheduledWake`]
    /// this is the chip's deep-sleep + RTC wake on real hardware; for
    /// [`PowerPolicy::AlwaysOn`] it's a light modem sleep that keeps the radio
    /// reachable. The sim is the same `thread::sleep` for both.
    fn sleep_for(&mut self, seconds: u32, policy: PowerPolicy);
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

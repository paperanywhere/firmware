//! Persisted device state — implements [`NvsStore`] for the device, backed
//! by the `nvs` partition declared in `flash/partition-table.csv` (24 KB
//! starting at 0x9000).
//!
//! ## Layout
//!
//! A single 512-byte record at the start of the partition, format:
//!
//! ```text
//! [0..4]    magic = "PA4B"
//! [4..8]    version (u32 LE, currently 1)
//! [8..12]   CRC32 of bytes [12..512] (poly 0xEDB88320, LE)
//! [12..]    TLV records, terminated by tag = 0
//!           record = [tag u8][len u16 BE][value (len bytes)]
//! ```
//!
//! TLV tags:
//!
//!   1 → device_token (UTF-8, ≤64 bytes)
//!   2 → wifi_ssid    (UTF-8, ≤32 bytes)
//!   3 → wifi_password (UTF-8, ≤64 bytes)
//!   4 → backend_url  (UTF-8, ≤128 bytes)
//!   5 → claim_code   (UTF-8, ≤16 bytes)
//!   6 → last_render_hash (8 bytes LE)
//!   7 → is_dev_build (1 byte)
//!   8 → device_uuid  (UTF-8, ≤48 bytes — backend-assigned, stable across boots)
//!   9 → device_name  (UTF-8, ≤64 bytes — user-set friendly name from /state)
//!
//! On boot, the cache is loaded once into an in-memory `NvsCache`. Reads come
//! from the cache; writes update the cache and write the whole record back —
//! the runtime only saves on new-image-applied, so the write rate is fine
//! without wear leveling. (Hardening pass should add a backup sector + a
//! generation counter so a power loss mid-write falls back to the previous
//! good record rather than triggering a re-claim.)

use alloc::string::{String, ToString};

use embedded_storage::{ReadStorage, Storage};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use heapless::String as HString;
use paperanywhere_ports::{NvsStore, WifiCreds};

const NVS_PARTITION_OFFSET: u32 = 0x9000;
const NVS_RECORD_SIZE: usize = 512;
const NVS_HEADER_LEN: usize = 12;
const NVS_PAYLOAD_LEN: usize = NVS_RECORD_SIZE - NVS_HEADER_LEN;
const NVS_MAGIC: [u8; 4] = *b"PA4B";
const NVS_VERSION: u32 = 1;

// `prov` partition: 16 KB at 0x12000 (see flash/partition-table.csv). Same
// TLV record layout as NVS but with magic "PA4P" so a mis-flashed blob is
// detectable. First-boot migration copies fields into NVS and zeros the
// prov region.
const PROV_PARTITION_OFFSET: u32 = 0x12000;
const PROV_BLOB_SIZE: usize = 16 * 1024;
const PROV_HEADER_LEN: usize = 12;
const PROV_MAGIC: [u8; 4] = *b"PA4P";
const PROV_VERSION: u32 = 1;

const TAG_DEVICE_TOKEN: u8 = 1;
const TAG_WIFI_SSID: u8 = 2;
const TAG_WIFI_PASSWORD: u8 = 3;
const TAG_BACKEND_URL: u8 = 4;
const TAG_CLAIM_CODE: u8 = 5;
/// 64-bit content hash of whatever's currently on the panel. Replaces the
/// older per-image-id cache: every render path computes a hash of its
/// content (boot screen bytes, backend's `sha256_hex` string for images,
/// future setup screens) and skips the refresh when this value matches.
/// Payload is 8 bytes little-endian.
const TAG_LAST_RENDER_HASH: u8 = 6;
/// Dev-build marker. `true` (1-byte payload = 0x01) means provtool was
/// invoked with `--dev` when this device was flashed; the firmware
/// suppresses the GitHub-release OTA check so a freshly hand-built
/// binary doesn't get clobbered by the next release. Absent / 0x00 =
/// production build, OTA path active.
const TAG_IS_DEV_BUILD: u8 = 7;
/// Backend-assigned device UUID (e.g. `019e4bff-47c0-7882-b1ce-49d4b26e1002`).
/// Returned by `POST /api/device/register` on first boot and persisted
/// here so the device displays a stable identity on the boot screen
/// across reboots and survives backend MAC re-lookups.
const TAG_DEVICE_UUID: u8 = 8;
/// User-supplied friendly name (e.g. "Kitchen calendar"). Comes back on
/// `GET /api/device/state` once a user has adopted the device through
/// the dashboard. Displayed on the boot screen / status bar in
/// preference to the UUID once set.
const TAG_DEVICE_NAME: u8 = 9;

#[derive(Default)]
struct NvsCache {
    device_token: Option<HString<64>>,
    ssid: Option<HString<32>>,
    password: Option<HString<64>>,
    backend_url: Option<HString<128>>,
    claim_code: Option<HString<16>>,
    /// Hash of the bytes currently rendered on the panel. `None` means the
    /// panel is in its power-on (blank) state or post-factory-reset.
    last_render_hash: Option<u64>,
    /// `true` if this device is flashed in dev mode. Defaults to `false`
    /// (production); set to `true` via the prov blob's `--dev` flag.
    is_dev_build: bool,
    /// Backend-assigned UUID (36 chars + null padding budget). Set once on
    /// first register; stable across reboots.
    device_uuid: Option<HString<48>>,
    /// User-supplied friendly name set during adoption. Refreshed on every
    /// /state call (cheap — the field comes back on every response).
    device_name: Option<HString<64>>,
}

pub struct FwNvs {
    storage: FlashStorage<'static>,
    cache: NvsCache,
}

impl FwNvs {
    /// Boot-time init: take the FLASH peripheral, read + validate the
    /// existing record (if any), build the in-memory cache.
    pub fn init(flash: FLASH<'static>) -> Self {
        let mut storage = FlashStorage::new(flash).multicore_auto_park();
        let cache = read_cache(&mut storage).unwrap_or_default();
        Self { storage, cache }
    }

    fn persist(&mut self) {
        if let Err(e) = write_cache(&mut self.storage, &self.cache) {
            esp_println::println!("nvs: persist failed: {:?}", e);
        }
    }

    /// Persist the device's claim code so a subsequent boot reuses
    /// it and the backend's claim-by-code lookup can find it. Called
    /// from the runtime after `POST /api/device/register` returns a
    /// fresh code on first boot.
    pub fn save_claim_code(&mut self, code: &str) {
        if self.cache.claim_code.as_ref().map(|s| s.as_str()) == Some(code) {
            return;
        }
        self.cache.claim_code = into_hstring::<16>(code);
        self.persist();
    }

    /// Persist the bearer token returned by
    /// `POST /api/device/register`. Called once per unclaimed device on
    /// first cold boot; subsequent boots load it via
    /// `NvsStore::load_device_token` and skip the register call.
    pub fn save_device_token(&mut self, token: &str) {
        if self.cache.device_token.as_ref().map(|s| s.as_str()) == Some(token) {
            return;
        }
        self.cache.device_token = into_hstring::<64>(token);
        self.persist();
    }

    /// Persist the backend-assigned UUID. Stable across reboots — once
    /// the backend has issued this UUID for this device's MAC, every
    /// re-register call returns the same value. Used by the boot
    /// screen to display a stable device identity even before adoption.
    pub fn save_device_uuid(&mut self, uuid: &str) {
        if self.cache.device_uuid.as_ref().map(|s| s.as_str()) == Some(uuid) {
            return;
        }
        self.cache.device_uuid = into_hstring::<48>(uuid);
        self.persist();
    }

    /// Persist the user-supplied friendly name returned in `GET /state`.
    /// Refreshed on every state poll so a dashboard rename surfaces on
    /// the next reboot's boot screen.
    pub fn save_device_name(&mut self, name: &str) {
        if self.cache.device_name.as_ref().map(|s| s.as_str()) == Some(name) {
            return;
        }
        self.cache.device_name = into_hstring::<64>(name);
        self.persist();
    }

    /// Try to migrate a `prov.bin` blob from the prov partition into NVS.
    /// Returns `true` if a valid blob was found and migrated; the prov region
    /// is then erased so the next boot doesn't re-migrate (and the WiFi
    /// password doesn't sit on a separately-flashable partition forever).
    pub fn try_migrate_prov(&mut self) -> bool {
        let prov = match read_prov(&mut self.storage) {
            Ok(p) => p,
            Err(NvsError::BadMagic) | Err(NvsError::BadCrc) => {
                // No prov blob present (or corrupted) — normal on every
                // subsequent boot after migration.
                return false;
            }
            Err(e) => {
                esp_println::println!("nvs: prov read error: {:?}", e);
                return false;
            }
        };
        esp_println::println!(
            "nvs: migrating prov → nvs (ssid={:?}, backend_url={:?})",
            prov.ssid.as_deref(),
            prov.backend_url.as_deref(),
        );
        if let Some(s) = prov.ssid {
            self.cache.ssid = into_hstring::<32>(&s);
        }
        if let Some(s) = prov.password {
            self.cache.password = into_hstring::<64>(&s);
        }
        if let Some(s) = prov.backend_url {
            self.cache.backend_url = into_hstring::<128>(&s);
        }
        if let Some(s) = prov.claim_code {
            self.cache.claim_code = into_hstring::<16>(&s);
        }
        // Dev flag is sticky once migrated — flashing a fresh prov.bin
        // without --dev resets it back to false on the next migration
        // (which is what you want when promoting a board from dev to
        // production: re-flash with a non-dev prov bundle and the next
        // boot picks up the new flag).
        self.cache.is_dev_build = prov.is_dev_build;
        if self.cache.is_dev_build {
            esp_println::println!("nvs: device flashed as DEV build — OTA suppressed");
        }
        self.persist();
        if let Err(e) = erase_prov(&mut self.storage) {
            esp_println::println!("nvs: prov erase failed: {:?}", e);
        }
        true
    }

    /// True if NVS has the minimum bootstrap state — at least a WiFi SSID.
    /// Used by `provisioning::resolve` to decide whether the device can
    /// proceed straight to the main polling loop or needs to drop into
    /// captive-portal setup.
    pub fn is_provisioned(&self) -> bool {
        self.cache.ssid.is_some()
    }

}

impl NvsStore for FwNvs {
    fn load_wifi_creds(&self) -> Option<WifiCreds> {
        let ssid = self.cache.ssid.clone()?;
        let password = self.cache.password.clone().unwrap_or_default();
        Some(WifiCreds { ssid, password })
    }

    fn load_device_token(&self) -> Option<String> {
        self.cache.device_token.as_ref().map(|h| h.as_str().to_string())
    }

    fn load_last_render_hash(&self) -> Option<u64> {
        self.cache.last_render_hash
    }

    fn save_last_render_hash(&mut self, hash: u64) {
        if self.cache.last_render_hash == Some(hash) {
            return;
        }
        self.cache.last_render_hash = Some(hash);
        self.persist();
    }

    fn load_backend_url(&self) -> Option<String> {
        self.cache.backend_url.as_ref().map(|h| h.as_str().to_string())
    }

    fn load_is_dev_build(&self) -> bool {
        self.cache.is_dev_build
    }

    fn load_claim_code(&self) -> Option<String> {
        self.cache.claim_code.as_ref().map(|s| s.as_str().to_string())
    }

    fn load_device_uuid(&self) -> Option<String> {
        self.cache.device_uuid.as_ref().map(|s| s.as_str().to_string())
    }

    fn load_device_name(&self) -> Option<String> {
        self.cache.device_name.as_ref().map(|s| s.as_str().to_string())
    }

    fn save_device_token(&mut self, token: &str) {
        FwNvs::save_device_token(self, token);
    }

    fn save_claim_code(&mut self, code: &str) {
        FwNvs::save_claim_code(self, code);
    }

    fn save_device_uuid(&mut self, uuid: &str) {
        FwNvs::save_device_uuid(self, uuid);
    }

    fn save_device_name(&mut self, name: &str) {
        FwNvs::save_device_name(self, name);
    }

    fn clear_device_token(&mut self) {
        if self.cache.device_token.is_none() {
            return;
        }
        self.cache.device_token = None;
        self.persist();
    }

    fn clear_claim_code(&mut self) {
        if self.cache.claim_code.is_none() {
            return;
        }
        self.cache.claim_code = None;
        self.persist();
    }
}

// ── Public mutators used by provisioning ──────────────────────────────────────
//
// Once provisioning runs through the runtime layer we'll wire these through
// `FwNvs` directly. For now the partitions reader/writer below is enough.

#[allow(dead_code)]
pub fn save_wifi_creds(_ssid: &str, _password: &str) {
    esp_println::println!("nvs: save_wifi_creds not yet wired through provisioning");
}

pub fn save_backend_url(_url: &str) {
    esp_println::println!("nvs: save_backend_url not yet wired through provisioning");
}

pub fn save_pending_claim_code(_code: &str) {
    esp_println::println!("nvs: save_pending_claim_code not yet wired through provisioning");
}

pub fn clear_pending_claim_code() {
    esp_println::println!("nvs: clear_pending_claim_code not yet wired through provisioning");
}

pub fn load_pending_claim_code() -> Option<HString<16>> {
    // Read-only sniff used by boot.rs to log "we have a pending claim code".
    // Doesn't need the FwNvs handle — reads its own copy from flash.
    let mut storage = FlashStorage::new(unsafe { FLASH::steal() }).multicore_auto_park();
    read_cache(&mut storage).ok().and_then(|c| c.claim_code)
}

pub fn load_wifi_creds_raw() -> Option<(HString<32>, HString<64>)> {
    let mut storage = FlashStorage::new(unsafe { FLASH::steal() }).multicore_auto_park();
    read_cache(&mut storage)
        .ok()
        .and_then(|c| Some((c.ssid?, c.password.unwrap_or_default())))
}

/// Wipe NVS — called on long-press factory reset. Writes a zeroed record so
/// the next boot's CRC check fails and the cache initialises empty.
pub fn factory_reset() {
    esp_println::println!("nvs: factory reset");
    let mut storage = FlashStorage::new(unsafe { FLASH::steal() }).multicore_auto_park();
    let zeros = [0u8; NVS_RECORD_SIZE];
    let _ = storage.write(NVS_PARTITION_OFFSET, &zeros);
}

/// Stub claim flow. Real impl renders a 6-char Crockford base32 code on the
/// panel, hosts a WiFi captive portal, then POSTs to /api/device/claim once
/// credentials arrive. Blocks until the dashboard consumes the code.
pub fn claim_flow_stub(_board: crate::boards::BoardConfig) {
    esp_println::println!("nvs: claim_flow_stub — render code, capture WiFi");
}

// ── Encode / decode internals ─────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
enum NvsError {
    Io,
    BadMagic,
    BadCrc,
    Truncated,
    OverlongField,
}

impl From<esp_storage::FlashStorageError> for NvsError {
    fn from(_: esp_storage::FlashStorageError) -> Self {
        NvsError::Io
    }
}

fn read_cache(storage: &mut FlashStorage<'static>) -> Result<NvsCache, NvsError> {
    let mut buf = [0u8; NVS_RECORD_SIZE];
    storage.read(NVS_PARTITION_OFFSET, &mut buf)?;

    if buf[0..4] != NVS_MAGIC {
        return Err(NvsError::BadMagic);
    }
    let version = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if version != NVS_VERSION {
        return Err(NvsError::BadMagic);
    }
    let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let actual_crc = crc32(&buf[NVS_HEADER_LEN..]);
    if stored_crc != actual_crc {
        return Err(NvsError::BadCrc);
    }
    parse_tlv(&buf[NVS_HEADER_LEN..])
}

fn write_cache(
    storage: &mut FlashStorage<'static>,
    cache: &NvsCache,
) -> Result<(), NvsError> {
    let mut buf = [0u8; NVS_RECORD_SIZE];
    buf[0..4].copy_from_slice(&NVS_MAGIC);
    buf[4..8].copy_from_slice(&NVS_VERSION.to_le_bytes());

    let payload_len = encode_tlv(cache, &mut buf[NVS_HEADER_LEN..])?;
    // Zero the unused tail so the CRC is deterministic.
    for slot in &mut buf[NVS_HEADER_LEN + payload_len..NVS_RECORD_SIZE] {
        *slot = 0;
    }

    let crc = crc32(&buf[NVS_HEADER_LEN..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());

    storage.write(NVS_PARTITION_OFFSET, &buf).map_err(NvsError::from)
}

fn encode_tlv(cache: &NvsCache, payload: &mut [u8]) -> Result<usize, NvsError> {
    let mut pos = 0;
    if let Some(s) = &cache.device_token {
        pos = write_record(payload, pos, TAG_DEVICE_TOKEN, s.as_bytes())?;
    }
    if let Some(s) = &cache.ssid {
        pos = write_record(payload, pos, TAG_WIFI_SSID, s.as_bytes())?;
    }
    if let Some(s) = &cache.password {
        pos = write_record(payload, pos, TAG_WIFI_PASSWORD, s.as_bytes())?;
    }
    if let Some(s) = &cache.backend_url {
        pos = write_record(payload, pos, TAG_BACKEND_URL, s.as_bytes())?;
    }
    if let Some(s) = &cache.claim_code {
        pos = write_record(payload, pos, TAG_CLAIM_CODE, s.as_bytes())?;
    }
    if let Some(h) = cache.last_render_hash {
        pos = write_record(payload, pos, TAG_LAST_RENDER_HASH, &h.to_le_bytes())?;
    }
    if cache.is_dev_build {
        // Single-byte payload; absence (or any non-0x01 value) means
        // production. Only emit the tag when true so an existing
        // production NVS record stays byte-identical to the pre-dev-flag
        // layout (cheap forward-compat for old firmware decoding it).
        pos = write_record(payload, pos, TAG_IS_DEV_BUILD, &[1u8])?;
    }
    if let Some(s) = &cache.device_uuid {
        pos = write_record(payload, pos, TAG_DEVICE_UUID, s.as_bytes())?;
    }
    if let Some(s) = &cache.device_name {
        pos = write_record(payload, pos, TAG_DEVICE_NAME, s.as_bytes())?;
    }
    // tag 0 terminator
    if pos < payload.len() {
        payload[pos] = 0;
        pos += 1;
    }
    Ok(pos)
}

fn write_record(buf: &mut [u8], pos: usize, tag: u8, value: &[u8]) -> Result<usize, NvsError> {
    if pos + 3 + value.len() > buf.len() {
        return Err(NvsError::OverlongField);
    }
    buf[pos] = tag;
    let len = value.len() as u16;
    buf[pos + 1] = (len >> 8) as u8;
    buf[pos + 2] = (len & 0xFF) as u8;
    buf[pos + 3..pos + 3 + value.len()].copy_from_slice(value);
    Ok(pos + 3 + value.len())
}

fn parse_tlv(payload: &[u8]) -> Result<NvsCache, NvsError> {
    let mut cache = NvsCache::default();
    let mut i = 0;
    while i < payload.len() {
        let tag = payload[i];
        if tag == 0 {
            break;
        }
        if i + 3 > payload.len() {
            return Err(NvsError::Truncated);
        }
        let len = ((payload[i + 1] as usize) << 8) | (payload[i + 2] as usize);
        let val_start = i + 3;
        let val_end = val_start + len;
        if val_end > payload.len() {
            return Err(NvsError::Truncated);
        }
        let value = &payload[val_start..val_end];
        // Binary tags decode before UTF-8.
        if tag == TAG_LAST_RENDER_HASH {
            if value.len() == 8 {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(value);
                cache.last_render_hash = Some(u64::from_le_bytes(buf));
            }
            i = val_end;
            continue;
        }
        if tag == TAG_IS_DEV_BUILD {
            cache.is_dev_build = !value.is_empty() && value[0] != 0;
            i = val_end;
            continue;
        }
        let value_str = core::str::from_utf8(value).map_err(|_| NvsError::Truncated)?;
        match tag {
            TAG_DEVICE_TOKEN => cache.device_token = into_hstring::<64>(value_str),
            TAG_WIFI_SSID => cache.ssid = into_hstring::<32>(value_str),
            TAG_WIFI_PASSWORD => cache.password = into_hstring::<64>(value_str),
            TAG_BACKEND_URL => cache.backend_url = into_hstring::<128>(value_str),
            TAG_CLAIM_CODE => cache.claim_code = into_hstring::<16>(value_str),
            TAG_DEVICE_UUID => cache.device_uuid = into_hstring::<48>(value_str),
            TAG_DEVICE_NAME => cache.device_name = into_hstring::<64>(value_str),
            _ => {} // unknown tag — silently skip
        }
        i = val_end;
    }
    Ok(cache)
}

fn into_hstring<const N: usize>(s: &str) -> Option<HString<N>> {
    let mut h: HString<N> = HString::new();
    h.push_str(s).ok()?;
    Some(h)
}

/// CRC32 with polynomial 0xEDB88320 (the same as IEEE 802.3 / gzip). No table —
/// we only checksum a 500-byte payload at most once per save, so the loop cost
/// is negligible against a flash erase + write.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ── prov partition I/O ────────────────────────────────────────────────────────

/// Parsed prov-blob contents. Same field set as `NvsCache` minus the device-
/// only fields; the prov path is *only* for what an installer can know
/// ahead of time (network creds + backend URL).
#[derive(Default)]
struct ProvData {
    ssid: Option<String>,
    password: Option<String>,
    backend_url: Option<String>,
    claim_code: Option<String>,
    /// Set by `provtool gen --dev`. When `true`, this device is in dev
    /// mode and the firmware suppresses the GitHub-release OTA check.
    is_dev_build: bool,
}

/// Read the prov partition and parse it. `BadMagic` or `BadCrc` is the
/// "nothing there" signal — every boot after migration.
fn read_prov(storage: &mut FlashStorage<'static>) -> Result<ProvData, NvsError> {
    // The prov region is 16 KB, but we only ever populate the header + a
    // few hundred bytes of TLV. Read the first 1 KB — plenty for our payload
    // ceiling and small enough to keep on the stack of the boot task.
    let mut buf = [0u8; 1024];
    storage.read(PROV_PARTITION_OFFSET, &mut buf)?;
    if buf[0..4] != PROV_MAGIC {
        return Err(NvsError::BadMagic);
    }
    let version = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if version != PROV_VERSION {
        return Err(NvsError::BadMagic);
    }
    let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);

    // The CRC was computed by provtool over everything after the header up
    // to the end of the prov-blob region the firmware reserves for it. We
    // read 1 KB; that has to match what provtool checksums. Restrict to the
    // same 1 KB span here.
    let actual_crc = crc32(&buf[PROV_HEADER_LEN..]);
    if stored_crc != actual_crc {
        // Try a smaller checksum window — older provtool versions might
        // have checksummed only up to the terminator. Walk the TLV records
        // to find the terminator + checksum just those bytes.
        let payload_end = find_tlv_end(&buf[PROV_HEADER_LEN..])
            .ok_or(NvsError::BadCrc)?;
        let alt_crc = crc32(&buf[PROV_HEADER_LEN..PROV_HEADER_LEN + payload_end]);
        if alt_crc != stored_crc {
            return Err(NvsError::BadCrc);
        }
    }

    parse_prov_tlv(&buf[PROV_HEADER_LEN..])
}

/// Walk a TLV stream and return the byte offset just past the `tag == 0`
/// terminator. Used by `read_prov` to relax the CRC window in case provtool
/// only checksummed the populated portion.
fn find_tlv_end(payload: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < payload.len() {
        let tag = payload[i];
        if tag == 0 {
            return Some(i + 1);
        }
        if i + 3 > payload.len() {
            return None;
        }
        let len = ((payload[i + 1] as usize) << 8) | (payload[i + 2] as usize);
        i = i + 3 + len;
    }
    None
}

fn parse_prov_tlv(payload: &[u8]) -> Result<ProvData, NvsError> {
    let mut prov = ProvData::default();
    let mut i = 0;
    while i < payload.len() {
        let tag = payload[i];
        if tag == 0 {
            break;
        }
        if i + 3 > payload.len() {
            return Err(NvsError::Truncated);
        }
        let len = ((payload[i + 1] as usize) << 8) | (payload[i + 2] as usize);
        let val_start = i + 3;
        let val_end = val_start + len;
        if val_end > payload.len() {
            return Err(NvsError::Truncated);
        }
        let value = &payload[val_start..val_end];
        // Binary tags decode before UTF-8 — they aren't valid UTF-8 by
        // construction (single byte 0x01 in dev-flag's case).
        if tag == TAG_IS_DEV_BUILD {
            prov.is_dev_build = !value.is_empty() && value[0] != 0;
            i = val_end;
            continue;
        }
        let s = core::str::from_utf8(value)
            .map_err(|_| NvsError::Truncated)?
            .to_string();
        match tag {
            TAG_WIFI_SSID => prov.ssid = Some(s),
            TAG_WIFI_PASSWORD => prov.password = Some(s),
            TAG_BACKEND_URL => prov.backend_url = Some(s),
            TAG_CLAIM_CODE => prov.claim_code = Some(s),
            _ => {} // unknown tag — skip
        }
        i = val_end;
    }
    Ok(prov)
}

/// Zero the prov partition (4 KB at a time — flash sector size) so the next
/// boot's CRC check fails and we don't re-migrate stale creds.
fn erase_prov(storage: &mut FlashStorage<'static>) -> Result<(), NvsError> {
    let zeros = [0u8; 4096];
    let mut offset = PROV_PARTITION_OFFSET;
    let end = PROV_PARTITION_OFFSET + PROV_BLOB_SIZE as u32;
    while offset < end {
        storage.write(offset, &zeros)?;
        offset += zeros.len() as u32;
    }
    Ok(())
}

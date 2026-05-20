//! Persisted device state, written to the `nvs` partition declared in
//! `flash/partition-table.csv` (24 KB starting at 0x9000).
//!
//! For simplicity we store everything in a single fixed-layout record at the
//! start of the partition. A real M4 hardening pass should rotate between two
//! sectors with a generation counter to make writes crash-safe; for now any
//! write goes through `FlashStorage::write` which does a read-modify-write of
//! the underlying 4 KB sector, so a power-loss mid-write corrupts the record.
//! We tolerate this in MVP because the worst case is "fall back to captive portal
//! on next boot" — no permanent damage.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;
use heapless::String;
use paperanywhere_proto::provisioning::crc32;

use crate::boards::BoardConfig;

const NVS_PARTITION_OFFSET: u32 = 0x9000;
const RECORD_SIZE: usize = 512;
const ERASED_BYTE: u8 = 0xFF;

const MAGIC: [u8; 4] = *b"PAv1";

// Field offsets within the 512-byte record.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_FLAGS: usize = 5;
const OFF_SSID_LEN: usize = 6;
const OFF_PASSWORD_LEN: usize = 7;
const OFF_BACKEND_URL_LEN: usize = 8;
const OFF_DEVICE_TOKEN_LEN: usize = 9;
const OFF_CLAIM_CODE_LEN: usize = 10;
const OFF_RESERVED: usize = 11; // 5 bytes
const OFF_SSID: usize = 16;
const OFF_PASSWORD: usize = 48;
const OFF_BACKEND_URL: usize = 112;
const OFF_DEVICE_ID: usize = 240;
const OFF_DEVICE_TOKEN: usize = 256;
const OFF_CLAIM_CODE: usize = 320;
const OFF_CRC: usize = 508;
const CRC_REGION_END: usize = 508;

const FLAG_HAS_DEVICE_TOKEN: u8 = 1 << 0;
const FLAG_HAS_BACKEND_URL: u8 = 1 << 1;
const FLAG_HAS_CLAIM_CODE: u8 = 1 << 2;

const MAX_SSID: usize = 32;
const MAX_PASSWORD: usize = 64;
const MAX_BACKEND_URL: usize = 128;
const MAX_DEVICE_TOKEN: usize = 64;
const MAX_CLAIM_CODE: usize = 16;

#[derive(Debug, Clone, Default)]
struct Record {
    ssid: String<MAX_SSID>,
    password: String<MAX_PASSWORD>,
    backend_url: Option<String<MAX_BACKEND_URL>>,
    device_id: Option<[u8; 16]>,
    device_token_hex: Option<String<MAX_DEVICE_TOKEN>>,
    claim_code_pending: Option<String<MAX_CLAIM_CODE>>,
}

#[derive(Debug, Clone)]
pub struct PersistedState {
    pub device_id_bytes: [u8; 16],
    pub device_token_hex: heapless::String<64>,
    pub heartbeat_interval_sec: u32,
}

fn read_record() -> Option<Record> {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; RECORD_SIZE];
    flash.read(NVS_PARTITION_OFFSET, &mut buf).ok()?;
    if buf.iter().all(|&b| b == ERASED_BYTE) {
        return None;
    }
    if buf[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
        return None;
    }
    let expected = u32::from_le_bytes(buf[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
    let actual = crc32(&buf[..CRC_REGION_END]);
    if expected != actual {
        esp_println::println!("nvs: CRC mismatch (record corrupt or partially written)");
        return None;
    }

    let flags = buf[OFF_FLAGS];
    let ssid = read_string(&buf, OFF_SSID, buf[OFF_SSID_LEN] as usize)?;
    let password = read_string(&buf, OFF_PASSWORD, buf[OFF_PASSWORD_LEN] as usize)?;
    let backend_url = if flags & FLAG_HAS_BACKEND_URL != 0 {
        read_string::<MAX_BACKEND_URL>(&buf, OFF_BACKEND_URL, buf[OFF_BACKEND_URL_LEN] as usize)
    } else {
        None
    };
    let device_id = if flags & FLAG_HAS_DEVICE_TOKEN != 0 {
        let mut id = [0u8; 16];
        id.copy_from_slice(&buf[OFF_DEVICE_ID..OFF_DEVICE_ID + 16]);
        Some(id)
    } else {
        None
    };
    let device_token_hex = if flags & FLAG_HAS_DEVICE_TOKEN != 0 {
        read_string::<MAX_DEVICE_TOKEN>(&buf, OFF_DEVICE_TOKEN, buf[OFF_DEVICE_TOKEN_LEN] as usize)
    } else {
        None
    };
    let claim_code_pending = if flags & FLAG_HAS_CLAIM_CODE != 0 {
        read_string::<MAX_CLAIM_CODE>(&buf, OFF_CLAIM_CODE, buf[OFF_CLAIM_CODE_LEN] as usize)
    } else {
        None
    };

    Some(Record {
        ssid,
        password,
        backend_url,
        device_id,
        device_token_hex,
        claim_code_pending,
    })
}

fn write_record(rec: &Record) -> Result<(), esp_storage::FlashStorageError> {
    let mut buf = [0u8; RECORD_SIZE];
    buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC);
    buf[OFF_VERSION] = 1;

    let mut flags = 0u8;
    if rec.backend_url.is_some() { flags |= FLAG_HAS_BACKEND_URL; }
    if rec.device_id.is_some() && rec.device_token_hex.is_some() { flags |= FLAG_HAS_DEVICE_TOKEN; }
    if rec.claim_code_pending.is_some() { flags |= FLAG_HAS_CLAIM_CODE; }
    buf[OFF_FLAGS] = flags;

    write_string(&mut buf, OFF_SSID, MAX_SSID, OFF_SSID_LEN, &rec.ssid);
    write_string(&mut buf, OFF_PASSWORD, MAX_PASSWORD, OFF_PASSWORD_LEN, &rec.password);
    if let Some(url) = &rec.backend_url {
        write_string(&mut buf, OFF_BACKEND_URL, MAX_BACKEND_URL, OFF_BACKEND_URL_LEN, url);
    }
    if let Some(id) = &rec.device_id {
        buf[OFF_DEVICE_ID..OFF_DEVICE_ID + 16].copy_from_slice(id);
    }
    if let Some(tok) = &rec.device_token_hex {
        write_string(&mut buf, OFF_DEVICE_TOKEN, MAX_DEVICE_TOKEN, OFF_DEVICE_TOKEN_LEN, tok);
    }
    if let Some(code) = &rec.claim_code_pending {
        write_string(&mut buf, OFF_CLAIM_CODE, MAX_CLAIM_CODE, OFF_CLAIM_CODE_LEN, code);
    }

    let crc = crc32(&buf[..CRC_REGION_END]);
    buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());

    let mut flash = FlashStorage::new();
    flash.write(NVS_PARTITION_OFFSET, &buf)
}

fn read_string<const N: usize>(buf: &[u8], off: usize, len: usize) -> Option<String<N>> {
    if len > N || off + len > buf.len() { return None; }
    let slice = &buf[off..off + len];
    let s = core::str::from_utf8(slice).ok()?;
    let mut out: String<N> = String::new();
    out.push_str(s).ok()?;
    Some(out)
}

fn write_string<const N: usize>(buf: &mut [u8], off: usize, cap: usize, len_off: usize, s: &String<N>) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap);
    buf[off..off + n].copy_from_slice(&bytes[..n]);
    buf[len_off] = n as u8;
}

// ── Public API ────────────────────────────────────────────────────────────

pub fn load_wifi_creds() -> Option<(String<32>, String<64>)> {
    let r = read_record()?;
    if r.ssid.is_empty() { return None; }
    Some((r.ssid, r.password))
}

pub fn save_wifi_creds(ssid: &str, password: &str) {
    let mut rec = read_record().unwrap_or_default();
    let mut s: String<MAX_SSID> = String::new();
    let _ = s.push_str(ssid);
    let mut p: String<MAX_PASSWORD> = String::new();
    let _ = p.push_str(password);
    rec.ssid = s;
    rec.password = p;
    if let Err(e) = write_record(&rec) {
        esp_println::println!("nvs: save_wifi_creds write failed: {e:?}");
    }
}

pub fn load_backend_url() -> Option<String<128>> {
    read_record().and_then(|r| r.backend_url)
}

pub fn save_backend_url(url: &str) {
    let mut rec = read_record().unwrap_or_default();
    let mut s: String<MAX_BACKEND_URL> = String::new();
    let _ = s.push_str(url);
    rec.backend_url = Some(s);
    if let Err(e) = write_record(&rec) {
        esp_println::println!("nvs: save_backend_url write failed: {e:?}");
    }
}

pub fn load_pending_claim_code() -> Option<String<16>> {
    read_record().and_then(|r| r.claim_code_pending)
}

pub fn save_pending_claim_code(code: &str) {
    let mut rec = read_record().unwrap_or_default();
    let mut s: String<MAX_CLAIM_CODE> = String::new();
    let _ = s.push_str(code);
    rec.claim_code_pending = Some(s);
    if let Err(e) = write_record(&rec) {
        esp_println::println!("nvs: save_pending_claim_code write failed: {e:?}");
    }
}

pub fn clear_pending_claim_code() {
    let mut rec = match read_record() {
        Some(r) => r,
        None => return,
    };
    rec.claim_code_pending = None;
    let _ = write_record(&rec);
}

pub fn load_device_token() -> Option<PersistedState> {
    let r = read_record()?;
    let id = r.device_id?;
    let tok = r.device_token_hex?;
    Some(PersistedState {
        device_id_bytes: id,
        device_token_hex: tok,
        heartbeat_interval_sec: 60,
    })
}

pub fn save_device_token(state: &PersistedState) {
    let mut rec = read_record().unwrap_or_default();
    rec.device_id = Some(state.device_id_bytes);
    rec.device_token_hex = Some(state.device_token_hex.clone());
    if let Err(e) = write_record(&rec) {
        esp_println::println!("nvs: save_device_token write failed: {e:?}");
    }
}

/// Wipe NVS — called on long-press factory reset. Writes 0xFF across the record.
pub fn factory_reset() {
    let mut flash = FlashStorage::new();
    let zero_pad = [ERASED_BYTE; RECORD_SIZE];
    if let Err(e) = flash.write(NVS_PARTITION_OFFSET, &zero_pad) {
        esp_println::println!("nvs: factory reset write failed: {e:?}");
    }
    esp_println::println!("nvs: factory reset complete");
}

/// Stub claim flow. Renders a 6-char Crockford base32 code on the panel, hosts a
/// WiFi captive portal, then POSTs to `/api/device/claim` once credentials
/// arrive. M4 work — real impl pulls from `crate::wifi` + `crate::ws_client`.
pub fn claim_flow_stub(_board: BoardConfig) {
    esp_println::println!("nvs: claim_flow_stub — render code + capture WiFi");
}

//! Provisioning paths. On boot we try, in priority order:
//!   1. **`prov` partition** — a `.bin` flashed alongside firmware via `espflash`.
//!      On success: migrate into NVS, erase the partition, proceed.
//!   2. **SD card `wifi.conf`** — board-gated. Only checked on boards with `has_sd_card`.
//!      On success: migrate into NVS, optionally delete the file, proceed.
//!   3. **Existing NVS state** — re-boots after first provisioning fall here.
//!   4. **Captive portal** — last-resort interactive setup (AP mode on the device).
//!
//! Factory reset (long-press a button at boot) wipes NVS, which puts the device
//! back at step 1 — flash a new prov bundle, or drop a new SD config, or use captive portal.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::FlashStorage;
use paperanywhere_proto::{ProvBlob, provisioning::BLOB_SIZE};

use crate::boards::BoardConfig;

/// Absolute flash offset of the `prov` partition. Must match the partition table
/// in `flash/partition-table.csv` (currently `prov, data, nvs, 0x12000, 16K`).
/// Kept as a compile-time constant so a misconfiguration in the partition table
/// surfaces as a wrong read offset rather than a runtime lookup failure.
pub const PROV_PARTITION_OFFSET: u32 = 0x12000;
pub const PROV_PARTITION_SIZE: u32 = 16 * 1024;

/// Sentinel value for an unwritten 4 KB sector — every byte is 0xFF on a freshly-erased
/// NOR flash region, so a fully-0xFF blob means "no provisioning flashed".
const ERASED_BYTE: u8 = 0xFF;

#[derive(Debug)]
pub enum SetupPath {
    /// Device read a `prov` partition blob and migrated it into NVS.
    FlashPartition,
    /// Device read `wifi.conf` from SD and migrated it into NVS.
    SdCard,
    /// NVS already had credentials from a prior provisioning.
    AlreadyProvisioned,
    /// No automatic path available; entered captive-portal AP mode.
    CaptivePortal,
    /// No credentials yet and no fallback ran. Device will show the claim code on the panel.
    NotProvisioned,
}

/// Walk the priority list. Returns the path that ultimately succeeded.
pub fn resolve(board: BoardConfig) -> SetupPath {
    if let Some(blob) = read_prov_partition() {
        esp_println::println!("provisioning: read prov partition (ssid={})", blob.ssid);
        if migrate_to_nvs(&blob).is_ok() {
            if let Err(e) = erase_prov_partition() {
                esp_println::println!("provisioning: erase failed: {e:?}");
            }
            return SetupPath::FlashPartition;
        }
    }

    if board.has_sd_card {
        if let Some(blob) = read_sd_wifi_conf() {
            esp_println::println!("provisioning: read SD wifi.conf (ssid={})", blob.ssid);
            if migrate_to_nvs(&blob).is_ok() {
                return SetupPath::SdCard;
            }
        }
    }

    if crate::nvs::load_wifi_creds().is_some() {
        return SetupPath::AlreadyProvisioned;
    }

    if try_captive_portal(board).is_ok() {
        return SetupPath::CaptivePortal;
    }

    SetupPath::NotProvisioned
}

/// Read + verify the `prov` partition. Returns `None` when the partition is
/// entirely erased, has a bad magic, or fails CRC.
fn read_prov_partition() -> Option<ProvBlob> {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; BLOB_SIZE];
    flash
        .read(PROV_PARTITION_OFFSET, &mut buf)
        .map_err(|e| esp_println::println!("provisioning: read failed: {e:?}"))
        .ok()?;
    if buf.iter().all(|&b| b == ERASED_BYTE) {
        return None;
    }
    match ProvBlob::decode(&buf) {
        Ok(b) => Some(b),
        Err(e) => {
            esp_println::println!("provisioning: blob decode failed: {e:?}");
            None
        }
    }
}

/// Erase the prov partition after a successful migration so the WiFi password
/// doesn't sit on a separately-flashable region forever.
///
/// We write 0xFF over the BLOB_SIZE prefix instead of doing a sector erase so
/// the operation is portable across esp-storage versions; FlashStorage::write
/// performs a read-modify-write on the underlying sector(s).
fn erase_prov_partition() -> Result<(), esp_storage::FlashStorageError> {
    let mut flash = FlashStorage::new();
    let zero_pad = [ERASED_BYTE; BLOB_SIZE];
    flash.write(PROV_PARTITION_OFFSET, &zero_pad)
}

/// Read `/wifi.conf` from the SD card's root. M4 stub for now — needs
/// `embedded-sdmmc` mount + open + read, which is board-specific GPIO wiring.
/// When implemented this calls `crate::sd_config::parse_wifi_conf(...)`.
fn read_sd_wifi_conf() -> Option<ProvBlob> {
    None
}

/// Persist creds into NVS. Returns `Ok` once both ssid+password are durable.
fn migrate_to_nvs(blob: &ProvBlob) -> Result<(), ()> {
    crate::nvs::save_wifi_creds(&blob.ssid, &blob.password);
    if let Some(url) = &blob.backend_url {
        crate::nvs::save_backend_url(url);
    }
    if let Some(code) = &blob.claim_code {
        crate::nvs::save_pending_claim_code(code);
    }
    Ok(())
}

fn try_captive_portal(_board: BoardConfig) -> Result<(), ()> {
    esp_println::println!("provisioning: captive portal stub — would host AP `paperanywhere-XXXX`");
    Err(())
}

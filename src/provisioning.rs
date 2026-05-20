//! Provisioning resolver. On every cold boot the firmware looks for WiFi
//! credentials + claim code in this priority order:
//!
//!   1. `prov` partition (4 KB blob flashed alongside firmware)
//!   2. SD card `wifi.conf` (board-gated to those with SD slots)
//!   3. Existing NVS state (post-first-boot path)
//!   4. Captive portal (last-resort interactive fallback)
//!
//! Whichever path fires first migrates the credentials into NVS and (for the
//! prov partition) erases the source so the WiFi password doesn't sit on a
//! separately-flashable partition forever.

use crate::boards::BoardConfig;

#[derive(Debug)]
pub enum SetupPath {
    /// Read a valid prov-partition blob; creds migrated to NVS, partition erased.
    FlashPartition,
    /// Read `wifi.conf` from SD card; creds migrated to NVS.
    SdCard,
    /// NVS already had creds from a previous successful boot.
    AlreadyProvisioned,
    /// Captive portal captured creds interactively.
    CaptivePortal,
    /// No credentials anywhere. Caller renders the claim code and halts.
    NotProvisioned,
}

/// Walk the priority list. Returns the path that ultimately succeeded.
pub fn resolve(board: BoardConfig) -> SetupPath {
    if let Some(prov) = read_prov_partition() {
        esp_println::println!("provisioning: read prov partition (ssid={})", prov.ssid);
        migrate_to_nvs(&prov);
        let _ = erase_prov_partition();
        return SetupPath::FlashPartition;
    }

    if board.has_sd_card {
        if let Some(prov) = crate::sd_config::read_wifi_conf() {
            esp_println::println!("provisioning: read SD wifi.conf");
            migrate_to_nvs(&prov);
            return SetupPath::SdCard;
        }
    }

    if crate::nvs::load_wifi_creds().is_some() {
        return SetupPath::AlreadyProvisioned;
    }

    if crate::wifi::captive_portal().is_ok() {
        return SetupPath::CaptivePortal;
    }

    SetupPath::NotProvisioned
}

/// In-memory representation of the `prov` partition blob.
#[derive(Debug, Clone)]
pub struct ProvData {
    pub ssid: alloc::string::String,
    pub password: alloc::string::String,
    pub backend_url: Option<alloc::string::String>,
    pub claim_code: Option<alloc::string::String>,
}

/// Read + verify the prov partition. M4 wires esp-storage; today returns `None`.
fn read_prov_partition() -> Option<ProvData> {
    None
}

/// Zero out the prov partition after a successful migration. M4 wires esp-storage.
fn erase_prov_partition() -> Result<(), ()> {
    Ok(())
}

fn migrate_to_nvs(p: &ProvData) {
    crate::nvs::save_wifi_creds(&p.ssid, &p.password);
    if let Some(url) = &p.backend_url {
        crate::nvs::save_backend_url(url);
    }
    if let Some(code) = &p.claim_code {
        crate::nvs::save_pending_claim_code(code);
    }
}

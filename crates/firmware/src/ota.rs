//! Over-the-air firmware updates.
//!
//! Implements [`FirmwareUpdater`] by:
//!
//! 1. Locating the inactive `ota_X` app partition via the ESP-IDF partition
//!    table (parsed by `esp-bootloader-esp-idf`).
//! 2. Streaming the new image from the backend's blob URL straight into
//!    that partition, computing SHA-256 inline so we can reject a
//!    corrupted download before touching `otadata`.
//! 3. On success: updating `otadata` to point at the new slot with state
//!    [`OtaImageState::New`], then triggering a software reset so the
//!    second-stage bootloader picks the new image.
//!
//! On the *next* successful wake the runtime should call
//! [`mark_current_app_valid`] to graduate the slot from `New` →
//! `PendingVerify` → `Valid`. If we crash or never reach that point within
//! a few boots the bootloader auto-rolls back to the previous slot.
//!
//! ## Revoke / kill-switch
//!
//! If the backend sets `firmware_update.revoke = true` we treat it as a
//! server-driven rollback: mark the *current* slot as
//! [`OtaImageState::Invalid`] and reset. The bootloader picks the other
//! slot on the next boot. This lets ops yank a bad release without
//! shipping a "good" replacement first.
//!
//! ## Flash ownership
//!
//! `FwNvs` consumes the `FLASH` peripheral at boot, but the OTA path
//! needs flash access too. We re-acquire it via `FLASH::steal()` inside
//! `apply` — safe because:
//!
//! - One wake cycle at a time: the runtime is single-task on core 0,
//!   and `apply` is awaited inline; nothing else touches flash during
//!   this window.
//! - On success `apply` does not return — `software_reset()` reboots
//!   the chip — so the stolen handle never outlives the wake cycle.

use alloc::string::String;

use embedded_storage::Storage;
use esp_bootloader_esp_idf::{
    ota::OtaImageState, ota_updater::OtaUpdater, partitions::PARTITION_TABLE_MAX_LEN,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use log::{info, warn};
use paperanywhere_ports::{FirmwareUpdate, FirmwareUpdater, HttpTransport};
use sha2::{Digest, Sha256};

/// Smallest unit esp-storage will erase. We buffer the streaming
/// download into sector-sized chunks before calling `write` so each
/// physical sector gets erased exactly once — writing smaller pieces
/// would trigger a read-modify-erase-write loop per chunk and turn a
/// minute-long OTA into an hour-long one.
const SECTOR_SIZE: usize = 4096;

#[derive(Debug)]
pub enum OtaError {
    /// `HttpTransport::stream_blob` returned an error — connection drop,
    /// non-2xx status, TLS hiccup. Safe to retry on a subsequent wake.
    Http,
    /// Couldn't read the partition table, or fewer than two OTA app
    /// partitions exist. Firmware was flashed with the wrong layout —
    /// retrying won't help.
    PartitionTable,
    /// `embedded_storage::Storage::write` failed mid-stream. Slot is
    /// half-written, but the bootloader will never see it (we abort
    /// before updating otadata).
    FlashWrite,
    /// SHA-256 of the bytes the server claimed to send didn't match
    /// what we received. Slot left dirty, otadata untouched.
    HashMismatch,
    /// Byte count didn't match `firmware_update.byte_len`. Same
    /// resolution as `HashMismatch`.
    SizeMismatch,
    /// Otadata update or activate_next_partition failed after a
    /// successful download. Slot is written but unreachable. The other
    /// slot (the one we're running) is still valid — next wake retries.
    Bootloader,
}

pub struct FwOta;

impl FwOta {
    pub const fn new() -> Self {
        Self
    }
}

impl FirmwareUpdater for FwOta {
    type Error = OtaError;

    async fn apply<H: HttpTransport>(
        &mut self,
        http: &mut H,
        token: &str,
        update: &FirmwareUpdate,
    ) -> Result<(), Self::Error> {
        let flash = unsafe { FLASH::steal() };
        let mut storage = FlashStorage::new(flash).multicore_auto_park();
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];

        if update.revoke {
            return revoke_current_slot(&mut storage, &mut pt_buf).await;
        }

        install_new_slot(&mut storage, &mut pt_buf, http, token, update).await
    }
}

async fn install_new_slot<H: HttpTransport>(
    storage: &mut FlashStorage<'static>,
    pt_buf: &mut [u8; PARTITION_TABLE_MAX_LEN],
    http: &mut H,
    token: &str,
    update: &FirmwareUpdate,
) -> Result<(), OtaError> {
    let mut updater = OtaUpdater::new(storage, pt_buf).map_err(|e| {
        warn!("ota: OtaUpdater::new failed: {:?}", e);
        OtaError::PartitionTable
    })?;

    let (mut region, next_subtype) = updater.next_partition().map_err(|e| {
        warn!("ota: next_partition failed: {:?}", e);
        OtaError::PartitionTable
    })?;
    info!(
        "ota: streaming {} bytes of {} into slot {:?}",
        update.byte_len, update.version, next_subtype
    );

    let mut sector_buf = [0u8; SECTOR_SIZE];
    let mut sector_filled: usize = 0;
    let mut flash_offset: u32 = 0;
    let mut hasher = Sha256::new();
    let mut write_failed = false;

    {
        let region = &mut region;
        let sector_buf = &mut sector_buf;
        let sector_filled = &mut sector_filled;
        let flash_offset = &mut flash_offset;
        let hasher = &mut hasher;
        let write_failed = &mut write_failed;

        let stream_result = http
            .stream_blob(token, &update.blob_url, &mut |mut chunk| {
                hasher.update(chunk);
                while !chunk.is_empty() {
                    let space = SECTOR_SIZE - *sector_filled;
                    let take = chunk.len().min(space);
                    sector_buf[*sector_filled..*sector_filled + take]
                        .copy_from_slice(&chunk[..take]);
                    *sector_filled += take;
                    chunk = &chunk[take..];
                    if *sector_filled == SECTOR_SIZE {
                        if region.write(*flash_offset, sector_buf).is_err() {
                            *write_failed = true;
                            return Err(());
                        }
                        *flash_offset += SECTOR_SIZE as u32;
                        *sector_filled = 0;
                    }
                }
                Ok(())
            })
            .await;

        if let Err(e) = stream_result {
            warn!("ota: stream_blob failed: {:?}", e);
            return Err(OtaError::Http);
        }
    }

    if write_failed {
        return Err(OtaError::FlashWrite);
    }

    // Flush the trailing partial sector.
    if sector_filled > 0 {
        if region
            .write(flash_offset, &sector_buf[..sector_filled])
            .is_err()
        {
            return Err(OtaError::FlashWrite);
        }
        flash_offset += sector_filled as u32;
    }

    let total_written = flash_offset as u64;
    if total_written != update.byte_len {
        warn!(
            "ota: size mismatch — wrote {} bytes, expected {}",
            total_written, update.byte_len
        );
        return Err(OtaError::SizeMismatch);
    }

    let digest = hasher.finalize();
    let got = hex_lower(&digest);
    if got != update.sha256_hex {
        warn!(
            "ota: sha256 mismatch\n  got      {}\n  expected {}",
            got, update.sha256_hex
        );
        return Err(OtaError::HashMismatch);
    }

    // Release the partition borrow before calling activate_next_partition
    // (which needs the updater mutably again).
    drop(region);

    updater.activate_next_partition().map_err(|e| {
        warn!("ota: activate_next_partition failed: {:?}", e);
        OtaError::Bootloader
    })?;
    updater
        .set_current_ota_state(OtaImageState::New)
        .map_err(|e| {
            warn!("ota: set_current_ota_state(New) failed: {:?}", e);
            OtaError::Bootloader
        })?;

    info!(
        "ota: install complete ({} bytes, sha256 ok). resetting into new slot",
        total_written
    );
    // Give the UART time to flush before the reset wipes our logs.
    embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    esp_hal::system::software_reset()
}

async fn revoke_current_slot(
    storage: &mut FlashStorage<'static>,
    pt_buf: &mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Result<(), OtaError> {
    warn!("ota: backend requested revoke — marking current slot invalid + rebooting");
    let mut updater = OtaUpdater::new(storage, pt_buf).map_err(|_| OtaError::PartitionTable)?;
    updater
        .set_current_ota_state(OtaImageState::Invalid)
        .map_err(|_| OtaError::Bootloader)?;
    embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    esp_hal::system::software_reset()
}

/// Boot-time housekeeping: if the previous boot left the slot marked
/// `New` or `PendingVerify`, promote it to `Valid` once the runtime has
/// completed its first successful wake. This is the "we got far enough
/// to phone home" handshake the bootloader watches for — without this
/// call the auto-rollback logic eventually reverts to the prior slot.
///
/// Cheap to call repeatedly: a no-op when state is already `Valid`.
pub fn mark_current_app_valid() {
    let flash = unsafe { FLASH::steal() };
    let mut storage = FlashStorage::new(flash).multicore_auto_park();
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];

    let mut updater = match OtaUpdater::new(&mut storage, &mut pt_buf) {
        Ok(u) => u,
        Err(_) => {
            // Single-slot / factory build — nothing to mark.
            return;
        }
    };

    let state = match updater.current_ota_state() {
        Ok(s) => s,
        Err(_) => return,
    };

    if matches!(state, OtaImageState::Valid) {
        return;
    }

    if let Err(e) = updater.set_current_ota_state(OtaImageState::Valid) {
        warn!("ota: mark_valid failed: {:?}", e);
    } else {
        info!("ota: current slot marked Valid (was {:?})", state);
    }
}

/// Lowercase hex encoding of a byte slice. SHA-256 is 32 bytes, so the
/// output is always 64 ASCII chars and the heap allocation is small.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let hi = b >> 4;
        let lo = b & 0x0F;
        s.push(nibble(hi));
        s.push(nibble(lo));
    }
    s
}

fn nibble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + n - 10) as char
    }
}

//! Boot orchestration. Implements the polling main loop:
//!
//! ```text
//! cold boot:
//!   resolve provisioning (prov partition → SD → existing NVS → captive portal)
//!   if no device_token yet: claim flow (render code, wait for user)
//!   enter main loop
//!
//! main loop (every wake):
//!   wifi associate
//!   GET /api/device/state
//!   if image is new:
//!     GET /api/device/blob/:id (streamed to panel)
//!     panel refresh
//!     POST /api/device/ack (Applied, battery, RSSI)
//!     persist last_applied_image_id to NVS
//!   wifi disconnect
//!   deep_sleep(next_check_at - now)
//! ```

use esp_println::println;

use crate::boards::{BoardConfig, PowerPolicy};
use crate::provisioning::SetupPath;

pub fn run(board: BoardConfig) -> ! {
    if factory_reset_held(board) {
        println!("boot: factory reset triggered");
        crate::nvs::factory_reset();
    }

    let path = crate::provisioning::resolve(board);
    println!("boot: setup path = {:?}", path);
    if matches!(path, SetupPath::NotProvisioned) {
        crate::nvs::claim_flow_stub(board);
        halt();
    }

    if let Some(code) = crate::nvs::load_pending_claim_code() {
        println!("boot: attempting auto-claim");
        if try_auto_claim(&code).is_ok() {
            crate::nvs::clear_pending_claim_code();
        }
    }

    main_loop(board)
}

fn main_loop(board: BoardConfig) -> ! {
    loop {
        let sleep_seconds = match single_wake_cycle(board) {
            Ok(secs) => secs,
            Err(e) => {
                println!("wake: cycle failed: {:?}", e);
                board.default_sleep_interval_sec
            }
        };

        match board.default_power_policy {
            PowerPolicy::ScheduledWake => crate::power::deep_sleep_for(sleep_seconds),
            PowerPolicy::AlwaysOn => crate::power::modem_sleep_ms(sleep_seconds.saturating_mul(1000)),
        }
    }
}

fn single_wake_cycle(_board: BoardConfig) -> Result<u32, WakeError> {
    let creds = crate::wifi::load_creds().ok_or(WakeError::NoWifi)?;
    let mut wifi = crate::wifi::Driver::new().map_err(|_| WakeError::WifiInit)?;
    wifi.associate(&creds).map_err(|_| WakeError::WifiAssociate)?;

    let token = crate::nvs::load_device_token().ok_or(WakeError::NoToken)?;
    let state = crate::http::get_state(token.as_str()).map_err(|_| WakeError::StateFetch)?;

    if let Some(image) = &state.image {
        let last_applied = crate::nvs::load_last_applied_image_id();
        if last_applied.as_deref() != Some(image.image_id.as_str()) {
            println!("wake: new image {}, downloading", image.image_id);
            let render_result = crate::http::stream_blob(token.as_str(), &image.blob_url, |_chunk| {
                // M4: pipe chunk into panel SPI via the board's panel driver.
                Ok(())
            });
            let phase = if render_result.is_ok() {
                crate::nvs::save_last_applied_image_id(&image.image_id);
                crate::wire::AckPhase::Applied
            } else {
                crate::wire::AckPhase::Failed
            };
            let ack = crate::wire::DeviceAck {
                image_id: image.image_id.clone(),
                phase,
                error: render_result.err().map(|e| alloc::format!("{:?}", e)),
                battery_mv: crate::power::battery_mv(),
                rssi_dbm: wifi.rssi_dbm().map(|v| v as i16),
            };
            let _ = crate::http::post_ack(token.as_str(), &ack);
        } else {
            println!("wake: image already applied, skipping");
        }
    }

    let _ = wifi.disconnect();
    let now = unix_now();
    let sleep_for = state.next_check_at.saturating_sub(now).min(u32::MAX as u64) as u32;
    Ok(sleep_for.max(60))
}

fn factory_reset_held(board: BoardConfig) -> bool {
    if !board.has_buttons { return false; }
    false
}

fn try_auto_claim(_code: &str) -> Result<(), ()> {
    Err(())
}

fn unix_now() -> u64 { 0 }

fn halt() -> ! {
    loop { core::hint::spin_loop(); }
}

#[derive(Debug)]
enum WakeError {
    NoWifi,
    WifiInit,
    WifiAssociate,
    NoToken,
    StateFetch,
}

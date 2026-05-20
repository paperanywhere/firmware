//! HTTPS client surface — three calls cover the entire polling protocol:
//!
//!   - [`get_state`]        — fetch the next-thing-to-do (config + maybe image)
//!   - [`stream_blob`]      — chunked download of the processed panel bytes
//!   - [`post_ack`]         — confirm image received / applied / failed
//!
//! All requests authenticate via `Authorization: Bearer <device_token>` from
//! NVS. TLS verification happens against a CA bundle baked into the firmware
//! (M4 will embed a small root cert chain pinned to the dashboard's TLS cert).
//!
//! Implementation is currently typed stubs while the esp-wifi crate matrix
//! aligns with esp-hal 1.1. Each function returns `Err(_)` so the boot
//! loop's fallback paths exercise correctly.

use crate::wire::{DeviceAck, DeviceState};

#[derive(Debug)]
pub enum HttpError {
    NotImplemented,
    WifiDown,
    TlsHandshake,
    BadStatus(u16),
    Decode,
    Network,
}

/// GET `/api/device/state`. Authenticates with the device token from NVS.
pub fn get_state(_token: &str) -> Result<DeviceState, HttpError> {
    esp_println::println!("http: get_state stub");
    Err(HttpError::NotImplemented)
}

/// GET `/api/device/blob/:image_id` and stream chunks into `on_chunk`. Caller
/// pipes each chunk into the panel SPI driver so the full image never sits in
/// RAM at once.
pub fn stream_blob<F>(_token: &str, _path_or_url: &str, mut _on_chunk: F) -> Result<(), HttpError>
where
    F: FnMut(&[u8]) -> Result<(), ()>,
{
    Err(HttpError::NotImplemented)
}

/// POST `/api/device/ack`. JSON body produced by `DeviceAck::to_json`.
pub fn post_ack(_token: &str, _ack: &DeviceAck) -> Result<(), HttpError> {
    esp_println::println!("http: post_ack stub");
    Err(HttpError::NotImplemented)
}

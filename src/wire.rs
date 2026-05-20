//! Wire types for the polling protocol. Replaces the WebSocket message types
//! from the old design — every interaction is now a single HTTP request.
//!
//! Hand-rolled types (no serde dep) so we avoid the cargo feature-unification
//! trap with esp-hal-procmacros' transitive serde_yaml. The protocol is JSON
//! over HTTPS; serialization lives next to each type as a tiny `to_json` /
//! `from_json` helper, which is fine for the handful of fields we need.

use alloc::string::String;
use alloc::vec::Vec;

/// Response from `GET /api/device/state`. Tells the device what to do next.
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// Image to render. `None` means "nothing new, just check again later".
    pub image: Option<ImageRef>,
    pub config: DeviceConfig,
    /// Unix seconds when the device should wake to poll again.
    pub next_check_at: u64,
}

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub image_id: String,
    pub blob_url: String,
    pub sha256_hex: String,
    pub byte_len: u64,
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
    /// Render to JSON without pulling serde. The keys + format match what the
    /// backend route expects in `crates/backend/src/routes/device_api.rs`.
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
            out.push_str(&alloc::format!("\"battery_mv\":{mv}"));
        }
        if let Some(rssi) = self.rssi_dbm {
            out.push(',');
            out.push_str(&alloc::format!("\"rssi_dbm\":{rssi}"));
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
            c if (c as u32) < 0x20 => out.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Best-effort JSON parser for the small `/state` response. Only handles the
/// shapes the backend produces — not a full JSON parser, no nested objects
/// beyond what we need, no arrays beyond `parts`. Returns `None` on any
/// structural surprise; caller treats `None` as "skip this wake, retry next".
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

    Some(DeviceState {
        image,
        config: DeviceConfig { sleep_interval_sec, power_policy },
        next_check_at,
    })
}

/// Find `"<key>":"<value>"` in `body` and return the unescaped value. Returns
/// `None` if the key isn't present or the value isn't a quoted string.
fn extract_str(body: &str, key: &str) -> Option<String> {
    let needle = alloc::format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let mut end = 0;
    let mut chars = rest.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch == '\\' {
            // Skip the escaped char without checking it.
            let _ = chars.next();
            continue;
        }
        if ch == '"' {
            end = i;
            break;
        }
    }
    if end == 0 { return None; }
    let raw = &rest[..end];
    // Quick-unescape the common cases. We don't roundtrip arbitrary JSON.
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
    let needle = alloc::format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = body[start..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[allow(dead_code)]
fn _force_use(_v: Vec<u8>) {}

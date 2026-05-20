//! SD-card based provisioning. Reads `wifi.conf` (a small TOML file at the SD
//! root) into a `ProvData` for the resolver. Board-gated to those with SD slots.

use alloc::string::String;

use crate::provisioning::ProvData;

/// Read + parse the wifi.conf file from the SD root. M4 wires the mount via
/// `embedded-sdmmc`; today returns `None` so the resolver falls through.
pub fn read_wifi_conf() -> Option<ProvData> {
    None
}

/// Parse a small TOML body. Recognized keys: `ssid`, `password`, `backend_url`,
/// `claim_code`. Strings may be double-quoted with `\"`, `\\`, `\n`, `\r`
/// escapes. Real code path once `read_wifi_conf` lands its impl.
#[allow(dead_code)]
pub fn parse_wifi_conf(input: &str) -> Option<ProvData> {
    let mut ssid = None;
    let mut password = None;
    let mut backend_url = None;
    let mut claim_code = None;
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let (key, value) = line.split_once('=')?;
        let value = unquote(value.trim())?;
        match key.trim() {
            "ssid" => ssid = Some(value),
            "password" => password = Some(value),
            "backend_url" => backend_url = Some(value),
            "claim_code" => claim_code = Some(value),
            _ => {}
        }
    }
    Some(ProvData {
        ssid: ssid?,
        password: password?,
        backend_url,
        claim_code,
    })
}

fn unquote(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

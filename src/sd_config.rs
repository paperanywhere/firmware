//! SD-card based provisioning. Parses a small `wifi.conf` TOML at the SD root
//! into a `ProvBlob` so the rest of the boot path treats it identically to a
//! flash-time bundle.
//!
//! M4 stub — real impl will mount the FAT filesystem via `embedded-sdmmc` and
//! parse the four known keys manually. We avoid a full TOML parser since this
//! is the only TOML the firmware ever reads.

use alloc::string::String;
use paperanywhere_proto::ProvBlob;

/// Parse a small TOML body. Recognized keys: `ssid`, `password`, `backend_url`, `claim_code`.
/// Strings may be double-quoted with `\"`, `\\`, `\n`, `\r` escapes.
pub fn parse_wifi_conf(input: &str) -> Option<ProvBlob> {
    let mut ssid = None;
    let mut password = None;
    let mut backend_url = None;
    let mut claim_code = None;
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let value = unquote(value.trim())?;
        match key {
            "ssid" => ssid = Some(value),
            "password" => password = Some(value),
            "backend_url" => backend_url = Some(value),
            "claim_code" => claim_code = Some(value),
            _ => {}
        }
    }
    Some(ProvBlob {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn parses_full_conf() {
        let body = r#"
            # comment
            ssid = "guest-iot"
            password = "let me in"
            backend_url = "https://api.paperanywhere.io"
            claim_code = "ABCD3F"
        "#;
        let p = parse_wifi_conf(body).unwrap();
        assert_eq!(p.ssid, "guest-iot");
        assert_eq!(p.password, "let me in");
        assert_eq!(p.backend_url.as_deref(), Some("https://api.paperanywhere.io"));
        assert_eq!(p.claim_code.as_deref(), Some("ABCD3F"));
    }

    #[test]
    fn handles_escape_sequences() {
        let body = r#"
            ssid = "with\"quote"
            password = "with\\backslash"
        "#;
        let p = parse_wifi_conf(body).unwrap();
        assert_eq!(p.ssid, "with\"quote");
        assert_eq!(p.password, "with\\backslash");
    }

    #[test]
    fn missing_required_field_returns_none() {
        let body = r#"ssid = "only-ssid""#;
        assert!(parse_wifi_conf(body).is_none());
    }
}

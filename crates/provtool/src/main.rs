//! paperanywhere-provtool — emits a `prov.bin` blob for the firmware's
//! `prov` partition (16 KB at flash offset 0x12000 on the reTerminal E-series
//! partition table).
//!
//! ## Usage
//!
//! ```text
//! # Either set env vars directly:
//! PAPERANYWHERE_PROV_SSID=mynet \
//! PAPERANYWHERE_PROV_PASS=secret \
//! PAPERANYWHERE_PROV_BACKEND_URL=http://10.0.1.109:8080 \
//!   cargo run -p paperanywhere-provtool
//!
//! # Or keep them in a gitignored .env file at the workspace root and just:
//! cargo run -p paperanywhere-provtool
//!
//! # The emitted file goes to ./prov.bin by default; --output to override.
//! ```
//!
//! ## Flash workflow
//!
//! After building the firmware and flashing the app slot, the prov partition
//! is written separately so the same firmware binary is genuinely backend-
//! agnostic:
//!
//! ```text
//! espflash flash --port COM6 \
//!   --partition-table crates/firmware/flash/partition-table.csv \
//!   target/xtensa-esp32s3-none-elf/release/paperanywhere-firmware
//!
//! espflash write-bin --port COM6 0x12000 prov.bin
//! ```
//!
//! On first boot the firmware migrates the prov blob into NVS, erases the
//! prov partition, and starts polling the backend.
//!
//! ## Blob format
//!
//! Matches the NVS record TLV format the firmware already speaks, just with
//! a different magic so a prov-flashed-into-nvs miswiring is detectable:
//!
//! ```text
//! [0..4]    magic = "PA4P"
//! [4..8]    version u32 LE = 1
//! [8..12]   CRC32 of bytes [12..PROV_PAYLOAD_LEN+12] (poly 0xEDB88320, LE)
//! [12..]    TLV records: [tag u8][len u16 BE][value]
//!           tag 2 = wifi_ssid
//!           tag 3 = wifi_password
//!           tag 4 = backend_url
//!           tag 5 = claim_code_pending (optional)
//!           tag 0 = end terminator
//! [rest]    zero-padded to PROV_BLOB_SIZE = 16 KB
//! ```

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const PROV_BLOB_SIZE: usize = 16 * 1024;
const PROV_HEADER_LEN: usize = 12;
const PROV_MAGIC: [u8; 4] = *b"PA4P";
const PROV_VERSION: u32 = 1;

const TAG_WIFI_SSID: u8 = 2;
const TAG_WIFI_PASSWORD: u8 = 3;
const TAG_BACKEND_URL: u8 = 4;
const TAG_CLAIM_CODE: u8 = 5;
/// Dev-build marker. 1-byte payload, value 0x01 = dev. Firmware reads
/// this and suppresses the GitHub-release OTA check so a hand-built
/// dev binary doesn't get clobbered by the next release. Omitted from
/// production prov bundles.
const TAG_IS_DEV_BUILD: u8 = 7;

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("provtool: {}", e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
    // Best-effort load of a .env at cwd. Doesn't override env vars already set
    // in the shell — explicit > file > nothing.
    let _ = load_dotenv(".env");
    // Also try a gitignored provision.env, since users probably want WiFi
    // secrets in a distinctly-named file separate from generic .env stuff.
    let _ = load_dotenv("provision.env");

    let mut output: PathBuf = "prov.bin".into();
    let mut is_dev = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                output = args
                    .next()
                    .ok_or_else(|| "--output requires a path".to_string())?
                    .into();
            }
            "--dev" => {
                is_dev = true;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown arg: {}", other)),
        }
    }
    // Env-var fallback so CI / shell scripts can set the flag without
    // remembering to pass --dev. Truthy = "1" / "true" / "yes" (case-
    // insensitive). Anything else (or unset) leaves the default.
    if !is_dev {
        if let Ok(v) = std::env::var("PAPERANYWHERE_PROV_DEV") {
            let v = v.trim().to_ascii_lowercase();
            is_dev = matches!(v.as_str(), "1" | "true" | "yes");
        }
    }

    let ssid = require_env("PAPERANYWHERE_PROV_SSID")?;
    let password = require_env("PAPERANYWHERE_PROV_PASS")?;
    let backend_url = require_env("PAPERANYWHERE_PROV_BACKEND_URL")?;
    let claim_code = std::env::var("PAPERANYWHERE_PROV_CLAIM_CODE").ok();

    let blob = build_blob(&ssid, &password, &backend_url, claim_code.as_deref(), is_dev)?;
    let mut padded = vec![0u8; PROV_BLOB_SIZE];
    padded[..blob.len()].copy_from_slice(&blob);
    fs::write(&output, &padded)
        .map_err(|e| format!("write {}: {}", output.display(), e))?;
    println!(
        "wrote {} ({} bytes of {} blob used, rest zero-padded){}",
        output.display(),
        blob.len(),
        PROV_BLOB_SIZE,
        if is_dev { " [DEV — OTA disabled]" } else { "" }
    );
    println!();
    println!("Flash to the device with:");
    let port = std::env::var("PAPERANYWHERE_PROV_PORT").unwrap_or_else(|_| "COM6".into());
    println!(
        "  espflash write-bin --port {} 0x12000 {}",
        port,
        output.display()
    );
    Ok(())
}

fn build_blob(
    ssid: &str,
    password: &str,
    backend_url: &str,
    claim_code: Option<&str>,
    is_dev: bool,
) -> Result<Vec<u8>, String> {
    if ssid.is_empty() {
        return Err("ssid is empty".into());
    }
    if ssid.len() > 32 {
        return Err(format!("ssid > 32 bytes ({} bytes)", ssid.len()));
    }
    if password.len() > 64 {
        return Err(format!("password > 64 bytes ({} bytes)", password.len()));
    }
    if backend_url.len() > 128 {
        return Err(format!("backend_url > 128 bytes ({} bytes)", backend_url.len()));
    }

    let mut payload: Vec<u8> = Vec::new();
    push_record(&mut payload, TAG_WIFI_SSID, ssid.as_bytes());
    push_record(&mut payload, TAG_WIFI_PASSWORD, password.as_bytes());
    push_record(&mut payload, TAG_BACKEND_URL, backend_url.as_bytes());
    if let Some(cc) = claim_code {
        if cc.len() > 16 {
            return Err(format!("claim_code > 16 bytes ({} bytes)", cc.len()));
        }
        push_record(&mut payload, TAG_CLAIM_CODE, cc.as_bytes());
    }
    if is_dev {
        // Single-byte payload, 0x01 = dev. Firmware decodes this in
        // nvs.rs's parse_prov_tlv and sets cache.is_dev_build = true.
        // Omitted entirely for production bundles so the wire layout
        // matches the pre-flag version byte-for-byte.
        push_record(&mut payload, TAG_IS_DEV_BUILD, &[1u8]);
    }
    payload.push(0); // end terminator

    let mut blob = Vec::with_capacity(PROV_HEADER_LEN + payload.len());
    blob.extend_from_slice(&PROV_MAGIC);
    blob.extend_from_slice(&PROV_VERSION.to_le_bytes());
    let crc_placeholder = [0u8; 4];
    blob.extend_from_slice(&crc_placeholder);
    blob.extend_from_slice(&payload);

    // CRC32 over [12..] — same poly the firmware uses for NVS records.
    let crc = crc32(&blob[PROV_HEADER_LEN..]);
    blob[8..12].copy_from_slice(&crc.to_le_bytes());

    if blob.len() > PROV_BLOB_SIZE {
        return Err(format!(
            "encoded blob {} bytes exceeds prov partition size {}",
            blob.len(),
            PROV_BLOB_SIZE
        ));
    }
    Ok(blob)
}

fn push_record(buf: &mut Vec<u8>, tag: u8, value: &[u8]) {
    buf.push(tag);
    let len = value.len() as u16;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(value);
}

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

fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| {
        format!(
            "{} must be set in env or .env (try `export {}=...` in your shell, \
             or add it to provision.env in the workspace root)",
            key, key
        )
    })
}

/// Tiny `.env` loader. Treats each non-blank, non-comment line as `KEY=VALUE`.
/// Strips surrounding double quotes. Doesn't expand `${...}` references — the
/// goal is to hold a few secrets, not be a shell.
fn load_dotenv(path: &str) -> Result<(), std::io::Error> {
    let body = fs::read_to_string(path)?;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let mut val = v.trim();
        if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
            val = &val[1..val.len() - 1];
        }
        // Only set if the env doesn't already define it — explicit shell
        // values win over file values.
        if std::env::var(key).is_err() {
            unsafe {
                std::env::set_var(key, val);
            }
        }
    }
    Ok(())
}

fn print_help() {
    let _ = std::io::stdout().write_all(
        b"paperanywhere-provtool: emit a prov.bin for the firmware's prov partition.

Reads (in priority order):
  1. shell env
  2. provision.env in cwd
  3. .env in cwd

Required:
  PAPERANYWHERE_PROV_SSID         <wifi ssid, <=32 bytes>
  PAPERANYWHERE_PROV_PASS         <wifi password, <=64 bytes>
  PAPERANYWHERE_PROV_BACKEND_URL  http://host[:port] or https://host[:port]

Optional:
  PAPERANYWHERE_PROV_CLAIM_CODE   pre-issued claim code, <=16 bytes
  PAPERANYWHERE_PROV_PORT         serial port for the flash hint (default COM6)
  PAPERANYWHERE_PROV_DEV          set to 1/true/yes for a dev build (same as
                                  --dev). Suppresses GitHub-release OTA on the
                                  resulting device so hand-built dev firmware
                                  doesn't get clobbered.

Args:
  --output, -o <path>   override the output path (default ./prov.bin)
  --dev                 mark this prov bundle as a DEV build -- device skips
                        the GitHub-release OTA check after flashing.
  --help, -h            this message
",
    );
}

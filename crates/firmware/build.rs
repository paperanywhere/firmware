//! Build script — rasterizes `assets/logo.svg` into panel-native bytes for
//! the active board feature and emits them as a `BOOT_SCREEN: &[u8]` const
//! that `boot.rs` includes via `include_bytes!`.
//!
//! Runs once per build; cached if neither the SVG nor this script changes.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use paperanywhere_boot_screen::{BootScreenSpec, render};
use paperanywhere_ports::ColorMode;

/// Status-bar height the compositor reserves at the top of the panel.
/// Must match `paperanywhere_compositor::DEFAULT_STATUS_BAR_HEIGHT` —
/// duplicated here as a `const` so build.rs doesn't have to depend on
/// the compositor crate (which has its own `embedded-graphics` build
/// deps we'd rather not pull into the host build script).
const STATUS_BAR_HEIGHT_PX: u32 = 32;

fn main() {
    emit_version_stamp();
    emit_build_time();
    let (width, full_height, color_mode) = active_board_spec();
    // Boot + OTA screens render into the TOP portion of the main
    // region only — the bottom portion is reserved for the compositor's
    // 3-column build-info block (Firmware / Network / Device + a
    // countdown row). Without this height-cap the logo's bottom half
    // would overlap the text and the fast-LUT refresh would smear
    // the previous logo pixels into the value-column text.
    //
    // Sizing: ~55% of the main-region height leaves enough vertical
    // space at the bottom for the 6-row build-info block (5 data rows
    // + 1 countdown row at 12 px each + ~24 px of breathing room
    // above and below = ~96 px). For an 800x448 main region that's
    // 800×246 for the logo and 800×202 for the info block.
    let main_height = full_height.saturating_sub(STATUS_BAR_HEIGHT_PX);
    const BOOT_LOGO_HEIGHT_FRACTION: f32 = 0.55;
    let height = ((main_height as f32) * BOOT_LOGO_HEIGHT_FRACTION) as u32;
    println!(
        "cargo:warning=boot-screen: rasterising for {}x{} (main region of {}x{}, info block reserved at bottom) {:?}",
        width, height, width, main_height, color_mode
    );

    let assets = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets");
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let spec = BootScreenSpec { width, height, color_mode, padding_fraction: 0.10 };

    rasterise(&assets, "logo.svg", "boot_screen.bin", "PAPERANYWHERE_BOOT_SCREEN", &spec, &out_dir);
    rasterise(&assets, "logo_ota.svg", "ota_screen.bin", "PAPERANYWHERE_OTA_SCREEN", &spec, &out_dir);
}

/// Bake a UTC build-time stamp like `2026-05-20 22:13 UTC`. Surfaced on
/// the boot screen so a deployed device can be eyeballed against an
/// expected release. Pure stdlib; no chrono dep just for this.
fn emit_build_time() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format_utc(now);
    println!("cargo:rustc-env=PAPERANYWHERE_BUILD_TIME={stamp}");
}

/// Convert a Unix timestamp to `YYYY-MM-DD HH:MM UTC` without any deps.
/// Good through 2099-12-31. Doesn't handle leap-seconds (nobody does).
fn format_utc(unix_secs: u64) -> String {
    let secs = unix_secs % 60;
    let mins = (unix_secs / 60) % 60;
    let hours = (unix_secs / 3600) % 24;
    let mut days = (unix_secs / 86_400) as i64;

    // 1970-01-01 was a Thursday; epoch day = 0.
    let mut year: i64 = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let months_normal = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months_leap = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months = if is_leap(year) { &months_leap } else { &months_normal };

    let mut month = 0;
    while days >= months[month] {
        days -= months[month];
        month += 1;
    }
    let day = days + 1;

    let _ = secs; // not displayed; minute precision is enough
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year,
        month + 1,
        day,
        hours,
        mins
    )
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Read an SVG from `<assets>/<svg_name>`, run it through the boot-screen
/// rasteriser, and emit the resulting panel-native bytes to
/// `<OUT_DIR>/<out_name>`. The path is exposed through `cargo:rustc-env`
/// under `env_var` so the firmware can pull it in via `env!`.
fn rasterise(
    assets: &Path,
    svg_name: &str,
    out_name: &str,
    env_var: &str,
    spec: &BootScreenSpec,
    out_dir: &std::ffi::OsStr,
) {
    let svg_path = assets.join(svg_name);
    println!("cargo:rerun-if-changed={}", svg_path.display());
    let svg = fs::read_to_string(&svg_path)
        .unwrap_or_else(|e| panic!("read {}: {}", svg_path.display(), e));
    let bytes = render(&svg, spec)
        .unwrap_or_else(|e| panic!("render {}: {:?}", svg_name, e));
    let out_path = Path::new(out_dir).join(out_name);
    fs::write(&out_path, &bytes)
        .unwrap_or_else(|e| panic!("write {}: {}", out_path.display(), e));
    println!("cargo:rustc-env={}={}", env_var, out_path.display());
}

/// Embed a version stamp the firmware reports to the backend (in the
/// /claim request and per-wake heartbeats). Backend uses it to decide
/// whether to attach a `firmware_update` payload to the next /state
/// response.
///
/// Format: `<cargo-version>+<git-short-sha>` (e.g. `0.1.0+a1b2c3d4`). When
/// built outside a git checkout (release tarball, sandboxed CI) the SHA is
/// replaced with `unknown`. The stamp is exposed via `env!` so the firmware
/// can `pub const FW_VERSION: &str = env!("PAPERANYWHERE_FW_VERSION");`.
fn emit_version_stamp() {
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PAPERANYWHERE_FW_VERSION={}+{}", cargo_version, git_sha);
    // Rerun if HEAD moves so the stamp tracks the active commit. `.git/HEAD`
    // changes on every checkout/rebase even when the working tree doesn't,
    // which is exactly what we want.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}

/// Resolve `(width, height, color_mode)` from the active board feature.
/// Keep this in sync with `src/boards/<name>.rs` — the boards module is
/// the source of truth at runtime; this duplicates only what the build
/// script needs at host time.
fn active_board_spec() -> (u32, u32, ColorMode) {
    if env::var("CARGO_FEATURE_BOARD_RETERMINAL_E1001").is_ok() {
        return (800, 480, ColorMode::Mono1bpp);
    }
    if env::var("CARGO_FEATURE_BOARD_RETERMINAL_E1002").is_ok() {
        return (800, 480, ColorMode::Color7);
    }
    if env::var("CARGO_FEATURE_BOARD_RETERMINAL_E1003").is_ok() {
        return (1404, 1872, ColorMode::Gray16);
    }
    if env::var("CARGO_FEATURE_BOARD_RETERMINAL_E1004").is_ok() {
        return (1200, 1600, ColorMode::Color7);
    }
    if env::var("CARGO_FEATURE_BOARD_INKPLATE_6").is_ok() {
        return (1024, 758, ColorMode::Gray4);
    }
    if env::var("CARGO_FEATURE_BOARD_INKPLATE_10").is_ok() {
        return (1200, 825, ColorMode::Gray4);
    }
    if env::var("CARGO_FEATURE_BOARD_GENERIC_ESP32S3_WAVESHARE_75").is_ok() {
        return (800, 480, ColorMode::Mono1bpp);
    }
    panic!(
        "no board-* feature enabled; build with --features board-reterminal-e1001 (or another)"
    );
}

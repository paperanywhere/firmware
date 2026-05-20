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

fn main() {
    emit_version_stamp();
    let (width, height, color_mode) = active_board_spec();
    println!(
        "cargo:warning=boot-screen: rasterising for {}x{} {:?}",
        width, height, color_mode
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

//! Rasterise the Font Awesome icon SVGs into 1bpp bitmaps at build
//! time. Output goes to `OUT_DIR`; `src/icons.rs` `include_bytes!`s
//! the resulting `.bin` blobs as `pub const` slices the status bar
//! blits into the framebuffer.

use std::env;
use std::fs;
use std::path::Path;

use paperanywhere_boot_screen::{BootScreenSpec, render};
use paperanywhere_ports::ColorMode;

/// Edge length (in pixels) the icons get rasterised at. The status
/// bar reserves 32 rows; 20 px leaves a 6 px top/bottom margin and is
/// large enough that the Font Awesome paths read clearly even after
/// 1bpp threshold dithering.
const ICON_PX: u32 = 20;

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("icons");
    let out_dir = env::var_os("OUT_DIR").unwrap();

    // padding_fraction=0 because Font Awesome SVGs already include
    // their own padding inside the 640×640 viewBox.
    let spec = BootScreenSpec {
        width: ICON_PX,
        height: ICON_PX,
        color_mode: ColorMode::Mono1bpp,
        padding_fraction: 0.0,
    };

    for (svg_name, out_name, env_var) in &[
        ("wifi.svg", "icon_wifi.bin", "PAPERANYWHERE_ICON_WIFI"),
        ("wifi-slash.svg", "icon_wifi_slash.bin", "PAPERANYWHERE_ICON_WIFI_SLASH"),
    ] {
        let svg_path = assets.join(svg_name);
        println!("cargo:rerun-if-changed={}", svg_path.display());
        let svg = fs::read_to_string(&svg_path)
            .unwrap_or_else(|e| panic!("read {}: {}", svg_path.display(), e));
        let bytes = render(&svg, &spec)
            .unwrap_or_else(|e| panic!("render {}: {:?}", svg_name, e));
        let out_path = Path::new(&out_dir).join(out_name);
        fs::write(&out_path, &bytes)
            .unwrap_or_else(|e| panic!("write {}: {}", out_path.display(), e));
        println!("cargo:rustc-env={}={}", env_var, out_path.display());
    }
}

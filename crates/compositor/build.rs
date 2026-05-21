//! Rasterise the Font Awesome icon SVGs into 1bpp bitmaps at build
//! time. Output goes to `OUT_DIR`; `src/icons.rs` `include_bytes!`s
//! the resulting `.bin` blobs as `pub static` slices the status bar
//! blits into the framebuffer.
//!
//! Unlike the boot-screen logo path, we do NOT dither here — at 20 px
//! the Font Awesome paths' antialiased edges turn into noise under
//! Floyd-Steinberg. A hard alpha threshold (any pixel with α ≥ 128
//! becomes ink) gives crisp solid icons. Bitmap convention matches
//! the boot-screen output (bit set = white = no ink) so the same
//! blit code paints both.

use std::env;
use std::fs;
use std::path::Path;

/// Edge length (in pixels) the icons get rasterised at. The status
/// bar reserves 32 rows; 20 px leaves a 6 px top/bottom margin and is
/// large enough that the Font Awesome paths read clearly even after
/// hard thresholding.
const ICON_PX: u32 = 20;

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("icons");
    let out_dir = env::var_os("OUT_DIR").unwrap();

    for (svg_name, out_name, env_var) in &[
        ("wifi.svg", "icon_wifi.bin", "PAPERANYWHERE_ICON_WIFI"),
        ("wifi-slash.svg", "icon_wifi_slash.bin", "PAPERANYWHERE_ICON_WIFI_SLASH"),
    ] {
        let svg_path = assets.join(svg_name);
        println!("cargo:rerun-if-changed={}", svg_path.display());
        let svg = fs::read_to_string(&svg_path)
            .unwrap_or_else(|e| panic!("read {}: {}", svg_path.display(), e));
        let bytes = rasterise_icon(&svg, ICON_PX);
        let out_path = Path::new(&out_dir).join(out_name);
        fs::write(&out_path, &bytes)
            .unwrap_or_else(|e| panic!("write {}: {}", out_path.display(), e));
        println!("cargo:rustc-env={}={}", env_var, out_path.display());
        println!(
            "cargo:warning=icon {} → {} bytes ({} ink pixels)",
            svg_name,
            bytes.len(),
            count_ink_pixels(&bytes, ICON_PX, ICON_PX)
        );
    }
}

fn rasterise_icon(svg: &str, edge_px: u32) -> Vec<u8> {
    // Parse the SVG. The FA glyphs use single-path black-fill artwork
    // on a transparent background, so we don't need any font support
    // here — just the path renderer.
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg, &opt).expect("parse FA icon SVG");
    let view_box = tree.size();

    // Scale the SVG's viewBox to fit edge_px × edge_px.
    let scale = (edge_px as f32) / view_box.width().max(view_box.height());
    let pixmap_w = edge_px;
    let pixmap_h = edge_px;
    let mut pixmap = tiny_skia::Pixmap::new(pixmap_w, pixmap_h)
        .expect("alloc tiny-skia pixmap");

    let xform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, xform, &mut pixmap.as_mut());

    // Pack to 1bpp, MSB-first, row-major. Convention: bit set = white
    // (no ink). Any pixel whose alpha clears the threshold counts as
    // ink; everything else is background.
    const ALPHA_THRESHOLD: u8 = 96;
    let stride = ((pixmap_w as usize) + 7) / 8;
    let mut out = vec![0xFFu8; stride * (pixmap_h as usize)];
    for y in 0..pixmap_h {
        for x in 0..pixmap_w {
            let i = (y * pixmap_w + x) as usize * 4;
            let alpha = pixmap.data()[i + 3];
            if alpha >= ALPHA_THRESHOLD {
                let byte_idx = (y as usize) * stride + (x as usize) / 8;
                let bit = 1u8 << (7 - (x % 8));
                out[byte_idx] &= !bit; // clear bit ⇒ ink
            }
        }
    }
    out
}

fn count_ink_pixels(bytes: &[u8], w: u32, h: u32) -> u32 {
    let stride = ((w as usize) + 7) / 8;
    let mut ink = 0;
    for y in 0..h {
        for x in 0..w {
            let byte_idx = (y as usize) * stride + (x as usize) / 8;
            let bit = 1u8 << (7 - (x % 8));
            if byte_idx < bytes.len() && bytes[byte_idx] & bit == 0 {
                ink += 1;
            }
        }
    }
    ink
}

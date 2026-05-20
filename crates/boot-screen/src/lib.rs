//! Rasterize the paperanywhere logo SVG into panel-native bytes.
//!
//! Pipeline:
//!
//! 1. Parse SVG with `usvg` (no font loading — text glyphs that can't be
//!    found render as their bounding box, which is intentional for now;
//!    embedding Inter is a follow-up if the result reads poorly).
//! 2. Compute a fit transform that scales the SVG to `width − 2·padding`
//!    by `height − 2·padding` preserving aspect ratio, then centers it.
//! 3. Rasterize to RGBA over a white background sized to the panel.
//! 4. Dither to the target color mode (Floyd-Steinberg for now — fine for
//!    Mono1bpp / Gray4 / Gray16; Color7 falls back to grayscale until the
//!    ACeP palette path lands).
//! 5. Pack into panel-native bytes (row-major MSB-first 1bpp etc.).
//!
//! Same algorithm is used at firmware build-time and in the sim at
//! runtime so the bytes match.

use paperanywhere_ports::ColorMode;
use tiny_skia::{Color, Pixmap, Rect, Transform};

/// What the caller needs to specify per consumer.
pub struct BootScreenSpec {
    pub width: u32,
    pub height: u32,
    pub color_mode: ColorMode,
    /// Fraction of `min(width, height)` reserved as margin on each side.
    /// 0.10 = 10 % padding.
    pub padding_fraction: f32,
}

/// Render the SVG. Returns panel-native packed bytes ready to feed into
/// `EpaperPanel::write_chunk` followed by `refresh`.
pub fn render(svg: &str, spec: &BootScreenSpec) -> Result<Vec<u8>, RenderError> {
    let mut opt = usvg::Options::default();
    // Load whatever fonts the host has so the SVG's
    // "Inter, system-ui, sans-serif" family list resolves. `load_system_fonts`
    // scans the OS font dirs; `set_*_family` calls map the SVG's *generic*
    // families ("system-ui", "sans-serif") to real installed faces — without
    // those mappings, fontdb gives up on generic names and the text vanishes.
    opt.fontdb_mut().load_system_fonts();
    let fallback_sans = pick_first_installed_family(
        &opt.fontdb,
        &["Inter", "Segoe UI", "DejaVu Sans", "Liberation Sans", "Arial", "Helvetica"],
    );
    if let Some(name) = fallback_sans {
        opt.fontdb_mut().set_sans_serif_family(name.clone());
        opt.fontdb_mut().set_serif_family(name.clone());
        opt.fontdb_mut().set_monospace_family(name);
    }

    let tree = usvg::Tree::from_str(svg, &opt)
        .map_err(|e| RenderError::SvgParse(e.to_string()))?;
    let svg_size = tree.size();

    // ── Fit transform: scale to (width − 2·pad) × (height − 2·pad)
    // preserving aspect ratio, then translate to centre on a panel-sized
    // canvas.
    let pad = (spec.width.min(spec.height) as f32 * spec.padding_fraction)
        .round()
        .max(8.0);
    let avail_w = (spec.width as f32) - 2.0 * pad;
    let avail_h = (spec.height as f32) - 2.0 * pad;
    let scale = (avail_w / svg_size.width()).min(avail_h / svg_size.height());
    let drawn_w = svg_size.width() * scale;
    let drawn_h = svg_size.height() * scale;
    let offset_x = ((spec.width as f32) - drawn_w) / 2.0;
    let offset_y = ((spec.height as f32) - drawn_h) / 2.0;
    // Construct the affine directly so the order is unambiguous: scaling on
    // the diagonal, translation in the last column. Equivalent to "scale then
    // translate" when applied to a point: result = (scale·x + offset_x, …).
    // tiny-skia's chained `.pre_scale` / `.post_scale` builders order
    // matrices in a way that's easy to get backwards — going matrix-direct
    // avoids that class of bug.
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, offset_x, offset_y);

    let mut pixmap = Pixmap::new(spec.width, spec.height)
        .ok_or(RenderError::PixmapAlloc)?;
    // White background — matches the panel's natural "no ink" state.
    pixmap.fill_rect(
        Rect::from_xywh(0.0, 0.0, spec.width as f32, spec.height as f32).unwrap(),
        &tiny_skia::Paint { shader: tiny_skia::Shader::SolidColor(Color::WHITE), ..Default::default() },
        Transform::identity(),
        None,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny_skia stores RGBA premultiplied; dither stage wants un-premul.
    let rgba: Vec<u8> = pixmap.data().to_vec();

    // ── Dither + pack per color mode ──
    match spec.color_mode {
        ColorMode::Mono1bpp
        | ColorMode::MonoRed1bpp
        | ColorMode::MonoYellow1bpp => Ok(dither_pack_mono(&rgba, spec.width, spec.height)),
        ColorMode::Gray4 => Ok(dither_pack_gray_n(&rgba, spec.width, spec.height, 2)),
        ColorMode::Gray16 => Ok(dither_pack_gray_n(&rgba, spec.width, spec.height, 4)),
        ColorMode::Color7 => {
            // No ACeP palette path yet — render as mono so the boot screen
            // is at least readable on Color7 panels. Visual quality improves
            // when the palette dither lands.
            Ok(dither_pack_mono(&rgba, spec.width, spec.height))
        }
    }
}

#[derive(Debug)]
pub enum RenderError {
    SvgParse(String),
    PixmapAlloc,
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RenderError::SvgParse(s) => write!(f, "SVG parse: {}", s),
            RenderError::PixmapAlloc => write!(f, "pixmap alloc failed"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Walks `db` and returns the first family in `candidates` that has a face
/// installed. Used to give fontdb a concrete sans-serif/serif/monospace
/// target — without this, generic family names like "sans-serif" don't
/// resolve and text disappears at render time.
fn pick_first_installed_family(
    db: &usvg::fontdb::Database,
    candidates: &[&str],
) -> Option<String> {
    for &name in candidates {
        let lower = name.to_ascii_lowercase();
        let found = db.faces().any(|face| {
            face.families
                .iter()
                .any(|(family, _)| family.to_ascii_lowercase() == lower)
        });
        if found {
            return Some(name.to_string());
        }
    }
    None
}

/// Floyd-Steinberg dither → 1bpp MSB-first packed.
fn dither_pack_mono(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut gray: Vec<f32> = rgba
        .chunks_exact(4)
        .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
        .collect();
    floyd_steinberg(&mut gray, w, h, 1);

    let stride = (w + 7) / 8;
    let mut out = vec![0u8; stride * h];
    for y in 0..h {
        for x in 0..w {
            let v = gray[y * w + x];
            // bit set = white = ink off — matches the firmware's NVS-baked
            // Mono1bpp convention.
            if v >= 128.0 {
                out[y * stride + x / 8] |= 1 << (7 - (x % 8));
            }
        }
    }
    out
}

/// Floyd-Steinberg dither → `bits`-bit grayscale, MSB-first packing. For
/// `bits=2` you get Gray4 (4 levels, 2 bpp); for `bits=4` you get Gray16
/// (16 levels, 4 bpp).
fn dither_pack_gray_n(rgba: &[u8], width: u32, height: u32, bits: u32) -> Vec<u8> {
    debug_assert!(bits == 2 || bits == 4);
    let w = width as usize;
    let h = height as usize;
    let levels = 1u32 << bits;
    let mut gray: Vec<f32> = rgba
        .chunks_exact(4)
        .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
        .collect();
    floyd_steinberg(&mut gray, w, h, bits as i32);

    let pixels_per_byte = (8 / bits) as usize;
    let stride = (w + pixels_per_byte - 1) / pixels_per_byte;
    let mut out = vec![0u8; stride * h];
    let mask = ((1u32 << bits) - 1) as u8;
    for y in 0..h {
        for x in 0..w {
            let v = gray[y * w + x];
            // Quantise to the nearest level.
            let lvl = ((v / 255.0) * (levels - 1) as f32)
                .round()
                .clamp(0.0, (levels - 1) as f32) as u8;
            let p_in_byte = x % pixels_per_byte;
            let shift = (pixels_per_byte - 1 - p_in_byte) as u32 * bits;
            out[y * stride + x / pixels_per_byte] |= (lvl & mask) << shift;
        }
    }
    out
}

/// Floyd-Steinberg error diffusion. Operates in-place on a grayscale plane.
/// `bits` is the target bit depth — controls the quantisation step.
fn floyd_steinberg(plane: &mut [f32], width: usize, height: usize, bits: i32) {
    let levels = (1i32 << bits) as f32;
    let step = 255.0 / (levels - 1.0);
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let old = plane[idx].clamp(0.0, 255.0);
            // Quantise to the nearest level.
            let new = (old / step).round() * step;
            plane[idx] = new;
            let err = old - new;
            if x + 1 < width {
                plane[idx + 1] += err * 7.0 / 16.0;
            }
            if y + 1 < height {
                if x > 0 {
                    plane[idx + width - 1] += err * 3.0 / 16.0;
                }
                plane[idx + width] += err * 5.0 / 16.0;
                if x + 1 < width {
                    plane[idx + width + 1] += err * 1.0 / 16.0;
                }
            }
        }
    }
}

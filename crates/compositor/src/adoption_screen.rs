//! Adoption screen — shown in the main region when the device has
//! no claim token in NVS (i.e. it hasn't been linked to a user
//! account yet).
//!
//! Current layout (no QR encoder yet — see task #85):
//!
//!     Adopt this device
//!
//!     Claim code:
//!         1234-5678          <- big font
//!
//!     Device:  D-XXXX
//!     IP:      10.0.1.42
//!
//!     Visit <adopt_url> and enter the code above
//!
//!     ⚠ Backend unreachable — retrying…   <- optional retry notice
//!
//! Once a no_std QR encoder lands the left half of the screen gets a
//! scannable QR pointing at `<adopt_url>?code=…&dev=…`.

use embedded_graphics::{
    geometry::{Point, Size},
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_8X13_BOLD, FONT_10X20},
    },
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use heapless::String as HString;

use crate::MainRegion;
use paperanywhere_ports::ColorMode;

/// Paint the adoption screen into the main-region framebuffer. The
/// caller is expected to have cleared the framebuffer to white first
/// (the compositor's `render_adoption_screen` does this).
pub fn draw_adoption_screen(
    region: &mut MainRegion<'_>,
    claim_code: &str,
    device_id: &str,
    ip: &str,
    adopt_url: &str,
    retry_notice: Option<&str>,
) {
    if !matches!(region.color_mode, ColorMode::Mono1bpp) {
        return;
    }
    let mut target = crate::status_bar::Mono1bppTarget::new(
        region.bytes,
        region.width_px,
        region.height_px,
    );
    let region_w = region.width_px as i32;
    let region_h = region.height_px as i32;

    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let left = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Bottom)
        .build();
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    let cx = region_w / 2;

    // Title — centered, large.
    let _ = Text::with_text_style(
        "Adopt this device",
        Point::new(cx, 70),
        big,
        centered,
    )
    .draw(&mut target);

    // "Claim code:" label centered.
    let _ = Text::with_text_style(
        "Claim code:",
        Point::new(cx, 130),
        bold,
        centered,
    )
    .draw(&mut target);

    // The code itself, very large + boxed for emphasis. FONT_10X20
    // → "1234-5678" is 9 chars × 10 px = 90 px wide.
    let code_text_baseline = 200;
    let _ = Text::with_text_style(
        claim_code,
        Point::new(cx, code_text_baseline),
        big,
        centered,
    )
    .draw(&mut target);
    // Box around the code so it pops at panel pixel density.
    let box_w = ((claim_code.len() as i32) * 10 + 40).max(140);
    let box_h = 40;
    let box_x = cx - box_w / 2;
    let box_y = code_text_baseline - 30;
    let _ = Rectangle::new(Point::new(box_x, box_y), Size::new(box_w as u32, box_h as u32))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
        .draw(&mut target);

    // Two-column details below the code: Device + IP.
    let detail_y = 260;
    let col_gap = 80;
    let device_label = "Device:";
    let ip_label = "IP:";
    let device_full = {
        let mut s: HString<48> = HString::new();
        let _ = s.push_str(device_label);
        let _ = s.push_str("  ");
        let _ = s.push_str(device_id);
        s
    };
    let ip_full = {
        let mut s: HString<48> = HString::new();
        let _ = s.push_str(ip_label);
        let _ = s.push_str("      ");
        let _ = s.push_str(ip);
        s
    };
    let _ = Text::with_text_style(
        device_full.as_str(),
        Point::new(cx - col_gap - 100, detail_y),
        small,
        left,
    )
    .draw(&mut target);
    let _ = Text::with_text_style(
        ip_full.as_str(),
        Point::new(cx + 20, detail_y),
        small,
        left,
    )
    .draw(&mut target);

    // Footer hint with the adopt URL.
    let mut hint: HString<160> = HString::new();
    let _ = hint.push_str("Visit ");
    let _ = hint.push_str(adopt_url);
    let _ = hint.push_str(" and enter the code above");
    let _ = Text::with_text_style(
        hint.as_str(),
        Point::new(cx, region_h - 30),
        small,
        centered,
    )
    .draw(&mut target);

    // Optional retry notice — rendered just above the footer hint with
    // a small attention marker. Caller sets this when the device can
    // see the network but can't reach the backend (or DHCP hasn't
    // landed yet, etc.). Truncates at 80 chars to keep one line on
    // even the smallest supported panel.
    if let Some(msg) = retry_notice {
        let mut line: HString<96> = HString::new();
        // Leading "!" inside a small box stands in for a triangle —
        // we don't have a glyph for ⚠ in FONT_6X10, and dragging in
        // an icon raster just for this would be overkill.
        let _ = line.push_str("[!] ");
        for c in msg.chars().take(80) {
            let _ = line.push(c);
        }
        let _ = Text::with_text_style(
            line.as_str(),
            Point::new(cx, region_h - 60),
            small,
            centered,
        )
        .draw(&mut target);
    }
}

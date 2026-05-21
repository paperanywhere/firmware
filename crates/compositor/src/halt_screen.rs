//! Halt screen — the device's "blue screen of death".
//!
//! Painted on the main region when the runtime hits a terminal
//! error (web endpoint unreachable, hash mismatch on a verified
//! payload, etc.) Caller paints, refreshes once with the FULL LUT,
//! then busy-loops forever — no further wake cycles, no deep
//! sleep, no auto-retry. The user must power-cycle or
//! re-provision to recover.
//!
//! Visual style mirrors Windows 11's BSOD: large sad-face token,
//! a one-sentence headline, a short descriptive paragraph, and an
//! error code at the bottom for support / log correlation.
//!
//! ```text
//!                 :(
//!
//!     Your device ran into a problem.
//!
//!     Could not reach the configured backend
//!     after several retries.
//!
//!     For more information, see:
//!     https://paperanywhere.io/errors/PA-NET-001
//!
//!     Error code: PA-NET-001
//! ```

use embedded_graphics::{
    geometry::Point,
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_8X13_BOLD, FONT_10X20},
    },
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use heapless::String as HString;

use crate::MainRegion;
use paperanywhere_ports::ColorMode;

/// Paint the halt screen onto the main region. Caller should clear
/// the framebuffer to white first (the compositor's
/// `render_halt_screen` does this).
pub fn draw_halt_screen(
    region: &mut MainRegion<'_>,
    headline: &str,
    detail: &str,
    code: &str,
) {
    if !matches!(region.color_mode, ColorMode::Mono1bpp) {
        return;
    }
    let mut target = crate::status_bar::Mono1bppTarget::new(
        region.bytes,
        region.width_px,
        region.height_px,
    );

    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    let cx = (region.width_px as i32) / 2;
    let h = region.height_px as i32;

    // Oversized ":(" — the symbol everyone associates with "something
    // went terribly wrong." We render it as text via a stack of two
    // FONT_10X20 chars; that's already the largest font we ship.
    // Scale up by sampling: print the same text multiple times with
    // small offsets to thicken the strokes.
    let face_y = h / 3;
    for dy in 0..=2 {
        for dx in 0..=2 {
            let _ = Text::with_text_style(
                ":(",
                Point::new(cx + dx, face_y + dy),
                big,
                centered,
            )
            .draw(&mut target);
        }
    }

    // Headline — bold, 8x13.
    let _ = Text::with_text_style(
        headline,
        Point::new(cx, face_y + 80),
        bold,
        centered,
    )
    .draw(&mut target);

    // Detail — small font, can be ≤ 80 chars without wrapping at
    // 800 px / 6 px = 133 chars. The caller is expected to keep it
    // short; we don't word-wrap here.
    let _ = Text::with_text_style(
        detail,
        Point::new(cx, face_y + 110),
        small,
        centered,
    )
    .draw(&mut target);

    // "For more information" footer with a link template + the code.
    let mut url_line: HString<96> = HString::new();
    let _ = url_line.push_str("For more information, see paperanywhere.io/errors/");
    let _ = url_line.push_str(code);
    let _ = Text::with_text_style(
        url_line.as_str(),
        Point::new(cx, h - 60),
        small,
        centered,
    )
    .draw(&mut target);

    // Big error-code line at the bottom.
    let mut code_line: HString<48> = HString::new();
    let _ = code_line.push_str("Error code: ");
    let _ = code_line.push_str(code);
    let _ = Text::with_text_style(
        code_line.as_str(),
        Point::new(cx, h - 25),
        bold,
        centered,
    )
    .draw(&mut target);
}

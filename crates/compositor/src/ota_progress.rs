//! Live OTA-progress view — rendered during a firmware update
//! (backend-instructed download, future GitHub-releases fetcher,
//! etc.). Layout:
//!
//!     Updating firmware
//!
//!     ╔════════════════════════════════╗
//!     ║███████████████░░░░░░░░░░░░░░░░░║   <- progress bar
//!     ╚════════════════════════════════╝
//!              45%
//!
//!     Receiving 421 / 938 KB
//!
//!     Do not power off the device.
//!
//! The compositor calls into here from `render_ota_progress`; first
//! call after a non-Idle phase forces a full-LUT refresh (so the
//! prior view — boot/image/adoption — is fully cleared), subsequent
//! ticks use the fast LUT so the bar can update without a 3 s flash
//! on every chunk.

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
use paperanywhere_ports::{ColorMode, OtaPhase};

pub fn draw_ota_progress(region: &mut MainRegion<'_>, phase: OtaPhase) {
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
    let cx = region_w / 2;

    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    // ── Title ─────────────────────────────────────────────────────
    let title = match phase {
        OtaPhase::Failed { .. } => "Update failed",
        _ => "Updating firmware",
    };
    let _ = Text::with_text_style(title, Point::new(cx, 90), big, centered).draw(&mut target);

    // ── Progress bar ──────────────────────────────────────────────
    let percent = phase.percent();
    let bar_w = (region_w as f32 * 0.6) as i32;
    let bar_h = 36;
    let bar_x = cx - bar_w / 2;
    let bar_y = 150;

    // Outer frame (stroke 2 px).
    let _ = Rectangle::new(Point::new(bar_x, bar_y), Size::new(bar_w as u32, bar_h as u32))
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 2))
        .draw(&mut target);

    // Fill — width = bar_w * percent / 100, leave a 4 px inset from
    // the frame so we never overlap the stroke.
    let inset = 4;
    let fill_max = bar_w - 2 * inset;
    let fill_w = (fill_max * percent as i32 / 100).max(0);
    if fill_w > 0 {
        let _ = Rectangle::new(
            Point::new(bar_x + inset, bar_y + inset),
            Size::new(fill_w as u32, (bar_h - 2 * inset) as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(&mut target);
    }

    // Percent label centered below the bar. Width-padded to 3 chars
    // so "5%" → "  5%" → "100%" all anchor at the same screen X and
    // the text doesn't visibly shift between progress ticks.
    let mut pct_buf: HString<8> = HString::new();
    let _ = core::fmt::write(&mut pct_buf, format_args!("{:>3}%", percent));
    let _ = Text::with_text_style(
        pct_buf.as_str(),
        Point::new(cx, bar_y + bar_h + 30),
        bold,
        centered,
    )
    .draw(&mut target);

    // ── Status line ───────────────────────────────────────────────
    //
    // The byte counts inside `write_status` are right-aligned to 5
    // digits ("    1 KB" → "  421 KB" → "  938 KB"), and the
    // surrounding "Receiving " / " / " / " KB" delimiters are
    // constant-width — so the whole string is fixed length across
    // every tick during a single push. Combined with `centered`
    // alignment this means the text stays anchored on the same
    // screen X column instead of dancing as the digit count grows.
    let mut status_buf: HString<64> = HString::new();
    phase.write_status(&mut status_buf);
    let _ = Text::with_text_style(
        status_buf.as_str(),
        Point::new(cx, bar_y + bar_h + 70),
        bold,
        centered,
    )
    .draw(&mut target);

    // ── Footer caution ────────────────────────────────────────────
    let footer = match phase {
        OtaPhase::Failed { .. } => "Reset the device or re-run pa-dev push.",
        _ => "Do not power off the device.",
    };
    let _ = Text::with_text_style(
        footer,
        Point::new(cx, region_h - 30),
        small,
        centered,
    )
    .draw(&mut target);
}

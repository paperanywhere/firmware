//! Main-view placeholder — shown in the main region when the device
//! is adopted (has a device token) but the backend hasn't yet pushed
//! an image for this device. Future iterations will pull a "default
//! background" set by the user from /state and render that instead;
//! for now this is a friendly text view that tells the user the
//! device is connected and waiting.
//!
//! Layout:
//!
//!     Connected
//!
//!     Waiting for content from the dashboard...
//!
//!     IP:           10.0.1.42
//!     Last update:  --

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

pub fn draw_main_placeholder(
    region: &mut MainRegion<'_>,
    ip: &str,
    last_update: Option<&str>,
    owner_email: Option<&str>,
    project_name: Option<&str>,
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

    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();
    let left = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Bottom)
        .build();

    let cx = region_w / 2;

    let _ = Text::with_text_style("Connected", Point::new(cx, 90), big, centered)
        .draw(&mut target);

    let _ = Text::with_text_style(
        "Waiting for content from the dashboard...",
        Point::new(cx, 150),
        bold,
        centered,
    )
    .draw(&mut target);

    // Detail rows, left-aligned in a single column that starts about
    // a third of the way across the panel. Labels are fixed-width
    // (14 chars including trailing spaces) so the values line up at
    // the same x regardless of label length.
    let detail_x = cx - 180;
    let mut row_y = 220;
    const ROW_PX: i32 = 18;

    draw_kv(
        &mut target,
        small,
        left,
        detail_x,
        row_y,
        "Owner:        ",
        owner_email.unwrap_or("--"),
    );
    row_y += ROW_PX;

    draw_kv(
        &mut target,
        small,
        left,
        detail_x,
        row_y,
        "Project:      ",
        project_name.unwrap_or("--"),
    );
    row_y += ROW_PX;

    draw_kv(&mut target, small, left, detail_x, row_y, "IP:           ", ip);
    row_y += ROW_PX;

    draw_kv(
        &mut target,
        small,
        left,
        detail_x,
        row_y,
        "Last update:  ",
        last_update.unwrap_or("--"),
    );
}

fn draw_kv(
    target: &mut crate::status_bar::Mono1bppTarget<'_>,
    style: MonoTextStyle<'_, BinaryColor>,
    text_style: embedded_graphics::text::TextStyle,
    x: i32,
    y: i32,
    label: &str,
    value: &str,
) {
    // Truncate the value at 48 chars so a long email + an even
    // longer project name can't overflow the heapless buffer or
    // spill into the right-side status bar.
    let mut line: HString<80> = HString::new();
    let _ = line.push_str(label);
    for c in value.chars().take(48) {
        let _ = line.push(c);
    }
    let _ = Text::with_text_style(line.as_str(), Point::new(x, y), style, text_style)
        .draw(target);
}

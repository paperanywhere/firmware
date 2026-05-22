//! Cardstock playlist page renderer.
//!
//! Walks a [`cardstock::Page`]'s card tree and rasterises each
//! variant into the main-region framebuffer. v1 supports only the
//! four system-widget cards (Battery / Clock / Wifi / Ip); each
//! reads its value from the firmware's chrome KV at paint time.
//!
//! Unknown variants are silently skipped — the schema is explicitly
//! forward-compatible (firmware MUST tolerate card types it doesn't
//! understand yet, per cardstock/README.md).

use embedded_graphics::{
    geometry::Point,
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_8X13_BOLD},
    },
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use heapless::String as HString;

use cardstock::{Card, Page, WidgetStyle};
use paperanywhere_ports::{ColorMode, chrome};

use crate::MainRegion;

pub fn draw_playlist_page(
    region: &mut MainRegion<'_>,
    page: &Page,
    index: u16,
    total: u16,
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

    let snap = chrome::snapshot();

    draw_card(&mut target, &page.layout, &snap);

    // Position pips along the bottom of the main region: filled
    // circle for the current page, hollow for the rest. Only render
    // when there's more than one page — a single-page playlist
    // doesn't need the indicator.
    if total > 1 {
        draw_pips(&mut target, region_w, region_h, index, total);
    }
}

fn draw_card(
    target: &mut crate::status_bar::Mono1bppTarget<'_>,
    card: &Card,
    snap: &chrome::ChromeState,
) {
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let left = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Bottom)
        .build();

    match card {
        Card::Battery { x, y, style } => {
            let value = match (snap.battery_percent, snap.battery_mv) {
                (Some(p), Some(mv)) => format_pct_mv(p, mv),
                (Some(p), None) => format_pct(p),
                _ => HString::<32>::try_from("--").unwrap_or_default(),
            };
            draw_widget(target, *x, *y, *style, "Battery", value.as_str(), bold, small, left);
        }
        Card::Clock {
            x,
            y,
            format: _,
            style,
            tz_offset_minutes: _,
            refresh_hint: _,
        } => {
            // NTP sync isn't wired yet (task #78). Render a stable
            // "--:--" placeholder so the card slot still occupies
            // the position the user laid out. tz_offset_minutes +
            // refresh_hint land here once NTP gives us a real wall
            // clock to apply them to.
            let value = HString::<32>::try_from("--:--").unwrap_or_default();
            draw_widget(target, *x, *y, *style, "Time", value.as_str(), bold, small, left);
        }
        Card::Wifi { x, y, style } => {
            let value = match (snap.ssid.as_deref(), snap.rssi_dbm) {
                (Some(ssid), Some(rssi)) => format_ssid_rssi(ssid, rssi),
                (Some(ssid), None) => HString::<32>::try_from(ssid).unwrap_or_default(),
                _ => HString::<32>::try_from("--").unwrap_or_default(),
            };
            draw_widget(target, *x, *y, *style, "WiFi", value.as_str(), bold, small, left);
        }
        Card::Ip { x, y, style } => {
            let value: HString<32> = snap
                .ip
                .as_deref()
                .map(|s| HString::<32>::try_from(s).unwrap_or_default())
                .unwrap_or_default();
            let value_str = if value.is_empty() { "--" } else { value.as_str() };
            draw_widget(target, *x, *y, *style, "IP", value_str, bold, small, left);
        }
        // Weather + Graph are server-rasterised cards: their data
        // ships alongside the playlist in `DeviceState.datasources`,
        // and the eventual renderer paints a pre-packed framebuffer
        // for the card's pixel rectangle. Skipping silently here
        // keeps the forward-compat contract while the dedicated
        // renderers come online.
        Card::Weather { .. } | Card::Graph { .. } => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_widget(
    target: &mut crate::status_bar::Mono1bppTarget<'_>,
    x: i32,
    y: i32,
    style: WidgetStyle,
    label: &str,
    value: &str,
    bold: MonoTextStyle<'_, BinaryColor>,
    small: MonoTextStyle<'_, BinaryColor>,
    text_style: embedded_graphics::text::TextStyle,
) {
    match style {
        // Icon-only style not yet supported (no icon glyphs cached
        // for the main-region widget sizes); fall through to
        // Labelled with the label text alone.
        WidgetStyle::Compact => {
            let _ = Text::with_text_style(label, Point::new(x, y), bold, text_style)
                .draw(target);
        }
        WidgetStyle::TextOnly => {
            let _ = Text::with_text_style(value, Point::new(x, y), bold, text_style)
                .draw(target);
        }
        WidgetStyle::Labelled => {
            // "Label  value" on one line. Cheap and good enough for
            // v1; future versions get icon assets.
            let mut line: HString<64> = HString::new();
            let _ = line.push_str(label);
            let _ = line.push_str(":  ");
            for c in value.chars().take(40) {
                let _ = line.push(c);
            }
            let _ = Text::with_text_style(line.as_str(), Point::new(x, y), bold, text_style)
                .draw(target);
            // Subtle baseline marker so widgets read as distinct
            // even when the user stacks them close together.
            let _ = Text::with_text_style(
                "—",
                Point::new(x, y + 4),
                small,
                text_style,
            )
            .draw(target);
        }
    }
}

fn draw_pips(
    target: &mut crate::status_bar::Mono1bppTarget<'_>,
    region_w: i32,
    region_h: i32,
    index: u16,
    total: u16,
) {
    use embedded_graphics::primitives::{Circle, PrimitiveStyle};
    const PIP_R: u32 = 4;
    const PIP_GAP: i32 = 14;
    let total_i = total as i32;
    let total_w = total_i * (2 * PIP_R as i32) + (total_i - 1) * (PIP_GAP - 2 * PIP_R as i32);
    let start_x = (region_w - total_w) / 2;
    let y = region_h - 16;
    for i in 0..total_i {
        let cx = start_x + i * PIP_GAP;
        let circle = Circle::new(Point::new(cx, y), PIP_R * 2);
        if i as u16 == index {
            let _ = circle
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(target);
        } else {
            let _ = circle
                .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
                .draw(target);
        }
    }
}

fn format_pct(p: u8) -> HString<32> {
    let mut s: HString<32> = HString::new();
    let _ = write_u32(&mut s, p as u32);
    let _ = s.push('%');
    s
}

fn format_pct_mv(p: u8, mv: u16) -> HString<32> {
    let mut s: HString<32> = HString::new();
    let _ = write_u32(&mut s, p as u32);
    let _ = s.push_str("%  (");
    let _ = write_u32(&mut s, mv as u32);
    let _ = s.push_str(" mV)");
    s
}

fn format_ssid_rssi(ssid: &str, rssi: i16) -> HString<32> {
    let mut s: HString<32> = HString::new();
    for c in ssid.chars().take(20) {
        let _ = s.push(c);
    }
    let _ = s.push_str("  (");
    if rssi < 0 {
        let _ = s.push('-');
        let _ = write_u32(&mut s, (-(rssi as i32)) as u32);
    } else {
        let _ = write_u32(&mut s, rssi as u32);
    }
    let _ = s.push_str(" dBm)");
    s
}

fn write_u32<const N: usize>(s: &mut HString<N>, mut n: u32) -> Result<(), ()> {
    if n == 0 {
        return s.push('0').map_err(|_| ());
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    while n > 0 && i > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let slice = core::str::from_utf8(&buf[i..]).map_err(|_| ())?;
    s.push_str(slice).map_err(|_| ())
}

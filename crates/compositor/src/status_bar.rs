//! Status-bar layer.
//!
//! Layout (left → right):
//!
//!   [IP: 10.0.1.42  |  Last Update: 10:23][USB][Battery 87%][WiFi]
//!
//! - 1 px black border framing the whole bar (top + bottom + sides).
//! - 1 px vertical dividers between widget cells on the right side.
//! - Left half is informational text: current IP state + local time
//!   of the most recent panel refresh.
//! - Right side has the chrome icons: USB (only when a serial host is
//!   present), battery + percent, wifi.
//! - Device ID lives on the boot screen (under the version line), not
//!   here — kept off the bar to keep the chrome compact.

use embedded_graphics::{
    geometry::{Point, Size},
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_8X13_BOLD},
    },
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use heapless::String as HString;

use crate::{BuildInfo, MainRegion, battery_mv_to_percent};
use paperanywhere_ports::ColorMode;

/// Inputs the runtime supplies before each refresh. Owned by value
/// inside the compositor; setters on the compositor's hook surface
/// (`set_chrome`, `on_wifi_state_changed`, …) mutate this in place.
#[derive(Debug, Clone, Default)]
pub struct StatusInputs {
    /// `None` if the firmware hasn't read a battery sample yet (e.g.
    /// board without a battery_adc binding). Renders as an outline-
    /// only icon with no fill / no percent.
    pub battery_mv: Option<u16>,
    /// RSSI of the current WiFi association, or `None` when not
    /// associated. Used as a binary connected/disconnected signal in
    /// the icon; the actual dBm value isn't shown today.
    pub wifi_rssi_dbm: Option<i16>,
    /// `Some(true)` shows the USB icon (a serial host is enumerated).
    /// `None` or `Some(false)` hides it. Stays `None` on boards without
    /// USB-CDC support — they should never set this.
    pub usb_connected: Option<bool>,
    /// Short device id rendered on the left side of the bar
    /// (e.g. `D-3F2A`). `None` shows a placeholder.
    pub device_id: Option<HString<24>>,
    /// Local-time stamp of the most recent successful render
    /// (e.g. `10:23`). `None` shows `--`.
    pub last_update_local: Option<HString<24>>,
    /// IPv4 dotted-quad string (e.g. `10.0.1.42`). Shown on the
    /// boot-screen overlay + left side of the status bar so the
    /// developer can `pa-dev push --device <ip>` without ARP-scanning.
    pub ip_address: Option<HString<24>>,
}

/// Render the bar into `framebuffer` (Mono1bpp packed row-major,
/// MSB-first). Other color modes are no-ops for now — `#71` adds
/// rasterisers for Gray4 / Gray16 / Color7.
pub fn render(
    status: &StatusInputs,
    framebuffer: &mut [u8],
    width_px: u32,
    status_bar_height: u32,
    color_mode: ColorMode,
) {
    if !matches!(color_mode, ColorMode::Mono1bpp) {
        return;
    }

    let mut target = Mono1bppTarget::new(framebuffer, width_px, status_bar_height);

    // Clear the bar to white. set_pixel with `on=false` writes a 1 bit
    // (no-ink) under the rasterizer convention. fill_rect over the
    // whole bar leaves us a clean canvas to draw the border + widgets.
    target.fill_rect(0, 0, width_px, status_bar_height, false);

    // Black border framing the whole bar — 1 px on top, bottom, and
    // both sides. Cheap and gives the user a visual anchor without
    // flooding the bar with ink.
    draw_border(&mut target, width_px, status_bar_height);

    // Right-edge cells, laid out right-to-left. Each cell ends at
    // `right` and consumes the width its widget needs; we draw a
    // 1 px vertical divider on each cell's LEFT edge so cells are
    // visually separated.
    let inset = 1; // border eats one pixel
    let mut right = (width_px as i32) - inset - 10; // 10 px right margin (was 4 — keeps the wifi glyph + BATT label off the panel edge)

    let cell_width = draw_wifi_cell(&mut target, right, status.wifi_rssi_dbm.is_some());
    let next_right = right - cell_width;
    draw_vertical_divider(&mut target, next_right, status_bar_height);
    right = next_right - 1;

    let cell_width = draw_battery_cell(&mut target, right, status.battery_mv);
    let next_right = right - cell_width;
    draw_vertical_divider(&mut target, next_right, status_bar_height);
    right = next_right - 1;

    if matches!(status.usb_connected, Some(true)) {
        let cell_width = draw_usb_cell(&mut target, right);
        let next_right = right - cell_width;
        draw_vertical_divider(&mut target, next_right, status_bar_height);
        right = next_right - 1;
    }

    // Everything to the left of `right` is the info-text region.
    draw_left_info(&mut target, status, inset + 6, right);
}

fn draw_border(target: &mut Mono1bppTarget<'_>, width_px: u32, height_px: u32) {
    let w = width_px as i32;
    let h = height_px as i32;
    // Top + bottom hairlines.
    target.fill_rect(0, 0, width_px, 1, true);
    target.fill_rect(0, (h - 1) as u32, width_px, 1, true);
    // Left + right hairlines.
    target.fill_rect(0, 0, 1, height_px, true);
    target.fill_rect((w - 1) as u32, 0, 1, height_px, true);
}

fn draw_vertical_divider(target: &mut Mono1bppTarget<'_>, x: i32, height_px: u32) {
    // 2 px top/bottom inset so the divider doesn't kiss the border.
    target.fill_rect_signed(x, 2, 1, height_px - 4, true);
}

// ── Right-side widgets ───────────────────────────────────────────────────────

fn draw_wifi_cell(target: &mut Mono1bppTarget<'_>, right_edge: i32, connected: bool) -> i32 {
    let icon_size = crate::icons::ICON_PX as i32;
    // Split padding evenly so the icon sits in the geometric middle
    // of its cell (was right-flush, which read as visually pulled
    // toward the panel border).
    const PADDING: i32 = 6;
    let cell_width = icon_size + PADDING;
    let top = ((target.height as i32) - icon_size) / 2;
    let left = right_edge - icon_size - PADDING / 2;
    let bitmap = if connected {
        crate::icons::WIFI
    } else {
        crate::icons::WIFI_SLASH
    };
    blit_mono_icon(target, bitmap, crate::icons::ICON_PX, crate::icons::ICON_PX, left, top);
    cell_width
}

/// Stamp a build-time-rasterised Mono1bpp icon onto the framebuffer.
/// Source convention matches the boot-screen rasteriser (bit set =
/// no ink, bit clear = ink). Only the "ink" pixels are painted; white
/// pixels leave the existing framebuffer underneath intact, so the
/// icon overlays cleanly without a knock-out background.
fn blit_mono_icon(
    target: &mut Mono1bppTarget<'_>,
    bitmap: &[u8],
    src_w: u32,
    src_h: u32,
    dst_x: i32,
    dst_y: i32,
) {
    let stride = ((src_w + 7) / 8) as usize;
    for sy in 0..src_h {
        let row_offset = (sy as usize) * stride;
        for sx in 0..src_w {
            let byte_idx = row_offset + (sx / 8) as usize;
            if byte_idx >= bitmap.len() {
                break;
            }
            let bit_mask = 1u8 << (7 - (sx % 8));
            // bit clear = ink; only then paint a pixel.
            if bitmap[byte_idx] & bit_mask == 0 {
                target.set_pixel(dst_x + sx as i32, dst_y + sy as i32, true);
            }
        }
    }
}

fn draw_battery_cell(target: &mut Mono1bppTarget<'_>, right_edge: i32, mv: Option<u16>) -> i32 {
    // Text-only label: "87% BATT" right-aligned to the cell edge.
    // No glyph at all — the cell border + the all-caps "BATT" suffix
    // are enough to identify the field without a battery icon.
    const PADDING: i32 = 6;
    // Inset so the last character isn't kissing the divider on its
    // right. embedded-graphics right-alignment puts the text's last
    // pixel at the given x; we offset 4 px inward to leave breathing
    // room between the "T" of BATT and the cell boundary.
    const TEXT_RIGHT_INSET: i32 = 4;
    let baseline = (target.height as i32) / 2 + 4;
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let right_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Right)
        .baseline(Baseline::Bottom)
        .build();

    let mut text: HString<16> = HString::new();
    match mv {
        Some(mv) => {
            let pct = battery_mv_to_percent(mv);
            let mut buf = [0u8; 5];
            let pct_str = u8_to_str(pct, &mut buf);
            let _ = text.push_str(pct_str);
            let _ = text.push_str(" BATT");
        }
        None => {
            let _ = text.push_str("-- BATT");
        }
    }

    let _ = Text::with_text_style(
        text.as_str(),
        Point::new(right_edge - TEXT_RIGHT_INSET, baseline),
        style,
        right_aligned,
    )
    .draw(target);

    // 9 chars max ("100% BATT") × 6 px per char + inset + padding.
    (text.len() as i32) * 6 + TEXT_RIGHT_INSET + PADDING
}

fn draw_usb_cell(target: &mut Mono1bppTarget<'_>, right_edge: i32) -> i32 {
    const ICON_W: i32 = 14;
    const ICON_H: i32 = 14;
    const PADDING: i32 = 6;
    let top = ((target.height as i32) - ICON_H) / 2;
    let left = right_edge - ICON_W;

    // Stylised USB glyph: trident head + stem. Hand-drawn so it
    // doesn't need a font glyph; replace with a Font Awesome bitmap
    // once `#71`'s sibling task lands.
    // Stem
    target.fill_rect_signed(left + ICON_W / 2 - 1, top + 2, 2, (ICON_H - 4) as u32, true);
    // Left fork
    target.fill_rect_signed(left + 1, top + 4, 4, 2, true);
    target.fill_rect_signed(left + 4, top + 4, 2, 6, true);
    // Right fork (square cap)
    target.fill_rect_signed(left + ICON_W - 5, top + 4, 4, 2, true);
    target.fill_rect_signed(left + ICON_W - 5, top + 4, 2, 6, true);
    // Round head at top
    target.fill_rect_signed(left + ICON_W / 2 - 2, top, 4, 3, true);
    // Plug at bottom
    target.fill_rect_signed(left + ICON_W / 2 - 3, top + ICON_H - 3, 6, 3, true);

    ICON_W + PADDING
}

// ── Left-side info text ─────────────────────────────────────────────────────

fn draw_left_info(
    target: &mut Mono1bppTarget<'_>,
    status: &StatusInputs,
    left_x: i32,
    right_limit: i32,
) {
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let baseline = (target.height as i32) / 2 + 4;

    // "Last Update: HH:MM" — the only text on the bar's left side.
    // IP and Device UUID live on the boot screen; the status bar is
    // intentionally compact so the chrome icons dominate.
    let mut line: HString<48> = HString::new();
    let _ = line.push_str("Last Update: ");
    match status.last_update_local.as_ref() {
        Some(t) => {
            let _ = line.push_str(t.as_str());
        }
        None => {
            let _ = line.push_str("--");
        }
    }

    // Clip naively to the available width by chopping the string at
    // 6 px per char. Past the trim point we just lose the trailing
    // characters; the alternative (ellipsis logic) isn't worth the
    // bytes today.
    let max_chars = ((right_limit - left_x) / 6).max(0) as usize;
    let s = if line.len() > max_chars {
        &line.as_str()[..max_chars]
    } else {
        line.as_str()
    };

    let _ = Text::with_text_style(
        s,
        Point::new(left_x, baseline),
        style,
        TextStyleBuilder::new()
            .alignment(Alignment::Left)
            .baseline(Baseline::Bottom)
            .build(),
    )
    .draw(target);
}

// ── Boot-screen overlay (used by boot.rs after the logo lands) ───────────────

/// Render the boot-screen overlay under the centered logo. Five
/// `Key: Value` lines, stacked from the top of the text block
/// downward toward the panel's bottom margin:
///
///   Build:       <version>
///   Environment: dev | production
///   Build Date:  <build_time>
///   IP:          <ip state>
///   Device UUID: <device_uuid>
///
/// `ip` is whatever the runtime last pushed via
/// [`paperanywhere_ports::EpaperPanel::set_ip`] — typically
/// `"10.0.1.42"`, `"connecting..."`, `"not connected"`, or
/// `"failed"`. Always rendered (no Option, no separate code path)
/// so the developer sees the connection state at a glance even
/// before DHCP completes.
pub fn draw_build_info(
    region: &mut MainRegion<'_>,
    info: &BuildInfo,
    device_uuid: &str,
    ip: &str,
) {
    if !matches!(region.color_mode, ColorMode::Mono1bpp) {
        return;
    }
    let mut target = Mono1bppTarget::new(region.bytes, region.width_px, region.height_px);

    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    let center_x = (region.width_px as i32) / 2;
    let bottom_y = (region.height_px as i32) - 8;

    // Stack five lines upward from the bottom margin with 12 px
    // between baselines. Total block height ~60 px; leaves the rest
    // of the main region for the logo above.
    let env = if info.is_dev { "dev" } else { "production" };
    let lines: [(&str, &str); 5] = [
        ("Build", info.fw_version),
        ("Environment", env),
        ("Build Date", info.build_time),
        ("IP", ip),
        ("Device UUID", device_uuid),
    ];

    for (i, (key, value)) in lines.iter().enumerate() {
        // bottom_y is the LAST line; iter index 0 is the topmost,
        // so we subtract from bottom_y by (count-1 - i) * 12.
        let y = bottom_y - ((lines.len() as i32 - 1 - i as i32) * 12);
        let mut buf: HString<80> = HString::new();
        let _ = buf.push_str(key);
        let _ = buf.push_str(": ");
        let _ = buf.push_str(value);
        let _ = Text::with_text_style(
            buf.as_str(),
            Point::new(center_x, y),
            style,
            centered,
        )
        .draw(&mut target);
    }
}

// ── Mono1bpp draw target ─────────────────────────────────────────────────────
//
// `embedded-graphics::DrawTarget` adapter over our row-major MSB-first
// framebuffer. Pixel value `On` ⇒ "paint ink (black)" which corresponds
// to *clearing* the bit under the rasterizer convention (bit set = no
// ink = white). The polarity here matches what the boot-screen crate
// emits, so the UC8179 driver's `panel_data_inverted` pass produces
// the right thing on hardware.

struct Mono1bppTarget<'a> {
    bytes: &'a mut [u8],
    width_px: u32,
    height: u32,
}

impl<'a> Mono1bppTarget<'a> {
    fn new(bytes: &'a mut [u8], width_px: u32, height_px: u32) -> Self {
        Self { bytes, width_px, height: height_px }
    }

    fn set_pixel(&mut self, x: i32, y: i32, on: bool) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);
        if x >= self.width_px || y >= self.height {
            return;
        }
        let stride = (self.width_px + 7) / 8;
        let byte_idx = (y * stride + x / 8) as usize;
        let bit = 7 - (x % 8) as u8;
        if byte_idx >= self.bytes.len() {
            return;
        }
        // Rasterizer convention: bit set = white. To paint ink (black)
        // we CLEAR the bit; to paint background (white) we SET it.
        if on {
            self.bytes[byte_idx] &= !(1 << bit);
        } else {
            self.bytes[byte_idx] |= 1 << bit;
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, on: bool) {
        for yy in y..(y + h) {
            for xx in x..(x + w) {
                self.set_pixel(xx as i32, yy as i32, on);
            }
        }
    }

    fn fill_rect_signed(&mut self, x: i32, y: i32, w: u32, h: u32, on: bool) {
        for yy in y..(y + h as i32) {
            for xx in x..(x + w as i32) {
                self.set_pixel(xx, yy, on);
            }
        }
    }

    fn draw_rect_outline(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
        let _ = Rectangle::new(Point::new(x, y), Size::new(w, h))
            .into_styled(style)
            .draw(self);
    }
}

impl<'a> Dimensions for Mono1bppTarget<'a> {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(Point::zero(), Size::new(self.width_px, self.height))
    }
}

impl<'a> DrawTarget for Mono1bppTarget<'a> {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            self.set_pixel(point.x, point.y, color == BinaryColor::On);
        }
        Ok(())
    }
}

/// Stringify a u8 percent into the provided buffer, returning a `&str`
/// view. Avoids pulling `alloc::format!` for the per-refresh path.
fn u8_to_str<'a>(n: u8, buf: &'a mut [u8; 5]) -> &'a str {
    let mut i = buf.len();
    i -= 1;
    buf[i] = b'%';
    let mut n = n as u16;
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n > 0 && i > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

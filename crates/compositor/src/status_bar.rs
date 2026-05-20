//! Status-bar layer.
//!
//! Layout (left → right):
//!
//!   [Device ID: D-3F2A  |  Last Update: 10:23][USB][Battery 87%][WiFi]
//!
//! - 1 px black border framing the whole bar (top + bottom + sides).
//! - 1 px vertical dividers between widget cells on the right side.
//! - Left half is informational text: short device id + the local time
//!   of the most recent panel refresh.
//! - Right side has the chrome icons: USB (only when a serial host is
//!   present), battery + percent, wifi.

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
    let mut right = (width_px as i32) - inset - 4; // 4 px right margin

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
    const ICON_W: i32 = 18;
    const ICON_H: i32 = 14;
    const PADDING: i32 = 6;
    let top = ((target.height as i32) - ICON_H) / 2;
    let left = right_edge - ICON_W;

    // Three horizontal bars, bottom-up, increasing in width — a
    // simplified "signal bars" glyph. Solid black; the cell border
    // already gives the bar enough separation visually.
    for (i, bar_w) in [6_i32, 12, 18].iter().enumerate() {
        let bar_h: i32 = 3;
        let y = top + ICON_H - (i as i32 + 1) * (bar_h + 1);
        let bar_x = left + (ICON_W - bar_w) / 2;
        target.fill_rect_signed(bar_x, y, *bar_w as u32, bar_h as u32, true);
    }

    if !connected {
        // Diagonal slash with a 1 px white outline so it reads as a cut
        // rather than another bar.
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 2);
        let _ = Line::new(Point::new(left, top + ICON_H), Point::new(left + ICON_W, top))
            .into_styled(style)
            .draw(target);
        let knockout = PrimitiveStyle::with_stroke(BinaryColor::Off, 1);
        let _ = Line::new(
            Point::new(left - 1, top + ICON_H),
            Point::new(left + ICON_W - 1, top),
        )
        .into_styled(knockout)
        .draw(target);
    }

    ICON_W + PADDING
}

fn draw_battery_cell(target: &mut Mono1bppTarget<'_>, right_edge: i32, mv: Option<u16>) -> i32 {
    const BODY_W: i32 = 22;
    const BODY_H: i32 = 12;
    const NUB_W: i32 = 2;
    const NUB_H: i32 = 6;
    const PADDING: i32 = 6;
    const LABEL_W: i32 = 26; // "100%" ≈ 24 px in FONT_6X10

    let top = ((target.height as i32) - BODY_H) / 2;
    let nub_x = right_edge - NUB_W;
    let nub_y = top + (BODY_H - NUB_H) / 2;
    target.fill_rect_signed(nub_x, nub_y, NUB_W as u32, NUB_H as u32, true);

    let body_right = nub_x;
    let body_left = body_right - BODY_W;
    target.draw_rect_outline(body_left, top, BODY_W as u32, BODY_H as u32);

    let label_baseline = top + (BODY_H + 9) / 2;
    let label_right = body_left - 3;
    if let Some(mv) = mv {
        let pct = battery_mv_to_percent(mv);
        let inner_w = (BODY_W - 4) as u32;
        let fill_w = (inner_w * pct as u32) / 100;
        if fill_w > 0 {
            target.fill_rect_signed(body_left + 2, top + 2, fill_w, (BODY_H - 4) as u32, true);
        }
        let mut buf = [0u8; 5];
        let s = u8_to_str(pct, &mut buf);
        let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let _ = Text::with_text_style(
            s,
            Point::new(label_right, label_baseline),
            style,
            TextStyleBuilder::new()
                .alignment(Alignment::Right)
                .baseline(Baseline::Bottom)
                .build(),
        )
        .draw(target);
    } else {
        // Empty / unknown battery: thin dash in the label slot to mark
        // the absence rather than leaving the cell blank.
        let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let _ = Text::with_text_style(
            "--",
            Point::new(label_right, label_baseline),
            style,
            TextStyleBuilder::new()
                .alignment(Alignment::Right)
                .baseline(Baseline::Bottom)
                .build(),
        )
        .draw(target);
    }

    BODY_W + NUB_W + LABEL_W + PADDING
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

    // Build "Device ID: XXX  |  IP: 10.0.1.42  |  Last Update: YYY"
    // without alloc::format. heapless::String<160> is sized for the
    // worst case (24 + 24 + 24 + labels + separators ≈ 100 chars).
    let mut line: HString<160> = HString::new();
    let _ = line.push_str("Device ID: ");
    if let Some(id) = status.device_id.as_ref() {
        let _ = line.push_str(id.as_str());
    } else {
        let _ = line.push_str("--");
    }
    let _ = line.push_str("  |  IP: ");
    if let Some(ip) = status.ip_address.as_ref() {
        let _ = line.push_str(ip.as_str());
    } else {
        let _ = line.push_str("--");
    }
    let _ = line.push_str("  |  Last Update: ");
    if let Some(t) = status.last_update_local.as_ref() {
        let _ = line.push_str(t.as_str());
    } else {
        let _ = line.push_str("--");
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

/// Render the firmware version + UTC build time stamp into the bottom-
/// center of the main region. Called from `boot.rs` after the boot-
/// screen logo has been blitted into the main-region framebuffer.
pub fn draw_build_info(region: &mut MainRegion<'_>, info: &BuildInfo) {
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

    let _ = Text::with_text_style(
        info.build_time,
        Point::new(center_x, bottom_y),
        style,
        centered,
    )
    .draw(&mut target);

    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    // Compose "<version> (DEV)" when the dev flag is set, else plain
    // version. heapless::String<48> is sized for the longest possible
    // SHA-suffixed version + " (DEV)" tail.
    let mut version_line: HString<48> = HString::new();
    let _ = version_line.push_str(info.fw_version);
    if info.is_dev {
        let _ = version_line.push_str(" (DEV)");
    }
    let _ = Text::with_text_style(
        version_line.as_str(),
        Point::new(center_x, bottom_y - 14),
        bold,
        centered,
    )
    .draw(&mut target);
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

//! Status-bar layer.
//!
//! Renders into the top `status_bar_height` rows of the panel
//! framebuffer. Layout from right to left:
//!
//!   [ … reserved for future widgets … ]   [BATT 87%]  [📶]
//!
//! Far-right: WiFi icon. Connected = three signal bars; disconnected =
//! the icon with a slash through it. To its left: a battery glyph
//! (outline + fill level) followed by a percent label.
//!
//! Center / left of the bar are deliberately empty for now — future
//! widgets (OTA-pending indicator, project name, time-of-day) will
//! land there.

use embedded_graphics::{
    geometry::{Point, Size},
    mono_font::{MonoTextStyle, ascii::FONT_6X10, ascii::FONT_8X13_BOLD},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};

use crate::{BuildInfo, MainRegion, battery_mv_to_percent};
use paperanywhere_ports::ColorMode;

/// Inputs the runtime supplies on every refresh. Wrapped in its own
/// struct rather than passed positionally so we can grow the widget
/// set (project name, OTA pending, etc.) without churning every call
/// site.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusInputs {
    /// `None` if the firmware hasn't read a battery sample yet (e.g.
    /// board without battery_adc). Renders as an outline-only icon.
    pub battery_mv: Option<u16>,
    /// RSSI of the current WiFi association, or `None` when not
    /// associated. Used as a binary connected/disconnected signal in
    /// the icon; the actual dBm value isn't shown today.
    pub wifi_rssi_dbm: Option<i16>,
}

/// Render the bar into `framebuffer` (Mono1bpp packed row-major,
/// MSB-first). Other color modes are no-ops for now — the visible
/// boards using Color7 / Gray16 will need per-mode pixel writers; file
/// a follow-up when those boards bring up.
pub fn render(
    status: &StatusInputs,
    framebuffer: &mut [u8],
    width_px: u32,
    status_bar_height: u32,
    color_mode: ColorMode,
) {
    if !matches!(color_mode, ColorMode::Mono1bpp) {
        // TODO(compositor): per-color-mode rasterisation for Gray4 /
        // Gray16 / Color7 panels. Status bar simply doesn't render
        // on those boards yet.
        return;
    }

    let mut target = Mono1bppTarget::new(framebuffer, width_px, status_bar_height);

    // Clear status-bar region to white. (Renderer convention: 0 = white,
    // 1 = black; the UC8179 driver's `panel_data_inverted` flag flips
    // bytes on the wire for panels whose native polarity is reversed.)
    target.fill_rect(0, 0, width_px, status_bar_height, false);

    // Bottom border so the bar is visually separated from the main
    // region. One-pixel hairline directly above the seam.
    target.fill_rect(0, status_bar_height.saturating_sub(1), width_px, 1, true);

    // Right-edge widgets, laid out right-to-left so adding new ones on
    // the left doesn't shift the existing layout.
    let mut cursor_x = width_px as i32 - 4; // 4px right margin

    cursor_x = draw_wifi_icon(&mut target, cursor_x, status.wifi_rssi_dbm.is_some());
    cursor_x -= 6; // gap between widgets

    cursor_x = draw_battery(&mut target, cursor_x, status.battery_mv);

    let _ = cursor_x;
}

/// Battery icon: outline rectangle + fill bar + percent label. Returns
/// the leftmost x consumed so the caller can chain more widgets to its
/// left.
fn draw_battery(target: &mut Mono1bppTarget<'_>, right_edge: i32, mv: Option<u16>) -> i32 {
    const W: i32 = 22; // body width
    const H: i32 = 12; // body height
    const NUB_W: i32 = 2; // positive-terminal nub
    const NUB_H: i32 = 6;

    let top = ((target.status_bar_height as i32) - H) / 2;
    let nub_x = right_edge - NUB_W;
    let nub_y = top + (H - NUB_H) / 2;
    target.fill_rect_signed(nub_x, nub_y, NUB_W as u32, NUB_H as u32, true);

    let body_right = nub_x;
    let body_left = body_right - W;
    // Outline.
    target.draw_rect_outline(body_left, top, W as u32, H as u32);

    // Fill bar — inset 2px from outline, width proportional to charge.
    if let Some(mv) = mv {
        let pct = battery_mv_to_percent(mv);
        let inner_w = (W - 4) as u32;
        let fill_w = (inner_w * pct as u32) / 100;
        if fill_w > 0 {
            target.fill_rect_signed(body_left + 2, top + 2, fill_w, (H - 4) as u32, true);
        }

        // "87%" label to the left of the icon.
        let mut buf = [0u8; 5];
        let s = u8_to_str(pct, &mut buf);
        let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let label_x = body_left - 2;
        let label_y = top + (H + 9) / 2; // baseline approx vertical center
        let _ = Text::with_text_style(
            s,
            Point::new(label_x, label_y),
            style,
            TextStyleBuilder::new()
                .alignment(Alignment::Right)
                .baseline(Baseline::Bottom)
                .build(),
        )
        .draw(target);

        body_left - 4 - (s.len() as i32 * 6)
    } else {
        body_left - 4
    }
}

/// WiFi icon: three signal bars stacked. Connected = solid bars;
/// disconnected = bars + diagonal slash overlay. Returns the leftmost
/// x consumed.
fn draw_wifi_icon(target: &mut Mono1bppTarget<'_>, right_edge: i32, connected: bool) -> i32 {
    const W: i32 = 18;
    const H: i32 = 14;
    let top = ((target.status_bar_height as i32) - H) / 2;
    let left = right_edge - W;

    // Three horizontal bars of increasing width, bottom-up.
    for (i, bar_w) in [6_i32, 12, 18].iter().enumerate() {
        let bar_h: i32 = 3;
        let y = top + H - (i as i32 + 1) * (bar_h + 1);
        let bar_x = left + (W - bar_w) / 2;
        target.fill_rect_signed(bar_x, y, *bar_w as u32, bar_h as u32, true);
    }

    if !connected {
        // Diagonal slash from bottom-left to top-right.
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 2);
        let _ = Line::new(Point::new(left, top + H), Point::new(left + W, top))
            .into_styled(style)
            .draw(target);
        // Background-coloured "knock-out" stroke on either side so the
        // slash reads as a cut through the bars rather than a fourth
        // diagonal bar.
        let knockout = PrimitiveStyle::with_stroke(BinaryColor::Off, 1);
        let _ = Line::new(Point::new(left - 1, top + H), Point::new(left + W - 1, top))
            .into_styled(knockout)
            .draw(target);
        let _ = Line::new(Point::new(left + 1, top + H), Point::new(left + W + 1, top))
            .into_styled(knockout)
            .draw(target);
    }

    left
}

/// Bake the firmware version + build-time text into the bottom-center
/// of the main region. Called from `boot.rs` after the boot-screen logo
/// has been blitted, so the text lands underneath the rasterised
/// "PaperAnywhere.io" wordmark.
///
/// Only handles Mono1bpp today; same TODO as the status bar for the
/// other color modes.
pub fn draw_build_info(region: &mut MainRegion<'_>, info: &BuildInfo) {
    if !matches!(region.color_mode, ColorMode::Mono1bpp) {
        return;
    }
    let mut target = Mono1bppTarget::new(region.bytes, region.width_px, region.height_px);

    // Use a small font (6x10) so two lines + padding fit within the
    // bottom ~40 px of the main region without crowding the logo.
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    let center_x = (region.width_px as i32) / 2;
    let bottom_y = (region.height_px as i32) - 8; // 8 px bottom margin

    let _ = Text::with_text_style(
        info.build_time,
        Point::new(center_x, bottom_y),
        style,
        centered,
    )
    .draw(&mut target);

    // Version above the build time, ~12 px gap.
    let bold = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let _ = Text::with_text_style(
        info.fw_version,
        Point::new(center_x, bottom_y - 14),
        bold,
        centered,
    )
    .draw(&mut target);
}

// ── Mono1bpp draw target ─────────────────────────────────────────────────────
//
// Tiny adapter that lets embedded-graphics paint into our row-major,
// MSB-first Mono1bpp framebuffer. Pixel value `On` ⇒ bit = 1 (black);
// `Off` ⇒ bit = 0 (white). The UC8179 driver's data-inversion flag
// flips this if the panel hardware wants the opposite native polarity.

struct Mono1bppTarget<'a> {
    bytes: &'a mut [u8],
    width_px: u32,
    /// Height of the region this target covers — used by status-bar
    /// drawing for vertical-centering and clipping. For the main-
    /// region overlay path, this is the main-region height.
    status_bar_height: u32,
}

impl<'a> Mono1bppTarget<'a> {
    fn new(bytes: &'a mut [u8], width_px: u32, height_px: u32) -> Self {
        Self { bytes, width_px, status_bar_height: height_px }
    }

    fn set_pixel(&mut self, x: i32, y: i32, on: bool) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);
        if x >= self.width_px || y >= self.status_bar_height {
            return;
        }
        let stride = (self.width_px + 7) / 8;
        let byte_idx = (y * stride + x / 8) as usize;
        let bit = 7 - (x % 8) as u8;
        if byte_idx >= self.bytes.len() {
            return;
        }
        if on {
            self.bytes[byte_idx] |= 1 << bit;
        } else {
            self.bytes[byte_idx] &= !(1 << bit);
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
        Rectangle::new(
            Point::zero(),
            Size::new(self.width_px, self.status_bar_height),
        )
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
/// Buffer must be ≥ 4 bytes ("100%" + null guard).
fn u8_to_str<'a>(n: u8, buf: &'a mut [u8; 5]) -> &'a str {
    let mut i = buf.len();
    // '%' suffix
    i -= 1;
    buf[i] = b'%';

    // digits
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
    // SAFETY: we only wrote ASCII digits and '%'.
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

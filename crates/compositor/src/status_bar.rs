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
use paperanywhere_ports::{ColorMode, DeviceStatus};

/// Inputs the runtime supplies before each refresh. Owned by value
/// inside the compositor; setters on the compositor's hook surface
/// (`set_chrome`, `on_wifi_state_changed`, …) mutate this in place.
// Old per-renderer state struct — kept for type-doc reference but
// nothing reads it anymore. Renderers in this file snapshot
// `paperanywhere_ports::chrome` directly. Remove this once we're
// confident no external crate is importing it.
#[deprecated(note = "use paperanywhere_ports::chrome::ChromeState")]
#[allow(dead_code)]
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
    /// Seconds remaining on the boot-screen hold countdown, or `None`
    /// when no countdown is active. Runtime decrements this each
    /// second after DHCP completes and before transitioning to the
    /// next view (adoption / image), giving the user a visible cue
    /// that the splash is about to disappear. Rendered as the last
    /// line of the build-info block.
    pub boot_countdown_secs: Option<u8>,
    /// IPv4 gateway address (e.g. `10.0.1.1`) read from embassy-net's
    /// DHCP lease once associated. Surfaced in the boot-screen's
    /// Network column. `None` until DHCP completes.
    pub gateway_v4: Option<HString<24>>,
    /// Backend URL the device polls / posts to (e.g.
    /// `https://api.paperanywhere.io`). Pulled from prov / NVS on
    /// boot; surfaced in the boot-screen's Firmware column so the
    /// user can verify the device is pointed at the right env.
    /// `None` until NVS load completes.
    pub backend_url: Option<HString<64>>,
    /// 3-state WiFi link status for the boot-screen Network column.
    /// `wifi_rssi_dbm` is the binary "have signal" flag the status-bar
    /// widgets read; this is the richer state the boot screen needs.
    pub wifi_link_state: paperanywhere_ports::WifiLinkState,
    /// SSID we're attempting to associate with / are associated to.
    /// `None` when no WiFi creds in NVS yet (factory state).
    pub ssid: Option<HString<32>>,
    /// Short device id rendered on the left side of the bar
    /// (e.g. `D-3F2A`). `None` shows a placeholder.
    pub device_id: Option<HString<24>>,
    /// Backend-assigned UUID (36 chars). Surfaced on the boot screen
    /// as a full-width line below the column block. `None` until the
    /// device's first `POST /api/device/register` lands.
    pub device_uuid: Option<HString<48>>,
    /// User-supplied friendly name. Pre-adoption this is the backend's
    /// auto-generated `device-XXXX`; post-adoption it's whatever the
    /// user typed. Shown on the boot screen's DEVICE column.
    pub device_name: Option<HString<64>>,
    /// Local-time stamp of the most recent successful render
    /// (e.g. `10:23`). `None` shows `--`.
    pub last_update_local: Option<HString<24>>,
    /// IPv4 dotted-quad string (e.g. `10.0.1.42`). Shown on the
    /// boot-screen overlay + left side of the status bar so the
    /// developer can identify the device without ARP-scanning.
    pub ip_address: Option<HString<24>>,
    /// High-level lifecycle state shown in the bar's top-left
    /// `Status: …` block. Defaults to `Booting`.
    pub device_status: DeviceStatus,
}

/// Render the bar into `framebuffer` (Mono1bpp packed row-major,
/// MSB-first). Other color modes are no-ops for now — `#71` adds
/// rasterisers for Gray4 / Gray16 / Color7.
///
/// Reads chrome state via [`paperanywhere_ports::chrome::snapshot`]
/// rather than taking it as a parameter — every renderer in this crate
/// reads from the same global KV, so passing the state through every
/// call site would just be redundant plumbing. Callers don't need to
/// know which fields each renderer touches.
pub fn render(
    framebuffer: &mut [u8],
    width_px: u32,
    status_bar_height: u32,
    color_mode: ColorMode,
) {
    if !matches!(color_mode, ColorMode::Mono1bpp) {
        return;
    }
    let status = paperanywhere_ports::chrome::snapshot();

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

    let cell_width = draw_wifi_cell(&mut target, right, status.rssi_dbm.is_some());
    let next_right = right - cell_width;
    draw_vertical_divider(&mut target, next_right, status_bar_height);
    right = next_right - 1;

    let cell_width = draw_battery_cell(
        &mut target,
        right,
        status.battery_mv,
        status.battery_percent,
    );
    let next_right = right - cell_width;
    draw_vertical_divider(&mut target, next_right, status_bar_height);
    right = next_right - 1;

    if matches!(status.usb_connected, Some(true)) {
        let cell_width = draw_usb_cell(&mut target, right);
        let next_right = right - cell_width;
        draw_vertical_divider(&mut target, next_right, status_bar_height);
        right = next_right - 1;
    }

    // Left-side cells, laid out left-to-right starting just inside the
    // border. Same fixed-width pattern as the right side: each cell
    // reserves enough room for its widest content so its left/right
    // edges never shift, followed by a 1 px vertical divider on the
    // cell's RIGHT edge. We bail out as soon as the next cell would
    // collide with the right-side cells' `right` boundary — partial
    // cells would either ghost on the fast LUT or get clipped mid-
    // glyph, neither of which is worth the extra logic to recover.
    let mut left = inset;
    let cell_width = draw_status_cell(&mut target, left, status.device_status);
    if left + cell_width < right {
        left += cell_width;
        draw_vertical_divider(&mut target, left, status_bar_height);
        left += 1;
        let cell_width = draw_last_update_cell(&mut target, left, status.last_update_local.as_deref());
        if left + cell_width < right {
            left += cell_width;
            draw_vertical_divider(&mut target, left, status_bar_height);
        }
    }
    let _ = left; // suppress unused-assignment warning when no more cells follow
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
    // Fixed-width network-status label, LEFT-aligned inside its cell.
    // Two states:
    //   - "NETWORK: Connected"    (18 chars) when associated
    //   - "NETWORK: Disconnected" (21 chars) when not
    //
    // Cell width is reserved for the longer string so the cell's
    // left edge never shifts when the state transitions. Per the
    // style guide (see project_compositor_full_lut_rule memory),
    // text inside the cell is left-aligned starting at a stable x;
    // the shorter "Connected" label simply leaves blank pixels on
    // the right — the fast LUT walks those ink → no-ink transitions
    // cleanly without ghosting the previous render's tail.
    const LEFT_PAD: i32 = 4;  // gap between cell-left divider and text
    const RIGHT_PAD: i32 = 4; // gap between text-end zone and cell-right divider
    const CHAR_PX: i32 = 6;   // FONT_6X10 advance width
    const MAX_CHARS: i32 = 21; // "NETWORK: Disconnected"
    let text_width = MAX_CHARS * CHAR_PX;
    let cell_width = LEFT_PAD + text_width + RIGHT_PAD;

    let label: &str = if connected {
        "NETWORK: Connected"
    } else {
        "NETWORK: Disconnected"
    };
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let left_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Middle)
        .build();
    let baseline_y = (target.height as i32) / 2;
    // Cell occupies (right_edge - cell_width .. right_edge). Text
    // starts LEFT_PAD pixels into the cell from its left edge.
    let cell_left = right_edge - cell_width;
    let text_x = cell_left + LEFT_PAD;
    let _ = Text::with_text_style(
        label,
        Point::new(text_x, baseline_y),
        style,
        left_aligned,
    )
    .draw(target);

    cell_width
}

/// Variant of [`blit_mono_icon`] that anchors on the icon's INK
/// bounding box instead of the bitmap's geometric extent. Useful for
/// Font Awesome glyphs whose drawn path doesn't fill the SVG viewBox
/// evenly — left-biased ink would otherwise read as "hanging left"
/// inside the cell.
fn blit_mono_icon_centered(
    target: &mut Mono1bppTarget<'_>,
    bitmap: &[u8],
    src_w: u32,
    src_h: u32,
    center_x: i32,
    center_y: i32,
) {
    let stride = ((src_w + 7) / 8) as usize;
    let mut min_x = src_w as i32;
    let mut min_y = src_h as i32;
    let mut max_x = -1i32;
    let mut max_y = -1i32;
    for sy in 0..src_h {
        for sx in 0..src_w {
            let byte_idx = sy as usize * stride + (sx / 8) as usize;
            if byte_idx >= bitmap.len() {
                continue;
            }
            let bit_mask = 1u8 << (7 - (sx % 8));
            if bitmap[byte_idx] & bit_mask == 0 {
                let x = sx as i32;
                let y = sy as i32;
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    if max_x < 0 {
        // No ink — nothing to draw.
        return;
    }
    let bbox_cx = (min_x + max_x) / 2;
    let bbox_cy = (min_y + max_y) / 2;
    // Shift the source so bbox center lands on (center_x, center_y).
    let dst_x = center_x - bbox_cx;
    let dst_y = center_y - bbox_cy;
    blit_mono_icon(target, bitmap, src_w, src_h, dst_x, dst_y);
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

fn draw_battery_cell(
    target: &mut Mono1bppTarget<'_>,
    right_edge: i32,
    mv: Option<u16>,
    percent: Option<u8>,
) -> i32 {
    // Fixed-width battery-percent label, LEFT-aligned inside its cell.
    // Format: "BATTERY: NNN%" where NNN is always 3 chars wide (right-
    // padded with spaces, or "--" when no sample). Layout never shifts
    // when the percentage changes — only the digit pixels transition
    // (e.g. "  87%" → "  86%"), which the fast LUT walks cleanly. See
    // project_compositor_full_lut_rule memory.
    const LEFT_PAD: i32 = 4;
    const RIGHT_PAD: i32 = 4;
    const CHAR_PX: i32 = 6;
    const MAX_CHARS: i32 = 13; // "BATTERY: 100%"
    let text_width = MAX_CHARS * CHAR_PX;
    let cell_width = LEFT_PAD + text_width + RIGHT_PAD;

    // Build the fixed-width text. The 3-char percent field is right-
    // aligned within itself so 100% is "100", 87% is " 87", 5% is "  5",
    // and "no reading" is " --". The string length is 13 chars in every
    // case — guarantees the trailing "%" lands at the same pixel column
    // regardless of magnitude.
    let mut text: HString<16> = HString::new();
    let _ = text.push_str("BATTERY: ");
    // Prefer the gauge's own percent reading (e.g. a fuel-gauge IC's
    // SoC). Fall back to deriving from mv via the shared LiPo curve
    // for ADC-only boards. Falls all the way to " --" when neither
    // is available (no sample this wake, board has no battery, etc.).
    let pct_opt = percent.or_else(|| mv.map(battery_mv_to_percent));
    match pct_opt {
        Some(pct) => {
            // Right-align pct into a 3-char field with leading spaces.
            let mut buf = [0u8; 5];
            let pct_str = u8_to_str(pct, &mut buf);
            for _ in 0..(3 - pct_str.len() as i32).max(0) {
                let _ = text.push(' ');
            }
            let _ = text.push_str(pct_str);
        }
        None => {
            // " --" — 3 chars matching the digit field.
            let _ = text.push_str(" --");
        }
    }
    let _ = text.push('%');

    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let left_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Middle)
        .build();
    let baseline_y = (target.height as i32) / 2;
    let cell_left = right_edge - cell_width;
    let text_x = cell_left + LEFT_PAD;
    let _ = Text::with_text_style(
        text.as_str(),
        Point::new(text_x, baseline_y),
        style,
        left_aligned,
    )
    .draw(target);

    cell_width
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

// ── Left-side cells ─────────────────────────────────────────────────────────

fn draw_status_cell(
    target: &mut Mono1bppTarget<'_>,
    left_edge: i32,
    status: DeviceStatus,
) -> i32 {
    // Fixed-width device-status cell. Cell capacity matches the longest
    // possible value ("STATUS: downloading configuration", 33 chars)
    // so the cell's right edge never shifts as the device transitions
    // between lifecycle states — the fast LUT walks shorter labels'
    // trailing whitespace cleanly. Pattern mirrors the right-side
    // cells (LEFT_PAD + text + RIGHT_PAD, left-aligned within the cell).
    const LEFT_PAD: i32 = 4;
    const RIGHT_PAD: i32 = 4;
    const CHAR_PX: i32 = 6;
    const MAX_CHARS: i32 = 33; // "STATUS: downloading configuration"
    let cell_width = LEFT_PAD + MAX_CHARS * CHAR_PX + RIGHT_PAD;

    let mut text: HString<48> = HString::new();
    let _ = text.push_str("STATUS: ");
    let _ = text.push_str(status.label());

    draw_cell_text(target, left_edge + LEFT_PAD, text.as_str());
    cell_width
}

fn draw_last_update_cell(
    target: &mut Mono1bppTarget<'_>,
    left_edge: i32,
    last_update: Option<&str>,
) -> i32 {
    // "LAST UPDATE: HH:MM" — 18 chars. The clock string is always
    // 5 chars (HH:MM) or 2 chars ("--") padded to keep the value's
    // trailing edge inside the cell on the same pixel every refresh.
    const LEFT_PAD: i32 = 4;
    const RIGHT_PAD: i32 = 4;
    const CHAR_PX: i32 = 6;
    const MAX_CHARS: i32 = 18; // "LAST UPDATE: HH:MM"
    let cell_width = LEFT_PAD + MAX_CHARS * CHAR_PX + RIGHT_PAD;

    let mut text: HString<24> = HString::new();
    let _ = text.push_str("LAST UPDATE: ");
    match last_update {
        Some(t) => {
            let _ = text.push_str(t);
        }
        None => {
            let _ = text.push_str("--");
        }
    }

    draw_cell_text(target, left_edge + LEFT_PAD, text.as_str());
    cell_width
}

/// Shared text-rendering helper for the bar's cells. Uses the same
/// font + baseline as the right-side widgets so the bar reads as one
/// row of consistent cells.
fn draw_cell_text(target: &mut Mono1bppTarget<'_>, text_x: i32, text: &str) {
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let left_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Middle)
        .build();
    let baseline_y = (target.height as i32) / 2;
    let _ = Text::with_text_style(text, Point::new(text_x, baseline_y), style, left_aligned)
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
) {
    if !matches!(region.color_mode, ColorMode::Mono1bpp) {
        return;
    }
    // Snapshot the global state once at the top — drawing functions
    // below read from `state.*`. Cheap clone (one critical section,
    // small struct); we don't want to re-lock for every field access.
    let state = paperanywhere_ports::chrome::snapshot();
    let device_uuid: &str = state.device_uuid.as_deref().unwrap_or("");
    let device_name: Option<&str> = state.device_name.as_deref();
    let ip: Option<&str> = state.ip.as_deref();
    let wifi_link_state = state.wifi_link_state;
    let ssid: Option<&str> = state.ssid.as_deref();
    let gateway: Option<&str> = state.gateway_v4.as_deref();
    let backend: Option<&str> = state.backend_url.as_deref();
    let countdown_secs = state.boot_countdown_secs;

    let mut target = Mono1bppTarget::new(region.bytes, region.width_px, region.height_px);

    let body_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let header_style = MonoTextStyle::new(&FONT_8X13_BOLD, BinaryColor::On);
    let key_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Right)
        .baseline(Baseline::Bottom)
        .build();
    let value_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Left)
        .baseline(Baseline::Bottom)
        .build();
    let center_aligned = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Bottom)
        .build();

    // 3-column layout at the bottom of the main region:
    //
    //   FIRMWARE              NETWORK              DEVICE
    //   Version:  ...         WiFi:    ...         Model: ...
    //   Branch:   ...         IP:      ...         UUID:  ...
    //   Env:      ...         Gateway: ...
    //   Backend:  ...
    //
    //                Transitioning in N seconds…   (when active)
    //
    // The columns share a single 5-row baseline grid (row 0 = header,
    // rows 1-4 = entries). Within each column, keys are right-aligned
    // to a per-column colon_x and values are left-aligned to value_x.
    // Both columns NEVER shift between redraws — fast LUT handles
    // in-place value changes cleanly per the style guide (see
    // project_compositor_full_lut_rule memory).
    let width = region.width_px as i32;
    let bottom_y = (region.height_px as i32) - 8;
    const ROW_PX: i32 = 12;
    const ROWS_IN_BLOCK: i32 = 5; // header + 4 entry rows
    // Row 0 = header (topmost), Row 4 = last entry row, then a one-row
    // gap, then countdown at bottom_y.
    let row_y = |row: i32| -> i32 {
        // Stack upward: bottom_y - (4 - row + 1) * 12 = bottom_y - (5 - row) * 12
        bottom_y - (ROWS_IN_BLOCK - row + 1) * ROW_PX
    };

    // Per-column anchor: colon_x is where the key text ends (right-
    // aligned to it); value_x = colon_x + 4 is where the value starts.
    // The block is centred horizontally on each panel-third's midpoint
    // but inset so the widest value fits without spilling between
    // columns or off the panel edge.
    let col_centers = [width / 6, width / 2, width * 5 / 6];
    // Per-column key-width allowances (in pixels). Tuned so the
    // longest key fits without overlapping the value column:
    //   Firmware: "Backend:"  (8 chars × 6 = 48 px)
    //   Network:  "Gateway:"  (8 chars × 6 = 48 px)
    //   Device:   "Model:"    (6 chars × 6 = 36 px)
    // We bias colon_x slightly LEFT of each column's centre so the
    // value text (longer, variable) has more rightward room.
    let firmware_colon_x = col_centers[0] - 30;
    let network_colon_x = col_centers[1] - 30;
    let device_colon_x = col_centers[2] - 30;
    let firmware_value_x = firmware_colon_x + 4;
    let network_value_x = network_colon_x + 4;
    let device_value_x = device_colon_x + 4;

    // ── Headers (row 0) ─────────────────────────────────────────────
    let header_y = row_y(0);
    let _ = Text::with_text_style(
        "FIRMWARE",
        Point::new(col_centers[0], header_y),
        header_style,
        center_aligned,
    )
    .draw(&mut target);
    let _ = Text::with_text_style(
        "NETWORK",
        Point::new(col_centers[1], header_y),
        header_style,
        center_aligned,
    )
    .draw(&mut target);
    let _ = Text::with_text_style(
        "DEVICE",
        Point::new(col_centers[2], header_y),
        header_style,
        center_aligned,
    )
    .draw(&mut target);

    // Helper to draw one key+value pair in a column.
    let mut draw_kv =
        |target: &mut Mono1bppTarget<'_>, colon_x: i32, value_x: i32, y: i32, key: &str, value: &str| {
            let mut key_buf: HString<24> = HString::new();
            let _ = key_buf.push_str(key);
            let _ = key_buf.push(':');
            let _ = Text::with_text_style(
                key_buf.as_str(),
                Point::new(colon_x, y),
                body_style,
                key_aligned,
            )
            .draw(target);
            let _ = Text::with_text_style(
                value,
                Point::new(value_x, y),
                body_style,
                value_aligned,
            )
            .draw(target);
        };

    // ── FIRMWARE column (rows 1-4) ──────────────────────────────────
    let env = if info.is_dev { "dev" } else { "production" };
    let backend_short = backend.unwrap_or("--");
    draw_kv(&mut target, firmware_colon_x, firmware_value_x, row_y(1), "Version", info.fw_version);
    draw_kv(&mut target, firmware_colon_x, firmware_value_x, row_y(2), "Branch", info.branch);
    draw_kv(&mut target, firmware_colon_x, firmware_value_x, row_y(3), "Env", env);
    draw_kv(&mut target, firmware_colon_x, firmware_value_x, row_y(4), "Backend", backend_short);

    // ── NETWORK column (rows 1-4) ───────────────────────────────────
    // WiFi shows the 3-state link label (Disconnected/Connecting/
    // Connected) — distinct from the IP field, which is now strictly
    // the assigned address (or "--" when no DHCP lease). Pre-#90
    // behaviour conflated the two by putting "connecting..." in the
    // IP field while WPA was in flight; the new layout cleanly
    // separates WiFi link-state from IP-address signalling.
    let wifi_label = wifi_link_state.label();
    let ssid_str = ssid.unwrap_or("--");
    let ip_str = ip.unwrap_or("--");
    let gateway_str = gateway.unwrap_or("--");
    draw_kv(&mut target, network_colon_x, network_value_x, row_y(1), "WiFi", wifi_label);
    draw_kv(&mut target, network_colon_x, network_value_x, row_y(2), "SSID", ssid_str);
    draw_kv(&mut target, network_colon_x, network_value_x, row_y(3), "IP", ip_str);
    draw_kv(&mut target, network_colon_x, network_value_x, row_y(4), "Gateway", gateway_str);

    // ── DEVICE column (rows 1-4) ────────────────────────────────────
    // Maker/Model/Name use the body font like the other columns. UUID
    // gets a compact font on its own row so the full 36-char UUID4
    // fits within the column's value budget — silent truncation would
    // hide a quarter of the device's identity, and pushing it
    // somewhere outside the DEVICE block visually disconnects it from
    // its label. The compact 4x6 font is only for the UUID value,
    // not its "UUID:" key (the key stays body-style so the column
    // alignment looks consistent down the page).
    let name_str = device_name.unwrap_or("--");
    draw_kv(&mut target, device_colon_x, device_value_x, row_y(1), "Maker", info.manufacturer);
    draw_kv(&mut target, device_colon_x, device_value_x, row_y(2), "Model", info.device_model);
    draw_kv(&mut target, device_colon_x, device_value_x, row_y(3), "Name", name_str);

    // UUID row: rendered in the column's body font so it lines up
    // visually with Maker / Model / Name. The full 36-char UUID
    // doesn't fit the column budget at body width, so we show the
    // first 8 chars (git-style short id) — enough to identify the
    // device without forcing a font mismatch on a single row. The
    // small-font path is kept around for callers that need the
    // full string elsewhere.
    let uuid_y = row_y(4);
    let short_uuid: HString<10> = if device_uuid.is_empty() {
        HString::try_from("(none)").unwrap_or_default()
    } else {
        let take = device_uuid.len().min(8);
        let mut s: HString<10> = HString::new();
        let _ = s.push_str(&device_uuid[..take]);
        s
    };
    draw_kv(&mut target, device_colon_x, device_value_x, uuid_y, "UUID", short_uuid.as_str());

    // ── Countdown row (at bottom_y) ─────────────────────────────────
    // Centred and prominent. Always-reserved row so showing/hiding the
    // countdown doesn't shift any other content. Uses ASCII-only text
    // (no `…` ellipsis) because FONT_6X10 only includes the ASCII
    // glyph set — non-ASCII characters render as a placeholder that
    // can be confused with `%`/`?` shapes.
    if let Some(n) = countdown_secs {
        use core::fmt::Write;
        let mut msg: HString<48> = HString::new();
        let unit = if n == 1 { "second" } else { "seconds" };
        let _ = write!(&mut msg, "Transitioning in {} {}...", n, unit);

        let center_x = width / 2;
        let _ = Text::with_text_style(
            msg.as_str(),
            Point::new(center_x, bottom_y),
            body_style,
            center_aligned,
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

pub(crate) struct Mono1bppTarget<'a> {
    bytes: &'a mut [u8],
    width_px: u32,
    pub(crate) height: u32,
}

impl<'a> Mono1bppTarget<'a> {
    pub(crate) fn new(bytes: &'a mut [u8], width_px: u32, height_px: u32) -> Self {
        Self { bytes, width_px, height: height_px }
    }

    pub(crate) fn set_pixel(&mut self, x: i32, y: i32, on: bool) {
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

/// Stringify a u8 into the provided buffer, returning a `&str` view.
/// Pure digits — the trailing `%` is appended by the caller. Avoids
/// pulling `alloc::format!` for the per-refresh path.
fn u8_to_str<'a>(n: u8, buf: &'a mut [u8; 5]) -> &'a str {
    let mut i = buf.len();
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

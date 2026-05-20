//! Layered compositor for paperanywhere panels.
//!
//! Sits between the runtime and the underlying [`EpaperPanel`] driver. The
//! screen is split into two regions:
//!
//! - **Status bar** — fixed-height strip at the very top of the panel.
//!   Reserved for chrome the firmware owns: battery, WiFi, etc. The
//!   runtime cannot write here directly.
//! - **Main region** — everything below the status bar. The runtime's
//!   image renders, boot screens, claim-code screens etc. land here.
//!
//! The compositor exposes the same [`EpaperPanel`] trait the panel driver
//! does, so the runtime stays unaware of the layering. `write_chunk(bytes)`
//! is interpreted as "main-region content"; `refresh()` paints the status
//! bar on top of the cached framebuffer and flushes everything to the
//! underlying panel.
//!
//! ## Memory
//!
//! One panel-sized Mono1bpp framebuffer in heap: 48 KB for an 800×480
//! panel, 192 KB for a 1404×1872 mono panel. Larger color modes
//! (Color7) scale by 4× since they pack 2 pixels per byte at most.
//! On ESP32-S3 with 8 MB PSRAM this is comfortably below the budget.
//!
//! ## Status-bar overdraw vs partial refresh
//!
//! Today we re-flush the whole panel on every refresh — even a battery-
//! icon update triggers a full 5-cycle refresh. That gets fixed by
//! UC8179 fast-refresh LUTs + partial-region writes (see firmware-repo
//! task #57). The compositor's API doesn't need to change when that
//! lands; we just route the dirty rect into a different driver path.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use paperanywhere_ports::{ColorMode, EpaperPanel};

pub mod status_bar;

pub use status_bar::StatusInputs;

/// Default reserved height for the status bar in pixels. 32 rows is
/// enough for a 16 px monospace font + 4 px top/bottom padding, and
/// fits the embedded-graphics `Font10x20` battery+percentage label
/// comfortably. Divisible by 8 so Mono1bpp row packing aligns cleanly
/// with byte boundaries — important for the eventual partial-refresh
/// path that flushes whole sectors of frame RAM at a time.
pub const DEFAULT_STATUS_BAR_HEIGHT: u32 = 32;

/// Owns a backbuffer for the whole panel and lets callers draw into
/// either the status-bar region (top) or the main region (everything
/// below). Implements [`EpaperPanel`] so the runtime can swap it in
/// for the raw driver.
pub struct Compositor<P: EpaperPanel> {
    panel: P,
    width_px: u32,
    height_px: u32,
    status_bar_height: u32,
    color_mode: ColorMode,
    /// Full-panel backbuffer in the panel's native packing. Allocated
    /// once at construction; never resized.
    framebuffer: Vec<u8>,
    /// Byte cursor inside `framebuffer`'s main region, advanced by
    /// `write_chunk` calls. Reset on `refresh()` so the next image
    /// stream starts at the top of the main region.
    main_cursor: usize,
    /// Current status-bar inputs. Set by the runtime via
    /// [`Compositor::update_status`]; rendered into the top region
    /// during `refresh()`.
    status: StatusInputs,
}

impl<P: EpaperPanel> Compositor<P> {
    /// Build a compositor wrapping `panel`. `width_px` × `height_px`
    /// must match the panel's actual resolution; the framebuffer is
    /// sized accordingly. `status_bar_height` is typically
    /// [`DEFAULT_STATUS_BAR_HEIGHT`].
    pub fn new(
        panel: P,
        width_px: u32,
        height_px: u32,
        color_mode: ColorMode,
        status_bar_height: u32,
    ) -> Self {
        let fb_bytes = framebuffer_size(width_px, height_px, color_mode);
        // Initial framebuffer: all white (`0` in the renderer-friendly
        // convention; the UC8179 driver's `panel_data_inverted` flag
        // flips bytes on the way out when needed).
        let framebuffer = vec![0u8; fb_bytes];
        Self {
            panel,
            width_px,
            height_px,
            status_bar_height,
            color_mode,
            framebuffer,
            main_cursor: main_region_offset(width_px, status_bar_height, color_mode),
            status: StatusInputs::default(),
        }
    }

    /// Width of the visible main region in pixels. Backend code building
    /// images for this device should target this minus zero — the main
    /// region is full width.
    pub fn main_width_px(&self) -> u32 {
        self.width_px
    }

    /// Height of the visible main region in pixels (panel height minus
    /// status-bar height). Backend boot-screen rasteriser passes this
    /// as the target height so the centered logo doesn't get clipped.
    pub fn main_height_px(&self) -> u32 {
        self.height_px.saturating_sub(self.status_bar_height)
    }

    /// Replace the in-memory status-bar state. Doesn't paint the panel —
    /// the next [`EpaperPanel::refresh`] call rasterises and flushes.
    pub fn update_status(&mut self, status: StatusInputs) {
        self.status = status;
    }

    /// Direct access to the main-region byte range, used by code paths
    /// that want to draw with embedded-graphics on top of the streamed
    /// image (e.g. the firmware-version overlay on the boot screen).
    pub fn main_region_mut(&mut self) -> MainRegion<'_> {
        // Snap immutable reads of `self` into locals before taking the
        // mut borrow on `framebuffer` — otherwise the borrow checker
        // sees overlapping &mut + & on `self`.
        let width_px = self.width_px;
        let height_px = self.main_height_px();
        let color_mode = self.color_mode;
        let offset = main_region_offset(width_px, self.status_bar_height, color_mode);
        MainRegion {
            bytes: &mut self.framebuffer[offset..],
            width_px,
            height_px,
            color_mode,
        }
    }
}

impl<P: EpaperPanel> EpaperPanel for Compositor<P> {
    fn set_chrome(&mut self, battery_mv: Option<u16>, wifi_rssi_dbm: Option<i16>) {
        self.status = StatusInputs { battery_mv, wifi_rssi_dbm };
    }

    fn on_wifi_state_changed(&mut self, rssi_dbm: Option<i16>) {
        self.status.wifi_rssi_dbm = rssi_dbm;
        // Note: doesn't itself flush. When fast-refresh LUTs (firmware
        // repo task #57) land we can call into a "status-bar-only"
        // partial-refresh path here.
    }

    fn on_battery_sample(&mut self, mv: Option<u16>) {
        self.status.battery_mv = mv;
    }

    fn init(&mut self) {
        self.panel.init();
        // Reset the framebuffer + cursor; do NOT paint the panel here.
        // The runtime's wake-cycle flow expects to issue `write_chunk`s
        // first and then a single `refresh` that flushes.
        for byte in self.framebuffer.iter_mut() {
            *byte = 0;
        }
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
    }

    fn write_chunk(&mut self, bytes: &[u8]) {
        let cap = self.framebuffer.len();
        let end = (self.main_cursor + bytes.len()).min(cap);
        let take = end - self.main_cursor;
        self.framebuffer[self.main_cursor..end].copy_from_slice(&bytes[..take]);
        self.main_cursor = end;
        if take < bytes.len() {
            log::warn!(
                "compositor: write_chunk truncated {} bytes (main region full)",
                bytes.len() - take
            );
        }
    }

    fn refresh(&mut self) {
        // Paint the status bar into the top region of the framebuffer,
        // then stream the whole buffer to the panel in one shot. When
        // partial-refresh LUTs land, we'll split this into "top region
        // only" vs "full refresh" based on what actually changed.
        status_bar::render(
            &self.status,
            &mut self.framebuffer,
            self.width_px,
            self.status_bar_height,
            self.color_mode,
        );
        self.panel.init();
        self.panel.write_chunk(&self.framebuffer);
        self.panel.refresh();
        // Reset the main cursor so the next render starts fresh.
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
    }
}

/// Mutable handle into the main region of the framebuffer. Lets a caller
/// draw with embedded-graphics on top of whatever's already there.
pub struct MainRegion<'a> {
    pub bytes: &'a mut [u8],
    pub width_px: u32,
    pub height_px: u32,
    pub color_mode: ColorMode,
}

/// Bytes required to hold a full panel framebuffer in the given mode.
pub const fn framebuffer_size(width_px: u32, height_px: u32, mode: ColorMode) -> usize {
    let pixels = width_px as usize * height_px as usize;
    match mode {
        ColorMode::Mono1bpp | ColorMode::MonoRed1bpp | ColorMode::MonoYellow1bpp => {
            (pixels + 7) / 8
        }
        ColorMode::Gray4 => (pixels + 3) / 4,
        ColorMode::Gray16 => (pixels + 1) / 2,
        ColorMode::Color7 => pixels / 2 + pixels % 2,
    }
}

/// Byte offset where the main region starts inside the framebuffer.
/// Assumes row-major packing — true for every mode we currently
/// support; revisit if a future controller packs by column.
pub const fn main_region_offset(width_px: u32, status_bar_height: u32, mode: ColorMode) -> usize {
    framebuffer_size(width_px, status_bar_height, mode)
}

/// Format a battery voltage in millivolts as a rough percentage, 0–100,
/// using a coarse linear curve for a 1S LiPo. Good enough for a status
/// icon; not a substitute for a real fuel gauge.
///
/// Anchors: 3.3V → 0%, 4.2V → 100%. Clamped on both ends.
pub fn battery_mv_to_percent(mv: u16) -> u8 {
    const EMPTY: u32 = 3300;
    const FULL: u32 = 4200;
    let mv = mv as u32;
    if mv <= EMPTY {
        return 0;
    }
    if mv >= FULL {
        return 100;
    }
    (((mv - EMPTY) * 100) / (FULL - EMPTY)) as u8
}

/// `chrono`-free build-time stamp baked into the firmware. Surfaced on
/// the boot screen so a deployed device can be cross-checked against
/// its expected release.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    /// e.g. `0.1.0+a1b2c3d4`
    pub fw_version: &'static str,
    /// e.g. `2026-05-20 22:13 UTC`
    pub build_time: &'static str,
}

impl BuildInfo {
    /// Render the version + build time onto the bottom-center of the
    /// passed main-region framebuffer. Called by `boot.rs` after the
    /// boot-screen logo has been blitted; before the panel flush.
    pub fn render_into(&self, region: &mut MainRegion<'_>) {
        crate::status_bar::draw_build_info(region, self);
    }
}

/// Used by the firmware to build the version string into a heap-
/// allocated, owned `String`. Useful for log lines + status payloads
/// where `&'static` won't do.
pub fn fw_version_owned(s: &str) -> String {
    String::from(s)
}

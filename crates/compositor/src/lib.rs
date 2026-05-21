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

pub mod adoption_screen;
pub mod icons;
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
    /// Cached boot screen + build info. Set once at boot.rs time via
    /// [`Compositor::cache_boot_template`]; consumed by
    /// [`EpaperPanel::redraw_boot_screen`] when the runtime wants to
    /// repaint the splash after DHCP comes up (so the IP can land on
    /// the boot-screen overlay, not just the status bar).
    boot_template: Option<BootTemplate>,
    /// Number of refreshes since the last full-LUT refresh. Used to
    /// promote every Nth refresh to full (clears partial-LUT ghosting).
    refresh_count: u32,
}

#[derive(Debug, Clone)]
struct BootTemplate {
    bytes: &'static [u8],
    info: BuildInfo,
}

/// Refreshes per full-LUT cycle. Partial refreshes are fast (~750 ms
/// on UC8179) but ghost slightly; one full refresh every
/// `FULL_REFRESH_EVERY` partials clears the residual back to a clean
/// surface. 8 is a conservative ratio — Waveshare's reference driver
/// recommends 5-ish for visually demanding content.
const FULL_REFRESH_EVERY: u32 = 8;

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
        // Initial framebuffer: all white. The boot-screen rasterizer's
        // convention is "bit set = white = ink off" (see the boot-screen
        // crate's docs), so 0xFF for every byte means "no ink anywhere"
        // — the panel renders an empty white surface. Zeroing the
        // buffer (which was the previous default) produced a fully-
        // inked black panel, which is the opposite of what we want.
        let framebuffer = vec![0xFFu8; fb_bytes];
        Self {
            panel,
            width_px,
            height_px,
            status_bar_height,
            color_mode,
            framebuffer,
            main_cursor: main_region_offset(width_px, status_bar_height, color_mode),
            status: StatusInputs::default(),
            boot_template: None,
            refresh_count: 0,
        }
    }

    /// Cache the boot screen bytes + build info so the runtime can ask
    /// for a repaint later (e.g. when DHCP completes and we want to
    /// add the IP line under the version stamp). One-shot from
    /// `boot::run`; the compositor holds &'static refs so there's no
    /// allocation here.
    pub fn cache_boot_template(&mut self, bytes: &'static [u8], info: BuildInfo) {
        self.boot_template = Some(BootTemplate { bytes, info });
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
        // Preserve everything else (usb, device id, last update) — this
        // call only refreshes the two right-side widgets.
        self.status.battery_mv = battery_mv;
        self.status.wifi_rssi_dbm = wifi_rssi_dbm;
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

    fn on_usb_state_changed(&mut self, connected: Option<bool>) {
        self.status.usb_connected = connected;
    }

    fn set_device_id(&mut self, id: &str) {
        let mut s: heapless::String<24> = heapless::String::new();
        let _ = s.push_str(id);
        self.status.device_id = Some(s);
    }

    fn set_last_update(&mut self, local_time: &str) {
        let mut s: heapless::String<24> = heapless::String::new();
        let _ = s.push_str(local_time);
        self.status.last_update_local = Some(s);
    }

    fn set_ip(&mut self, ip: &str) {
        let mut s: heapless::String<24> = heapless::String::new();
        let _ = s.push_str(ip);
        self.status.ip_address = Some(s);
    }

    fn set_status(&mut self, status: paperanywhere_ports::DeviceStatus) {
        self.status.device_status = status;
    }

    fn render_adoption_screen(
        &mut self,
        claim_code: &str,
        device_id: &str,
        ip: &str,
        adopt_url: &str,
    ) {
        // Reset the framebuffer to all-white before painting the
        // adoption screen. The status-bar region gets re-rendered in
        // the usual compose() / refresh() flow afterward.
        for b in self.framebuffer.iter_mut() {
            *b = 0xFF;
        }
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);

        let width_px = self.width_px;
        let height_px = self.main_height_px();
        let color_mode = self.color_mode;
        let offset = main_region_offset(width_px, self.status_bar_height, color_mode);
        let mut region = MainRegion {
            bytes: &mut self.framebuffer[offset..],
            width_px,
            height_px,
            color_mode,
        };
        crate::adoption_screen::draw_adoption_screen(
            &mut region,
            claim_code,
            device_id,
            ip,
            adopt_url,
        );
    }

    fn redraw_boot_screen(&mut self) {
        let Some(tpl) = self.boot_template.clone() else {
            return;
        };
        // Reset framebuffer to white, then blit the cached boot screen
        // into the main region.
        for b in self.framebuffer.iter_mut() {
            *b = 0xFF;
        }
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
        let end = (self.main_cursor + tpl.bytes.len()).min(self.framebuffer.len());
        let take = end - self.main_cursor;
        self.framebuffer[self.main_cursor..end].copy_from_slice(&tpl.bytes[..take]);
        self.main_cursor = end;

        // Paint the build-info overlay onto the main region. The
        // values need to outlive the mutable borrow on `framebuffer`,
        // so snapshot them by clone first. IP state defaults to
        // "connecting..." when nothing has been pushed yet — runtime
        // overrides via `set_ip` once DHCP completes (or fails).
        let ip_copy: heapless::String<24> = self
            .status
            .ip_address
            .clone()
            .unwrap_or_else(|| {
                let mut s: heapless::String<24> = heapless::String::new();
                let _ = s.push_str("connecting...");
                s
            });
        let device_id_copy: heapless::String<24> = self
            .status
            .device_id
            .clone()
            .unwrap_or_else(|| {
                let mut s: heapless::String<24> = heapless::String::new();
                let _ = s.push_str("unassigned");
                s
            });
        let width_px = self.width_px;
        let height_px = self.main_height_px();
        let color_mode = self.color_mode;
        let offset = main_region_offset(width_px, self.status_bar_height, color_mode);
        let mut region = MainRegion {
            bytes: &mut self.framebuffer[offset..],
            width_px,
            height_px,
            color_mode,
        };
        crate::status_bar::draw_build_info(
            &mut region,
            &tpl.info,
            device_id_copy.as_str(),
            ip_copy.as_str(),
        );
    }

    fn init(&mut self) {
        self.panel.init();
        // Reset the framebuffer to "all white" (0xFF in the rasterizer's
        // bit-set = white convention), not 0x00 — see `Compositor::new`
        // for the polarity rationale.
        for byte in self.framebuffer.iter_mut() {
            *byte = 0xFF;
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

    fn compose(&mut self) {
        // Paint the status bar into the top region of the framebuffer.
        // After this returns, the framebuffer holds exactly the bytes
        // that would be sent to the panel — perfect for hashing.
        status_bar::render(
            &self.status,
            &mut self.framebuffer,
            self.width_px,
            self.status_bar_height,
            self.color_mode,
        );
    }

    fn pending_hash(&self) -> Option<u64> {
        // Hash the entire framebuffer post-compose. The runtime
        // compares this against the last persisted hash to decide
        // whether to skip the (expensive) refresh.
        Some(paperanywhere_ports::hash_bytes(&self.framebuffer))
    }

    fn refresh(&mut self) {
        // Stream the staged framebuffer to the panel. The runtime is
        // expected to have called `compose()` (and consulted
        // `pending_hash()`) immediately before this; calling refresh
        // without compose is fine too, the framebuffer just won't have
        // the latest status bar painted.
        //
        // Promote every Nth refresh to a full-LUT cycle to clear
        // partial-LUT ghosting; the rest run the partial path. On
        // panels without partial-LUT support `refresh_fast` is a
        // default that falls through to `refresh`, so the per-N
        // alternation is a no-op there.
        self.panel.init();
        self.panel.write_chunk(&self.framebuffer);
        self.refresh_count = self.refresh_count.wrapping_add(1);
        if self.refresh_count.is_multiple_of(FULL_REFRESH_EVERY) {
            self.panel.refresh();
        } else {
            self.panel.refresh_fast();
        }
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
    }

    fn refresh_fast(&mut self) {
        // Caller explicitly asked for the fast path — honour it. Don't
        // touch refresh_count here; full-refresh cadence is driven by
        // the unconditional refresh() entry above.
        self.panel.init();
        self.panel.write_chunk(&self.framebuffer);
        self.panel.refresh_fast();
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
    /// `true` when the device was flashed via `provtool --dev`. The
    /// boot screen tags the version line with " (DEV)" so it's
    /// visually obvious the firmware is on the dev channel.
    pub is_dev: bool,
}

impl BuildInfo {
    /// Render the boot-screen `Key: Value` overlay (Build /
    /// Environment / Build Date / IP / Device UUID) onto the bottom-
    /// center of the passed main-region framebuffer. Called by
    /// `boot.rs` after the boot-screen logo has been blitted; before
    /// the panel flush. `ip` is a state string (e.g. `"connecting..."`,
    /// `"10.0.1.42"`); `device_uuid` defaults to MAC-suffix today and
    /// will be backend-issued once the register endpoint lands.
    pub fn render_into(&self, region: &mut MainRegion<'_>, device_uuid: &str, ip: &str) {
        crate::status_bar::draw_build_info(region, self, device_uuid, ip);
    }
}

/// Used by the firmware to build the version string into a heap-
/// allocated, owned `String`. Useful for log lines + status payloads
/// where `&'static` won't do.
pub fn fw_version_owned(s: &str) -> String {
    String::from(s)
}

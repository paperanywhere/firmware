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
//! ## Full-LUT vs fast-LUT refresh policy
//!
//! `force_full_next_refresh = true` must ONLY be set when the panel's
//! view *context* changes — boot → adoption, adoption → image, *
//! → halt, Idle → OTA-progress, etc. Updates *within* a view (e.g.
//! refreshing the boot-screen IP overlay once DHCP completes, ticking
//! the OTA progress bar mid-upload, updating a status-bar widget) use
//! the default fast LUT. Periodic `FULL_REFRESH_EVERY` (= 8) cycles
//! in [`Compositor::refresh`] clear accumulated ghosting from the
//! fast-LUT runs.
//!
//! Rationale: full LUT is ~3 s and visibly flashes the panel; fast
//! LUT is ~750 ms and incremental. Using full for every render is
//! wasteful AND visually ugly. The full-LUT-only-on-view-change
//! discipline keeps the user experience clean while letting the fast
//! path service the high-frequency in-view updates cheaply.
//!
//! Per-renderer policy is documented in memory (see
//! `project_compositor_full_lut_rule`). If you add a new view, follow
//! the same pattern: force full inside the `render_*` method, set the
//! state field that mirrors `last_ota_phase` for the OtaProgress
//! within-view case if applicable.
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
use paperanywhere_ports::{ColorMode, EpaperPanel, OtaPhase};

pub mod adoption_screen;
pub mod main_placeholder;
pub mod ota_progress;
pub mod halt_screen;
pub mod icons;
pub mod playlist_page;
pub mod status_bar;

// StatusInputs is gone — state lives in paperanywhere_ports::chrome.

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
    // Status-bar state has moved to the global
    // `paperanywhere_ports::chrome` KV. Renderers snapshot it during
    // refresh; setter trait methods on this struct forward to the
    // global. No per-field plumbing through the compositor anymore.
    /// Cached boot screen + build info. Set once at boot.rs time via
    /// [`Compositor::cache_boot_template`]; consumed by
    /// [`EpaperPanel::redraw_boot_screen`] when the runtime wants to
    /// repaint the splash after DHCP comes up (so the IP can land on
    /// the boot-screen overlay, not just the status bar).
    boot_template: Option<BootTemplate>,
    /// Number of refreshes since the last full-LUT refresh. Used to
    /// promote every Nth refresh to full (clears partial-LUT ghosting).
    refresh_count: u32,
    /// Set when the framebuffer has been repainted with a whole new
    /// "view" (boot screen, adoption screen, halt screen) — the next
    /// refresh must use the full LUT to clear ghosting from whatever
    /// the panel was previously showing. Cleared by [`Self::refresh`].
    force_full_next_refresh: bool,
    /// Tracks the most recent OTA phase so we only force a full
    /// refresh on the transition INTO the OTA view, not on every
    /// progress-bar tick. Without this, the compositor would burn a
    /// 3 s full-LUT refresh on each progress update (~10× per push
    /// at 64 KB granularity), making the view feel choppy and the
    /// total push wall-clock balloon.
    last_ota_phase: OtaPhase,
}

#[derive(Debug, Clone)]
struct BootTemplate {
    bytes: &'static [u8],
    info: BuildInfo,
}

/// Refreshes per full-LUT cycle. Partial refreshes are fast (~750 ms
/// on UC8179) but ghost slightly; one full refresh every
/// `FULL_REFRESH_EVERY` partials clears the residual back to a clean
/// surface.
///
/// Was 8 originally — Waveshare's reference recommends 5-ish for
/// visually demanding content. Bumped to 32 because at 8 the scheduled
/// full refresh consistently landed mid-boot-countdown (refresh #8 =
/// countdown tick "6" on a 10-second hold), producing a ~3 s stall
/// that backed up the paint channel and made the remaining ticks
/// appear to "fly through" once the actor caught up. 32 places the
/// next forced full pass well past any sequence we issue back-to-back
/// during boot — the user-perceptible ghosting accumulation over
/// 32 fast refreshes is acceptable for the boot path, which clears
/// to a fresh adoption / image view shortly after anyway (and those
/// views force-full on entry per the view-transition rule, also
/// clearing accumulated ghost).
const FULL_REFRESH_EVERY: u32 = 32;

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
            boot_template: None,
            refresh_count: 0,
            // First refresh after construction is always full — the
            // panel was either freshly powered on (random ghost from
            // factory) or just left over from the previous boot in an
            // unknown state. A fast refresh on top of that would smear
            // the previous content into the boot screen.
            force_full_next_refresh: true,
            last_ota_phase: OtaPhase::Idle,
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
    // All chrome-state setters forward to the global KV in
    // `paperanywhere_ports::chrome`. The compositor no longer owns a
    // copy of the state — renderers snapshot the global directly at
    // refresh time. Producers can either keep calling these trait
    // methods (legacy path) or call `chrome::set_*(...)` directly to
    // skip the panel-actor channel entirely.
    fn set_chrome(&mut self, battery_mv: Option<u16>, wifi_rssi_dbm: Option<i16>) {
        paperanywhere_ports::chrome::set_battery_mv(battery_mv);
        paperanywhere_ports::chrome::set_rssi_dbm(wifi_rssi_dbm);
    }
    fn on_wifi_state_changed(&mut self, rssi_dbm: Option<i16>) {
        paperanywhere_ports::chrome::set_rssi_dbm(rssi_dbm);
    }
    fn on_battery_sample(&mut self, mv: Option<u16>) {
        paperanywhere_ports::chrome::set_battery_mv(mv);
    }
    fn on_usb_state_changed(&mut self, connected: Option<bool>) {
        paperanywhere_ports::chrome::set_usb_connected(connected);
    }
    fn set_device_id(&mut self, id: &str) {
        paperanywhere_ports::chrome::set_device_id(Some(id));
    }
    fn set_last_update(&mut self, local_time: &str) {
        paperanywhere_ports::chrome::set_last_update(Some(local_time));
    }
    fn set_ip(&mut self, ip: &str) {
        paperanywhere_ports::chrome::set_ip(Some(ip));
    }
    fn set_status(&mut self, status: paperanywhere_ports::DeviceStatus) {
        paperanywhere_ports::chrome::set_device_status(status);
    }
    fn set_boot_countdown(&mut self, seconds: Option<u8>) {
        paperanywhere_ports::chrome::set_boot_countdown_secs(seconds);
    }
    fn set_gateway(&mut self, ip: Option<&str>) {
        paperanywhere_ports::chrome::set_gateway(ip);
    }
    fn set_backend_url(&mut self, url: Option<&str>) {
        paperanywhere_ports::chrome::set_backend_url(url);
    }
    fn set_wifi_link_state(&mut self, state: paperanywhere_ports::WifiLinkState) {
        paperanywhere_ports::chrome::set_wifi_link_state(state);
    }
    fn set_ssid(&mut self, ssid: Option<&str>) {
        paperanywhere_ports::chrome::set_ssid(ssid);
    }
    fn set_device_uuid(&mut self, uuid: Option<&str>) {
        paperanywhere_ports::chrome::set_device_uuid(uuid);
    }
    fn set_device_name(&mut self, name: Option<&str>) {
        paperanywhere_ports::chrome::set_device_name(name);
    }

    fn force_full_next_refresh(&mut self) {
        self.force_full_next_refresh = true;
    }

    fn render_halt_screen(&mut self, headline: &str, detail: &str, code: &str) {
        // View transition — clear ghosting with a full refresh.
        self.force_full_next_refresh = true;
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
        crate::halt_screen::draw_halt_screen(&mut region, headline, detail, code);
    }

    fn render_adoption_screen(
        &mut self,
        claim_code: &str,
        device_id: &str,
        ip: &str,
        adopt_url: &str,
        retry_notice: Option<&str>,
    ) {
        // NOTE: empirically the full-LUT here (~3 s of wait_idle_async
        // polling) still pushes the AP past its STA inactivity threshold
        // on some networks even with the task #90 yield_now in
        // write_chunk — the cumulative refresh time across boot →
        // countdown → adoption (1 full + 10 fast + 1 full ≈ 13 LUT
        // cycles) drops the device off the network and `wifi.associate`'s
        // short-circuit then refuses to re-handshake. Keeping adoption
        // at fast LUT (~750 ms) until we have a more robust mitigation
        // (e.g. periodic gratuitous ARP, or recoverable re-associate
        // when ICMP unreachable bursts arrive). Per the style guide
        // this is a violation of the "view-transition = full LUT" rule;
        // accepted as a temporary trade-off — see TODO above re. fix.
        // Some ghosting from the prior boot screen may bleed through
        // the first adoption render; the periodic FULL_REFRESH_EVERY
        // cycle in compositor::refresh clears it eventually.
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
            retry_notice,
        );
    }

    fn render_main_placeholder(
        &mut self,
        ip: &str,
        last_update: Option<&str>,
        owner_email: Option<&str>,
        project_name: Option<&str>,
    ) {
        // View transition — clear ghosting with a full refresh.
        self.force_full_next_refresh = true;
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
        crate::main_placeholder::draw_main_placeholder(
            &mut region,
            ip,
            last_update,
            owner_email,
            project_name,
        );
    }

    fn render_playlist_page(
        &mut self,
        page: &cardstock::Page,
        index: u16,
        total: u16,
    ) {
        // View transition — full LUT so any prior placeholder /
        // image / page is fully cleared before this one paints.
        self.force_full_next_refresh = true;
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
        crate::playlist_page::draw_playlist_page(&mut region, page, index, total);
    }

    fn render_ota_progress(&mut self, phase: OtaPhase) {
        // First non-Idle phase after Idle (or after another view) is a
        // view transition — force a full LUT so the prior content is
        // properly cleared. Subsequent ticks (progress bar updates
        // mid-upload) use the fast LUT so the bar can tick frequently
        // without burning ~3 s per update on a full refresh. The
        // earlier "bar leftmost portion doesn't fill" bug that drove
        // us to always-full is now fixed by `begin_frame()` resetting
        // the DTM2 cursor — the partial LUT can correctly drive the
        // diff from one frame to the next.
        let was_idle = matches!(self.last_ota_phase, OtaPhase::Idle);
        let transitioning = was_idle && !matches!(phase, OtaPhase::Idle);
        if transitioning {
            self.force_full_next_refresh = true;
        }
        self.last_ota_phase = phase;

        // Reset framebuffer to white. The status-bar region gets
        // recomposed on top in compose() / refresh().
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
        crate::ota_progress::draw_ota_progress(&mut region, phase);
    }

    fn redraw_boot_screen(&mut self) {
        let Some(tpl) = self.boot_template.clone() else {
            return;
        };
        // NOT a view transition — same boot template, just updated
        // build-info overlay (IP went from "connecting..." to the
        // real DHCP-assigned address, eventually wall-clock time
        // once NTP lands per task #78). Let the next refresh take
        // the fast-LUT path: most pixels are identical to what's
        // already on the panel, so the partial waveform handles the
        // delta cheaply. The scheduled FULL_REFRESH_EVERY cycle in
        // `refresh()` still clears accumulated ghosting on a regular
        // cadence. If a future caller uses redraw_boot_screen for an
        // actual view change (e.g. image → boot splash transition),
        // they should set `force_full_next_refresh = true` themselves
        // before triggering the refresh.
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
        // so snapshot them by clone first. IP is now an Option —
        // draw_build_info renders "--" when None rather than the old
        // "connecting..." placeholder, since the IP field is strictly
        // an *address* readout (the WiFi field carries the link-state
        // signalling instead).
        // All chrome values come from the global state via
        // draw_build_info's internal snapshot. No more per-field
        // copying from self.status — that's gone, single source of
        // truth lives in `paperanywhere_ports::chrome`.
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
        crate::status_bar::draw_build_info(&mut region, &tpl.info);
    }

    async fn init(&mut self) {
        // Delegate to the bare panel's controller init (UC8179 boot
        // sequence). DO NOT clear the framebuffer here — callers may
        // have already staged content into it (e.g. boot.rs writes
        // the boot-screen bytes + build-info overlay into the
        // compositor framebuffer *before* the executor starts, then
        // the panel-actor task's init runs once the executor is
        // alive). Clearing on init would erase that pre-staged
        // content. Compositor::new already initialises the
        // framebuffer to 0xFF (all white) at construction, so init()
        // doesn't need to do it again — a second call to init()
        // would now leave the previous frame intact, which is also
        // the desired behaviour for any future "soft reinit" path.
        self.panel.init().await;
    }

    // Compositor's `write_chunk` just stages bytes into its own
    // framebuffer (no SPI; that happens at refresh time). The trait
    // signature is now async to match the bare panel impl, but
    // there's nothing to await here — we use `core::future::ready`
    // so the call resolves on first poll without scheduling overhead.
    fn write_chunk(&mut self, bytes: &[u8]) -> impl core::future::Future<Output = ()> {
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
        core::future::ready(())
    }

    fn compose(&mut self) {
        // Paint the status bar into the top region of the framebuffer.
        // status_bar::render snapshots chrome state internally — single
        // source of truth, no per-field plumbing through self.
        status_bar::render(
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

    async fn refresh(&mut self) {
        self.refresh_count = self.refresh_count.wrapping_add(1);
        let scheduled_full = self.refresh_count.is_multiple_of(FULL_REFRESH_EVERY);
        let forced_full = self.force_full_next_refresh;
        let full = scheduled_full || forced_full;
        log::info!(
            "compositor::refresh #{} ({} LUT, forced={}) — flushing {} bytes",
            self.refresh_count,
            if full { "FULL" } else { "fast" },
            forced_full,
            self.framebuffer.len()
        );
        // begin_frame resets the panel's frame-RAM write cursor to 0
        // (UC8179: re-issues CMD_DTM2). Without it, write_chunk would
        // append past the previous refresh's data, garbling the
        // displayed frame. This replaces the old `self.panel.init()`
        // call (which did the same thing as a side effect of running
        // the full boot sequence, but cost ~300 ms hard_reset every
        // refresh AND clobbered partial-LUT state we need for fast
        // refresh).
        self.panel.begin_frame().await;
        self.panel.write_chunk(&self.framebuffer).await;
        if full {
            self.panel.refresh().await;
        } else {
            self.panel.refresh_fast().await;
        }
        self.force_full_next_refresh = false;
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
    }

    async fn refresh_fast(&mut self) {
        log::info!(
            "compositor::refresh_fast — flushing {} bytes",
            self.framebuffer.len()
        );
        self.panel.begin_frame().await;
        self.panel.write_chunk(&self.framebuffer).await;
        self.panel.refresh_fast().await;
        self.main_cursor =
            main_region_offset(self.width_px, self.status_bar_height, self.color_mode);
    }
}

// `flush_with_yields` helper removed: chunking the framebuffer flush
// into 2 KB pieces with `yield_now` between each prevented the panel
// from displaying the adoption screen — boot screen stayed indefinitely
// even though /info responded, suggesting one of the intermediate
// write_chunk calls interacts badly with the UC8179's CMD_DTM2 cursor
// when paused mid-frame. The async-SPI + DMA path (task #90) is the
// proper route — it'll yield via the await on each SPI transfer
// without splitting the data plane.

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
    /// e.g. `2026-05-20 22:13 UTC`. Kept on the struct (logged on
    /// boot, surfaced via /info) even though the 3-column boot-screen
    /// layout no longer reserves a row for it.
    pub build_time: &'static str,
    /// `true` when the device was flashed via `provtool --dev`. The
    /// boot screen tags the version line with " (DEV)" so it's
    /// visually obvious the firmware is on the dev channel.
    pub is_dev: bool,
    /// Git branch the firmware was built from (e.g. `main`, `feat/x`).
    /// Populated by build.rs from `git rev-parse --abbrev-ref HEAD`;
    /// falls back to `unknown` when the build runs outside a git
    /// worktree (e.g. CI build from a tarball).
    pub branch: &'static str,
    /// Hardware manufacturer (e.g. "Seeed Studio") — sourced from the
    /// firmware-internal BoardConfig.manufacturer field. Displayed on
    /// the boot-screen Device column as the "Maker" key.
    pub manufacturer: &'static str,
    /// Specific model identifier (e.g. "reTerminal E1001"). Sibling
    /// to `manufacturer` — the two together reproduce the legacy
    /// combined-name display.
    pub device_model: &'static str,
}

impl BuildInfo {
    /// Render the boot-screen `Key: Value` overlay (Build /
    /// Environment / Build Date / IP / Device UUID + countdown) onto
    /// the bottom-center of the passed main-region framebuffer. Called
    /// by `boot.rs` after the boot-screen logo has been blitted; before
    /// the panel flush. `ip` is a state string (e.g. `"connecting..."`,
    /// `"10.0.1.42"`); `device_uuid` defaults to MAC-suffix today and
    /// will be backend-issued once the register endpoint lands.
    /// `countdown_secs` populates the bottom line — `Some(n)` shows
    /// "Transitioning in N seconds…", `None` leaves the line blank
    /// (the layout always reserves the row regardless so adding /
    /// removing the countdown doesn't shift the rest of the block).
    /// Paint the boot-screen overlay into `region`. All chrome values
    /// (UUID, name, IP, WiFi state, SSID, gateway, backend, countdown)
    /// are read from the global `paperanywhere_ports::chrome` state at
    /// render time — set them via `chrome::set_X(...)` before calling.
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

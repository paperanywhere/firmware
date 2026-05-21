//! Cross-task paint protocol.
//!
//! Every panel-touching operation flows through a single
//! `Channel<PaintCmd>` so the runtime task and any other producer
//! never directly own a `&mut Panel`. A dedicated
//! `panel_actor_task` (lives in the firmware crate, since it needs
//! the concrete board-specific Panel type) takes exclusive ownership
//! of the panel and consumes commands from the channel.
//!
//! Why this exists:
//!   * Decouples view-change initiators from view-change executors.
//!     Runtime says "show adoption screen"; actor handles the
//!     full-LUT vs fast-LUT decision, hash dedup, and refresh timing.
//!   * Gives one obvious place to add async-SPI / DMA / multi-core
//!     refactors later — the actor task body is the only thing that
//!     calls the trait, so when the trait moves to async-SPI only
//!     one file needs to change.
//!   * Coalesces bursty traffic: many `UpdateChrome` events in rapid
//!     succession get queued; the actor processes them in order and
//!     can batch a single refresh at the end.
//!
//! What this DOES NOT solve on a single-core cooperative executor:
//! sync SPI bytes inside the actor still hold the CPU for ~80 ms per
//! frame write. Other embassy tasks (embassy-net, runtime) cannot
//! preempt that. To genuinely free the executor during panel I/O we
//! need either `embedded-hal-async::spi::SpiDevice` + esp-hal DMA OR
//! a second-core executor for the panel (esp-rtos supports both).
//! The actor pattern is a prerequisite for either of those — it puts
//! the panel work in one place so the eventual async/multi-core
//! migration touches only the actor body, not every call site.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::watch::Watch;
use heapless::String as HString;
use paperanywhere_ports::chrome::RefreshKind;
use paperanywhere_ports::{DeviceStatus, OtaPhase};

/// Capacity of the paint channel. Sized for the bursty case: a
/// wake cycle might emit ~6 paint commands in quick succession
/// (set_chrome / set_ip / set_status / show_view), and the actor
/// processes them one-by-one. 16 slots gives comfortable headroom
/// without burning much RAM (each PaintCmd is small except
/// ShowImage which holds a Vec on the heap).
pub const PAINT_CHANNEL_DEPTH: usize = 16;

/// Channel element. Each paint submission gets a monotonic seq
/// assigned at send time; the actor publishes the highest-processed
/// seq to `PROCESSED_SEQ_WATCH` after each command finishes, so a
/// [`PaintHandle`] can be awaited until "its" command has actually
/// painted.
///
/// The seq sits BESIDE the cmd in the channel (not inside the
/// PaintCmd variants) so it can't be forgotten on a new variant
/// addition. New variants just need to be added to PaintCmd — the
/// queueing / completion infrastructure is variant-agnostic.
pub struct SeqCmd {
    pub seq: u32,
    pub cmd: PaintCmd,
}

/// The cross-task paint channel type. Static instances live in the
/// firmware crate; runtime accepts a `&'static PaintChannel` to its
/// `run()` entry point so the same code drives the sim and the
/// real device (sim plugs in a no-op actor).
pub type PaintChannel = Channel<CriticalSectionRawMutex, SeqCmd, PAINT_CHANNEL_DEPTH>;

/// Monotonic send counter. Each [`submit`] / [`submit_silent`] call
/// performs an atomic fetch_add to claim its own seq, then puts the
/// (seq, cmd) pair into the channel. Wrap-around at u32::MAX is a
/// non-concern — at 1 µs per submission that's ~580k years.
static SEND_SEQ: AtomicU32 = AtomicU32::new(0);

/// How many awaiting `PaintHandle`s can exist concurrently. Each one
/// holds a slot in this watch. 8 is well past any plausible
/// fan-out in the codebase (the runtime is single-threaded; the
/// only realistic case is "submit + immediately await" which uses one
/// slot at a time).
pub const MAX_PAINT_WAITERS: usize = 8;

/// Latest seq the actor has finished processing. Published via a
/// monotonic `send_if_modified` so an out-of-order publish (rare:
/// can only happen if two senders raced before enqueue) never makes
/// the value go backwards. Awaiters watch this until they see their
/// seq <= published.
pub static PROCESSED_SEQ_WATCH: Watch<CriticalSectionRawMutex, u32, MAX_PAINT_WAITERS> =
    Watch::new();

/// Handle returned by [`submit`]. Awaitable. Cheap (one u32); copying
/// or dropping it is free. Marked `#[must_use]` so a caller writing
/// `paint::submit(channel, cmd).await;` notices they're discarding
/// the handle — they probably wanted [`submit_silent`].
#[must_use = "PaintHandle is the receipt for awaiting the paint command; drop it explicitly or use submit_silent if you don't need to await"]
#[derive(Clone, Copy, Debug)]
pub struct PaintHandle {
    seq: u32,
}

impl PaintHandle {
    /// The monotonic seq this submission was assigned at send time.
    /// Mostly diagnostic — the await side calls [`await_processed`]
    /// rather than reading this directly.
    pub fn seq(self) -> u32 {
        self.seq
    }

    /// Block until the panel actor has finished processing this
    /// submission (and any refresh it triggered). Returns immediately
    /// if the actor has already moved past this seq.
    ///
    /// Falls back to a warn-and-return if all MAX_PAINT_WAITERS
    /// slots are taken — the caller's future still progresses (i.e.
    /// is not stuck), but it can't actually verify completion. In
    /// practice the slot count is well above any realistic fan-out.
    pub async fn await_processed(self) {
        let Some(mut rx) = PROCESSED_SEQ_WATCH.receiver() else {
            log::warn!(
                "paint: all {} watch slots taken; await_processed(seq={}) degrading to no-op",
                MAX_PAINT_WAITERS,
                self.seq
            );
            return;
        };
        loop {
            let current = rx.get().await;
            if current >= self.seq {
                return;
            }
            // Wait for the actor to publish a new value, then re-check.
            let _ = rx.changed().await;
        }
    }
}

/// Submit a paint command and return a handle that can be awaited.
/// Most callers can ignore the handle (use [`submit_silent`] in that
/// case); the runtime's adoption-before-register flow is the
/// canonical user of `submit` + `await_processed`.
pub async fn submit(channel: &'static PaintChannel, cmd: PaintCmd) -> PaintHandle {
    // Reserve a seq BEFORE enqueueing so the (seq, cmd) pair stays
    // together. The actor publishes the seq AFTER processing the
    // cmd — that's what await_processed waits on.
    //
    // Race note: with two concurrent senders, fetch_add can return
    // 5, 6 to A, B respectively while B enqueues first (the awaits
    // on channel.send aren't atomic with fetch_add). The actor then
    // processes (6, cmd_B), (5, cmd_A) in that order. mark_processed
    // uses send_if_modified with a `>` check so the watch goes 6→5
    // becomes a no-op (stays at 6) — A's handle wakes when the
    // watch crosses 5 (via the initial 6 publication), B's handle
    // wakes when it crosses 6. Both safe.
    let seq = SEND_SEQ.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    channel.send(SeqCmd { seq, cmd }).await;
    PaintHandle { seq }
}

/// Submit without keeping a handle. Replaces `paint.send(cmd).await`
/// at sites that don't need to synchronise on completion. Same
/// underlying behaviour as [`submit`] minus the returned handle.
pub async fn submit_silent(channel: &'static PaintChannel, cmd: PaintCmd) {
    let _ = submit(channel, cmd).await;
}

/// Publish that the actor has finished processing seq `n`. Called
/// from the panel-actor task after each `handle_cmd` returns. Safe
/// to call with a seq that's lower than the current published
/// value — the watch is monotonic via `send_if_modified`.
pub fn mark_processed(seq: u32) {
    PROCESSED_SEQ_WATCH.sender().send_if_modified(|v_opt| {
        // `v_opt` is `Option<&mut u32>` per embassy 0.7's send_if_modified
        // signature — None on the first publish, Some thereafter.
        match v_opt {
            Some(v) => {
                if seq > *v {
                    *v = seq;
                    true
                } else {
                    false
                }
            }
            None => {
                *v_opt = Some(seq);
                true
            }
        }
    });
}

/// Bounded device-id string (panel status-bar identifier).
/// Big enough for a full 36-char UUID4 + headroom. Used as the
/// device-identity slot on the adoption screen so the value matches
/// what the dashboard's device row shows — silent truncation would
/// hide a quarter of the identity.
pub type DeviceIdStr = HString<48>;
/// Bounded IPv4 dotted-quad string (e.g. `10.0.1.42`).
pub type IpStr = HString<24>;
/// Bounded local-time stamp (`HH:MM`).
pub type LastUpdateStr = HString<24>;
/// Bounded claim-code string (matches the issued code format).
pub type ClaimCodeStr = HString<16>;
/// Bounded adopt-URL string.
pub type AdoptUrlStr = HString<64>;

/// All view-change + chrome-update commands the actor can process.
/// Variants are deliberately split into "view changes" (the actor
/// uses a FULL-LUT refresh on these — view transitions must clear
/// the previous content) and "chrome / progress updates" (FAST-LUT
/// when the panel state allows).
pub enum PaintCmd {
    // ── View transitions (actor uses FULL-LUT refresh) ─────────

    /// Repaint the cached boot template. Used after DHCP completes so
    /// the IP overlay lands on the splash. Actor will skip if the
    /// hash matches the last save (no double-render).
    RedrawBootScreen,

    /// Switch to the adoption-screen view. Carries everything needed
    /// to render the view without the actor having to call back into
    /// shared state.
    ShowAdoption {
        claim_code: ClaimCodeStr,
        device_id: DeviceIdStr,
        ip: IpStr,
        adopt_url: AdoptUrlStr,
        retry_notice: Option<&'static str>,
    },

    /// Terminal halt-screen view. Carries the headline + detail +
    /// stable error code. After processing this command the actor
    /// stays on this view forever (no further refreshes).
    ShowHalt {
        headline: &'static str,
        detail: &'static str,
        code: &'static str,
    },

    /// One-shot image render. Bytes are panel-native packed framebuf
    /// content (1bpp for the GDEW075T7, etc.); the actor writes them
    /// via panel.write_chunk + refresh. Vec ownership transfers into
    /// the channel — sender allocates, actor drops after rendering.
    ShowImage {
        bytes: Vec<u8>,
        /// Update the status bar's `last_update` line to this local-
        /// time stamp before composing.
        last_update: Option<LastUpdateStr>,
    },

    // ── Chrome / progress updates (actor may use FAST-LUT) ──────

    /// Status-bar battery + WiFi RSSI sample. Doesn't itself trigger
    /// a refresh; the actor stages the new chrome state and flushes
    /// on the next view-change command (or on the next dedicated
    /// `Recompose` if/when we add one).
    UpdateChrome {
        battery_mv: Option<u16>,
        rssi_dbm: Option<i16>,
    },

    /// Device-status text for the status-bar's left block. Same
    /// no-refresh semantics as `UpdateChrome` — the new status lands
    /// in the actor's chrome state and gets flushed with the next
    /// view-change command.
    UpdateStatus(DeviceStatus),

    /// IPv4 dotted-quad for the status-bar and any view that shows
    /// the IP (boot template, adoption screen). No immediate refresh.
    UpdateIp(IpStr),

    /// IPv4 gateway address from the active DHCP lease. Surfaced on
    /// the boot-screen's Network column. `None` clears the field
    /// (e.g. when wifi disconnects). No immediate refresh.
    UpdateGateway(Option<IpStr>),

    /// Device-id fingerprint for the status-bar's left block. Set
    /// once at boot from the device token (or the MAC-derived
    /// fallback).
    UpdateDeviceId(DeviceIdStr),

    /// WiFi connectivity dropped (sender observed
    /// `WifiLink::rssi_dbm() == None`). Updates the slashed-icon
    /// state in the status bar.
    WifiDisconnected,

    /// 3-state WiFi link signal for the boot-screen Network column.
    /// Sender (runtime) emits `Connecting` before `wifi.associate`,
    /// `Connected` once DHCP succeeds, `Disconnected` on teardown.
    /// No immediate refresh.
    UpdateWifiLinkState(paperanywhere_ports::WifiLinkState),

    /// SSID the radio is associating with / associated to. Sender
    /// emits this once per cold boot (immediately after reading creds
    /// from NVS, before associate). `None` clears the field. No
    /// immediate refresh.
    UpdateSsid(Option<heapless::String<32>>),

    /// Backend-assigned device UUID (36 chars). Sender emits this
    /// after `POST /api/device/register` succeeds; the boot screen
    /// shows the new value in the dedicated UUID line below the
    /// column block. `None` clears the field. No immediate refresh.
    UpdateDeviceUuid(Option<heapless::String<48>>),

    /// User-supplied friendly name. Sender emits this after `/state`
    /// returns the `name` field; the boot screen shows it in the
    /// DEVICE column. `None` clears the field. No immediate refresh.
    UpdateDeviceName(Option<heapless::String<64>>),

    /// Live OTA progress event. The actor uses the OTA-only branch
    /// in its inner loop: enters OTA mode on the first non-Idle
    /// phase, suspends normal view changes until phase reaches
    /// `Failed` (terminal) or the chip resets on `Applied`.
    OtaProgress(OtaPhase),

    /// Synthesised by the actor's `select3` arm when a chrome::set_*
    /// call fires the dirty signal. Carries the `RefreshKind` the
    /// setter requested. Handled like any other paint command — same
    /// OTA-mode latch, same hash-dedup. Producers never construct
    /// this directly; they call `chrome::set_*(...)` and the actor
    /// synthesises this internally.
    ChromeChanged(RefreshKind),

    /// Boot-screen hold countdown. Sender is the runtime, sent once
    /// per tick (every second) for the duration of the hold window
    /// after network address and NTP wall-clock are ready and before
    /// transitioning to the next view (adoption / image). `None`
    /// clears the countdown line (used after the transition starts
    /// so subsequent boot-screen redraws don't show a stale value).
    /// The actor re-renders the boot template with the new countdown
    /// value via fast LUT — no view change, just an overlay text
    /// update.
    UpdateBootCountdown(Option<u8>),
}

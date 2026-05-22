//! Shared device-state KV. One static `Mutex<RefCell<ChromeState>>` accessed
//! from every task on every core; both producers (runtime, boot, OTA) and
//! the panel-actor consumer hit it directly.
//!
//! ## Why this exists
//!
//! Each chrome property used to require 5 touchpoints to add:
//!
//!   1. Field on `compositor::Status`
//!   2. Method on `EpaperPanel` trait
//!   3. Compositor impl of that method
//!   4. `PaintCmd::UpdateX(Value)` variant
//!   5. Actor dispatch arm + every sender
//!
//! That meant each new field was ~15 LoC across 5 files for what should be a
//! one-line mutation. With this module, adding a field is: add the field to
//! `ChromeState`, mutate it from anywhere via [`with_mut`], read snapshots
//! via [`snapshot`] in the renderer. Zero new variants, zero new trait
//! methods, zero new actor arms.
//!
//! ## What this is NOT
//!
//! Not the paint channel. View transitions (`ShowAdoption`, `RedrawBootScreen`,
//! `ShowImage`, `ShowHalt`, `OtaProgress`) still flow through
//! `PaintChannel` — they carry an explicit refresh decision and the actor's
//! LUT-selection logic lives there. State changes that DON'T imply a
//! refresh (a new RSSI sample, an IP update, a UUID coming back from
//! register) flow through here.
//!
//! ## Cross-core safety
//!
//! `CriticalSectionRawMutex` uses the `critical-section` crate, whose
//! esp-hal impl disables interrupts on the calling core AND coordinates
//! with the other core via the chip's shared CS lock. The actor on core 1
//! and producers on core 0 can hit this concurrently without races.

use core::cell::RefCell;
use core::sync::atomic::{AtomicPtr, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::String as HString;

use crate::{DeviceStatus, WifiLinkState};

/// How aggressively the compositor should react to this state change.
/// `Fast` is the default — most chrome updates are in-place text /
/// icon swaps the UC8179 partial-refresh waveform handles cleanly.
/// `Full` is for state changes that genuinely warrant clearing ghost
/// (rare; view transitions still use explicit PaintCmd events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshKind {
    Fast,
    Full,
}

/// Whether a [`set_*`] call should also push the value out to flash.
/// Default `Volatile` — memory only, gone on reset. `Flash` triggers
/// the registered persistence hook, which on the firmware writes
/// through to NVS so the value survives reboots.
///
/// Only some fields are meaningful to persist. The hook decides which
/// fields it cares about — passing `Persist::Flash` for an
/// always-volatile field (e.g. RSSI) is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    Volatile,
    Flash,
}

/// Snapshot-able device chrome state. Cheap to clone (a handful of small
/// heapless strings + scalars; ~250 bytes total). The renderer takes a
/// clone under the lock then releases — never holds the lock across the
/// e-paper SPI burst.
#[derive(Debug, Clone, Default)]
pub struct ChromeState {
    // ── Network ─────────────────────────────────────────────
    pub ssid: Option<HString<32>>,
    pub ip: Option<HString<24>>,
    pub gateway_v4: Option<HString<24>>,
    pub backend_url: Option<HString<64>>,
    pub wifi_link_state: WifiLinkState,
    pub rssi_dbm: Option<i16>,
    // ── Identity ─────────────────────────────────────────────
    /// Short identifier rendered in the status bar (e.g. `D-3F2A`). Set by
    /// boot to the MAC-derived fallback; production code may overwrite
    /// with a shorter form of the assigned UUID once one is known.
    pub device_id: Option<HString<24>>,
    /// Backend-assigned UUID (36 chars). Surfaced on the boot screen +
    /// adoption screen. None until the device's first `POST
    /// /api/device/register` lands.
    pub device_uuid: Option<HString<48>>,
    /// User-supplied friendly name. Pre-adoption: backend's auto-generated
    /// `device-XXXX`. Post-adoption: whatever the user typed.
    pub device_name: Option<HString<64>>,
    /// Email of the user whose project this device belongs to.
    /// Surfaced on the main-view placeholder. `None` while the
    /// device is in the unclaimed pool.
    pub owner_email: Option<HString<64>>,
    /// Friendly name of the project the device sits in (e.g.
    /// "Kitchen Displays"). `None` for unclaimed devices.
    pub project_name: Option<HString<48>>,
    // ── Power / connectivity ─────────────────────────────────
    pub battery_mv: Option<u16>,
    /// State-of-charge percentage (0-100). Set alongside `battery_mv`
    /// by the per-board `BatteryGauge` impl — boards with a fuel-gauge
    /// IC publish the chip's reading here; boards with only an ADC +
    /// divider publish `lipo_percent_from_mv(mv)`. Status-bar widgets
    /// prefer this field over re-deriving from `battery_mv` so that
    /// fuel-gauge boards don't lose accuracy in the render path.
    pub battery_percent: Option<u8>,
    pub usb_connected: Option<bool>,
    // ── Lifecycle ────────────────────────────────────────────
    pub device_status: DeviceStatus,
    pub last_update_local: Option<HString<24>>,
    /// Seconds remaining on the boot-screen hold countdown. `None` when
    /// no countdown is active.
    pub boot_countdown_secs: Option<u8>,
}

/// Global state instance. `const new()` requires `ChromeState::default()`
/// to be const, which embassy_sync's `Mutex::new` and `RefCell::new`
/// support since heapless::String::new() is const and our scalar types
/// are too.
static CHROME: Mutex<CriticalSectionRawMutex, RefCell<ChromeState>> =
    Mutex::new(RefCell::new(ChromeState {
        ssid: None,
        ip: None,
        gateway_v4: None,
        backend_url: None,
        wifi_link_state: WifiLinkState::Disconnected,
        rssi_dbm: None,
        device_id: None,
        device_uuid: None,
        device_name: None,
        owner_email: None,
        project_name: None,
        battery_mv: None,
        battery_percent: None,
        usb_connected: None,
        device_status: DeviceStatus::Booting,
        last_update_local: None,
        boot_countdown_secs: None,
    }));

/// Dirty signal. Fires every time a `set_*` helper (or
/// [`invalidate`] directly) is called. The panel actor on core 1 selects
/// on this in its main loop and triggers a compose + refresh of the
/// requested kind when it fires. embassy_sync's `Signal` coalesces:
/// multiple sets within one waveform cycle collapse into a single
/// refresh, which is exactly what the LUT cadence wants.
///
/// `Full` wins over `Fast` only in arrival order — if a `Fast`
/// invalidation arrives AFTER a `Full` one before the actor wakes,
/// the actor sees Fast. Callers that care about getting a `Full`
/// refresh should usually emit an explicit `PaintCmd` view-transition
/// event instead — the paint channel is the durable, ordered path
/// for things that need to happen specifically.
static DIRTY: Signal<CriticalSectionRawMutex, RefreshKind> = Signal::new();

/// `&'static` so panel-actor's `select3` can await it. Keep this stable
/// — it's part of the public actor / chrome contract.
pub fn dirty_signal() -> &'static Signal<CriticalSectionRawMutex, RefreshKind> {
    &DIRTY
}

/// Wake the compositor without changing state. Useful when an external
/// event (the OTA install path, a periodic timer, etc.) needs the
/// chrome rendered again even though no chrome value changed — most
/// callers should not need this; the `set_*` helpers already
/// invalidate.
pub fn invalidate(kind: RefreshKind) {
    DIRTY.signal(kind);
}

/// Persistence hook registry. The firmware installs a hook in
/// `boot.rs` that mirrors flash-relevant chrome fields to NVS. Other
/// consumers (sim, tests) can leave it null — `Persist::Flash` becomes
/// a memory-only update.
///
/// We hold this as a raw fn pointer cast to `*mut ()` so the static
/// can be `AtomicPtr` (which is `Sync`) — function pointers themselves
/// aren't directly storable in atomics on all targets.
static PERSIST_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the flash persistence hook. Idempotent — calling twice
/// replaces the previous hook. The hook is invoked synchronously
/// (from whichever task calls `set_X` with `Persist::Flash`); it MUST
/// be cheap and non-blocking — typically a single NVS record write
/// (~tens of ms on esp-storage's flash driver).
///
/// SAFETY: `hook` must be a valid `fn()` for the lifetime of the
/// program (i.e. a normal function, not a closure capturing locals).
pub fn register_persistence_hook(hook: fn()) {
    PERSIST_HOOK.store(hook as *mut (), Ordering::Release);
}

/// Trigger the registered persist hook (if any). Called from setters
/// with `Persist::Flash`. Safe to call without a registered hook —
/// it's a no-op then.
pub fn persist_now() {
    let raw = PERSIST_HOOK.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: only set via register_persistence_hook with a valid fn().
    let hook: fn() = unsafe { core::mem::transmute(raw) };
    hook();
}

/// Take a snapshot of the current chrome. Renderers call this at the
/// start of each refresh; mutating the global afterward doesn't change
/// the snapshot they're working from.
pub fn snapshot() -> ChromeState {
    CHROME.lock(|c| c.borrow().clone())
}

/// Atomically read and modify the chrome state. The closure runs under
/// the lock; keep it short (no I/O, no awaits — it's a blocking mutex).
/// Returns whatever the closure returns so callers can compose.
///
/// Does NOT auto-invalidate — caller controls whether to fire the
/// dirty signal. Use this for batch updates that should produce a
/// single refresh, e.g.:
///
/// ```ignore
/// chrome::with_mut(|s| {
///     s.battery_mv = Some(3850);
///     s.rssi_dbm = Some(-52);
/// });
/// chrome::invalidate(RefreshKind::Fast);
/// ```
pub fn with_mut<R>(f: impl FnOnce(&mut ChromeState) -> R) -> R {
    CHROME.lock(|c| f(&mut c.borrow_mut()))
}

// ── Convenience setters ─────────────────────────────────────
//
// Thin wrappers over `with_mut`. Each one writes the field then fires
// the dirty signal with `RefreshKind::Fast`, so the panel actor wakes
// and refreshes without the caller needing to send a PaintCmd. Adding
// a new property: add a field to ChromeState, add a one-liner setter
// here, add a read in the relevant draw function. Three lines total.
//
// Setters for fields that NVS persists (device_uuid, device_name,
// ssid, backend_url) come in two flavours:
//
//   * `set_X(value)`        — memory-only, auto-invalidate Fast
//   * `set_X_with(value, p)` — same, but honours `Persist::Flash` by
//                              calling the registered persistence hook
//
// All other (volatile-only) fields just have the simple form.

/// `None` clears the field; `Some(&str)` stores it truncated to the
/// field's heapless capacity. Truncation is silent because every
/// destination is sized to the spec maximum for what it holds (SSID =
/// 32 bytes, UUID = 48 bytes incl. headroom, etc.) — any longer
/// value is already invalid at the protocol level.
pub fn set_ssid(ssid: Option<&str>) {
    set_ssid_with(ssid, Persist::Volatile);
}
pub fn set_ssid_with(ssid: Option<&str>, persist: Persist) {
    with_mut(|s| s.ssid = ssid.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_ip(ip: Option<&str>) {
    with_mut(|s| s.ip = ip.map(hstring_from));
    invalidate(RefreshKind::Fast);
}

pub fn set_gateway(gw: Option<&str>) {
    with_mut(|s| s.gateway_v4 = gw.map(hstring_from));
    invalidate(RefreshKind::Fast);
}

pub fn set_backend_url(url: Option<&str>) {
    set_backend_url_with(url, Persist::Volatile);
}
pub fn set_backend_url_with(url: Option<&str>, persist: Persist) {
    with_mut(|s| s.backend_url = url.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_wifi_link_state(state: WifiLinkState) {
    with_mut(|s| s.wifi_link_state = state);
    invalidate(RefreshKind::Fast);
}

pub fn set_rssi_dbm(rssi: Option<i16>) {
    with_mut(|s| s.rssi_dbm = rssi);
    invalidate(RefreshKind::Fast);
}

pub fn set_device_id(id: Option<&str>) {
    with_mut(|s| s.device_id = id.map(hstring_from));
    invalidate(RefreshKind::Fast);
}

pub fn set_device_uuid(uuid: Option<&str>) {
    set_device_uuid_with(uuid, Persist::Volatile);
}
pub fn set_device_uuid_with(uuid: Option<&str>, persist: Persist) {
    with_mut(|s| s.device_uuid = uuid.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_device_name(name: Option<&str>) {
    set_device_name_with(name, Persist::Volatile);
}
pub fn set_device_name_with(name: Option<&str>, persist: Persist) {
    with_mut(|s| s.device_name = name.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_owner_email(email: Option<&str>) {
    set_owner_email_with(email, Persist::Volatile);
}
pub fn set_owner_email_with(email: Option<&str>, persist: Persist) {
    with_mut(|s| s.owner_email = email.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_project_name(name: Option<&str>) {
    set_project_name_with(name, Persist::Volatile);
}
pub fn set_project_name_with(name: Option<&str>, persist: Persist) {
    with_mut(|s| s.project_name = name.map(hstring_from));
    maybe_persist(persist);
    invalidate(RefreshKind::Fast);
}

pub fn set_battery_mv(mv: Option<u16>) {
    with_mut(|s| s.battery_mv = mv);
    invalidate(RefreshKind::Fast);
}

/// Atomic battery update: mv + percent published together. Used by
/// [`BatteryGauge`] consumers so the status bar never reads a torn
/// pair where mv reflects a fresh sample but percent is stale (or
/// vice versa). Pass `None` to clear (e.g. "battery disconnected").
pub fn set_battery(sample: Option<crate::BatterySample>) {
    with_mut(|s| {
        s.battery_mv = sample.map(|x| x.mv);
        s.battery_percent = sample.map(|x| x.percent);
    });
    invalidate(RefreshKind::Fast);
}

pub fn set_usb_connected(c: Option<bool>) {
    with_mut(|s| s.usb_connected = c);
    invalidate(RefreshKind::Fast);
}

pub fn set_device_status(status: DeviceStatus) {
    with_mut(|s| s.device_status = status);
    invalidate(RefreshKind::Fast);
}

pub fn set_last_update(stamp: Option<&str>) {
    with_mut(|s| s.last_update_local = stamp.map(hstring_from));
    invalidate(RefreshKind::Fast);
}

pub fn set_boot_countdown_secs(secs: Option<u8>) {
    with_mut(|s| s.boot_countdown_secs = secs);
    invalidate(RefreshKind::Fast);
}

fn maybe_persist(persist: Persist) {
    if matches!(persist, Persist::Flash) {
        persist_now();
    }
}

/// Build a heapless::String<N> from `&str`, truncating silently if the
/// input exceeds `N` bytes. We pre-size the destinations to the spec /
/// protocol maximums of the respective fields, so truncation in practice
/// means the caller passed something already-invalid.
fn hstring_from<const N: usize>(s: &str) -> HString<N> {
    let mut h: HString<N> = HString::new();
    let cap = s.len().min(N);
    // Walk by char boundary to avoid panicking on a sliced multibyte
    // char. ASCII (the only thing we put here in practice — UUIDs,
    // IPs, SSIDs, hex tokens) is unaffected.
    let safe_end = s
        .char_indices()
        .take_while(|(i, _)| *i <= cap)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let _ = h.push_str(&s[..safe_end]);
    h
}

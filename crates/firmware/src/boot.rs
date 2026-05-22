//! Boot orchestration — runs the firmware-specific cold-boot work (factory
//! reset detection, provisioning resolution, claim-flow stub), spins up the
//! IP stack, then drives [`paperanywhere_runtime::run`] on an embassy
//! executor alongside the embassy-net runner task.
//!
//! Each port instance is parked in a `StaticCell` so the embassy-task macro —
//! which requires `'static` arguments — can accept it.

use embassy_net::Stack;
use esp_hal::system::Stack as CoreStack;
use esp_println::println;
use paperanywhere_ports::{EpaperPanel, NvsStore, Sleeper};
use static_cell::StaticCell;

use crate::panel_actor;

use crate::boards;
use crate::http::FwHttp;
use crate::nvs::FwNvs;
use crate::ota::FwOta;
use crate::battery::FwBatteryGauge;
use crate::power::FwSleeper;
use crate::provisioning::SetupPath;
use crate::resources::FirmwareResources;
use crate::wifi::FwWifi;

/// Boot-screen bytes baked at build time by `build.rs` from `assets/logo.svg`,
/// rasterised + dithered + packed for the active board's panel format.
/// Rendered once at boot through `EpaperPanel::write_chunk`.
pub const BOOT_SCREEN: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/boot_screen.bin"));

/// "Updating firmware..." status screen, baked from `assets/logo_ota.svg`.
/// Rendered by the runtime right before kicking off an OTA install so
/// the user sees something during the ~30–60s flash-write window. After
/// the actor-pattern refactor the live OTA-progress view (see
/// `panel_actor::handle_cmd` for `PaintCmd::OtaProgress`) supersedes
/// this fallback bitmap; we keep the asset baked so a future
/// fall-back-path (e.g. headless install with no progress signal)
/// can use it without a build script change.
#[allow(dead_code)]
pub const OTA_SCREEN: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/ota_screen.bin"));

// Each port lives in a `StaticCell` so we can hand the embassy task a
// `&'static mut` to it. The cells are filled exactly once during `run` and
// then never freed — the firmware loops forever.
static WIFI: StaticCell<FwWifi> = StaticCell::new();
static HTTP: StaticCell<FwHttp> = StaticCell::new();
static NVS: StaticCell<FwNvs> = StaticCell::new();
static PANEL: StaticCell<boards::Panel> = StaticCell::new();
/// Shared SPI2 bus the panel and the SD card both draw against.
/// Populated once at boot from main.rs's `Spi<Async>` handle.
static SHARED_SPI_BUS: StaticCell<boards::SharedSpiBus> = StaticCell::new();
static SLEEPER: StaticCell<FwSleeper> = StaticCell::new();
static BATTERY: StaticCell<FwBatteryGauge> = StaticCell::new();
static OTA: StaticCell<FwOta> = StaticCell::new();
static STACK_HANDLE: StaticCell<Stack<'static>> = StaticCell::new();
static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();

/// Pointer to the FwNvs instance the persistence hook writes through.
/// Stored as an `AtomicPtr<FwNvs>` instead of `*mut FwNvs` so the
/// boot path can publish it before the hook is registered without
/// upsetting the no-Sync-for-raw-pointers rule. `chrome::persist_now`
/// (called from `chrome::set_*_with(.., Persist::Flash)`) invokes the
/// hook below, which reads this atomic.
///
/// SAFETY contract:
///   * Set ONCE from `boot::run` after FwNvs::init returns, before
///     any task could call `set_*_with(.., Persist::Flash)`.
///   * Read by the chrome-persistence hook on whatever task triggered
///     the persist call. Reads happen long after the write completes,
///     so Acquire/Release ordering is enough — no further sync needed.
///   * The pointed-to FwNvs lives `'static` (via the NVS StaticCell)
///     so the pointer is always valid for the lifetime of the program.
pub(crate) static NVS_HANDOFF: core::sync::atomic::AtomicPtr<FwNvs> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Module-level pointer to the FwWifi instance, published once at boot.
/// Lets the HTTP retry-on-blackhole path force a WiFi reassociation when
/// post-DHCP unicast traffic gets blackholed by the AP (see
/// `project_register_unifi_blackhole` memory) without threading a
/// `&mut FwWifi` through the HTTP call sites.
///
/// SAFETY: same contract as [`NVS_HANDOFF`] — set once at boot before
/// any task starts; reads are Acquire-ordered; the pointed-to FwWifi
/// lives `'static`.
pub(crate) static WIFI_HANDOFF_GLOBAL: core::sync::atomic::AtomicPtr<crate::wifi::FwWifi> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Persistence hook installed via `chrome::register_persistence_hook`.
/// Called synchronously whenever a chrome setter is invoked with
/// `Persist::Flash`. Reads the current chrome snapshot and mirrors the
/// flash-relevant fields (device_uuid, device_name, backend_url,
/// ssid) into NVS. Each `nvs.save_*` early-returns if the value is
/// unchanged, so spurious calls are cheap.
fn flash_persist_hook() {
    let raw = NVS_HANDOFF.load(core::sync::atomic::Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: see NVS_HANDOFF invariants. Single mutable
    // reference is fine here because save_* methods don't await or
    // recurse into chrome.
    let nvs = unsafe { &mut *raw };
    let snap = paperanywhere_ports::chrome::snapshot();
    if let Some(uuid) = snap.device_uuid.as_deref() {
        nvs.save_device_uuid(uuid);
    }
    if let Some(name) = snap.device_name.as_deref() {
        nvs.save_device_name(name);
    }
    // backend_url + ssid persistence intentionally NOT wired here yet
    // — those are sourced from the prov partition today, and adding a
    // write path before the captive-portal / settings UI lands would
    // create surprising "where did my SSID go" scenarios on a reset.
    // The hook is the right place once those flows exist.
}

/// 16 KB stack for the APP core's main thread (where its embassy
/// executor lives). The actor task itself runs on the executor's
/// task arena, not this stack — this is just the boot/poll loop for
/// the second core. 16 KB matches what we'd give a freshly-spawned
/// embassy task on the primary core.
const APP_CORE_STACK_BYTES: usize = 16 * 1024;
static APP_CORE_STACK: StaticCell<CoreStack<APP_CORE_STACK_BYTES>> = StaticCell::new();
/// Embassy executor that owns the panel-actor task on core 1. Lives
/// in a StaticCell so we can hand the `&'static mut` required by
/// `Executor::run` into the second-core closure.
static CORE1_EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();

/// Cross-task channel for OTA progress events. The runtime's OTA
/// install path writes to this; the panel-actor task reads it and
/// renders the live progress view. Latest-wins semantics: if
/// multiple signals arrive before the actor picks them up, only the
/// most recent phase is preserved — which is what we want, since the
/// progress view only ever renders the current state.
pub static OTA_PROGRESS: paperanywhere_runtime::OtaProgressChannel =
    paperanywhere_runtime::OtaProgressChannel::new();

/// Cross-task paint channel: all view-change + chrome-update commands
/// flow through here to the panel-actor task (which OWNS the panel
/// exclusively). Currently the runtime still drives the panel
/// directly via `&mut Panel`; the next-session migration replaces
/// every panel.* call site with `PAINT_CHANNEL.send(...)` and
/// spawns the actor (`panel_actor::panel_actor_task`) on core 1
/// for true dual-core parallelism. See `panel_actor.rs` for the
/// task body.
pub static PAINT_CHANNEL: paperanywhere_runtime::PaintChannel =
    paperanywhere_runtime::PaintChannel::new();

pub fn run(resources: FirmwareResources) -> ! {
    let FirmwareResources {
        board,
        timg0,
        sw_int0,
        rng,
        wifi,
        lpwr,
        flash,
        cpu_ctrl,
        sw_int1,
        panel,
        battery,
        sd,
    } = resources;

    if factory_reset_held(board) {
        println!("boot: factory reset triggered");
        crate::nvs::factory_reset();
    }

    // Build NVS *before* resolving provisioning so the prov-partition
    // migration path can write directly into it. The resolver may also
    // migrate from other future sources (SD card, captive portal).
    let mut nvs = FwNvs::init(flash);
    let path = crate::provisioning::resolve(board, &mut nvs);
    println!("boot: setup path = {:?}", path);
    if matches!(path, SetupPath::NotProvisioned) {
        crate::nvs::claim_flow_stub(board);
        halt();
    }

    if crate::nvs::load_pending_claim_code().is_some() {
        println!("boot: pending claim code in NVS (auto-claim TBD)");
    }

    // One-time radio init. The station interface comes back paired with the
    // controller so we can hand it to embassy-net for the IP layer.
    let (wifi, interface) = match FwWifi::init(timg0, sw_int0, rng, wifi) {
        Ok(pair) => pair,
        Err(e) => {
            println!("boot: wifi init failed: {:?}", e);
            halt();
        }
    };

    // Build the stack now (on core 0); both the embassy-net runner task and
    // the polling runtime task will share it through `STACK_HANDLE`.
    let (stack, runner) = crate::network::build(interface);
    let stack_ref: &'static Stack<'static> = STACK_HANDLE.init(stack);

    // FwNvs was already built above so provisioning::resolve could migrate
    // into it. Pull backend_url back out for the HTTP client constructor.
    let backend_url = nvs.load_backend_url();

    let wifi_ref = WIFI.init(wifi);
    // Let FwWifi see the embassy-net stack so `WifiLink::local_ip` can
    // pull the DHCP-assigned address for the status bar + dev /info.
    wifi_ref.attach_stack(stack_ref);
    let http_ref = HTTP.init(FwHttp::new(stack_ref, backend_url.as_deref()));
    let nvs_ref = NVS.init(nvs);
    // Park the panel's SPI bus into the shared `'static` async
    // Mutex. Panel uses an async SpiDevice wrapper; SD wraps the
    // same Mutex with a blocking-over-async adapter. The Mutex
    // serves both worlds.
    let crate::resources::PanelHardware { spi_bus, cs, dc, rst, busy } = panel;
    let shared_bus_ref: &'static boards::SharedSpiBus =
        SHARED_SPI_BUS.init(embassy_sync::mutex::Mutex::new(spi_bus));
    let panel_pins = boards::PanelPins { cs, dc, rst, busy };
    let panel_ref =
        PANEL.init(boards::build_panel(shared_bus_ref, panel_pins, board));
    // Now bring up the SD card against the same shared bus.
    let sd_board = board.sd;
    if let (Some(sd_hw), Some(sd_quirks)) = (sd, sd_board) {
        let state = crate::sd::FwSd::mount(shared_bus_ref, sd_hw, &sd_quirks);
        log::info!("sd: mount() returned {}", state.describe());
        // Driver handle drops here — once a consumer (e.g. the
        // panel actor for graph rasters, or SwapAlloc) wants it,
        // pull it out and StaticCell-park alongside the rest of
        // the firmware's '_static handles.
        let _ = state;
    } else {
        log::info!("sd: skipping mount — board has no SD slot or pins not wired");
    }
    let sleeper_ref = SLEEPER.init(FwSleeper::new(lpwr));
    // Battery gauge built from the per-board hardware bundle. Owned
    // by core 1 alongside the runtime (it's a peripheral-access path,
    // not a network-stack one).
    let battery_ref = BATTERY.init(crate::battery::new_from_resources(battery));
    let ota_ref = OTA.init(FwOta::new());

    // Hand the NVS instance to the chrome persistence hook so
    // `chrome::set_*_with(.., Persist::Flash)` from any task can write
    // through to flash. Pointer published BEFORE register_persistence_hook
    // so a setter racing this couldn't dereference a stale null.
    NVS_HANDOFF.store(
        nvs_ref as *mut FwNvs,
        core::sync::atomic::Ordering::Release,
    );
    paperanywhere_ports::chrome::register_persistence_hook(flash_persist_hook);

    // Device-id fallback: if NVS has no claim token yet, derive an
    // identifier from the chip's base MAC address (last two octets).
    // The runtime's set_device_id call later may override this if a
    // real token shows up. With or without claim, the status bar then
    // shows something useful instead of "--".
    let mac = esp_hal::efuse::base_mac_address();
    let mac_bytes: &[u8] = mac.as_bytes();
    let mac_id = if mac_bytes.len() >= 6 {
        alloc::format!("D-{:02X}{:02X}", mac_bytes[4], mac_bytes[5])
    } else {
        alloc::string::String::from("D-????")
    };
    panel_ref.set_device_id(&mac_id);

    // Build the identity payload for /api/device/register. Format the
    // MAC as `aa:bb:cc:dd:ee:ff` since that's the canonical form the
    // backend's storage layer normalises to anyway. panel_model_id
    // comes straight from the active board's `BoardConfig`.
    let mac_str = if mac_bytes.len() >= 6 {
        alloc::format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2],
            mac_bytes[3], mac_bytes[4], mac_bytes[5],
        )
    } else {
        alloc::string::String::from("00:00:00:00:00:00")
    };
    let identity = paperanywhere_ports::DeviceIdentity {
        mac: mac_str,
        panel_model_id: board.panel_model_id,
        fw_version: alloc::string::String::from(crate::FW_VERSION),
    };

    // If this boot is the first one after an OTA install, the slot is
    // still marked `New`/`PendingVerify`. Graduate it to `Valid` so the
    // bootloader doesn't auto-roll-back. Cheap no-op on normal boots.
    crate::ota::mark_current_app_valid();

    // Translate the board's local PowerPolicy enum to the ports enum. They're
    // structurally identical; the board crate stays self-contained so its
    // module table doesn't have to import ports just for one enum.
    //
    // Dev devices force AlwaysOn so the developer can iterate without
    // waiting 6 h for the next scheduled wake. Backend-driven OTA
    // pushes (task #93) rely on the device polling the backend
    // continuously to pick up `firmware_update` offers; deep sleep
    // would defer that to the next scheduled wake.
    let policy = if nvs_ref.load_is_dev_build() {
        paperanywhere_ports::PowerPolicy::AlwaysOn
    } else {
        match board.default_power_policy {
            crate::boards::PowerPolicy::ScheduledWake => {
                paperanywhere_ports::PowerPolicy::ScheduledWake
            }
            crate::boards::PowerPolicy::AlwaysOn => paperanywhere_ports::PowerPolicy::AlwaysOn,
        }
    };

    // Stage the boot screen: logo into the main region, build-info
    // overlay on top, status bar composed by the compositor. Then hash
    // the final composed framebuffer and ask NVS whether we already
    // displayed this. Hash dedup happens at the driver level (here,
    // the compositor's pending_hash) so a status-bar widget update
    // also counts as "new content" — the previous wake's battery %
    // becoming stale doesn't get skipped just because the boot screen
    // bytes are identical.
    // Cache the boot screen on the compositor so the runtime can
    // re-render it after DHCP (with the IP overlaid under the version
    // line). The initial paint below uses the same template with
    // ip=None so the panel comes up immediately, even before WiFi
    // associates.
    let build_info = crate::build_info(
        nvs_ref.load_is_dev_build(),
        board.manufacturer,
        board.model,
    );
    panel_ref.cache_boot_template(BOOT_SCREEN, build_info);
    // Seed runtime boot-screen state with what we know pre-executor.
    // Gateway stays None until DHCP completes (the runtime will push
    // it via set_gateway once embassy-net's stack has a config_v4
    // lease). Backend URL is read from NVS at boot, so we can render
    // it on the splash immediately.
    panel_ref.set_backend_url(backend_url.as_deref());
    panel_ref.set_gateway(None);
    panel_ref.set_boot_countdown(None);

    // Pre-stage the boot-screen content into the compositor's
    // framebuffer + chrome state. NO panel SPI yet — that all happens
    // inside the panel-actor task once the executor is running.
    //
    // Why: the panel SPI driver is now async (task #90). Async SPI
    // writes register a real interrupt waker which `embassy_futures::
    // block_on`'s noop_waker can't fire — so a pre-executor block_on
    // of `panel.init().await` busy-loops forever waiting for a wake
    // that never arrives. Pushing the init + first refresh into the
    // actor task body sidesteps the problem: by the time the actor
    // runs, esp-rtos's embassy executor is alive and wakers work.
    //
    // Cost: the boot screen appears ~1-2 frames later (executor
    // startup time) instead of immediately. In practice the panel
    // takes ~3 s for its full-LUT refresh anyway, so the delta is
    // imperceptible.
    // Battery starts unknown — the first real sample comes from the
    // runtime's wake cycle once the gauge is owned by core 1. The
    // status bar renders "--" until that lands (~immediately).
    panel_ref.set_chrome(None, None);
    // IP stays unset (None) pre-DHCP — the boot-screen render
    // displays "--" rather than the old "connecting..." placeholder.
    // The WiFi field carries the link-state signalling instead, set
    // below to Connecting since the first wake cycle will attempt
    // association immediately.
    use paperanywhere_ports::WifiLinkState;
    panel_ref.set_wifi_link_state(WifiLinkState::Connecting);
    // Surface the SSID we're about to associate with so the user can
    // verify which network the device is targeting (especially useful
    // on a shared workbench / multiple test networks).
    let initial_ssid = crate::nvs::load_wifi_creds_raw().map(|(ssid, _)| ssid);
    panel_ref.set_ssid(initial_ssid.as_deref());

    // Pre-stage UUID + friendly name from NVS so the FIRST boot screen
    // already shows them on a non-fresh boot. The runtime refreshes
    // both on register / /state success — so a freshly factory-reset
    // device renders "(awaiting...)" in the UUID slot until register
    // returns, then updates live. We deliberately do NOT fall back
    // to any MAC-derived placeholder: a UUID slot showing a MAC-like
    // value misleads the user into thinking that's their identity.
    let cached_uuid: Option<alloc::string::String> = nvs_ref.load_device_uuid();
    let cached_name: Option<alloc::string::String> = nvs_ref.load_device_name();
    panel_ref.set_device_uuid(cached_uuid.as_deref());
    panel_ref.set_device_name(cached_name.as_deref());

    // Compositor's write_chunk now returns a future (trait signature
    // matches the bare panel impl), but the actual framebuffer write
    // happens at the synchronous call edge — the returned future is
    // just `core::future::ready(())`. Drop it; no await needed.
    drop(panel_ref.write_chunk(BOOT_SCREEN));
    {
        // Pre-stage every chrome value the boot-screen overlay reads.
        // After this block, the global `chrome` KV has everything the
        // compositor needs; render_into snapshots it internally.
        // Producers no longer need to thread arguments through the
        // render API — this IS the chrome-as-source-of-truth design.
        paperanywhere_ports::chrome::set_device_uuid(cached_uuid.as_deref());
        paperanywhere_ports::chrome::set_device_name(cached_name.as_deref());
        paperanywhere_ports::chrome::set_ip(None);
        paperanywhere_ports::chrome::set_wifi_link_state(WifiLinkState::Connecting);
        paperanywhere_ports::chrome::set_ssid(initial_ssid.as_deref());
        paperanywhere_ports::chrome::set_gateway(None);
        paperanywhere_ports::chrome::set_backend_url(backend_url.as_deref());
        paperanywhere_ports::chrome::set_boot_countdown_secs(None);

        let mut region = panel_ref.main_region_mut();
        build_info.render_into(&mut region);
    }
    panel_ref.compose();

    // Dual-core split (task #100):
    //
    //   * Core 0 (PRO_CPU): net_task + http_proxy_task + esp-radio
    //     internals. The network stack and HTTP client get this core
    //     to themselves, so a heavy wake-cycle on the application
    //     side can't starve TCP / DHCP / DNS polling.
    //   * Core 1 (APP_CPU): runtime_task + panel_actor_task. The
    //     state machine + panel rendering share an executor. When
    //     the runtime needs HTTP it sends through `http_proxy::
    //     REQ_CHANNEL` (a `CriticalSectionRawMutex` channel that
    //     crosses cores cleanly) and awaits a `Signal`-backed reply
    //     slot. The runtime can stall briefly during a panel SPI
    //     burst on the same executor, but the proxy on core 0 keeps
    //     driving the actual TCP socket — no network starvation.
    //
    // Why the runtime moved off core 0: pre-#100, runtime + net_task
    // shared an executor; a fanout of chrome::set_* + paint::submit
    // + post-DHCP countdown ticks consumed enough CPU that net_task
    // couldn't poll often enough, and a first-register on a fresh
    // boot would take ~90 s instead of seconds. Splitting them
    // collapses that to the actual TLS+TCP RTT.
    //
    // Why HTTP can't just live on core 1: embassy_net::Stack is
    // !Send (PhantomData<*const ()>) — the Stack runner + every
    // task that builds a TcpSocket against it must share an
    // executor. esp-radio's tasks also pin to core 0
    // (esp-wifi-sys#412 crashes if migrated). So the Stack and
    // FwHttp stay on core 0; the proxy task on core 0 is the
    // single user of FwHttp.
    //
    // Why the AtomicPtr handoffs: esp-hal marks `Async` as `!Send`
    // via PhantomData<*const ()> — `boards::Panel` (containing
    // Spi<Async>) is !Send, and `&'static mut Panel` is !Send too.
    // `start_second_core` takes `FnOnce + Send + 'static`, and a
    // closure that captures any !Send value isn't Send. We
    // sidestep by parking each ref in a `static AtomicPtr` BEFORE
    // calling `start_second_core`; the closure captures nothing
    // typed and stays Send. Core 1 reads each pointer once and
    // never again. FwWifi / FwNvs / FwSleeper / FwOta get the same
    // treatment for consistency — even ones that might be Send
    // today could acquire !Send fields later, and the AtomicPtr
    // pattern is uniform.
    //
    // SAFETY of each handoff:
    //   1. Core 0 stores the pointer before `start_second_core`'s
    //      barrier (Release).
    //   2. Core 1 loads it once at task-spawn time (Acquire) and
    //      never re-reads.
    //   3. Each underlying object is `&'static mut`, lives forever,
    //      and is never accessed by core 0 again after the handoff.
    static PANEL_HANDOFF: core::sync::atomic::AtomicPtr<boards::Panel> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static NVS_REF_HANDOFF: core::sync::atomic::AtomicPtr<FwNvs> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static SLEEPER_HANDOFF: core::sync::atomic::AtomicPtr<FwSleeper> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static OTA_HANDOFF: core::sync::atomic::AtomicPtr<FwOta> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static BATTERY_HANDOFF: core::sync::atomic::AtomicPtr<FwBatteryGauge> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    // FwWifi + FwHttp moved to core 1 after the embassy-net fork made
    // `&Stack` cross-core safe (#101). The proxy pattern's main cost
    // was a `Vec<u8>` buffer per blob download — removing it streams
    // bytes directly from FwHttp → runtime → panel, which is what
    // was blowing the heap on /state. esp-radio's only hard core-0
    // pin remains net_task (the Stack Runner).
    static WIFI_HANDOFF: core::sync::atomic::AtomicPtr<crate::wifi::FwWifi> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static HTTP_HANDOFF: core::sync::atomic::AtomicPtr<crate::http::FwHttp> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static IDENTITY_CELL: StaticCell<paperanywhere_ports::DeviceIdentity> =
        StaticCell::new();
    PANEL_HANDOFF.store(
        panel_ref as *mut boards::Panel,
        core::sync::atomic::Ordering::Release,
    );
    NVS_REF_HANDOFF.store(
        nvs_ref as *mut FwNvs,
        core::sync::atomic::Ordering::Release,
    );
    SLEEPER_HANDOFF.store(
        sleeper_ref as *mut FwSleeper,
        core::sync::atomic::Ordering::Release,
    );
    OTA_HANDOFF.store(
        ota_ref as *mut FwOta,
        core::sync::atomic::Ordering::Release,
    );
    BATTERY_HANDOFF.store(
        battery_ref as *mut FwBatteryGauge,
        core::sync::atomic::Ordering::Release,
    );
    WIFI_HANDOFF.store(
        wifi_ref as *mut crate::wifi::FwWifi,
        core::sync::atomic::Ordering::Release,
    );
    // Also publish to the module-level handoff so the retry-on-
    // blackhole path in http.rs / wifi.rs can find it without
    // having to plumb a &mut FwWifi through every call.
    WIFI_HANDOFF_GLOBAL.store(
        wifi_ref as *mut crate::wifi::FwWifi,
        core::sync::atomic::Ordering::Release,
    );
    HTTP_HANDOFF.store(
        http_ref as *mut crate::http::FwHttp,
        core::sync::atomic::Ordering::Release,
    );
    // DeviceIdentity goes through a StaticCell because it's an
    // owned, Send, !Copy value — capturing it by-value in the
    // closure would consume it, and the runtime needs an owned
    // copy. Static-cell promotes it to a 'static ref the closure
    // can clone from cleanly.
    let identity_ref: &'static paperanywhere_ports::DeviceIdentity =
        IDENTITY_CELL.init(identity);

    let app_stack = APP_CORE_STACK.init(CoreStack::new());
    esp_rtos::start_second_core(
        cpu_ctrl,
        sw_int1,
        app_stack,
        move || {
            // SAFETY: see handoff invariants in the parent comment.
            let panel_ref: &'static mut boards::Panel = unsafe {
                &mut *PANEL_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let nvs_ref: &'static mut FwNvs = unsafe {
                &mut *NVS_REF_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let sleeper_ref: &'static mut FwSleeper = unsafe {
                &mut *SLEEPER_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let ota_ref: &'static mut FwOta = unsafe {
                &mut *OTA_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let battery_ref: &'static mut FwBatteryGauge = unsafe {
                &mut *BATTERY_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let wifi_ref_core1: &'static mut crate::wifi::FwWifi = unsafe {
                &mut *WIFI_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let http_ref_core1: &'static mut crate::http::FwHttp = unsafe {
                &mut *HTTP_HANDOFF.load(core::sync::atomic::Ordering::Acquire)
            };
            let core1_executor =
                CORE1_EXECUTOR.init(esp_rtos::embassy::Executor::new());
            core1_executor.run(|spawner| {
                // Panel actor: unchanged.
                let actor_token = panel_actor::panel_actor_task(
                    panel_ref,
                    &PAINT_CHANNEL,
                    &OTA_PROGRESS,
                )
                .expect("panel_actor_task pool");
                spawner.spawn(actor_token);
                // DIAG (task #117): runtime moved BACK to core 0 to
                // share an executor with net_task. Tests whether the
                // cross-core arrangement is what's breaking TCP
                // connect (SYN goes out but never completes
                // handshake). The unused captures below are kept so
                // the closure still compiles; runtime spawn is now
                // in core 0's executor.run below.
                let _ = (wifi_ref_core1, http_ref_core1, nvs_ref,
                         sleeper_ref, ota_ref, battery_ref);
            });
        },
    );

    // Core 0's executor: net_task + heartbeat only. esp-radio's
    // embassy-net Runner has a hard core-0 pin (esp-wifi-sys#412),
    // so net_task can't migrate — but every other path moved to
    // core 1 once the embassy-net fork made `&Stack` Sync. Removing
    // the proxies also removed the per-blob `Vec<u8>` buffer in
    // stream_blob, which was the prime OOM suspect on /state.
    let executor = EXECUTOR.init(esp_rtos::embassy::Executor::new());
    executor.run(|spawner| {
        let net_token = crate::network::net_task(runner).expect("net_task pool");
        spawner.spawn(net_token);
        // Periodic heartbeat: 5-second heap + chrome-state dumps so
        // a stuck wake gets a continuous "where things are" trace on
        // the serial monitor without needing a JTAG attach. Lives
        // on core 0 alongside net_task so it can't itself be
        // starved by application work.
        let diag_token =
            crate::diagnostics::heartbeat_task().expect("heartbeat_task pool");
        spawner.spawn(diag_token);
        // DIAG (task #117): runtime co-located with net_task on
        // core 0 instead of core 1. Same executor → smoltcp's
        // wakers fire in-executor instead of crossing the IPI
        // boundary that's been breaking TCP handshake.
        let rt_token = runtime_task(
            wifi_ref,
            http_ref,
            nvs_ref,
            sleeper_ref,
            ota_ref,
            battery_ref,
            policy,
            identity_ref.clone(),
            &OTA_PROGRESS,
            &PAINT_CHANNEL,
        )
        .expect("runtime_task pool");
        spawner.spawn(rt_token);
    })
}

#[embassy_executor::task]
async fn runtime_task(
    wifi: &'static mut crate::wifi::FwWifi,
    http: &'static mut crate::http::FwHttp,
    nvs: &'static mut FwNvs,
    sleeper: &'static mut FwSleeper,
    ota: &'static mut FwOta,
    battery: &'static mut FwBatteryGauge,
    policy: paperanywhere_ports::PowerPolicy,
    identity: paperanywhere_ports::DeviceIdentity,
    ota_progress: &'static paperanywhere_runtime::OtaProgressChannel,
    paint: &'static paperanywhere_runtime::PaintChannel,
) -> ! {
    paperanywhere_runtime::run(
        wifi,
        http,
        nvs,
        sleeper,
        ota,
        battery,
        policy,
        identity,
        ota_progress,
        paint,
    )
    .await
}

fn factory_reset_held(board: crate::boards::BoardConfig) -> bool {
    if !board.has_buttons {
        return false;
    }
    false
}

fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

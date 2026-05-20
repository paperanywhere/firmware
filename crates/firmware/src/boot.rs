//! Boot orchestration — runs the firmware-specific cold-boot work (factory
//! reset detection, provisioning resolution, claim-flow stub), spins up the
//! IP stack, then drives [`paperanywhere_runtime::run`] on an embassy
//! executor alongside the embassy-net runner task.
//!
//! Each port instance is parked in a `StaticCell` so the embassy-task macro —
//! which requires `'static` arguments — can accept it.

use embassy_net::Stack;
use esp_println::println;
use paperanywhere_ports::{EpaperPanel, NvsStore, Sleeper};
use static_cell::StaticCell;

use crate::boards;
use crate::http::FwHttp;
use crate::nvs::FwNvs;
use crate::ota::FwOta;
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
/// Rendered by the runtime right before kicking off an OTA install so the
/// user sees something during the ~30–60s flash-write window instead of a
/// stale image. After the chip resets into the new slot the regular boot
/// screen takes over.
pub const OTA_SCREEN: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/ota_screen.bin"));

// Each port lives in a `StaticCell` so we can hand the embassy task a
// `&'static mut` to it. The cells are filled exactly once during `run` and
// then never freed — the firmware loops forever.
static WIFI: StaticCell<FwWifi> = StaticCell::new();
static HTTP: StaticCell<FwHttp> = StaticCell::new();
static NVS: StaticCell<FwNvs> = StaticCell::new();
static PANEL: StaticCell<boards::Panel> = StaticCell::new();
static SLEEPER: StaticCell<FwSleeper> = StaticCell::new();
static OTA: StaticCell<FwOta> = StaticCell::new();
static STACK_HANDLE: StaticCell<Stack<'static>> = StaticCell::new();
static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();

pub fn run(resources: FirmwareResources) -> ! {
    let FirmwareResources {
        board,
        timg0,
        sw_int0,
        rng,
        wifi,
        lpwr,
        flash,
        cpu_ctrl: _,
        sw_int1: _,
        panel,
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
    let http_ref = HTTP.init(FwHttp::new(stack_ref, backend_url.as_deref()));
    let nvs_ref = NVS.init(nvs);
    let panel_ref = PANEL.init(boards::build_panel(panel, board));
    let sleeper_ref = SLEEPER.init(FwSleeper::new(lpwr));
    let ota_ref = OTA.init(FwOta::new());

    // If this boot is the first one after an OTA install, the slot is
    // still marked `New`/`PendingVerify`. Graduate it to `Valid` so the
    // bootloader doesn't auto-roll-back. Cheap no-op on normal boots.
    crate::ota::mark_current_app_valid();

    // Translate the board's local PowerPolicy enum to the ports enum. They're
    // structurally identical; the board crate stays self-contained so its
    // module table doesn't have to import ports just for one enum.
    let policy = match board.default_power_policy {
        crate::boards::PowerPolicy::ScheduledWake => {
            paperanywhere_ports::PowerPolicy::ScheduledWake
        }
        crate::boards::PowerPolicy::AlwaysOn => paperanywhere_ports::PowerPolicy::AlwaysOn,
    };

    // Content-hash dedup: only render the boot screen if its hash differs
    // from whatever the panel last showed. Subsequent RTC-wake cold boots
    // see the same hash in NVS and skip the 5-cycle full refresh — the
    // previous content stays visible on the bistable e-paper while WiFi
    // associates and /state polls in the background. Same mechanism the
    // runtime uses for image updates, so a real image push and a
    // boot-screen render are deduplicated by the same path.
    // Boot-screen content hash factors in the version stamp so an OTA
    // install (which changes the overlaid version text) re-renders the
    // splash on first boot of the new firmware. Without the suffix the
    // version label would be stale through one extra wake.
    let boot_screen_hash = {
        let logo = paperanywhere_ports::hash_bytes(BOOT_SCREEN);
        let version = paperanywhere_ports::hash_bytes(crate::FW_VERSION.as_bytes());
        logo ^ version
    };
    let render_boot_screen =
        if nvs_ref.load_last_render_hash() == Some(boot_screen_hash) {
            false
        } else {
            // Save the hash *before* spawning the runtime — if the refresh
            // panics or the device resets mid-flash we accept the lost
            // splash rather than re-flashing on every retry.
            nvs_ref.save_last_render_hash(boot_screen_hash);
            true
        };
    if render_boot_screen {
        // Paint the logo into the main region, overlay version + build
        // time below it, then flush. Subsequent wake cycles skip this
        // because of the hash match above.
        panel_ref.init();
        // No live state yet at cold-boot (WiFi not associated, no
        // battery sample). Compositor renders the bar with `None`
        // inputs: disconnected wifi icon, empty battery outline.
        panel_ref.set_chrome(sleeper_ref.battery_mv(), None);
        panel_ref.write_chunk(BOOT_SCREEN);
        {
            let mut region = panel_ref.main_region_mut();
            crate::BUILD_INFO.render_into(&mut region);
        }
        panel_ref.refresh();
    }

    let executor = EXECUTOR.init(esp_rtos::embassy::Executor::new());
    executor.run(|spawner| {
        let net_token = crate::network::net_task(runner).expect("net_task pool");
        spawner.spawn(net_token);
        // Boot screen is already on the panel by this point (paint
        // above), so the runtime doesn't need to render it again. Pass
        // an empty slice to short-circuit its boot-screen path.
        let rt_token = runtime_task(
            wifi_ref,
            http_ref,
            nvs_ref,
            panel_ref,
            sleeper_ref,
            ota_ref,
            policy,
            &[],
            OTA_SCREEN,
        )
        .expect("runtime_task pool");
        spawner.spawn(rt_token);
    })
}

#[embassy_executor::task]
async fn runtime_task(
    wifi: &'static mut FwWifi,
    http: &'static mut FwHttp,
    nvs: &'static mut FwNvs,
    panel: &'static mut boards::Panel,
    sleeper: &'static mut FwSleeper,
    ota: &'static mut FwOta,
    policy: paperanywhere_ports::PowerPolicy,
    boot_screen: &'static [u8],
    ota_screen: &'static [u8],
) -> ! {
    paperanywhere_runtime::run(
        wifi,
        http,
        nvs,
        panel,
        sleeper,
        ota,
        policy,
        boot_screen,
        ota_screen,
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

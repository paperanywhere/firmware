//! paperanywhere-sim — desktop simulator entrypoint.
//!
//! Layout: one background thread runs the same `paperanywhere-runtime` polling
//! state machine the firmware uses, with ports wired to `reqwest` / a JSON
//! file / `std::thread::sleep` / an in-memory framebuffer. The main thread
//! runs eframe and renders that framebuffer + a mock telemetry side-panel.
//!
//! Run with `RUST_LOG=info cargo run -p paperanywhere-sim --bin paperanywhere-sim`.
//! Config file path is logged on startup; edit it to point at your backend.

mod app;
mod config;
mod logger;
mod panel;
mod ports;
mod state;

use std::sync::Arc;

use log::info;
use paperanywhere_ports::PowerPolicy;

use crate::app::SimApp;
use crate::config::SimConfig;
use crate::panel::VirtualPanel;
use crate::ports::{SimFirmwareUpdater, SimHttp, SimNvs, SimSleeper, SimWifi};
use crate::state::SimState;

fn main() -> eframe::Result<()> {
    // Install the dual-sink logger first so anything that emits during
    // config loading already lands in the buffer once `SimState` exists.
    let logger_handle = logger::init();

    let config = SimConfig::load_or_init();
    info!("sim config: {:?}", config);

    let state = Arc::new(SimState::new(&config));
    logger_handle.attach(state.clone());

    // The runtime task takes shared `SimState` and the same SimConfig the UI
    // uses. The ports it constructs all hold their own `Arc<SimState>` so the
    // egui side can read what wake step we're in / what battery we mock /
    // what the framebuffer last looked like.
    //
    // We need a multi-threaded tokio runtime so the synchronous `thread::
    // sleep` in `SimSleeper::sleep_for` doesn't starve the reqwest tasks —
    // they live on separate workers and continue draining packets while the
    // runtime task naps.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("paperanywhere-rt")
        .build()
        .expect("build tokio runtime");

    let runtime_state = state.clone();
    let runtime_config = config.clone();
    tokio_rt.spawn(async move { run_runtime(runtime_state, runtime_config).await });
    // Hold the runtime alive for the duration of the program — eframe owns
    // the main thread, so we leak the handle rather than block on it.
    Box::leak(Box::new(tokio_rt));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([config.window_width(), config.window_height()])
            .with_title("paperanywhere — virtual e-paper panel"),
        ..Default::default()
    };

    let ui_state = state.clone();
    eframe::run_native(
        "paperanywhere-sim",
        options,
        Box::new(move |cc| {
            ui_state.set_repaint_ctx(cc.egui_ctx.clone());
            Ok(Box::new(SimApp::new(ui_state)))
        }),
    )
}

async fn run_runtime(state: Arc<SimState>, config: SimConfig) -> ! {
    let mut wifi = SimWifi::new(state.clone());
    let mut http = SimHttp::new(state.clone(), config.backend_url.clone());
    let color_mode = config.parsed_color_mode();

    // Rasterise the boot-screen SVG for the sim's panel dimensions. Same
    // algorithm the firmware uses at build time — both consumers route the
    // bytes through `EpaperPanel::write_chunk`, so the bytes the runtime
    // emits match the bytes the device would emit.
    let boot_screen = build_status_screen(&config, color_mode, LOGO_SVG, "boot");
    let ota_screen = build_status_screen(&config, color_mode, LOGO_OTA_SVG, "ota");

    let mut nvs = SimNvs::new(state.clone(), config);
    let mut panel = VirtualPanel::new(state.clone(), color_mode);
    let mut sleeper = SimSleeper::new(state.clone());
    let mut fw_updater = SimFirmwareUpdater::new(state.clone());

    // The sim runs always-on so updates appear within seconds. Real firmware
    // would derive this from the device's server-configured policy; for the
    // sim we just pick the more responsive of the two and let `/state`'s
    // response override later wakes via the runtime's internal tracking.
    paperanywhere_runtime::run(
        &mut wifi,
        &mut http,
        &mut nvs,
        &mut panel,
        &mut sleeper,
        &mut fw_updater,
        PowerPolicy::AlwaysOn,
        &boot_screen,
        &ota_screen,
    )
    .await
}

/// Embedded copies of the same SVGs the firmware bakes at build time —
/// duplicated here rather than reached via a relative path so the sim crate
/// stays movable as its own checkout.
const LOGO_SVG: &str = include_str!("../../../assets/logo.svg");
const LOGO_OTA_SVG: &str = include_str!("../../../assets/logo_ota.svg");

fn build_status_screen(
    config: &SimConfig,
    color_mode: paperanywhere_ports::ColorMode,
    svg: &str,
    label: &str,
) -> Vec<u8> {
    let spec = paperanywhere_boot_screen::BootScreenSpec {
        width: config.panel_width_px,
        height: config.panel_height_px,
        color_mode,
        padding_fraction: 0.10,
    };
    match paperanywhere_boot_screen::render(svg, &spec) {
        Ok(bytes) => {
            log::info!("{}-screen: rasterised {} bytes for sim panel", label, bytes.len());
            bytes
        }
        Err(e) => {
            log::warn!("{}-screen: render failed: {} — skipping", label, e);
            Vec::new()
        }
    }
}
